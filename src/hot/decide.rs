//! The buy decision on the hot path (A6 and the checks of A2-A4), allocation-free. It repeats the
//! slow path's checks in the slow path's order, on the same data, with no RPC:
//!
//! 1. `shred_feed::snipe`: unknown create layout, dev buy through another program, pump.fun config,
//!    an unbelievable bundle;
//! 2. `common::spawn_snipe`: trading paused, a non-standard curve, the entry rules
//!    (`rules::launch_verdict_code`), the open-positions limit;
//! 3. `common::snipe`: trading hours, the block producer's region, the deployer's wallet age;
//! 4. `common::buy_once`: too many slots past the launch, the bet, the quote and the price guard.
//!
//! What only RPC could tell (a wallet age or a deployer balance not cached) is `Unknown`: the slow
//! path, which asks RPC, decides those launches.

use {
    super::decode::{Buy, Launch},
    crate::{
        config::{buy_lamports, rules, TRADE_SETTING},
        instructions::{
            pumpfun_buy_ix::{pumpfun_config, CurveReserves, PumpFunMarket},
            pumpswap_buy_ix::PUMP_PROGRAM_ID,
        },
        processors::{common, deployer_balances, shred_feed},
        utils::{control, creator_history, dashboard, deployer_record, deployer_wallet, latency, launch_results, leaders, reference, snipe_test},
    },
    solana_pubkey::Pubkey,
};

/// Why the hot path does not buy. Codes 1-12 are `rules::Reject`; these follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Rules(rules::Reject),
    LayoutUnknown,
    DevBuyHidden,
    ConfigMissing,
    BundleTooBig,
    Paused,
    BadCurve,
    Positions,
    OutsideHours,
    FarProducer,
    YoungWallet,
    TooLate,
    QuoteFailed,
    ZeroOutput,
    /// The fee engine's route for the leader's client class says not to buy (`fee::skips`).
    FeeRoute,
    /// The distance rule: the leader (or, in its last slot, the next one) is too far (`distance`).
    Distance,
    /// The snipe test's filter or caps (`utils::snipe_test`, SNIPE_TEST=1).
    Test(snipe_test::TestReject),
}

impl Reject {
    pub fn code(self) -> u8 {
        match self {
            Reject::Rules(r) => r as u8,
            Reject::LayoutUnknown => 20,
            Reject::DevBuyHidden => 21,
            Reject::ConfigMissing => 22,
            Reject::BundleTooBig => 23,
            Reject::Paused => 24,
            Reject::BadCurve => 25,
            Reject::Positions => 26,
            Reject::OutsideHours => 27,
            Reject::FarProducer => 28,
            Reject::YoungWallet => 29,
            Reject::TooLate => 30,
            Reject::QuoteFailed => 31,
            Reject::ZeroOutput => 32,
            Reject::FeeRoute => 33,
            Reject::Distance => 34,
            Reject::Test(t) => t.code(),
        }
    }
}

/// What the decision needed and did not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    WalletAge,
    Balance,
}

/// Every per-coin account of the buy, derived as soon as the mint and creator are known (A6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Derived {
    /// The coin's creator on the curve (the holder-rewards account for a holder-reward coin).
    pub creator: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub creator_vault: Pubkey,
    pub bonding_curve_v2: Pubkey,
    pub user_ata: Pubkey,
}

/// Derives the per-coin accounts for `wallet`'s buy. The bonding curve and its token account come
/// from the create's own accounts when it names them (no hashing then).
pub fn derive(l: &Launch, wallet: &Pubkey) -> Derived {
    use super::pda::{associated_token_address, find_program_address};
    let creator = if l.holder_reward { find_program_address(&[b"holder-rewards", l.mint.as_ref()], &PUMP_PROGRAM_ID).0 } else { l.named_creator };
    let bonding_curve = l.bonding_curve.unwrap_or_else(|| find_program_address(&[b"bonding-curve", l.mint.as_ref()], &PUMP_PROGRAM_ID).0);
    let associated_bonding_curve = l.associated_bonding_curve.unwrap_or_else(|| associated_token_address(&bonding_curve, &l.mint, &l.token_program));
    Derived {
        creator,
        bonding_curve,
        associated_bonding_curve,
        creator_vault: creator_vault_of(&creator),
        bonding_curve_v2: find_program_address(&[b"bonding-curve-v2", l.mint.as_ref()], &PUMP_PROGRAM_ID).0,
        user_ata: associated_token_address(wallet, &l.mint, &l.token_program),
    }
}

