//! Minimal, allocation-free decoding of pump.fun launches from wire bytes (A3). It reads only what
//! the buy needs: the pump.fun program's instructions, the create's discriminator, its accounts
//! (mint, bonding curve, its token account, deployer, token program, quote mint) and arguments
//! (name, uri, creator, the create_v2 tail), and the top-level buys of the coin. Legacy, v0 and v1
//! messages, as `processors::shred_wire` reads them; v0 lookup-table accounts come from
//! `processors::lookup_tables`' cache. It accepts exactly what `shred_feed::parse_launches` and
//! `pump_buys_by` accept (tests compare the two on real transactions).
//!
//! Discriminators: pump.fun IDL (pump-fun/pump-public-docs `idl/pump.json`, 2026-09-12; the v3
//! trades from its 2026-10-13 version), equal to `sha256("global:<name>")[..8]`; see DECISIONS.md.

use {
    crate::{
        instructions::pumpswap_buy_ix::{PUMP_FEE_PROGRAM_ID, PUMP_PROGRAM_ID},
        processors::lookup_tables,
        utils::utils::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT},
    },
    solana_pubkey::Pubkey,
};

pub const CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
pub const CREATE_V2: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];
pub const BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
pub const BUY_V2: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
pub const BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
pub const BUY_EXACT_QUOTE_IN_V2: [u8; 8] = [194, 171, 28, 70, 104, 77, 91, 47];
/// The v3 trades (pump.fun, October 2026): the coin at account 1, the user at 8, no creator vault.
/// Dev buys moved to buy_v3 during 13 Oct: the early path's "no top-level dev buy" skips rose from
/// 280 an hour at 03:00 UTC to 953 at 21:00, while the regular stream showed a dev buy in about 90%
/// of the creates all day. Unread, those launches looked like creates without a dev buy.
pub const BUY_V3: [u8; 8] = [7, 5, 29, 196, 245, 23, 101, 80];
pub const BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];
/// pump.fun's migrate_bonding_curve_creator (the coin at account 0) and pump fees'
/// create_fee_sharing_config (the coin at account 4), which calls it: the curve's creator becomes
/// the coin's sharing config (`sharing_config_of`).
pub const MIGRATE_BONDING_CURVE_CREATOR: [u8; 8] = [87, 124, 52, 191, 52, 38, 214, 232];
pub const CREATE_FEE_SHARING_CONFIG: [u8; 8] = [195, 78, 86, 76, 111, 52, 251, 213];

const COMPUTE_BUDGET: Pubkey = Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
const V0_PREFIX: u8 = 0x80;
const V1_PREFIX: u8 = 0x81;
const V1_PRIORITY_FEE: u32 = 0b11;
const V1_COMPUTE_UNIT_LIMIT: u32 = 0b100;
const V1_LOADED_DATA_SIZE: u32 = 0b1000;
const V1_HEAP_SIZE: u32 = 0b1_0000;
const V1_KNOWN_BITS: u32 = 0b1_1111;
/// create_v2 argument tails the parser knows (see `shred_feed::CREATE_V2_TAILS`).
const CREATE_V2_TAILS: [usize; 5] = [1, 2, 3, 10, 11];
/// At most this many top-level instructions are looked at in one transaction.
const MAX_IXS: usize = 64;

#[derive(Clone, Copy)]
struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    #[inline]
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    #[inline]
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    #[inline]
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    /// Compact-u16.
    #[inline]
    fn short(&mut self) -> Option<usize> {
        let mut value = 0usize;
        for i in 0..3 {
            let b = self.u8()?;
            value |= ((b & 0x7f) as usize) << (7 * i);
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
}

/// One top-level instruction, borrowed from the wire bytes.
#[derive(Clone, Copy)]
pub struct Ix<'a> {
    pub program: u8,
    pub accounts: &'a [u8],
    pub data: &'a [u8],
}

/// A transaction's layout over its wire bytes. Nothing is copied.
#[derive(Clone, Copy)]
pub struct Tx<'a> {
    bytes: &'a [u8],
    /// Offset of the first signature.
    sig_at: usize,
    keys_at: usize,
    n_keys: usize,
    ixs: [Ix<'a>; MAX_IXS],
    n_ixs: usize,
    /// v0 lookups: offset of the first and their count.
    lookups_at: usize,
    n_lookups: usize,
    /// v1: the config mask and where its values start (the priority fee and limits live there).
    v1_mask: u32,
    v1_config_at: usize,
    /// Bytes the transaction took (to walk an entry batch).
    pub len: usize,
}

const NO_IX: Ix<'static> = Ix { program: 0, accounts: &[], data: &[] };

