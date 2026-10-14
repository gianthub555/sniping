



















#![forbid(unsafe_code)]

#![allow(unknown_lints)]
#![allow(
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::manual_range_contains,
    clippy::manual_div_ceil,
    clippy::manual_is_multiple_of,
    clippy::question_mark,
    clippy::identity_op,
    clippy::cloned_ref_to_slice_refs
)]

use std::cmp::Ordering;
use std::fmt;






pub const PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

pub const TICK_ARRAY_SIZE: i32 = 60;

pub const TICK_ARRAY_BITMAP_SIZE: i32 = 512;

pub const EXTENSION_TICKARRAY_BITMAP_SIZE: usize = 14;
pub const MIN_TICK: i32 = -443636;
pub const MAX_TICK: i32 = 443636;
pub const MIN_SQRT_PRICE_X64: u128 = 4295048016;
pub const MAX_SQRT_PRICE_X64: u128 = 79226673521066979257578248091;
pub const FEE_RATE_DENOMINATOR_VALUE: u32 = 1_000_000;

pub const MAX_FEE_RATE_NUMERATOR: u32 = 100_000;
pub const VOLATILITY_ACCUMULATOR_SCALE: u32 = 10_000;
pub const REDUCTION_FACTOR_DENOMINATOR: u32 = 10_000;
pub const DYNAMIC_FEE_CONTROL_DENOMINATOR: u32 = 100_000;
const Q64: u128 = 1u128 << 64;

pub const POOL_STATE_LEN: usize = 1544;
pub const AMM_CONFIG_LEN: usize = 117;
pub const TICK_ARRAY_LEN: usize = 10240;
pub const TICK_STATE_LEN: usize = 168;
pub const BITMAP_EXTENSION_LEN: usize = 1832;
pub const DYNAMIC_FEE_INFO_OFFSET: usize = 1096;
pub const DYNAMIC_FEE_INFO_LEN: usize = 80;


pub const POOL_STATE_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];

pub const AMM_CONFIG_DISCRIMINATOR: [u8; 8] = [0xda, 0xf4, 0x21, 0x68, 0xcb, 0xcb, 0x2b, 0x6f];

pub const TICK_ARRAY_DISCRIMINATOR: [u8; 8] = [0xc0, 0x9b, 0x55, 0xcd, 0x31, 0xf9, 0x81, 0x2a];

pub const BITMAP_EXTENSION_DISCRIMINATOR: [u8; 8] = [0x3c, 0x96, 0x24, 0xdb, 0x61, 0x80, 0x8b, 0x99];

pub const SWAP_EVENT_DISCRIMINATOR: [u8; 8] = [0x40, 0xc6, 0xcd, 0xe8, 0x26, 0x08, 0x71, 0xe2];






#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClmmError {
    
    InvalidAccountData(&'static str),
    ZeroAmountSpecified,
    /// Swap disabled by `PoolState.status` bit 4.
    NotApproved,
    /// `require_gt!(block_timestamp, pool.open_time)` failed.
    PoolNotOpen,
    SqrtPriceLimitOverflow,
    NotEnoughTickArrayAccount,
    InvalidFirstTickArrayAccount,
    InsufficientLiquidityForDirection,
    /// No further initialized tick array in the swap direction.
    LiquidityInsufficient,
    MissingTickArrayBitmapExtensionAccount,
    InvalidTickArray,
    InvalidTickIndex,
    InvalidTickArrayBoundary,
    TickUpperOverflow,
    SqrtPriceX64,
    ZeroLiquidity,
    ZeroSqrtPrice,
    CalculateOverflow,
    MaxTokenOverflow,
    LiquiditySubValueErr,
    LiquidityAddValueErr,
    InvalidLimitOrderAmount,
    TooSmallInputOrOutputAmount,
    /// The swap could not consume the whole specified amount. With `sqrt_price_limit_x64 == 0`
    /// the program rejects this (`require_eq!(amount_specified, transfer_amount)`).
    PartialFill { specified: u64, filled: u64 },
    /// A `require!`/`require_eq!` without a dedicated error code.
    RequireFailed(&'static str),
}

impl fmt::Display for ClmmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for ClmmError {}

type Result<T> = std::result::Result<T, ClmmError>;

// ---------------------------------------------------------------------------------------------
// Wide integers (replacement for the `uint` crate types used on-chain)
// ---------------------------------------------------------------------------------------------

/// 256-bit unsigned integer, little-endian `u64` limbs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct U256(pub [u64; 4]);

impl U256 {
    pub const ZERO: U256 = U256([0; 4]);
    pub const MAX: U256 = U256([u64::MAX; 4]);

    #[inline]
    pub const fn from_u64(v: u64) -> U256 {
        U256([v, 0, 0, 0])
    }

    #[inline]
    pub const fn from_u128(v: u128) -> U256 {
        U256([v as u64, (v >> 64) as u64, 0, 0])
    }

    #[inline]
    pub fn is_zero(&self) -> bool {
        (self.0[0] | self.0[1] | self.0[2] | self.0[3]) == 0
    }

    #[inline]
    pub fn to_u64(&self) -> Option<u64> {
        if (self.0[1] | self.0[2] | self.0[3]) == 0 {
            Some(self.0[0])
        } else {
            None
        }
    }

    #[inline]
    pub fn to_u128(&self) -> Option<u128> {
        if (self.0[2] | self.0[3]) == 0 {
            Some(((self.0[1] as u128) << 64) | self.0[0] as u128)
        } else {
            None
        }
    }

    /// Exact `a * b`.
    #[inline]
    pub fn mul_u128(a: u128, b: u128) -> U256 {
        let r = full_mul_256(&U256::from_u128(a).0, &U256::from_u128(b).0);
        U256([r[0], r[1], r[2], r[3]])
    }

    #[inline]
    pub fn checked_add(self, o: U256) -> Option<U256> {
        let mut r = [0u64; 4];
        let mut carry = 0u64;
        for i in 0..4 {
            let (s1, c1) = self.0[i].overflowing_add(o.0[i]);
            let (s2, c2) = s1.overflowing_add(carry);
            r[i] = s2;
            carry = (c1 as u64) + (c2 as u64);
        }
        if carry != 0 {
            None
        } else {
            Some(U256(r))
        }
    }

    #[inline]
    pub fn checked_sub(self, o: U256) -> Option<U256> {
        let mut r = [0u64; 4];
        let mut borrow = 0u64;
        for i in 0..4 {
            let (s1, b1) = self.0[i].overflowing_sub(o.0[i]);
            let (s2, b2) = s1.overflowing_sub(borrow);
            r[i] = s2;
            borrow = (b1 as u64) + (b2 as u64);
        }
        if borrow != 0 {
            None
        } else {
            Some(U256(r))
        }
    }

    /// Logical shift right (bits shifted out are dropped). `shift >= 256` yields zero.
    #[inline]
    pub fn shr_bits(self, shift: u32) -> U256 {
        if shift >= 256 {
            return U256::ZERO;
        }
        let ws = (shift / 64) as usize;
        let bs = shift % 64;
        let mut r = [0u64; 4];
        for i in 0..(4 - ws) {
            let mut v = self.0[i + ws] >> bs;
            if bs > 0 && i + ws + 1 < 4 {
                v |= self.0[i + ws + 1] << (64 - bs);
            }
            r[i] = v;
        }
        U256(r)
    }

    /// `floor(self * num / denom)`; `None` if `denom == 0` or the result exceeds 256 bits.
    pub fn mul_div_floor(self, num: U256, denom: U256) -> Option<U256> {
        if denom.is_zero() {
            return None;
        }
        let prod = full_mul_256(&self.0, &num.0);
        let (q, _) = divrem_512_by_256(&prod, &denom.0)?;
        low_256_if_fits(&q)
    }

    /// `ceil(self * num / denom)`; `None` if `denom == 0` or the result exceeds 256 bits.
    pub fn mul_div_ceil(self, num: U256, denom: U256) -> Option<U256> {
        if denom.is_zero() {
            return None;
        }
        let prod = full_mul_256(&self.0, &num.0);
        let (mut q, r) = divrem_512_by_256(&prod, &denom.0)?;
        if r != [0u64; 4] && !inc_limbs(&mut q) {
            return None;
        }
        low_256_if_fits(&q)
    }
}

impl PartialOrd for U256 {
    fn partial_cmp(&self, other: &U256) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for U256 {
    fn cmp(&self, other: &U256) -> Ordering {
        cmp_limbs(&self.0, &other.0)
    }
}

#[inline]
fn cmp_limbs(a: &[u64], b: &[u64]) -> Ordering {
    for i in (0..a.len()).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => continue,
            o => return o,
        }
    }
    Ordering::Equal
}

#[inline]
fn inc_limbs(x: &mut [u64]) -> bool {
    for limb in x.iter_mut() {
        let (v, c) = limb.overflowing_add(1);
        *limb = v;
        if !c {
            return true;
        }
    }
    false
}

#[inline]
fn low_256_if_fits(q: &[u64; 8]) -> Option<U256> {
    if (q[4] | q[5] | q[6] | q[7]) != 0 {
        None
    } else {
        Some(U256([q[0], q[1], q[2], q[3]]))
    }
}

/// Exact 256 x 256 -> 512 bit multiply.
#[inline]
fn full_mul_256(a: &[u64; 4], b: &[u64; 4]) -> [u64; 8] {
    let mut r = [0u64; 8];
    for i in 0..4 {
        if a[i] == 0 {
            continue;
        }
        let mut carry: u128 = 0;
        for j in 0..4 {
            let t = (a[i] as u128) * (b[j] as u128) + (r[i + j] as u128) + carry;
            r[i + j] = t as u64;
            carry = t >> 64;
        }
        r[i + 4] = carry as u64;
    }
    r
}

/// Knuth algorithm D (Hacker's Delight `divmnu64`) on 64-bit limbs: 512-bit / 256-bit.

fn divrem_512_by_256(num: &[u64; 8], den: &[u64; 4]) -> Option<([u64; 8], [u64; 4])> {
    let n = match den.iter().rposition(|&x| x != 0) {
        Some(i) => i + 1,
        None => return None,
    };
    let m = match num.iter().rposition(|&x| x != 0) {
        Some(i) => i + 1,
        None => return Some(([0; 8], [0; 4])),
    };
    let mut q = [0u64; 8];
    if m < n {
        let mut r = [0u64; 4];
        r[..m].copy_from_slice(&num[..m]);
        return Some((q, r));
    }
    if n == 1 {
        let d = den[0] as u128;
        let mut rem: u128 = 0;
        for i in (0..m).rev() {
            let cur = (rem << 64) | num[i] as u128;
            q[i] = (cur / d) as u64;
            rem = cur % d;
        }
        return Some((q, [rem as u64, 0, 0, 0]));
    }
    let s = den[n - 1].leading_zeros();
    let mut vn = [0u64; 4];
    let mut un = [0u64; 9];
    if s == 0 {
        vn[..n].copy_from_slice(&den[..n]);
        un[..m].copy_from_slice(&num[..m]);
        un[m] = 0;
    } else {
        for i in (1..n).rev() {
            vn[i] = (den[i] << s) | (den[i - 1] >> (64 - s));
        }
        vn[0] = den[0] << s;
        un[m] = num[m - 1] >> (64 - s);
        for i in (1..m).rev() {
            un[i] = (num[i] << s) | (num[i - 1] >> (64 - s));
        }
        un[0] = num[0] << s;
    }
    const B: u128 = 1u128 << 64;
    let vtop = vn[n - 1] as u128;
    let vsec = vn[n - 2] as u128;
    for j in (0..=(m - n)).rev() {
        let num2 = ((un[j + n] as u128) << 64) | un[j + n - 1] as u128;
        let mut qhat = num2 / vtop;
        let mut rhat = num2 % vtop;
        loop {
            if qhat >= B || qhat * vsec > ((rhat << 64) | un[j + n - 2] as u128) {
                qhat -= 1;
                rhat += vtop;
                if rhat < B {
                    continue;
                }
            }
            break;
        }
        
        let mut borrow: u64 = 0;
        let mut carry: u128 = 0;
        for i in 0..n {
            let p = qhat * (vn[i] as u128) + carry;
            carry = p >> 64;
            let (t1, o1) = un[i + j].overflowing_sub(p as u64);
            let (t2, o2) = t1.overflowing_sub(borrow);
            un[i + j] = t2;
            borrow = (o1 as u64) + (o2 as u64);
        }
        let (t1, o1) = un[j + n].overflowing_sub(carry as u64);
        let (t2, o2) = t1.overflowing_sub(borrow);
        un[j + n] = t2;
        if o1 || o2 {
            
            #[cfg(test)]
            test_hooks::note_add_back();
            qhat -= 1;
            let mut c: u128 = 0;
            for i in 0..n {
                let sum = un[i + j] as u128 + vn[i] as u128 + c;
                un[i + j] = sum as u64;
                c = sum >> 64;
            }
            un[j + n] = un[j + n].wrapping_add(c as u64);
        }
        q[j] = qhat as u64;
    }
    let mut r = [0u64; 4];
    if s == 0 {
        r[..n].copy_from_slice(&un[..n]);
    } else {
        for i in 0..n {
            r[i] = (un[i] >> s) | (un[i + 1] << (64 - s));
        }
    }
    Some((q, r))
}

#[cfg(test)]
mod test_hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static ADD_BACKS: AtomicUsize = AtomicUsize::new(0);
    pub fn note_add_back() {
        ADD_BACKS.fetch_add(1, Ordering::Relaxed);
    }
}


#[inline]
fn mul_pow2_div(x: U256, shift: u32, denom: U256, round_up: bool) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    let ws = (shift / 64) as usize;
    let bs = shift % 64;
    let mut n = [0u64; 8];
    for i in 0..4 {
        n[i + ws] |= x.0[i] << bs;
        if bs > 0 {
            n[i + ws + 1] |= x.0[i] >> (64 - bs);
        }
    }
    let (mut q, r) = divrem_512_by_256(&n, &denom.0)?;
    if round_up && r != [0u64; 4] && !inc_limbs(&mut q) {
        return None;
    }
    low_256_if_fits(&q)
}


#[inline]
pub fn mul_div_floor_u128(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    if let Some(p) = a.checked_mul(b) {
        return Some(p / d);
    }
    U256::mul_u128(a, b)
        .mul_div_floor(U256::from_u64(1), U256::from_u128(d))
        .and_then(|r| r.to_u128())
}


#[inline]
pub fn mul_div_ceil_u128(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    if let Some(p) = a.checked_mul(b) {
        if let Some(n) = p.checked_add(d - 1) {
            return Some(n / d);
        }
    }
    U256::mul_u128(a, b)
        .mul_div_ceil(U256::from_u64(1), U256::from_u128(d))
        .and_then(|r| r.to_u128())
}


#[inline]
pub fn mul_div_floor_u64(a: u64, b: u64, d: u64) -> Option<u64> {
    if d == 0 {
        return None;
    }
    let r = (a as u128 * b as u128) / d as u128;
    if r > u64::MAX as u128 {
        None
    } else {
        Some(r as u64)
    }
}


#[inline]
pub fn mul_div_ceil_u64(a: u64, b: u64, d: u64) -> Option<u64> {
    if d == 0 {
        return None;
    }
    let r = (a as u128 * b as u128 + (d as u128 - 1)) / d as u128;
    if r > u64::MAX as u128 {
        None
    } else {
        Some(r as u64)
    }
}





