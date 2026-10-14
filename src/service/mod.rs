pub mod helius_sender;
pub mod jito;
pub mod nozomi;
pub mod utils;
pub mod zero_slot;

use {
    crate::{
        config::{lamports_to_sol, trading_wallet, ConfirmService, PUBKEY, RPC_CLIENT, TRADE_SETTING},
        utils::{
            build_and_sign::{build_and_sign_nonced, build_and_sign_with_fee, build_and_sign_with_limit, build_unsigned_with_limit, serialize_base64},
            latency::ReadyMark,
            utils::set_loaded_accounts_data_size_limit,
        },
    },
    anyhow::{bail, Result},
    futures::stream::{FuturesUnordered, StreamExt},
    jito::{init_jito, jito_tip_instruction, send_jito, JITO_MIN_TIP_LAMPORTS},
    log::{debug, warn},
    nozomi::{init_nozomi, nozomi_tip_instruction, send_nozomi, send_nozomi_bundle, send_nozomi_fast, NOZOMI_MIN_TIP_LAMPORTS},
    solana_commitment_config::CommitmentConfig,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
    solana_rpc_client_api::config::{RpcSendTransactionConfig, RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig},
    solana_account_decoder_client_types::UiAccountEncoding,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    std::time::Duration,
    zero_slot::{
        init_zero_slot, send_zero_slot, zero_slot_tip_instruction,
        ZERO_SLOT_ADVANCED_MIN_TIP_LAMPORTS, ZERO_SLOT_MIN_TIP_LAMPORTS,
    },
};


const MIN_CONTEXT_SLOT_RETRIES: u32 = 20;


const RESEND_INTERVAL: Duration = Duration::from_millis(100);



const RESEND_WINDOW: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone)]
pub enum Submitted {
    Sent {
        signature: Signature,
        /// The other copies of a fanned-out buy (NONCE_FANOUT): at most one of all of them lands.
        alternates: Vec<Signature>,
        
        ready: ReadyMark,
    },
    
    Simulated {
        signature: Signature,
        units_consumed: Option<u64>,
        error: Option<String>,
        logs: Vec<String>,
        
        ready: ReadyMark,
        
        
        
        watched_tokens: Option<u64>,
    },
}



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendProfile {
    Buy,
    Sell,
}

impl SendProfile {
    pub fn services(self) -> &'static [ConfirmService] {
        match self {
            // A bundle buy goes through Nozomi (and Jito with JITO_COPY), never RPC: a copy sent
            // over RPC could land and fail.
            SendProfile::Buy if TRADE_SETTING.nonce_fanout => &TRADE_SETTING.fanout_services,
            SendProfile::Buy if TRADE_SETTING.nozomi_bundle && TRADE_SETTING.jito_copy => &[ConfirmService::Nozomi, ConfirmService::Jito],
            SendProfile::Buy if TRADE_SETTING.nozomi_bundle => &[ConfirmService::Nozomi],
            SendProfile::Buy => &TRADE_SETTING.confirm_services,
            SendProfile::Sell => &TRADE_SETTING.sell_confirm_services,
        }
    }

    /// The compute limit each profile's transactions ask for (sells their own, tighter one).
    pub fn compute_unit_limit(self) -> u32 {
        match self {
            SendProfile::Buy => TRADE_SETTING.compute_unit_limit,
            SendProfile::Sell => TRADE_SETTING.sell_compute_unit_limit,
        }
    }

    pub fn priority_fee(self) -> u64 {
        match self {
            SendProfile::Buy => TRADE_SETTING.priority_fee_micro_lamports,
            SendProfile::Sell => TRADE_SETTING.sell_priority_fee_micro_lamports,
        }
    }
}

/// What one landed transaction of `profile` costs in fees and tips, in lamports: base fee,
/// priority fee over the full CU limit, and every service's tip. DRY_RUN charges it to

pub fn modeled_cost_lamports(profile: SendProfile) -> u64 {
    // A fanned-out buy lands one copy, which pays its own service's tip only: the dearest one.
    if matches!(profile, SendProfile::Buy) && TRADE_SETTING.nonce_fanout {
        let tip = TRADE_SETTING.fanout_services.iter().map(|s| fanout_tip(*s)).max().unwrap_or(0);
        return landed_fee_lamports(profile) + tip;
    }
    let tips: u64 = tip_amounts(profile).iter().map(|(_, lamports)| lamports).sum();
    landed_fee_lamports(profile) + tips
}