impl<'a> Tx<'a> {
    /// Lays out one transaction at the start of `bytes`. None when it does not parse.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let mut c = Cursor { b: bytes, at: 0 };
        let mut tx = Tx { bytes, sig_at: 0, keys_at: 0, n_keys: 0, ixs: [NO_IX; MAX_IXS], n_ixs: 0, lookups_at: 0, n_lookups: 0, v1_mask: 0, v1_config_at: 0, len: 0 };
        if *bytes.first()? == V1_PREFIX {
            c.at = 1;
            let header = c.take(3)?;
            let mask = c.u32()?;
            let priority_bits = mask & V1_PRIORITY_FEE;
            if mask & !V1_KNOWN_BITS != 0 || (priority_bits != 0 && priority_bits != V1_PRIORITY_FEE) {
                return None;
            }
            c.take(32)?;
            let n_ixs = c.u8()? as usize;
            tx.n_keys = c.u8()? as usize;
            tx.keys_at = c.at;
            c.take(32 * tx.n_keys)?;
            tx.v1_mask = mask;
            tx.v1_config_at = c.at;
            let mut config = 0;
            for (bit, size) in [(V1_PRIORITY_FEE, 8), (V1_COMPUTE_UNIT_LIMIT, 4), (V1_LOADED_DATA_SIZE, 4), (V1_HEAP_SIZE, 4)] {
                if mask & bit == bit {
                    config += size;
                }
            }
            c.take(config)?;
            let headers = c.take(4 * n_ixs)?;
            if n_ixs > MAX_IXS {
                return None;
            }
            for (i, h) in headers.chunks_exact(4).enumerate() {
                let accounts = c.take(h[1] as usize)?;
                let data = c.take(u16::from_le_bytes([h[2], h[3]]) as usize)?;
                tx.ixs[i] = Ix { program: h[0], accounts, data };
            }
            tx.n_ixs = n_ixs;
            tx.sig_at = c.at;
            c.take(64 * header[0] as usize)?;
            if header[0] == 0 {
                return None;
            }
            tx.len = c.at;
            return Some(tx);
        }
        let n_sigs = c.short()?;
        tx.sig_at = c.at;
        c.take(64 * n_sigs)?;
        if n_sigs == 0 {
            return None;
        }
        let versioned = match *bytes.get(c.at)? {
            V0_PREFIX => {
                c.at += 1;
                true
            }
            b if b & 0x80 != 0 => return None,
            _ => false,
        };
        c.take(3)?;
        tx.n_keys = c.short()?;
        tx.keys_at = c.at;
        c.take(32 * tx.n_keys)?;
        c.take(32)?;
        let n_ixs = c.short()?;
        if n_ixs > MAX_IXS {
            return None;
        }
        for i in 0..n_ixs {
            let program = c.u8()?;
            let n = c.short()?;
            let accounts = c.take(n)?;
            let n = c.short()?;
            let data = c.take(n)?;
            tx.ixs[i] = Ix { program, accounts, data };
        }
        tx.n_ixs = n_ixs;
        if versioned {
            tx.n_lookups = c.short()?;
            tx.lookups_at = c.at;
            for _ in 0..tx.n_lookups {
                c.take(32)?;
                let n = c.short()?;
                c.take(n)?;
                let n = c.short()?;
                c.take(n)?;
            }
        }
        tx.len = c.at;
        Some(tx)
    }

    pub fn signature(&self) -> [u8; 64] {
        self.bytes[self.sig_at..self.sig_at + 64].try_into().expect("64 bytes")
    }

    pub fn instructions(&self) -> &[Ix<'a>] {
        &self.ixs[..self.n_ixs]
    }

    #[inline]
    fn static_key(&self, i: usize) -> Option<Pubkey> {
        (i < self.n_keys).then(|| Pubkey::new_from_array(self.bytes[self.keys_at + 32 * i..self.keys_at + 32 * i + 32].try_into().unwrap()))
    }

    /// Position of `program` among the static keys (programs are never loaded from tables).
    pub fn key_index(&self, program: &Pubkey) -> Option<u8> {
        let target = program.as_ref();
        (0..self.n_keys).find(|&i| &self.bytes[self.keys_at + 32 * i..self.keys_at + 32 * i + 32] == target).map(|i| i as u8)
    }

    pub fn fee_payer(&self) -> Option<Pubkey> {
        self.static_key(0)
    }

    /// The account at `index`: static keys, then each lookup's writable positions, then each
    /// lookup's read-only positions (the v0 order). None when a table is not cached.
    pub fn account(&self, index: u8) -> Option<Pubkey> {
        let index = index as usize;
        if index < self.n_keys {
            return self.static_key(index);
        }
        let mut rest = index - self.n_keys;
        for readonly in [false, true] {
            let mut c = Cursor { b: self.bytes, at: self.lookups_at };
            for _ in 0..self.n_lookups {
                let table = Pubkey::new_from_array(c.take(32)?.try_into().ok()?);
                let n = c.short()?;
                let writable = c.take(n)?;
                let n = c.short()?;
                let ro = c.take(n)?;
                let positions = if readonly { ro } else { writable };
                match positions.get(rest) {
                    Some(&position) => return lookup_tables::resolve(&table, position),
                    None => rest -= positions.len(),
                }
            }
        }
        None
    }

    /// The transaction's priority bid as the leader ranks plain transactions: micro-lamports per
    /// compute unit. Legacy and v0: its SetComputeUnitPrice. v1: its total priority fee over its
    /// compute limit (200,000 when it sets none). 0 when it bids nothing.
    pub fn cu_price(&self) -> u64 {
        if self.v1_config_at != 0 {
            let mut c = Cursor { b: self.bytes, at: self.v1_config_at };
            let mut fee = 0u64;
            let mut limit = 0u32;
            if self.v1_mask & V1_PRIORITY_FEE == V1_PRIORITY_FEE {
                fee = c.u64().unwrap_or(0);
            }
            if self.v1_mask & V1_COMPUTE_UNIT_LIMIT != 0 {
                limit = c.u32().unwrap_or(0);
            }
            let limit = if limit == 0 { 200_000 } else { limit } as u128;
            return (fee as u128 * 1_000_000 / limit) as u64;
        }
        let Some(budget) = self.key_index(&COMPUTE_BUDGET) else { return 0 };
        for ix in self.instructions().iter().filter(|ix| ix.program == budget) {
            if ix.data.first() == Some(&3) && ix.data.len() >= 9 {
                return u64::from_le_bytes(ix.data[1..9].try_into().expect("8"));
            }
        }
        0
    }

    /// The transaction's compute limit: its SetComputeUnitLimit (legacy, v0) or v1 config value;
    /// 200,000 when it sets none (what the runtime assumes for one instruction).
    pub fn cu_limit(&self) -> u32 {
        const DEFAULT: u32 = 200_000;
        if self.v1_config_at != 0 {
            let mut c = Cursor { b: self.bytes, at: self.v1_config_at };
            if self.v1_mask & V1_PRIORITY_FEE == V1_PRIORITY_FEE {
                c.take(8);
            }
            if self.v1_mask & V1_COMPUTE_UNIT_LIMIT != 0 {
                return c.u32().filter(|l| *l > 0).unwrap_or(DEFAULT);
            }
            return DEFAULT;
        }
        let Some(budget) = self.key_index(&COMPUTE_BUDGET) else { return DEFAULT };
        for ix in self.instructions().iter().filter(|ix| ix.program == budget) {
            if ix.data.first() == Some(&2) && ix.data.len() >= 5 {
                return u32::from_le_bytes(ix.data[1..5].try_into().expect("4")).max(1);
            }
        }
        DEFAULT
    }

    /// The largest system transfer to an account `is_tip` names: (lamports, the account). Off
    /// the hot path (it resolves keys).
    pub fn tip(&self, is_tip: impl Fn(&Pubkey) -> bool) -> Option<(u64, Pubkey)> {
        let system = self.key_index(&Pubkey::default())?;
        let mut best: Option<(u64, Pubkey)> = None;
        for ix in self.instructions().iter().filter(|ix| ix.program == system) {
            if ix.data.len() != 12 || ix.data[..4] != [2, 0, 0, 0] {
                continue;
            }
            let lamports = u64::from_le_bytes(ix.data[4..12].try_into().expect("8"));
            let Some(to) = self.ix_account(ix, 1) else { continue };
            if lamports > 0 && is_tip(&to) && best.is_none_or(|(l, _)| lamports > l) {
                best = Some((lamports, to));
            }
        }
        best
    }

    /// The `i`-th account of an instruction.
    #[inline]
    pub fn ix_account(&self, ix: &Ix<'_>, i: usize) -> Option<Pubkey> {
        self.account(*ix.accounts.get(i)?)
    }
}

