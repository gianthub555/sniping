//! Program-derived addresses without allocating. `Pubkey::find_program_address` (solana-address
//! 2.6, `syscalls.rs`) copies the seed list into a new `Vec` for every bump it tries; this is the
//! same search (bump 255 down, the first off-curve hash) with the seeds on the stack.

use {crate::utils::utils::ASSOCIATED_TOKEN_PROGRAM_ID, solana_pubkey::Pubkey};

const MAX_SEEDS: usize = 15;

/// The program address of `seeds` under `program_id` and its bump, as `find_program_address`.
#[inline]
pub fn find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> (Pubkey, u8) {
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
}