/// The fee a landed transaction pays: the base fee and the priority fee on the whole compute
/// limit (a landed bundle pays it too).
fn landed_fee_lamports(profile: SendProfile) -> u64 {
    5_000 + (profile.priority_fee() as u128 * TRADE_SETTING.compute_unit_limit as u128 / 1_000_000) as u64
}

/// Buys go out only as bundles (Nozomi's, or Jito's with CONFIRM_SERVICES=JITO): one that would
/// fail is dropped, never charged.
pub fn buys_are_bundles() -> bool {
    !TRADE_SETTING.nonce_fanout && TRADE_SETTING.nozomi_bundle || (TRADE_SETTING.jito_bundle_only && SendProfile::Buy.services() == [ConfirmService::Jito])
}

/// A buy that no route delivered in time cost nothing: a dropped bundle, or a fanned-out buy none of
/// whose copies landed (the copies that did not land never ran).
pub fn buy_timeout_is_free() -> bool {
    buys_are_bundles() || TRADE_SETTING.nonce_fanout
}

/// What a transaction that fails on chain costs: the fee and the priority fee on the whole
/// compute limit. Tips are instructions in the transaction, so they revert with it. A Jito
/// bundle-only buy that would fail is never included: nothing. A Nozomi bundle buy also goes as
/// a plain send, whose failing copy pays the base fee plus any priority fee.
pub fn modeled_failed_cost_lamports(profile: SendProfile) -> u64 {
    if matches!(profile, SendProfile::Buy) && buys_are_bundles() && !TRADE_SETTING.nozomi_bundle {
        return 0;
    }
    landed_fee_lamports(profile)
}


pub async fn init() -> Result<()> {
    let mut services = TRADE_SETTING.confirm_services.clone();
    for service in &TRADE_SETTING.sell_confirm_services {
        if !services.contains(service) {
            services.push(*service);
        }
    }
    if TRADE_SETTING.nozomi_bundle && !services.contains(&ConfirmService::Nozomi) {
        services.push(ConfirmService::Nozomi);
    }
    if TRADE_SETTING.nonce_fanout {
        for service in &TRADE_SETTING.fanout_services {
            if !services.contains(service) {
                services.push(*service);
            }
        }
        // Paper buys are simulated, never sent: they need no nonce account.
        if !TRADE_SETTING.dry_run {
            crate::utils::nonce::init().await?;
        }
    }
    if TRADE_SETTING.jito_copy {
        if !TRADE_SETTING.nozomi_bundle || TRADE_SETTING.jito_uuid.is_none() || !TRADE_SETTING.jito_bundle_only {
            bail!("JITO_COPY needs NOZOMI_BUNDLE=true, JITO_BUNDLE_ONLY=true and JITO_UUID");
        }
        if !services.contains(&ConfirmService::Jito) {
            services.push(ConfirmService::Jito);
        }
    }
    if services.contains(&ConfirmService::Nozomi) && TRADE_SETTING.buy_third_party_fee_lamports < NOZOMI_MIN_TIP_LAMPORTS {
        bail!("BUY_THIRD_PARTY_FEE must be at least {} SOL for Nozomi", lamports_to_sol(NOZOMI_MIN_TIP_LAMPORTS));
    }
    for service in services {
        init_one(service).await?;
    }
    Ok(())
}