/// A pump.fun buy of some coin: (tokens exact or 0, SOL in or its cap, buyer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buy {
    pub mint: Pubkey,
    pub tokens: u64,
    pub lamports: u64,
    pub buyer: Option<Pubkey>,
    /// Its transaction's priority bid, micro-lamports per compute unit (`Tx::cu_price`).
    pub cu_price: u64,
}

/// The coin a pump.fun buy instruction buys, decoded as `pump_buys_by` decodes it.
pub fn buy_of(tx: &Tx<'_>, ix: &Ix<'_>) -> Option<Buy> {
    let d = ix.data;
    let disc: [u8; 8] = d.get(..8)?.try_into().ok()?;
    // Every buy's arguments start with two u64 (the third, an OptionBool, may be absent).
    if d.len() < 24 {
        return None;
    }
    let a = u64::from_le_bytes(d[8..16].try_into().ok()?);
    let b = u64::from_le_bytes(d[16..24].try_into().ok()?);
    let (mint_at, buyer_at, tokens, lamports) = match disc {
        BUY_EXACT_SOL_IN => (2, 6, 0, a),
        BUY => (2, 6, a, b),
        BUY_EXACT_QUOTE_IN_V2 => (1, 13, 0, a),
        BUY_V2 => (1, 13, a, b),
        BUY_EXACT_QUOTE_IN_V3 => (1, 8, 0, a),
        BUY_V3 => (1, 8, a, b),
        _ => return None,
    };
    Some(Buy { mint: tx.ix_account(ix, mint_at)?, tokens, lamports, buyer: tx.ix_account(ix, buyer_at), cu_price: 0 })
}

