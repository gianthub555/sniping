//! The hot-path threads (A2, A6, A7). Each early feed gets its own thread: the Helius
//! preprocessed WebSocket (a blocking socket, no executor) and pumpfeed's UDP entry push. A feed
//! thread stamps each frame, decodes it with `decode` (no allocation) and hands the launch to a
//! decider, either on the same thread (`HOT_HANDOFF=inline`) or over an SPSC ring to the pinned
//! decision thread (`spsc`, the default). The decider derives the coin's accounts as soon as the
//! create arrives (A6), waits out the 250 us bundle window on the clock (preprocessed only),
//! decides (`decide`), fills the buy template, signs and serializes it, and records the stamps.
//!
//! Every frame then goes on to the slow path unchanged (`preprocessed::frame`,
//! `shred_feed::handle_batch`), which keeps doing everything it did: paper buys, launch records,
//! the dashboard. Until the send lanes exist (Part B) the hot path sends nothing: it runs only
//! with DRY_RUN and measures. `HOT_SHADOW=true` builds and signs a buy for every create, bought or
//! not, so the timing has samples on every launch.

use {
    super::{
        affinity,
        clock::now_ns,
        decide::{self, Derived, Outcome},
        decode::{self, Buy, Launch, Tx},
        lanes::{self, LaneConfig, Lanes},
        live::{self, Decision, HotBuy, Kind},
        parity, positions,
        shreds::{self, book, deshred::Deshredder, net, shred::Shred, walk},
        spsc,
        stamps::{self, Feed, Mode, Stamp, FLAG_BUY, FLAG_SAMPLE, FLAG_SHADOW, FLAG_UNKNOWN},
        template::{base64_into, spec_from_settings, BuyFill, BuyTemplate, TemplateSpec, MAX_TIPS, PACKET, PACKET_B64},
        verify,
    },
    crate::{
        config::{env_opt, rules, TRADE_SETTING},
        instructions::pumpfun_buy_ix::pumpfun_config,
        processors::common,
        utils::{blockhash, dashboard::now_unix},
    },
    log::{info, warn},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::{
        net::UdpSocket,
        sync::{Arc, LazyLock, Mutex},
        thread::Thread,
        time::Duration,
    },
};

const WINDOW_NS: u64 = 250_000;
/// After a create is decided, its slot's buys are still counted this long, for the cross-check
/// (`verify::note_late`): buys the slow path may count that came after the hot window.
const LATE_NS: u64 = 20_000_000;
const MAX_LATE: usize = 32;
const MAX_PENDING: usize = 64;
const MAX_LATER: usize = 32;
const MAX_UDP_LATER: usize = 16;
const RING: usize = 256;
/// Most transactions one deshredded batch can hold (a 64 KB datagram of 150-byte transactions).
const MAX_BATCH_TXS: usize = 512;
/// A dummy bet for shadow builds (launches the bot would not buy).
const SHADOW_LAMPORTS: u64 = 100_000_000;

#[derive(Clone, Debug)]
pub struct HotConfig {
    pub enabled: bool,
    pub mode: Mode,
    pub shadow: bool,
    pub busy_poll: bool,
    pub pin: bool,
    /// Cores for the preprocessed feed, the UDP feed and the decision thread.
    pub cores: [usize; 3],
    /// The in-process shred thread's core: HOT_CORES' fourth entry, else the UDP feed's core
    /// when that feed is off.
    pub inproc_core: Option<usize>,
    /// While idle, run a throwaway decode / derive / sign this often (microseconds; 0: never), so
    /// the path stays in the CPU caches between launches (pumpfeed's ENGINE_WARM_US).
    pub warm_us: u64,
    /// HOT_PRESIGN (default true, 2026-10-11): the buy's signature finished from a nonce made
    /// ahead of time (`hot::presign`, ~3 µs) instead of `Keypair::sign_message` (25–46 µs).
    pub presign: bool,
    /// HOT_PREDECIDE (default true, 2026-10-11): the decision's table lookups made when the create
    /// arrives, inside its bundle window (`decide::prepare`); at the window's end only arithmetic
    /// is left. false: all of it at the window's end, as before.
    pub predecide: bool,
    /// HOT_WARM_STEPS (default true, 2026-10-11): the idle warm-up in steps of at most one address
    /// search, the rings looked at between steps, so a create waits at most one step (~10-25 µs)
    /// instead of a whole warm-up (60-130 µs). false: one warm-up in one go, as before.
    pub warm_steps: bool,
}

impl HotConfig {
    pub fn from_env() -> Self {
        let flag = |k: &str, default: bool| env_opt(k).map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(default);
        let cores: Vec<usize> = env_opt("HOT_CORES").unwrap_or_else(|| "5,6,7".into()).split(',').filter_map(|c| c.trim().parse().ok()).collect();
        HotConfig {
            enabled: flag("FAST_PATH", false),
            mode: if env_opt("HOT_HANDOFF").is_some_and(|m| m.eq_ignore_ascii_case("inline")) { Mode::Inline } else { Mode::Spsc },
            shadow: flag("HOT_SHADOW", false),
            busy_poll: flag("HOT_BUSY_POLL", true),
            pin: flag("HOT_PIN", true),
            cores: [*cores.first().unwrap_or(&5), *cores.get(1).unwrap_or(&6), *cores.get(2).unwrap_or(&7)],
            inproc_core: cores.get(3).copied(),
            warm_us: env_opt("HOT_WARM_US").and_then(|v| v.parse().ok()).unwrap_or(1_000),
            presign: flag("HOT_PRESIGN", true),
            predecide: flag("HOT_PREDECIDE", true),
            warm_steps: flag("HOT_WARM_STEPS", true),
        }
    }
}

pub static CONFIG: LazyLock<HotConfig> = LazyLock::new(HotConfig::from_env);

/// A create as a feed hands it over. `Copy`, fixed size: it crosses the ring by value.
#[derive(Clone, Copy)]
pub struct CreateEv {
    pub launch: Launch,
    pub slot: u64,
    pub feed: Feed,
    pub t_recv: u64,
    pub t_matched: u64,
    /// In-process only (0 otherwise): the shred completing the create's batch off the socket, and
    /// the batch reassembled.
    pub t_shred: u64,
    pub t_deshred: u64,
}

/// A pump.fun buy seen on the preprocessed stream, for the open bundle windows of its slot.
#[derive(Clone, Copy)]
pub struct BuyEv {
    pub buy: Buy,
    pub slot: u64,
    /// The transaction's first signature bytes, to count a re-sent frame once.
    pub sig: [u8; 8],
    /// The buy's place among the transaction's buys: a bundle that buys for several wallets in
    /// one transaction gives each the same signature, so (sig, seq) is what counts a buy once.
    pub seq: u8,
}

/// A coin's creator moving to a fee-sharing account (`decode::creator_migrations`), seen on a
/// feed: a buy of the coin that lands after it pays the sharing config's creator vault.
#[derive(Clone, Copy)]
pub struct MigrationEv {
    pub mint: Pubkey,
    pub slot: u64,
}

/// bundle10 (BUNDLE10_SHADOW): one pump.fun trade of a coin created a moment before in the same
/// slot, for bundle10's count (`hot::bundle10::Count`). Smaller than `CreateEv`: the ring's slots
/// do not grow.
#[derive(Clone, Copy)]
pub struct TradeEv {
    pub trade: super::bundle10::Trade,
    pub slot: u64,
    /// The transaction's first signature bytes, and the trade's place among its trades.
    pub sig: [u8; 8],
    pub seq: u16,
}

#[derive(Clone, Copy)]
pub enum Ev {
    Create(CreateEv),
    Buy(BuyEv),
    /// UDP: the batch that held this coin's create has been read (its later buys came before this).
    BatchEnd(Pubkey),
    /// bundle10 only (never sent while BUNDLE10_SHADOW is off).
    Trade(TradeEv),
    CreatorMigration(MigrationEv),
}

/// First sighting of a coin across feeds and threads: a direct-mapped set (one slot per hash of
/// the mint, a newer coin replaces an older one in its slot). One lookup, no scan, no allocation.
/// A second sighting comes within milliseconds, long before 65,536 slots refill.
const SEEN_SLOTS: usize = 1 << 16;

#[derive(Clone, Copy)]
struct SeenSlot {
    mint: [u8; 32],
    at: u64,
    feed: Feed,
}

static SEEN: LazyLock<Mutex<Box<[SeenSlot]>>> =
    LazyLock::new(|| Mutex::new(vec![SeenSlot { mint: [0; 32], at: 0, feed: Feed::Bench }; SEEN_SLOTS].into_boxed_slice()));

/// Which feed sees creates first: counts, and the winner's lead when both feeds saw a coin.
pub mod feeds {
    use std::sync::{atomic::AtomicU64, Mutex};
    pub static PP_FRAMES: AtomicU64 = AtomicU64::new(0);
    pub static PP_CREATES: AtomicU64 = AtomicU64::new(0);
    pub static UDP_BATCHES: AtomicU64 = AtomicU64::new(0);
    pub static UDP_BAD: AtomicU64 = AtomicU64::new(0);
    pub static UDP_CREATES: AtomicU64 = AtomicU64::new(0);
    pub static FIRST_PP: AtomicU64 = AtomicU64::new(0);
    pub static FIRST_UDP: AtomicU64 = AtomicU64::new(0);
    pub static INPROC_BATCHES: AtomicU64 = AtomicU64::new(0);
    pub static INPROC_CREATES: AtomicU64 = AtomicU64::new(0);
    pub static FIRST_INPROC: AtomicU64 = AtomicU64::new(0);
    /// LaserStream's transactions handed to the hot path while the preprocessed stream is silent.
    pub static GRPC_FRAMES: AtomicU64 = AtomicU64::new(0);
    pub static GRPC_CREATES: AtomicU64 = AtomicU64::new(0);
    pub static FIRST_GRPC: AtomicU64 = AtomicU64::new(0);
    /// Open windows whose coin's creator moved to fee sharing (the buy paid the new vault).
    pub static MIGRATIONS: AtomicU64 = AtomicU64::new(0);
    /// The preprocessed feed runs, and `now_ns` at its last frame (the live guard's stall check).
    pub static PP_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub static PP_LAST_NS: AtomicU64 = AtomicU64::new(0);
    /// Lead in ns of the first feed over the second on coins both saw: positive when the
    /// preprocessed stream was first. A fixed ring.
    pub static LEADS: Mutex<([i64; 4096], usize, u64)> = Mutex::new(([0; 4096], 0, 0));
}

fn first_sighting(mint: &Pubkey, feed: Feed, at: u64) -> bool {
    let key = mint.to_bytes();
    let i = u64::from_le_bytes(key[..8].try_into().expect("8")) as usize & (SEEN_SLOTS - 1);
    let mut s = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let slot = &mut s[i];
    if slot.mint == key {
        if slot.feed != feed {
            let lead = at as i64 - slot.at as i64;
            let signed = if slot.feed == Feed::Preprocessed { lead } else { -lead };
            let mut l = feeds::LEADS.lock().unwrap_or_else(|e| e.into_inner());
            let (ring, next, total) = &mut *l;
            ring[*next] = signed;
            *next = (*next + 1) % ring.len();
            *total += 1;
        }
        return false;
    }
    *slot = SeenSlot { mint: key, at, feed };
    let first = match feed {
        Feed::Preprocessed => &feeds::FIRST_PP,
        Feed::Inproc => &feeds::FIRST_INPROC,
        Feed::Grpc => &feeds::FIRST_GRPC,
        _ => &feeds::FIRST_UDP,
    };
    first.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// One line on the feeds for the stats log (off the hot path: it allocates).
pub fn feed_report() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let (mut leads, total) = {
        let l = feeds::LEADS.lock().unwrap_or_else(|e| e.into_inner());
        let n = (l.2 as usize).min(l.0.len());
        (l.0[..n].to_vec(), l.2)
    };
    leads.sort_unstable();
    let pp_first = leads.iter().filter(|x| **x > 0).count();
    let mut pp: Vec<i64> = leads.iter().copied().filter(|x| *x > 0).collect();
    let mut udp: Vec<i64> = leads.iter().filter(|x| **x < 0).map(|x| -x).collect();
    pp.sort_unstable();
    udp.sort_unstable();
    let med = |v: &[i64]| v.get(v.len() / 2).map_or("-".to_string(), |x| format!("{:.2} ms", *x as f64 / 1e6));
    format!(
        "feeds: preprocessed {} frames, {} creates, first on {}; udp {} batches ({} unreadable), {} creates, first on {}; in-process {} batches, {} creates, first on {}; LaserStream backup {} transactions, {} creates, first on {}; seen by preprocessed and a shred feed: {} (last {}: preprocessed first {}, lead median {}; shred feed first {}, lead median {}); creators moved to fee sharing in a window: {}",
        feeds::PP_FRAMES.load(Relaxed),
        feeds::PP_CREATES.load(Relaxed),
        feeds::FIRST_PP.load(Relaxed),
        feeds::UDP_BATCHES.load(Relaxed),
        feeds::UDP_BAD.load(Relaxed),
        feeds::UDP_CREATES.load(Relaxed),
        feeds::FIRST_UDP.load(Relaxed),
        feeds::INPROC_BATCHES.load(Relaxed),
        feeds::INPROC_CREATES.load(Relaxed),
        feeds::FIRST_INPROC.load(Relaxed),
        feeds::GRPC_FRAMES.load(Relaxed),
        feeds::GRPC_CREATES.load(Relaxed),
        feeds::FIRST_GRPC.load(Relaxed),
        total,
        leads.len(),
        pp_first,
        med(&pp),
        udp.len(),
        med(&udp),
        feeds::MIGRATIONS.load(Relaxed)
    )
}

#[derive(Clone, Copy)]
struct Pending {
    ev: CreateEv,
    derived: Derived,
    t_picked: u64,
    t_derived: u64,
    deadline: u64,
    /// UDP: decided when its batch ends, not on a clock.
    at_batch_end: bool,
    /// DRY_RUN: built and sent as an unlandable copy to time the lanes, bought or not.
    sample: bool,
    buys: [Option<Buy>; MAX_LATER],
    sigs: [([u8; 8], u8); MAX_LATER],
    n: usize,
    /// HOT_PREDECIDE: the decision's lookups, made at arrival.
    pre: Option<decide::Pre>,
    /// The creator moved to fee sharing in the window (the create's bundle): the sharing config
    /// and its creator vault, which the buy pays (the rules keep the named creator).
    migrated: Option<(Pubkey, Pubkey)>,
}

/// A decided create whose slot's later buys are still counted (`LATE_NS`).
#[derive(Clone, Copy)]
struct Late {
    mint: Pubkey,
    slot: u64,
    closed_at: u64,
    buys: u32,
    lamports: u64,
    sigs: [([u8; 8], u8); 16],
}

/// What a decider does with a buy it built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendMode {
    /// Nothing is sent (DRY_RUN, Part A).
    Off,
    /// DRY_RUN with HOT_DRY_LANES: paper buys and a sample of other launches go out on the lanes
    /// as unlandable copies (all-zero blockhash), to time the lanes on the real connections.
    Dry,
    /// Real buys.
    Live,
}