async fn init_one(service: ConfirmService) -> Result<()> {
    let s = &*TRADE_SETTING;
    match service {
        ConfirmService::Rpc => {}
        ConfirmService::Jito => {
            if s.jito_tip_lamports < JITO_MIN_TIP_LAMPORTS {
                bail!(
                    "CONFIRM_SERVICE=JITO needs JITO_TIP_VALUE (or THIRD_PARTY_FEE) of at least {} SOL",
                    lamports_to_sol(JITO_MIN_TIP_LAMPORTS)
                );
            }
            init_jito().await?;
        }
        ConfirmService::Nozomi => {
            if s.third_party_fee_lamports < NOZOMI_MIN_TIP_LAMPORTS {
                bail!(
                    "CONFIRM_SERVICE=NOZOMI needs THIRD_PARTY_FEE of at least {} SOL \
                     (Nozomi silently drops transactions with smaller tips)",
                    lamports_to_sol(NOZOMI_MIN_TIP_LAMPORTS)
                );
            }
            init_nozomi().await?;
        }
        ConfirmService::ZeroSlot => {
            if s.third_party_fee_lamports < ZERO_SLOT_ADVANCED_MIN_TIP_LAMPORTS {
                bail!(
                    "CONFIRM_SERVICE=ZERO_SLOT needs THIRD_PARTY_FEE of at least {} SOL",
                    lamports_to_sol(ZERO_SLOT_ADVANCED_MIN_TIP_LAMPORTS)
                );
            }
            if s.third_party_fee_lamports < ZERO_SLOT_MIN_TIP_LAMPORTS {
                warn!(
                    "THIRD_PARTY_FEE is below 0slot's {} SOL minimum; only the Advanced plan accepts it",
                    lamports_to_sol(ZERO_SLOT_MIN_TIP_LAMPORTS)
                );
            }
            init_zero_slot().await?;
        }
        ConfirmService::Helius => {
            if s.helius_sender_tip_lamports < helius_sender::HELIUS_SENDER_MIN_TIP_LAMPORTS {
                bail!("CONFIRM_SERVICE=HELIUS needs HELIUS_SENDER_TIP_SOL of at least 0.000005");
            }
            helius_sender::init_helius_sender().await?;
        }
    }
    Ok(())
}



pub fn tip_instructions(payer: &Pubkey) -> Vec<Instruction> {
    tip_instructions_for(SendProfile::Buy, payer)
}


pub fn tip_instructions_for(profile: SendProfile, payer: &Pubkey) -> Vec<Instruction> {
    tip_amounts(profile).into_iter().filter_map(|(service, lamports)| tip_instruction_with(service, payer, lamports)).collect()
}

/// Each service's tip on one transaction of `profile` (RPC gets none). Sending and the paper
/// cost model both use it.
pub fn tip_amounts(profile: SendProfile) -> Vec<(ConfirmService, u64)> {
    profile.services().iter().copied().filter(|s| *s != ConfirmService::Rpc).map(|s| (s, fixed_tip(profile, s))).collect()
}

/// The configured fixed tip of `service` for `profile`.
pub(crate) fn fixed_tip(profile: SendProfile, service: ConfirmService) -> u64 {
    let s = &*TRADE_SETTING;
    match service {
        ConfirmService::Rpc => 0,
        ConfirmService::Jito => s.jito_tip_lamports,
        ConfirmService::Nozomi | ConfirmService::ZeroSlot => third_party_tip(profile),
        ConfirmService::Helius => s.helius_sender_tip_lamports,
    }
}

/// `service`'s tip instruction for `lamports` (None for RPC).
pub fn tip_instruction_for_amount(service: ConfirmService, payer: &Pubkey, lamports: u64) -> Option<Instruction> {
    tip_instruction_with(service, payer, lamports)
}

fn tip_instruction_with(service: ConfirmService, payer: &Pubkey, lamports: u64) -> Option<Instruction> {
    match service {
        ConfirmService::Rpc => None,
        ConfirmService::Jito => Some(jito_tip_instruction(payer, lamports)),
        ConfirmService::Nozomi => Some(nozomi_tip_instruction(payer, lamports)),
        ConfirmService::ZeroSlot => Some(zero_slot_tip_instruction(payer, lamports)),
        ConfirmService::Helius => Some(helius_sender::helius_sender_tip_instruction(payer, lamports)),
    }
}

/// Nozomi / 0slot tip for `profile`: buys may bid more (BUY_THIRD_PARTY_FEE).
fn third_party_tip(profile: SendProfile) -> u64 {
    match profile {
        SendProfile::Buy => TRADE_SETTING.buy_third_party_fee_lamports,
        SendProfile::Sell => TRADE_SETTING.third_party_fee_lamports,
    }
}



