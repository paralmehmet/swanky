use crate::edabits::RcRefCell;
use crate::homcom::{
    FComProver, FComVerifier, MacProver, MacVerifier, StateMultCheckProver, StateMultCheckVerifier,
};
use eyre::{eyre, Context, Result};
use generic_array::{typenum::Unsigned, GenericArray};
use log::{debug, info, warn};
use ocelot::svole::wykw::LpnParams;
use rand::{CryptoRng, Rng};
use scuttlebutt::{field::FiniteField, AbstractChannel};

// Some design decisions:
// * There is one queue for the multiplication check and another queue for `assert_zero`s.
// * The communication during circuit evaluation goes from the prover to the verifier,
//   therefore it is possible to flush only when queues are full and mult or zero checks are performed.
// * Gates do not specifiy whether their input values are public or private, their execution
//   does a case analysis to perform the right operation.
//   For example, a multiplication with public values requires a simple field multiplication,
//   whereas the input are private it requires a zero_knowledge multiplication check.

// function adapted from the `mac-and-cheese-rfme` branch
fn padded_read<FE: FiniteField>(mut x: &[u8]) -> Result<FE> {
    // This assumes that finite field elements can be zero padded in their byte reprs. For prime
    // fields, this assumes that the byte representation is little-endian.
    while x.last() == Some(&0) {
        x = &x[0..x.len() - 1];
    }
    if x.len() > FE::ByteReprLen::USIZE {
        Err(eyre!("Invalid field element"))
    } else {
        let mut out = GenericArray::default();
        let size = x.len().min(FE::ByteReprLen::USIZE);
        out[0..size].copy_from_slice(&x[0..size]);
        // NOTE: the FE type doesn't require that from_bytes be little-endian. However, we
        // currently implement it that way for all fields.
        FE::from_bytes(&out).context("Invalid field element")
    }
}

/// Converts a little-endian byte slice to a field element. The byte slice may be zero padded.
pub fn from_bytes_le<FE: FiniteField>(val: &[u8]) -> Result<FE> {
    padded_read(val)
}

const QUEUE_CAPACITY: usize = 3_000_000;
const TICK_TIMER: usize = 5_000_000;

#[derive(Default)]
struct Monitor {
    tick: usize,
    monitor_instance: usize,
    monitor_witness: usize,
    monitor_mul: usize,
    monitor_mulc: usize,
    monitor_add: usize,
    monitor_addc: usize,
    monitor_check_zero: usize,
    monitor_zk_check_zero: usize,
    monitor_zk_mult_check: usize,
}

impl Monitor {
    fn tick(&mut self) {
        self.tick += 1;
        if self.tick >= TICK_TIMER {
            self.tick %= TICK_TIMER;
            self.log_monitor();
        }
    }

    fn incr_monitor_instance(&mut self) {
        self.tick();
        self.monitor_instance += 1;
    }
    fn incr_monitor_mul(&mut self) {
        self.tick();
        self.monitor_mul += 1;
    }
    fn incr_monitor_mulc(&mut self) {
        self.tick();
        self.monitor_mulc += 1;
    }
    fn incr_monitor_add(&mut self) {
        self.tick();
        self.monitor_add += 1;
    }
    fn incr_monitor_addc(&mut self) {
        self.tick();
        self.monitor_addc += 1;
    }
    fn incr_monitor_check_zero(&mut self) {
        self.tick();
        self.monitor_check_zero += 1;
    }
    fn incr_monitor_witness(&mut self) {
        self.tick();
        self.monitor_witness += 1;
    }

    fn incr_zk_mult_check(&mut self, n: usize) {
        self.monitor_zk_mult_check += n;
    }
    fn incr_zk_check_zero(&mut self, n: usize) {
        self.monitor_zk_check_zero += n;
    }

    fn log_monitor(&self) {
        info!(
            "inp:{:<11} witn:{:<11} mul:{:<11} czero:{:<11}",
            self.monitor_instance, self.monitor_witness, self.monitor_mul, self.monitor_check_zero,
        );
    }