/// Decides launches and builds their buys; one per deciding thread. Owns every buffer it needs.
pub struct Decider {
    template: BuyTemplate,
    /// HOT_NONCE_FANOUT: the durable-nonce copy's template, the nonce manager's ring ends (free
    /// accounts in, used ones out) and this thread's copy counter.
    nonce_template: Option<BuyTemplate>,
    nonce_ends: Option<(spsc::Consumer<super::nonces::Grant>, spsc::Producer<super::nonces::Used>)>,
    copy_seq: u64,
    keypair: Arc<Keypair>,
    /// HOT_PRESIGN: signatures finished from nonces made ahead (None: the keypair signs).
    presigner: Option<super::presign::Presigner>,
    wallet: Pubkey,
    tx: [u8; PACKET],
    b64: [u8; PACKET_B64],
    rng: u64,
    shadow: bool,
    mode: Mode,
    pending: [Option<Pending>; MAX_PENDING],
    warm_ns: u64,
    last_warm: u64,
    warm_launch: Launch,
    /// HOT_WARM_STEPS: the warm-up's next step (0: not started) and its accounts so far.
    warm_steps: bool,
    warm_step: u8,
    warm_derived: Derived,
    warm_len: usize,
    /// HOT_PREDECIDE.
    predecide: bool,
    send: SendMode,
    lanes: Option<Lanes>,
    handoff: Option<tokio::sync::mpsc::UnboundedSender<HotBuy>>,
    sample_ns: u64,
    last_sample: u64,
    late: [Option<Late>; MAX_LATE],
    /// bundle10's shadow (BUNDLE10_SHADOW): None while off.
    b10: Option<Box<B10>>,
}

/// bundle10's shadow on a decider: each open create's count, apart from the existing windows
/// (`Pending` is untouched), and the ring to the shadow log (`processors::bundle10_shadow`).
struct B10 {
    windows: [Option<B10Window>; B10_WINDOWS],
    out: spsc::Producer<super::bundle10::Rec>,
    /// The 0slot lane's tip (the shadow's buy cost).
    zero_slot_tip: u64,
}

#[derive(Clone, Copy)]
struct B10Window {
    mint: Pubkey,
    slot: u64,
    count: super::bundle10::Count,
}

/// Creates whose bundle10 count is open at once (a window lasts 250 us).
const B10_WINDOWS: usize = 16;
/// A feed thread hands over the trades of coins it saw created in the same slot this recently.
const B10_FEED_NS: u64 = 2 * WINDOW_NS;

impl B10 {
    fn open(&mut self, ev: &CreateEv) {
        let l = &ev.launch;
        // Shadow logs launches with a top-level dev buy only.
        if l.dev_buy_lamports == 0 {
            return;
        }
        let w = B10Window { mint: l.mint, slot: ev.slot, count: super::bundle10::Count::new(l.mint, l.deployer, l.named_creator, None) };
        let i = self.windows.iter().position(|w| w.is_none()).unwrap_or_else(|| {
            // Full (16 creates within 250 us does not happen): the oldest slot's goes.
            (0..B10_WINDOWS).min_by_key(|&i| self.windows[i].map_or(0, |w| w.slot)).unwrap_or(0)
        });
        self.windows[i] = Some(w);
    }
}

impl Decider {
    pub fn new(template: BuyTemplate, keypair: Arc<Keypair>, shadow: bool, mode: Mode) -> Self {
        let wallet = keypair.pubkey();
        let k = |b: u8| Pubkey::new_from_array([b; 32]);
        let warm_launch = Launch {
            mint: k(11),
            deployer: k(12),
            named_creator: k(13),
            holder_reward: false,
            token_program: crate::utils::utils::TOKEN_2022_PROGRAM_ID,
            bonding_curve: Some(k(14)),
            associated_bonding_curve: Some(k(15)),
            mayhem: false,
            creator_set_onchain: false,
            dev_buy_tokens: 0,
            dev_buy_lamports: 1_000_000_000,
            risky_host: None,
            fast_rug_host: false,
            name_all_caps: false,
            fee_payer: None,
            signature: [0; 64],
        };
        Decider {
            template,
            nonce_template: None,
            nonce_ends: None,
            copy_seq: 1,
            keypair,
            presigner: None,
            wallet,
            tx: [0; PACKET],
            b64: [0; PACKET_B64],
            rng: 0x2545_F491_4F6C_DD1D,
            shadow,
            mode,
            pending: [None; MAX_PENDING],
            warm_ns: CONFIG.warm_us * 1_000,
            last_warm: 0,
            warm_steps: CONFIG.warm_steps,
            warm_step: 0,
            warm_len: 0,
            warm_derived: Derived {
                creator: Pubkey::default(),
                bonding_curve: Pubkey::default(),
                associated_bonding_curve: Pubkey::default(),
                creator_vault: Pubkey::default(),
                bonding_curve_v2: Pubkey::default(),
                user_ata: Pubkey::default(),
            },
            predecide: CONFIG.predecide,
            warm_launch,
            send: SendMode::Off,
            lanes: None,
            handoff: None,
            sample_ns: 0,
            last_sample: 0,
            late: [None; MAX_LATE],
            b10: None,
        }
    }

    /// bundle10's shadow: each create's bundle10 count, handed to `out` at its window's end.
    /// Nothing of it is signed or sent.
    pub fn with_bundle10(mut self, out: spsc::Producer<super::bundle10::Rec>, zero_slot_tip: u64) -> Self {
        self.b10 = Some(Box::new(B10 { windows: [None; B10_WINDOWS], out, zero_slot_tip }));
        self
    }

    /// Gives the decider its lanes and what to do with its buys.
    pub fn with_lanes(mut self, send: SendMode, lanes: Option<Lanes>, handoff: Option<tokio::sync::mpsc::UnboundedSender<HotBuy>>, sample_ms: u64) -> Self {
        self.send = send;
        self.lanes = lanes;
        self.handoff = handoff;
        self.sample_ns = sample_ms * 1_000_000;
        self
    }

    /// While idle: a throwaway derive, fill and signature (nothing recorded, no table touched),
    /// at most every `HOT_WARM_US`. True when it ran.
    pub fn warm(&mut self, now: u64) -> bool {
        if self.warm_ns == 0 || self.has_pending() {
            return false;
        }
        if !self.warm_steps {
            if now.saturating_sub(self.last_warm) < self.warm_ns {
                return false;
            }
            self.last_warm = now;
            let l = self.warm_launch;
            let d = decide::derive(&l, &self.wallet);
            self.warm_fill_sign(&d);
            std::hint::black_box(base64_into(&self.tx[..self.warm_len], &mut self.b64));
            return true;
        }
        // HOT_WARM_STEPS: one step per call; the caller looks at its rings between steps.
        if self.warm_step == 0 {
            if now.saturating_sub(self.last_warm) < self.warm_ns {
                return false;
            }
            self.last_warm = now;
        }
        let l = self.warm_launch;
        match self.warm_step {
            0..=3 => decide::derive_step(&l, &self.wallet, &mut self.warm_derived, self.warm_step),
            4 => {
                let d = self.warm_derived;
                self.warm_fill_sign(&d);
            }
            _ => {
                std::hint::black_box(base64_into(&self.tx[..self.warm_len], &mut self.b64));
            }
        }
        self.warm_step = if self.warm_step >= 5 { 0 } else { self.warm_step + 1 };
        true
    }

    /// The warm-up's template fill and signature (into `self.tx`, its length in `warm_len`).
    fn warm_fill_sign(&mut self, d: &Derived) {
        let l = self.warm_launch;
        let fill = BuyFill {
            mint: l.mint,
            token_program: l.token_program,
            bonding_curve: d.bonding_curve,
            associated_bonding_curve: d.associated_bonding_curve,
            user_ata: d.user_ata,
            creator_vault: d.creator_vault,
            bonding_curve_v2: d.bonding_curve_v2,
            fee_recipient: d.creator,
            buyback_fee_recipient: d.creator_vault,
            lamports_in: 1,
            min_out: 1,
            cu_price: 0,
            blockhash: solana_hash::Hash::default(),
            tip_pick: 0,
            paid: None,
            cu_limit: 0,
        };
        self.template.fill(&fill);
        self.warm_len = self.sign_tx();
    }

    /// Benchmarks: the warm-up in steps (HOT_WARM_STEPS) or in one go.
    pub fn set_warm_steps(&mut self, steps: bool) {
        self.warm_steps = steps;
        self.warm_step = 0;
    }

    /// Benchmarks and tests: the decision's lookups at arrival (HOT_PREDECIDE) or at the window's end.
    pub fn set_predecide(&mut self, on: bool) {
        self.predecide = on;
    }

    /// The signer the buys use (HOT_PRESIGN): nonces made ahead on their own thread.
    pub fn with_presigner(mut self, p: super::presign::Presigner) -> Self {
        self.presigner = Some(p);
        self
    }

    /// Signatures made with a ready nonce and without one (the queue was empty).
    pub fn presign_counts(&self) -> (u64, u64) {
        self.presigner.as_ref().map_or((0, 0), |p| (p.presigned, p.fallbacks))
    }

    /// A signed transaction from the slow path (a sell, `hot::sell`), written on the lanes as a
    /// buy is: base64 into the stack buffer, the threaded lanes primed, every lane written. No
    /// allocation. Stamped as `Feed::Sell` (`t_recv` = queued, `t_picked` = here, `t_ready`,
    /// the lane writes). Without lanes (DRY_RUN) nothing is sent.
    #[inline]
    pub fn on_send(&mut self, r: &super::sell::SendReq) {
        let t_picked = now_ns();
        let Some(lanes) = self.lanes.as_mut() else {
            return;
        };
        let n = (r.len as usize).min(PACKET);
        let m = base64_into(&r.tx[..n], &mut self.b64);
        let mut stamp = Stamp {
            mint: r.mint,
            feed: Feed::Sell,
            mode: self.mode,
            flags: stamps::FLAG_SELL,
            t_recv: r.t_queued,
            t_matched: r.t_queued,
            t_picked,
            t_ready: now_ns(),
            ..Default::default()
        };
        lanes.prime();
        if r.lane == super::sell::ALL_LANES {
            lanes.send(&r.mint, &r.tx[..n], &self.b64[..m], &mut stamp.t_sent);
        } else {
            // HOT_SELL_FANOUT: this lane's own copy.
            lanes.send_turn(r.lane as usize, &r.mint, &r.tx[..n], &self.b64[..m], &mut stamp.t_sent);
        }
        stamps::record(&stamp);
    }

    /// The filled template signed into `self.tx`: with a presigned nonce when there is one, else
    /// the keypair's RFC 8032 signature. The transaction's length.
    #[inline]
    fn sign_tx(&mut self) -> usize {
        let Decider { template, presigner, keypair, tx, .. } = self;
        match presigner.as_mut() {
            Some(p) => template.sign_into_with(|m| p.sign(m), tx),
            None => template.sign_into(keypair, tx),
        }
    }

    /// The filled nonced template signed into `self.tx` (HOT_NONCE_FANOUT).
    #[inline]
    fn sign_nonce_tx(&mut self) -> usize {
        let Decider { nonce_template, presigner, keypair, tx, .. } = self;
        let Some(template) = nonce_template.as_ref() else { return 0 };
        match presigner.as_mut() {
            Some(p) => template.sign_into_with(|m| p.sign(m), tx),
            None => template.sign_into(keypair, tx),
        }
    }

    /// HOT_NONCE_FANOUT: the durable-nonce copy's template and the nonce manager's ring ends.
    pub fn with_nonce_fanout(mut self, template: BuyTemplate, ends: (spsc::Consumer<super::nonces::Grant>, spsc::Producer<super::nonces::Used>)) -> Self {
        self.nonce_template = Some(template);
        self.nonce_ends = Some(ends);
        self
    }

    fn next_rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    pub fn has_pending(&self) -> bool {
        self.pending.iter().any(|p| p.is_some())
    }

    /// The earliest bundle-window end of the waiting creates.
    pub fn next_deadline(&self) -> Option<u64> {
        self.pending.iter().flatten().filter(|p| !p.at_batch_end).map(|p| p.deadline).min()
    }

    /// A create arrives: derive its accounts now, then wait for its bundle (preprocessed) or
    /// decide at once (UDP: the bundle came with it).
    pub fn on_create(&mut self, ev: CreateEv, t_picked: u64) {
        if !first_sighting(&ev.launch.mint, ev.feed, ev.t_matched) {
            return;
        }
        lanes::FEED_SLOT.fetch_max(ev.slot, std::sync::atomic::Ordering::Relaxed);
        if let Some(b) = self.b10.as_mut() {
            b.open(&ev);
        }
        let sample = self.send == SendMode::Dry && self.sample_ns > 0 && ev.t_recv.saturating_sub(self.last_sample) >= self.sample_ns;
        if sample {
            self.last_sample = ev.t_recv;
        }
        // Only a followed coin needs its accounts (the entry rules refuse the rest before they are
        // used); shadow builds and samples derive every coin.
        // The threaded lanes wake now, so they are spinning when a buy for this create is ready.
        if let Some(lanes) = self.lanes.as_ref() {
            lanes.prime();
        }
        let derived = if self.shadow || sample { decide::derive(&ev.launch, &self.wallet) } else { decide::derive_if_followed(&ev.launch, &self.wallet) };
        let t_derived = now_ns();
        // HOT_PREDECIDE: the decision's lookups now, inside the window.
        let pre = self.predecide.then(|| decide::prepare(&ev.launch, &derived, ev.slot, now_unix()));
        // Transaction feeds (preprocessed, the LaserStream backup) close the window after 250 us;
        // entry batches at their end.
        let udp = !matches!(ev.feed, Feed::Preprocessed | Feed::Grpc);
        let deadline = if udp { u64::MAX } else { ev.t_recv + WINDOW_NS };
        let p = Pending { ev, derived, t_picked, t_derived, deadline, at_batch_end: udp, sample, buys: [None; MAX_LATER], sigs: [([0; 8], 0); MAX_LATER], n: 0, pre, migrated: None };
        match self.pending.iter_mut().find(|s| s.is_none()) {
            Some(slot) => *slot = Some(p),
            // Full (64 creates within 250 us does not happen): decide now.
            None => self.fire(&p, now_ns()),
        }
    }

    /// A buy for the open windows of its slot (the create's bundle), or one that came after the
    /// window of a decided create (counted for the cross-check).
    pub fn on_buy(&mut self, ev: BuyEv) {
        for l in self.late.iter_mut().flatten() {
            if l.slot == ev.slot && l.mint == ev.buy.mint && !l.sigs[..(l.buys as usize).min(16)].contains(&(ev.sig, ev.seq)) {
                if (l.buys as usize) < 16 {
                    l.sigs[l.buys as usize] = (ev.sig, ev.seq);
                }
                l.buys += 1;
                l.lamports += ev.buy.lamports;
            }
        }
        for p in self.pending.iter_mut().flatten() {
            if p.ev.slot == ev.slot && p.ev.launch.mint == ev.buy.mint && p.n < MAX_LATER && !p.sigs[..p.n].contains(&(ev.sig, ev.seq)) {
                p.buys[p.n] = Some(ev.buy);
                p.sigs[p.n] = (ev.sig, ev.seq);
                p.n += 1;
            }
        }
    }

    /// Tests: the buffer the last buy was built in (its signed bytes first).
    #[cfg(test)]
    pub(crate) fn last_built(&self) -> &[u8] {
        &self.tx
    }

