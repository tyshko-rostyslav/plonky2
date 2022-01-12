use std::cmp::max;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use log::{debug, info, Level};
use plonky2_field::cosets::get_unique_coset_shifts;
use plonky2_field::extension_field::{Extendable, FieldExtension};
use plonky2_field::fft::fft_root_table;
use plonky2_field::field_types::Field;
use plonky2_field::polynomial::PolynomialValues;
use plonky2_util::{log2_ceil, log2_strict};

use crate::fri::oracle::PolynomialBatch;
use crate::fri::{FriConfig, FriParams};
use crate::gadgets::arithmetic_extension::ExtensionArithmeticOperation;
use crate::gadgets::arithmetic_u32::U32Target;
use crate::gates::arithmetic_base::ArithmeticGate;
use crate::gates::arithmetic_extension::ArithmeticExtensionGate;
use crate::gates::arithmetic_u32::U32ArithmeticGate;
use crate::gates::constant::ConstantGate;
use crate::gates::gate::{Gate, GateInstance, GateRef, PrefixedGate};
use crate::gates::gate_tree::Tree;
use crate::gates::multiplication_extension::MulExtensionGate;
use crate::gates::noop::NoopGate;
use crate::gates::public_input::{PublicInputGate, PublicInputOperation};
use crate::gates::random_access::RandomAccessGate;
use crate::gates::subtraction_u32::U32SubtractionGate;
use crate::gates::switch::SwitchGate;
use crate::hash::hash_types::{HashOutTarget, MerkleCapTarget, RichField};
use crate::iop::ext_target::ExtensionTarget;
use crate::iop::generator::{
    CopyGenerator, RandomValueGenerator, SimpleGenerator, WitnessGenerator,
};
use crate::iop::operation::{Operation, OperationRef};
use crate::iop::target::{BoolTarget, Target};
use crate::iop::wire::Wire;
use crate::plonk::circuit_data::{
    CircuitConfig, CircuitData, CommonCircuitData, ProverCircuitData, ProverOnlyCircuitData,
    VerifierCircuitData, VerifierOnlyCircuitData,
};
use crate::plonk::config::{GenericConfig, Hasher};
use crate::plonk::copy_constraint::CopyConstraint;
use crate::plonk::permutation_argument::Forest;
use crate::plonk::plonk_common::PlonkOracle;
use crate::util::context_tree::ContextTree;
use crate::util::marking::{Markable, MarkedTargets};
use crate::util::partial_products::num_partial_products;
use crate::util::timing::TimingTree;
use crate::util::{transpose, transpose_poly_values};

pub struct CircuitBuilder<F: RichField + Extendable<D>, const D: usize> {
    pub(crate) config: CircuitConfig,

    /// Targets to be made public.
    public_inputs: Vec<Target>,

    /// The next available index for a `Target`.
    target_index: usize,

    copy_constraints: Vec<CopyConstraint>,

    /// A tree of named scopes, used for debugging.
    context_log: ContextTree,

    /// A vector of marked targets. The values assigned to these targets will be displayed by the prover.
    marked_targets: Vec<MarkedTargets<D>>,

    operations: HashSet<OperationRef<F, D>>,

    /// Generators used to generate the witness.
    generators: Vec<Box<dyn WitnessGenerator<F>>>,

    constants_to_targets: HashMap<F, Target>,
    targets_to_constants: HashMap<Target, F>,
}

impl<F: RichField + Extendable<D>, const D: usize> CircuitBuilder<F, D> {
    pub fn new(config: CircuitConfig) -> Self {
        let builder = CircuitBuilder {
            config,
            public_inputs: Vec::new(),
            target_index: 0,
            copy_constraints: Vec::new(),
            context_log: ContextTree::new(),
            marked_targets: Vec::new(),
            operations: HashSet::new(),
            generators: Vec::new(),
            constants_to_targets: HashMap::new(),
            targets_to_constants: HashMap::new(),
        };
        builder.check_config();
        builder
    }

