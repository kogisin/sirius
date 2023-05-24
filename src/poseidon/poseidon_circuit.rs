use poseidon::{self, Spec};
use halo2_proofs::{
    circuit::{Value, AssignedCell},
    plonk::{Advice, ConstraintSystem, Column, 
        Fixed, Expression, Error},
    poly::Rotation};
use halo2curves::group::ff::PrimeField;
use std::marker::PhantomData;
//use std::mem;
use std::convert::TryInto;
use crate::standard_gate::RegionCtx;

#[derive(Clone, Debug)]
pub struct PoseidonConfig<F: PrimeField, const T: usize, const RATE: usize> {
    state: [Column<Advice>; T],
    input: Column<Advice>,
    out: Column<Advice>,
    // for linear term
    q_1: [Column<Fixed>; T],
    // for quintic term
    q_5: [Column<Fixed>; T],
    q_i: Column<Fixed>,
    q_o: Column<Fixed>,
    rc: Column<Fixed>,
    _marker: PhantomData<F>
}

#[derive(Debug)]
pub struct PoseidonChip<F: PrimeField, const T: usize, const RATE: usize> {
    config: PoseidonConfig<F, T, RATE>,
    spec: Spec<F, T, RATE>,
    buf: Vec<F>,
    offset: usize // TODO: support multiple uses of squeeze when needed
}

impl<F: PrimeField, const T: usize, const RATE: usize> PoseidonChip<F,T,RATE> {
    pub fn new(config: PoseidonConfig<F, T, RATE>, spec: Spec<F,T,RATE>) -> Self {
        Self {
            config,
            spec,
            buf: Vec::new(),
            offset: 0,
        }
    }