    /// bundle10: a trade for the open bundle10 counts of its slot and coin.
    pub fn on_trade(&mut self, ev: TradeEv) {
        if let Some(b) = self.b10.as_mut() {
            for w in b.windows.iter_mut().flatten() {
                if w.slot == ev.slot && w.mint == ev.trade.mint {
                    w.count.event(None, ev.sig, ev.seq, &ev.trade);
                }
            }
        }
    }

    /// A coin's creator moved to fee sharing: its open window's buy pays the sharing config's
    /// vault. Both addresses are derived here, inside the window, not when the buy is built.
    pub fn on_migration(&mut self, ev: MigrationEv) {
        for p in self.pending.iter_mut().flatten() {
            if p.ev.launch.mint == ev.mint && ev.slot >= p.ev.slot && p.migrated.is_none() {
                let sharing = decode::sharing_config_of(&ev.mint);
                p.migrated = Some((sharing, decide::creator_vault_of(&sharing)));
                feeds::MIGRATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// UDP: the create's batch has been read, its bundle buys are in: decide now.
    pub fn on_batch_end(&mut self, mint: &Pubkey) {
        let now = now_ns();
        for i in 0..MAX_PENDING {
            if self.pending[i].as_ref().is_some_and(|p| p.at_batch_end && p.ev.launch.mint == *mint) {
                let p = self.pending[i].take().expect("checked");
                self.fire(&p, now);
            }
        }
    }

    /// Decides every create whose window has closed; reports late-buy counts that are done.
    pub fn tick(&mut self, now: u64) {
        for slot in self.late.iter_mut() {
            if let Some(l) = slot.filter(|l| now.saturating_sub(l.closed_at) >= LATE_NS) {
                verify::note_late(&l.mint, l.buys, l.lamports);
                *slot = None;
            }
        }
        for i in 0..MAX_PENDING {
            if self.pending[i].as_ref().is_some_and(|p| now >= p.deadline) {
                let p = self.pending[i].take().expect("checked");
                self.fire(&p, now);
            }
        }
    }

    fn fire(&mut self, p: &Pending, t_decide_start: u64) {
        let l = &p.ev.launch;
        let mut later = [Buy { mint: Pubkey::default(), tokens: 0, lamports: 0, buyer: None, cu_price: 0 }; MAX_LATER];
        for (i, b) in p.buys[..p.n].iter().enumerate() {
            later[i] = b.expect("counted");
        }
        let outcome = match &p.pre {
            Some(pre) => decide::decide_prepared(l, &p.derived, p.ev.slot, &later[..p.n], now_unix(), pre),
            None => decide::decide(l, &p.derived, p.ev.slot, &later[..p.n], now_unix()),
        };
        let t_decided = now_ns();
        let mut stamp = Stamp {
            mint: l.mint.to_bytes(),
            feed: p.ev.feed,
            mode: self.mode,
            flags: 0,
            reject: outcome.reject_code(),
            window_ns: if matches!(p.ev.feed, Feed::Preprocessed | Feed::Grpc) { WINDOW_NS } else { 0 },
            t_recv: p.ev.t_recv,
            t_shred_recv: p.ev.t_shred,
            t_deshred: p.ev.t_deshred,
            t_matched: p.ev.t_matched,
            t_picked: p.t_picked,
            t_derived: p.t_derived,
            t_decide_start,
            t_decided,
            ..Default::default()
        };
        let decision = match &outcome {
            Outcome::Buy(..) => Decision::Bought,
            Outcome::Unknown(..) => Decision::Left,
            Outcome::Reject(r, _) => Decision::of_reject(*r),
        };
        let (mut plan, facts, rules_ok) = match outcome {
            Outcome::Buy(plan, facts) => {
                stamp.flags |= FLAG_BUY;
                (Some(plan), Some(facts), true)
            }
            Outcome::Unknown(_, facts) => {
                stamp.flags |= FLAG_UNKNOWN;
                (None, Some(facts), true)
            }
            Outcome::Reject(decide::Reject::Rules(_), facts) => (None, facts, false),
            Outcome::Reject(_, facts) => (None, facts, facts.is_some()),
        };
        verify::note_fast(&l.mint, facts, rules_ok);
        let late = Late { mint: l.mint, slot: p.ev.slot, closed_at: t_decided, buys: 0, lamports: 0, sigs: [([0; 8], 0); 16] };
        match self.late.iter_mut().find(|s| s.is_none()) {
            Some(s) => *s = Some(late),
            // Full: the oldest one is reported now.
            None => {
                let oldest = self.late.iter_mut().min_by_key(|s| s.map_or(0, |l| l.closed_at)).expect("non-empty");
                if let Some(o) = oldest.take() {
                    verify::note_late(&o.mint, o.buys, o.lamports);
                }
                *oldest = Some(late);
            }
        }
        // Live: the position is taken before the send (the slow path counts it at once).
        let mut position_slot = None;
        let mut decision = decision;
        // The fee engine's fill-failure breaker pauses live buys until the limit is re-simulated.
        if self.send == SendMode::Live && plan.is_some() && super::fee::paused() {
            plan = None;
            stamp.flags &= !FLAG_BUY;
            stamp.reject = decide::Reject::Positions.code();
            decision = Decision::Declined;
        }
        let tier = match (&plan, &facts) {
            (Some(p), Some(f)) => super::fee::tier(p.dev_buy_lamports, Some(p.follow), f.mayhem),
            (None, Some(f)) => super::fee::tier(f.dev_buy_lamports, f.follow, f.mayhem),
            _ => 0,
        };
        // The fee engine's route for the create slot's leader class may say not to buy (Harmonic
        // over its cap, a fee test's skip arm), in paper as live.
        if plan.is_some() && super::fee::skips(p.ev.slot) {
            plan = None;
            stamp.flags &= !FLAG_BUY;
            stamp.reject = decide::Reject::FeeRoute.code();
            decision = Decision::Declined;
        }
        // The distance rule: the create slot's leader answers under DISTANCE_MAX_RTT_MS, and in its
        // last slot with a far next leader the create came early in the slot.
        let t_create = if p.ev.t_shred > 0 { p.ev.t_shred.min(p.ev.t_recv) } else { p.ev.t_recv };
        super::distance::note_create(&stamp.mint, t_create);
        if plan.is_some() && super::distance::verdict(p.ev.slot, t_create) != super::distance::Verdict::Go {
            plan = None;
            stamp.flags &= !FLAG_BUY;
            stamp.reject = decide::Reject::Distance.code();
            decision = Decision::Declined;
        }
        if self.send == SendMode::Live && plan.is_some() {
            match common::try_reserve_position(rules::max_open_positions()) {
                Some(s) => position_slot = Some(s),
                None => {
                    plan = None;
                    stamp.flags &= !FLAG_BUY;
                    stamp.reject = decide::Reject::Positions.code();
                    decision = Decision::Declined;
                }
            }
            // The snipe test's caps: the entry is taken here, before the bytes go out.
            if plan.is_some() && crate::utils::snipe_test::on() {
                if let Err(t) = crate::utils::snipe_test::try_enter(now_unix()) {
                    plan = None;
                    position_slot = None;
                    stamp.flags &= !FLAG_BUY;
                    stamp.reject = decide::Reject::Test(t).code();
                    decision = Decision::Declined;
                }
            }
        }
        let send_now = match self.send {
            SendMode::Off => false,
            SendMode::Dry => plan.is_some() || p.sample,
            SendMode::Live => plan.is_some(),
        };
        let mut built = None;
        // HOT_NONCE_FANOUT: each lane's copy (signature, tip) and the nonce account used.
        let mut copies: [Option<solana_signature::Signature>; super::stamps::LANES] = [None; super::stamps::LANES];
        let mut copy_tips = [0u64; super::stamps::LANES];
        let mut nonce_used: Option<(u8, u64)> = None;
        if plan.is_some() || self.shadow || p.sample {
            if plan.is_none() {
                stamp.flags |= if p.sample { FLAG_SAMPLE } else { FLAG_SHADOW };
            }
            if let (Some((blockhash, _, _)), Ok(config)) = (blockhash::current(), pumpfun_config()) {
                let pick = self.next_rand();
                let d = &p.derived;
                let (lamports_in, min_out) = plan.map_or((SHADOW_LAMPORTS, 1), |p| (p.lamports_in, p.min_out));
                let rival = rival_bid(l, &later[..p.n], &self.wallet);
                // A1: the measured limit of the coin's token program (the spec's until measured).
                let cu_limit = match super::cu::limit_for(&l.token_program) {
                    0 => self.template.spec.cu_limit,
                    n => n,
                };
                // Part B: the fee engine's quote (table or fee-test arm): the bid and the lanes the
                // route names (a Jito bundle alone, or the class's gateways); without it, the lanes'
                // bid and the old sending (LANE_ROTATE's turn, or every lane).
                let gateway_floor = self.lanes.as_ref().map_or(0, |x| x.cfg.lanes[lanes::LANE_ZEROSLOT].tip.max(x.cfg.lanes[lanes::LANE_EVERSTAKE].tip));
                let quote = super::fee::quote(p.ev.slot, tier, cu_limit, rival, &l.mint.to_bytes(), gateway_floor);
                let (cu_price, raised) = match (&quote, &self.lanes) {
                    (Some(q), _) => (q.cu_price, q.raised),
                    (None, Some(lanes)) => lanes::bid(&lanes.cfg, cu_limit, rival),
                    (None, None) => (TRADE_SETTING.priority_fee_micro_lamports, false),
                };
                let mask = quote.map_or(0, |q| q.lanes);
                let jito_bit = super::fee::lane_bit(lanes::LANE_JITO);
                // HOT_NONCE_FANOUT: a free nonce account for this buy's copies (none: as before).
                let grant = if send_now && self.lanes.is_some() && self.nonce_template.is_some() {
                    self.nonce_ends.as_mut().and_then(|(g, _)| g.pop())
                } else {
                    None
                };
                // The lanes that carry this buy, a bit each; `turn` is LANE_ROTATE's pick.
                let mut carry: u8 = 0;
                let mut turn = None;
                if let Some(lanes) = self.lanes.as_mut().filter(|_| send_now) {
                    if grant.is_some() {
                        // Every lane of the route (no rotation), each with its own copy.
                        let allowed = if mask == 0 { u8::MAX } else { mask };
                        carry = (0..super::stamps::LANES).filter(|&i| allowed & super::fee::lane_bit(i) != 0 && lanes.is_on(i)).fold(0, |m, i| m | super::fee::lane_bit(i));
                    } else if mask == 0 {
                        if lanes.cfg.rotate {
                            turn = lanes.next_turn();
                            carry = turn.map_or(0, |(lane, _)| super::fee::lane_bit(lane));
                        }
                    } else {
                        let gateways = mask & !jito_bit;
                        if lanes.cfg.rotate && gateways.count_ones() > 1 {
                            turn = lanes.next_turn_in(gateways);
                            carry = turn.map_or(0, |(lane, _)| super::fee::lane_bit(lane));
                        } else {
                            carry = (0..super::stamps::LANES).filter(|&i| gateways & super::fee::lane_bit(i) != 0 && lanes.is_on(i)).fold(0, |m, i| m | super::fee::lane_bit(i));
                        }
                        if mask & jito_bit != 0 && lanes.is_on(lanes::LANE_JITO) {
                            carry |= jito_bit;
                        }
                    }
                }
                let jito_tip = quote.and_then(|q| q.jito_tip).filter(|_| carry & jito_bit != 0);
                // The tips paid: each carrying lane's own (the Jito bundle's from the quote); with
                // every lane carrying it (no quote, no rotation), every lane's.
                let paid = match &self.lanes {
                    Some(lanes) if carry != 0 => {
                        let mut a = [0u64; MAX_TIPS];
                        for lane in 0..super::stamps::LANES {
                            if carry & super::fee::lane_bit(lane) == 0 {
                                continue;
                            }
                            if let Some(i) = lanes.cfg.tip_index(lane) {
                                a[i as usize] = match (lane, jito_tip) {
                                    (lanes::LANE_JITO, Some(tip)) => tip,
                                    _ => quote.and_then(|q| q.tip).unwrap_or(lanes.cfg.lanes[lane].tip),
                                };
                            }
                        }
                        Some(a)
                    }
                    _ => None,
                };
                let fill = BuyFill {
                    mint: l.mint,
                    token_program: l.token_program,
                    bonding_curve: d.bonding_curve,
                    associated_bonding_curve: d.associated_bonding_curve,
                    user_ata: d.user_ata,
                    creator_vault: p.migrated.map_or(d.creator_vault, |(_, vault)| vault),
                    bonding_curve_v2: d.bonding_curve_v2,
                    fee_recipient: config.fee_recipients[(pick as usize) % config.fee_recipients.len().max(1)],
                    buyback_fee_recipient: config.buyback_fee_recipients[((pick >> 16) as usize) % config.buyback_fee_recipients.len().max(1)],
                    lamports_in,
                    min_out,
                    cu_price,
                    // A DRY_RUN copy that goes out can never land: no blockhash is all zeros.
                    blockhash: if send_now && self.send == SendMode::Dry { solana_hash::Hash::default() } else { blockhash },
                    tip_pick: (pick >> 32) as u32,
                    paid,
                    cu_limit,
                };
                if let Some(g) = grant {
                    // HOT_NONCE_FANOUT: one copy per carrying lane, all on `g`'s nonce, each paying
                    // only its own lane's tip; at most one lands. Threaded lanes are handed theirs
                    // first (a push each), then the direct writes. A DRY_RUN copy keeps a zero
                    // blockhash too (it is already unlandable: the paper key, a synthetic account).
                    let value = if self.send == SendMode::Dry { solana_hash::Hash::default() } else { g.value };
                    let all_tips = paid.unwrap_or([0; MAX_TIPS]);
                    let copy = self.copy_seq;
                    self.copy_seq += 1;
                    let mut first = true;
                    for threaded in [true, false] {
                        for lane in FANOUT_ORDER {
                            let Some(lanes) = self.lanes.as_ref() else { break };
                            if carry & super::fee::lane_bit(lane) == 0 || lanes.is_threaded(lane) != threaded {
                                continue;
                            }
                            let mut own = [0u64; MAX_TIPS];
                            if let Some(i) = lanes.cfg.tip_index(lane) {
                                own[i as usize] = all_tips[i as usize];
                            }
                            let copy_fill = BuyFill { blockhash: value, paid: Some(own), ..fill };
                            if let Some(t) = self.nonce_template.as_mut() {
                                t.fill(&copy_fill);
                                t.set_nonce_account(&g.account);
                            }
                            let n = self.sign_nonce_tx();
                            if first {
                                stamp.t_signed = now_ns();
                            }
                            let m = base64_into(&self.tx[..n], &mut self.b64);
                            if first {
                                stamp.t_ready = now_ns();
                                first = false;
                            }
                            if let Some(lanes) = self.lanes.as_mut() {
                                lanes.send_turn(lane, &stamp.mint, &self.tx[..n], &self.b64[..m], &mut stamp.t_sent);
                            }
                            let mut sig = [0u8; 64];
                            sig.copy_from_slice(&self.tx[1..65]);
                            copies[lane] = Some(solana_signature::Signature::from(sig));
                            copy_tips[lane] = own.iter().sum();
                        }
                    }
                    let sent = self.send == SendMode::Live && copies.iter().any(Option::is_some);
                    if let Some((_, used)) = self.nonce_ends.as_mut() {
                        let _ = used.push(super::nonces::Used { index: g.index, copy, sent });
                    }
                    nonce_used = Some((g.index, copy));
                } else {
                    self.template.fill(&fill);
                    let n = self.sign_tx();
                    stamp.t_signed = now_ns();
                    let m = base64_into(&self.tx[..n], &mut self.b64);
                    stamp.t_ready = now_ns();
                    if send_now {
                        if let Some(lanes) = self.lanes.as_mut() {
                            if carry == 0 {
                                lanes.send(&stamp.mint, &self.tx[..n], &self.b64[..m], &mut stamp.t_sent);
                            } else {
                                // Jito first: its thread starts while the gateways are written.
                                if carry & jito_bit != 0 {
                                    lanes.send_turn(lanes::LANE_JITO, &stamp.mint, &self.tx[..n], &self.b64[..m], &mut stamp.t_sent);
                                }
                                for lane in 0..super::stamps::LANES {
                                    if lane != lanes::LANE_JITO && carry & super::fee::lane_bit(lane) != 0 {
                                        lanes.send_turn(lane, &stamp.mint, &self.tx[..n], &self.b64[..m], &mut stamp.t_sent);
                                    }
                                }
                            }
                        }
                    }
                }
                let tips_paid: u64 = if nonce_used.is_some() { copy_tips.iter().copied().max().unwrap_or(0) } else { paid.map_or(0, |a| a.iter().sum()) };
                built = Some((cu_price, raised, rival, carry, cu_limit, paid.map(|_| tips_paid), quote.and_then(|q| q.arm), turn.is_some()));
            }
        }
        stamps::record(&stamp);

        // The bytes are out: the rest may take its time.
        if self.send == SendMode::Live {
            if plan.is_some() && built.is_none() {
                // Not built (no blockhash or no pump.fun config yet): the slow path tries.
                decision = Decision::Left;
            }
            live::note(&l.mint, decision);
        }
        let kind = match (self.send, plan.is_some()) {
            (SendMode::Live, true) => Some(Kind::Live),
            (SendMode::Live, false) => None,
            (_, true) => Some(Kind::Paper),
            (SendMode::Dry, false) if p.sample => Some(Kind::Sample),
            _ => None,
        };
        if let (Some(kind), Some((cu_price, raised, rival, carry, cu_limit, tips_paid, fee_arm, _rotated)), Some(handoff)) = (kind, built, self.handoff.as_ref()) {
            let mut signature = [0u8; 64];
            signature.copy_from_slice(&self.tx[1..65]);
            let signature = copies.iter().flatten().next().copied().unwrap_or(solana_signature::Signature::from(signature));
            let sent = send_now && self.lanes.is_some();
            let _ = handoff.send(HotBuy {
                kind,
                launch: *l,
                plan,
                facts,
                bonding_curve: p.derived.bonding_curve,
                // The curve's creator the position's sells pay (moved to fee sharing, or not).
                creator: p.migrated.map_or(p.derived.creator, |(sharing, _)| sharing),
                create_slot: p.ev.slot,
                signature,
                copies,
                copy_tips,
                nonce: nonce_used,
                cu_price,
                cu_limit,
                tips: match (sent, tips_paid) {
                    (false, _) => 0,
                    (true, Some(t)) => t,
                    (true, None) => self.lanes.as_ref().map_or(0, |x| x.cfg.tips_total()),
                },
                tier,
                fee_arm,
                sent_unix_ms: crate::utils::dashboard::now_unix_ms(),
                rival_cu_price: rival,
                raised,
                lanes: std::array::from_fn(|i| {
                    sent && (carry == 0 || carry & super::fee::lane_bit(i) != 0) && lanes::counters::ON[i].load(std::sync::atomic::Ordering::Relaxed)
                }),
                t_recv: p.ev.t_recv,
                t_ready: stamp.t_ready,
                position_slot: position_slot.take(),
            });
        }
        // A position taken for a buy that did not go out is given back here (drop).
        drop(position_slot);
        // bundle10's shadow, after the existing decision and its send: nothing here is sent.
        if self.b10.is_some() {
            self.b10_report(p, &later[..p.n], stamp.reject, stamp.flags & FLAG_BUY != 0, tier, t_decide_start);
        }
    }

    /// bundle10's view of `p` at its window's end, to the shadow log's ring. Never waits: a full
    /// ring drops the record (counted in `bundle10::DROPPED`).
    fn b10_report(&mut self, p: &Pending, later: &[Buy], existing_reject: u8, existing_bought: bool, tier: u8, t_decide_start: u64) {
        use super::bundle10::{self as b10, Facts, Miss, Rec, Stop};
        let t0 = now_ns();
        let l = &p.ev.launch;
        let Some(b) = self.b10.as_mut() else { return };
        let Some(w) = b.windows.iter_mut().find(|w| w.as_ref().is_some_and(|w| w.mint == l.mint && w.slot == p.ev.slot)).and_then(Option::take) else { return };
        let zero_slot_tip = b.zero_slot_tip;
        let count = &w.count;
        let config = decide::config_now();
        let (initial, fee_bps) = config.unwrap_or_default();
        // The existing hot path's count, on the existing window's buys (one per transaction).
        let (after_dev, hot_after, hot_count) = decide::bundle(l, &p.derived, initial, fee_bps, later);
        let post = count.post_bundle(after_dev, fee_bps);
        let launch = b10::spot(&initial);
        let post_mult = if launch > 0.0 { b10::spot(&post.reserves) / launch } else { f64::NAN };
        let facts = Facts {
            layout_known: !l.creator_set_onchain,
            // The decoder takes only SOL-quoted creates (`decode::Skip::QuoteNotSol`).
            sol_quote: true,
            initial: (initial.virtual_sol_reserves, initial.virtual_token_reserves),
            mayhem: l.mayhem,
            // From the create's event, in the shadow log.
            cashback: false,
            dev_buy_lamports: after_dev.real_sol_reserves,
            count: count.wallets(),
            overflow: count.overflow(),
            bundle_lamports: post.lamports,
            post_bundle_mult: post_mult,
        };
        let miss = if config.is_none() { Miss::Config.code() } else { b10::rule(&facts, crate::config::bundle10::settings()).err().map_or(0, Miss::code) };
        let t_create = if p.ev.t_shred > 0 { p.ev.t_shred.min(p.ev.t_recv) } else { p.ev.t_recv };
        let setting = &*TRADE_SETTING;
        let cu_limit = match super::cu::limit_for(&l.token_program) {
            0 => self.template.spec.cu_limit,
            n => n,
        };
        let rival = rival_bid(l, later, &self.wallet);
        let (buy_cu_price, buy_tip) = super::fee::peek(p.ev.slot, tier, rival, zero_slot_tip).unwrap_or((setting.priority_fee_micro_lamports, zero_slot_tip));
        let mut sigs = [[0u8; 8]; b10::MAX_TXS];
        sigs[..count.txs()].copy_from_slice(count.sigs());
        let mut rec = Rec {
            mint: l.mint,
            deployer: l.deployer,
            creator: l.named_creator,
            slot: p.ev.slot,
            feed: p.ev.feed as u8,
            recv_unix_ms: crate::utils::dashboard::now_unix_ms().saturating_sub(t0.saturating_sub(p.ev.t_recv) / 1_000_000),
            window_ns: t_decide_start.saturating_sub(p.ev.t_recv),
            compute_ns: 0,
            mayhem: l.mayhem,
            token_2022: l.token_program == crate::utils::utils::TOKEN_2022_PROGRAM_ID,
            initial: (initial.virtual_sol_reserves, initial.virtual_token_reserves),
            fee_bps,
            dev_buy_lamports: after_dev.real_sol_reserves,
            count: count.wallets(),
            txs: count.txs() as u8,
            stop: Stop::code(count.stop()),
            unreadable: count.unreadable(),
            trades: count.trades(),
            overflow: count.overflow(),
            mixed: count.mixed(),
            sigs,
            bundle_lamports: post.lamports,
            largest: post.largest,
            post: post.reserves,
            post_mult,
            miss,
            hot_count,
            hot_bundle_lamports: hot_after.real_sol_reserves.saturating_sub(after_dev.real_sol_reserves),
            hot_buys: later.iter().filter(|b| b.mint == l.mint).count() as u32,
            existing_reject,
            existing_bought,
            fee_route_skip: super::fee::skips(p.ev.slot),
            far_producer: !setting.leader_regions.is_empty() && crate::utils::leaders::far_producer_skips(p.ev.slot, &setting.leader_regions),
            distance_go: super::distance::verdict(p.ev.slot, t_create) == super::distance::Verdict::Go,
            buy_cu_price,
            buy_cu_limit: cu_limit,
            buy_tip,
        };
        rec.compute_ns = now_ns().saturating_sub(t0);
        if let Some(b) = self.b10.as_mut() {
            if !b.out.push(rec) {
                b10::DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// The fan-out's lanes in the order their copies are made (threaded ones first in each pass: see
/// the send).
const FANOUT_ORDER: [usize; super::stamps::LANES] =
    [lanes::LANE_JITO, lanes::LANE_EVERSTAKE, lanes::LANE_ALLENHARK, lanes::LANE_TRITON, lanes::LANE_QUIC, lanes::LANE_ZEROSLOT, lanes::LANE_HELIUS, lanes::LANE_NOZOMI];

/// The highest priority bid among rivals' buys in the create's bundle window (not the deployer's,
/// the named creator's, the create's fee payer's or ours).
fn rival_bid(l: &Launch, later: &[Buy], wallet: &Pubkey) -> u64 {
    later
        .iter()
        .filter(|b| b.buyer.is_some_and(|u| u != l.deployer && u != l.named_creator && Some(u) != l.fee_payer && u != *wallet))
        .map(|b| b.cu_price)
        .max()
        .unwrap_or(0)
}

/// Where a feed thread hands its launches.
enum Sink<'a> {
    Inline(&'a mut Decider),
    Spsc(&'a mut spsc::Producer<Ev>, &'a Thread),
}

impl Sink<'_> {
    #[inline]
    fn create(&mut self, ev: CreateEv) {
        match self {
            Sink::Inline(d) => d.on_create(ev, ev.t_matched),
            Sink::Spsc(p, t) => {
                if p.push(Ev::Create(ev)) {
                    t.unpark();
                }
            }
        }
    }
    #[inline]
    fn buy(&mut self, ev: BuyEv) {
        match self {
            Sink::Inline(d) => d.on_buy(ev),
            Sink::Spsc(p, t) => {
                if p.push(Ev::Buy(ev)) {
                    t.unpark();
                }
            }
        }
    }
    #[inline]
    fn trade(&mut self, ev: TradeEv) {
        match self {
            Sink::Inline(d) => d.on_trade(ev),
            Sink::Spsc(p, t) => {
                if p.push(Ev::Trade(ev)) {
                    t.unpark();
                }
            }
        }
    }
    #[inline]
    fn migration(&mut self, ev: MigrationEv) {
        match self {
            Sink::Inline(d) => d.on_migration(ev),
            Sink::Spsc(p, t) => {
                if p.push(Ev::CreatorMigration(ev)) {
                    t.unpark();
                }
            }
        }
    }
    #[inline]
    fn batch_end(&mut self, mint: Pubkey) {
        match self {
            Sink::Inline(d) => d.on_batch_end(&mint),
            Sink::Spsc(p, t) => {
                if p.push(Ev::BatchEnd(mint)) {
                    t.unpark();
                }
            }
        }
    }
    #[inline]
    fn tick(&mut self) {
        if let Sink::Inline(d) = self {
            d.tick(now_ns());
        }
    }
    /// While idle: the decider's warm-up when it runs on this thread.
    #[inline]
    fn warm(&mut self, now: u64) {
        if let Sink::Inline(d) = self {
            d.warm(now);
        }
    }
}

/// Recent frame signatures (first 8 bytes), so a frame sent twice counts once: direct-mapped,
/// one lookup per frame.
pub struct RecentSigs {
    sigs: Box<[u64]>,
    /// bundle10 (BUNDLE10_SHADOW): the creates this feed thread saw last (mint, slot, arrival),
    /// so that only their coins' trades go to the decider.
    b10: [(Pubkey, u64, u64); 8],
    b10_next: usize,
}

const RECENT_SLOTS: usize = 1 << 14;

impl RecentSigs {
    pub fn new() -> Box<Self> {
        Box::new(RecentSigs { sigs: vec![0u64; RECENT_SLOTS].into_boxed_slice(), b10: [(Pubkey::default(), 0, 0); 8], b10_next: 0 })
    }
    fn first_time(&mut self, sig: [u8; 8]) -> bool {
        let v = u64::from_le_bytes(sig);
        let slot = &mut self.sigs[(v as usize) & (RECENT_SLOTS - 1)];
        if *slot == v {
            return false;
        }
        *slot = v;
        true
    }
    fn b10_note(&mut self, mint: Pubkey, slot: u64, t_recv: u64) {
        self.b10[self.b10_next] = (mint, slot, t_recv);
        self.b10_next = (self.b10_next + 1) % self.b10.len();
    }
    fn b10_wants(&self, mint: Option<&Pubkey>, slot: u64, t_recv: u64) -> bool {
        self.b10.iter().any(|(m, s, t)| *s == slot && *t > 0 && t_recv.saturating_sub(*t) <= B10_FEED_NS && mint.is_none_or(|x| x == m))
    }
}

const PP_HEADER: usize = 1 + 8 + 64;

/// One preprocessed frame: buys go to the open windows first, then the create opens its own (as
/// `preprocessed::frame` does: a create's dev buy is not its own bundle).
fn handle_pp_frame(bytes: &[u8], t_recv: u64, recent: &mut RecentSigs, sink: &mut Sink<'_>, feed: Feed) -> bool {
    let (Some(slot), Some(sig)) = (bytes.get(1..9).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes), bytes.get(9..17)) else { return false };
    super::distance::note_seen(slot, t_recv);
    if !recent.first_time(sig.try_into().expect("8")) {
        return false;
    }
    let Some(tx) = bytes.get(PP_HEADER..).and_then(Tx::parse) else { return false };
    let grpc = feed == Feed::Grpc;
    (if grpc { &feeds::GRPC_FRAMES } else { &feeds::PP_FRAMES }).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sig8: [u8; 8] = sig.try_into().expect("8");
    let mut seq = 0u8;
    decode::buys(&tx, |buy| {
        sink.buy(BuyEv { buy, slot, sig: sig8, seq });
        seq = seq.wrapping_add(1);
    });
    // bundle10 (BUNDLE10_SHADOW): the trades of coins created in this slot a moment ago.
    let b10 = super::bundle10::FEED.load(std::sync::atomic::Ordering::Relaxed);
    if b10 && recent.b10_wants(None, slot, t_recv) {
        decode::trades(&tx, |seq, trade| {
            if recent.b10_wants(Some(&trade.mint), slot, t_recv) {
                sink.trade(TradeEv { trade, slot, sig: sig8, seq });
            }
        });
    }
    let creates = decode::launches(
        &tx,
        |launch| {
            if b10 {
                recent.b10_note(launch.mint, slot, t_recv);
            }
            (if grpc { &feeds::GRPC_CREATES } else { &feeds::PP_CREATES }).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let t_matched = now_ns();
            sink.create(CreateEv { launch, slot, feed, t_recv, t_matched, t_shred: 0, t_deshred: 0 });
            parity::note(&launch.mint, slot, feed, parity::View { t_recv, t_matched, ..Default::default() }, t_matched);
        },
        |_| {},
    );
    // A creator's move to fee sharing (the create's own transaction or one of its bundle's).
    decode::creator_migrations(&tx, |mint| sink.migration(MigrationEv { mint, slot }));
    creates > 0
}

/// One UDP datagram from pumpfeed's entry push: the slot, then the pump.fun part of an entry batch.
fn handle_udp_batch(datagram: &[u8], t_recv: u64, ranges: &mut [(u32, u32)], sink: &mut Sink<'_>) {
    let Some(slot) = datagram.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes) else { return };
    feeds::UDP_BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::distance::note_seen(slot, t_recv);
    if handle_entry_batch(slot, &datagram[8..], Feed::Udp, t_recv, 0, 0, ranges, sink).is_none() {
        feeds::UDP_BAD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The pump.fun transactions of one entry batch (a bincode `Vec<Entry>`), from either shred path:
/// each create, then its coin's buys later in the batch, then the batch's end. Both paths run this
/// on the same bytes (in-process applies pumpfeed's pump.fun filter first), so the decider gets the
/// same events from either. None when the batch does not parse.
#[allow(clippy::too_many_arguments)]
fn handle_entry_batch(slot: u64, payload: &[u8], feed: Feed, t_recv: u64, t_shred: u64, t_deshred: u64, ranges: &mut [(u32, u32)], sink: &mut Sink<'_>) -> Option<()> {
    let n = decode::entry_transactions(payload, ranges)?;
    for i in 0..n {
        let (at, len) = ranges[i];
        let Some(tx) = Tx::parse(&payload[at as usize..(at + len) as usize]) else { continue };
        let mut creates: [Option<Launch>; 4] = [None; 4];
        let mut k = 0;
        decode::launches(
            &tx,
            |l| {
                if k < 4 {
                    creates[k] = Some(l);
                    k += 1;
                }
            },
            |_| {},
        );
        for launch in creates.into_iter().flatten() {
            let counter = if feed == Feed::Inproc { &feeds::INPROC_CREATES } else { &feeds::UDP_CREATES };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let t_matched = now_ns();
            sink.create(CreateEv { launch, slot, feed, t_recv, t_matched, t_shred, t_deshred });
            let mut view = parity::View { t_recv, t_matched, ..Default::default() };
            let mut m = 0;
            for &(at2, len2) in &ranges[i + 1..n] {
                if let Some(other) = Tx::parse(&payload[at2 as usize..(at2 + len2) as usize]) {
                    let sig: [u8; 8] = other.signature()[..8].try_into().expect("8");
                    let mut seq = 0u8;
                    decode::buys(&other, |b| {
                        let this = seq;
                        seq = seq.wrapping_add(1);
                        if b.mint == launch.mint && m < MAX_UDP_LATER {
                            sink.buy(BuyEv { buy: b, slot, sig, seq: this });
                            m += 1;
                            view.later += 1;
                            view.later_lamports += b.lamports;
                            if b.buyer.is_some_and(|u| u != launch.deployer && u != launch.named_creator && Some(u) != launch.fee_payer) {
                                view.rival = view.rival.max(b.cu_price);
                            }
                        }
                    });
                }
            }
            // A move of the coin's creator to fee sharing, in its create's transaction or later in
            // the batch.
            for &(at2, len2) in &ranges[i..n] {
                if let Some(other) = Tx::parse(&payload[at2 as usize..(at2 + len2) as usize]) {
                    decode::creator_migrations(&other, |mint| {
                        if mint == launch.mint {
                            sink.migration(MigrationEv { mint, slot });
                        }
                    });
                }
            }
            // bundle10 (BUNDLE10_SHADOW): every trade of the coin later in the batch, in order.
            if super::bundle10::FEED.load(std::sync::atomic::Ordering::Relaxed) {
                for &(at2, len2) in &ranges[i + 1..n] {
                    if let Some(other) = Tx::parse(&payload[at2 as usize..(at2 + len2) as usize]) {
                        let sig: [u8; 8] = other.signature()[..8].try_into().expect("8");
                        decode::trades(&other, |seq, trade| {
                            if trade.mint == launch.mint {
                                sink.trade(TradeEv { trade, slot, sig, seq });
                            }
                        });
                    }
                }
            }
            sink.batch_end(launch.mint);
            parity::note(&launch.mint, slot, feed, view, t_matched);
        }
    }
    Some(())
}

// ---- the in-process shred engine (ENGINE_INPROC) ----------------------------------------------

/// Where the in-process engine reads shreds (env).
#[derive(Clone, Debug)]
pub struct InprocConfig {
    pub enabled: bool,
    /// The ShredStream proxy's output (one of its DEST_IP_PORTS).
    pub bind: Option<std::net::SocketAddr>,
    /// Packet capture of the proxy's input: interface and UDP ports (needs CAP_NET_RAW).
    pub capture: Option<(String, Vec<(u16, u16)>)>,
    /// Helius raw shreds (a paid seat sends them to this host's IP and port): a third input of
    /// the same decoder, raced shred by shred against AllenHark's (`shreds::source_race`).
    pub helius: Option<std::net::SocketAddr>,
    pub rcvbuf: usize,
    pub busy_poll_us: u32,
    pub recv_batch: usize,
}

impl InprocConfig {
    pub fn from_env() -> Self {
        let bind = env_opt("ENGINE_SHRED_BIND").unwrap_or_else(|| "127.0.0.1:20003".into());
        let capture = match (env_opt("ENGINE_CAPTURE_IFACE"), env_opt("ENGINE_CAPTURE_PORTS")) {
            (Some(iface), Some(ports)) if !iface.is_empty() => net::parse_port_ranges(&ports).map(|p| (iface, p)),
            _ => None,
        };
        InprocConfig {
            // On unless set to false (pumpfeed's UDP push, kept for this release only).
            enabled: !env_opt("ENGINE_INPROC").is_some_and(|v| v.eq_ignore_ascii_case("false")),
            bind: (!bind.is_empty()).then(|| bind.parse().ok()).flatten(),
            capture,
            helius: env_opt("ENGINE_HELIUS_SHRED_BIND").and_then(|a| a.trim().parse().ok()),
            rcvbuf: env_opt("ENGINE_RCVBUF_BYTES").and_then(|v| v.parse().ok()).unwrap_or(64 << 20),
            busy_poll_us: env_opt("ENGINE_BUSY_POLL_US").and_then(|v| v.parse().ok()).unwrap_or(50),
            recv_batch: 64,
        }
    }
}

pub static INPROC: LazyLock<InprocConfig> = LazyLock::new(InprocConfig::from_env);

/// Most transactions one pump.fun-filtered batch holds here (pumpfeed's push capped batches at
/// 64 KB; in-process ones are not capped).
const INPROC_TXS: usize = 4096;
/// Which path delivered each shred first: direct-mapped by (slot, index, kind).
const RACE_SLOTS: usize = 1 << 16;
const PUMP_PROGRAM: [u8; 32] = crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID.to_bytes();

/// The in-process engine's state, owned by its thread: slot reassembly, the filtered batch, the
/// transaction ranges and the path race. Built once; nothing on the common path allocates (a
/// Reed-Solomon recovery, on packet loss, does inside the library).
pub struct Inproc {
    deshredder: Deshredder,
    filtered: Vec<u8>,
    ranges: Box<[(u32, u32); INPROC_TXS]>,
    race: Box<[(u64, u32, u8, u64)]>,
    /// Per shred, when AllenHark's stream and Helius raw shreds first delivered it (only while
    /// Helius raw shreds arrive).
    source: Box<[(u64, u32, u64, u64)]>,
}

impl Default for Inproc {
    fn default() -> Self {
        Self::new()
    }
}

impl Inproc {
    pub fn new() -> Self {
        Inproc {
            deshredder: Deshredder::new(8, 8192),
            filtered: Vec::with_capacity(1 << 20),
            ranges: Box::new([(0, 0); INPROC_TXS]),
            race: vec![(0u64, 0u32, 0u8, 0u64); RACE_SLOTS].into_boxed_slice(),
            source: vec![(0u64, 0u32, 0u64, 0u64); RACE_SLOTS].into_boxed_slice(),
        }
    }

    /// Data shreds rebuilt by Reed-Solomon so far.
    pub fn recovered(&self) -> u64 {
        self.deshredder.stats.recovered_shreds
    }

    #[inline]
    fn race(&mut self, path: usize, shred: &Shred<'_>, t: u64) {
        let tag = shred.index | if matches!(shred.kind, shreds::shred::ShredKind::Data { .. }) { 1 << 31 } else { 0 };
        let i = (shred.slot.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ tag as u64) as usize & (RACE_SLOTS - 1);
        let e = &mut self.race[i];
        if e.0 == shred.slot && e.1 == tag && e.2 != 0 {
            if e.2 != path as u8 + 1 {
                // The second copy: counted once.
                e.2 = 0;
            }
            return;
        }
        *e = (shred.slot, tag, path as u8 + 1, t);
        shreds::counters::FIRST[path].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// AllenHark's stream (paths 0 and 1) against Helius raw shreds (path 2), shred by shred.
    #[inline]
    fn source_race(&mut self, path: usize, shred: &Shred<'_>, t: u64) {
        if shreds::counters::PACKETS[2].load(std::sync::atomic::Ordering::Relaxed) == 0 {
            return;
        }
        let tag = shred.index | if matches!(shred.kind, shreds::shred::ShredKind::Data { .. }) { 1 << 31 } else { 0 };
        let i = (shred.slot.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ tag as u64) as usize & (RACE_SLOTS - 1);
        let e = &mut self.source[i];
        if e.0 != shred.slot || e.1 != tag {
            *e = (shred.slot, tag, 0, 0);
        }
        let helius = path == 2;
        let (mine, other) = if helius { (&mut e.3, e.2) } else { (&mut e.2, e.3) };
        if *mine != 0 {
            return;
        }
        *mine = t;
        if other != 0 {
            let class = super::clients::class_of_slot(shred.slot).unwrap_or(super::clients::Class::Other) as usize;
            shreds::source_race::note(class, !helius, t.saturating_sub(other));
        }
    }

    /// One shred packet from `path` (0: the proxy's output, 1: capture, 2: Helius raw shreds),
    /// off the socket at `t`.
    #[inline]
    fn on_packet(&mut self, path: usize, pkt: &[u8], t: u64, sink: &mut Sink<'_>, forward: Option<&Forward>) {
        use {shreds::counters as c, std::sync::atomic::Ordering::Relaxed};
        let Some(shred) = Shred::parse(pkt) else {
            c::BAD_PACKETS.fetch_add(1, Relaxed);
            return;
        };
        self.race(path, &shred, t);
        self.source_race(path, &shred, t);
        super::distance::note_seen(shred.slot, t);
        let Inproc { deshredder, filtered, ranges, .. } = self;
        deshredder.insert(&shred, t, &mut |info, batch| {
            if !info.complete {
                return;
            }
            let t_deshred = now_ns();
            c::BATCHES.fetch_add(1, Relaxed);
            feeds::INPROC_BATCHES.fetch_add(1, Relaxed);
            filtered.clear();
            filtered.extend_from_slice(&info.slot.to_le_bytes());
            match walk::filter_batch(batch, filtered, |tx| tx.has(&PUMP_PROGRAM)) {
                Some(n) if n > 0 => {
                    c::PUMP_BATCHES.fetch_add(1, Relaxed);
                    if handle_entry_batch(info.slot, &filtered[8..], Feed::Inproc, t, t, t_deshred, &mut ranges[..], sink).is_none() {
                        c::BAD_BATCHES.fetch_add(1, Relaxed);
                    }
                    if let Some(f) = forward {
                        // The slow path's copy (pumpfeed's datagram format), after the hot path.
                        let _ = f.send(filtered.clone());
                    }
                }
                Some(_) => {}
                None => {
                    c::BAD_BATCHES.fetch_add(1, Relaxed);
                }
            }
            if info.last_in_slot {
                if let Some(h) = walk::last_entry_hash(batch) {
                    book::on_slot_hash(info.slot, h, t_deshred);
                }
            }
        });
        c::RECOVERED_SHREDS.store(deshredder.stats.recovered_shreds, Relaxed);
        c::INCOMPLETE_SLOTS.store(deshredder.stats.incomplete_slots, Relaxed);
    }
}

/// The in-process shred thread: both inputs drained with `recvmmsg`, one deshredder.
fn run_inproc(cfg: InprocConfig, udp: Option<socket2::Socket>, capture: Option<socket2::Socket>, mut sink: Sink<'_>, forward: Option<Forward>, busy: bool) {
    use {shreds::counters as c, std::sync::atomic::Ordering::Relaxed};
    let mut rxs: Vec<(usize, net::PacketReceiver)> = Vec::new();
    if let Some(s) = udp {
        rxs.push((0, net::PacketReceiver::new(s, net::Framing::Udp, cfg.recv_batch)));
    }
    if let Some(s) = capture {
        rxs.push((1, net::PacketReceiver::new(s, net::Framing::Ip, cfg.recv_batch)));
    }
    if let Some(a) = cfg.helius {
        match net::bind_udp(a, &net::SocketOpts { rcvbuf: cfg.rcvbuf, busy_poll_us: cfg.busy_poll_us }) {
            Ok(s) => {
                info!("hot in-process: listening for Helius raw shreds on {a}");
                rxs.push((2, net::PacketReceiver::new(s, net::Framing::Udp, cfg.recv_batch)));
            }
            Err(e) => warn!("hot in-process: Helius raw shreds: bind {a}: {e}"),
        }
    }
    if rxs.is_empty() {
        warn!("hot in-process: no shred input; the thread ends");
        return;
    }
    let mut st = Inproc::new();
    info!("hot in-process: reading shreds on its own thread ({})", if busy { "busy-polling" } else { "polling" });
    loop {
        let mut got = 0;
        for (path, rx) in rxs.iter_mut() {
            let n = match rx.recv() {
                Ok(n) => n,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => 0,
                Err(e) => {
                    warn!("hot in-process: recv: {e}");
                    0
                }
            };
            if n == 0 {
                continue;
            }
            let t = now_ns();
            c::LAST_PACKET_NS.store(t, Relaxed);
            c::PACKETS[*path].fetch_add(n as u64, Relaxed);
            got += n;
            for i in 0..n {
                st.on_packet(*path, rx.packet(i), t, &mut sink, forward.as_ref());
            }
        }
        if got == 0 {
            sink.warm(now_ns());
            if busy {
                std::hint::spin_loop();
            } else {
                let socks: Vec<&socket2::Socket> = rxs.iter().map(|(_, rx)| rx.socket()).collect();
                net::wait_readable(&socks, 1);
            }
        }
        sink.tick();
    }
}

/// Opens the in-process engine's sockets (on the caller's thread, so a busy port or a missing
/// capability shows at startup).
fn open_inproc(cfg: &InprocConfig) -> (Option<socket2::Socket>, Option<socket2::Socket>) {
    let opts = net::SocketOpts { rcvbuf: cfg.rcvbuf, busy_poll_us: cfg.busy_poll_us };
    let udp = cfg.bind.and_then(|a| match net::bind_udp(a, &opts) {
        Ok(s) => {
            info!("hot in-process: listening for the proxy's shreds on {a}");
            Some(s)
        }
        Err(e) => {
            warn!("hot in-process: bind {a}: {e}");
            None
        }
    });
    let capture = cfg.capture.as_ref().and_then(|(iface, ports)| match net::capture_socket(iface, ports, &opts) {
        Ok(s) => {
            info!("hot in-process: capturing the proxy's input on {iface} ports {ports:?}");
            Some(s)
        }
        Err(e) => {
            warn!("hot in-process: capture on {iface} (needs CAP_NET_RAW): {e}");
            None
        }
    });
    (udp, capture)
}

/// The feed thread's warm-up: its last create frame, decoded again while idle (nothing is handed
/// on), so the decoder stays in the CPU caches between launches.
struct DecodeWarm {
    frame: Box<[u8; 4096]>,
    len: usize,
    every_ns: u64,
    last: u64,
}

impl DecodeWarm {
    fn new() -> Self {
        DecodeWarm { frame: Box::new([0; 4096]), len: 0, every_ns: CONFIG.warm_us * 1_000, last: 0 }
    }
    /// Keeps `bytes` (a frame or transaction that held a create) for later warm-ups.
    fn keep(&mut self, bytes: &[u8]) {
        if self.every_ns > 0 && bytes.len() <= self.frame.len() {
            self.frame[..bytes.len()].copy_from_slice(bytes);
            self.len = bytes.len();
        }
    }
    fn run(&mut self, now: u64, offset: usize) {
        if self.every_ns == 0 || self.len <= offset || now.saturating_sub(self.last) < self.every_ns {
            return;
        }
        self.last = now;
        if let Some(tx) = Tx::parse(&self.frame[offset..self.len]) {
            decode::buys(&tx, |b| {
                std::hint::black_box(b);
            });
            decode::launches(&tx, |l| {
                std::hint::black_box(l);
            }, |_| {});
        }
    }
}

/// Exponential back-off between reconnects, capped: 100 ms, 200 ms, 400 ms, then at least 1 s.
fn backoff(failures: u32) -> Duration {
    let ms = 100u64 << failures.min(6);
    Duration::from_millis(if failures >= 3 { ms.clamp(1_000, 30_000) } else { ms })
}

/// Hands a frame to the slow path (on the runtime: it spawns tasks).
type Forward = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// The hot feeds that run and how long ago each delivered (ns; None: nothing yet), for the live
/// guard's stall check (`utils::live_guard`).
pub fn feed_ages(now: u64) -> Vec<(&'static str, Option<u64>)> {
    use std::sync::atomic::Ordering::Relaxed;
    let age = |last: u64| (last > 0).then(|| now.saturating_sub(last));
    let mut out = Vec::with_capacity(2);
    if feeds::PP_ON.load(Relaxed) {
        // With the LaserStream backup on, the feed is down only when both are silent.
        let last = feeds::PP_LAST_NS.load(Relaxed).max(super::backup::stream_last_ns());
        out.push((if super::backup::enabled() { "preprocessed and LaserStream" } else { "preprocessed" }, age(last)));
    }
    if CONFIG.enabled && INPROC.enabled {
        out.push(("in-process shreds", age(super::shreds::counters::LAST_PACKET_NS.load(Relaxed))));
    }
    out
}

/// The LaserStream backup's thread: frames `hot::backup` builds from the regular stream while the
/// preprocessed stream is silent, through the same decoder (off the hot cores: it only works
/// during a failover).
fn run_backup(rx: std::sync::mpsc::Receiver<(Vec<u8>, u64)>, mut sink: Sink<'_>) {
    super::affinity::keep_off_hot_cores();
    let mut recent = RecentSigs::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(1)) {
            Ok((frame, t_recv)) => {
                handle_pp_frame(&frame, t_recv, &mut recent, &mut sink, Feed::Grpc);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        sink.tick();
    }
}

fn run_preprocessed(url: String, mut sink: Sink<'_>, forward: Forward, busy: bool) {
    feeds::PP_ON.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut recent = RecentSigs::new();
    let mut warm = DecodeWarm::new();
    let mut failures = 0u32;
    loop {
        let connected = tungstenite::connect(url.as_str());
        let mut ws = match connected {
            Ok((ws, _)) => ws,
            Err(e) => {
                failures += 1;
                warn!("hot preprocessed: connect: {}; retrying in {:?}", e.to_string().replace(&url, "<url>"), backoff(failures));
                std::thread::sleep(backoff(failures));
                continue;
            }
        };
        let tcp = match ws.get_mut() {
            tungstenite::stream::MaybeTlsStream::Rustls(s) => &s.sock,
            tungstenite::stream::MaybeTlsStream::Plain(s) => &*s,
            _ => {
                warn!("hot preprocessed: unexpected stream type");
                return;
            }
        };
        let _ = tcp.set_nodelay(true);
        // pump fees too: its create_fee_sharing_config (a creator's move to fee sharing) names
        // pump.fun only through a lookup table, which the stream's filter does not see.
        let subscribe = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "preprocessedSubscribe", "params": [{ "accountInclude": [
            crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID.to_string(),
            crate::instructions::pumpswap_buy_ix::PUMP_FEE_PROGRAM_ID.to_string(),
        ] }]});
        if let Err(e) = ws.send(tungstenite::Message::Text(subscribe.to_string().into())) {
            failures += 1;
            warn!("hot preprocessed: subscribe: {e}");
            std::thread::sleep(backoff(failures));
            continue;
        }
        info!("hot preprocessed: connected on its own thread ({})", if busy { "busy-polling" } else { "blocking reads" });
        failures = 0;
        let mut last_ping = now_ns();
        let mut nonblocking = false;
        loop {
            // Poll without blocking while a bundle window is open here (inline), or always when
            // busy-polling; otherwise block up to 2 ms at a time.
            let want = busy || matches!(&sink, Sink::Inline(d) if d.has_pending());
            if want != nonblocking {
                let tcp = match ws.get_mut() {
                    tungstenite::stream::MaybeTlsStream::Rustls(s) => &s.sock,
                    tungstenite::stream::MaybeTlsStream::Plain(s) => &*s,
                    _ => unreachable!(),
                };
                let _ = tcp.set_nonblocking(want);
                if !want {
                    let _ = tcp.set_read_timeout(Some(Duration::from_millis(2)));
                }
                nonblocking = want;
            }
            match ws.read() {
                Ok(tungstenite::Message::Binary(bytes)) => {
                    let t_recv = now_ns();
                    feeds::PP_LAST_NS.store(t_recv, std::sync::atomic::Ordering::Relaxed);
                    if handle_pp_frame(&bytes, t_recv, &mut recent, &mut sink, Feed::Preprocessed) {
                        warm.keep(&bytes);
                    }
                    let _ = forward.send(bytes.to_vec());
                }
                Ok(tungstenite::Message::Text(text)) => {
                    if text.contains("\"error\"") {
                        warn!("hot preprocessed: subscription refused: {text}");
                        break;
                    }
                }
                Ok(tungstenite::Message::Close(c)) => {
                    warn!("hot preprocessed: closed by the server: {c:?}");
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    let now = now_ns();
                    warm.run(now, PP_HEADER);
                    sink.warm(now);
                }
                Err(e) => {
                    warn!("hot preprocessed: {e}");
                    break;
                }
            }
            sink.tick();
            let now = now_ns();
            if now.saturating_sub(last_ping) > 20_000_000_000 {
                let _ = ws.send(tungstenite::Message::Ping(Vec::new().into()));
                last_ping = now;
            }
            let _ = ws.flush();
        }
        failures += 1;
        std::thread::sleep(backoff(failures));
    }
}

fn run_udp(port: u16, mut sink: Sink<'_>, forward: Forward, busy: bool) {
    let mut failures = 0u32;
    let mut buf = vec![0u8; 65_536];
    let mut ranges = Box::new([(0u32, 0u32); MAX_BATCH_TXS]);
    loop {
        let socket = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => s,
            Err(e) => {
                failures += 1;
                warn!("hot udp: bind {port}: {e}; retrying in {:?}", backoff(failures));
                std::thread::sleep(backoff(failures));
                continue;
            }
        };
        if busy {
            let _ = socket.set_nonblocking(true);
        } else {
            let _ = socket.set_read_timeout(Some(Duration::from_millis(2)));
        }
        info!("hot udp: listening on {port} on its own thread ({})", if busy { "busy-polling" } else { "blocking reads" });
        failures = 0;
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let t_recv = now_ns();
                    handle_udp_batch(&buf[..n], t_recv, &mut ranges[..], &mut sink);
                    let _ = forward.send(buf[..n].to_vec());
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    sink.warm(now_ns());
                    if busy {
                        std::hint::spin_loop();
                    }
                }
                Err(e) => {
                    warn!("hot udp: {e}");
                    break;
                }
            }
            sink.tick();
        }
        failures += 1;
        std::thread::sleep(backoff(failures));
    }
}

fn pin(core: usize, what: &str) {
    if CONFIG.pin {
        if affinity::pin_current_thread(core) {
            info!("hot {what}: pinned to core {core}");
        } else {
            warn!("hot {what}: could not pin to core {core} ({} cores)", affinity::core_count());
        }
    }
}

fn decider(shadow: bool, mode: Mode, send: SendMode, lane_cfg: &LaneConfig) -> anyhow::Result<Decider> {
    // Live: the wallet's key; DRY_RUN: the throwaway paper key (never the wallet's).
    let keypair = crate::config::hot_signer();
    let setting = &*TRADE_SETTING;
    // With lanes, the buy carries their tips; without (DRY_RUN, nothing sent), Part A's template.
    let spec = if send == SendMode::Off {
        spec_from_settings(keypair.pubkey())
    } else {
        TemplateSpec {
            payer: keypair.pubkey(),
            cu_limit: setting.compute_unit_limit,
            loaded_data_bytes: setting.loaded_data_limit_bytes.map(|b| b as u32),
            tips: lane_cfg.tips(),
            // The Jito bundle's tip transfer carries jitodontfront.
            dontfront: lane_cfg.tip_index(lanes::LANE_JITO),
        }
    };
    let nonced = (super::nonces::hot_fanout() && send != SendMode::Off).then(|| BuyTemplate::build_nonced(spec.clone()));
    let template = BuyTemplate::build(spec)?;
    let lanes = (send != SendMode::Off).then(|| Lanes::start(lane_cfg.clone()));
    let sample_ms = env_opt("HOT_DRY_SAMPLE_MS").and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let mut decider = Decider::new(template, keypair.clone(), shadow, mode).with_lanes(send, lanes, live::sender(), sample_ms);
    // bundle10's shadow log (BUNDLE10_SHADOW): this decider's ring to it.
    if crate::config::bundle10::shadow() {
        decider = decider.with_bundle10(crate::processors::bundle10_shadow::producer(), lane_cfg.lanes[lanes::LANE_ZEROSLOT].tip);
    }
    if CONFIG.presign {
        decider = decider.with_presigner(start_presign(&keypair));
    }
    // HOT_NONCE_FANOUT: one decider takes the manager's rings (the decision thread's, in SPSC mode).
    if let Some(nonced) = nonced {
        match (nonced, super::nonces::take_decider_ends()) {
            (Ok(t), Some(ends)) => {
                info!("hot nonce fan-out: each buy goes to every lane of its route as its own copy on one durable nonce, each paying only its lane's tip");
                decider = decider.with_nonce_fanout(t, ends);
            }
            (Err(e), _) => warn!("hot nonce fan-out: template: {e:#}; buys go out the usual way"),
            (Ok(_), None) => {}
        }
    }
    Ok(decider)
}

/// The presigned nonces' refill thread (HOT_PRESIGN): off the hot cores, a look at the queue every
/// millisecond; runs for the process's life. The hot thread's end comes back, filled.
fn start_presign(keypair: &Keypair) -> super::presign::Presigner {
    use super::presign;
    static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let (pre, mut refill) = presign::pair(Arc::new(presign::HotKey::of(keypair)), presign::CAPACITY);
    refill.fill();
    match std::thread::Builder::new().name("hot-presign".into()).spawn(move || {
        super::affinity::keep_off_hot_cores();
        refill.run(&STOP);
    }) {
        Ok(_) => log::info!("hot presign: {} nonces ready, refilled every millisecond off the hot cores", presign::CAPACITY),
        Err(e) => log::warn!("hot presign: refill thread failed to start ({e}); the ready nonces are used, then the keypair signs"),
    }
    pre
}

/// Starts the hot path when FAST_PATH=true: its feed threads replace the slow path's early-feed
/// readers and forward every frame to it. True when it took over the feeds.
pub fn start() -> bool {
    let cfg = &*CONFIG;
    if !cfg.enabled {
        return false;
    }
    let setting = &*TRADE_SETTING;
    let lane_cfg = LaneConfig::from_env();
    let send = if !setting.dry_run {
        // Real buys from the hot path need a second, explicit switch.
        if !env_opt("HOT_LIVE").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            warn!("FAST_PATH is on and DRY_RUN is off, but HOT_LIVE is not true: the hot path stays off and the slow path buys as before");
            return false;
        }
        SendMode::Live
    } else if lane_cfg.dry_lanes {
        SendMode::Dry
    } else {
        SendMode::Off
    };
    super::clock::init();
    // bundle10's shadow: the feed threads hand over the trades its count needs.
    if crate::config::bundle10::shadow() {
        super::bundle10::FEED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    live::init();
    if send == SendMode::Live {
        live::set_live();
    }
    live::spawn();
    positions::spawn();
    super::clients::spawn();
    super::cu::spawn();
    super::fee::spawn();
    super::distance::spawn();
    stamps::spawn_reporter();
    // Every table the hot path touches is built now, not on the first launch.
    verify::init();
    LazyLock::force(&SEEN);
    // The slow path's readers, fed from the hot threads.
    let (pp_tx, mut pp_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let (udp_tx, mut udp_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(frame) = pp_rx.recv().await {
            super::bids::note_pp_frame(&frame);
            crate::processors::position_feed::observe_preprocessed(&frame);
            if crate::processors::preprocessed::frame(&frame) {
                tokio::task::yield_now().await;
            }
        }
    });
    tokio::spawn(async move {
        while let Some(datagram) = udp_rx.recv().await {
            if let Some((slot, payload)) = crate::processors::shred_feed::udp_batch(&datagram) {
                super::bids::note_entries(slot, payload);
                if crate::processors::shred_feed::handle_batch(slot, payload) {
                    tokio::task::yield_now().await;
                }
            }
        }
    });

    let build = |what: &str| match decider(cfg.shadow, cfg.mode, send, &lane_cfg) {
        Ok(d) => Some(d),
        Err(e) => {
            warn!("hot {what}: buy template: {e:#}; the hot path stays off");
            None
        }
    };
    let pp_url = setting.preprocessed_url.clone();
    let udp_port = setting.shred_udp_port;
    let busy = cfg.busy_poll;
    // The in-process shred engine. With pumpfeed's UDP push on as well, both feed the hot path
    // and the parity log compares them create by create.
    let inproc = &*INPROC;
    let inproc_socks = inproc.enabled.then(|| open_inproc(inproc)).filter(|(u, c)| u.is_some() || c.is_some());
    if inproc.enabled && inproc_socks.is_none() {
        warn!("hot in-process: no shred input opened; it stays off");
    }
    if inproc_socks.is_some() && udp_port.is_some() {
        parity::init();
        parity::enable();
        info!("engine parity: pumpfeed's UDP push and the in-process engine both feed the hot path (logs/engine_parity.csv)");
    } else if inproc_socks.is_some() && pp_url.is_some() {
        // The feed race: each create's arrival on Helius's preprocessed stream and on the shreds.
        parity::init();
        parity::enable();
        info!("feed race: every create's arrival on Helius's preprocessed stream and on the in-process shreds, with the slot leader's client (logs/engine_parity.csv)");
    }
    // The slow path gets each batch once: from the UDP push while it runs, else from in-process.
    let inproc_forward = udp_port.is_none().then(|| udp_tx.clone());
    let inproc_core = cfg.inproc_core.or(udp_port.is_none().then_some(cfg.cores[1]));
    let pin_inproc = move || match inproc_core {
        Some(core) => pin(core, "in-process shred thread"),
        None => info!("hot in-process: no core of its own (HOT_CORES has no fourth entry while the UDP feed runs); it stays off the hot cores"),
    };
    // The LaserStream backup (`hot::backup`): its own feed thread, used while the preprocessed
    // stream is silent.
    let backup_rx = (pp_url.is_some() && super::backup::enabled()).then(super::backup::install);
    match cfg.mode {
        Mode::Inline => {
            if let Some(rx) = backup_rx {
                let Some(mut d) = build("backup") else { return false };
                std::thread::Builder::new().name("hot-backup".into()).spawn(move || run_backup(rx, Sink::Inline(&mut d))).expect("hot thread");
            }
            if let Some(url) = pp_url {
                let Some(mut d) = build("preprocessed") else { return false };
                let tx = pp_tx.clone();
                std::thread::Builder::new()
                    .name("hot-preprocessed".into())
                    .spawn(move || {
                        pin(cfg.cores[0], "preprocessed feed");
                        run_preprocessed(url, Sink::Inline(&mut d), tx, busy);
                    })
                    .expect("hot thread");
            }
            if let Some(port) = udp_port {
                let Some(mut d) = build("udp") else { return false };
                let tx = udp_tx.clone();
                std::thread::Builder::new()
                    .name("hot-udp".into())
                    .spawn(move || {
                        pin(cfg.cores[1], "udp feed");
                        run_udp(port, Sink::Inline(&mut d), tx, busy);
                    })
                    .expect("hot thread");
            }
            if let Some((u, c)) = inproc_socks {
                let Some(mut d) = build("in-process") else { return false };
                let (icfg, fwd) = (inproc.clone(), inproc_forward.clone());
                std::thread::Builder::new()
                    .name("hot-inproc".into())
                    .spawn(move || {
                        pin_inproc();
                        run_inproc(icfg, u, c, Sink::Inline(&mut d), fwd, busy);
                    })
                    .expect("hot thread");
            }
        }
        Mode::Spsc => {
            let Some(mut d) = build("decision") else { return false };
            let (mut pp_prod, mut pp_cons) = spsc::channel::<Ev>(RING);
            let (mut udp_prod, mut udp_cons) = spsc::channel::<Ev>(RING);
            let (mut in_prod, mut in_cons) = spsc::channel::<Ev>(RING);
            let (mut bk_prod, mut bk_cons) = spsc::channel::<Ev>(RING);
            // Sells from the slow path, written on this thread's lanes (`hot::sell`, 2026-10-11).
            let (sell_prod, mut sell_cons) = spsc::channel::<super::sell::SendReq>(super::sell::RING);
            let decision = std::thread::Builder::new()
                .name("hot-decide".into())
                .spawn(move || {
                    pin(cfg.cores[2], "decision thread");
                    info!("hot decision thread: {}", if busy { "busy-polling" } else { "parking between events" });
                    loop {
                        let mut got = false;
                        for cons in [&mut pp_cons, &mut udp_cons, &mut in_cons, &mut bk_cons] {
                            while let Some(ev) = cons.pop() {
                                got = true;
                                match ev {
                                    Ev::Create(c) => d.on_create(c, now_ns()),
                                    Ev::Buy(b) => d.on_buy(b),
                                    Ev::BatchEnd(mint) => d.on_batch_end(&mint),
                                    Ev::Trade(t) => d.on_trade(t),
                                    Ev::CreatorMigration(m) => d.on_migration(m),
                                }
                            }
                        }
                        while let Some(r) = sell_cons.pop() {
                            got = true;
                            d.on_send(&r);
                        }
                        let now = now_ns();
                        d.tick(now);
                        if !got {
                            if d.warm(now) {
                                continue;
                            }
                            if busy {
                                std::hint::spin_loop();
                            } else {
                                let wait = d.next_deadline().map_or(1_000_000, |t| t.saturating_sub(now).max(1_000));
                                std::thread::park_timeout(Duration::from_nanos(wait));
                            }
                        }
                    }
                })
                .expect("hot thread");
            let decision_thread = decision.thread().clone();
            // Live: the sells themselves. DRY_RUN with the lanes on: the paper sells' unlandable
            // copies, to time the sell path in a paper run.
            if send != SendMode::Off {
                let tips = lane_cfg.tips();
                let lane_tips = std::array::from_fn(|lane| lane_cfg.enabled(lane).then(|| lane_cfg.tip_index(lane).map(|i| tips[i as usize])));
                super::sell::install(sell_prod, decision_thread.clone(), tips, lane_tips, CONFIG.presign.then(|| start_presign(&crate::config::hot_signer())));
                info!(
                    "hot sell: {} go through the lanes (HOT_SELL_LANES; presigned: {})",
                    if send == SendMode::Live { "sells" } else { "the paper sells' unlandable copies" },
                    CONFIG.presign
                );
            }
            if let Some(rx) = backup_rx {
                let t = decision_thread.clone();
                std::thread::Builder::new().name("hot-backup".into()).spawn(move || run_backup(rx, Sink::Spsc(&mut bk_prod, &t))).expect("hot thread");
            }
            if let Some(url) = pp_url {
                let t = decision_thread.clone();
                let tx = pp_tx.clone();
                std::thread::Builder::new()
                    .name("hot-preprocessed".into())
                    .spawn(move || {
                        pin(cfg.cores[0], "preprocessed feed");
                        run_preprocessed(url, Sink::Spsc(&mut pp_prod, &t), tx, busy);
                    })
                    .expect("hot thread");
            }
            if let Some(port) = udp_port {
                let t = decision_thread.clone();
                let tx = udp_tx.clone();
                std::thread::Builder::new()
                    .name("hot-udp".into())
                    .spawn(move || {
                        pin(cfg.cores[1], "udp feed");
                        run_udp(port, Sink::Spsc(&mut udp_prod, &t), tx, busy);
                    })
                    .expect("hot thread");
            }
            if let Some((u, c)) = inproc_socks {
                let t = decision_thread;
                let (icfg, fwd) = (inproc.clone(), inproc_forward.clone());
                std::thread::Builder::new()
                    .name("hot-inproc".into())
                    .spawn(move || {
                        pin_inproc();
                        run_inproc(icfg, u, c, Sink::Spsc(&mut in_prod, &t), fwd, busy);
                    })
                    .expect("hot thread");
            }
        }
    }
    info!(
        "hot path on: {:?} handoff, shadow builds {}, busy-poll {}, cores {:?}, warm-up {}; {}",
        cfg.mode,
        cfg.shadow,
        cfg.busy_poll,
        cfg.cores,
        if cfg.warm_us > 0 { format!("every {} us", cfg.warm_us) } else { "off".into() },
        match send {
            SendMode::Off => "DRY_RUN, nothing is sent".to_string(),
            SendMode::Dry => "DRY_RUN, unlandable copies (all-zero blockhash) of paper buys and samples go out on the lanes".to_string(),
            SendMode::Live => "LIVE: the hot path sends real buys".to_string(),
        }
    );
    true
}

/// For the benchmark and the allocation test: a decider with the bot's settings and `keypair`.
pub fn bench_decider(keypair: Arc<Keypair>, shadow: bool, mode: Mode) -> anyhow::Result<Decider> {
    let template = BuyTemplate::build(spec_from_settings(keypair.pubkey()))?;
    // Presigned as live (HOT_PRESIGN): the nonces filled here, no refill thread.
    let (pre, mut refill) = super::presign::pair(Arc::new(super::presign::HotKey::of(&keypair)), 1 << 16);
    refill.fill();
    Ok(Decider::new(template, keypair, shadow, mode).with_presigner(pre))
}

/// For the benchmark and the allocation test: one preprocessed frame through a decider inline
/// (`recent` is reused across calls, as on the feed thread).
pub fn bench_frame(d: &mut Decider, recent: &mut RecentSigs, frame: &[u8], t_recv: u64) {
    handle_pp_frame(frame, t_recv, recent, &mut Sink::Inline(d), Feed::Preprocessed);
}

/// For the benchmark and the allocation test: spins until every open bundle window has closed and
/// its create was decided and built.
pub fn bench_settle(d: &mut Decider) {
    while let Some(deadline) = d.next_deadline() {
        let now = now_ns();
        if now >= deadline {
            d.tick(now);
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Tests: forget every first sighting, so fixture coins count as new again.
#[cfg(test)]
pub fn forget_seen() {
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().for_each(|s| s.mint = [0; 32]);
}

/// For the allocation test: one shred packet through the in-process engine into a decider inline.
pub fn bench_shred(st: &mut Inproc, d: &mut Decider, pkt: &[u8], t: u64) {
    st.on_packet(0, pkt, t, &mut Sink::Inline(d), None);
}

#[cfg(test)]
mod inproc_tests {
    use {
        super::*,
        crate::hot::shreds::{testkit, walk},
        base64::Engine,
        solana_instruction::{AccountMeta, Instruction},
        solana_message::{legacy::Message, VersionedMessage},
        solana_transaction::versioned::VersionedTransaction,
    };

    const COMPUTE_BUDGET: Pubkey = Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

    /// A signed pump.fun buy of `mint` by `buyer` at `cu_price`.
    fn buy_tx(mint: Pubkey, buyer: &Keypair, lamports: u64, cu_price: u64) -> Vec<u8> {
        let mut accounts: Vec<AccountMeta> = (0..16).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect();
        accounts[2] = AccountMeta::new_readonly(mint, false);
        accounts[6] = AccountMeta::new(buyer.pubkey(), true);
        let mut data = decode::BUY_EXACT_SOL_IN.to_vec();
        data.extend_from_slice(&lamports.to_le_bytes());
        data.extend_from_slice(&1u64.to_le_bytes());
        data.push(1);
        let mut price = vec![3u8];
        price.extend_from_slice(&cu_price.to_le_bytes());
        let ixs = [Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: price }, Instruction { program_id: crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID, accounts, data }];
        let msg = Message::new_with_blockhash(&ixs, Some(&buyer.pubkey()), &solana_hash::Hash::new_from_array([5; 32]));
        bincode::serialize(&VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[buyer]).unwrap()).unwrap()
    }

    /// A real create from the fixtures, and its launch.
    fn create() -> (Vec<u8>, Launch) {
        let all: serde_json::Value = serde_json::from_str(include_str!("../processors/testdata/pump_creates.json")).unwrap();
        for f in all.as_object().unwrap().values() {
            let bytes = base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap();
            let Some(tx) = Tx::parse(&bytes) else { continue };
            let mut found = None;
            decode::launches(&tx, |l| found = Some(l), |_| {});
            if let Some(l) = found {
                return (bytes, l);
            }
        }
        panic!("no create fixture decodes");
    }

    /// What the decider is handed, comparable across feeds.
    fn events(rx: &mut spsc::Consumer<Ev>) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(ev) = rx.pop() {
            out.push(match ev {
                Ev::Create(c) => format!("create {} slot {}", c.launch.mint, c.slot),
                Ev::Buy(b) => format!("buy {} by {:?} {} lamports at {} sig {:?}", b.buy.mint, b.buy.buyer, b.buy.lamports, b.buy.cu_price, b.sig),
                Ev::BatchEnd(m) => format!("batch end {m}"),
                Ev::Trade(t) => format!("trade {} {} by {:?} seq {}", t.trade.mint, if t.trade.buy { "buy" } else { "sell" }, t.trade.user, t.seq),
                Ev::CreatorMigration(m) => format!("creator migration {}", m.mint),
            });
        }
        out
    }

    #[test]
    fn in_process_shreds_give_the_decider_what_pumpfeeds_push_gave() {
        let _g = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let slot: u64 = 777_000_001;
        forget_seen();
        let (create_bytes, launch) = create();
        let (b1, b2, b3) = (Keypair::new(), Keypair::new(), Keypair::new());
        let other = |sig: u8| walk::testkit::filler(sig, &[[sig; 32]]);
        let batch = walk::testkit::batch(&[
            ([1; 32], vec![other(1), create_bytes.clone(), buy_tx(launch.mint, &b1, 500_000_000, 5_000), other(2), buy_tx(launch.mint, &b2, 300_000_000, 9_000)]),
            ([2; 32], vec![other(3)]),
            ([3; 32], vec![buy_tx(launch.mint, &b3, 100_000_000, 7_000), buy_tx(Pubkey::new_unique(), &b1, 1, 99_999)]),
        ]);
        let thread = std::thread::current();

        // pumpfeed's push: the slot, then the pump.fun part of the batch.
        let mut datagram = slot.to_le_bytes().to_vec();
        walk::filter_batch(&batch, &mut datagram, |t| t.has(&PUMP_PROGRAM)).unwrap();
        let (mut p, mut c) = spsc::channel::<Ev>(64);
        let mut ranges = Box::new([(0u32, 0u32); MAX_BATCH_TXS]);
        handle_udp_batch(&datagram, now_ns(), &mut ranges[..], &mut Sink::Spsc(&mut p, &thread));
        let via_udp = events(&mut c);
        assert_eq!(via_udp.len(), 5, "create, three later buys of the coin, batch end: {via_udp:#?}");

        // In-process, with data shred 1 lost: Reed-Solomon rebuilds it.
        let (data, code) = testkit::shreds(slot, &batch, true);
        assert!(data.len() >= 3, "a batch of several shreds");
        let mut st = Inproc::new();
        let mut order: Vec<&Vec<u8>> = vec![&data[0], &code[0]];
        order.extend(data[2..].iter());
        let (mut p2, mut c2) = spsc::channel::<Ev>(64);
        for pkt in order {
            st.on_packet(1, pkt, now_ns(), &mut Sink::Spsc(&mut p2, &thread), None);
        }
        assert!(st.deshredder.stats.recovered_shreds >= 1);
        let in_process = events(&mut c2);
        assert_eq!(in_process, via_udp, "the same events, in the same order");

        // The slot's last batch gave its blockhash: the last entry's hash.
        assert_eq!(book::BOOK.lock().unwrap().find(slot).map(|e| e.1), Some([3; 32]));
    }

    #[test]
    fn only_the_coins_own_buys_after_the_create_reach_the_decider() {
        let _g = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        forget_seen();
        let (create_bytes, launch) = create();
        let (b1, b2) = (Keypair::new(), Keypair::new());
        let batch = walk::testkit::batch(&[([1; 32], vec![create_bytes, buy_tx(launch.mint, &b1, 1, 9_000), buy_tx(Pubkey::new_unique(), &b2, 1, 99_999)])]);
        let mut datagram = 5u64.to_le_bytes().to_vec();
        walk::filter_batch(&batch, &mut datagram, |t| t.has(&PUMP_PROGRAM)).unwrap();
        let thread = std::thread::current();
        let (mut p, mut c) = spsc::channel::<Ev>(64);
        let mut ranges = Box::new([(0u32, 0u32); MAX_BATCH_TXS]);
        handle_udp_batch(&datagram, now_ns(), &mut ranges[..], &mut Sink::Spsc(&mut p, &thread));
        let got = events(&mut c);
        assert_eq!(got.iter().filter(|e| e.starts_with("buy")).count(), 1, "only the coin's buy: {got:#?}");
        assert!(got.iter().any(|e| e.contains(" at 9000 ")));
    }
}

#[cfg(test)]
mod bundle10_tests {
    use {
        super::*,
        crate::hot::{alloc_audit::allocations, bundle10::Rec, testkit},
        solana_instruction::{AccountMeta, Instruction},
        solana_message::{legacy::Message, VersionedMessage},
        solana_transaction::versioned::VersionedTransaction,
        std::sync::atomic::Ordering,
    };