const SQRT_FACTORS: [(u32, u128); 18] = [
    (0x2, 0xfff97272373d4000),
    (0x4, 0xfff2e50f5f657000),
    (0x8, 0xffe5caca7e10f000),
    (0x10, 0xffcb9843d60f7000),
    (0x20, 0xff973b41fa98e800),
    (0x40, 0xff2ea16466c9b000),
    (0x80, 0xfe5dee046a9a3800),
    (0x100, 0xfcbe86c7900bb000),
    (0x200, 0xf987a7253ac65800),
    (0x400, 0xf3392b0822bb6000),
    (0x800, 0xe7159475a2caf000),
    (0x1000, 0xd097f3bdfd2f2000),
    (0x2000, 0xa9f746462d9f8000),
    (0x4000, 0x70d869a156f31c00),
    (0x8000, 0x31be135f97ed3200),
    (0x10000, 0x9aa508b5b85a500),
    (0x20000, 0x5d6af8dedc582c),
    (0x40000, 0x2216e584f5fa),
];


pub fn get_sqrt_price_at_tick(tick: i32) -> Result<u128> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(ClmmError::TickUpperOverflow);
    }
    let abs_tick = tick.unsigned_abs();
    let mut ratio: u128 = if abs_tick & 0x1 != 0 {
        0xfffcb933bd6fb800
    } else {
        1u128 << 64
    };
    for &(bit, factor) in SQRT_FACTORS.iter() {
        if abs_tick & bit != 0 {
            ratio = (ratio * factor) >> 64;
        }
    }
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    Ok(ratio)
}


pub fn get_tick_at_sqrt_price(sqrt_price_x64: u128) -> Result<i32> {
    if !(sqrt_price_x64 >= MIN_SQRT_PRICE_X64 && sqrt_price_x64 < MAX_SQRT_PRICE_X64) {
        return Err(ClmmError::SqrtPriceX64);
    }
    let msb: u32 = 128 - sqrt_price_x64.leading_zeros() - 1;
    let log2p_integer_x32 = (msb as i128 - 64) << 32;

    let mut bit: i128 = 0x8000_0000_0000_0000i128;
    let mut precision = 0;
    let mut log2p_fraction_x64: i128 = 0;
    let mut r: u128 = if msb >= 64 {
        sqrt_price_x64 >> (msb - 63)
    } else {
        sqrt_price_x64 << (63 - msb)
    };
    while bit > 0 && precision < 16 {
        r *= r;
        let is_r_more_than_two = (r >> 127) as u32;
        r >>= 63 + is_r_more_than_two;
        log2p_fraction_x64 += bit * is_r_more_than_two as i128;
        bit >>= 1;
        precision += 1;
    }
    let log2p_fraction_x32 = log2p_fraction_x64 >> 32;
    let log2p_x32 = log2p_integer_x32 + log2p_fraction_x32;
    let log_sqrt_10001_x64 = log2p_x32 * 59543866431248i128;
    let tick_low = ((log_sqrt_10001_x64 - 184467440737095516i128) >> 64) as i32;
    let tick_high = ((log_sqrt_10001_x64 + 15793534762490258745i128) >> 64) as i32;
    if tick_low == tick_high {
        Ok(tick_low)
    } else if get_sqrt_price_at_tick(tick_high)? <= sqrt_price_x64 {
        Ok(tick_high)
    } else {
        Ok(tick_low)
    }
}


pub fn get_price_from_sqrt_price(sqrt_price_x64: u128, round_up: bool) -> Result<u128> {
    let mut prod = U256::mul_u128(sqrt_price_x64, sqrt_price_x64);
    if round_up {
        prod = prod
            .checked_add(U256::from_u128(Q64 - 1))
            .ok_or(ClmmError::CalculateOverflow)?;
    }
    prod.shr_bits(64).to_u128().ok_or(ClmmError::CalculateOverflow)
}





fn get_next_sqrt_price_from_amount_0_rounding_up(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    add: bool,
) -> Result<u128> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    
    let numerator_1 = U256([0, liquidity as u64, (liquidity >> 64) as u64, 0]);
    let product = U256::mul_u128(amount as u128, sqrt_price_x64);
    let denominator = if add {
        
        numerator_1
            .checked_add(product)
            .ok_or(ClmmError::CalculateOverflow)?
    } else {
        numerator_1
            .checked_sub(product)
            .ok_or(ClmmError::CalculateOverflow)?
    };
    numerator_1
        .mul_div_ceil(U256::from_u128(sqrt_price_x64), denominator)
        .ok_or(ClmmError::CalculateOverflow)?
        .to_u128()
        .ok_or(ClmmError::CalculateOverflow)
}

fn get_next_sqrt_price_from_amount_1_rounding_down(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    add: bool,
) -> Result<u128> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    if liquidity == 0 {
        return Err(ClmmError::ZeroLiquidity);
    }
    let x = (amount as u128) << 64;
    if add {
        sqrt_price_x64
            .checked_add(x / liquidity)
            .ok_or(ClmmError::CalculateOverflow)
    } else {
        let q = x / liquidity + ((x % liquidity > 0) as u128);
        sqrt_price_x64
            .checked_sub(q)
            .ok_or(ClmmError::CalculateOverflow)
    }
}

fn get_next_sqrt_price_from_input(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_in: u64,
    zero_for_one: bool,
) -> Result<u128> {
    if !(sqrt_price_x64 >= MIN_SQRT_PRICE_X64 && sqrt_price_x64 < MAX_SQRT_PRICE_X64) {
        return Err(ClmmError::SqrtPriceX64);
    }
    if liquidity == 0 {
        return Err(ClmmError::ZeroLiquidity);
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount_0_rounding_up(sqrt_price_x64, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount_1_rounding_down(sqrt_price_x64, liquidity, amount_in, true)
    }
}

fn get_next_sqrt_price_from_output(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_out: u64,
    zero_for_one: bool,
) -> Result<u128> {
    if !(sqrt_price_x64 >= MIN_SQRT_PRICE_X64 && sqrt_price_x64 < MAX_SQRT_PRICE_X64) {
        return Err(ClmmError::SqrtPriceX64);
    }
    if liquidity == 0 {
        return Err(ClmmError::ZeroLiquidity);
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount_1_rounding_down(sqrt_price_x64, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount_0_rounding_up(sqrt_price_x64, liquidity, amount_out, false)
    }
}



pub fn get_delta_amounts_for_swap(
    mut sqrt_ratio_a_x64: u128,
    mut sqrt_ratio_b_x64: u128,
    liquidity: u128,
    zero_for_one: bool,
) -> Result<(u64, u64)> {
    if sqrt_ratio_a_x64 > sqrt_ratio_b_x64 {
        std::mem::swap(&mut sqrt_ratio_a_x64, &mut sqrt_ratio_b_x64);
    }
    if sqrt_ratio_a_x64 == 0 {
        return Err(ClmmError::ZeroSqrtPrice);
    }
    let amount_1_x64 = U256::mul_u128(liquidity, sqrt_ratio_b_x64 - sqrt_ratio_a_x64);
    let product = U256::mul_u128(sqrt_ratio_a_x64, sqrt_ratio_b_x64);
    let (amount_in, amount_out) = if zero_for_one {
        let ain = mul_pow2_div(amount_1_x64, 64, product, true).ok_or(ClmmError::CalculateOverflow)?;
        (ain, amount_1_x64.shr_bits(64))
    } else {
        let ain = amount_1_x64
            .checked_add(U256::from_u64(u64::MAX))
            .ok_or(ClmmError::CalculateOverflow)?
            .shr_bits(64);
        let aout = mul_pow2_div(amount_1_x64, 64, product, false).ok_or(ClmmError::CalculateOverflow)?;
        (ain, aout)
    };
    let ain = amount_in.to_u64().ok_or(ClmmError::MaxTokenOverflow)?;
    let aout = amount_out.to_u64().ok_or(ClmmError::MaxTokenOverflow)?;
    Ok((ain, aout))
}

fn add_delta(x: u128, y: i128) -> Result<u128> {
    if y < 0 {
        let z = x
            .checked_sub(y.unsigned_abs())
            .ok_or(ClmmError::CalculateOverflow)?;
        if !(x > z) {
            return Err(ClmmError::LiquiditySubValueErr);
        }
        Ok(z)
    } else {
        let z = x
            .checked_add(y.unsigned_abs())
            .ok_or(ClmmError::CalculateOverflow)?;
        Ok(z)
    }
}






#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapStep {
    pub sqrt_price_next_x64: u128,
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee_amount: u64,
}


pub fn compute_swap(
    sqrt_price_current_x64: u128,
    sqrt_price_target_x64: u128,
    liquidity: u128,
    amount_remaining: u64,
    fee_rate: u32,
    is_base_input: bool,
    zero_for_one: bool,
    is_fee_on_input: bool,
) -> Result<SwapStep> {
    let denom = FEE_RATE_DENOMINATOR_VALUE as u64;
    let one_minus_fee = || -> Result<u64> {
        FEE_RATE_DENOMINATOR_VALUE
            .checked_sub(fee_rate)
            .map(|v| v as u64)
            .ok_or(ClmmError::CalculateOverflow)
    };
    let mut result = SwapStep::default();
    let amount_for_price_calc = if is_base_input {
        if is_fee_on_input {
            mul_div_floor_u64(amount_remaining, one_minus_fee()?, denom)
                .ok_or(ClmmError::CalculateOverflow)?
        } else {
            amount_remaining
        }
    } else if is_fee_on_input {
        amount_remaining
    } else {
        mul_div_ceil_u64(amount_remaining, denom, one_minus_fee()?)
            .ok_or(ClmmError::CalculateOverflow)?
    };

    let max_reachable = match get_delta_amounts_for_swap(
        sqrt_price_target_x64,
        sqrt_price_current_x64,
        liquidity,
        zero_for_one,
    ) {
        Ok((ain, aout)) => {
            let user_limit = if is_base_input { ain } else { aout };
            if amount_for_price_calc >= user_limit {
                Some((ain, aout))
            } else {
                None
            }
        }
        Err(ClmmError::MaxTokenOverflow) => None,
        Err(e) => return Err(e),
    };

    if let Some((ain, aout)) = max_reachable {
        result.sqrt_price_next_x64 = sqrt_price_target_x64;
        result.amount_in = ain;
        result.amount_out = aout;
    } else {
        let sqrt_next = if is_base_input {
            get_next_sqrt_price_from_input(
                sqrt_price_current_x64,
                liquidity,
                amount_for_price_calc,
                zero_for_one,
            )?
        } else {
            get_next_sqrt_price_from_output(
                sqrt_price_current_x64,
                liquidity,
                amount_for_price_calc,
                zero_for_one,
            )?
        };
        result.sqrt_price_next_x64 = sqrt_next;
        let (ain, aout) =
            get_delta_amounts_for_swap(sqrt_next, sqrt_price_current_x64, liquidity, zero_for_one)?;
        result.amount_in = ain;
        result.amount_out = aout;
    }

    if zero_for_one {
        if result.sqrt_price_next_x64 < sqrt_price_target_x64 {
            return Err(ClmmError::RequireFailed("compute_swap: sqrt_next >= target"));
        }
    } else if sqrt_price_target_x64 < result.sqrt_price_next_x64 {
        return Err(ClmmError::RequireFailed("compute_swap: target >= sqrt_next"));
    }

    if is_base_input {
        if is_fee_on_input {
            if result.sqrt_price_next_x64 != sqrt_price_target_x64 {
                result.fee_amount = amount_remaining
                    .checked_sub(result.amount_in)
                    .ok_or(ClmmError::CalculateOverflow)?;
            } else {
                result.fee_amount = mul_div_ceil_u64(result.amount_in, fee_rate as u64, one_minus_fee()?)
                    .ok_or(ClmmError::CalculateOverflow)?;
            }
        } else {
            result.fee_amount = mul_div_ceil_u64(result.amount_out, fee_rate as u64, denom)
                .ok_or(ClmmError::CalculateOverflow)?;
            result.amount_out = result
                .amount_out
                .checked_sub(result.fee_amount)
                .ok_or(ClmmError::CalculateOverflow)?;
            if result.sqrt_price_next_x64 != sqrt_price_target_x64 {
                result.amount_in = amount_remaining;
            }
        }
    } else if is_fee_on_input {
        result.amount_out = result.amount_out.min(amount_remaining);
        result.fee_amount = mul_div_ceil_u64(result.amount_in, fee_rate as u64, one_minus_fee()?)
            .ok_or(ClmmError::CalculateOverflow)?;
    } else {
        result.fee_amount = mul_div_ceil_u64(result.amount_out, fee_rate as u64, denom)
            .ok_or(ClmmError::CalculateOverflow)?;
        let net_output = result
            .amount_out
            .checked_sub(result.fee_amount)
            .ok_or(ClmmError::CalculateOverflow)?;
        if net_output > amount_remaining {
            result.fee_amount = result
                .amount_out
                .checked_sub(amount_remaining)
                .ok_or(ClmmError::CalculateOverflow)?;
            result.amount_out = amount_remaining;
        } else {
            result.amount_out = net_output;
        }
    }
    Ok(result)
}






#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmmConfigState {
    
    pub trade_fee_rate: u32,
    pub tick_spacing: u16,
    
    pub protocol_fee_rate: u32,
    
    pub fund_fee_rate: u32,
    pub index: u16,
}


#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DynamicFeeInfo {
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub dynamic_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub tick_spacing_index_reference: i32,
    pub volatility_reference: u32,
    pub volatility_accumulator: u32,
    pub last_update_timestamp: u64,
}

impl DynamicFeeInfo {
    
    pub fn update_volatility_accumulator(&mut self, tick_spacing_index: i32) -> Result<()> {
        let index_delta = self
            .tick_spacing_index_reference
            .checked_sub(tick_spacing_index)
            .ok_or(ClmmError::CalculateOverflow)?
            .unsigned_abs();
        let va = self.volatility_reference as u64
            + index_delta as u64 * VOLATILITY_ACCUMULATOR_SCALE as u64;
        self.volatility_accumulator = va.min(self.max_volatility_accumulator as u64) as u32;
        Ok(())
    }

    
    pub fn update_reference(&mut self, tick_spacing_index: i32, current_timestamp: u64) {
        let dt = current_timestamp.saturating_sub(self.last_update_timestamp);
        if dt < self.filter_period as u64 {
            
        } else if dt < self.decay_period as u64 {
            self.tick_spacing_index_reference = tick_spacing_index;
            self.volatility_reference = (self.volatility_accumulator as u64
                * self.reduction_factor as u64
                / REDUCTION_FACTOR_DENOMINATOR as u64) as u32;
            self.last_update_timestamp = current_timestamp;
        } else {
            self.tick_spacing_index_reference = tick_spacing_index;
            self.volatility_reference = 0;
            self.last_update_timestamp = current_timestamp;
        }
    }

    
    pub fn dynamic_fee_rate(&self, tick_spacing: u16) -> Result<u32> {
        let crossed = self
            .volatility_accumulator
            .checked_mul(tick_spacing as u32)
            .ok_or(ClmmError::CalculateOverflow)?;
        let squared = crossed as u64 * crossed as u64;
        let denominator = DYNAMIC_FEE_CONTROL_DENOMINATOR as u128
            * VOLATILITY_ACCUMULATOR_SCALE as u128
            * VOLATILITY_ACCUMULATOR_SCALE as u128;
        let fee_rate = mul_div_ceil_u128(self.dynamic_fee_control as u128, squared as u128, denominator)
            .ok_or(ClmmError::CalculateOverflow)?;
        Ok(if fee_rate > MAX_FEE_RATE_NUMERATOR as u128 {
            MAX_FEE_RATE_NUMERATOR
        } else {
            fee_rate as u32
        })
    }
}