/// `derive` in four steps of at most one address search each (the decision thread's warm-up runs
/// one step per idle look at its rings, `HOT_WARM_STEPS`): 0 the creator and the bonding curve,
/// 1 the curve's token account and the creator vault, 2 the v2 curve, 3 our token account. After
/// step 3, `d` is `derive`'s.
pub fn derive_step(l: &Launch, wallet: &Pubkey, d: &mut Derived, step: u8) {
    use super::pda::{associated_token_address, find_program_address};
    match step {
        0 => {
            d.creator = if l.holder_reward { find_program_address(&[b"holder-rewards", l.mint.as_ref()], &PUMP_PROGRAM_ID).0 } else { l.named_creator };
            d.bonding_curve = l.bonding_curve.unwrap_or_else(|| find_program_address(&[b"bonding-curve", l.mint.as_ref()], &PUMP_PROGRAM_ID).0);
        }
        1 => {
            d.associated_bonding_curve = l.associated_bonding_curve.unwrap_or_else(|| associated_token_address(&d.bonding_curve, &l.mint, &l.token_program));
            d.creator_vault = creator_vault_of(&d.creator);
        }
        2 => d.bonding_curve_v2 = find_program_address(&[b"bonding-curve-v2", l.mint.as_ref()], &PUMP_PROGRAM_ID).0,
        _ => d.user_ata = associated_token_address(wallet, &l.mint, &l.token_program),
    }
}

/// The creator vault of `creator`, derived once per creator on this thread (2026-10-11): deployers
/// repeat, and one `find_program_address` costs ~10 µs of the launch's path. A direct-mapped table
/// (4,096 entries, 256 KB) built on first use; a collision just derives again. No allocation after
/// the first call. HOT_VAULT_CACHE=false derives it every time, as before.
pub fn creator_vault_of(creator: &Pubkey) -> Pubkey {
    use super::pda::find_program_address;
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| !crate::config::env_opt("HOT_VAULT_CACHE").is_some_and(|v| v.eq_ignore_ascii_case("false")));
    if !*ON {
        return find_program_address(&[b"creator-vault", creator.as_ref()], &PUMP_PROGRAM_ID).0;
    }
    const SLOTS: usize = 4096;
    thread_local! {
        static VAULTS: std::cell::RefCell<Box<[([u8; 32], [u8; 32])]>> =
            std::cell::RefCell::new(vec![([0u8; 32], [0u8; 32]); SLOTS].into_boxed_slice());
    }
    let key = creator.to_bytes();
    let i = usize::from_le_bytes(key[..8].try_into().expect("8 bytes")) & (SLOTS - 1);
    VAULTS.with(|v| {
        let mut v = v.borrow_mut();
        if v[i].0 == key {
            return Pubkey::new_from_array(v[i].1);
        }
        let vault = find_program_address(&[b"creator-vault", creator.as_ref()], &PUMP_PROGRAM_ID).0;
        v[i] = (key, vault.to_bytes());
        vault
    })
}

/// `derive`, but for a coin nobody follows only its creator (the entry rules refuse it before any
/// other account is used): one hash for a holder-reward coin, none otherwise.
pub fn derive_if_followed(l: &Launch, wallet: &Pubkey) -> Derived {
    let creator = if l.holder_reward { super::pda::find_program_address(&[b"holder-rewards", l.mint.as_ref()], &PUMP_PROGRAM_ID).0 } else { l.named_creator };
    let followed = reference::pair_view(&l.deployer, &creator, dashboard::now_unix()).0.is_some();
    if followed {
        return derive(l, wallet);
    }
    let none = Pubkey::default();
    Derived { creator, bonding_curve: none, associated_bonding_curve: none, creator_vault: none, bonding_curve_v2: none, user_ata: none }
}