pub const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
pub const SELL_V2: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
pub const SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];

/// The coin a pump.fun sell instruction sells, its seller and tokens (bundle10's count).
pub fn sell_of(tx: &Tx<'_>, ix: &Ix<'_>) -> Option<super::bundle10::Trade> {
    let d = ix.data;
    let disc: [u8; 8] = d.get(..8)?.try_into().ok()?;
    if d.len() < 24 {
        return None;
    }
    let (mint_at, user_at) = match disc {
        SELL => (2, 6),
        SELL_V2 => (1, 13),
        SELL_V3 => (1, 8),
        _ => return None,
    };
    let tokens = u64::from_le_bytes(d[8..16].try_into().ok()?);
    let min_out = u64::from_le_bytes(d[16..24].try_into().ok()?);
    Some(super::bundle10::Trade { mint: tx.ix_account(ix, mint_at)?, buy: false, user: tx.ix_account(ix, user_at), tokens, lamports: min_out })
}

/// Every top-level pump.fun buy and sell of the transaction, in instruction order, each with its
/// place among them (bundle10's count: a transaction's first trade of a coin decides it).
pub fn trades(tx: &Tx<'_>, mut f: impl FnMut(u16, super::bundle10::Trade)) {
    let Some(pump) = tx.key_index(&PUMP_PROGRAM_ID) else { return };
    let mut seq = 0u16;
    for ix in tx.instructions().iter().filter(|ix| ix.program == pump) {
        let trade = match buy_of(tx, ix) {
            Some(b) => Some(super::bundle10::Trade::of_buy(&b)),
            None => sell_of(tx, ix),
        };
        if let Some(t) = trade {
            f(seq, t);
            seq = seq.saturating_add(1);
        }
    }
}

/// Every coin whose creator the transaction moves to a fee-sharing account (pump fees'
/// create_fee_sharing_config, or pump.fun's migrate_bonding_curve_creator at the top level). A buy
/// that lands after it must pay the sharing config's creator vault, or it fails (2006).
/// Allocation-free.
pub fn creator_migrations(tx: &Tx<'_>, mut f: impl FnMut(Pubkey)) {
    // The discriminator first (8 bytes an instruction); the program's key only on a match, so a
    // frame without a move costs a few comparisons (every frame of the hot path comes here).
    for ix in tx.instructions() {
        let Some(disc) = ix.data.get(..8) else { continue };
        let (program, mint_at) = if disc == CREATE_FEE_SHARING_CONFIG {
            (PUMP_FEE_PROGRAM_ID, 4)
        } else if disc == MIGRATE_BONDING_CURVE_CREATOR {
            (PUMP_PROGRAM_ID, 0)
        } else {
            continue;
        };
        if tx.account(ix.program) != Some(program) {
            continue;
        }
        if let Some(mint) = tx.ix_account(ix, mint_at) {
            f(mint);
        }
    }
}

/// The coin's sharing config (pump fees: `["sharing-config", mint]`): its curve's creator after a
/// move to fee sharing. The creator vault is then `creator-vault` of it.
pub fn sharing_config_of(mint: &Pubkey) -> Pubkey {
    super::pda::find_program_address(&[b"sharing-config", mint.as_ref()], &PUMP_FEE_PROGRAM_ID).0
}

/// A create's is_cashback_enabled from its instruction data: a create_v2's OptionBool, one byte
/// after is_mayhem_mode (absent: false); a legacy create has none (false). None for any other
/// instruction or arguments that do not parse.
pub fn create_cashback(data: &[u8]) -> Option<bool> {
    let disc = data.get(..8)?;
    if disc == CREATE {
        return Some(false);
    }
    if disc != CREATE_V2 {
        return None;
    }
    let mut c = Cursor { b: data, at: 8 };
    borsh_str(&mut c)?;
    borsh_str(&mut c)?;
    borsh_str(&mut c)?;
    c.take(32)?;
    let tail = &data[c.at..];
    tail.first()?;
    Some(tail.get(1).is_some_and(|b| *b != 0))
}

/// Why a create was not taken (no launch for the hot path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// create_v2 whose quote mint is not wrapped SOL.
    QuoteNotSol,
    /// create_v2 whose quote mint sits in a lookup table not cached yet.
    QuoteUnresolved,
    /// The mint or the deployer sits in a lookup table not cached yet.
    Unresolved,
}

/// A launch as the hot path needs it. All `Copy`.
#[derive(Clone, Copy, Debug)]
pub struct Launch {
    pub mint: Pubkey,
    pub deployer: Pubkey,
    /// The creator the create names (before the holder-reward account replaces it).
    pub named_creator: Pubkey,
    /// Its creator fees go to holders: the curve's creator is `holder_rewards_account(mint)`.
    pub holder_reward: bool,
    pub token_program: Pubkey,
    /// The create's own bonding-curve accounts (positions 2 and 3), when readable.
    pub bonding_curve: Option<Pubkey>,
    pub associated_bonding_curve: Option<Pubkey>,
    pub mayhem: bool,
    /// create_v2 args end in a layout the parser does not know.
    pub creator_set_onchain: bool,
    pub dev_buy_tokens: u64,
    pub dev_buy_lamports: u64,
    /// The metadata host is a DUMP_RISK_HOSTS host (None: no host in the uri).
    pub risky_host: Option<bool>,
    /// The metadata host is a FAST_RUG_HOSTS host.
    pub fast_rug_host: bool,
    pub name_all_caps: bool,
    pub fee_payer: Option<Pubkey>,
    pub signature: [u8; 64],
}