#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolSnapshot {
    pub amm_config: [u8; 32],
    pub token_mint_0: [u8; 32],
    pub token_mint_1: [u8; 32],
    pub token_vault_0: [u8; 32],
    pub token_vault_1: [u8; 32],
    pub observation_key: [u8; 32],
    pub mint_decimals_0: u8,
    pub mint_decimals_1: u8,
    pub tick_spacing: u16,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current: i32,
    pub status: u8,
    
    pub fee_on: u8,
    pub tick_array_bitmap: [u64; 16],
    pub open_time: u64,
    
    pub dynamic_fee: Option<DynamicFeeInfo>,
}

impl PoolSnapshot {
    
    pub fn is_fee_on_input(&self, zero_for_one: bool) -> bool {
        match self.fee_on {
            0 => true,
            1 => zero_for_one,
            2 => !zero_for_one,
            _ => true,
        }
    }

    
    pub fn is_fee_on_token0(&self, zero_for_one: bool) -> bool {
        match self.fee_on {
            0 => zero_for_one,
            1 => true,
            2 => false,
            _ => zero_for_one,
        }
    }

    
    pub fn swap_enabled(&self) -> bool {
        self.status & (1 << 4) == 0
    }
}


#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickState {
    pub tick: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
    pub order_phase: u64,
    pub orders_amount: u64,
    pub part_filled_orders_remaining: u64,
    pub unfilled_ratio_x64: u128,
}

impl TickState {
    pub fn is_initialized(&self) -> bool {
        self.has_liquidity() || self.has_limit_orders()
    }

    pub fn has_limit_orders(&self) -> bool {
        self.orders_amount > 0 || self.part_filled_orders_remaining > 0
    }

    pub fn has_liquidity(&self) -> bool {
        self.liquidity_gross > 0
    }

    pub fn limit_order_unfilled_amount(&self) -> Result<u64> {
        self.orders_amount
            .checked_add(self.part_filled_orders_remaining)
            .ok_or(ClmmError::CalculateOverflow)
    }

    fn limit_order_output_with_price(amount_in: u64, token_0_price_x64: u128, zero_for_one: bool) -> Result<u128> {
        if zero_for_one {
            mul_div_floor_u128(amount_in as u128, token_0_price_x64, Q64)
        } else {
            mul_div_floor_u128(amount_in as u128, Q64, token_0_price_x64)
        }
        .ok_or(ClmmError::CalculateOverflow)
    }

    fn limit_order_input_with_price(amount_out: u64, token_0_price_x64: u128, zero_for_one: bool) -> Result<u64> {
        let amount_in = if zero_for_one {
            mul_div_ceil_u128(amount_out as u128, token_0_price_x64, Q64)
        } else {
            mul_div_ceil_u128(amount_out as u128, Q64, token_0_price_x64)
        }
        .ok_or(ClmmError::CalculateOverflow)?;
        if amount_in > u64::MAX as u128 {
            return Err(ClmmError::CalculateOverflow);
        }
        Ok(amount_in as u64)
    }

    
    pub fn match_limit_order_with_sqrt_price(
        &mut self,
        swap_amount: u64,
        swap_direction_zero_for_one: bool,
        is_base_input: bool,
        fee_rate: u32,
        is_fee_on_input: bool,
        sqrt_price_x64: u128,
    ) -> Result<LimitOrderMatch> {
        let mut result = LimitOrderMatch::default();
        let total_unfilled_amount = self.limit_order_unfilled_amount()?;
        if swap_amount == 0 || total_unfilled_amount == 0 {
            return Ok(result);
        }
        let fee_den = FEE_RATE_DENOMINATOR_VALUE as u64;
        let one_minus_fee = || -> Result<u64> {
            FEE_RATE_DENOMINATOR_VALUE
                .checked_sub(fee_rate)
                .map(|v| v as u64)
                .ok_or(ClmmError::CalculateOverflow)
        };
        let token_0_price_x64 = get_price_from_sqrt_price(sqrt_price_x64, !swap_direction_zero_for_one)?;

        if is_base_input {
            if is_fee_on_input {
                result.amm_fee_amount = mul_div_ceil_u64(swap_amount, fee_rate as u64, fee_den)
                    .ok_or(ClmmError::CalculateOverflow)?;
                result.amount_in = swap_amount
                    .checked_sub(result.amm_fee_amount)
                    .ok_or(ClmmError::CalculateOverflow)?;
            } else {
                result.amount_in = swap_amount;
            }
            let matched_output = Self::limit_order_output_with_price(
                result.amount_in,
                token_0_price_x64,
                swap_direction_zero_for_one,
            )?;
            if matched_output > total_unfilled_amount as u128 {
                result.amount_out = total_unfilled_amount;
                result.amount_in = Self::limit_order_input_with_price(
                    total_unfilled_amount,
                    token_0_price_x64,
                    !swap_direction_zero_for_one,
                )?;
                if is_fee_on_input {
                    result.amm_fee_amount = mul_div_ceil_u64(result.amount_in, fee_rate as u64, one_minus_fee()?)
                        .ok_or(ClmmError::CalculateOverflow)?;
                }
            } else {
                result.amount_out = matched_output as u64;
            }
        } else {
            let net_output = swap_amount.min(total_unfilled_amount);
            result.amount_out = if is_fee_on_input {
                net_output
            } else {
                mul_div_ceil_u64(net_output, fee_den, one_minus_fee()?)
                    .ok_or(ClmmError::CalculateOverflow)?
                    .min(total_unfilled_amount)
            };
            result.amount_in = Self::limit_order_input_with_price(
                result.amount_out,
                token_0_price_x64,
                !swap_direction_zero_for_one,
            )?;
            if is_fee_on_input {
                result.amm_fee_amount = mul_div_ceil_u64(result.amount_in, fee_rate as u64, one_minus_fee()?)
                    .ok_or(ClmmError::CalculateOverflow)?;
            }
        }

        let mut consume_from_part_remaining = 0;
        if self.part_filled_orders_remaining > 0 {
            consume_from_part_remaining = self.part_filled_orders_remaining.min(result.amount_out);
            if consume_from_part_remaining > 0 {
                self.unfilled_ratio_x64 = mul_div_floor_u128(
                    self.unfilled_ratio_x64,
                    (self.part_filled_orders_remaining - consume_from_part_remaining) as u128,
                    self.part_filled_orders_remaining as u128,
                )
                .ok_or(ClmmError::CalculateOverflow)?;
            }
            self.part_filled_orders_remaining = self
                .part_filled_orders_remaining
                .saturating_sub(consume_from_part_remaining);
        }
        let amount_out_continue_to_consume = result.amount_out.saturating_sub(consume_from_part_remaining);
        if amount_out_continue_to_consume > 0 {
            if self.part_filled_orders_remaining != 0 {
                return Err(ClmmError::RequireFailed("part_filled_orders_remaining == 0"));
            }
            if self.orders_amount < amount_out_continue_to_consume {
                return Err(ClmmError::InvalidLimitOrderAmount);
            }
            self.order_phase = self.order_phase.saturating_add(1);
            self.unfilled_ratio_x64 = mul_div_floor_u128(
                Q64,
                (self.orders_amount - amount_out_continue_to_consume) as u128,
                self.orders_amount as u128,
            )
            .ok_or(ClmmError::CalculateOverflow)?;
            self.part_filled_orders_remaining = self.orders_amount - amount_out_continue_to_consume;
            self.orders_amount = 0;
        }
        if !is_fee_on_input {
            result.amm_fee_amount = mul_div_ceil_u64(result.amount_out, fee_rate as u64, fee_den)
                .ok_or(ClmmError::CalculateOverflow)?;
            result.amount_out = result
                .amount_out
                .checked_sub(result.amm_fee_amount)
                .ok_or(ClmmError::CalculateOverflow)?;
        }
        Ok(result)
    }
}


#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimitOrderMatch {
    pub amount_in: u64,
    pub amount_out: u64,
    pub amm_fee_amount: u64,
}


#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickArray {
    pub pool_id: [u8; 32],
    pub start_tick_index: i32,
    
    pub ticks: Vec<TickState>,
    
    pub initialized_tick_count: u8,
}



#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitmapExtension {
    pub pool_id: [u8; 32],
    pub positive: [[u64; 8]; EXTENSION_TICKARRAY_BITMAP_SIZE],
    pub negative: [[u64; 8]; EXTENSION_TICKARRAY_BITMAP_SIZE],
}

impl Default for BitmapExtension {
    fn default() -> Self {
        BitmapExtension {
            pool_id: [0; 32],
            positive: [[0; 8]; EXTENSION_TICKARRAY_BITMAP_SIZE],
            negative: [[0; 8]; EXTENSION_TICKARRAY_BITMAP_SIZE],
        }
    }
}





#[inline]
fn rd_u16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
#[inline]
fn rd_u32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}
#[inline]
fn rd_i32(d: &[u8], o: usize) -> i32 {
    rd_u32(d, o) as i32
}
#[inline]
fn rd_u64(d: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    u64::from_le_bytes(b)
}
#[inline]
fn rd_u128(d: &[u8], o: usize) -> u128 {
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[o..o + 16]);
    u128::from_le_bytes(b)
}
#[inline]
fn rd_key(d: &[u8], o: usize) -> [u8; 32] {
    let mut b = [0u8; 32];
    b.copy_from_slice(&d[o..o + 32]);
    b
}


pub fn parse_amm_config(data: &[u8]) -> Result<AmmConfigState> {
    if data.len() < AMM_CONFIG_LEN {
        return Err(ClmmError::InvalidAccountData("AmmConfig: length < 117"));
    }
    if data[..8] != AMM_CONFIG_DISCRIMINATOR {
        return Err(ClmmError::InvalidAccountData("AmmConfig: bad discriminator"));
    }
    let cfg = AmmConfigState {
        index: rd_u16(data, 9),
        protocol_fee_rate: rd_u32(data, 43),
        trade_fee_rate: rd_u32(data, 47),
        tick_spacing: rd_u16(data, 51),
        fund_fee_rate: rd_u32(data, 53),
    };
    if cfg.tick_spacing == 0 || cfg.trade_fee_rate >= FEE_RATE_DENOMINATOR_VALUE {
        return Err(ClmmError::InvalidAccountData("AmmConfig: invalid tick_spacing/trade_fee_rate"));
    }
    Ok(cfg)
}


pub fn parse_pool(data: &[u8]) -> Result<PoolSnapshot> {
    if data.len() != POOL_STATE_LEN {
        return Err(ClmmError::InvalidAccountData("PoolState: length != 1544"));
    }
    if data[..8] != POOL_STATE_DISCRIMINATOR {
        return Err(ClmmError::InvalidAccountData("PoolState: bad discriminator"));
    }
    let mut bitmap = [0u64; 16];
    for (i, w) in bitmap.iter_mut().enumerate() {
        *w = rd_u64(data, 904 + 8 * i);
    }
    let dfi = &data[DYNAMIC_FEE_INFO_OFFSET..DYNAMIC_FEE_INFO_OFFSET + DYNAMIC_FEE_INFO_LEN];
    let dynamic_fee = if dfi.iter().all(|&b| b == 0) {
        None
    } else {
        Some(DynamicFeeInfo {
            filter_period: rd_u16(dfi, 0),
            decay_period: rd_u16(dfi, 2),
            reduction_factor: rd_u16(dfi, 4),
            dynamic_fee_control: rd_u32(dfi, 6),
            max_volatility_accumulator: rd_u32(dfi, 10),
            tick_spacing_index_reference: rd_i32(dfi, 14),
            volatility_reference: rd_u32(dfi, 18),
            volatility_accumulator: rd_u32(dfi, 22),
            last_update_timestamp: rd_u64(dfi, 26),
        })
    };
    let pool = PoolSnapshot {
        amm_config: rd_key(data, 9),
        token_mint_0: rd_key(data, 73),
        token_mint_1: rd_key(data, 105),
        token_vault_0: rd_key(data, 137),
        token_vault_1: rd_key(data, 169),
        observation_key: rd_key(data, 201),
        mint_decimals_0: data[233],
        mint_decimals_1: data[234],
        tick_spacing: rd_u16(data, 235),
        liquidity: rd_u128(data, 237),
        sqrt_price_x64: rd_u128(data, 253),
        tick_current: rd_i32(data, 269),
        status: data[389],
        fee_on: data[390],
        tick_array_bitmap: bitmap,
        open_time: rd_u64(data, 1080),
        dynamic_fee,
    };
    if pool.tick_spacing == 0 {
        return Err(ClmmError::InvalidAccountData("PoolState: tick_spacing == 0"));
    }
    Ok(pool)
}


pub fn parse_tick_array(data: &[u8]) -> Result<TickArray> {
    if data.len() != TICK_ARRAY_LEN {
        return Err(ClmmError::InvalidAccountData("TickArrayState: length != 10240"));
    }
    if data[..8] != TICK_ARRAY_DISCRIMINATOR {
        return Err(ClmmError::InvalidAccountData("TickArrayState: bad discriminator"));
    }
    let mut ticks = Vec::with_capacity(TICK_ARRAY_SIZE as usize);
    for i in 0..TICK_ARRAY_SIZE as usize {
        let o = 44 + TICK_STATE_LEN * i;
        ticks.push(TickState {
            tick: rd_i32(data, o),
            liquidity_net: rd_u128(data, o + 4) as i128,
            liquidity_gross: rd_u128(data, o + 20),
            order_phase: rd_u64(data, o + 116),
            orders_amount: rd_u64(data, o + 124),
            part_filled_orders_remaining: rd_u64(data, o + 132),
            unfilled_ratio_x64: rd_u128(data, o + 140),
        });
    }
    Ok(TickArray {
        pool_id: rd_key(data, 8),
        start_tick_index: rd_i32(data, 40),
        ticks,
        initialized_tick_count: data[10124],
    })
}


pub fn parse_bitmap_extension(data: &[u8]) -> Result<BitmapExtension> {
    if data.len() != BITMAP_EXTENSION_LEN {
        return Err(ClmmError::InvalidAccountData("TickArrayBitmapExtension: length != 1832"));
    }
    if data[..8] != BITMAP_EXTENSION_DISCRIMINATOR {
        return Err(ClmmError::InvalidAccountData("TickArrayBitmapExtension: bad discriminator"));
    }
    let mut ext = BitmapExtension {
        pool_id: rd_key(data, 8),
        ..Default::default()
    };
    for i in 0..EXTENSION_TICKARRAY_BITMAP_SIZE {
        for j in 0..8 {
            ext.positive[i][j] = rd_u64(data, 40 + 64 * i + 8 * j);
            ext.negative[i][j] = rd_u64(data, 936 + 64 * i + 8 * j);
        }
    }
    Ok(ext)
}





#[inline]
fn tick_count(tick_spacing: u16) -> i32 {
    TICK_ARRAY_SIZE * tick_spacing as i32
}



pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    if tick_spacing == 0 {
        return 0;
    }
    let ticks_in_array = tick_count(tick_spacing);
    let mut start = tick / ticks_in_array;
    if tick < 0 && tick % ticks_in_array != 0 {
        start -= 1;
    }
    start * ticks_in_array
}


pub fn tick_spacing_index_from_tick(tick_index: i32, tick_spacing: u16) -> i32 {
    if tick_spacing == 0 {
        return 0;
    }
    let ts = tick_spacing as i32;
    if tick_index % ts == 0 || tick_index >= 0 {
        tick_index / ts
    } else {
        tick_index / ts - 1
    }
}

fn check_is_valid_start_index(tick_index: i32, tick_spacing: u16) -> bool {
    if !(MIN_TICK..=MAX_TICK).contains(&tick_index) {
        if tick_index > MAX_TICK {
            return false;
        }
        return tick_index == tick_array_start_index(MIN_TICK, tick_spacing);
    }
    tick_index % tick_count(tick_spacing) == 0
}