    fn check_config(&self) {
        let &CircuitConfig {
            security_bits,
            fri_config:
                FriConfig {
                    rate_bits,
                    proof_of_work_bits,
                    num_query_rounds,
                    ..
                },
            ..
        } = &self.config;

        // Conjectured FRI security; see the ethSTARK paper.
        let fri_field_bits = F::Extension::order().bits() as usize;
        let fri_query_security_bits = num_query_rounds * rate_bits + proof_of_work_bits as usize;
        let fri_security_bits = fri_field_bits.min(fri_query_security_bits);
        assert!(
            fri_security_bits >= security_bits,
            "FRI params fall short of target security"
        );
    }

    /// Registers the given target as a public input.
    pub fn register_public_input(&mut self, target: Target) {
        self.public_inputs.push(target);
    }

    /// Registers the given targets as public inputs.
    pub fn register_public_inputs(&mut self, targets: &[Target]) {
        targets.iter().for_each(|&t| self.register_public_input(t));
    }

    /// Adds a new "virtual" target. This is not an actual wire in the witness, but just a target
    /// that help facilitate witness generation. In particular, a generator can assign a values to a
    /// virtual target, which can then be copied to other (virtual or concrete) targets. When we
    /// generate the final witness (a grid of wire values), these virtual targets will go away.
    pub fn add_target(&mut self) -> Target {
        let index = self.target_index;
        self.target_index += 1;
        Target(index)
    }

    pub fn add_targets(&mut self, n: usize) -> Vec<Target> {
        (0..n).map(|_i| self.add_target()).collect()
    }

    pub fn add_hash(&mut self) -> HashOutTarget {
        HashOutTarget::from_vec(self.add_targets(4))
    }

    pub fn add_hashes(&mut self, n: usize) -> Vec<HashOutTarget> {
        (0..n).map(|_i| self.add_hash()).collect()
    }

    pub fn add_cap(&mut self, cap_height: usize) -> MerkleCapTarget {
        MerkleCapTarget(self.add_hashes(1 << cap_height))
    }

    pub fn add_extension_target(&mut self) -> ExtensionTarget<D> {
        ExtensionTarget(self.add_targets(D).try_into().unwrap())
    }

    pub fn add_extension_targets(&mut self, n: usize) -> Vec<ExtensionTarget<D>> {
        (0..n).map(|_i| self.add_extension_target()).collect()
    }

    // TODO: Unsafe
    pub fn add_bool_target(&mut self) -> BoolTarget {
        BoolTarget::new_unsafe(self.add_target())
    }

    /// Adds an operartion to the builder.
    pub fn add_operation(&mut self, op: impl Operation<F, D>) {
        self.operations.insert(OperationRef(Arc::new(op)));
    }

    pub fn num_operations(&self) -> usize {
        self.operations.len()
    }

    /// Adds a gate to the circuit, and returns its index.
    pub fn add_gate<G: Gate<F, D>>(&mut self, gate_type: G, constants: Vec<F>) -> usize {
        // self.check_gate_compatibility(&gate_type);
        // assert_eq!(
        //     gate_type.num_constants(),
        //     constants.len(),
        //     "Number of constants doesn't match."
        // );
        //
        // let index = self.gate_instances.len();
        //
        // // Note that we can't immediately add this gate's generators, because the list of constants
        // // could be modified later, i.e. in the case of `ConstantGate`. We will add them later in
        // // `build` instead.
        //
        // // Register this gate type if we haven't seen it before.
        // let gate_ref = GateRef::new(gate_type);
        // self.gates.insert(gate_ref.clone());
        //
        // self.gate_instances.push(GateInstance {
        //     gate_ref,
        //     constants,
        // });
        //
        // index
        todo!()
    }

    fn check_gate_compatibility<G: Gate<F, D>>(&self, gate: &G) {
        assert!(
            gate.num_wires() <= self.config.num_wires,
            "{:?} requires {} wires, but our GateConfig has only {}",
            gate.id(),
            gate.num_wires(),
            self.config.num_wires
        );
    }

    pub fn connect_extension(&mut self, src: ExtensionTarget<D>, dst: ExtensionTarget<D>) {
        for i in 0..D {
            self.connect(src.0[i], dst.0[i]);
        }
    }