#[inline]
fn borsh_str<'a>(c: &mut Cursor<'a>) -> Option<&'a str> {
    let n = c.u32()? as usize;
    std::str::from_utf8(c.take(n)?).ok()
}

/// ASCII-case-insensitive substring test (`needle` lower-case ASCII).
fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    n.is_empty() || h.windows(n.len()).any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// The host of a metadata uri, as `common::uri_host` takes it, but borrowed.
fn uri_host(uri: &str) -> Option<&str> {
    let rest = uri.trim().split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then_some(host)
}

/// The launches of one transaction, each passed to `f` (at most a few per transaction).
/// Returns how many creates it saw, taken or skipped; `skipped` gets each skip.
pub fn launches(tx: &Tx<'_>, mut f: impl FnMut(Launch), mut skipped: impl FnMut(Skip)) -> usize {
    let Some(pump) = tx.key_index(&PUMP_PROGRAM_ID) else { return 0 };
    let mut seen = 0;
    for ix in tx.instructions().iter().filter(|ix| ix.program == pump) {
        let Some(disc) = ix.data.get(..8) else { continue };
        let v2 = disc == CREATE_V2;
        if !v2 && disc != CREATE {
            continue;
        }
        let mut c = Cursor { b: ix.data, at: 8 };
        let (Some(name), Some(_symbol), Some(uri), Some(creator)) = (borsh_str(&mut c), borsh_str(&mut c), borsh_str(&mut c), c.take(32)) else {
            continue;
        };
        let named_creator = Pubkey::new_from_array(creator.try_into().expect("32"));
        let tail = &ix.data[c.at..];
        let (mayhem, holder_reward, creator_set_onchain) = if v2 {
            // is_mayhem_mode is a strict borsh bool.
            match tail.first() {
                Some(0) => (false, false, false),
                Some(1) => (true, false, false),
                _ => continue,
            }
        } else {
            (false, false, false)
        };
        let (holder_reward, creator_set_onchain) = if v2 {
            (matches!(tail.len(), 3 | 11) && tail[tail.len() - 1] == 1, !CREATE_V2_TAILS.contains(&tail.len()))
        } else {
            (holder_reward, creator_set_onchain)
        };
        seen += 1;
        if v2 && ix.accounts.len() > 16 {
            match tx.ix_account(ix, 16) {
                Some(q) if q == WSOL_MINT => {}
                Some(_) => {
                    skipped(Skip::QuoteNotSol);
                    continue;
                }
                None => {
                    skipped(Skip::QuoteUnresolved);
                    continue;
                }
            }
        }
        let (mint, deployer) = match (tx.ix_account(ix, 0), tx.ix_account(ix, if v2 { 5 } else { 7 })) {
            (Some(m), Some(d)) => (m, d),
            _ => {
                skipped(Skip::Unresolved);
                continue;
            }
        };
        let token_program = if v2 { tx.ix_account(ix, 7).unwrap_or(TOKEN_2022_PROGRAM_ID) } else { TOKEN_PROGRAM_ID };
        let host = uri_host(uri);
        let mut launch = Launch {
            mint,
            deployer,
            named_creator,
            holder_reward,
            token_program,
            bonding_curve: tx.ix_account(ix, 2),
            associated_bonding_curve: tx.ix_account(ix, 3),
            mayhem,
            creator_set_onchain,
            dev_buy_tokens: 0,
            dev_buy_lamports: 0,
            risky_host: host.map(|h| crate::config::rules::DUMP_RISK_HOSTS.iter().any(|r| contains_ci(h, r))),
            fast_rug_host: host.is_some_and(|h| crate::config::rules::FAST_RUG_HOSTS.iter().any(|r| contains_ci(h, r))),
            name_all_caps: crate::config::rules::name_is_all_caps(name),
            fee_payer: tx.fee_payer(),
            signature: tx.signature(),
        };
        for other in tx.instructions().iter().filter(|o| o.program == pump) {
            if let Some(buy) = buy_of(tx, other) {
                if buy.mint == mint {
                    launch.dev_buy_tokens += buy.tokens;
                    launch.dev_buy_lamports += buy.lamports;
                }
            }
        }
        f(launch);
    }
    seen
}

/// Every pump.fun buy in the transaction, passed to `f`.
pub fn buys(tx: &Tx<'_>, mut f: impl FnMut(Buy)) {
    let Some(pump) = tx.key_index(&PUMP_PROGRAM_ID) else { return };
    let mut price = None;
    for ix in tx.instructions().iter().filter(|ix| ix.program == pump) {
        if let Some(mut buy) = buy_of(tx, ix) {
            buy.cu_price = *price.get_or_insert_with(|| tx.cu_price());
            f(buy);
        }
    }
}