/// A decided buy.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub lamports_in: u64,
    pub min_out: u64,
    pub expected: u64,
    pub follow: rules::Follow,
    pub small_bet: bool,
    /// The curve after the dev buy and the create's bundle (the quote's state).
    pub reserves: CurveReserves,
    /// SOL the dev buy put into the curve.
    pub dev_buy_lamports: u64,
    /// The bundle as the decision saw it: distinct buyers besides the dev (None: one unreadable)
    /// and the SOL they put in.
    pub bundle_wallets: Option<u32>,
    pub bundle_lamports: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Buy(Plan, rules::LaunchFacts),
    Reject(Reject, Option<rules::LaunchFacts>),
    Unknown(Missing, rules::LaunchFacts),
}

impl Outcome {
    pub fn reject_code(&self) -> u8 {
        match self {
            Outcome::Reject(r, _) => r.code(),
            _ => 0,
        }
    }
}

/// The create's bundle as the decision sees it: the curve after it and its distinct wallets.
pub(crate) fn bundle(l: &Launch, d: &Derived, initial: CurveReserves, fee_bps: u64, later: &[Buy]) -> (CurveReserves, CurveReserves, Option<u32>) {
    let after_dev = shred_feed::apply_buy(initial, l.dev_buy_tokens, l.dev_buy_lamports, fee_bps);
    let mut reserves = after_dev;
    // Distinct buyers besides the deployer and the creator, at most 16 tracked (more is "many").
    let mut wallets = [Pubkey::default(); 16];
    let mut n = 0usize;
    let mut known = true;
    for b in later.iter().filter(|b| b.mint == l.mint) {
        reserves = shred_feed::apply_buy(reserves, b.tokens, b.lamports, fee_bps);
        match b.buyer {
            None => known = false,
            Some(w) if w == l.deployer || w == d.creator => {}
            Some(w) => {
                if !wallets[..n.min(16)].contains(&w) {
                    if n < 16 {
                        wallets[n] = w;
                    }
                    n += 1;
                }
            }
        }
    }
    (after_dev, reserves, known.then_some(n as u32))
}

/// The bet for a dev buy, capped by what the wallet can spend (`common::bet_size` without its log).
fn bet(dev_buy_lamports: u64) -> (u64, bool) {
    // The snipe test: its size, never scaled (the wallet check is the decision's).
    if snipe_test::on() {
        return (snipe_test::size_lamports(), false);
    }
    let stake = buy_lamports();
    let lamports = rules::bet_lamports(stake, dev_buy_lamports);
    let small = lamports < stake;
    let reserve = rules::lamports(rules::BALANCE_RESERVE_SOL);
    match dashboard::wallet_balance_lamports() {
        Some(balance) => {
            let spendable = balance.saturating_sub(reserve);
            if lamports <= spendable || spendable == 0 { (lamports, small) } else { (spendable, small) }
        }
        None => (lamports, small),
    }
}

/// What the decision reads from the bot's shared tables (each a lock and a map lookup), gathered
/// when the create arrives, inside its bundle window (HOT_PREDECIDE, 2026-10-11), so the decision
/// at the window's end is arithmetic on values in hand. None of these lookups has a side effect.
#[derive(Clone, Copy, Debug)]
pub struct Pre {
    /// pump.fun's initial curve and fee (bps), when its config is loaded.
    config: Option<(CurveReserves, u64)>,
    prior: creator_history::PriorLaunches,
    balance: Option<u64>,
    view: (Option<rules::Follow>, Option<i64>, Option<u32>),
    record: (u32, u32),
    wallet_age: Option<bool>,
    /// The scorecard's per-deployer facts: the wallet's first transaction, the creator's earlier
    /// launches.
    first_tx: Option<deployer_wallet::FirstTx>,
    earlier: Option<(u32, f64, f64)>,
}

pub(crate) fn config_now() -> Option<(CurveReserves, u64)> {
    pumpfun_config().ok().map(|c| (c.initial_reserves, c.fee_tiers.first().map(|(_, f)| f.protocol_bps + f.creator_bps).unwrap_or(125)))
}