    /// Adds a generator which will copy `src` to `dst`.
    pub fn generate_copy(&mut self, src: Target, dst: Target) {
        self.add_simple_generator(CopyGenerator { src, dst });
    }

    /// Uses Plonk's permutation argument to require that two elements be equal.
    pub fn connect(&mut self, x: Target, y: Target) {
        self.copy_constraints
            .push(CopyConstraint::new((x, y), self.context_log.open_stack()));
    }

    pub fn assert_zero(&mut self, x: Target) {
        let zero = self.zero();
        self.connect(x, zero);
    }

    pub fn assert_one(&mut self, x: Target) {
        let one = self.one();
        self.connect(x, one);
    }

    pub fn add_generators(&mut self, generators: Vec<Box<dyn WitnessGenerator<F>>>) {
        self.generators.extend(generators);
    }

    pub fn add_simple_generator<G: SimpleGenerator<F>>(&mut self, generator: G) {
        self.generators.push(Box::new(generator.adapter()));
    }

    /// Returns a routable target with a value of 0.
    pub fn zero(&mut self) -> Target {
        self.constant(F::ZERO)
    }

    /// Returns a routable target with a value of 1.
    pub fn one(&mut self) -> Target {
        self.constant(F::ONE)
    }

    /// Returns a routable target with a value of 2.
    pub fn two(&mut self) -> Target {
        self.constant(F::TWO)
    }

    /// Returns a routable target with a value of `order() - 1`.
    pub fn neg_one(&mut self) -> Target {
        self.constant(F::NEG_ONE)
    }

    pub fn _false(&mut self) -> BoolTarget {
        BoolTarget::new_unsafe(self.zero())
    }

    pub fn _true(&mut self) -> BoolTarget {
        BoolTarget::new_unsafe(self.one())
    }

    /// Returns a routable target with the given constant value.
    pub fn constant(&mut self, c: F) -> Target {
        // if let Some(&target) = self.constants_to_targets.get(&c) {
        //     // We already have a wire for this constant.
        //     return target;
        // }
        //
        // let (gate, instance) = self.constant_gate_instance();
        // let target = self.add_target();
        // self.gate_instances[gate].constants[instance] = c;
        //
        // self.constants_to_targets.insert(c, target);
        // self.targets_to_constants.insert(target, c);
        //
        // target
        todo!()
    }

    pub fn constants(&mut self, constants: &[F]) -> Vec<Target> {
        constants.iter().map(|&c| self.constant(c)).collect()
    }

    pub fn constant_bool(&mut self, b: bool) -> BoolTarget {
        if b {
            self._true()
        } else {
            self._false()
        }
    }

    /// Returns a U32Target for the value `c`, which is assumed to be at most 32 bits.
    pub fn constant_u32(&mut self, c: u32) -> U32Target {
        U32Target(self.constant(F::from_canonical_u32(c)))
    }

    /// If the given target is a constant (i.e. it was created by the `constant(F)` method), returns
    /// its constant value. Otherwise, returns `None`.
    pub fn target_as_constant(&self, target: Target) -> Option<F> {
        self.targets_to_constants.get(&target).cloned()
    }

    /// If the given `ExtensionTarget` is a constant (i.e. it was created by the
    /// `constant_extension(F)` method), returns its constant value. Otherwise, returns `None`.
    pub fn target_as_constant_ext(&self, target: ExtensionTarget<D>) -> Option<F::Extension> {
        // Get a Vec of any coefficients that are constant. If we end up with exactly D of them,
        // then the `ExtensionTarget` as a whole is constant.
        let const_coeffs: Vec<F> = target
            .0
            .iter()
            .filter_map(|&t| self.target_as_constant(t))
            .collect();

        if let Ok(d_const_coeffs) = const_coeffs.try_into() {
            Some(F::Extension::from_basefield_array(d_const_coeffs))
        } else {
            None
        }
    }

    pub fn push_context(&mut self, level: log::Level, ctx: &str) {
        self.context_log.push(ctx, level, self.num_operations());
    }