    /// BUNDLE10_SHADOW's feed flag on for a test, off again however it ends.
    struct FeedOn;
    impl FeedOn {
        fn new() -> Self {
            super::super::bundle10::FEED.store(true, Ordering::Relaxed);
            FeedOn
        }
    }
    impl Drop for FeedOn {
        fn drop(&mut self) {
            super::super::bundle10::FEED.store(false, Ordering::Relaxed);
        }
    }

    /// A transaction of pump.fun trades: (mint, buy, user, SOL in for a buy or tokens for a sell).
    fn trade_tx(sig: u8, trades: &[(Pubkey, bool, Pubkey, u64)]) -> Vec<u8> {
        let ixs: Vec<Instruction> = trades
            .iter()
            .map(|(mint, buy, user, amount)| {
                let mut accounts: Vec<AccountMeta> = (0..16).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect();
                accounts[2] = AccountMeta::new_readonly(*mint, false);
                accounts[6] = AccountMeta::new(*user, false);
                let mut data = if *buy { decode::BUY_EXACT_SOL_IN.to_vec() } else { decode::SELL.to_vec() };
                data.extend_from_slice(&amount.to_le_bytes());
                data.extend_from_slice(&1u64.to_le_bytes());
                if *buy {
                    data.push(1);
                }
                Instruction { program_id: crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID, accounts, data }
            })
            .collect();
        let msg = Message::new_with_blockhash(&ixs, Some(&Pubkey::new_unique()), &solana_hash::Hash::new_from_array([5; 32]));
        let tx = VersionedTransaction { signatures: vec![solana_signature::Signature::from([sig; 64])], message: VersionedMessage::Legacy(msg) };
        bincode::serialize(&tx).unwrap()
    }