    fn log_final_monitor(&self) {
        if self.monitor_mul != self.monitor_zk_mult_check {
            warn!(
                "diff numb of mult gates {} and mult_check {}",
                self.monitor_mul, self.monitor_zk_mult_check
            );
        }

        info!("nb inst:   {:>11}", self.monitor_instance);
        info!("nb witn:   {:>11}", self.monitor_witness);
        info!("nb addc:   {:>11}", self.monitor_addc);
        info!("nb add:    {:>11}", self.monitor_add);
        info!("nb multc:  {:>11}", self.monitor_mulc);
        info!("nb mult:   {:>11}", self.monitor_mul);
        info!("nb czero:  {:>11}", self.monitor_check_zero);
    }
}

// The prover/verifier structures and functions are generic over a `FiniteField` named `FE`.
// `FE` is the type for the authenticated values whereas the clear values are from
// the underlying prime field `FE::PrimeField`.
type FieldClear<FE> = <FE as FiniteField>::PrimeField;

/// Prover for Diet Mac'n'Cheese.
pub struct DietMacAndCheeseProver<FE: FiniteField, C: AbstractChannel, RNG: CryptoRng + Rng> {
    is_ok: bool,
    prover: RcRefCell<FComProver<FE>>,
    pub channel: C,
    pub rng: RNG,
    check_zero_list: Vec<MacProver<FE>>,
    monitor: Monitor,
    state_mult_check: StateMultCheckProver<FE>,
    no_batching: bool,
}

impl<FE: FiniteField, C: AbstractChannel, RNG: CryptoRng + Rng> DietMacAndCheeseProver<FE, C, RNG> {
    /// Initialize the prover by providing a channel, a random generator and a pair of LPN parameters as defined by svole.
    pub fn init(
        channel: &mut C,
        mut rng: RNG,
        lpn_setup: LpnParams,
        lpn_extend: LpnParams,
        no_batching: bool,
    ) -> Result<Self> {
        let state_mult_check = StateMultCheckProver::init(channel)?;
        Ok(Self {
            is_ok: true,
            prover: RcRefCell::new(FComProver::init(channel, &mut rng, lpn_setup, lpn_extend)?),
            channel: channel.clone(),
            rng,
            check_zero_list: Vec::new(),
            monitor: Monitor::default(),
            state_mult_check,
            no_batching,
        })
    }

    /// Initialize the verifier by providing a reference to a fcom.
    pub fn init_with_fcom(
        channel: &mut C,
        rng: RNG,
        fcom: &RcRefCell<FComProver<FE>>,
        no_batching: bool,
    ) -> Result<Self> {
        let state_mult_check = StateMultCheckProver::init(channel)?;
        Ok(Self {
            is_ok: true,
            prover: fcom.clone(),
            channel: channel.clone(),
            rng,
            check_zero_list: Vec::new(),
            monitor: Monitor::default(),
            state_mult_check,
            no_batching,
        })
    }

    /// Get party
    pub(crate) fn get_party(&mut self) -> &RcRefCell<FComProver<FE>> {
        &self.prover
    }

    // this function should be called before every function exposed publicly by the API.
    fn check_is_ok(&self) -> Result<()> {
        if !self.is_ok {
            return Err(eyre!(
                "An error occurred earlier. This functionality should not be used further"
            ));
        }
        Ok(())
    }

    fn input(&mut self, v: FE::PrimeField) -> Result<MacProver<FE>> {
        let tag = self
            .prover
            .get_refmut()
            .input1(&mut self.channel, &mut self.rng, v);
        if tag.is_err() {
            self.is_ok = false;
        }
        Ok(MacProver::new(v, tag?))
    }

    fn do_mult_check(&mut self) -> Result<usize> {
        debug!("do mult_check");
        self.channel.flush()?;
        let cnt = self.prover.get_refmut().quicksilver_finalize(
            &mut self.channel,
            &mut self.rng,
            &mut self.state_mult_check,
        )?;
        self.monitor.incr_zk_mult_check(cnt);
        Ok(cnt)
    }

    fn do_check_zero(&mut self) -> Result<()> {
        // debug!("do check_zero");
        self.channel.flush()?;
        let r = self
            .prover
            .get_refmut()
            .check_zero(&mut self.channel, &self.check_zero_list);
        if r.is_err() {
            warn!("check_zero fails");
            self.is_ok = false;
        }
        self.monitor.incr_zk_check_zero(self.check_zero_list.len());
        self.check_zero_list.clear();
        r
    }