pub fn tip_instruction(service: ConfirmService, payer: &Pubkey) -> Option<Instruction> {
    let s = &*TRADE_SETTING;
    match service {
        ConfirmService::Rpc => None,
        ConfirmService::Jito => Some(jito_tip_instruction(payer, s.jito_tip_lamports)),
        ConfirmService::Nozomi => Some(nozomi_tip_instruction(payer, s.third_party_fee_lamports)),
        ConfirmService::ZeroSlot => {
            Some(zero_slot_tip_instruction(payer, s.third_party_fee_lamports))
        }
        ConfirmService::Helius => Some(helius_sender::helius_sender_tip_instruction(payer, s.helius_sender_tip_lamports)),
    }
}







pub async fn submit(instructions: Vec<Instruction>, min_context_slot: Option<u64>) -> Result<Submitted> {
    submit_with(SendProfile::Buy, instructions, min_context_slot).await
}


pub async fn submit_with(profile: SendProfile, instructions: Vec<Instruction>, min_context_slot: Option<u64>) -> Result<Submitted> {
    submit_watching(profile, instructions, min_context_slot, None).await
}


/// Buys state the account data they load (LOADED_DATA_LIMIT_MIB), which ranks them higher at the
/// same fee; paper mode simulates with it, so a limit that is too small shows up there first.
fn with_loaded_data_limit(profile: SendProfile, mut instructions: Vec<Instruction>) -> Vec<Instruction> {
    if let (SendProfile::Buy, Some(bytes)) = (profile, TRADE_SETTING.loaded_data_limit_bytes) {
        instructions.insert(0, set_loaded_accounts_data_size_limit(bytes));
    }
    instructions
}

pub async fn submit_watching(
    profile: SendProfile,
    instructions: Vec<Instruction>,
    min_context_slot: Option<u64>,
    watch: Option<Pubkey>,
) -> Result<Submitted> {
    let setting = &*TRADE_SETTING;
    let instructions = with_loaded_data_limit(profile, instructions);
    if setting.dry_run {
        let wallet = trading_wallet();
        let tips = match (profile, setting.fanout_services.first()) {
            (SendProfile::Buy, Some(first)) if setting.nonce_fanout => tip_instruction_with(*first, &wallet, fanout_tip(*first)).into_iter().collect(),
            _ => tip_instructions_for(profile, &wallet),
        };
        let tx = build_unsigned_with_limit(instructions, tips, &wallet, profile.priority_fee(), profile.compute_unit_limit())?;
        return simulate_watching(&tx, min_context_slot, watch).await;
    }
    if matches!(profile, SendProfile::Buy) && setting.nonce_fanout {
        match crate::utils::nonce::lease() {
            Some(lease) => return send_fanout(instructions, lease).await,
            None => warn!("nonce fan-out: every nonce account is in use; this buy goes out as one copy through {:?}", setting.fanout_services.first()),
        }
        let Some(first) = setting.fanout_services.first().copied() else { bail!("FANOUT_SERVICES is empty") };
        let tips = tip_instruction_with(first, &PUBKEY, fanout_tip(first)).into_iter().collect();
        let tx = build_and_sign_with_fee(instructions, tips, profile.priority_fee())?;
        let ready = ReadyMark::now();
        send_copy(first, &tx).await?;
        return Ok(Submitted::Sent { signature: tx.signatures[0], alternates: Vec::new(), ready });
    }

    let tx = build_and_sign_with_limit(instructions, tip_instructions_for(profile, &PUBKEY), profile.priority_fee(), profile.compute_unit_limit())?;
    let ready = ReadyMark::now();
    send_via(profile, &tx).await?;
    
    
    
    spawn_resend(profile, tx.clone());
    Ok(Submitted::Sent { signature: tx.signatures[0], alternates: Vec::new(), ready })
}



/// A service's tip on its copy of a fanned-out buy (the copy carries no other tip).
pub fn fanout_tip(service: ConfirmService) -> u64 {
    fixed_tip(SendProfile::Buy, service)
}

/// The service of each recent fanned-out copy, to name the one that landed (`route_of`).
static ROUTES: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<(Signature, ConfirmService)>>> =
    std::sync::LazyLock::new(Default::default);