    fn frame(slot: u64, tx: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8];
        f.extend(slot.to_le_bytes());
        f.extend_from_slice(&tx[1..65]);
        f.extend_from_slice(tx);
        f
    }

    const SOL: u64 = 500_000_000;

    /// The legacy fixture create, then its bundle: A (4 wallets), B (3 wallets and a sell: mixed),
    /// C (another coin's), D (a wallet of A again and 2 new), E (2 new), then F (2 more: a fifth
    /// transaction). bundle10 counts 4 + 3 + 2 + 2 = 11 wallets in 4 transactions.
    fn launch_frames(slot: u64) -> (Launch, Vec<Vec<u8>>) {
        let frames = testkit::frames(slot);
        let (_, create) = frames.iter().find(|(n, _)| n.contains("legacy")).unwrap().clone();
        let mut launch = None;
        decode::launches(&Tx::parse(&create[73..]).unwrap(), |l| launch = Some(l), |_| {});
        let l = launch.unwrap();
        let m = l.mint;
        let w: Vec<Pubkey> = (0..13).map(|_| Pubkey::new_unique()).collect();
        let b = |i: usize| (m, true, w[i], SOL);
        let txs = [
            trade_tx(0xA1, &[b(0), b(1), b(2), b(3)]),
            trade_tx(0xB2, &[b(4), (m, false, w[4], 1_000_000_000), b(5), b(6)]),
            trade_tx(0xC3, &[(Pubkey::new_unique(), true, w[7], SOL), (Pubkey::new_unique(), true, w[8], SOL)]),
            trade_tx(0xD4, &[b(0), b(7), b(8)]),
            trade_tx(0xE5, &[b(9), b(10)]),
            trade_tx(0xF6, &[b(11), b(12)]),
        ];
        let mut out = vec![create];
        out.extend(txs.iter().map(|t| frame(slot, t)));
        // Each frame's signature (the frame dedupe's and the count's) unique to this slot.
        for (i, f) in out.iter_mut().enumerate() {
            f[9..17].copy_from_slice(&((slot << 8) | i as u64 | 0xB10 << 32).to_le_bytes());
        }
        (l, out)
    }

