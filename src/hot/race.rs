//! The lane race (ticket 5.1): which send lane lands first, measured with real transactions.
//!
//! Each round sends every enabled lane its own copy of the same transaction: advance the durable
//! nonce, a 1-lamport transfer to self, the compute budget, and a tip to one of that lane's
//! accounts (direct QUIC and Triton tip nobody). The copies differ only in the tip transfer, and
//! they all use the same nonce, so at most one can land per round: the lane whose copy lands has
//! won the round. A round is counted at `confirmed`; one that times out or fails stays in the
//! denominator. Run by `src/bin/lane_race.rs`; nothing here sends.

use {
    super::lanes::{LaneConfig, LANE_NAMES},
    super::stamps::LANES,
    crate::utils::{nonce, utils::transfer_sol},
    anyhow::{bail, Result},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::{legacy::Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
};

/// Compute units a race transaction may use (advance nonce, two transfers, the budget).
pub const RACE_CU_LIMIT: u32 = 3_000;
/// The base fee of one signature.
pub const SIGNATURE_FEE: u64 = 5_000;

/// One lane's copy for a round.
pub struct RaceTx {
    pub lane: usize,
    pub tip: u64,
    pub tip_account: Option<Pubkey>,
    pub bytes: Vec<u8>,
    pub signature: Signature,
}

/// Every enabled lane's copy for round `round`: the same nonce `value`, the same priority price,
/// each tipping only its own lane (accounts rotated by round).
pub fn round_txs(payer: &Keypair, nonce_account: &Pubkey, value: &Hash, cfg: &LaneConfig, round: u64, cu_price: u64) -> Result<Vec<RaceTx>> {
    let me = payer.pubkey();
    let mut out = Vec::new();
    for lane in (0..LANES).filter(|&l| cfg.enabled(l)) {
        let mut ixs = vec![
            nonce::advance_instruction(nonce_account, &me),
            crate::utils::utils::set_compute_unit_limit(RACE_CU_LIMIT),
            crate::utils::utils::set_compute_unit_price(cu_price),
            transfer_sol(&me, &me, 1),
        ];
        let tip = cfg.lanes[lane].tip;
        let accounts = cfg.lanes[lane].tip_accounts;
        let tip_account = (tip > 0 && !accounts.is_empty()).then(|| accounts[(round as usize) % accounts.len()]);
        if let Some(to) = tip_account {
            ixs.push(transfer_sol(&me, &to, tip));
        }
        let message = Message::new_with_blockhash(&ixs, Some(&me), value);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[payer])?;
        let bytes = bincode::serialize(&tx)?;
        if bytes.len() > super::template::PACKET {
            bail!("{}: the race transaction is {} bytes", LANE_NAMES[lane], bytes.len());
        }
        out.push(RaceTx { lane, tip: tip_account.map_or(0, |_| tip), tip_account, signature: tx.signatures[0], bytes });
    }
    Ok(out)
}

/// What a lane did over the race.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    /// Rounds it was in.
    pub rounds: u64,
    /// Rounds its copy landed (confirmed, no error).
    pub landed: u64,
    /// Its copy landed with an error (paid, did not count).
    pub failed: u64,
    /// Slots from the send to the landing, per landing.
    pub delays: Vec<u64>,
    /// Lamports it cost: fees and tips of its landed copies, failed ones included.
    pub lamports: u64,
}

impl Tally {
    /// A copy of this lane landed `delay` slots after the send, or failed on chain.
    pub fn land(&mut self, delay: u64, ok: bool, cost: u64) {
        self.lamports += cost;
        if ok {
            self.landed += 1;
            self.delays.push(delay);
        } else {
            self.failed += 1;
        }
    }
}

fn pct(v: &[u64], p: f64) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let mut v = v.to_vec();
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize].to_string()
}

/// The race's table: per lane, land rate, slot delays, same-slot share and SOL spent.
pub fn table(tallies: &[Tally; LANES]) -> String {
    let mut out = String::from("| lane | rounds | landed | land rate | median slot delay | same slot | p90 delay | p99 delay | failed on chain | SOL spent |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for (lane, t) in tallies.iter().enumerate().filter(|(_, t)| t.rounds > 0) {
        let same = t.delays.iter().filter(|&&d| d == 0).count();
        out.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {} | {} | {} | {} | {} | {:.6} |\n",
            LANE_NAMES[lane],
            t.rounds,
            t.landed,
            100.0 * t.landed as f64 / t.rounds as f64,
            pct(&t.delays, 0.5),
            if t.landed == 0 { "-".into() } else { format!("{:.1}%", 100.0 * same as f64 / t.landed as f64) },
            pct(&t.delays, 0.9),
            pct(&t.delays, 0.99),
            t.failed,
            t.lamports as f64 / 1e9
        ));
    }
    out
}