    fn push_check_zero_list(&mut self, e: MacProver<FE>) -> Result<()> {
        self.check_zero_list.push(e);

        if self.check_zero_list.len() == QUEUE_CAPACITY || self.no_batching {
            self.do_check_zero()?;
        }
        Ok(())
    }

    /// Assert a value is zero.
    pub(crate) fn assert_zero(&mut self, value: &MacProver<FE>) -> Result<()> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_check_zero();
        self.push_check_zero_list(*value)
    }

    /// Add two values.
    pub(crate) fn add(&mut self, a: &MacProver<FE>, b: &MacProver<FE>) -> Result<MacProver<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_add();
        Ok(self.prover.get_refmut().add(*a, *b))
    }

    /// Multiply two values.
    pub(crate) fn mul(&mut self, a: &MacProver<FE>, b: &MacProver<FE>) -> Result<MacProver<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_mul();
        let a_clr = a.value();
        let b_clr = b.value();
        let product = a_clr * b_clr;

        let out = self.input(product)?;
        self.prover
            .get_refmut()
            .quicksilver_push(&mut self.state_mult_check, &(*a, *b, out))?;
        Ok(out)
    }

    /// Add a value and a constant.
    pub(crate) fn addc(&mut self, a: &MacProver<FE>, b: FE::PrimeField) -> Result<MacProver<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_addc();
        Ok(self.prover.get_refmut().affine_add_cst(b, *a))
    }

    /// Multiply a value and a constant.
    pub(crate) fn mulc(
        &mut self,
        value: &MacProver<FE>,
        constant: FE::PrimeField,
    ) -> Result<MacProver<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_mulc();
        Ok(self.prover.get_refmut().affine_mult_cst(constant, *value))
    }

    /// Input a public value.
    pub(crate) fn input_public(&mut self, value: FieldClear<FE>) -> MacProver<FE> {
        self.monitor.incr_monitor_instance();
        MacProver::new(value, FE::ZERO)
    }

    /// Input a private value.
    pub(crate) fn input_private(&mut self, value: FieldClear<FE>) -> Result<MacProver<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_witness();
        self.input(value)
    }

    /// `finalize` execute its queued multiplication and zero checks.
    /// It can be called at any time and it is also called when the functionality is dropped.
    pub fn finalize(&mut self) -> Result<()> {
        debug!("finalize");
        self.check_is_ok()?;
        self.channel.flush()?;
        let zero_len = self.check_zero_list.len();
        self.do_check_zero()?;

        let mult_len = self.do_mult_check()?;
        debug!("ERASE ME:  mult_len {:?}", mult_len);
        debug!(
            "finalize: mult_check:{:?}, check_zero:{:?} ",
            mult_len, zero_len
        );
        self.log_final_monitor();
        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        self.prover.get_refmut().reset(&mut self.state_mult_check);
        self.is_ok = true;
    }

    fn log_final_monitor(&self) {
        info!("field largest value: {:?}", (FE::ZERO - FE::ONE).to_bytes());
        self.monitor.log_final_monitor();
    }
}

impl<FE: FiniteField, C: AbstractChannel, RNG: CryptoRng + Rng> Drop
    for DietMacAndCheeseProver<FE, C, RNG>
{
    fn drop(&mut self) {
        if self.is_ok && !self.check_zero_list.is_empty() {
            warn!("Dropped in unexpected state: either `finalize()` has not been called or an error occured earlier.");
        }
    }
}

/// Verifier for Diet Mac'n'Cheese.
pub struct DietMacAndCheeseVerifier<FE: FiniteField, C: AbstractChannel, RNG: CryptoRng + Rng> {
    verifier: RcRefCell<FComVerifier<FE>>,
    pub channel: C,
    pub rng: RNG,
    check_zero_list: Vec<MacVerifier<FE>>,
    monitor: Monitor,
    state_mult_check: StateMultCheckVerifier<FE>,
    is_ok: bool,
    no_batching: bool,
}