#[inline]
fn max_tick_in_tickarray_bitmap(tick_spacing: u16) -> i32 {
    tick_spacing as i32 * TICK_ARRAY_SIZE * TICK_ARRAY_BITMAP_SIZE
}

fn get_bitmap_tick_boundary(tick_array_start_index: i32, tick_spacing: u16) -> (i32, i32) {
    let ticks_in_one_bitmap = max_tick_in_tickarray_bitmap(tick_spacing);
    let mut m = tick_array_start_index.abs() / ticks_in_one_bitmap;
    if tick_array_start_index < 0 && tick_array_start_index.abs() % ticks_in_one_bitmap != 0 {
        m += 1;
    }
    let min_value = ticks_in_one_bitmap * m;
    if tick_array_start_index < 0 {
        (-min_value, -min_value + ticks_in_one_bitmap)
    } else {
        (min_value, min_value + ticks_in_one_bitmap)
    }
}


pub fn default_bitmap_start_index_range(tick_spacing: u16) -> (i32, i32) {
    let mut max_tick_boundary = max_tick_in_tickarray_bitmap(tick_spacing);
    let mut min_tick_boundary = -max_tick_boundary;
    if max_tick_boundary > MAX_TICK {
        max_tick_boundary = tick_array_start_index(MAX_TICK, tick_spacing) + tick_count(tick_spacing);
    }
    if min_tick_boundary < MIN_TICK {
        min_tick_boundary = tick_array_start_index(MIN_TICK, tick_spacing);
    }
    (min_tick_boundary, max_tick_boundary)
}


pub fn is_overflow_default_tickarray_bitmap(tick_index: i32, tick_spacing: u16) -> bool {
    let (min_b, max_b) = default_bitmap_start_index_range(tick_spacing);
    let start = tick_array_start_index(tick_index, tick_spacing);
    start >= max_b || start < min_b
}


#[inline]
fn highest_set_bit_at_or_below(words: &[u64], pos: usize) -> Option<usize> {
    let mut w = pos / 64;
    let b = pos % 64;
    let mut word = if b == 63 { words[w] } else { words[w] & ((1u64 << (b + 1)) - 1) };
    loop {
        if word != 0 {
            return Some(w * 64 + 63 - word.leading_zeros() as usize);
        }
        if w == 0 {
            return None;
        }
        w -= 1;
        word = words[w];
    }
}


#[inline]
fn lowest_set_bit_at_or_above(words: &[u64], pos: usize) -> Option<usize> {
    let mut w = pos / 64;
    if w >= words.len() {
        return None;
    }
    let b = pos % 64;
    let mut word = words[w] & (u64::MAX << b);
    loop {
        if word != 0 {
            return Some(w * 64 + word.trailing_zeros() as usize);
        }
        w += 1;
        if w >= words.len() {
            return None;
        }
        word = words[w];
    }
}


fn check_current_tick_array_is_initialized(bit_map: &[u64; 16], tick_current: i32, tick_spacing: u16) -> Result<(bool, i32)> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick_current) {
        return Err(ClmmError::InvalidTickIndex);
    }
    let multiplier = tick_count(tick_spacing);
    let mut compressed = tick_current / multiplier + 512;
    if tick_current < 0 && tick_current % multiplier != 0 {
        compressed -= 1;
    }
    let bit_pos = compressed.unsigned_abs() as usize;
    let initialized = bit_pos < 1024 && (bit_map[bit_pos / 64] >> (bit_pos % 64)) & 1 == 1;
    Ok((initialized, (compressed - 512) * multiplier))
}


fn next_initialized_tick_array_start_index_default(
    bit_map: &[u64; 16],
    last_tick_array_start_index: i32,
    tick_spacing: u16,
    zero_for_one: bool,
) -> Result<(bool, i32)> {
    if !check_is_valid_start_index(last_tick_array_start_index, tick_spacing) {
        return Err(ClmmError::InvalidTickArrayBoundary);
    }
    let tick_boundary = max_tick_in_tickarray_bitmap(tick_spacing);
    let next = if zero_for_one {
        last_tick_array_start_index - tick_count(tick_spacing)
    } else {
        last_tick_array_start_index + tick_count(tick_spacing)
    };
    if next < -tick_boundary || next >= tick_boundary {
        return Ok((false, last_tick_array_start_index));
    }
    let multiplier = tick_count(tick_spacing);
    let mut compressed = next / multiplier + 512;
    if next < 0 && next % multiplier != 0 {
        compressed -= 1;
    }
    let bit_pos = compressed.unsigned_abs() as usize;
    if zero_for_one {
        match highest_set_bit_at_or_below(bit_map, bit_pos) {
            Some(b) => Ok((true, (b as i32 - 512) * multiplier)),
            None => Ok((false, -tick_boundary)),
        }
    } else {
        match lowest_set_bit_at_or_above(bit_map, bit_pos) {
            Some(b) => Ok((true, (b as i32 - 512) * multiplier)),
            None => Ok((false, tick_boundary - tick_count(tick_spacing))),
        }
    }
}

impl BitmapExtension {
    fn check_extension_boundary(tick_index: i32, tick_spacing: u16) -> Result<()> {
        let positive_tick_boundary = max_tick_in_tickarray_bitmap(tick_spacing);
        let negative_tick_boundary = -positive_tick_boundary;
        if !(MAX_TICK > positive_tick_boundary) || !(negative_tick_boundary > MIN_TICK) {
            return Err(ClmmError::RequireFailed("extension boundary: tick_spacing too large"));
        }
        if tick_index >= negative_tick_boundary && tick_index < positive_tick_boundary {
            return Err(ClmmError::InvalidTickArrayBoundary);
        }
        Ok(())
    }

    fn get_bitmap_offset(tick_index: i32, tick_spacing: u16) -> Result<usize> {
        if !check_is_valid_start_index(tick_index, tick_spacing) {
            return Err(ClmmError::InvalidTickIndex);
        }
        Self::check_extension_boundary(tick_index, tick_spacing)?;
        let ticks_in_one_bitmap = max_tick_in_tickarray_bitmap(tick_spacing);
        let mut offset = tick_index.abs() / ticks_in_one_bitmap - 1;
        if tick_index < 0 && tick_index.abs() % ticks_in_one_bitmap == 0 {
            offset -= 1;
        }
        if offset < 0 || offset as usize >= EXTENSION_TICKARRAY_BITMAP_SIZE {
            return Err(ClmmError::InvalidTickArrayBoundary);
        }
        Ok(offset as usize)
    }

    fn get_bitmap(&self, tick_index: i32, tick_spacing: u16) -> Result<(usize, &[u64; 8])> {
        let offset = Self::get_bitmap_offset(tick_index, tick_spacing)?;
        if tick_index < 0 {
            Ok((offset, &self.negative[offset]))
        } else {
            Ok((offset, &self.positive[offset]))
        }
    }

    fn tick_array_offset_in_bitmap(tick_array_start_index: i32, tick_spacing: u16) -> i32 {
        let m = tick_array_start_index.abs() % max_tick_in_tickarray_bitmap(tick_spacing);
        let mut off = m / tick_count(tick_spacing);
        if tick_array_start_index < 0 && m != 0 {
            off = TICK_ARRAY_BITMAP_SIZE - off;
        }
        off
    }

    
    pub fn check_tick_array_is_initialized(&self, tick_array_start_index: i32, tick_spacing: u16) -> Result<(bool, i32)> {
        let (_, bitmap) = self.get_bitmap(tick_array_start_index, tick_spacing)?;
        let off = Self::tick_array_offset_in_bitmap(tick_array_start_index, tick_spacing) as usize;
        let set = off < 512 && (bitmap[off / 64] >> (off % 64)) & 1 == 1;
        Ok((set, tick_array_start_index))
    }

    
    pub fn flip_tick_array_bit(&mut self, tick_array_start_index: i32, tick_spacing: u16) -> Result<()> {
        let offset = Self::get_bitmap_offset(tick_array_start_index, tick_spacing)?;
        let off = Self::tick_array_offset_in_bitmap(tick_array_start_index, tick_spacing) as usize;
        if off >= 512 {
            return Err(ClmmError::InvalidTickArrayBoundary);
        }
        let words = if tick_array_start_index < 0 {
            &mut self.negative[offset]
        } else {
            &mut self.positive[offset]
        };
        words[off / 64] ^= 1u64 << (off % 64);
        Ok(())
    }

    
    pub fn next_initialized_tick_array_from_one_bitmap(
        &self,
        last_tick_array_start_index: i32,
        tick_spacing: u16,
        zero_for_one: bool,
    ) -> Result<(bool, i32)> {
        let multiplier = tick_count(tick_spacing);
        let next = if zero_for_one {
            last_tick_array_start_index - multiplier
        } else {
            last_tick_array_start_index + multiplier
        };
        let min_start = tick_array_start_index(MIN_TICK, tick_spacing);
        let max_start = tick_array_start_index(MAX_TICK, tick_spacing);
        if next < min_start || next > max_start {
            return Ok((false, next));
        }
        let (_, bitmap) = self.get_bitmap(next, tick_spacing)?;
        let (bmin, bmax) = get_bitmap_tick_boundary(next, tick_spacing);
        let off = Self::tick_array_offset_in_bitmap(next, tick_spacing);
        if !(0..TICK_ARRAY_BITMAP_SIZE).contains(&off) {
            return Err(ClmmError::InvalidTickArrayBoundary);
        }
        let off = off as usize;
        if zero_for_one {
            match highest_set_bit_at_or_below(bitmap, off) {
                Some(b) => Ok((true, next - (off - b) as i32 * multiplier)),
                None => Ok((false, bmin)),
            }
        } else {
            match lowest_set_bit_at_or_above(bitmap, off) {
                Some(b) => Ok((true, next + (b - off) as i32 * multiplier)),
                None => Ok((false, bmax - multiplier)),
            }
        }
    }

    
    
    
    pub fn from_tick_arrays(tick_spacing: u16, tick_arrays: &[TickArray]) -> BitmapExtension {
        let mut ext = BitmapExtension::default();
        if tick_spacing == 0 {
            return ext;
        }
        for ta in tick_arrays {
            let s = ta.start_tick_index;
            if ta.initialized_tick_count == 0 || !is_overflow_default_tickarray_bitmap(s, tick_spacing) {
                continue;
            }
            if let Ok((false, _)) = ext.check_tick_array_is_initialized(s, tick_spacing) {
                let _ = ext.flip_tick_array_bit(s, tick_spacing);
            }
        }
        ext
    }
}


fn next_tick_array_index(
    pool: &PoolSnapshot,
    ext: Option<&BitmapExtension>,
    last_tick_array_start_index: i32,
    zero_for_one: bool,
) -> Result<Option<i32>> {
    let ts = pool.tick_spacing;
    let mut last = tick_array_start_index(last_tick_array_start_index, ts);
    loop {
        let (found, start) =
            next_initialized_tick_array_start_index_default(&pool.tick_array_bitmap, last, ts, zero_for_one)?;
        if found {
            return Ok(Some(start));
        }
        if ts >= 15 {
            return Ok(None);
        }
        last = start;
        let e = ext.ok_or(ClmmError::MissingTickArrayBitmapExtensionAccount)?;
        let (found, start) = e.next_initialized_tick_array_from_one_bitmap(last, ts, zero_for_one)?;
        if found {
            return Ok(Some(start));
        }
        last = start;
        if !(MIN_TICK..=MAX_TICK).contains(&last) {
            return Ok(None);
        }
    }
}


fn first_tick_array_index(pool: &PoolSnapshot, ext: Option<&BitmapExtension>, zero_for_one: bool) -> Result<(bool, i32)> {
    let ts = pool.tick_spacing;
    let current_start = tick_array_start_index(pool.tick_current, ts);
    let (is_initialized, start) = if is_overflow_default_tickarray_bitmap(pool.tick_current, ts) {
        let e = ext.ok_or(ClmmError::MissingTickArrayBitmapExtensionAccount)?;
        e.check_tick_array_is_initialized(current_start, ts)?
    } else {
        check_current_tick_array_is_initialized(&pool.tick_array_bitmap, pool.tick_current, ts)?
    };
    if is_initialized {
        return Ok((true, start));
    }
    let next = next_tick_array_index(pool, ext, current_start, zero_for_one)?
        .ok_or(ClmmError::InsufficientLiquidityForDirection)?;
    Ok((false, next))
}






pub fn initialized_tick_array_starts(
    pool: &PoolSnapshot,
    ext: Option<&BitmapExtension>,
    zero_for_one: bool,
    max: usize,
) -> Vec<i32> {
    let mut out = Vec::new();
    if max == 0 || pool.tick_spacing == 0 {
        return out;
    }
    let mut last = match first_tick_array_index(pool, ext, zero_for_one) {
        Ok((_, s)) => s,
        Err(_) => return out,
    };
    out.push(last);
    while out.len() < max {
        match next_tick_array_index(pool, ext, last, zero_for_one) {
            Ok(Some(s)) => {
                out.push(s);
                last = s;
            }
            _ => break,
        }
    }
    out
}






#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickUpdate {
    pub array_start_index: i32,
    
    pub offset: usize,
    pub tick: TickState,
}


#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapQuote {
    
    
    pub amount_in: u64,
    
    
    pub amount_out: u64,
    
    pub fee_amount: u64,
    pub end_sqrt_price_x64: u128,
    pub end_tick: i32,
    
    
    pub tick_arrays_used: Vec<i32>,
    pub end_liquidity: u128,
    
    pub fee_on_token0: bool,
    pub protocol_fee: u64,
    pub fund_fee: u64,
    
    pub ticks_crossed: u32,
    
    pub max_total_fee_rate: u32,
    
    pub dynamic_fee_after: Option<DynamicFeeInfo>,
    
    pub updated_ticks: Vec<TickUpdate>,
    
    pub tick_array_counts: Vec<(i32, u8)>,
    
    pub emptied_tick_arrays: Vec<i32>,
}

struct SwapState {
    amount_specified_remaining: u64,
    amount_calculated: u64,
    sqrt_price_x64: u128,
    tick: i32,
    lp_fee: u64,
    protocol_fee: u64,
    fund_fee: u64,
    liquidity: u128,
    sqrt_price_next_x64: u128,
    tick_next: i32,
    tick_spacing: u16,
    base_fee_rate: u32,
    tick_spacing_index: i32,
    dynamic_fee_info: Option<DynamicFeeInfo>,
}

impl SwapState {
    fn apply_swap_amounts(
        &mut self,
        amount_in: u64,
        amount_out: u64,
        fee_amount: u64,
        is_base_input: bool,
        is_fee_on_input: bool,
        protocol_fee_rate: u32,
        fund_fee_rate: u32,
    ) -> Result<()> {
        let consumed = if is_fee_on_input {
            amount_in.checked_add(fee_amount).ok_or(ClmmError::CalculateOverflow)?
        } else {
            amount_in
        };
        if is_base_input {
            self.amount_specified_remaining = self
                .amount_specified_remaining
                .checked_sub(consumed)
                .ok_or(ClmmError::CalculateOverflow)?;
            self.amount_calculated = self
                .amount_calculated
                .checked_add(amount_out)
                .ok_or(ClmmError::CalculateOverflow)?;
        } else {
            self.amount_specified_remaining = self
                .amount_specified_remaining
                .checked_sub(amount_out)
                .ok_or(ClmmError::CalculateOverflow)?;
            self.amount_calculated = self
                .amount_calculated
                .checked_add(consumed)
                .ok_or(ClmmError::CalculateOverflow)?;
        }
        
        let denom = FEE_RATE_DENOMINATOR_VALUE as u128;
        let protocol_delta = (fee_amount as u128 * protocol_fee_rate as u128 / denom) as u64;
        let fund_delta = (fee_amount as u128 * fund_fee_rate as u128 / denom) as u64;
        self.protocol_fee = self
            .protocol_fee
            .checked_add(protocol_delta)
            .ok_or(ClmmError::CalculateOverflow)?;
        self.fund_fee = self.fund_fee.checked_add(fund_delta).ok_or(ClmmError::CalculateOverflow)?;
        let remaining_fee = fee_amount
            .checked_sub(protocol_delta)
            .and_then(|v| v.checked_sub(fund_delta))
            .ok_or(ClmmError::CalculateOverflow)?;
        if self.liquidity > 0 {
            self.lp_fee = self.lp_fee.checked_add(remaining_fee).ok_or(ClmmError::CalculateOverflow)?;
        } else {
            self.protocol_fee = self
                .protocol_fee
                .checked_add(remaining_fee)
                .ok_or(ClmmError::CalculateOverflow)?;
        }
        Ok(())
    }