fn note_routes(copies: &[(ConfirmService, VersionedTransaction)]) {
    let mut routes = ROUTES.lock().unwrap_or_else(|e| e.into_inner());
    for (service, tx) in copies {
        routes.push_back((tx.signatures[0], *service));
    }
    while routes.len() > 64 {
        routes.pop_front();
    }
}

/// The service that carried a fanned-out copy (None: not a fanned-out copy, or long gone).
pub fn route_of(signature: &Signature) -> Option<ConfirmService> {
    ROUTES.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| s == signature).map(|(_, service)| *service)
}

/// A buy as one transaction per service of FANOUT_SERVICES, each tipping only its service and all
/// advancing `lease`'s nonce: sent at once, the first to land is the buy, the rest are void.
async fn send_fanout(instructions: Vec<Instruction>, lease: crate::utils::nonce::Lease) -> Result<Submitted> {
    let copies = TRADE_SETTING
        .fanout_services
        .iter()
        .map(|&service| {
            let tips = tip_instruction_with(service, &PUBKEY, fanout_tip(service)).into_iter().collect();
            build_and_sign_nonced(instructions.clone(), tips, SendProfile::Buy.priority_fee(), &lease).map(|tx| (service, tx))
        })
        .collect::<Result<Vec<_>>>();
    let copies = match copies {
        Ok(copies) => copies,
        Err(e) => {
            crate::utils::nonce::release(lease);
            return Err(e);
        }
    };
    note_routes(&copies);
    let ready = ReadyMark::now();
    let results = futures::future::join_all(copies.iter().map(|(service, tx)| send_copy(*service, tx))).await;
    // Whatever happened, the nonce account is read again: a copy may land even after an error.
    crate::utils::nonce::release(lease);
    let mut accepted = 0;
    for ((service, tx), result) in copies.iter().zip(&results) {
        match result {
            Ok(()) => accepted += 1,
            Err(e) => warn!("{service:?} refused its copy {}: {e:#}", tx.signatures[0]),
        }
    }
    if accepted == 0 {
        bail!("every service refused its copy of the buy");
    }
    let signatures: Vec<Signature> = copies.iter().map(|(_, tx)| tx.signatures[0]).collect();
    spawn_fanout_resend(copies);
    Ok(Submitted::Sent { signature: signatures[0], alternates: signatures[1..].to_vec(), ready })
}

/// One service's copy: Nozomi's both as a bundle and a plain send with NOZOMI_BUNDLE (as buys went
/// before the fan-out), every other service its own way.
async fn send_copy(service: ConfirmService, tx: &VersionedTransaction) -> Result<()> {
    let encoded = serialize_base64(tx)?;
    match service {
        ConfirmService::Nozomi if TRADE_SETTING.nozomi_bundle => {
            let members = [tx];
            let (bundle, plain) = tokio::join!(send_nozomi_bundle(&members), send_nozomi_fast(&encoded));
            match (bundle, plain) {
                (Err(b), Err(p)) => bail!("bundle: {b:#}; plain: {p:#}"),
                _ => Ok(()),
            }
        }
        ConfirmService::Nozomi => send_nozomi(&encoded).await.map(|_| ()),
        ConfirmService::Jito => send_jito(&encoded).await.map(|_| ()),
        ConfirmService::ZeroSlot => send_zero_slot(&encoded).await.map(|_| ()),
        ConfirmService::Helius => helius_sender::send_helius_sender(&encoded).await.map(|_| ()),
        ConfirmService::Rpc => send_rpc(tx).await.map(|_| ()),
    }
}

/// Re-sends every non-Nozomi copy every RESEND_INTERVAL for RESEND_WINDOW, until one of the copies
/// shows up on chain (Nozomi retries by itself and asks clients not to).
fn spawn_fanout_resend(copies: Vec<(ConfirmService, VersionedTransaction)>) {
    let landed = copies.iter().map(|(_, tx)| tx.signatures[0]).collect::<Vec<_>>();
    let copies: Vec<_> = copies.into_iter().filter(|(s, _)| *s != ConfirmService::Nozomi).collect();
    if copies.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + RESEND_WINDOW;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(RESEND_INTERVAL).await;
            if landed.iter().any(crate::utils::latency::own_tx_seen) {
                return;
            }
            futures::future::join_all(copies.iter().map(|(service, tx)| send_copy(*service, tx))).await;
        }
    });
}