    fn run(d: &mut Decider, frames: &[Vec<u8>]) {
        let mut recent = RecentSigs::new();
        let t0 = now_ns();
        for (i, f) in frames.iter().enumerate() {
            bench_frame(d, &mut recent, f, t0 + i as u64 * 1_000);
        }
        bench_settle(d);
    }

    #[test]
    fn the_shadow_counts_every_wallet_of_each_bundle_transaction_and_logs_the_existing_count() {
        let _g = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        super::super::clock::init();
        stamps::init();
        testkit::seed(now_unix());
        forget_seen();
        let _feed = FeedOn::new();
        let (l, frames) = launch_frames(1);
        let (p, mut c) = spsc::channel::<Rec>(16);
        let mut d = bench_decider(Arc::new(Keypair::new()), false, Mode::Inline).unwrap().with_bundle10(p, 1_000_000);
        run(&mut d, &frames);
        let rec = c.pop().expect("a record at the window's end");
        assert!(c.pop().is_none());
        assert_eq!(rec.mint, l.mint);
        assert_eq!((rec.count, rec.txs, rec.stop, rec.mixed, rec.trades, rec.unreadable, rec.overflow), (11, 4, 3, true, 13, 0, false));
        let sig = |i: usize| -> [u8; 8] { frames[i][9..17].try_into().unwrap() };
        assert_eq!(&rec.sigs[..], &[sig(1), sig(2), sig(4), sig(5)], "A, B, D, E: not C (another coin), not F (a fifth)");
        // The existing count takes every buy of every transaction in its window: A, B, D, E and F (the
        // fifth, which the shadow stops before), 14 buys of 13 wallets (D repeats A's wallet). Until
        // 14 Oct it kept one buy a transaction (the signature dedupe): 5 buys, 4 wallets.
        assert_eq!(rec.hot_count, Some(13));
        assert_eq!(rec.hot_buys, 14);
        assert!(rec.hot_bundle_lamports > rec.bundle_lamports, "F's buys too: {} vs {}", rec.hot_bundle_lamports, rec.bundle_lamports);
        assert_eq!(rec.miss, 0, "a standard create with 11 wallets passes: {}", super::super::bundle10::Miss::name_of(rec.miss));
        assert!(rec.post_mult > 1.0 && rec.post_mult < 6.0, "{}", rec.post_mult);
        assert!(rec.window_ns >= WINDOW_NS && rec.compute_ns > 0);
        assert_eq!(rec.buy_tip, 1_000_000);
    }