    fn get_target_price_based_on_next_tick(&mut self, tick_next: i32, zero_for_one: bool, limit: u128) -> Result<u128> {
        self.tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
        self.sqrt_price_next_x64 = get_sqrt_price_at_tick(self.tick_next)?;
        let target = if (zero_for_one && self.sqrt_price_next_x64 < limit)
            || (!zero_for_one && self.sqrt_price_next_x64 > limit)
        {
            limit
        } else {
            self.sqrt_price_next_x64
        };
        if zero_for_one {
            if !(self.tick >= self.tick_next && self.sqrt_price_x64 >= self.sqrt_price_next_x64 && self.sqrt_price_x64 >= target) {
                return Err(ClmmError::RequireFailed("zero_for_one target direction"));
            }
        } else if !(self.tick_next > self.tick && self.sqrt_price_next_x64 >= self.sqrt_price_x64 && target >= self.sqrt_price_x64) {
            return Err(ClmmError::RequireFailed("one_for_zero target direction"));
        }
        Ok(target)
    }

    fn update_volatility_accumulator(&mut self) -> Result<()> {
        let tsi = self.tick_spacing_index;
        if let Some(info) = &mut self.dynamic_fee_info {
            info.update_volatility_accumulator(tsi)?;
        }
        Ok(())
    }

    fn update_dynamic_fee_index(&mut self, zero_for_one: bool, is_skipped_tick_spacing: bool) -> Result<()> {
        if let Some(info) = self.dynamic_fee_info {
            if is_skipped_tick_spacing {
                let tick_index = if self.sqrt_price_x64 == self.sqrt_price_next_x64 {
                    self.tick_next
                } else {
                    self.tick
                };
                let mut tsi = tick_spacing_index_from_tick(tick_index, self.tick_spacing);
                if !zero_for_one && tick_index % (self.tick_spacing as i32) == 0 {
                    tsi -= 1;
                }
                self.tick_spacing_index = tsi;
                if info.volatility_accumulator != info.max_volatility_accumulator {
                    self.update_volatility_accumulator()?;
                }
            }
            self.tick_spacing_index += if zero_for_one { -1 } else { 1 };
        }
        Ok(())
    }

    
    fn get_spacing_bounded_price(&self, target_price: u128, zero_for_one: bool) -> Result<(bool, u128, Option<i32>)> {
        let info = match &self.dynamic_fee_info {
            Some(i) => i,
            None => return Ok((true, target_price, None)),
        };
        if self.liquidity == 0 || info.volatility_accumulator == info.max_volatility_accumulator {
            return Ok((true, target_price, None));
        }
        let ts = self.tick_spacing as i32;
        let bounded_tick = if zero_for_one {
            self.tick_spacing_index.saturating_mul(ts)
        } else {
            self.tick_spacing_index.saturating_add(1).saturating_mul(ts)
        };
        let clamped = bounded_tick.clamp(MIN_TICK, MAX_TICK);
        let bounded_sqrt = get_sqrt_price_at_tick(clamped)?;
        if zero_for_one {
            if target_price > bounded_sqrt {
                Ok((false, target_price, None))
            } else {
                Ok((false, bounded_sqrt, Some(clamped)))
            }
        } else if target_price < bounded_sqrt {
            Ok((false, target_price, None))
        } else {
            Ok((false, bounded_sqrt, Some(clamped)))
        }
    }

    fn get_total_fee_rate(&self) -> Result<u32> {
        if let Some(info) = &self.dynamic_fee_info {
            let dynamic = info.dynamic_fee_rate(self.tick_spacing)?;
            let total = self
                .base_fee_rate
                .checked_add(dynamic)
                .ok_or(ClmmError::CalculateOverflow)?;
            return Ok(total.min(MAX_FEE_RATE_NUMERATOR));
        }
        Ok(self.base_fee_rate)
    }
}


struct ArrayView<'a> {
    arrays: &'a [TickArray],
    tick_overlay: Vec<(usize, usize, TickState)>,
    count_overlay: Vec<(usize, u8)>,
}

impl<'a> ArrayView<'a> {
    #[inline]
    fn tick(&self, ai: usize, off: usize) -> TickState {
        if !self.tick_overlay.is_empty() {
            if let Some((_, _, t)) = self.tick_overlay.iter().rev().find(|(a, o, _)| *a == ai && *o == off) {
                return *t;
            }
        }
        self.arrays[ai].ticks[off]
    }

    #[inline]
    fn is_initialized(&self, ai: usize, off: usize) -> bool {
        self.tick(ai, off).is_initialized()
    }

    fn count(&self, ai: usize) -> u8 {
        if let Some((_, c)) = self.count_overlay.iter().rev().find(|(a, _)| *a == ai) {
            return *c;
        }
        self.arrays[ai].initialized_tick_count
    }

    
    fn next_initialized_tick(&self, ai: usize, current_tick_index: i32, tick_spacing: u16, zero_for_one: bool) -> Option<usize> {
        let start = self.arrays[ai].start_tick_index;
        if tick_array_start_index(current_tick_index, tick_spacing) != start {
            return None;
        }
        let offset = ((current_tick_index - start) / tick_spacing as i32) as usize;
        if zero_for_one {
            (0..=offset).rev().find(|&i| self.is_initialized(ai, i))
        } else {
            ((offset + 1)..TICK_ARRAY_SIZE as usize).find(|&i| self.is_initialized(ai, i))
        }
    }

    
    fn first_initialized_tick(&self, ai: usize, zero_for_one: bool) -> Result<usize> {
        let n = TICK_ARRAY_SIZE as usize;
        let found = if zero_for_one {
            (0..n).rev().find(|&i| self.is_initialized(ai, i))
        } else {
            (0..n).find(|&i| self.is_initialized(ai, i))
        };
        found.ok_or(ClmmError::InvalidTickArray)
    }
}
















pub fn quote_exact_input(
    pool: &PoolSnapshot,
    config: &AmmConfigState,
    tick_arrays: &[TickArray],
    zero_for_one: bool,
    amount_in: u64,
    now_ts: u64,
) -> Result<SwapQuote> {
    simulate_swap(pool, config, None, tick_arrays, zero_for_one, amount_in, 0, true, now_ts)
}


pub fn quote_exact_input_with_ext(
    pool: &PoolSnapshot,
    config: &AmmConfigState,
    ext: Option<&BitmapExtension>,
    tick_arrays: &[TickArray],
    zero_for_one: bool,
    amount_in: u64,
    now_ts: u64,
) -> Result<SwapQuote> {
    simulate_swap(pool, config, ext, tick_arrays, zero_for_one, amount_in, 0, true, now_ts)
}









pub fn simulate_swap(
    pool: &PoolSnapshot,
    config: &AmmConfigState,
    ext: Option<&BitmapExtension>,
    tick_arrays: &[TickArray],
    zero_for_one: bool,
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
    now_ts: u64,
) -> Result<SwapQuote> {
    let ts = pool.tick_spacing;
    if ts == 0 {
        return Err(ClmmError::InvalidAccountData("PoolState: tick_spacing == 0"));
    }
    if tick_arrays.iter().any(|a| a.ticks.len() != TICK_ARRAY_SIZE as usize) {
        return Err(ClmmError::InvalidAccountData("TickArray: ticks.len() != 60"));
    }
    
    if now_ts <= pool.open_time {
        return Err(ClmmError::PoolNotOpen);
    }
    let limit = if sqrt_price_limit_x64 == 0 {
        if zero_for_one {
            MIN_SQRT_PRICE_X64 + 1
        } else {
            MAX_SQRT_PRICE_X64 - 1
        }
    } else {
        sqrt_price_limit_x64
    };
    
    if amount_specified == 0 {
        return Err(ClmmError::ZeroAmountSpecified);
    }
    if !pool.swap_enabled() {
        return Err(ClmmError::NotApproved);
    }
    let limit_ok = if zero_for_one {
        limit < pool.sqrt_price_x64 && limit > MIN_SQRT_PRICE_X64
    } else {
        limit > pool.sqrt_price_x64 && limit < MAX_SQRT_PRICE_X64
    };
    if !limit_ok {
        return Err(ClmmError::SqrtPriceLimitOverflow);
    }

    let virtual_ext;
    let ext: Option<&BitmapExtension> = match ext {
        Some(e) => Some(e),
        None if ts < 15 => {
            virtual_ext = BitmapExtension::from_tick_arrays(ts, tick_arrays);
            Some(&virtual_ext)
        }
        None => None,
    };

    let (mut first_contains_pool_tick, first_valid_start) = first_tick_array_index(pool, ext, zero_for_one)?;
    let mut current_valid_start = first_valid_start;

    
    let mut next_pop = 0usize;
    let pop = |np: &mut usize| -> Result<usize> {
        if *np < tick_arrays.len() {
            *np += 1;
            Ok(*np - 1)
        } else {
            Err(ClmmError::NotEnoughTickArrayAccount)
        }
    };
    let mut cur = pop(&mut next_pop)?;
    let remaining_len = tick_arrays.len() - next_pop;
    for _ in 0..remaining_len {
        if tick_arrays[cur].start_tick_index == current_valid_start {
            break;
        }
        cur = pop(&mut next_pop)?;
    }
    if tick_arrays[cur].start_tick_index != current_valid_start {
        return Err(ClmmError::InvalidFirstTickArrayAccount);
    }
    let mut used = vec![current_valid_start];

    let is_fee_on_input = pool.is_fee_on_input(zero_for_one);
    let mut state = SwapState {
        amount_specified_remaining: amount_specified,
        amount_calculated: 0,
        sqrt_price_x64: pool.sqrt_price_x64,
        tick: pool.tick_current,
        lp_fee: 0,
        protocol_fee: 0,
        fund_fee: 0,
        liquidity: pool.liquidity,
        sqrt_price_next_x64: 0,
        tick_next: 0,
        tick_spacing: ts,
        base_fee_rate: config.trade_fee_rate,
        tick_spacing_index: 0,
        dynamic_fee_info: pool.dynamic_fee,
    };
    if let Some(info) = &mut state.dynamic_fee_info {
        state.tick_spacing_index = tick_spacing_index_from_tick(state.tick, ts);
        
        info.update_reference(state.tick_spacing_index, now_ts & 0xffff_ffff);
    }

    let mut view = ArrayView {
        arrays: tick_arrays,
        tick_overlay: Vec::new(),
        count_overlay: Vec::new(),
    };
    let mut emptied: Vec<i32> = Vec::new();
    let mut ticks_crossed: u32 = 0;
    let mut max_total_fee_rate: u32 = 0;

    while state.amount_specified_remaining != 0 && state.sqrt_price_x64 != limit {
        let nt_off = if let Some(off) = view.next_initialized_tick(cur, state.tick, ts, zero_for_one) {
            off
        } else if !first_contains_pool_tick {
            first_contains_pool_tick = true;
            view.first_initialized_tick(cur, zero_for_one)?
        } else {
            let next_start = next_tick_array_index(pool, ext, current_valid_start, zero_for_one)?
                .ok_or(ClmmError::LiquidityInsufficient)?;
            while tick_arrays[cur].start_tick_index != next_start {
                cur = pop(&mut next_pop)?;
            }
            current_valid_start = next_start;
            used.push(next_start);
            view.first_initialized_tick(cur, zero_for_one)?
        };
        let nt_ai = cur;
        let mut next_tick = view.tick(nt_ai, nt_off);
        if !next_tick.is_initialized() {
            return Err(ClmmError::RequireFailed("next_initialized_tick.is_initialized()"));
        }

        let target_price = state.get_target_price_based_on_next_tick(next_tick.tick, zero_for_one, limit)?;
        let mut liquidity_next = state.liquidity;
        loop {
            state.update_volatility_accumulator()?;
            let total_fee_rate = state.get_total_fee_rate()?;
            max_total_fee_rate = max_total_fee_rate.max(total_fee_rate);
            let (is_skipped, bounded_price, bounded_tick) = state.get_spacing_bounded_price(target_price, zero_for_one)?;

            let step = if state.sqrt_price_x64 != bounded_price {
                let r = compute_swap(
                    state.sqrt_price_x64,
                    bounded_price,
                    state.liquidity,
                    state.amount_specified_remaining,
                    total_fee_rate,
                    is_base_input,
                    zero_for_one,
                    is_fee_on_input,
                )?;
                state.apply_swap_amounts(
                    r.amount_in,
                    r.amount_out,
                    r.fee_amount,
                    is_base_input,
                    is_fee_on_input,
                    config.protocol_fee_rate,
                    config.fund_fee_rate,
                )?;
                r
            } else {
                SwapStep {
                    sqrt_price_next_x64: bounded_price,
                    ..Default::default()
                }
            };

            let unfilled_before = next_tick.limit_order_unfilled_amount()?;
            if state.sqrt_price_next_x64 == step.sqrt_price_next_x64 {
                let lo = next_tick.match_limit_order_with_sqrt_price(
                    state.amount_specified_remaining,
                    zero_for_one,
                    is_base_input,
                    total_fee_rate,
                    is_fee_on_input,
                    state.sqrt_price_next_x64,
                )?;
                if lo.amount_in != 0 || lo.amount_out != 0 || lo.amm_fee_amount != 0 {
                    state.apply_swap_amounts(
                        lo.amount_in,
                        lo.amount_out,
                        lo.amm_fee_amount,
                        is_base_input,
                        is_fee_on_input,
                        config.protocol_fee_rate,
                        config.fund_fee_rate,
                    )?;
                }
                if !next_tick.is_initialized() {
                    let c = view.count(nt_ai).checked_sub(1).ok_or(ClmmError::CalculateOverflow)?;
                    view.count_overlay.push((nt_ai, c));
                    if c == 0 {
                        emptied.push(tick_arrays[nt_ai].start_tick_index);
                    }
                }
                if next_tick.has_liquidity() && !next_tick.has_limit_orders() {
                    let mut net = next_tick.liquidity_net;
                    if zero_for_one {
                        net = net.checked_neg().ok_or(ClmmError::CalculateOverflow)?;
                    }
                    liquidity_next = add_delta(state.liquidity, net)?;
                    ticks_crossed += 1;
                }
                
                if tick_array_start_index(next_tick.tick, ts) != tick_arrays[nt_ai].start_tick_index {
                    return Err(ClmmError::InvalidTickArray);
                }
                if next_tick != view.tick(nt_ai, nt_off) {
                    view.tick_overlay.push((nt_ai, nt_off, next_tick));
                }
                state.tick = if (zero_for_one && !next_tick.has_limit_orders())
                    || (!zero_for_one && next_tick.has_limit_orders())
                {
                    state.tick_next - 1
                } else {
                    state.tick_next
                };
            } else if state.sqrt_price_x64 != step.sqrt_price_next_x64 {
                state.tick = match bounded_tick {
                    Some(t) if step.sqrt_price_next_x64 == bounded_price => t,
                    _ => get_tick_at_sqrt_price(step.sqrt_price_next_x64)?,
                };
            }
            state.sqrt_price_x64 = step.sqrt_price_next_x64;
            state.update_dynamic_fee_index(zero_for_one, is_skipped)?;
            if state.amount_specified_remaining == 0 || state.sqrt_price_x64 == target_price {
                let unfilled_after = next_tick.limit_order_unfilled_amount()?;
                if state.amount_specified_remaining != 0 && unfilled_after != unfilled_before && unfilled_after != 0 {
                    return Err(ClmmError::RequireFailed("limit order must be fully consumed"));
                }
                break;
            }
        }
        state.liquidity = liquidity_next;
    }

    
    let consumed = amount_specified
        .checked_sub(state.amount_specified_remaining)
        .ok_or(ClmmError::CalculateOverflow)?;
    let (amount_0, amount_1) = if zero_for_one == is_base_input {
        (consumed, state.amount_calculated)
    } else {
        (state.amount_calculated, consumed)
    };
    let total_fee = state
        .lp_fee
        .checked_add(state.protocol_fee)
        .and_then(|v| v.checked_add(state.fund_fee))
        .ok_or(ClmmError::CalculateOverflow)?;

    
    if amount_0 == 0 || amount_1 == 0 {
        return Err(ClmmError::TooSmallInputOrOutputAmount);
    }
    if sqrt_price_limit_x64 == 0 && consumed != amount_specified {
        return Err(ClmmError::PartialFill {
            specified: amount_specified,
            filled: consumed,
        });
    }

    let (amount_in, amount_out) = if zero_for_one { (amount_0, amount_1) } else { (amount_1, amount_0) };
    let mut updated_ticks: Vec<TickUpdate> = Vec::new();
    for (ai, off, t) in view.tick_overlay.iter() {
        let u = TickUpdate {
            array_start_index: tick_arrays[*ai].start_tick_index,
            offset: *off,
            tick: *t,
        };
        match updated_ticks
            .iter_mut()
            .find(|x| x.array_start_index == u.array_start_index && x.offset == u.offset)
        {
            Some(x) => *x = u,
            None => updated_ticks.push(u),
        }
    }
    let mut tick_array_counts: Vec<(i32, u8)> = Vec::new();
    for (ai, c) in view.count_overlay.iter() {
        let s = tick_arrays[*ai].start_tick_index;
        match tick_array_counts.iter_mut().find(|x| x.0 == s) {
            Some(x) => x.1 = *c,
            None => tick_array_counts.push((s, *c)),
        }
    }
    let dynamic_fee_after = match (pool.dynamic_fee, state.dynamic_fee_info) {
        (Some(mut p), Some(s)) => {
            p.last_update_timestamp = s.last_update_timestamp;
            p.volatility_reference = s.volatility_reference;
            p.tick_spacing_index_reference = s.tick_spacing_index_reference;
            p.volatility_accumulator = s.volatility_accumulator;
            Some(p)
        }
        _ => None,
    };
    Ok(SwapQuote {
        amount_in,
        amount_out,
        fee_amount: total_fee,
        end_sqrt_price_x64: state.sqrt_price_x64,
        end_tick: state.tick,
        tick_arrays_used: used,
        end_liquidity: state.liquidity,
        fee_on_token0: pool.is_fee_on_token0(zero_for_one),
        protocol_fee: state.protocol_fee,
        fund_fee: state.fund_fee,
        ticks_crossed,
        max_total_fee_rate,
        dynamic_fee_after,
        updated_ticks,
        tick_array_counts,
        emptied_tick_arrays: emptied,
    })
}




