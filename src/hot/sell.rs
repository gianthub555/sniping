//! Sells through the hot lanes (2026-10-11, HOT_SELL_LANES). The slow path (`utils::auto_sell`)
//! still decides and builds the sell's instructions; this signs them with a presigned nonce
//! (`hot::presign`) and hands the signed bytes to the decision thread, which writes them on the
//! lanes it already holds open, as it does a buy: warm sockets, no HTTP client, no allocation on
//! that thread. The sell carries one tip per tipping lane (the lanes' own tips, as a buy does).
//! The landing is awaited as before, by signature. Off, or without lanes (DRY_RUN, inline mode,
//! a full queue, no blockhash): `None`, and the slow path sends it its own way.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use anyhow::{bail, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_signature::Signature;

use super::clock::now_ns;
use super::presign;
use super::spsc;
use super::template::PACKET;
use crate::config::{env_opt, trading_wallet};
use crate::service::SendProfile;
use crate::utils::build_and_sign::build_unsigned_with_limit;
use crate::utils::utils::transfer_sol;

/// Signed transactions waiting for the decision thread (by value across the ring).
pub const RING: usize = 64;

/// One signed transaction for the lanes.
#[derive(Clone, Copy)]
pub struct SendReq {
    pub mint: [u8; 32],
    pub len: u16,
    pub tx: [u8; PACKET],
    /// When it was queued (`clock::now_ns`), the stamp's `t_recv`.
    pub t_queued: u64,
    /// The lane to write it on (a fan-out copy), or `ALL_LANES`.
    pub lane: u8,
}

/// `SendReq::lane` for every lane.
pub const ALL_LANES: u8 = u8::MAX;

struct Queue {
    prod: spsc::Producer<SendReq>,
    thread: std::thread::Thread,
}