    pub fn pop_context(&mut self) {
        self.context_log.pop(self.num_operations());
    }

    pub fn add_marked(&mut self, targets: Markable<D>, name: &str) {
        self.marked_targets.push(MarkedTargets {
            targets,
            name: name.to_string(),
        })
    }

    /// The number of (base field) `arithmetic` operations that can be performed in a single gate.
    pub(crate) fn num_base_arithmetic_ops_per_gate(&self) -> usize {
        if self.config.use_base_arithmetic_gate {
            ArithmeticGate::new_from_config(&self.config).num_ops
        } else {
            self.num_ext_arithmetic_ops_per_gate()
        }
    }

    /// The number of `arithmetic_extension` operations that can be performed in a single gate.
    pub(crate) fn num_ext_arithmetic_ops_per_gate(&self) -> usize {
        ArithmeticExtensionGate::<D>::new_from_config(&self.config).num_ops
    }

    pub fn print_gate_counts(&self, min_delta: usize) {
        // // Print gate counts for each context.
        // self.context_log
        //     .filter(self.num_gates(), min_delta)
        //     .print(self.num_gates());
        //
        // // Print total count of each gate type.
        // debug!("Total gate counts:");
        // for gate in self.gates.iter().cloned() {
        //     let count = self
        //         .gate_instances
        //         .iter()
        //         .filter(|inst| inst.gate_ref == gate)
        //         .count();
        //     debug!("- {} instances of {}", count, gate.0.id());
        // }
        todo!()
    }