    #[test]
    fn the_shadow_changes_no_decision_and_sends_no_trade_while_off() {
        let _g = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        super::super::clock::init();
        stamps::init();
        testkit::seed(now_unix());
        // The fixture's deployer followed (bought, slot 2 has a near leader), then trading paused
        // (refused): each decision with the shadow off and on.
        let decisions = |shadow: bool| {
            let _feed = shadow.then(FeedOn::new);
            let (p, _c) = spsc::channel::<Rec>(16);
            let mut d = bench_decider(Arc::new(Keypair::new()), false, Mode::Inline).unwrap();
            if shadow {
                d = d.with_bundle10(p, 1_000_000);
            }
            let mut head = 0;
            let mut out = Vec::new();
            stamps::drain(&mut head, &mut out);
            out.clear();
            let mut mints = Vec::new();
            for trading in [true, false] {
                forget_seen();
                crate::utils::control::set_trading(trading);
                let (l, frames) = launch_frames(2);
                testkit::seed_follow(&frames[0]);
                run(&mut d, &frames);
                mints.push(l.mint.to_bytes());
            }
            crate::utils::control::set_trading(true);
            stamps::drain(&mut head, &mut out);
            out.iter().filter(|s| mints.contains(&s.mint)).map(|s| (s.reject, s.flags & (stamps::FLAG_BUY | stamps::FLAG_UNKNOWN), s.t_ready > 0)).collect::<Vec<_>>()
        };
        let off = decisions(false);
        let on = decisions(true);
        assert_eq!(off.len(), 2, "{off:?}");
        assert!(off[0].1 & stamps::FLAG_BUY != 0 && off[0].2, "the followed launch is bought and built: {off:?}");
        assert_eq!(off[1].0, decide::Reject::Paused.code(), "{off:?}");
        assert_eq!(on, off, "the same decisions with the shadow on");

        // Off, a feed thread hands the decider no trade; on, the coin's (not another coin's).
        let thread = std::thread::current();
        let feed_events = |on: bool| {
            forget_seen();
            let _feed = on.then(FeedOn::new);
            let (_, frames) = launch_frames(3);
            let (mut p, mut c) = spsc::channel::<Ev>(256);
            let mut recent = RecentSigs::new();
            for f in &frames {
                handle_pp_frame(f, now_ns(), &mut recent, &mut Sink::Spsc(&mut p, &thread), Feed::Preprocessed);
            }
            let mut trades = 0;
            let mut total = 0;
            while let Some(ev) = c.pop() {
                total += 1;
                trades += matches!(ev, Ev::Trade(_)) as usize;
            }
            (trades, total)
        };
        let (trades_off, total_off) = feed_events(false);
        let (trades_on, total_on) = feed_events(true);
        assert_eq!(trades_off, 0);
        assert_eq!(trades_on, 4 + 4 + 3 + 2 + 2, "every trade of the coin in the five transactions after the create");
        assert_eq!(total_on - trades_on, total_off, "the existing events are the same");
    }