impl<FE: FiniteField, C: AbstractChannel, RNG: CryptoRng + Rng>
    DietMacAndCheeseVerifier<FE, C, RNG>
{
    /// Initialize the verifier by providing a channel, a random generator and a pair of LPN parameters as defined by svole.
    pub fn init(
        channel: &mut C,
        mut rng: RNG,
        lpn_setup: LpnParams,
        lpn_extend: LpnParams,
        no_batching: bool,
    ) -> Result<Self> {
        let state_mult_check = StateMultCheckVerifier::init(channel, &mut rng)?;
        Ok(Self {
            verifier: RcRefCell::new(FComVerifier::init(
                channel, &mut rng, lpn_setup, lpn_extend,
            )?),
            channel: channel.clone(),
            rng,
            check_zero_list: Vec::new(),
            monitor: Monitor::default(),
            state_mult_check,
            is_ok: true,
            no_batching,
        })
    }

    /// Initialize the verifier by providing a reference to a fcom.
    pub fn init_with_fcom(
        channel: &mut C,
        mut rng: RNG,
        fcom: &RcRefCell<FComVerifier<FE>>,
        no_batching: bool,
    ) -> Result<Self> {
        let state_mult_check = StateMultCheckVerifier::init(channel, &mut rng)?;
        Ok(Self {
            is_ok: true,
            verifier: fcom.clone(),
            channel: channel.clone(),
            rng,
            check_zero_list: Vec::new(),
            monitor: Monitor::default(),
            state_mult_check,
            no_batching,
        })
    }

    /// Get party
    pub(crate) fn get_party(&mut self) -> &RcRefCell<FComVerifier<FE>> {
        &self.verifier
    }

    // this function should be called before every function exposed publicly by the API.
    fn check_is_ok(&self) -> Result<()> {
        if !self.is_ok {
            return Err(eyre!(
                "An error occurred earlier. This functionality should not be used further"
            ));
        }
        Ok(())
    }

    fn input(&mut self) -> Result<MacVerifier<FE>> {
        let tag = self
            .verifier
            .get_refmut()
            .input1(&mut self.channel, &mut self.rng);
        if tag.is_err() {
            self.is_ok = false;
        }
        tag
    }

    fn do_mult_check(&mut self) -> Result<usize> {
        debug!("do mult_check");
        self.channel.flush()?;
        let cnt = self.verifier.get_refmut().quicksilver_finalize(
            &mut self.channel,
            &mut self.rng,
            &mut self.state_mult_check,
        )?;
        self.monitor.incr_zk_mult_check(cnt);
        Ok(cnt)
    }

    fn do_check_zero(&mut self) -> Result<()> {
        // debug!("do check_zero");
        self.channel.flush()?;
        let r = self.verifier.get_refmut().check_zero(
            &mut self.channel,
            &mut self.rng,
            &self.check_zero_list,
        );
        if r.is_err() {
            warn!("check_zero fails");
            self.is_ok = false;
        }
        self.monitor.incr_zk_check_zero(self.check_zero_list.len());
        self.check_zero_list.clear();
        r
    }

    fn push_check_zero_list(&mut self, e: MacVerifier<FE>) -> Result<()> {
        self.check_zero_list.push(e);

        if self.check_zero_list.len() == QUEUE_CAPACITY || self.no_batching {
            self.do_check_zero()?;
        }
        Ok(())
    }

    /// Assert a value is zero.
    pub(crate) fn assert_zero(&mut self, value: &MacVerifier<FE>) -> Result<()> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_check_zero();
        self.push_check_zero_list(*value)
    }

    /// Add two values.
    pub(crate) fn add(
        &mut self,
        a: &MacVerifier<FE>,
        b: &MacVerifier<FE>,
    ) -> Result<MacVerifier<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_add();
        Ok(self.verifier.get_refmut().add(*a, *b))
    }

    /// Multiply two values.
    pub(crate) fn mul(
        &mut self,
        a: &MacVerifier<FE>,
        b: &MacVerifier<FE>,
    ) -> Result<MacVerifier<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_mul();
        let tag = self.input()?;
        self.verifier
            .get_refmut()
            .quicksilver_push(&mut self.state_mult_check, &(*a, *b, tag))?;
        Ok(tag)
    }

    /// Add a value and a constant.
    pub(crate) fn addc(
        &mut self,
        a: &MacVerifier<FE>,
        b: FE::PrimeField,
    ) -> Result<MacVerifier<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_addc();
        Ok(self.verifier.get_refmut().affine_add_cst(b, *a))
    }

    /// Multiply a value and a constant.
    pub(crate) fn mulc(
        &mut self,
        a: &MacVerifier<FE>,
        b: FE::PrimeField,
    ) -> Result<MacVerifier<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_mulc();
        Ok(self.verifier.get_refmut().affine_mult_cst(b, *a))
    }

    /// Input a public value and wraps it in a verifier value.
    pub(crate) fn input_public(&mut self, val: FieldClear<FE>) -> MacVerifier<FE> {
        self.monitor.incr_monitor_instance();
        MacVerifier::new(-val * self.get_party().get_refmut().get_delta())
    }

    /// Input a private value and verifier value.
    pub(crate) fn input_private(&mut self) -> Result<MacVerifier<FE>> {
        self.check_is_ok()?;
        self.monitor.incr_monitor_witness();
        self.input()
    }

    /// `finalize` execute its internal queued multiplication and zero checks.
    /// It can be called at any time and it is also be called when the functionality is dropped.
    pub fn finalize(&mut self) -> Result<()> {
        debug!("finalize");
        self.check_is_ok()?;
        self.channel.flush()?;
        let zero_len = self.check_zero_list.len();
        self.do_check_zero()?;

        let mult_len = self.do_mult_check()?;
        debug!(
            "finalize: mult_check:{:?}, check_zero:{:?} ",
            mult_len, zero_len
        );
        self.log_final_monitor();
        Ok(())
    }

    fn log_final_monitor(&self) {
        info!("field largest value: {:?}", (FE::ZERO - FE::ONE).to_bytes());
        self.monitor.log_final_monitor();
    }

    pub(crate) fn reset(&mut self) {
        self.verifier.get_refmut().reset(&mut self.state_mult_check);
        self.is_ok = true;
    }
}