/// Byte ranges of every transaction of a bincode `Vec<Entry>` payload (deshredded entries), into
/// `out`; how many were found, or None when the payload does not parse or `out` is too small.
pub fn entry_transactions(payload: &[u8], out: &mut [(u32, u32)]) -> Option<usize> {
    let mut c = Cursor { b: payload, at: 0 };
    let entries = c.u64()?;
    let mut n = 0;
    for _ in 0..entries {
        c.take(8 + 32)?;
        let count = c.u64()?;
        if count as usize > payload.len() / 64 {
            return None;
        }
        for _ in 0..count {
            let tx = Tx::parse(&payload[c.at..])?;
            *out.get_mut(n)? = (c.at as u32, tx.len as u32);
            n += 1;
            c.at += tx.len;
        }
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::processors::{shred_feed, shred_wire},
        base64::Engine,
    };

    fn fixtures() -> Vec<(String, Vec<u8>)> {
        let all: serde_json::Value = serde_json::from_str(include_str!("../processors/testdata/pump_creates.json")).unwrap();
        all.as_object()
            .unwrap()
            .iter()
            .map(|(v, f)| (v.clone(), base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap()))
            .collect()
    }

    /// A fixture file with its lookup tables cached, and its transaction's bytes.
    fn fixture_with_tables(json: &str) -> (serde_json::Value, Vec<u8>) {
        let f: serde_json::Value = serde_json::from_str(json).unwrap();
        for (table, addresses) in f["tables"].as_object().unwrap() {
            let addresses = addresses.as_array().unwrap().iter().map(|a| a.as_str().unwrap().parse().unwrap()).collect();
            lookup_tables::insert(table.parse().unwrap(), addresses);
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap();
        (f, bytes)
    }

    /// The create of 13 Oct whose dev buy is buy_v3: both decoders read its dev buy (before
    /// buy_v3 was known both saw none, and the launch was skipped as a hidden dev buy).
    #[test]
    fn a_buy_v3_dev_buy_is_read_like_the_full_parser() {
        let (f, bytes) = fixture_with_tables(include_str!("../processors/testdata/pump_create_buy_v3.json"));
        same_as_full("buy_v3 create", &bytes);
        let tx = Tx::parse(&bytes).unwrap();
        let mut got = None;
        launches(&tx, |l| got = Some(l), |_| {});
        let l = got.expect("a launch");
        assert_eq!(l.mint.to_string(), f["mint"].as_str().unwrap());
        assert!(l.dev_buy_tokens > 0 && l.dev_buy_lamports > 0, "{l:?}");
        let mut b = Vec::new();
        buys(&tx, |x| b.push(x));
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].mint, b[0].buyer, b[0].tokens, b[0].lamports), (l.mint, Some(l.deployer), l.dev_buy_tokens, l.dev_buy_lamports));
    }

    /// The transaction right after 5Dio's create on 13 Oct: pump fees' create_fee_sharing_config
    /// moves its creator to the sharing config. Both decoders find the coin; the sharing config is
    /// the one the chain names.
    #[test]
    fn a_creators_move_to_fee_sharing_is_read_from_its_transaction() {
        let (f, bytes) = fixture_with_tables(include_str!("../processors/testdata/pump_fee_sharing.json"));
        let mint: Pubkey = f["mint"].as_str().unwrap().parse().unwrap();
        let tx = Tx::parse(&bytes).unwrap();
        let mut got = Vec::new();
        creator_migrations(&tx, |m| got.push(m));
        assert_eq!(got, vec![mint]);
        let full = shred_wire::decode_transaction(&bytes).unwrap();
        assert_eq!(shred_feed::creator_migrations_in(&full.top_instructions()), vec![mint]);
        assert_eq!(sharing_config_of(&mint).to_string(), "3Z9mtCi8ebunhQ8fHTUHEutRvhCUHHZ7yAWHZkVp6695");
        // No create and no trade in it.
        let mut n = 0;
        launches(&tx, |_| n += 1, |_| {});
        trades(&tx, |_, _| n += 1);
        assert_eq!(n, 0);
        let ((), allocated) = crate::hot::alloc_audit::allocations(|| creator_migrations(&tx, |m| assert_eq!(m, mint)));
        assert_eq!(allocated, 0);
    }

    /// buy_v3, buy_exact_quote_in_v3 and sell_v3 (the coin at account 1, the user at 8), as the
    /// hot decoder and the full parser read them.
    #[test]
    fn the_v3_trades_are_read_by_both_decoders() {
        use {
            solana_instruction::{AccountMeta, Instruction},
            solana_message::{legacy::Message, VersionedMessage},
            solana_transaction::versioned::VersionedTransaction,
        };
        let (mint, user) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ix = |disc: [u8; 8], a: u64, b: u64| {
            let mut accounts: Vec<AccountMeta> = (0..17).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect();
            accounts[1] = AccountMeta::new_readonly(mint, false);
            accounts[8] = AccountMeta::new(user, true);
            let mut data = disc.to_vec();
            data.extend_from_slice(&a.to_le_bytes());
            data.extend_from_slice(&b.to_le_bytes());
            Instruction { program_id: PUMP_PROGRAM_ID, accounts, data }
        };
        let ixs = [ix(BUY_V3, 5_000, 300_000_000), ix(BUY_EXACT_QUOTE_IN_V3, 200_000_000, 1), ix(SELL_V3, 7_000, 2)];
        let msg = Message::new_with_blockhash(&ixs, Some(&user), &solana_hash::Hash::new_from_array([3; 32]));
        let bytes = bincode::serialize(&VersionedTransaction { signatures: vec![solana_signature::Signature::from([9; 64])], message: VersionedMessage::Legacy(msg) }).unwrap();
        let tx = Tx::parse(&bytes).unwrap();
        let mut got = Vec::new();
        trades(&tx, |seq, t| got.push((seq, t.buy, t.mint, t.user, t.tokens, t.lamports)));
        assert_eq!(
            got,
            vec![(0, true, mint, Some(user), 5_000, 300_000_000), (1, true, mint, Some(user), 0, 200_000_000), (2, false, mint, Some(user), 7_000, 2)]
        );
        let full = shred_wire::decode_transaction(&bytes).unwrap();
        assert_eq!(shred_feed::pump_buys_by(&full.top_instructions(), &mint), vec![(5_000, 300_000_000, Some(user)), (0, 200_000_000, Some(user))]);
    }

    fn lookup_fixture() -> Vec<u8> {
        let f: serde_json::Value = serde_json::from_str(include_str!("../processors/testdata/pump_create_lookup.json")).unwrap();
        for (table, addresses) in f["tables"].as_object().unwrap() {
            let addresses = addresses.as_array().unwrap().iter().map(|a| a.as_str().unwrap().parse().unwrap()).collect();
            lookup_tables::insert(table.parse().unwrap(), addresses);
        }
        base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap()
    }

    /// The minimal decoder sees the same launches, with the same fields, as the full one.
    fn same_as_full(name: &str, bytes: &[u8]) {
        let full = shred_wire::decode_transaction(bytes).unwrap_or_else(|| panic!("{name}: full decode"));
        let expected = shred_feed::parse_launches(&full.top_instructions());
        let tx = Tx::parse(bytes).unwrap_or_else(|| panic!("{name}: minimal decode"));
        assert_eq!(tx.len, bytes.len(), "{name}: the whole transaction");
        assert_eq!(tx.signature(), full.signature, "{name}");
        let mut got = Vec::new();
        launches(&tx, |l| got.push(l), |_| {});
        assert_eq!(got.len(), expected.len(), "{name}: launch count");
        for (l, e) in got.iter().zip(&expected) {
            assert_eq!(l.mint, e.mint, "{name}");
            assert_eq!(l.deployer, e.deployer, "{name}");
            let creator = if l.holder_reward { shred_feed::holder_rewards_account(&l.mint) } else { l.named_creator };
            assert_eq!(creator, e.creator, "{name}");
            assert_eq!(l.token_program, e.token_program, "{name}");
            assert_eq!(l.mayhem, e.mayhem, "{name}");
            assert_eq!(l.creator_set_onchain, e.creator_set_onchain, "{name}");
            assert_eq!((l.dev_buy_tokens, l.dev_buy_lamports), (e.dev_buy_tokens, e.dev_buy_lamports), "{name}");
            assert_eq!(l.risky_host, e.metadata_host.as_deref().map(crate::config::rules::risky_host), "{name}");
            assert_eq!(l.fast_rug_host, crate::config::rules::fast_rug_profile(e.metadata_host.as_deref(), 0), "{name}");
            assert_eq!(l.name_all_caps, crate::config::rules::name_is_all_caps(&e.name), "{name}");
            assert_eq!(l.bonding_curve, Some(crate::instructions::pumpfun_buy_ix::bonding_curve_address(&l.mint)), "{name}: create's curve account");
        }
        let mut minimal = Vec::new();
        buys(&tx, |b| minimal.push((b.tokens, b.lamports, b.buyer, b.mint)));
        let mut wanted = Vec::new();
        for l in &got {
            for (t, s, buyer) in shred_feed::pump_buys_by(&full.top_instructions(), &l.mint) {
                wanted.push((t, s, buyer, l.mint));
            }
        }
        let mine: Vec<_> = minimal.iter().filter(|b| got.iter().any(|l| l.mint == b.3)).copied().collect();
        assert_eq!(mine, wanted, "{name}: buys of the coin");
    }

    #[test]
    fn every_wire_format_decodes_like_the_full_parser() {
        let f = fixtures();
        assert_eq!(f.len(), 3, "legacy, v0 and v1");
        for (name, bytes) in &f {
            same_as_full(name, bytes);
        }
    }

    #[test]
    fn a_lookup_table_launch_decodes_like_the_full_parser_once_cached() {
        let _state = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = lookup_fixture();
        same_as_full("lookup", &bytes);
    }

    #[test]
    fn the_priority_bid_is_read_from_the_compute_budget_or_the_v1_config() {
        use solana_transaction::versioned::VersionedTransaction;
        for (name, bytes) in fixtures() {
            let tx = Tx::parse(&bytes).unwrap();
            let full: VersionedTransaction = bincode::deserialize(&bytes).unwrap_or_else(|_| VersionedTransaction::default());
            let keys = full.message.static_account_keys();
            let expected = full
                .message
                .instructions()
                .iter()
                .filter(|ix| keys.get(ix.program_id_index as usize) == Some(&COMPUTE_BUDGET) && ix.data.first() == Some(&3))
                .map(|ix| u64::from_le_bytes(ix.data[1..9].try_into().unwrap()))
                .next();
            if bytes[0] != V1_PREFIX {
                assert_eq!(tx.cu_price(), expected.unwrap_or(0), "{name}");
            }
            let mut prices = Vec::new();
            buys(&tx, |b| prices.push(b.cu_price));
            assert!(prices.iter().all(|p| *p == tx.cu_price()), "{name}: every buy carries its transaction's bid");
        }
    }

    #[test]
    fn non_pump_and_garbage_bytes_give_nothing() {
        let (_, bytes) = fixtures().remove(0);
        assert!(Tx::parse(&bytes[..bytes.len() / 2]).is_none(), "truncated");
        assert!(Tx::parse(&[]).is_none());
        let mut n = 0;
        launches(&Tx::parse(&bytes).unwrap(), |_| n += 1, |_| {});
        assert_eq!(n, 1);
    }

    #[test]
    fn an_entry_batch_is_walked_transaction_by_transaction() {
        let txs: Vec<Vec<u8>> = fixtures().into_iter().map(|(_, b)| b).collect();
        // bincode Vec<Entry>: one entry holding the three transactions.
        let mut payload = Vec::new();
        payload.extend(1u64.to_le_bytes());
        payload.extend(7u64.to_le_bytes());
        payload.extend([9u8; 32]);
        payload.extend((txs.len() as u64).to_le_bytes());
        for t in &txs {
            payload.extend(t);
        }
        let mut ranges = [(0u32, 0u32); 8];
        let n = entry_transactions(&payload, &mut ranges).expect("parses");
        assert_eq!(n, 3);
        for ((at, len), t) in ranges[..n].iter().zip(&txs) {
            assert_eq!(&payload[*at as usize..(*at + *len) as usize], &t[..]);
        }
        assert_eq!(shred_wire::entry_transactions(&payload).map(|v| v.len()), Some(3), "same count as the full walker");
    }
}