    #[test]
    fn the_ring_slot_does_not_grow_and_the_shadow_allocates_nothing() {
        assert!(std::mem::size_of::<TradeEv>() <= std::mem::size_of::<CreateEv>(), "{} > {}", std::mem::size_of::<TradeEv>(), std::mem::size_of::<CreateEv>());
        let _g = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        super::super::clock::init();
        stamps::init();
        testkit::seed(now_unix());
        let _feed = FeedOn::new();
        let (p, mut c) = spsc::channel::<Rec>(64);
        let mut d = bench_decider(Arc::new(Keypair::new()), false, Mode::Inline).unwrap().with_bundle10(p, 1_000_000);
        // Warm every lazily built table once.
        let mut recent = RecentSigs::new();
        forget_seen();
        let (_, frames) = launch_frames(4);
        testkit::seed_follow(&frames[0]);
        run_with(&mut d, &mut recent, &frames);
        for slot in 5..8 {
            forget_seen();
            let (_, frames) = launch_frames(slot);
            testkit::seed_follow(&frames[0]);
            let ((), n) = allocations(|| run_with(&mut d, &mut recent, &frames));
            assert_eq!(n, 0, "slot {slot}: {n} allocations with the shadow on");
        }
        let mut got = 0;
        while c.pop().is_some() {
            got += 1;
        }
        assert_eq!(got, 4);
    }

    /// `run` with its `RecentSigs` made outside the measured part.
    fn run_with(d: &mut Decider, recent: &mut RecentSigs, frames: &[Vec<u8>]) {
        let t0 = now_ns();
        for (i, f) in frames.iter().enumerate() {
            bench_frame(d, recent, f, t0 + i as u64 * 1_000);
        }
        bench_settle(d);
    }
}

#[cfg(test)]
mod migration_tests {
    use {
        super::*,
        crate::{hot::testkit, instructions::pumpswap_buy_ix::PUMP_FEE_PROGRAM_ID},
        solana_instruction::{AccountMeta, Instruction},
        solana_message::{legacy::Message, VersionedMessage},
        solana_transaction::versioned::VersionedTransaction,
    };

    /// A frame of pump fees' create_fee_sharing_config for `mint` (the coin at account 4).
    fn fee_sharing_frame(slot: u64, mint: Pubkey) -> Vec<u8> {
        let mut accounts: Vec<AccountMeta> = (0..13).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect();
        accounts[4] = AccountMeta::new_readonly(mint, false);
        accounts[8] = AccountMeta::new_readonly(crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID, false);
        let ix = Instruction { program_id: PUMP_FEE_PROGRAM_ID, accounts, data: decode::CREATE_FEE_SHARING_CONFIG.to_vec() };
        let msg = Message::new_with_blockhash(&[ix], Some(&Pubkey::new_unique()), &solana_hash::Hash::new_from_array([5; 32]));
        let tx = bincode::serialize(&VersionedTransaction { signatures: vec![solana_signature::Signature::from([0xFE; 64])], message: VersionedMessage::Legacy(msg) }).unwrap();
        let mut f = vec![0u8];
        f.extend(slot.to_le_bytes());
        f.extend_from_slice(&tx[1..65]);
        f.extend_from_slice(&tx);
        f
    }

    fn contains(hay: &[u8], needle: &Pubkey) -> bool {
        hay.windows(32).any(|w| w == needle.as_ref())
    }

    /// The fixture create, alone and then followed in its window by its creator's move to fee
    /// sharing: the buy pays the named creator's vault, then the sharing config's.
    #[test]
    fn a_move_to_fee_sharing_in_the_window_makes_the_buy_pay_the_new_creators_vault() {
        let _g = testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        super::super::clock::init();
        stamps::init();
        testkit::seed(now_unix());
        let (_, create) = testkit::frames(1).into_iter().find(|(n, _)| n.contains("legacy")).unwrap();
        let mut launch = None;
        decode::launches(&Tx::parse(&create[73..]).unwrap(), |l| launch = Some(l), |_| {});
        let l = launch.unwrap();
        let named_vault = decide::creator_vault_of(&decide::derive(&l, &Pubkey::new_unique()).creator);
        let sharing = decode::sharing_config_of(&l.mint);
        let moved_vault = decide::creator_vault_of(&sharing);
        // Shadow builds: the buy is built whatever the rules say.
        let before = feeds::MIGRATIONS.load(std::sync::atomic::Ordering::Relaxed);
        for moved in [false, true] {
            forget_seen();
            let mut d = bench_decider(Arc::new(Keypair::new()), true, Mode::Inline).unwrap();
            let mut recent = RecentSigs::new();
            let t0 = now_ns();
            bench_frame(&mut d, &mut recent, &create, t0);
            if moved {
                bench_frame(&mut d, &mut recent, &fee_sharing_frame(1, l.mint), t0 + 2_000);
            }
            bench_settle(&mut d);
            let built = d.last_built();
            assert_eq!((contains(built, &named_vault), contains(built, &moved_vault)), (!moved, moved), "moved {moved}");
        }
        assert_eq!(feeds::MIGRATIONS.load(std::sync::atomic::Ordering::Relaxed), before + 1);
        // On an entry ring the move goes to the decider as its own event.
        let thread = std::thread::current();
        let (mut p, mut c) = spsc::channel::<Ev>(16);
        handle_pp_frame(&fee_sharing_frame(1, l.mint), now_ns(), &mut RecentSigs::new(), &mut Sink::Spsc(&mut p, &thread), Feed::Preprocessed);
        assert!(matches!(c.pop(), Some(Ev::CreatorMigration(m)) if m.mint == l.mint && m.slot == 1));
    }
}