static QUEUE: Mutex<Option<Queue>> = Mutex::new(None);
/// The tipping lanes' accounts and tips (one transfer each, as in a buy).
static TIPS: OnceLock<Vec<(&'static [Pubkey], u64)>> = OnceLock::new();
/// Each lane that is on and its own tip (HOT_SELL_FANOUT: one copy per lane, only its tip).
static LANE_TIPS: OnceLock<[Option<Option<(&'static [Pubkey], u64)>>; super::stamps::LANES]> = OnceLock::new();
static SIGNER: Mutex<Option<presign::Presigner>> = Mutex::new(None);
static PICK: AtomicU32 = AtomicU32::new(0);
static ENABLED: LazyLock<bool> = LazyLock::new(|| env_opt("HOT_SELL_LANES").map_or(true, |v| v.eq_ignore_ascii_case("true")));

/// HOT_SELL_LANES (default true) and the decision thread's lanes installed.
pub fn enabled() -> bool {
    *ENABLED && QUEUE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// `engine::start` (live, SPSC mode): the ring's producer, the decision thread to wake, the lanes'
/// tips, and the sell signer (its own presigned nonces, refilled off the hot cores).
pub fn install(
    prod: spsc::Producer<SendReq>,
    thread: std::thread::Thread,
    tips: Vec<(&'static [Pubkey], u64)>,
    lane_tips: [Option<Option<(&'static [Pubkey], u64)>>; super::stamps::LANES],
    signer: Option<presign::Presigner>,
) {
    let _ = TIPS.set(tips);
    let _ = LANE_TIPS.set(lane_tips);
    *SIGNER.lock().unwrap_or_else(|e| e.into_inner()) = signer;
    *QUEUE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Queue { prod, thread });
}

/// The sell `instructions` of `mint` as one signed transaction for every lane (each tipping
/// lane's tip inside, the sell profile's priority fee and compute limit), queued for the
/// decision thread. Its signature; `None` when the lanes are not there (the slow path sends).
pub fn send(mint: &Pubkey, instructions: Vec<Instruction>) -> Result<Option<Signature>> {
    send_signed(mint, instructions, false)
}

/// DRY_RUN with the lanes timing paper trades (HOT_DRY_LANES): the paper sell's unlandable copy
/// (all-zero blockhash, paid and signed by the throwaway paper key: it cannot land and costs
/// nothing, as the paper buys' copies), written on the lanes as a live sell would be, so a paper
/// run times the sell path too. `instructions` must name the paper key as the seller.
pub fn send_dry(mint: &Pubkey, instructions: Vec<Instruction>) -> Result<Option<Signature>> {
    if super::nonces::sell_fanout() && enabled() && !super::nonces::is_live() {
        if let Some(sig) = send_dry_fanout(mint, &instructions)? {
            return Ok(Some(sig));
        }
    }
    send_signed(mint, instructions, true)
}

/// HOT_SELL_FANOUT in a paper run: the paper sell's copies, one per lane that is on, on a synthetic
/// nonce (all-zero value, an account nothing on chain uses), paid and signed by the throwaway paper
/// key: the fan-out's build, queue and lane writes timed, nothing can land. Sent once (re-bids and
/// cancels need landings: live only); the account is released at once.
fn send_dry_fanout(mint: &Pubkey, instructions: &[Instruction]) -> Result<Option<Signature>> {
    let Some(lane_tips) = LANE_TIPS.get() else { return Ok(None) };
    let copy = super::nonces::next_copy();
    let Some((index, account, value)) = super::nonces::acquire(copy) else { return Ok(None) };
    let wallet = solana_signer::Signer::pubkey(&*crate::config::hot_signer());
    let profile = SendProfile::Sell;
    let pick = PICK.fetch_add(1, Ordering::Relaxed) as usize;
    let mut first = None;
    for lane in (0..super::stamps::LANES).filter(|&l| lane_tips[l].is_some()) {
        let built = {
            let mut signer = SIGNER.lock().unwrap_or_else(|e| e.into_inner());
            let mut sign = |m: &[u8]| -> [u8; 64] {
                match signer.as_mut() {
                    Some(p) => p.sign(m),
                    None => {
                        let mut sig = [0u8; 64];
                        sig.copy_from_slice(solana_signer::Signer::sign_message(&*crate::config::hot_signer(), m).as_ref());
                        sig
                    }
                }
            };
            fanout_copy(mint, instructions, &wallet, lane, lane_tips[lane].flatten(), pick, (&account, &value), profile.priority_fee(), profile.compute_unit_limit(), &mut sign)
        };
        let Ok((req, sig, _)) = built else { continue };
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(q) = q.as_mut() else { break };
        if q.prod.push(req) {
            q.thread.unpark();
            first.get_or_insert(sig);
        }
    }
    super::nonces::unsent(index, copy);
    Ok(first)
}

fn send_signed(mint: &Pubkey, instructions: Vec<Instruction>, unlandable: bool) -> Result<Option<Signature>> {
    if !enabled() {
        return Ok(None);
    }
    // A paper copy is paid and signed by the throwaway paper key (its sell names that key too).
    let wallet = if unlandable { solana_signer::Signer::pubkey(&*crate::config::hot_signer()) } else { trading_wallet() };
    send_with(mint, instructions, &wallet, unlandable, |msg| {
        let mut s = SIGNER.lock().unwrap_or_else(|e| e.into_inner());
        match s.as_mut() {
            Some(p) => p.sign(msg),
            None => {
                let mut sig = [0u8; 64];
                sig.copy_from_slice(solana_signer::Signer::sign_message(&*crate::config::hot_signer(), msg).as_ref());
                sig
            }
        }
    })
}

/// `send` with the payer and the signature's maker given (tests).
pub fn send_with(mint: &Pubkey, instructions: Vec<Instruction>, wallet: &Pubkey, unlandable: bool, sign: impl FnOnce(&[u8]) -> [u8; 64]) -> Result<Option<Signature>> {
    let tips = TIPS.get().map(Vec::as_slice).unwrap_or(&[]);
    let pick = PICK.fetch_add(1, Ordering::Relaxed) as usize;
    let tip_ixs: Vec<Instruction> = tips
        .iter()
        .filter(|(list, lamports)| !list.is_empty() && *lamports > 0)
        .map(|(list, lamports)| transfer_sol(wallet, &list[pick % list.len()], *lamports))
        .collect();
    let profile = SendProfile::Sell;
    let mut tx = build_unsigned_with_limit(instructions, tip_ixs, wallet, profile.priority_fee(), profile.compute_unit_limit())?;
    if unlandable {
        tx.message.set_recent_blockhash(solana_hash::Hash::default());
    }
    let msg = tx.message.serialize();
    if 65 + msg.len() > PACKET {
        bail!("the sell is {} bytes, over a packet", 65 + msg.len());
    }
    let sig = sign(&msg);
    let mut req = SendReq { mint: mint.to_bytes(), len: (65 + msg.len()) as u16, tx: [0; PACKET], t_queued: now_ns(), lane: ALL_LANES };
    req.tx[0] = 1;
    req.tx[1..65].copy_from_slice(&sig);
    req.tx[65..65 + msg.len()].copy_from_slice(&msg);
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(q) = q.as_mut() else {
        return Ok(None);
    };
    if !q.prod.push(req) {
        log::warn!("hot sell: the queue is full ({RING} waiting); the slow path sends this one");
        return Ok(None);
    }
    q.thread.unpark();
    Ok(Some(Signature::from(sig)))
}

// ---- HOT_SELL_FANOUT (2026-10-12) ---------------------------------------------------------------

/// Re-bids after the first copies (one a slot, the fee doubled, the same nonce).
pub const REBIDS: u32 = 4;
/// How long a cancelled sell waits for its nonce account to settle (no copy of it can land after).
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How a fanned-out sell ended.
#[derive(Debug)]
pub struct Fanout {
    pub outcome: crate::utils::utils::ConfirmOutcome,
    pub fill: Option<crate::processors::position_feed::Fill>,
    /// The copy that landed (the first one sent when none did).
    pub signature: Signature,
    /// Re-bids sent, the last fee per CU (micro-lamports), the landed copy's lane and tip.
    pub rebids: u32,
    pub cu_price: u64,
    pub lane: Option<usize>,
    pub tip: u64,
    /// What this fresh sell cost in fees (lamports): a landed copy's base and priority fee (and its
    /// tip when it succeeded), or the cancel's when none landed.
    pub fees_lamports: u64,
}

/// What a cancel (an advance with a small priority fee) costs, about: the base fee and 600
/// lamports of priority.
pub const CANCEL_COST_LAMPORTS: u64 = 5_600;

pub fn fanout_on() -> bool {
    enabled() && super::nonces::sell_fanout() && super::nonces::is_live()
}

/// The sell's fee per CU for re-bid `rebid` (0: the first copies): the sell profile's, doubled per
/// re-bid, never over the cap (SELL_FEE_CAP_SOL, else BUY_FEE_CAP_SOL: the whole priority fee).
pub fn rebid_fee(base: u64, rebid: u32, cu_limit: u32, cap_lamports: u64) -> u64 {
    let cap_per_cu = (cap_lamports as u128 * 1_000_000 / cu_limit.max(1) as u128) as u64;
    base.max(1).saturating_mul(1 << rebid.min(20)).min(cap_per_cu.max(base))
}

fn sell_fee_cap_lamports() -> u64 {
    let sol = |k: &str| env_opt(k).and_then(|v| v.trim().parse::<f64>().ok());
    (sol("SELL_FEE_CAP_SOL").or_else(|| sol("BUY_FEE_CAP_SOL")).unwrap_or(0.005) * 1e9) as u64
}

/// One copy of the sell for `lane`: AdvanceNonceAccount on `account`, the compute budget at
/// `cu_price`, the lane's own tip (if it tips), then `instructions`, with the nonce `value` as its
/// blockhash; signed. Its lane request, signature and tip.
pub fn fanout_copy(
    mint: &Pubkey,
    instructions: &[Instruction],
    wallet: &Pubkey,
    lane: usize,
    tip: Option<(&'static [Pubkey], u64)>,
    pick: usize,
    nonce: (&Pubkey, &solana_hash::Hash),
    cu_price: u64,
    cu_limit: u32,
    sign: &mut dyn FnMut(&[u8]) -> [u8; 64],
) -> Result<(SendReq, Signature, u64)> {
    use crate::utils::utils::{set_compute_unit_limit, set_compute_unit_price};
    let mut ixs = Vec::with_capacity(instructions.len() + 4);
    ixs.push(crate::utils::nonce::advance_instruction(nonce.0, wallet));
    ixs.push(set_compute_unit_limit(cu_limit));
    ixs.push(set_compute_unit_price(cu_price));
    let tip_lamports = match tip {
        Some((list, lamports)) if !list.is_empty() && lamports > 0 => {
            ixs.push(transfer_sol(wallet, &list[pick % list.len()], lamports));
            lamports
        }
        _ => 0,
    };
    ixs.extend_from_slice(instructions);
    let message = solana_message::Message::new_with_blockhash(&ixs, Some(wallet), nonce.1);
    let msg = solana_message::VersionedMessage::Legacy(message).serialize();
    if 65 + msg.len() > PACKET {
        bail!("the sell copy is {} bytes, over a packet", 65 + msg.len());
    }
    let sig = sign(&msg);
    let mut req = SendReq { mint: mint.to_bytes(), len: (65 + msg.len()) as u16, tx: [0; PACKET], t_queued: now_ns(), lane: lane as u8 };
    req.tx[0] = 1;
    req.tx[1..65].copy_from_slice(&sig);
    req.tx[65..65 + msg.len()].copy_from_slice(&msg);
    Ok((req, Signature::from(sig), tip_lamports))
}

/// The first of `sigs` seen on chain within `wait` (processed or later): its signature and slot.
async fn seen_any(sigs: &[Signature], wait: std::time::Duration) -> Option<(Signature, u64)> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Ok(r) = crate::config::RPC_CLIENT.get_signature_statuses(sigs).await {
            if let Some((sig, st)) = sigs.iter().zip(r.value.iter()).find_map(|(s, v)| v.as_ref().map(|v| (*s, v.slot))) {
                return Some((sig, st));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Waits one slot from now (the stream's next slot, 150-600 ms).
async fn one_slot() {
    let start = crate::utils::latency::latest_slot().unwrap_or(0);
    let t = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let el = t.elapsed();
        if el >= std::time::Duration::from_millis(600) || (el >= std::time::Duration::from_millis(150) && crate::utils::latency::latest_slot().unwrap_or(0) > start) {
            return;
        }
    }
}

/// HOT_SELL_FANOUT, live: the sell as one copy per lane that is on, all on one durable nonce, each
/// paying only its lane's tip (at most one lands), re-bid every slot on the same nonce with the fee
/// doubled (at most `REBIDS` times, under the cap) until a copy lands; then the copies are cancelled
/// (the nonce manager's advance) and this waits until that settles, so no copy can land after the
/// caller's fresh sell. `None`: not taken (off, no free account, the queue full): the old path sends.
pub async fn fanout_sell(mint: &Pubkey, instructions: Vec<Instruction>) -> Result<Option<Fanout>> {
    use crate::utils::utils::ConfirmOutcome;
    if !fanout_on() {
        return Ok(None);
    }
    let Some(lane_tips) = LANE_TIPS.get() else { return Ok(None) };
    let lanes: Vec<(usize, Option<(&'static [Pubkey], u64)>)> = (0..super::stamps::LANES).filter_map(|l| lane_tips[l].map(|t| (l, t))).collect();
    if lanes.is_empty() {
        return Ok(None);
    }
    let copy = super::nonces::next_copy();
    let Some((index, account, value)) = super::nonces::acquire(copy) else { return Ok(None) };
    let wallet = trading_wallet();
    let profile = SendProfile::Sell;
    let cu_limit = profile.compute_unit_limit();
    let cap = sell_fee_cap_lamports();
    let pick = PICK.fetch_add(1, Ordering::Relaxed) as usize;
    let mut sent: Vec<(Signature, usize, u64, u64)> = Vec::new();
    let mut first: Option<Signature> = None;
    let mut cu_price = 0;
    let mut rebids = 0;
    for rebid in 0..=REBIDS {
        cu_price = rebid_fee(profile.priority_fee(), rebid, cu_limit, cap);
        let mut pushed = 0;
        for (lane, tip) in &lanes {
            let built = {
                let mut signer = SIGNER.lock().unwrap_or_else(|e| e.into_inner());
                let mut sign = |m: &[u8]| -> [u8; 64] {
                    match signer.as_mut() {
                        Some(p) => p.sign(m),
                        None => {
                            let mut sig = [0u8; 64];
                            sig.copy_from_slice(solana_signer::Signer::sign_message(&*crate::config::hot_signer(), m).as_ref());
                            sig
                        }
                    }
                };
                fanout_copy(mint, &instructions, &wallet, *lane, *tip, pick, (&account, &value), cu_price, cu_limit, &mut sign)
            };
            let (req, sig, tip_lamports) = match built {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("hot sell fan-out {mint}: {e:#}");
                    continue;
                }
            };
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            let Some(q) = q.as_mut() else { break };
            if q.prod.push(req) {
                q.thread.unpark();
                pushed += 1;
                first.get_or_insert(sig);
                sent.push((sig, *lane, tip_lamports, cu_price));
            }
        }
        if pushed == 0 {
            if rebid == 0 {
                super::nonces::unsent(index, copy);
                return Ok(None);
            }
            break;
        }
        if rebid == 0 {
            super::nonces::sent(index, copy);
        } else {
            super::nonces::resent(index, copy);
            rebids = rebid;
        }
        one_slot().await;
        let sigs: Vec<Signature> = sent.iter().map(|s| s.0).collect();
        if let Some((sig, slot)) = seen_any(&sigs, std::time::Duration::from_millis(1)).await {
            super::nonces::landed(index, copy, slot);
            return Ok(Some(finish(sig, &sent, rebids).await));
        }
    }
    // Cancelled (the manager advances the account 2 s after the last re-bid): wait until it settles,
    // still watching for a copy that landed meanwhile.
    let sigs: Vec<Signature> = sent.iter().map(|s| s.0).collect();
    let deadline = tokio::time::Instant::now() + SETTLE_WAIT;
    while tokio::time::Instant::now() < deadline {
        if let Some((sig, slot)) = seen_any(&sigs, std::time::Duration::from_millis(1)).await {
            super::nonces::landed(index, copy, slot);
            return Ok(Some(finish(sig, &sent, rebids).await));
        }
        if super::nonces::settled(index, copy) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    if !super::nonces::settled(index, copy) {
        log::warn!("hot sell fan-out {mint}: the nonce account's cancel has not settled in {} s", SETTLE_WAIT.as_secs());
    }
    Ok(Some(Fanout { outcome: ConfirmOutcome::TimedOut, fill: None, signature: first.unwrap_or_default(), rebids, cu_price, lane: None, tip: 0, fees_lamports: CANCEL_COST_LAMPORTS }))
}

/// The landed copy's outcome (confirmed or failed), fill, lane and tip.
async fn finish(sig: Signature, sent: &[(Signature, usize, u64, u64)], rebids: u32) -> Fanout {
    let (outcome, fill) = crate::processors::common::await_landing(&sig, std::time::Duration::from_secs(45))
        .await
        .unwrap_or((crate::utils::utils::ConfirmOutcome::TimedOut, None));
    let (lane, tip, cu_price) = sent.iter().find(|s| s.0 == sig).map_or((None, 0, 0), |s| (Some(s.1), s.2, s.3));
    let priority = (cu_price as u128 * SendProfile::Sell.compute_unit_limit() as u128 / 1_000_000) as u64;
    // A failed transaction reverts its tip transfer: only the fees are paid.
    let paid_tip = if matches!(outcome, crate::utils::utils::ConfirmOutcome::Confirmed { .. }) { tip } else { 0 };
    Fanout { outcome, fill, signature: sig, rebids, cu_price, lane, tip, fees_lamports: 5_000 + priority + paid_tip }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_keypair::Keypair;
    use solana_signer::Signer;

    /// A queued sell is one legacy transaction with the payer's one valid signature, the sell
    /// profile's compute budget, one tip transfer per tipping lane (accounts rotated) and the sell
    /// instructions after them; its signature comes back; without the lanes installed, None.
    #[test]
    fn a_sell_is_signed_once_with_every_lanes_tip_and_queued_for_the_lanes() {
        let _state = super::super::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        super::super::clock::init();
        super::super::testkit::seed(crate::utils::dashboard::now_unix());
        let payer = Keypair::new();
        let wallet = payer.pubkey();
        let mint = Pubkey::new_unique();
        let sell = || vec![transfer_sol(&wallet, &mint, 7)];
        let tip_a: &'static [Pubkey] = Box::leak(vec![Pubkey::new_unique(), Pubkey::new_unique()].into_boxed_slice());
        let tip_b: &'static [Pubkey] = Box::leak(vec![Pubkey::new_unique()].into_boxed_slice());
        // Nothing installed: the slow path's turn.
        *QUEUE.lock().unwrap() = None;
        assert!(send_with(&mint, sell(), &wallet, false, |m| sig_of(&payer, m)).unwrap().is_none());
        let (prod, mut cons) = spsc::channel::<SendReq>(RING);
        install(prod, std::thread::current(), vec![(tip_a, 1_000), (tip_b, 2_000), (&[], 5)], [None; super::super::stamps::LANES], None);
        let before = PICK.load(Ordering::Relaxed);
        let got = send_with(&mint, sell(), &wallet, false, |m| sig_of(&payer, m)).unwrap().expect("queued");
        let req = cons.pop().expect("one request");
        assert!(cons.pop().is_none());
        assert_eq!((req.mint, req.t_queued > 0), (mint.to_bytes(), true));
        let bytes = &req.tx[..req.len as usize];
        let tx: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(bytes).unwrap();
        assert_eq!(tx.signatures.len(), 1);
        assert_eq!(tx.signatures[0], got);
        let msg = tx.message;
        assert!(got.verify(&wallet.to_bytes(), &msg.serialize()), "the signature verifies");
        let keys = msg.static_account_keys();
        let ixs: Vec<_> = msg.instructions().iter().map(|ix| (keys[ix.program_id_index as usize], ix.data.clone(), ix.accounts.clone())).collect();
        let system = crate::utils::utils::SYSTEM_PROGRAM_ID;
        let transfers: Vec<(Pubkey, u64)> = ixs
            .iter()
            .filter(|(p, d, _)| *p == system && d.len() == 12 && d[..4] == 2u32.to_le_bytes())
            .map(|(_, d, a)| (keys[a[1] as usize], u64::from_le_bytes(d[4..].try_into().unwrap())))
            .collect();
        let pick = before as usize;
        assert_eq!(transfers, vec![(tip_a[pick % 2], 1_000), (tip_b[0], 2_000), (mint, 7)], "two lane tips, then the sell");
        assert_eq!(keys[0], wallet, "the payer signs");
        // The next sell rotates the tip account.
        let _ = send_with(&mint, sell(), &wallet, false, |m| sig_of(&payer, m)).unwrap().expect("queued");
        let req2 = cons.pop().unwrap();
        let tx2: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(&req2.tx[..req2.len as usize]).unwrap();
        let keys2 = tx2.message.static_account_keys().to_vec();
        assert!(keys2.contains(&tip_a[(pick + 1) % 2]));
        // A paper sell's copy (`send_dry`): the same sell, its blockhash all zero (it cannot land).
        let _ = send_with(&mint, sell(), &wallet, true, |m| sig_of(&payer, m)).unwrap().expect("queued");
        let req3 = cons.pop().unwrap();
        let tx3: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(&req3.tx[..req3.len as usize]).unwrap();
        assert_eq!(*tx3.message.recent_blockhash(), solana_hash::Hash::default(), "unlandable");
        assert_ne!(*tx2.message.recent_blockhash(), solana_hash::Hash::default(), "a live sell has the blockhash");
        assert_eq!(tx3.message.instructions().len(), tx2.message.instructions().len());
        assert!(tx3.signatures[0].verify(&wallet.to_bytes(), &tx3.message.serialize()));
        // A full queue: None (the slow path sends), nothing lost.
        for _ in 0..RING {
            let _ = send_with(&mint, sell(), &wallet, false, |m| sig_of(&payer, m)).unwrap();
        }
        assert!(send_with(&mint, sell(), &wallet, false, |m| sig_of(&payer, m)).unwrap().is_none());
        *QUEUE.lock().unwrap() = None;
    }

    /// The decision thread's side: a queued sell goes to the lanes it holds (here lanes that never
    /// connect: nothing leaves the machine) and is stamped as a sell; nothing allocates on the way.
    #[test]
    fn the_decision_thread_writes_a_queued_sell_on_its_lanes_without_allocating() {
        use super::super::{alloc_audit, engine, lanes, stamps};
        let _state = super::super::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        super::super::clock::init();
        stamps::init();
        super::super::testkit::seed(crate::utils::dashboard::now_unix());
        let keypair = std::sync::Arc::new(Keypair::new());
        let cfg = lanes::test_config(&[(lanes::LANE_ZEROSLOT, 1_000_000), (lanes::LANE_HELIUS, 5_000)]);
        let mut d = engine::bench_decider(keypair, true, stamps::Mode::Spsc)
            .unwrap()
            .with_lanes(engine::SendMode::Live, Some(lanes::Lanes::start(cfg)), None, 0);
        let mint = Pubkey::new_unique();
        let mut req = SendReq { mint: mint.to_bytes(), len: 300, tx: [0x5a; PACKET], t_queued: now_ns(), lane: ALL_LANES };
        req.tx[0] = 1;
        let mut head = 0u64;
        let mut drained = Vec::new();
        stamps::drain(&mut head, &mut drained);
        drained.clear();
        d.on_send(&req);
        let ((), n) = alloc_audit::allocations(|| d.on_send(&req));
        assert_eq!(n, 0, "{n} heap allocations writing a sell on the lanes");
        stamps::drain(&mut head, &mut drained);
        let sells: Vec<&stamps::Stamp> = drained.iter().filter(|s| s.feed == stamps::Feed::Sell).collect();
        assert_eq!(sells.len(), 2);
        let s = sells[0];
        assert_eq!((s.mint, s.flags, s.t_recv), (mint.to_bytes(), stamps::FLAG_SELL, req.t_queued));
        assert!(s.t_picked >= s.t_recv && s.t_ready >= s.t_picked, "{s:?}");
        // A fan-out copy: written on its own lane only.
        let mut one = req;
        one.lane = lanes::LANE_ZEROSLOT as u8;
        drained.clear();
        d.on_send(&one);
        stamps::drain(&mut head, &mut drained);
        let s = drained.iter().find(|s| s.feed == stamps::Feed::Sell).expect("stamped");
        assert!((0..stamps::LANES).all(|l| l == lanes::LANE_ZEROSLOT || s.t_sent[l] == 0), "only its lane: {:?}", s.t_sent);
        // Without lanes (DRY_RUN) nothing is written and nothing stamped.
        let mut off = engine::bench_decider(std::sync::Arc::new(Keypair::new()), true, stamps::Mode::Spsc).unwrap();
        drained.clear();
        off.on_send(&req);
        stamps::drain(&mut head, &mut drained);
        assert!(drained.iter().all(|s| s.feed != stamps::Feed::Sell));
    }

    /// HOT_SELL_FANOUT: a lane's copy is AdvanceNonceAccount on the account, the compute budget at
    /// the re-bid's fee, only that lane's tip, then the sell, on the nonce value; signed.
    #[test]
    fn a_sell_copy_carries_the_nonce_its_fee_and_only_its_lanes_tip() {
        let payer = Keypair::new();
        let wallet = payer.pubkey();
        let mint = Pubkey::new_unique();
        let sell = vec![transfer_sol(&wallet, &mint, 7)];
        let tips: &'static [Pubkey] = Box::leak(vec![Pubkey::new_unique(), Pubkey::new_unique()].into_boxed_slice());
        let (account, value) = (Pubkey::new_unique(), solana_hash::Hash::new_from_array([9; 32]));
        let mut sign = |m: &[u8]| sig_of(&payer, m);
        let (req, sig, tip) = fanout_copy(&mint, &sell, &wallet, 6, Some((tips, 300_000)), 1, (&account, &value), 40_000, 90_000, &mut sign).unwrap();
        assert_eq!((req.lane, tip), (6, 300_000));
        let tx: solana_transaction::versioned::VersionedTransaction = bincode::deserialize(&req.tx[..req.len as usize]).unwrap();
        assert_eq!(tx.signatures[0], sig);
        assert!(sig.verify(&wallet.to_bytes(), &tx.message.serialize()));
        assert_eq!(*tx.message.recent_blockhash(), value);
        let keys = tx.message.static_account_keys();
        let ixs = tx.message.instructions();
        assert_eq!((keys[ixs[0].program_id_index as usize], ixs[0].data.clone()), (crate::utils::nonce::SYSTEM_PROGRAM, vec![4, 0, 0, 0]));
        assert_eq!(keys[ixs[0].accounts[0] as usize], account);
        let budget = crate::utils::utils::set_compute_unit_price(40_000);
        assert!(ixs.iter().any(|ix| keys[ix.program_id_index as usize] == budget.program_id && ix.data == budget.data), "the fee");
        let system = crate::utils::utils::SYSTEM_PROGRAM_ID;
        let transfers: Vec<(Pubkey, u64)> = ixs
            .iter()
            .filter(|ix| keys[ix.program_id_index as usize] == system && ix.data.len() == 12 && ix.data[..4] == 2u32.to_le_bytes())
            .map(|ix| (keys[ix.accounts[1] as usize], u64::from_le_bytes(ix.data[4..].try_into().unwrap())))
            .collect();
        assert_eq!(transfers, vec![(tips[1], 300_000), (mint, 7)], "its own tip, then the sell");
        // A lane that tips nothing: no tip transfer.
        let (req, _, tip) = fanout_copy(&mint, &sell, &wallet, 4, None, 0, (&account, &value), 40_000, 90_000, &mut sign).unwrap();
        assert_eq!((req.lane, tip), (4, 0));
    }

    /// A re-bid doubles the fee per CU, never past the cap on the whole priority fee.
    #[test]
    fn rebids_double_the_fee_under_the_cap() {
        // Cap 0.005 SOL over 100,000 CU: 50,000,000 micro-lamports per CU.
        let fees: Vec<u64> = (0..=REBIDS).map(|r| rebid_fee(10_000_000, r, 100_000, 5_000_000)).collect();
        assert_eq!(fees, vec![10_000_000, 20_000_000, 40_000_000, 50_000_000, 50_000_000]);
        assert_eq!(rebid_fee(100_000_000, 2, 100_000, 5_000_000), 100_000_000, "a base over the cap is not lowered");
    }

    fn sig_of(k: &Keypair, m: &[u8]) -> [u8; 64] {
        let mut s = [0u8; 64];
        s.copy_from_slice(k.sign_message(m).as_ref());
        s
    }
}