#[cfg(test)]
mod capture_tests {
    use {super::*, crate::processors::{shred_feed, shred_wire}};

    /// Real pumpfeed datagrams (slot, then entries) from HOT_UDP_DIR: the minimal walker and
    /// decoder must find what the full ones find. `cargo test -- --ignored udp_capture`.
    #[test]
    #[ignore]
    fn udp_capture_decodes_like_the_full_parser() {
        let Ok(dir) = std::env::var("HOT_UDP_DIR") else { return };
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        let (mut batches, mut old_ok, mut new_ok, mut old_creates, mut new_creates, mut diff) = (0, 0, 0, 0, 0, 0);
        let mut ranges = vec![(0u32, 0u32); 512];
        for f in &files {
            let d = std::fs::read(f).unwrap();
            let Some((_, payload)) = shred_feed::udp_batch(&d) else { continue };
            batches += 1;
            let old = shred_wire::entry_transactions(payload);
            let new = entry_transactions(payload, &mut ranges);
            if old.is_some() { old_ok += 1 }
            if new.is_some() { new_ok += 1 }
            let oc = old.as_ref().map_or(0, |txs| txs.iter().map(|t| shred_feed::parse_launches(&t.top_instructions()).len()).sum::<usize>());
            let nc = new.map_or(0, |n| {
                ranges[..n]
                    .iter()
                    .map(|&(at, len)| {
                        let mut taken = 0;
                        if let Some(tx) = Tx::parse(&payload[at as usize..(at + len) as usize]) {
                            launches(&tx, |_| taken += 1, |_| {});
                        }
                        taken
                    })
                    .sum::<usize>()
            });
            old_creates += oc;
            new_creates += nc;
            if old.map(|v| v.len()) != new || oc != nc {
                diff += 1;
                if diff <= 3 {
                    println!("{}: old {:?} txs {oc} creates, new {:?} txs {nc} creates", f.display(), shred_wire::entry_transactions(payload).map(|v| v.len()), entry_transactions(payload, &mut ranges));
                }
            }
        }
        println!("{batches} batches: parsed old {old_ok} new {new_ok}; creates old {old_creates} new {new_creates}; {diff} differ");
        assert_eq!(diff, 0);
    }
}