/// The plan of a race, without sending: each lane's copy (size, tip), and what `rounds` rounds
/// can cost at most (the dearest lane winning every round).
pub fn dry_plan(payer: &Keypair, cfg: &LaneConfig, rounds: u64, cu_price: u64) -> Result<String> {
    let fake_nonce = Pubkey::new_unique();
    let txs = round_txs(payer, &fake_nonce, &Hash::new_from_array([1; 32]), cfg, 0, cu_price)?;
    if txs.is_empty() {
        bail!("no lane is enabled");
    }
    let priority = cu_price as u128 * RACE_CU_LIMIT as u128 / 1_000_000;
    let mut out = format!("lane race, dry: {rounds} rounds, one every 1.6 s (~{:.0} min), priority {priority} lamports per copy\n", rounds as f64 * 1.6 / 60.0);
    let mut dearest = 0u64;
    for t in &txs {
        let cost = SIGNATURE_FEE + priority as u64 + t.tip;
        dearest = dearest.max(cost);
        out.push_str(&format!(
            "  {:<10} {} bytes, tip {} lamports{}, {} lamports if it lands\n",
            LANE_NAMES[t.lane],
            t.bytes.len(),
            t.tip,
            t.tip_account.map(|a| format!(" to {a}")).unwrap_or_default(),
            cost
        ));
    }
    out.push_str(&format!("  at most {:.6} SOL for the race (one landing per round), plus the nonce account's rent\n", (dearest * rounds) as f64 / 1e9));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::hot::lanes::{LaneSpec, LANE_ALLENHARK, LANE_HELIUS, LANE_JITO, LANE_QUIC},
    };

    fn cfg() -> LaneConfig {
        let mut lanes: [LaneSpec; LANES] = Default::default();
        for (i, tip) in [(LANE_HELIUS, 5_000), (LANE_ALLENHARK, 1_000_000), (LANE_JITO, 100_000), (LANE_QUIC, 0)] {
            lanes[i] = LaneSpec { enabled: true, url: Some("http://x/".into()), tip, tip_accounts: crate::hot::lanes::tip_accounts(i), ..Default::default() };
        }
        LaneConfig { lanes, set: "race".into(), jito_uuid: None, quic_identity: None, fee_default: 700_000, fee_cap: 5_000_000, outbid_bps: 1_000, dry_lanes: false, rotate: false, rotate_lanes: [true; crate::hot::stamps::LANES] }
    }

    #[test]
    fn the_copies_share_the_nonce_and_differ_only_in_the_tip() {
        let payer = Keypair::new();
        let nonce_account = Pubkey::new_unique();
        let value = Hash::new_from_array([4; 32]);
        let cfg = cfg();
        let txs = round_txs(&payer, &nonce_account, &value, &cfg, 3, 5_833).unwrap();
        assert_eq!(txs.iter().map(|t| t.lane).collect::<Vec<_>>(), vec![LANE_HELIUS, LANE_ALLENHARK, LANE_JITO, LANE_QUIC]);
        let mut sigs = std::collections::HashSet::new();
        for t in &txs {
            let tx: VersionedTransaction = bincode::deserialize(&t.bytes).unwrap();
            assert_eq!(tx.message.recent_blockhash(), &value, "the nonce value as the blockhash");
            let keys = tx.message.static_account_keys();
            let ixs = tx.message.instructions();
            assert_eq!(keys[ixs[0].program_id_index as usize], Pubkey::default(), "the system program's AdvanceNonceAccount first");
            assert_eq!(ixs[0].data, vec![4, 0, 0, 0]);
            assert_eq!(keys[ixs[0].accounts[0] as usize], nonce_account);
            assert_eq!(ixs.len(), if t.tip > 0 { 5 } else { 4 });
            if let Some(a) = t.tip_account {
                let accounts = cfg.lanes[t.lane].tip_accounts;
                assert!(accounts.contains(&a));
                assert_eq!(a, accounts[3 % accounts.len()], "rotated by round");
            }
            assert!(sigs.insert(t.signature), "each copy its own signature");
        }
    }

    #[test]
    fn a_dry_invocation_plans_without_sending_and_the_table_counts_misses() {
        let plan = dry_plan(&Keypair::new(), &cfg(), 1_000, 5_833).unwrap();
        assert!(plan.contains("1000 rounds") && plan.contains("allenhark") && plan.contains("at most"), "{plan}");
        let mut t: [Tally; LANES] = Default::default();
        t[LANE_HELIUS].rounds = 4;
        t[LANE_HELIUS].land(0, true, 6_000);
        t[LANE_HELIUS].land(2, true, 6_000);
        t[LANE_HELIUS].land(0, false, 6_000);
        let table = table(&t);
        assert!(table.contains("| helius | 4 | 2 | 50.0% |"), "a failed copy and a miss stay in the denominator: {table}");
        assert!(table.contains("| 50.0% |"), "same-slot share of the landings");
        assert!(table.contains("0.000018"), "every paid copy counted: {table}");
    }
}