fn spawn_resend(profile: SendProfile, tx: VersionedTransaction) {
    // Nozomi retries on its own, and its docs say client-side resubmission "wastes your rate limit
    // and lowers your priority" (it also drew HTTP 429s live on 26 Sep 2026); a re-sent bundle is
    // dropped as a duplicate. Only the other routes are re-sent.
    if matches!(profile, SendProfile::Buy) && TRADE_SETTING.nozomi_bundle {
        return;
    }
    let services: Vec<ConfirmService> = profile.services().iter().copied().filter(|s| *s != ConfirmService::Nozomi).collect();
    if services.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let signature = tx.signatures[0];
        let deadline = tokio::time::Instant::now() + RESEND_WINDOW;
        let mut sent = 1;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(RESEND_INTERVAL).await;
            if crate::utils::latency::own_tx_seen(&signature) {
                debug!("{signature} landed after {sent} send(s); stopping re-sends");
                return;
            }
            if send_through(&services, &tx, false).await.is_err() {
                return;
            }
            sent += 1;
        }
        debug!("{signature} not seen in the stream after {sent} send(s)");
    });
}






pub async fn send(tx: &VersionedTransaction) -> Result<()> {
    send_via(SendProfile::Buy, tx).await
}


pub async fn send_via(profile: SendProfile, tx: &VersionedTransaction) -> Result<()> {
    // Bundle buys go both ways at once: as a Nozomi bundle (block builders, a failing buy is
    // dropped free) and as a plain Nozomi send (direct-to-leader paths, faster into the launch
    // slot; with PRIORITY_FEE_MICRO_LAMPORT=0 a failing copy costs the 5,000-lamport base fee).
    // One signed transaction, so it lands at most once. Never RPC. Paired probes on 26 Sep 2026:
    // the two paths landed within 3 positions of each other, and a bundle once slipped two slots.
    if matches!(profile, SendProfile::Buy) && TRADE_SETTING.nozomi_bundle {
        let encoded = serialize_base64(tx)?;
        let members = [tx];
        let jito = async {
            if TRADE_SETTING.jito_copy { send_jito(&encoded).await.map(|_| ()) } else { Ok(()) }
        };
        let (bundle, plain, jito) = tokio::join!(send_nozomi_bundle(&members), send_nozomi_fast(&encoded), jito);
        if let Err(e) = &plain {
            warn!("nozomi: plain copy of {} not accepted: {e:#}", tx.signatures[0]);
        }
        if let Err(e) = &jito {
            warn!("jito: bundle copy of {} not accepted: {e:#}", tx.signatures[0]);
        }
        return match (bundle, plain, jito) {
            (Err(b), Err(p), Err(j)) => bail!("every copy refused: nozomi bundle: {b:#}; nozomi plain: {p:#}; jito: {j:#}"),
            (Err(b), Err(p), Ok(())) if !TRADE_SETTING.jito_copy => bail!("nozomi refused both copies: bundle: {b:#}; plain: {p:#}"),
            _ => Ok(()),
        };
    }
    send_through(profile.services(), tx, true).await
}

/// Sends `tx` through every one of `services` at once; with `fallback`, through RPC when all of
/// them refuse it.
async fn send_through(services: &[ConfirmService], tx: &VersionedTransaction, fallback: bool) -> Result<()> {
    let encoded = serialize_base64(tx)?;
    let sends = services
        .iter()
        .map(|service| {
            let encoded = encoded.clone();
            let tx = tx.clone();
            let service = *service;
            async move {
                let result = match service {
                    ConfirmService::Rpc => send_rpc(&tx).await.map(|_| ()),
                    ConfirmService::Jito => send_jito(&encoded).await.map(|_| ()),
                    ConfirmService::Nozomi => send_nozomi(&encoded).await.map(|_| ()),
                    ConfirmService::ZeroSlot => send_zero_slot(&encoded).await.map(|_| ()),
                    ConfirmService::Helius => helius_sender::send_helius_sender(&encoded).await.map(|_| ()),
                };
                (service, result)
            }
        });
    if race_submissions(sends, tx.signatures[0]).await {
        return Ok(());
    }
    if services == [ConfirmService::Rpc] || !fallback {
        bail!("every route rejected {}", tx.signatures[0]);
    }
    warn!("every service rejected {}; sending through RPC instead", tx.signatures[0]);
    send_rpc(tx).await?;
    Ok(())
}