pub fn apply_swap_to_state(
    pool: &mut PoolSnapshot,
    ext: Option<&mut BitmapExtension>,
    tick_arrays: &mut [TickArray],
    quote: &SwapQuote,
) -> Result<()> {
    pool.sqrt_price_x64 = quote.end_sqrt_price_x64;
    pool.tick_current = quote.end_tick;
    pool.liquidity = quote.end_liquidity;
    if pool.dynamic_fee.is_some() {
        if let Some(d) = quote.dynamic_fee_after {
            pool.dynamic_fee = Some(d);
        }
    }
    for u in &quote.updated_ticks {
        if let Some(a) = tick_arrays.iter_mut().find(|a| a.start_tick_index == u.array_start_index) {
            if u.offset < a.ticks.len() {
                a.ticks[u.offset] = u.tick;
            }
        }
    }
    let mut ext = ext;
    for &(s, c) in &quote.tick_array_counts {
        if let Some(a) = tick_arrays.iter_mut().find(|a| a.start_tick_index == s) {
            a.initialized_tick_count = c;
        }
    }
    for &s in &quote.emptied_tick_arrays {
        if is_overflow_default_tickarray_bitmap(s, pool.tick_spacing) {
            let e = ext.as_deref_mut().ok_or(ClmmError::MissingTickArrayBitmapExtensionAccount)?;
            e.flip_tick_array_bit(s, pool.tick_spacing)?;
        } else {
            if !check_is_valid_start_index(s, pool.tick_spacing) {
                return Err(ClmmError::InvalidTickIndex);
            }
            let bit = (s / tick_count(pool.tick_spacing) + TICK_ARRAY_BITMAP_SIZE) as usize;
            pool.tick_array_bitmap[bit / 64] ^= 1u64 << (bit % 64);
        }
    }
    Ok(())
}






#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapEvent {
    pub pool_state: [u8; 32],
    pub sender: [u8; 32],
    pub token_account_0: [u8; 32],
    pub token_account_1: [u8; 32],
    pub amount_0: u64,
    pub transfer_fee_0: u64,
    pub amount_1: u64,
    pub transfer_fee_1: u64,
    pub zero_for_one: bool,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick: i32,
    pub trade_fee_0: u64,
    pub trade_fee_1: u64,
}


pub fn parse_swap_event(data: &[u8]) -> Result<SwapEvent> {
    const BASE: usize = 8 + 32 * 4 + 8 * 4 + 1 + 16 + 16 + 4;
    if data.len() < BASE || data[..8] != SWAP_EVENT_DISCRIMINATOR {
        return Err(ClmmError::InvalidAccountData("SwapEvent: bad length/discriminator"));
    }
    let mut e = SwapEvent {
        pool_state: rd_key(data, 8),
        sender: rd_key(data, 40),
        token_account_0: rd_key(data, 72),
        token_account_1: rd_key(data, 104),
        amount_0: rd_u64(data, 136),
        transfer_fee_0: rd_u64(data, 144),
        amount_1: rd_u64(data, 152),
        transfer_fee_1: rd_u64(data, 160),
        zero_for_one: data[168] != 0,
        sqrt_price_x64: rd_u128(data, 169),
        liquidity: rd_u128(data, 185),
        tick: rd_i32(data, 201),
        ..Default::default()
    };
    if data.len() >= BASE + 16 {
        e.trade_fee_0 = rd_u64(data, 205);
        e.trade_fee_1 = rd_u64(data, 213);
    }
    Ok(e)
}