    /// Builds a "full circuit", with both prover and verifier data.
    pub fn build<C: GenericConfig<D, F = F>>(mut self) -> CircuitData<F, C, D> {
        todo!()
    }
    /// Builds a "full circuit", with both prover and verifier data.
    pub fn tomove<C: GenericConfig<D, F = F>>(mut self) -> CircuitData<F, C, D> {
        let mut timing = TimingTree::new("preprocess", Level::Trace);
        let start = Instant::now();
        let rate_bits = self.config.fri_config.rate_bits;

        // Hash the public inputs, and route them to a `PublicInputGate` which will enforce that
        // those hash wires match the claimed public inputs.
        let public_inputs_hash =
            self.hash_n_to_hash::<C::InnerHasher>(self.public_inputs.clone(), true);
        self.add_operation(PublicInputOperation {
            public_inputs_hash,
            gate: PublicInputGate,
        });

        info!(
            "Degree before blinding & padding: {}",
            self.gate_instances.len()
        );
        self.blind_and_pad();
        let degree = self.gate_instances.len();
        info!("Degree after blinding & padding: {}", degree);
        let degree_bits = log2_strict(degree);
        let fri_params = self.fri_params(degree_bits);
        assert!(
            fri_params.total_arities() <= degree_bits,
            "FRI total reduction arity is too large.",
        );

        let gates = self.gates.iter().cloned().collect();
        let (gate_tree, max_filtered_constraint_degree, num_constants) = Tree::from_gates(gates);
        let prefixed_gates = PrefixedGate::from_tree(gate_tree);

        // `quotient_degree_factor` has to be between `max_filtered_constraint_degree-1` and `1<<rate_bits`.
        // We find the value that minimizes `num_partial_product + quotient_degree_factor`.
        let min_quotient_degree_factor = max_filtered_constraint_degree - 1;
        let max_quotient_degree_factor = self.config.max_quotient_degree_factor.min(1 << rate_bits);
        let quotient_degree_factor = (min_quotient_degree_factor..=max_quotient_degree_factor)
            .min_by_key(|&q| num_partial_products(self.config.num_routed_wires, q).0 + q)
            .unwrap();
        debug!("Quotient degree factor set to: {}.", quotient_degree_factor);

        let subgroup = F::two_adic_subgroup(degree_bits);

        let constant_vecs = self.constant_polys(&prefixed_gates, num_constants);

        let k_is = get_unique_coset_shifts(degree, self.config.num_routed_wires);
        let (sigma_vecs, forest) = self.sigma_vecs(&k_is, &subgroup);

        // Precompute FFT roots.
        let max_fft_points = 1 << (degree_bits + max(rate_bits, log2_ceil(quotient_degree_factor)));
        let fft_root_table = fft_root_table(max_fft_points);

        let constants_sigmas_vecs = [constant_vecs, sigma_vecs.clone()].concat();
        let constants_sigmas_commitment = PolynomialBatch::from_values(
            constants_sigmas_vecs,
            rate_bits,
            PlonkOracle::CONSTANTS_SIGMAS.blinding,
            self.config.fri_config.cap_height,
            &mut timing,
            Some(&fft_root_table),
        );

        let constants_sigmas_cap = constants_sigmas_commitment.merkle_tree.cap.clone();
        let verifier_only = VerifierOnlyCircuitData {
            constants_sigmas_cap: constants_sigmas_cap.clone(),
        };

        // Add gate generators.
        self.add_generators(
            self.gate_instances
                .iter()
                .enumerate()
                .flat_map(|(index, gate)| gate.gate_ref.0.generators(index, &gate.constants))
                .collect(),
        );

        // Index generator indices by their watched targets.
        let mut generator_indices_by_watches = BTreeMap::new();
        for (i, generator) in self.generators.iter().enumerate() {
            for watch in generator.watch_list() {
                let watch_index = forest.target_index(watch);
                let watch_rep_index = forest.parents[watch_index];
                generator_indices_by_watches
                    .entry(watch_rep_index)
                    .or_insert_with(Vec::new)
                    .push(i);
            }
        }
        for indices in generator_indices_by_watches.values_mut() {
            indices.dedup();
            indices.shrink_to_fit();
        }

        let prover_only = ProverOnlyCircuitData {
            generators: self.generators,
            generator_indices_by_watches,
            constants_sigmas_commitment,
            sigmas: transpose_poly_values(sigma_vecs),
            subgroup,
            public_inputs: self.public_inputs,
            marked_targets: self.marked_targets,
            representative_map: forest.parents,
            fft_root_table: Some(fft_root_table),
        };

        // The HashSet of gates will have a non-deterministic order. When converting to a Vec, we
        // sort by ID to make the ordering deterministic.
        let mut gates = self.gates.iter().cloned().collect::<Vec<_>>();
        gates.sort_unstable_by_key(|gate| gate.0.id());

        let num_gate_constraints = gates
            .iter()
            .map(|gate| gate.0.num_constraints())
            .max()
            .expect("No gates?");

        let num_partial_products =
            num_partial_products(self.config.num_routed_wires, quotient_degree_factor);

        // TODO: This should also include an encoding of gate constraints.
        let circuit_digest_parts = [
            constants_sigmas_cap.flatten(),
            vec![/* Add other circuit data here */],
        ];
        let circuit_digest = C::Hasher::hash(circuit_digest_parts.concat(), false);

        let common = CommonCircuitData {
            config: self.config,
            fri_params,
            degree_bits,
            gates: prefixed_gates,
            quotient_degree_factor,
            num_gate_constraints,
            num_constants,
            num_virtual_targets: self.virtual_target_index,
            k_is,
            num_partial_products,
            circuit_digest,
        };

        debug!("Building circuit took {}s", start.elapsed().as_secs_f32());
        CircuitData {
            prover_only,
            verifier_only,
            common,
        }
    }

    /// Builds a "prover circuit", with data needed to generate proofs but not verify them.
    pub fn build_prover<C: GenericConfig<D, F = F>>(self) -> ProverCircuitData<F, C, D> {
        // TODO: Can skip parts of this.
        let CircuitData {
            prover_only,
            common,
            ..
        } = self.build();
        ProverCircuitData {
            prover_only,
            common,
        }
    }

    /// Builds a "verifier circuit", with data needed to verify proofs but not generate them.
    pub fn build_verifier<C: GenericConfig<D, F = F>>(self) -> VerifierCircuitData<F, C, D> {
        // TODO: Can skip parts of this.
        let CircuitData {
            verifier_only,
            common,
            ..
        } = self.build();
        VerifierCircuitData {
            verifier_only,
            common,
        }
    }
}