fn balance_now(l: &Launch, slot: u64, config: Option<(CurveReserves, u64)>) -> Option<u64> {
    let (initial, fee_bps) = config?;
    let dev_buy_lamports = shred_feed::apply_buy(initial, l.dev_buy_tokens, l.dev_buy_lamports, fee_bps).real_sol_reserves;
    l.fee_payer.and_then(|payer| deployer_balances::balance_before(&payer, slot, dev_buy_lamports + common::CREATE_COST_LAMPORTS))
}

/// The decision's table lookups for a create in `slot` at unix time `now` (`Pre`).
pub fn prepare(l: &Launch, d: &Derived, slot: u64, now: u64) -> Pre {
    let config = config_now();
    let prior = creator_history::prior(&l.deployer, &l.mint, now);
    Pre {
        config,
        prior,
        balance: balance_now(l, slot, config),
        view: reference::pair_view(&l.deployer, &d.creator, now),
        record: deployer_record::record_of(&l.deployer, now),
        wallet_age: common::wallet_age_skip_cached(&l.deployer, Some(prior), now),
        first_tx: deployer_wallet::get(&l.deployer),
        earlier: launch_results::earlier(&d.creator, now),
    }
}

/// Decides one launch seen in `slot`, with the create's bundle buys `later`, at unix time `now`.
pub fn decide(l: &Launch, d: &Derived, slot: u64, later: &[Buy], now: u64) -> Outcome {
    decide_prepared(l, d, slot, later, now, &prepare(l, d, slot, now))
}