impl<FE: FiniteField, C: AbstractChannel, RNG: CryptoRng + Rng> Drop
    for DietMacAndCheeseVerifier<FE, C, RNG>
{
    fn drop(&mut self) {
        if self.is_ok && !self.check_zero_list.is_empty() {
            warn!("Dropped in unexpected state: either `finalize()` has not been called or an error occured earlier.");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        backend::{DietMacAndCheeseProver, DietMacAndCheeseVerifier},
        backend_trait::BackendT,
    };
    use ocelot::svole::wykw::{LPN_EXTEND_SMALL, LPN_SETUP_SMALL};
    use rand::SeedableRng;
    use scuttlebutt::{field::F40b, ring::FiniteRing};
    use scuttlebutt::{
        field::{F61p, FiniteField},
        AesRng, Channel,
    };
    use std::{
        io::{BufReader, BufWriter},
        os::unix::net::UnixStream,
    };

    fn test<FE: FiniteField>() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let rng = AesRng::from_seed(Default::default());
            let reader = BufReader::new(sender.try_clone().unwrap());
            let writer = BufWriter::new(sender);
            let mut channel = Channel::new(reader, writer);

            let mut dmc: DietMacAndCheeseProver<FE, _, _> = DietMacAndCheeseProver::init(
                &mut channel,
                rng,
                LPN_SETUP_SMALL,
                LPN_EXTEND_SMALL,
                false,
            )
            .unwrap();

            // one1        = public(1)
            // one2        = public(1)
            // two_pub     = add(one1, one2)
            // three_pub   = addc(two_pub, 1)
            // two_priv    = priv(2)
            // six         = mul(two_priv, three_pub)
            // twelve_priv = mulc(six, 2)
            // n24_priv    = mul(twelve_priv, two_priv)
            // r_zero_priv = addc(n24_priv, -24)
            // assert_zero(r_zero_priv)
            // assert_zero(n24_priv) !!!!FAIL!!!!!
            let one = FE::PrimeField::ONE;
            let two = one + one;
            let three = two + one;
            let one1 = dmc.input_public(one);
            let one2 = dmc.input_public(one);
            let two_pub = dmc.add(&one1, &one2).unwrap();
            assert_eq!(two_pub, dmc.input_public(two));
            let three_pub = dmc.addc(&two_pub, FE::PrimeField::ONE).unwrap();
            assert_eq!(three_pub, dmc.input_public(three));
            let two_priv = dmc
                .input_private(FE::PrimeField::ONE + FE::PrimeField::ONE)
                .unwrap();
            let six = dmc.mul(&two_priv, &three_pub).unwrap();
            let twelve_priv = dmc.mulc(&six, two).unwrap();
            assert_eq!(twelve_priv.value(), three * two * two);
            let n24_priv = dmc.mul(&twelve_priv, &two_priv).unwrap();
            let r_zero_priv = dmc.addc(&n24_priv, -(three * two * two * two)).unwrap();
            dmc.assert_zero(&r_zero_priv).unwrap();
            dmc.finalize().unwrap();
            dmc.assert_zero(&n24_priv).unwrap();
            assert!(dmc.finalize().is_err());
        });

        let rng = AesRng::from_seed(Default::default());
        let reader = BufReader::new(receiver.try_clone().unwrap());
        let writer = BufWriter::new(receiver);
        let mut channel = Channel::new(reader, writer);

        let mut dmc: DietMacAndCheeseVerifier<FE, _, _> = DietMacAndCheeseVerifier::init(
            &mut channel,
            rng,
            LPN_SETUP_SMALL,
            LPN_EXTEND_SMALL,
            false,
        )
        .unwrap();

        let one = FE::PrimeField::ONE;
        let two = one + one;
        let three = two + one;
        let one1 = dmc.input_public(one);
        let one2 = dmc.input_public(one);
        let two_pub = dmc.add(&one1, &one2).unwrap();
        let three_pub = dmc.addc(&two_pub, FE::PrimeField::ONE).unwrap();
        let two_priv = dmc.input_private().unwrap();
        let six = dmc.mul(&two_priv, &three_pub).unwrap();
        let twelve_priv = dmc.mulc(&six, two).unwrap();
        let n24_priv = dmc.mul(&twelve_priv, &two_priv).unwrap();
        let r_zero_priv = dmc.addc(&n24_priv, -(three * two * two * two)).unwrap();
        dmc.assert_zero(&r_zero_priv).unwrap();
        dmc.finalize().unwrap();
        dmc.assert_zero(&n24_priv).unwrap();
        assert!(dmc.finalize().is_err());

        handle.join().unwrap();
    }

    fn test_challenge<F: FiniteField>() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            let rng = AesRng::from_seed(Default::default());
            let reader = BufReader::new(sender.try_clone().unwrap());
            let writer = BufWriter::new(sender);
            let mut channel = Channel::new(reader, writer);

            let mut dmc: DietMacAndCheeseProver<F, _, _> = DietMacAndCheeseProver::init(
                &mut channel,
                rng,
                LPN_SETUP_SMALL,
                LPN_EXTEND_SMALL,
                false,
            )
            .unwrap();

            let challenge = dmc.challenge().unwrap();

            dmc.finalize().unwrap();

            challenge
        });

        let rng = AesRng::from_seed(Default::default());
        let reader = BufReader::new(receiver.try_clone().unwrap());
        let writer = BufWriter::new(receiver);
        let mut channel = Channel::new(reader, writer);

        let mut dmc: DietMacAndCheeseVerifier<F, _, _> = DietMacAndCheeseVerifier::init(
            &mut channel,
            rng,
            LPN_SETUP_SMALL,
            LPN_EXTEND_SMALL,
            false,
        )
        .unwrap();

        let challenge = dmc.challenge().unwrap();
        dmc.finalize().unwrap();

        let prover_challenge = handle.join().unwrap();
        assert_eq!(prover_challenge.mac(), challenge.mac());
    }

    #[test]
    fn test_f61p() {
        test::<F61p>();
        test_challenge::<F61p>();
    }

    #[test]
    fn test_f40b() {
        test_challenge::<F40b>();
    }
}
