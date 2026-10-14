//! Program-derived addresses without allocating. `Pubkey::find_program_address` (solana-address
//! 2.6, `syscalls.rs`) copies the seed list into a new `Vec` for every bump it tries; this is the
//! same search (bump 255 down, the first off-curve hash) with the seeds on the stack.

use {
    crate::utils::utils::ASSOCIATED_TOKEN_PROGRAM_ID,
    crrl::field::GF25519,
    sha2::{Digest, Sha256},
    solana_pubkey::Pubkey,
    std::sync::LazyLock,
};

const MAX_SEEDS: usize = 15;
const MAX_SEED_LEN: usize = 32;
const PDA_MARKER: &[u8] = b"ProgramDerivedAddress";

/// HOT_FAST_PDA=true: the search's on-curve test is a Legendre symbol instead of the library's
/// point decompression (`fast_find`). Read once (`warm`, at the engine's start).
static FAST: LazyLock<bool> = LazyLock::new(|| crate::config::env_opt("HOT_FAST_PDA").is_some_and(|v| v.eq_ignore_ascii_case("true")));
/// Edwards25519's d = -121665/121666.
static D: LazyLock<GF25519> = LazyLock::new(|| GF25519::from_i32(-121665) / GF25519::from_i32(121666));

/// Reads HOT_FAST_PDA and computes d, so the first search on the hot thread does neither.
pub fn warm() -> bool {
    LazyLock::force(&D);
    *FAST
}

/// The program address of `seeds` under `program_id` and its bump, as `find_program_address`.
#[inline]
pub fn find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> (Pubkey, u8) {
    if *FAST {
        return fast_find(seeds, program_id);
    }
    assert!(seeds.len() <= MAX_SEEDS, "too many seeds");
    let n = seeds.len();
    let mut bump = u8::MAX;
    loop {
        let b = [bump];
        let mut with_bump: [&[u8]; MAX_SEEDS + 1] = [&[]; MAX_SEEDS + 1];
        with_bump[..n].copy_from_slice(seeds);
        with_bump[n] = &b;
        if let Ok(address) = Pubkey::create_program_address(&with_bump[..n + 1], program_id) {
            return (address, bump);
        }
        bump = bump.checked_sub(1).expect("a program address exists for some bump");
    }
}

/// The same search with the on-curve test of `on_curve`: sha256(seeds, bump, program, marker), the
/// first hash (bump 255 down) that is not a curve point. Each try on Frankfurt's CPU: 0.6 us of
/// hashing and 2.3 us of test, against 4.8 us for the library's decompression.
#[inline]
pub fn fast_find(seeds: &[&[u8]], program_id: &Pubkey) -> (Pubkey, u8) {
    assert!(seeds.len() <= MAX_SEEDS, "too many seeds");
    assert!(seeds.iter().all(|s| s.len() <= MAX_SEED_LEN), "a seed over 32 bytes");
    let mut bump = u8::MAX;
    loop {
        let mut h = Sha256::new();
        for s in seeds {
            h.update(s);
        }
        h.update([bump]);
        h.update(program_id.as_ref());
        h.update(PDA_MARKER);
        let hash: [u8; 32] = h.finalize().into();
        if !on_curve(&hash) {
            return (Pubkey::new_from_array(hash), bump);
        }
        bump = bump.checked_sub(1).expect("a program address exists for some bump");
    }
}

/// Whether `bytes` decompress to an Edwards25519 point, as curve25519-dalek 4's
/// `CompressedEdwardsY::decompress().is_some()` (what `create_program_address` rejects): y is the
/// 255 low bits reduced mod p (bit 255, the sign, plays no part), and the point exists when
/// u/v = (y^2 - 1)/(d y^2 + 1) is a square or zero. v is never zero (-1/d is not a square), so that
/// is the Legendre symbol of u*v being 0 or 1.
#[inline]
pub fn on_curve(bytes: &[u8; 32]) -> bool {
    let mut b = *bytes;
    b[31] &= 0x7f;
    let y = GF25519::decode_reduce(&b);
    let yy = y.square();
    let u = yy - GF25519::ONE;
    let v = yy * *D + GF25519::ONE;
    (u * v).legendre() >= 0
}