/// `decide` with its table lookups made earlier (`prepare`): the same checks in the same order.
/// A value that was missing then (the config, the deployer's balance, the wallet's age) is looked
/// up again now, in case it came in meanwhile.
pub fn decide_prepared(l: &Launch, d: &Derived, slot: u64, later: &[Buy], now: u64, pre: &Pre) -> Outcome {
    // 1. shred_feed::snipe
    if l.creator_set_onchain {
        return Outcome::Reject(Reject::LayoutUnknown, None);
    }
    if l.dev_buy_lamports == 0 {
        return Outcome::Reject(Reject::DevBuyHidden, None);
    }
    let Some((initial, fee_bps)) = pre.config.or_else(config_now) else { return Outcome::Reject(Reject::ConfigMissing, None) };
    let (after_dev, reserves, bundle_wallets) = bundle(l, d, initial, fee_bps, later);
    if reserves.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves) > shred_feed::MAX_EARLY_BUNDLE_LAMPORTS {
        return Outcome::Reject(Reject::BundleTooBig, None);
    }
    let dev_buy_lamports = after_dev.real_sol_reserves;
    let prior = pre.prior;
    let balance = pre.balance.or_else(|| balance_now(l, slot, Some((initial, fee_bps))));

    // 2. common::spawn_snipe
    if !control::trading_enabled() {
        return Outcome::Reject(Reject::Paused, None);
    }
    if reserves.virtual_sol_reserves == 0 || reserves.virtual_token_reserves == 0 {
        return Outcome::Reject(Reject::BadCurve, None);
    }
    let (follow, reference_profit, reference_trades) = pre.view;
    let facts = rules::LaunchFacts {
        mayhem: l.mayhem,
        dev_buy_lamports,
        deployer_lamports: balance,
        follow,
        reference_profit_lamports: reference_profit,
        reference_trades,
        insider_lamports: Some(reserves.real_sol_reserves),
        risky_host: l.risky_host,
        secs_since_previous_launch: prior.secs_since_previous,
        launches_last_day: Some(prior.last_day),
        bundle_wallets,
        name_all_caps: Some(l.name_all_caps),
        deployer_record: pre.record,
        scoring: rules::Scoring {
            launches_last_hour: Some(prior.last_hour),
            wallet_first_bt: pre.first_tx.and_then(|f| f.first_bt),
            funded_sol: pre.first_tx.and_then(|f| f.funded_sol),
            earlier: pre.earlier,
            bundle_lamports: Some(reserves.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves)),
            now,
        },
    };
    let verdict = if balance.is_some() { rules::launch_verdict_code(&facts) } else { rules::verdict_code_before_balance(&facts) };
    let follow = match verdict {
        Ok(f) => f,
        Err(r) => return Outcome::Reject(Reject::Rules(r), Some(facts)),
    };
    // The snipe test: its four rules on what this decision sees (SNIPE_TEST_RULES=on), then its
    // caps (read only; the entry is taken where the buy commits).
    if snipe_test::on() {
        let visible = snipe_test::Visible {
            dev_buy_lamports,
            bundle_wallets,
            real_sol_lamports: reserves.real_sol_reserves,
            complete: reserves.real_token_reserves == 0,
        };
        let rules = if snipe_test::rules_on() { snipe_test::filter(&visible) } else { Ok(()) };
        if let Err(t) = rules.and_then(|()| snipe_test::check_caps(now)) {
            return Outcome::Reject(Reject::Test(t), Some(facts));
        }
        let reserve = rules::lamports(rules::BALANCE_RESERVE_SOL);
        if dashboard::wallet_balance_lamports().is_some_and(|b| b.saturating_sub(reserve) < snipe_test::size_lamports()) {
            return Outcome::Reject(Reject::Test(snipe_test::TestReject::Wallet), Some(facts));
        }
    }
    if common::open_position_count() >= rules::max_open_positions() {
        return Outcome::Reject(Reject::Positions, Some(facts));
    }

    // 3. common::snipe
    let setting = &*TRADE_SETTING;
    if let Some(window) = setting.trading_hours {
        let minute = ((now % 86_400) / 60) as u16;
        if !crate::config::trade_setting::in_window(window, minute) {
            return Outcome::Reject(Reject::OutsideHours, Some(facts));
        }
    }
    if !setting.leader_regions.is_empty() && leaders::far_producer_skips(slot, &setting.leader_regions) {
        return Outcome::Reject(Reject::FarProducer, Some(facts));
    }
    // With the scorecard on, the wallet's age is one of the score's facts, not a rule.
    if !crate::config::scorecard::enabled() {
        match pre.wallet_age.or_else(|| common::wallet_age_skip_cached(&l.deployer, Some(prior), now)) {
            Some(true) => return Outcome::Reject(Reject::YoungWallet, Some(facts)),
            Some(false) => {}
            None => return Outcome::Unknown(Missing::WalletAge, facts),
        }
    }
    if balance.is_none() {
        return Outcome::Unknown(Missing::Balance, facts);
    }

    // 4. common::buy_once
    if latency::latest_slot().is_some_and(|latest| latest > slot + rules::MAX_LAUNCH_AGE_SLOTS) {
        return Outcome::Reject(Reject::TooLate, Some(facts));
    }
    let (lamports_in, small_bet) = bet(dev_buy_lamports);
    let market = PumpFunMarket::with_curve(l.mint, d.bonding_curve, l.token_program, d.creator, l.mayhem, reserves);
    let expected = match market.quote_buy(lamports_in) {
        Ok(0) => return Outcome::Reject(Reject::ZeroOutput, Some(facts)),
        Ok(t) => t,
        Err(_) => return Outcome::Reject(Reject::QuoteFailed, Some(facts)),
    };
    let min_out = setting.buy_min_out(expected);
    let bundle_lamports = reserves.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves);
    Outcome::Buy(Plan { lamports_in, min_out, expected, follow, small_bet, reserves, dev_buy_lamports, bundle_wallets, bundle_lamports }, facts)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            instructions::pumpfun_buy_ix::{bonding_curve_address, set_test_config},
            utils::utils::{TOKEN_2022_PROGRAM_ID, WSOL_MINT},
        },
    };

    fn launch() -> Launch {
        let mint = Pubkey::new_unique();
        Launch {
            mint,
            deployer: Pubkey::new_unique(),
            named_creator: Pubkey::new_unique(),
            holder_reward: false,
            token_program: TOKEN_2022_PROGRAM_ID,
            bonding_curve: Some(bonding_curve_address(&mint)),
            associated_bonding_curve: None,
            mayhem: false,
            creator_set_onchain: false,
            dev_buy_tokens: 0,
            dev_buy_lamports: 2_025_000_000,
            risky_host: Some(false),
            fast_rug_host: false,
            name_all_caps: false,
            fee_payer: Some(WSOL_MINT),
            signature: [0; 64],
        }
    }

    #[test]
    fn the_accounts_match_the_market_builders_derivation() {
        set_test_config();
        let l = launch();
        let wallet = Pubkey::new_unique();
        let d = derive(&l, &wallet);
        let market = PumpFunMarket::new(l.mint, l.token_program, l.named_creator, false, Default::default());
        let built = market.buy_instructions(&wallet, 1, 1).unwrap();
        let a = &built[1].accounts;
        assert_eq!(
            (d.bonding_curve, d.associated_bonding_curve, d.user_ata, d.creator_vault, d.bonding_curve_v2),
            (a[3].pubkey, a[4].pubkey, a[5].pubkey, a[9].pubkey, a[16].pubkey)
        );
        let holder = Launch { holder_reward: true, ..l };
        assert_eq!(derive(&holder, &wallet).creator, shred_feed::holder_rewards_account(&l.mint));
    }

    #[test]
    fn early_skips_come_in_the_slow_paths_order() {
        let _state = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        set_test_config();
        let l = launch();
        let d = derive(&l, &Pubkey::new_unique());
        let code = |l: &Launch| decide(l, &d, 1, &[], 2_000_000_000).reject_code();
        assert_eq!(code(&Launch { creator_set_onchain: true, dev_buy_lamports: 0, ..l }), Reject::LayoutUnknown.code());
        assert_eq!(code(&Launch { dev_buy_lamports: 0, ..l }), Reject::DevBuyHidden.code());
        control::set_trading(false);
        assert_eq!(code(&l), Reject::Paused.code());
        control::set_trading(true);
        // Nobody follows this deployer: the entry rules refuse it.
        assert_eq!(code(&l), rules::Reject::NotFollowed as u8);
        control::set_trading(false);
    }

    #[test]
    fn a_big_bundle_is_counted_by_distinct_wallets() {
        set_test_config();
        let l = launch();
        let d = derive(&l, &Pubkey::new_unique());
        let config = pumpfun_config().unwrap();
        let (w1, w2) = (Pubkey::new_unique(), Pubkey::new_unique());
        let buy = |buyer| Buy { mint: l.mint, tokens: 0, lamports: 100_000_000, buyer, cu_price: 0 };
        let later = [buy(Some(w1)), buy(Some(w1)), buy(Some(w2)), buy(Some(l.deployer)), Buy { mint: Pubkey::new_unique(), ..buy(Some(w2)) }];
        let (after_dev, after, wallets) = bundle(&l, &d, config.initial_reserves, 125, &later);
        assert_eq!(wallets, Some(2), "w1 twice and the deployer count once and not at all");
        assert!(after.real_sol_reserves > after_dev.real_sol_reserves, "the bundle moved the curve");
        let (_, _, unknown) = bundle(&l, &d, config.initial_reserves, 125, &[buy(None)]);
        assert_eq!(unknown, None, "an unreadable buyer leaves the count unknown");
    }

    /// The creator vault comes from the table after the first derivation, the same address, and
    /// nothing allocates on a hit.
    #[test]
    fn the_creator_vault_is_derived_once_per_creator() {
        use super::super::pda::find_program_address;
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let want_a = find_program_address(&[b"creator-vault", a.as_ref()], &PUMP_PROGRAM_ID).0;
        let want_b = find_program_address(&[b"creator-vault", b.as_ref()], &PUMP_PROGRAM_ID).0;
        assert_eq!(creator_vault_of(&a), want_a);
        assert_eq!(creator_vault_of(&b), want_b);
        let (got, n) = super::super::alloc_audit::allocations(|| creator_vault_of(&a));
        assert_eq!((got, n), (want_a, 0));
    }
}