#[cfg(test)]
mod tests {
    use super::*;

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        
        fn limb(&mut self) -> u64 {
            match self.next() % 8 {
                0 => 0,
                1 => u64::MAX,
                2 => 1u64 << 63,
                3 => (1u64 << 63) - 1,
                4 => u64::MAX - 1,
                5 => 1,
                _ => self.next(),
            }
        }
    }

    
    fn ref_divrem(num: &[u64; 8], den: &[u64; 4]) -> ([u64; 8], [u64; 4]) {
        let mut q = [0u64; 8];
        let mut r = [0u64; 5]; 
        for bit in (0..512).rev() {
            
            let mut carry = (num[bit / 64] >> (bit % 64)) & 1;
            for limb in r.iter_mut() {
                let nc = *limb >> 63;
                *limb = (*limb << 1) | carry;
                carry = nc;
            }
            
            let d5 = [den[0], den[1], den[2], den[3], 0];
            if cmp_limbs(&r, &d5) != Ordering::Less {
                let mut borrow = 0u64;
                for i in 0..5 {
                    let (s1, b1) = r[i].overflowing_sub(d5[i]);
                    let (s2, b2) = s1.overflowing_sub(borrow);
                    r[i] = s2;
                    borrow = (b1 as u64) + (b2 as u64);
                }
                q[bit / 64] |= 1 << (bit % 64);
            }
        }
        (q, [r[0], r[1], r[2], r[3]])
    }

    #[test]
    fn divrem_matches_reference_random() {
        let mut rng = XorShift(0x9e3779b97f4a7c15);
        let mut n_cases = 0;
        for _ in 0..200_000 {
            let nl = (rng.next() % 9) as usize;
            let dl = 1 + (rng.next() % 4) as usize;
            let mut num = [0u64; 8];
            let mut den = [0u64; 4];
            for x in num.iter_mut().take(nl) {
                *x = rng.limb();
            }
            for x in den.iter_mut().take(dl) {
                *x = rng.limb();
            }
            if den == [0; 4] {
                assert!(divrem_512_by_256(&num, &den).is_none());
                continue;
            }
            let got = divrem_512_by_256(&num, &den).unwrap();
            let want = ref_divrem(&num, &den);
            assert_eq!(got, want, "num={:x?} den={:x?}", num, den);
            n_cases += 1;
        }
        assert!(n_cases > 100_000);
    }

    #[test]
    fn divrem_add_back_known_case() {
        
        
        let num = [0, 1u64 << 63, (1u64 << 63) - 1, 0, 0, 0, 0, 0];
        let den = [1, 0, 1u64 << 63, 0];
        assert_eq!(divrem_512_by_256(&num, &den).unwrap(), ref_divrem(&num, &den));
        let num = [3, 0, 1u64 << 63, 0, 0, 0, 0, 0];
        let den = [0, u64::MAX, 1u64 << 63, 0];
        assert_eq!(divrem_512_by_256(&num, &den).unwrap(), ref_divrem(&num, &den));
        
        let before = test_hooks::ADD_BACKS.load(std::sync::atomic::Ordering::Relaxed);
        let num = [3, 0, 1u64 << 63, 0, 0, 0, 0, 0];
        let den = [1, 0, 1u64 << 61, 0];
        let (q, r) = divrem_512_by_256(&num, &den).unwrap();
        assert_eq!(q, [3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r, [0, 0, 1u64 << 61, 0]);
        assert!(test_hooks::ADD_BACKS.load(std::sync::atomic::Ordering::Relaxed) > before);
    }

    #[test]
    fn mul_div_wide() {
        
        let a = U256::from_u128(u128::MAX);
        let b = U256::from_u128(u128::MAX);
        let d = U256::from_u128(u128::MAX);
        assert_eq!(a.mul_div_floor(b, d), Some(U256::from_u128(u128::MAX)));
        assert_eq!(a.mul_div_ceil(b, d), Some(U256::from_u128(u128::MAX)));
        assert_eq!(U256::MAX.mul_div_floor(U256::MAX, U256::MAX), Some(U256::MAX));
        assert_eq!(U256::MAX.mul_div_floor(U256::from_u64(2), U256::from_u64(1)), None);
        assert_eq!(U256::MAX.mul_div_floor(U256::from_u64(2), U256::from_u64(2)), Some(U256::MAX));
        assert_eq!(U256::from_u64(7).mul_div_ceil(U256::from_u64(3), U256::from_u64(2)), Some(U256::from_u64(11)));
        assert_eq!(U256::from_u64(7).mul_div_floor(U256::from_u64(3), U256::ZERO), None);
        
        let mut rng = XorShift(12345);
        for _ in 0..100_000 {
            let a = rng.next() as u128;
            let b = rng.next() as u128;
            let d = (rng.next() as u128).max(1);
            assert_eq!(mul_div_floor_u128(a, b, d), Some(a * b / d));
            assert_eq!(mul_div_ceil_u128(a, b, d), Some((a * b + d - 1) / d));
            let (x, y, z) = (rng.next(), rng.next(), rng.next().max(1));
            let f = x as u128 * y as u128 / z as u128;
            assert_eq!(mul_div_floor_u64(x, y, z), if f > u64::MAX as u128 { None } else { Some(f as u64) });
            
            let big = (a << 64) | b;
            assert_eq!(mul_div_floor_u128(big, Q64, d << 64), Some(big / d));
            let c = mul_div_ceil_u128(big, Q64, d << 64).unwrap();
            assert_eq!(c, big / d + ((big % d != 0) as u128));
        }
        assert_eq!(mul_div_floor_u128(u128::MAX, u128::MAX, 1), None);
        assert_eq!(mul_div_ceil_u128(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        
        let p = U256::mul_u128(u128::MAX, u128::MAX);
        assert_eq!(p.0, [1, 0, u64::MAX - 1, u64::MAX]);
        
        assert_eq!(p.shr_bits(128).to_u128(), Some(u128::MAX - 1));
        assert_eq!(U256::from_u128(1u128 << 100).shr_bits(36).to_u128(), Some(1u128 << 64));
        
        let x = U256::from_u128(12345678901234567890123456789);
        let q = mul_pow2_div(x, 64, U256::from_u128(987654321), false).unwrap();
        let expect = U256::mul_u128(12345678901234567890123456789, Q64 >> 0)
            .mul_div_floor(U256::from_u64(1), U256::from_u128(987654321))
            .unwrap();
        assert_eq!(q, expect);
    }

    #[test]
    fn tick_math_known_values() {
        assert_eq!(get_sqrt_price_at_tick(MIN_TICK).unwrap(), MIN_SQRT_PRICE_X64);
        assert_eq!(get_sqrt_price_at_tick(MAX_TICK).unwrap(), MAX_SQRT_PRICE_X64);
        assert_eq!(get_sqrt_price_at_tick(0).unwrap(), 1u128 << 64);
        assert!(get_sqrt_price_at_tick(MIN_TICK - 1).is_err());
        assert!(get_sqrt_price_at_tick(MAX_TICK + 1).is_err());
        assert_eq!(get_tick_at_sqrt_price(MIN_SQRT_PRICE_X64).unwrap(), MIN_TICK);
        assert_eq!(get_tick_at_sqrt_price(MAX_SQRT_PRICE_X64 - 1).unwrap(), MAX_TICK - 1);
        assert!(get_tick_at_sqrt_price(MAX_SQRT_PRICE_X64).is_err());
        assert!(get_tick_at_sqrt_price(MIN_SQRT_PRICE_X64 - 1).is_err());
        assert_eq!(get_tick_at_sqrt_price(1u128 << 64).unwrap(), 0);
        assert_eq!(get_tick_at_sqrt_price((1u128 << 64) - 1).unwrap(), -1);
        
        let s = get_sqrt_price_at_tick(-28861).unwrap();
        assert_eq!(get_tick_at_sqrt_price(s).unwrap(), -28861);
        assert_eq!(get_tick_at_sqrt_price(s + 1).unwrap(), -28861);
        assert_eq!(get_tick_at_sqrt_price(get_sqrt_price_at_tick(-28860).unwrap() - 1).unwrap(), -28861);
        assert_eq!(get_tick_at_sqrt_price(s - 1).unwrap(), -28862);
        let s = get_sqrt_price_at_tick(28861).unwrap();
        assert_eq!(get_tick_at_sqrt_price(s).unwrap(), 28861);
        assert_eq!(get_tick_at_sqrt_price(s + 1).unwrap(), 28861);
        assert_eq!(get_tick_at_sqrt_price(get_sqrt_price_at_tick(28862).unwrap() - 1).unwrap(), 28861);
        assert_eq!(get_tick_at_sqrt_price(s - 1).unwrap(), 28860);
        
        
        for &t in &[-443636, -300000, -100000, -1, 1, 100000, 300000, 443636] {
            let exact = (1.0001f64).powf(t as f64 / 2.0) * 18446744073709551616.0;
            let got = get_sqrt_price_at_tick(t).unwrap() as f64;
            assert!(((got - exact) / exact).abs() < 1e-9, "tick {} got {} want {}", t, got, exact);
        }
    }

    #[test]
    fn tick_math_roundtrip_exhaustive() {
        let mut prev = 0u128;
        for tick in MIN_TICK..MAX_TICK {
            let p = get_sqrt_price_at_tick(tick).unwrap();
            assert!(p > prev, "not monotonic at {}", tick);
            prev = p;
            assert_eq!(get_tick_at_sqrt_price(p).unwrap(), tick, "round trip at {}", tick);
            if tick % 97 == 0 {
                let next = get_sqrt_price_at_tick(tick + 1).unwrap();
                assert_eq!(get_tick_at_sqrt_price(next - 1).unwrap(), tick);
                if p > MIN_SQRT_PRICE_X64 {
                    assert_eq!(get_tick_at_sqrt_price(p - 1).unwrap(), tick - 1);
                }
            }
        }
    }

    #[test]
    fn start_index_and_default_bitmap() {
        assert_eq!(tick_array_start_index(0, 1), 0);
        assert_eq!(tick_array_start_index(59, 1), 0);
        assert_eq!(tick_array_start_index(60, 1), 60);
        assert_eq!(tick_array_start_index(-1, 1), -60);
        assert_eq!(tick_array_start_index(-60, 1), -60);
        assert_eq!(tick_array_start_index(-61, 1), -120);
        assert_eq!(tick_array_start_index(82981, 120), 79200);
        assert_eq!(tick_array_start_index(MIN_TICK, 1), -443640);
        assert_eq!(default_bitmap_start_index_range(1), (-30720, 30720));
        assert_eq!(get_bitmap_tick_boundary(-430080, 1), (-430080, -399360));
        assert_eq!(get_bitmap_tick_boundary(-430140, 1), (-460800, -430080));
        assert_eq!(get_bitmap_tick_boundary(430080, 1), (430080, 460800));
        assert_eq!(get_bitmap_tick_boundary(430020, 1), (399360, 430080));

        
        let bm: [u64; 16] = [
            1, 0, 0, 0, 0, 0, 9223372036854775808, 16140901064495857665, 7, 1, 0, 0, 0, 0, 0,
            9223372036854775808,
        ];
        let n = |last, z| next_initialized_tick_array_start_index_default(&bm, last, 10, z).unwrap().1;
        assert_eq!(n(0, true), -600);
        assert_eq!(n(-600, true), -1200);
        assert_eq!(n(-1200, true), -1800);
        assert_eq!(n(-1800, true), -38400);
        assert_eq!(n(-38400, true), -39000);
        assert_eq!(n(-39000, true), -307200);
        assert_eq!(n(0, false), 600);
        assert_eq!(n(600, false), 1200);
        assert_eq!(n(1200, false), 38400);
        assert_eq!(n(38400, false), 306600);

        
        let all = [u64::MAX; 16];
        let s = (MIN_TICK / TICK_ARRAY_SIZE - 1) * TICK_ARRAY_SIZE;
        assert_eq!(next_initialized_tick_array_start_index_default(&all, s, 1, false).unwrap(), (false, s));
        let s = (MAX_TICK / TICK_ARRAY_SIZE) * TICK_ARRAY_SIZE;
        assert_eq!(next_initialized_tick_array_start_index_default(&all, s, 1, true).unwrap(), (false, s));
    }

    #[test]
    fn extension_bitmap_program_tests() {
        let tc = TICK_ARRAY_SIZE;
        assert_eq!(BitmapExtension::get_bitmap_offset(tc * 512, 1).unwrap(), 0);
        assert_eq!(BitmapExtension::get_bitmap_offset(tc * 513, 1).unwrap(), 0);
        assert_eq!(BitmapExtension::get_bitmap_offset(tc * 1024, 1).unwrap(), 1);
        assert_eq!(BitmapExtension::get_bitmap_offset(tc * 7393, 1).unwrap(), 13);
        assert!(BitmapExtension::get_bitmap_offset(tc * 7394, 1).is_err());
        assert!(BitmapExtension::get_bitmap_offset(-tc * 512, 1).is_err());
        assert_eq!(BitmapExtension::get_bitmap_offset(-tc * 513, 1).unwrap(), 0);
        assert_eq!(BitmapExtension::get_bitmap_offset(-tc * 1024, 1).unwrap(), 0);
        assert_eq!(BitmapExtension::get_bitmap_offset(-tc * 1025, 1).unwrap(), 1);
        assert_eq!(BitmapExtension::get_bitmap_offset(-tc * 7394, 1).unwrap(), 13);
        assert!(BitmapExtension::get_bitmap_offset(-tc * 7395, 1).is_err());

        let bit = |w: &[u64; 8], i: usize| (w[i / 64] >> (i % 64)) & 1 == 1;
        let mut e = BitmapExtension::default();
        for s in [tc * 512, tc * 513, tc * 7393, -tc * 513, -tc * 514, -tc * 1024, -tc * 7394] {
            e.flip_tick_array_bit(s, 1).unwrap();
        }
        assert!(bit(&e.positive[0], 0) && bit(&e.positive[0], 1) && bit(&e.positive[13], 225));
        assert!(bit(&e.negative[0], 511) && bit(&e.negative[0], 510) && bit(&e.negative[0], 0));
        assert!(bit(&e.negative[13], 286));

        let mut e = BitmapExtension::default();
        for s in [3 * tc * 512, 3 * tc * 2464, -3 * tc * 513, -3 * tc * 2465] {
            e.flip_tick_array_bit(s, 3).unwrap();
        }
        assert!(bit(&e.positive[0], 0) && bit(&e.positive[3], 416));
        assert!(bit(&e.negative[0], 511) && bit(&e.negative[3], 95));

        
        let mut e = BitmapExtension::default();
        for s in [tc * 512, tc * 1000, tc * 7393] {
            e.flip_tick_array_bit(s, 1).unwrap();
        }
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(tc * 511, 1, false).unwrap().1, tc * 512);
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(tc * 512, 1, false).unwrap().1, tc * 1000);
        assert!(!e.next_initialized_tick_array_from_one_bitmap(tc * 7393, 1, false).unwrap().0);
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(tc * 1001, 1, true).unwrap().1, tc * 1000);
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(tc * 1000, 1, true).unwrap().1, tc * 512);
        assert!(e.next_initialized_tick_array_from_one_bitmap(tc * 512, 1, true).is_err());

        
        let mut e = BitmapExtension::default();
        for s in [-tc * 513, -tc * 1000, -tc * 7394] {
            e.flip_tick_array_bit(s, 1).unwrap();
        }
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(-tc * 1001, 1, false).unwrap().1, -tc * 1000);
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(-tc * 1000, 1, false).unwrap().1, -tc * 513);
        assert!(e.next_initialized_tick_array_from_one_bitmap(-tc * 513, 1, false).is_err());
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(-tc * 512, 1, true).unwrap().1, -tc * 513);
        assert_eq!(e.next_initialized_tick_array_from_one_bitmap(-tc * 513, 1, true).unwrap().1, -tc * 1000);
        assert!(!e.next_initialized_tick_array_from_one_bitmap(-tc * 7394, 1, true).unwrap().0);

        
        let e = BitmapExtension {
            pool_id: [0; 32],
            positive: [[u64::MAX; 8]; 14],
            negative: [[u64::MAX; 8]; 14],
        };
        for ts in [1u16, 10] {
            let step = ts as i32 * tc;
            let boundary = max_tick_in_tickarray_bitmap(ts);
            let loops = (tick_array_start_index(MAX_TICK, ts) - boundary) / step;
            let mut s = boundary;
            for _ in 0..loops {
                let (f, n) = e.next_initialized_tick_array_from_one_bitmap(s, ts, false).unwrap();
                assert!(f);
                assert_eq!(n, s + step);
                s = n;
            }
            let mut s = -boundary;
            for _ in 0..loops {
                let (f, n) = e.next_initialized_tick_array_from_one_bitmap(s, ts, true).unwrap();
                assert!(f);
                assert_eq!(n, s - step);
                s = n;
            }
        }
    }

    fn pool_with(ts: u16, tick: i32) -> PoolSnapshot {
        PoolSnapshot {
            tick_spacing: ts,
            tick_current: tick,
            sqrt_price_x64: get_sqrt_price_at_tick(tick).unwrap(),
            ..Default::default()
        }
    }

    fn set_default_bit(pool: &mut PoolSnapshot, start: i32) {
        let bit = (start / tick_count(pool.tick_spacing) + 512) as usize;
        pool.tick_array_bitmap[bit / 64] |= 1 << (bit % 64);
    }

    #[test]
    fn traversal_across_default_and_extension() {
        let mut pool = pool_with(1, 100);
        let mut ext = BitmapExtension::default();
        for s in [60, -60, -30720, -30780, -461_000 / 60 * 60] {
            if is_overflow_default_tickarray_bitmap(s, 1) {
                if check_is_valid_start_index(s, 1) && BitmapExtension::get_bitmap_offset(s, 1).is_ok() {
                    ext.flip_tick_array_bit(s, 1).unwrap();
                }
            } else {
                set_default_bit(&mut pool, s);
            }
        }
        for s in [30720, 60000, 443580] {
            ext.flip_tick_array_bit(s, 1).unwrap();
        }
        let down = initialized_tick_array_starts(&pool, Some(&ext), true, 10);
        assert_eq!(down, vec![60, -60, -30720, -30780]);
        let up = initialized_tick_array_starts(&pool, Some(&ext), false, 10);
        assert_eq!(up, vec![60, 30720, 60000, 443580]);
        
        assert_eq!(initialized_tick_array_starts(&pool, None, false, 10), vec![60]);
        
        let mut pool2 = pool_with(1, 45_000);
        pool2.tick_array_bitmap = pool.tick_array_bitmap;
        assert_eq!(initialized_tick_array_starts(&pool2, Some(&ext), true, 10), vec![30720, 60, -60, -30720, -30780]);
        assert_eq!(initialized_tick_array_starts(&pool2, Some(&ext), false, 10), vec![60000, 443580]);
        
        let mut p60 = pool_with(60, -200_000);
        for s in [-201_600, -3600, 439_200, -446_400] {
            set_default_bit(&mut p60, s);
        }
        assert_eq!(initialized_tick_array_starts(&p60, None, true, 10), vec![-201_600, -446_400]);
        assert_eq!(initialized_tick_array_starts(&p60, None, false, 10), vec![-201_600, -3600, 439_200]);
    }

    fn full_range_array(start: i32, ticks: &[(i32, i128)], ts: u16) -> TickArray {
        let mut ta = TickArray {
            start_tick_index: start,
            ticks: vec![TickState::default(); 60],
            ..Default::default()
        };
        for &(t, net) in ticks {
            let off = ((t - start) / ts as i32) as usize;
            ta.ticks[off] = TickState {
                tick: t,
                liquidity_net: net,
                liquidity_gross: net.unsigned_abs(),
                ..Default::default()
            };
            ta.initialized_tick_count += 1;
        }
        ta
    }

    #[test]
    fn swap_single_range_matches_closed_form() {
        
        let ts = 60u16;
        let liq: u128 = 1_000_000_000_000;
        let mut pool = pool_with(ts, 1234);
        let lower = -443580;
        let upper = 443580;
        let a_lo = full_range_array(tick_array_start_index(lower, ts), &[(lower, liq as i128)], ts);
        let a_up = full_range_array(tick_array_start_index(upper, ts), &[(upper, -(liq as i128))], ts);
        set_default_bit(&mut pool, a_lo.start_tick_index);
        set_default_bit(&mut pool, a_up.start_tick_index);
        pool.liquidity = liq;
        let cfg = AmmConfigState { trade_fee_rate: 2500, tick_spacing: ts, ..Default::default() };

        
        let q = quote_exact_input(&pool, &cfg, &[a_lo.clone()], true, 1_000_000, 1).unwrap();
        let amt = 1_000_000u64 * (1_000_000 - 2500) / 1_000_000;
        let s = pool.sqrt_price_x64;
        let num = U256([0, liq as u64, (liq >> 64) as u64, 0]);
        let den = num.checked_add(U256::mul_u128(amt as u128, s)).unwrap();
        let next = num.mul_div_ceil(U256::from_u128(s), den).unwrap().to_u128().unwrap();
        assert_eq!(q.end_sqrt_price_x64, next);
        let (ain, aout) = get_delta_amounts_for_swap(next, s, liq, true).unwrap();
        assert_eq!(q.amount_out, aout);
        assert_eq!(q.amount_in, 1_000_000);
        assert_eq!(q.fee_amount, 1_000_000 - ain);
        assert_eq!(q.end_tick, get_tick_at_sqrt_price(next).unwrap());
        assert_eq!(q.tick_arrays_used, vec![a_lo.start_tick_index]);

        
        pool.fee_on = 1;
        let q = quote_exact_input(&pool, &cfg, &[a_up.clone()], false, 5_000_000, 1).unwrap();
        let next = s + ((5_000_000u128 << 64) / liq);
        let (_, gross) = get_delta_amounts_for_swap(next, s, liq, false).unwrap();
        let fee = mul_div_ceil_u64(gross, 2500, 1_000_000).unwrap();
        assert_eq!(q.end_sqrt_price_x64, next);
        assert_eq!(q.amount_out, gross - fee);
        assert_eq!(q.fee_amount, fee);
        assert_eq!(q.amount_in, 5_000_000);
        assert!(q.fee_on_token0);

        
        assert_eq!(quote_exact_input(&pool, &cfg, &[], true, 1000, 1).unwrap_err(), ClmmError::NotEnoughTickArrayAccount);
        assert_eq!(
            quote_exact_input(&pool, &cfg, &[a_up.clone()], true, 1000, 1).unwrap_err(),
            ClmmError::InvalidFirstTickArrayAccount
        );
        
        pool.open_time = 10;
        assert_eq!(quote_exact_input(&pool, &cfg, &[a_lo.clone()], true, 1000, 10).unwrap_err(), ClmmError::PoolNotOpen);
    }

    #[test]
    fn swap_crosses_ticks_and_arrays() {
        let ts = 10u16;
        let mut pool = pool_with(ts, 5);
        
        let l1: i128 = 1_000_000_000_000;
        let l2: i128 = 500_000_000_000;
        let arr_m600 = full_range_array(-600, &[(-600, l1), (-300, -l2)], ts);
        let arr_m1200 = full_range_array(-1200, &[(-1200, l2)], ts);
        let arr_600 = full_range_array(600, &[(600, -l1)], ts);
        for a in [&arr_m600, &arr_m1200, &arr_600] {
            set_default_bit(&mut pool, a.start_tick_index);
        }
        pool.liquidity = l1 as u128;
        let cfg = AmmConfigState { trade_fee_rate: 500, tick_spacing: ts, ..Default::default() };
        let arrays = vec![arr_m600.clone(), arr_m1200.clone()];
        assert_eq!(initialized_tick_array_starts(&pool, None, true, 5), vec![-600, -1200]);
        
        let amount = 45_000_000_000u64;
        let q = quote_exact_input(&pool, &cfg, &arrays, true, amount, 1).unwrap();
        assert_eq!(q.tick_arrays_used, vec![-600, -1200]);
        assert!(q.end_tick < -600 && q.end_tick >= -1200, "{}", q.end_tick);
        assert_eq!(q.end_liquidity, l2 as u128);
        
        let mut s = pool.sqrt_price_x64;
        let mut rem = amount;
        let mut out = 0u64;
        let mut fee = 0u64;
        let mut l = l1 as u128;
        for (t, net) in [(-300, -l2), (-600, l1), (-1200, l2)] {
            let target = get_sqrt_price_at_tick(t).unwrap();
            let st = compute_swap(s, target, l, rem, 500, true, true, true).unwrap();
            rem -= st.amount_in + st.fee_amount;
            out += st.amount_out;
            fee += st.fee_amount;
            s = st.sqrt_price_next_x64;
            if s != target {
                break;
            }
            l = (l as i128 - net) as u128;
        }
        assert_eq!(rem, 0);
        assert_eq!(q.amount_out, out);
        assert_eq!(q.fee_amount, fee);
        assert_eq!(q.end_sqrt_price_x64, s);
        
        let bad = vec![arr_m1200.clone(), arr_m600.clone()];
        assert_eq!(quote_exact_input(&pool, &cfg, &bad, true, amount, 1).unwrap_err(), ClmmError::NotEnoughTickArrayAccount);
        
        let lead = vec![arr_600.clone(), arr_m600.clone(), arr_m1200.clone()];
        assert_eq!(quote_exact_input(&pool, &cfg, &lead, true, amount, 1).unwrap(), q);
        
        let e = quote_exact_input(&pool, &cfg, &[arr_600.clone()], false, 10_000_000_000_000, 1).unwrap_err();
        assert!(matches!(e, ClmmError::LiquidityInsufficient | ClmmError::PartialFill { .. }), "{:?}", e);
    }

    #[test]
    fn dynamic_fee_reference_and_rate() {
        let mut d = DynamicFeeInfo {
            filter_period: 60,
            decay_period: 600,
            reduction_factor: 5000,
            dynamic_fee_control: 15000,
            max_volatility_accumulator: 600000,
            tick_spacing_index_reference: -5635,
            volatility_reference: 5307,
            volatility_accumulator: 15307,
            last_update_timestamp: 1000,
        };
        let mut x = d;
        x.update_reference(-5640, 1030);
        assert_eq!(x, d); 
        let mut x = d;
        x.update_reference(-5640, 1100);
        assert_eq!((x.tick_spacing_index_reference, x.volatility_reference, x.last_update_timestamp), (-5640, 7653, 1100));
        let mut x = d;
        x.update_reference(-5640, 1600);
        assert_eq!((x.volatility_reference, x.last_update_timestamp), (0, 1600));
        d.update_volatility_accumulator(-5638).unwrap();
        assert_eq!(d.volatility_accumulator, 5307 + 3 * 10000);
        
        let crossed = 35307u128 * 10;
        let want = (15000 * crossed * crossed + 10_000_000_000_000 - 1) / 10_000_000_000_000;
        assert_eq!(d.dynamic_fee_rate(10).unwrap() as u128, want);
        assert_eq!(tick_spacing_index_from_tick(-1, 10), -1);
        assert_eq!(tick_spacing_index_from_tick(-10, 10), -1);
        assert_eq!(tick_spacing_index_from_tick(-11, 10), -2);
        assert_eq!(tick_spacing_index_from_tick(19, 10), 1);
    }

    #[test]
    fn limit_order_fill_zero_for_one() {
        
        let ts = 10u16;
        let mut pool = pool_with(ts, 5);
        let mut arr = full_range_array(-600, &[], ts);
        arr.ticks[50] = TickState { tick: -100, orders_amount: 1_000_000, unfilled_ratio_x64: Q64, ..Default::default() };
        arr.initialized_tick_count = 1;
        set_default_bit(&mut pool, -600);
        pool.liquidity = 0;
        let cfg = AmmConfigState { trade_fee_rate: 1000, tick_spacing: ts, ..Default::default() };
        let q = quote_exact_input(&pool, &cfg, &[arr.clone()], true, 500_000, 1).unwrap();
        let price = get_price_from_sqrt_price(get_sqrt_price_at_tick(-100).unwrap(), false).unwrap();
        let fee = mul_div_ceil_u64(500_000, 1000, 1_000_000).unwrap();
        let out = mul_div_floor_u128((500_000 - fee) as u128, price, Q64).unwrap() as u64;
        assert_eq!(q.amount_out, out);
        assert_eq!(q.fee_amount, fee);
        assert_eq!(q.end_tick, -100);
        assert_eq!(q.end_sqrt_price_x64, get_sqrt_price_at_tick(-100).unwrap());
        assert_eq!(q.updated_ticks.len(), 1);
        assert_eq!(q.updated_ticks[0].tick.part_filled_orders_remaining, 1_000_000 - out);
        
        assert!(quote_exact_input(&pool, &cfg, &[arr], true, 5_000_000, 1).is_err());
    }

    fn mock_order_tick(tick: i32, order_phase: u64, orders_amount: u64, part: u64) -> TickState {
        TickState {
            tick,
            liquidity_gross: 1_000_000,
            order_phase,
            orders_amount,
            part_filled_orders_remaining: part,
            unfilled_ratio_x64: Q64,
            ..Default::default()
        }
    }

    fn match_at_tick(t: &mut TickState, amount: u64, z41: bool, base: bool, fee: u32, fee_in: bool) -> LimitOrderMatch {
        let sp = get_sqrt_price_at_tick(t.tick).unwrap();
        t.match_limit_order_with_sqrt_price(amount, z41, base, fee, fee_in, sp).unwrap()
    }

    
    #[test]
    fn limit_order_match_program_vectors() {
        
        let mut t = mock_order_tick(0, 0, 1000, 0);
        let r = match_at_tick(&mut t, 500, false, true, 100, true);
        assert_eq!((r.amount_in, r.amount_out, r.amm_fee_amount), (499, 499, 1));
        assert_eq!(t.part_filled_orders_remaining, 501);
        let r = match_at_tick(&mut t, 1000, false, true, 100, true);
        assert_eq!((r.amount_in, r.amount_out), (501, 501));
        assert_eq!(t.part_filled_orders_remaining, 0);
        
        let mut t = mock_order_tick(0, 0, 1000, 0);
        let r = match_at_tick(&mut t, 500, false, true, 100, false);
        assert_eq!((r.amount_in, r.amount_out, r.amm_fee_amount), (500, 499, 1));
        assert_eq!(t.part_filled_orders_remaining, 500);
        let r = match_at_tick(&mut t, 400, false, false, 100, false);
        assert_eq!((r.amount_out, r.amm_fee_amount), (400, 1));
        assert_eq!(t.part_filled_orders_remaining, 500 - 401);
        let r = match_at_tick(&mut t, 1000, false, true, 100, false);
        assert_eq!((r.amount_out, r.amm_fee_amount), (98, 1));
        assert_eq!(t.part_filled_orders_remaining, 0);
        
        let mut t = mock_order_tick(0, 0, 1000, 0);
        let r = match_at_tick(&mut t, 500, false, false, 100, true);
        assert_eq!(r.amount_out, 500);
        assert!(r.amm_fee_amount > 0);
        assert_eq!(t.part_filled_orders_remaining, 500);
        
        let mut t = mock_order_tick(0, 1_000_000, 500, 200);
        let r = match_at_tick(&mut t, 400, false, false, 0, true);
        assert_eq!(r.amount_out, 400);
        assert_eq!((t.part_filled_orders_remaining, t.orders_amount, t.order_phase), (300, 0, 1_000_001));
        
        let mut t = mock_order_tick(0, 0, 1000, 0);
        let r = match_at_tick(&mut t, 500, false, true, 0, true);
        assert_eq!((r.amm_fee_amount, r.amount_in, r.amount_out), (0, 500, 500));
        let r = match_at_tick(&mut t, 400, false, false, 0, false);
        assert_eq!((r.amm_fee_amount, r.amount_out), (0, 400));
        
        let mut t = mock_order_tick(0, 0, 1000, 0);
        match_at_tick(&mut t, 500, false, true, 100, true);
        let r = match_at_tick(&mut t, 2000, false, true, 100, true);
        assert!(r.amount_out <= 501 && r.amount_out >= 499 && r.amount_in < 2000);
        assert_eq!(t.part_filled_orders_remaining, 0);
    }

    fn lo_test_setup(arrays_order: &[i32]) -> (PoolSnapshot, AmmConfigState, Vec<TickArray>) {
        
        let liquidity: u128 = 5124165121219;
        let mut pool = pool_with(1, 0);
        pool.liquidity = liquidity;
        let cfg = AmmConfigState { trade_fee_rate: 1000, tick_spacing: 1, ..Default::default() };
        let mut t_m10 = TickState { tick: -10, liquidity_gross: 6408486554, liquidity_net: -6408486554, ..Default::default() };
        t_m10.orders_amount = 1_000_000;
        let t_10 = TickState { tick: 10, orders_amount: 1_000_000, ..Default::default() };
        let t_20 = TickState { tick: 20, liquidity_gross: 790917615645, ..Default::default() };
        let t_m20 = TickState { tick: -20, liquidity_gross: 1330680689, liquidity_net: -1330680689, ..Default::default() };
        let mk = |start: i32, ts: Vec<TickState>| {
            let mut a = TickArray { start_tick_index: start, ticks: vec![TickState::default(); 60], ..Default::default() };
            a.initialized_tick_count = ts.len() as u8;
            for t in ts {
                a.ticks[(t.tick - start) as usize] = t;
            }
            a
        };
        let mut arrays = Vec::new();
        for &s in arrays_order {
            let a = if s == 0 { mk(0, vec![t_10, t_20]) } else { mk(-60, vec![t_m20, t_m10]) };
            set_default_bit(&mut pool, s);
            arrays.push(a);
        }
        (pool, cfg, arrays)
    }

    fn find_tick(arrays: &[TickArray], tick: i32) -> TickState {
        *arrays.iter().flat_map(|a| a.ticks.iter()).find(|t| t.tick == tick).unwrap()
    }

    
    #[test]
    fn limit_order_swap_zero_for_one_program_test() {
        let (mut pool, cfg, mut arrays) = lo_test_setup(&[0, -60]);
        let liquidity = pool.liquidity;
        let unfilled = find_tick(&arrays, -10).limit_order_unfilled_amount().unwrap();
        let q = simulate_swap(&pool, &cfg, None, &arrays, true, 2565160190, get_sqrt_price_at_tick(-10).unwrap(), true, 1).unwrap();
        apply_swap_to_state(&mut pool, None, &mut arrays, &q).unwrap();
        assert_eq!(pool.tick_current, -10);
        assert_eq!(pool.sqrt_price_x64, get_sqrt_price_at_tick(-10).unwrap());
        assert_eq!(pool.liquidity, liquidity);
        let mut t1 = find_tick(&arrays, -10);
        let unfilled1 = t1.limit_order_unfilled_amount().unwrap();
        assert_eq!(unfilled1, unfilled);

        let r = match_at_tick(&mut t1, 1000, true, false, cfg.trade_fee_rate, true);
        let amount_in = r.amount_in + r.amm_fee_amount;
        let q = simulate_swap(&pool, &cfg, None, &arrays, true, amount_in, MIN_SQRT_PRICE_X64 + 1, true, 1).unwrap();
        assert_eq!((q.amount_out, q.amount_in), (1000, amount_in));
        apply_swap_to_state(&mut pool, None, &mut arrays, &q).unwrap();
        assert_eq!(pool.tick_current, -10);
        assert_eq!(pool.sqrt_price_x64, get_sqrt_price_at_tick(-10).unwrap());
        assert_eq!(pool.liquidity, liquidity);
        let mut t2 = find_tick(&arrays, -10);
        let unfilled2 = t2.limit_order_unfilled_amount().unwrap();
        assert_eq!(unfilled1 - unfilled2, 1000);

        let r = match_at_tick(&mut t2, unfilled2, true, false, cfg.trade_fee_rate, true);
        let amount_in = r.amount_in + r.amm_fee_amount;
        let q = simulate_swap(&pool, &cfg, None, &arrays, true, amount_in, get_sqrt_price_at_tick(-11).unwrap(), true, 1).unwrap();
        assert_eq!((q.amount_in, q.amount_out), (amount_in, unfilled2));
        apply_swap_to_state(&mut pool, None, &mut arrays, &q).unwrap();
        assert_eq!(pool.tick_current, -11);
        assert_eq!(pool.liquidity, add_delta(liquidity, 6408486554).unwrap());
        assert_eq!(pool.sqrt_price_x64, get_sqrt_price_at_tick(-10).unwrap());
        assert_eq!(find_tick(&arrays, -10).limit_order_unfilled_amount().unwrap(), 0);
    }

    
    #[test]
    fn limit_order_swap_one_for_zero_program_test() {
        let (mut pool, cfg, mut arrays) = lo_test_setup(&[-60, 0]);
        let liquidity = pool.liquidity;
        let unfilled = find_tick(&arrays, 10).limit_order_unfilled_amount().unwrap();
        let q = simulate_swap(&pool, &cfg, None, &arrays, false, 2565160190, get_sqrt_price_at_tick(10).unwrap(), true, 1).unwrap();
        apply_swap_to_state(&mut pool, None, &mut arrays, &q).unwrap();
        assert_eq!(pool.tick_current, 9);
        assert_eq!(pool.sqrt_price_x64, get_sqrt_price_at_tick(10).unwrap());
        assert_eq!(pool.liquidity, liquidity);
        let mut t1 = find_tick(&arrays, 10);
        let unfilled1 = t1.limit_order_unfilled_amount().unwrap();
        assert_eq!(unfilled1, unfilled);

        let r = match_at_tick(&mut t1, 1000, false, false, cfg.trade_fee_rate, true);
        let amount_in = r.amount_in + r.amm_fee_amount;
        let q = simulate_swap(&pool, &cfg, None, &arrays, false, amount_in, MAX_SQRT_PRICE_X64 - 1, true, 1).unwrap();
        assert_eq!((q.amount_out, q.amount_in), (1000, amount_in));
        apply_swap_to_state(&mut pool, None, &mut arrays, &q).unwrap();
        assert_eq!(pool.tick_current, 9);
        assert_eq!(pool.sqrt_price_x64, get_sqrt_price_at_tick(10).unwrap());
        let mut t2 = find_tick(&arrays, 10);
        let unfilled2 = t2.limit_order_unfilled_amount().unwrap();
        assert_eq!(unfilled1 - unfilled2, 1000);

        let r = match_at_tick(&mut t2, unfilled2, false, false, cfg.trade_fee_rate, true);
        let amount_in = r.amount_in + r.amm_fee_amount;
        let q = simulate_swap(&pool, &cfg, None, &arrays, false, amount_in, MAX_SQRT_PRICE_X64 - 1, true, 1).unwrap();
        apply_swap_to_state(&mut pool, None, &mut arrays, &q).unwrap();
        assert_eq!(pool.tick_current, 10);
        assert_eq!(pool.sqrt_price_x64, get_sqrt_price_at_tick(10).unwrap());
        assert_eq!(pool.liquidity, liquidity);
        assert_eq!(find_tick(&arrays, 10).limit_order_unfilled_amount().unwrap(), 0);
        
        assert_eq!(arrays.iter().find(|a| a.start_tick_index == 0).unwrap().initialized_tick_count, 1);
    }

    #[test]
    fn parse_rejects_bad_accounts() {
        assert!(parse_pool(&[0u8; 1544]).is_err());
        assert!(parse_pool(&[0u8; 1543]).is_err());
        assert!(parse_tick_array(&[0u8; 10240]).is_err());
        assert!(parse_bitmap_extension(&[0u8; 1832]).is_err());
        assert!(parse_amm_config(&[0u8; 117]).is_err());
        let mut p = vec![0u8; 1544];
        p[..8].copy_from_slice(&POOL_STATE_DISCRIMINATOR);
        p[235] = 1;
        let snap = parse_pool(&p).unwrap();
        assert_eq!(snap.dynamic_fee, None);
        p[1096 + 79] = 1; 
        assert!(parse_pool(&p).unwrap().dynamic_fee.is_some());
    }
}