/// `get_associated_token_address`, allocation-free.
#[inline]
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    find_program_address(&[owner.as_ref(), token_program.as_ref(), mint.as_ref()], &ASSOCIATED_TOKEN_PROGRAM_ID).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_addresses_and_bumps_as_the_library() {
        let mut seed = 1u64;
        for _ in 0..2_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = Pubkey::new_unique();
            let tag = seed.to_le_bytes();
            let program = Pubkey::new_unique();
            let seeds: [&[u8]; 3] = [b"bonding-curve", a.as_ref(), &tag];
            assert_eq!(find_program_address(&seeds, &program), Pubkey::find_program_address(&seeds, &program));
            assert_eq!(find_program_address(&seeds[..1], &program), Pubkey::find_program_address(&seeds[..1], &program));
        }
        let (owner, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let tp = crate::utils::utils::TOKEN_2022_PROGRAM_ID;
        assert_eq!(associated_token_address(&owner, &mint, &tp), crate::utils::utils::get_associated_token_address(&owner, &mint, &tp));
    }

    #[test]
    fn the_fast_search_finds_the_librarys_address_and_bump() {
        let mut seed = 7u64;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (a, b, program) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
            let tag = seed.to_le_bytes();
            let three: [&[u8]; 3] = [a.as_ref(), b.as_ref(), &tag];
            let curve: [&[u8]; 2] = [b"bonding-curve-v2", a.as_ref()];
            assert_eq!(fast_find(&three, &program), Pubkey::find_program_address(&three, &program));
            assert_eq!(fast_find(&curve, &program), Pubkey::find_program_address(&curve, &program));
            assert_eq!(fast_find(&three[..1], &program), Pubkey::find_program_address(&three[..1], &program));
        }
        let (owner, mint) = (Pubkey::new_unique(), Pubkey::new_unique());
        let tp = crate::utils::utils::TOKEN_2022_PROGRAM_ID;
        let seeds: [&[u8]; 3] = [owner.as_ref(), tp.as_ref(), mint.as_ref()];
        assert_eq!(fast_find(&seeds, &ASSOCIATED_TOKEN_PROGRAM_ID).0, crate::utils::utils::get_associated_token_address(&owner, &mint, &tp));
    }

    #[test]
    fn the_on_curve_test_agrees_with_dalek_on_random_and_edge_inputs() {
        use curve25519_dalek::edwards::CompressedEdwardsY;
        let lib = |b: &[u8; 32]| CompressedEdwardsY(*b).decompress().is_some();
        let mut s = 0x9E3779B97F4A7C15u64;
        let mut on = 0;
        for _ in 0..300_000 {
            let mut b = [0u8; 32];
            for ch in b.chunks_mut(8) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ch.copy_from_slice(&s.to_le_bytes());
            }
            assert_eq!(on_curve(&b), lib(&b), "{b:02x?}");
            on += lib(&b) as u32;
        }
        assert!((140_000..160_000).contains(&on), "about half are points ({on})");
        // y = k, p - k and p + k (not reduced), each with the sign bit off and on; all ones.
        let p: [u8; 32] = {
            let mut x = [0xffu8; 32];
            x[0] = 0xed;
            x[31] = 0x7f;
            x
        };
        let add = |base: [u8; 32], k: i64| {
            let (mut out, mut carry) = (base, k as i128);
            for byte in out.iter_mut() {
                let v = *byte as i128 + carry;
                *byte = v.rem_euclid(256) as u8;
                carry = v.div_euclid(256);
            }
            out
        };
        let mut edge = vec![[0xffu8; 32]];
        for k in 0..64i64 {
            edge.push(add([0; 32], k));
            edge.push(add(p, -k));
            let mut above = add(p, k);
            above[31] &= 0x7f;
            edge.push(above);
        }
        for e in edge.clone() {
            let mut signed = e;
            signed[31] |= 0x80;
            edge.push(signed);
        }
        for e in &edge {
            assert_eq!(on_curve(e), lib(e), "{e:02x?}");
        }
    }
}