    pub fn next_state_val(state: [Value<F>; T], q_1: [F; T], q_5: [F; T], q_o: F, rc: F) -> Value<F> {
        let pow_5 = |v: Value<F>| {
            let v2 = v * v;
            v2 * v2 * v
        };
        let mut out = Value::known(rc);
        for ((s, q1), q5) in state.iter().zip(q_1).zip(q_5) {
            out = out + pow_5(*s) * Value::known(q5) + *s * Value::known(q1);
        }
        out * Value::known((-q_o).invert().unwrap())
    }

    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        adv_cols: &mut (impl Iterator<Item = Column<Advice>> + Clone),
        fix_cols: &mut (impl Iterator<Item = Column<Fixed>> + Clone),
    ) -> PoseidonConfig<F, T, RATE> {
        
        let state = [0; T].map(|_| adv_cols.next().unwrap());
        let input = adv_cols.next().unwrap();
        let out = adv_cols.next().unwrap();
        let q_1 = [0; T].map(|_| fix_cols.next().unwrap());
        let q_5 = [0; T].map(|_| fix_cols.next().unwrap());
        let q_i = fix_cols.next().unwrap();
        let q_o = fix_cols.next().unwrap();
        let rc = fix_cols.next().unwrap();

        state.map(|s| {
            meta.enable_equality(s);
        });
        meta.enable_equality(out);

        let pow_5 = |v: Expression<F>| {
            let v2 = v.clone() * v.clone();
            v2.clone() * v2 * v
        };

        meta.create_gate("sum_i(q_1[i]*s[i]) + sum_i(q_5[i]*s[i]^5) + rc + q_i*input + q_o*out=0", |meta|{
            let state = state.into_iter().map(|s| meta.query_advice(s, Rotation::cur())).collect::<Vec<_>>();
            let input = meta.query_advice(input, Rotation::cur());
            let out = meta.query_advice(out, Rotation::cur());
            let q_1 = q_1.into_iter().map(|q| meta.query_fixed(q, Rotation::cur())).collect::<Vec<_>>();
            let q_5 = q_5.into_iter().map(|q| meta.query_fixed(q, Rotation::cur())).collect::<Vec<_>>();
            let q_i = meta.query_fixed(q_i, Rotation::cur());
            let q_o = meta.query_fixed(q_o, Rotation::cur());
            let rc = meta.query_fixed(rc, Rotation::cur());
            let res = state.into_iter().zip(q_1).zip(q_5).map(|((w, q1), q5)| {
                q1 * w.clone()  +  q5 * pow_5(w)
            }).fold(q_i * input + rc +  q_o * out, |acc, item| {
                acc + item
            });
            vec![res]
        });

        PoseidonConfig {
            state,
            input,
            out,
            q_1,
            q_5,
            q_i,
            q_o,
            rc,
            _marker: PhantomData
        }
    }

    pub fn pre_round(&self, ctx: &mut RegionCtx<'_, F>, inputs: Vec<F>, state_idx: usize, state: &[AssignedCell<F, F>; T]) -> Result<AssignedCell<F, F>, Error> {
        assert!(inputs.len() <= RATE); 
        let s_val = state[state_idx].value().copied();

        let inputs = std::iter::once(F::ZERO).chain(inputs.into_iter())
        .chain(std::iter::once(F::ONE))
        .chain(std::iter::repeat(F::ZERO))
        .take(T).collect::<Vec<_>>();
        let input_val = Value::known(inputs[state_idx]);

        let constants = self.spec.constants().start();
        let pre_constants = constants[0];
        let rc_val = pre_constants[state_idx];

        let out_val = s_val + input_val + Value::known(rc_val);

        let si = ctx.assign_advice(||"first round: state", self.config.state[state_idx], s_val)?;
        ctx.constrain_equal(state[state_idx].cell(), si.cell())?;

        ctx.assign_advice(||"pre_round: input", self.config.input, input_val)?;
        ctx.assign_fixed(||"pre_round: q_1", self.config.q_1[state_idx], F::ONE)?;
        ctx.assign_fixed(||"pre_round: q_i", self.config.q_i, F::ONE)?;
        ctx.assign_fixed(||"pre_round: q_o", self.config.q_o, -F::ONE)?;
        ctx.assign_fixed(||"pre_round: rc", self.config.rc, rc_val)?;
        let out = ctx.assign_advice(||"pre_round: out", self.config.out, out_val)?;
    
        ctx.next();
        Ok(out)
    }

    // round_idx \in [0; r_f - 1] indicates the round index of either first half full or second half full
    pub fn full_round(&self, ctx: &mut RegionCtx<'_, F>, is_first_half_full: bool, round_idx: usize, state_idx: usize, state: &[AssignedCell<F,F>; T]) -> Result<AssignedCell<F,F>,Error> {
        let mut state_vals = [Value::known(F::ZERO); T];
        let q_1_vals = [F::ZERO; T];
        let mut q_5_vals = [F::ZERO; T];
        let q_o_val = -F::ONE;

        let r_f = self.spec.r_f() / 2;
        let constants = if is_first_half_full { self.spec.constants().start() } else { self.spec.constants().end() };
        let rcs = if is_first_half_full { constants[round_idx + 1] } else if round_idx < r_f - 1 { constants[round_idx] } else { [F::ZERO; T] };

        let mds = if is_first_half_full && round_idx == r_f - 1  { self.spec.mds_matrices().pre_sparse_mds().rows() } else { self.spec.mds_matrices().mds().rows() };
        let mds_row = mds[state_idx];


        let mut rc_val = F::ZERO;
        for (j, (mij, cj)) in mds_row.iter().zip(rcs).enumerate() {
            rc_val = rc_val + *mij * cj;
            q_5_vals[j] = *mij;
            ctx.assign_fixed(||format!("full_round {}: q_5", round_idx), self.config.q_5[j], q_5_vals[j])?;
        }

        for (i, s) in state.iter().enumerate() {
            state_vals[i] = s.value().copied();
            let si = ctx.assign_advice(||format!("full_round {}: state", round_idx), self.config.state[i], s.value().copied())?;
            ctx.constrain_equal(s.cell(), si.cell())?;
        }

        ctx.assign_fixed(||format!("full_round {}: rc", round_idx), self.config.rc, rc_val)?;
        ctx.assign_fixed(||format!("full_round {}: q_o", round_idx), self.config.q_o, q_o_val)?;
        let out_val = Self::next_state_val(state_vals, q_1_vals, q_5_vals, q_o_val, rc_val);
        let out = ctx.assign_advice(||format!("full_round {}: out", round_idx), self.config.out, out_val)?;
        ctx.next();
        Ok(out)
    }

    pub fn partial_round(&self, ctx: &mut RegionCtx<'_, F>, round_idx: usize, state_idx: usize, state: &[AssignedCell<F, F>; T]) -> Result<AssignedCell<F, F>, Error> {
        let mut state_vals = [Value::known(F::ZERO); T];
        let mut q_1_vals = [F::ZERO; T];
        let mut q_5_vals = [F::ZERO; T];
        let q_o_val = -F::ONE;

        let constants =  self.spec.constants().partial(); 
        let rc = constants[round_idx];

        let sparse_mds = self.spec.mds_matrices().sparse_matrices();
        let row = sparse_mds[round_idx].row();
        let col_hat = sparse_mds[round_idx].col_hat();

        for (i, s) in state.iter().enumerate() {
            state_vals[i] = s.value().copied();
            let si = ctx.assign_advice(||format!("partial_round {}: state", round_idx), self.config.state[i], s.value().copied())?;
            ctx.constrain_equal(s.cell(), si.cell())?;
        }

        let rc_val;
        if state_idx == 0 {
            q_5_vals[0] = row[0];
            ctx.assign_fixed(||format!("partial_round {}: q_5", round_idx), self.config.q_5[0], q_5_vals[0])?;
            rc_val = row[0] * rc;
            ctx.assign_fixed(||format!("partial_round {}: rc", round_idx), self.config.rc, rc_val)?;
            for j in 1..T {
                q_1_vals[j] = row[j];
                ctx.assign_fixed(||format!("partial_round {}: q_1", round_idx), self.config.q_1[j], q_1_vals[j])?;
            }
        } else {
            q_5_vals[0] = col_hat[state_idx - 1];
            q_1_vals[state_idx] = F::ONE;
            ctx.assign_fixed(||format!("partial_round {}: q_5", round_idx), self.config.q_5[0], q_5_vals[0])?;
            ctx.assign_fixed(||format!("partial_round {}: q_1", round_idx), self.config.q_1[state_idx], q_1_vals[state_idx])?;
            rc_val = col_hat[state_idx - 1] * rc;
            ctx.assign_fixed(||format!("partial_round {}, rc", round_idx), self.config.rc, rc_val)?;
        }

        let out_val = Self::next_state_val(state_vals, q_1_vals, q_5_vals, -F::ONE, rc_val);
        ctx.assign_fixed(||format!("full_round {}: q_o", round_idx), self.config.q_o, q_o_val)?;
        let out = ctx.assign_advice(||format!("full_round {}: out", round_idx), self.config.out, out_val)?;
        ctx.next();
        Ok(out)
    }

    pub fn permutation(&self, ctx: &mut RegionCtx<'_, F>, inputs: Vec<F>, init_state: &[AssignedCell<F, F>; T]) -> Result<[AssignedCell<F, F>; T], Error> {
        let mut state = Vec::new();
        for i in 0..T {
            let si = self.pre_round(ctx, inputs.clone(), i, init_state)?;
            state.push(si);
        }

        let r_f = self.spec.r_f() / 2;
        let r_p = self.spec.constants().partial().len();

        for round_idx in 0..r_f {
            let mut next_state = Vec::new();
            for state_idx in 0..T {
                let si = self.full_round(ctx, true, round_idx, state_idx, state[..].try_into().unwrap())?;
                next_state.push(si);
            }
            state = next_state;
        }

        for round_idx in 0..r_p {
            let mut next_state = Vec::new();
            for  state_idx in 0..T {
                let si = self.partial_round(ctx, round_idx, state_idx, state[..].try_into().unwrap())?;
                next_state.push(si);
            }
            state = next_state;
        }

        for round_idx in 0..r_f {
            let mut next_state = Vec::new();
            for  state_idx in 0..T {
                let si = self.full_round(ctx, false, round_idx, state_idx, state[..].try_into().unwrap())?;
                next_state.push(si);
            }
            state = next_state;
        }
        let res: [AssignedCell<F, F>; T] = state.try_into().unwrap();
        Ok(res)
    }

    pub fn update(&mut self, inputs: Vec<F>) {
        self.buf.extend(inputs)
    }

    pub fn squeeze(&mut self, ctx: &mut RegionCtx<'_, F>) -> Result<AssignedCell<F, F>, Error> {
        //let buf = mem::take(&mut self.buf);
        ctx.reset(self.offset);
        let buf = self.buf.clone();
        let exact = buf.len() % RATE == 0;
        let mut state = Vec::new();
        let state0: [F; T] = poseidon::State::default().words();
        for i in 0..T {
            let si = ctx.assign_advice(||"initial state", self.config.state[i], Value::known(state0[i]))?;
            state.push(si);
        }
        for chunk in buf.chunks(RATE) {
            let next_state = self.permutation(ctx, chunk.to_vec(), state[..].try_into().unwrap())?;
            state = next_state.to_vec();
        }
        if exact {
            let next_state = self.permutation(ctx, Vec::new(), state[..].try_into().unwrap())?;
            state = next_state.to_vec();
        }
        self.offset = ctx.offset();

        Ok(state[1].clone())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use halo2_proofs::poly::ipa::commitment::{IPACommitmentScheme, ParamsIPA};
    use halo2_proofs::poly::ipa::multiopen::ProverIPA;
    use halo2_proofs::poly::{VerificationStrategy, ipa::strategy::SingleStrategy};
    use halo2_proofs::poly::commitment::ParamsProver;
    use halo2_proofs::transcript::{Blake2bRead, Blake2bWrite, Challenge255, TranscriptReadBuffer, TranscriptWriterBuffer};
    use halo2_proofs::plonk::{Circuit, Instance, create_proof, keygen_pk, keygen_vk, verify_proof};
    use halo2_proofs::circuit::{Layouter, SimpleFloorPlanner};
    use halo2curves::pasta::{vesta, EqAffine, Fp};
    use halo2curves::group::ff::FromUniformBytes;
    use rand_core::OsRng;

    const T: usize = 3;
    const RATE: usize = 2;
    const R_F: usize = 4;
    const R_P: usize = 3;

    #[derive(Clone, Debug)]
    struct TestCircuitConfig<F: PrimeField> {
       pconfig: PoseidonConfig<F, T, RATE>,
       instance: Column<Instance>
    }

    struct TestCircuit<F: PrimeField> {
        inputs: Vec<F>,
    }

    impl<F:PrimeField> TestCircuit<F> {
        fn new(inputs: Vec<F>) -> Self {
            Self {
                inputs,
            }
        }
    }


    impl<F: PrimeField + FromUniformBytes<64>> Circuit<F> for TestCircuit<F> {
        type Config = TestCircuitConfig<F>;
        type FloorPlanner = SimpleFloorPlanner;


        fn without_witnesses(&self) -> Self {
            Self {
                inputs: Vec::new(),
            }
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            let instance = meta.instance_column();
            meta.enable_equality(instance);
            let mut adv_cols = [(); T+2].map(|_| meta.advice_column()).into_iter();
            let mut fix_cols = [(); 2*T+3].map(|_| meta.fixed_column()).into_iter();
            let pconfig = PoseidonChip::configure(meta, &mut adv_cols, &mut fix_cols);
            Self::Config {
                pconfig,
                instance,
            }
        }

        fn synthesize(&self, config: Self::Config, mut layouter: impl Layouter<F>) -> Result<(), Error> {
             let spec = Spec::new(R_F, R_P);
             let mut pchip = PoseidonChip::new(config.pconfig, spec);
             pchip.update(self.inputs.clone());
             let output = layouter.assign_region(||"poseidon hash", |region|{
                 let ctx = &mut RegionCtx::new(region, 0);
                 pchip.squeeze(ctx)
             })?;
             layouter.constrain_instance(output.cell(), config.instance, 0)?;
             Ok(())
        }
    }


    #[test]
    fn test_poseidon_circuit() {
        println!("-----running Poseidon Circuit-----");
        const K:u32 = 8;
        type Scheme = IPACommitmentScheme<EqAffine>;
        let params: ParamsIPA<vesta::Affine> = ParamsIPA::<EqAffine>::new(K);
        let mut inputs = Vec::new();
        for i in 0..5 {
            inputs.push(Fp::from(i as u64));
        }
        let circuit = TestCircuit::new(inputs);

        let vk = keygen_vk(&params, &circuit).expect("keygen_vk should not fail");
        let pk = keygen_pk(&params, vk, &circuit).expect("keygen_pk should not fail");
        // hex = 0x1cd3150d8e12454ff385da8a4d864af6d0f021529207b16dd6c3d8f2b52cfc67
        let out_hash = Fp::from_str_vartime("13037709793114148810823325920380362524528554380279235267325741570708489436263").unwrap();
        let public_inputs: &[&[Fp]] = &[&[out_hash]];
        let mut transcript = Blake2bWrite::<_, EqAffine, Challenge255<_>>::init(vec![]);
        create_proof::<IPACommitmentScheme<_>, ProverIPA<_>, _, _, _, _>(&params, &pk, &[circuit], &[public_inputs], OsRng, &mut transcript)
              .expect("proof generation should not fail");

        let proof = transcript.finalize();
        let mut transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);
        let strategy = SingleStrategy::new(&params);
        verify_proof(&params, pk.get_vk(), strategy, &[public_inputs], &mut transcript).unwrap();
        println!("-----poseidon circuit works fine-----");
    }

    #[test] 
    fn test_mock() {
        use halo2_proofs::dev::MockProver;
        const K:u32 = 8;
        let mut inputs = Vec::new();
        for i in 0..5 {
            inputs.push(Fp::from(i as u64));
        }
        let circuit = TestCircuit::new(inputs);
        // hex = 0x1cd3150d8e12454ff385da8a4d864af6d0f021529207b16dd6c3d8f2b52cfc67
        let out_hash = Fp::from_str_vartime("13037709793114148810823325920380362524528554380279235267325741570708489436263").unwrap();
        let public_inputs = vec![vec![out_hash]];
        let prover = match MockProver::run(K, &circuit, public_inputs) {
            Ok(prover) => prover,
            Err(e) => panic!("{:#?}", e),
        };
        assert_eq!(prover.verify(), Ok(()));
    }
}