async fn race_submissions<F>(sends: impl IntoIterator<Item = F>, signature: Signature) -> bool
where
    F: std::future::Future<Output = (ConfirmService, Result<()>)> + Send + 'static,
{
    let mut pending: FuturesUnordered<_> = sends.into_iter().map(|send| {
        tokio::spawn(async move {
            let (service, result) = send.await;
            if let Err(error) = &result {
                warn!("{service:?} rejected {signature}: {error:#}");
            }
            result.is_ok()
        })
    }).collect();
    while let Some(result) = pending.next().await {
        match result {
            Ok(true) => return true,
            Ok(false) => {},
            Err(error) => warn!("relay task for {signature} failed: {error}"),
        }
    }
    false
}

pub async fn send_rpc(tx: &VersionedTransaction) -> Result<Signature> {
    let config = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: Some(CommitmentConfig::processed().commitment),
        ..Default::default()
    };
    Ok(RPC_CLIENT.send_transaction_with_config(tx, config).await?)
}

pub async fn simulate(tx: &VersionedTransaction, min_context_slot: Option<u64>) -> Result<Submitted> {
    simulate_watching(tx, min_context_slot, None).await
}

/// SPL token account amount (bytes 64..72; the same for Token-2022 accounts).
fn token_amount(data: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(64..72)?.try_into().ok()?))
}

pub async fn simulate_watching(tx: &VersionedTransaction, min_context_slot: Option<u64>, watch: Option<Pubkey>) -> Result<Submitted> {
    let ready = ReadyMark::now();
    let config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        min_context_slot,
        accounts: watch.map(|w| RpcSimulateTransactionAccountsConfig {
            encoding: Some(UiAccountEncoding::Base64),
            addresses: vec![w.to_string()],
        }),
        ..Default::default()
    };
    let mut attempt = 0;
    let result = loop {
        match RPC_CLIENT.simulate_transaction_with_config(tx, config.clone()).await {
            Ok(response) => break response.value,
            // The node has not reached the slot yet (JSON-RPC error -32016); give it a moment.
            Err(e) if attempt < MIN_CONTEXT_SLOT_RETRIES && e.to_string().contains("context slot") => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let watched_tokens = result
        .accounts
        .as_ref()
        .and_then(|accounts| accounts.first().cloned().flatten())
        .and_then(|account| account.data.decode())
        .and_then(|data| token_amount(&data));
    Ok(Submitted::Simulated {
        watched_tokens,
        signature: tx.signatures[0],
        units_consumed: result.units_consumed,
        error: result.err.map(|e| format!("{e:?}")),
        logs: result.logs.unwrap_or_default(),
        ready,
    })
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    type RelayFuture = futures::future::BoxFuture<'static, (ConfirmService, Result<()>)>;

    #[tokio::test]
    async fn first_acceptance_does_not_cancel_other_relays() {
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let (completed, completion) = tokio::sync::oneshot::channel();
        let sends: Vec<RelayFuture> = vec![
            Box::pin(async { (ConfirmService::Rpc, Ok(())) }),
            Box::pin(async move {
                wait.await.unwrap();
                completed.send(()).unwrap();
                (ConfirmService::Nozomi, Ok(()))
            }),
        ];
        assert!(race_submissions(sends, Signature::default()).await);
        release.send(()).expect("second relay must remain alive after first acceptance");
        tokio::time::timeout(Duration::from_secs(1), completion).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn only_all_failures_require_fallback() {
        for results in [vec![false, false], vec![false, true]] {
            let expected = results.iter().any(|ok| *ok);
            let sends = results.into_iter().map(|ok| async move {
                (ConfirmService::Rpc, if ok { Ok(()) } else { Err(anyhow::anyhow!("rejected")) })
            });
            assert_eq!(race_submissions(sends, Signature::default()).await, expected);
        }
    }
}
