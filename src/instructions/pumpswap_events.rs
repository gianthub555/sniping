//! PumpSwap (pump-amm) trade events, read from the CPI event of a buy or a sell (2026-10-13, the
//! snipe test's graduations): the pool before the trade, the amounts, the fees and, in events new
//! enough to carry it, the virtual quote the trade was priced on. Layout as the program's IDL (the
//! decoder crate's `BuyEvent` / `SellEvent`) and the copy bot's `pump-decode::amm`, with the newer
//! tail fields optional.
//!
//! Checked on mainnet (`pool_check`): on the event's pool (vault reserves plus its virtual quote),
//! `PumpSwapMarket::quote_buy` / `quote_sell` give exactly what the trader got. The virtual quote
//! moves from trade to trade, so a pool read some seconds later prices a little differently.

use {
    crate::{instructions::pumpswap_buy_ix::PUMPSWAP_PROGRAM_ID, utils::parse::BorshReader},
    anyhow::Result,
    carbon_core::transaction::TransactionMetadata,
    solana_pubkey::Pubkey,
};

pub const BUY_EVENT_DISCRIMINATOR: [u8; 8] = [0x67, 0xf4, 0x52, 0x1f, 0x2c, 0xf5, 0x77, 0x77];
pub const SELL_EVENT_DISCRIMINATOR: [u8; 8] = [0x3e, 0x2f, 0x37, 0x0a, 0xa5, 0x03, 0xdc, 0x2a];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTrade {
    pub is_buy: bool,
    pub pool: Pubkey,
    pub user: Pubkey,
    /// Tokens out of the pool (buy) or in (sell).
    pub base_amount: u64,
    /// The pool before the trade: base vault, quote vault (without the virtual quote).
    pub pool_base: u64,
    pub pool_quote: u64,
    /// Buy: quote swapped (no fees); sell: the constant-product output before fees.
    pub quote_amount: u64,
    pub lp_fee: u64,
    /// Buy: quote swapped plus the LP fee (what enters the vault); sell: output less the LP fee
    /// (what leaves the vault).
    pub quote_vault_change: u64,
    /// Buy: what the buyer paid in all; sell: what the seller got.
    pub user_quote: u64,
    pub virtual_quote: Option<i128>,
}

impl PoolTrade {
    /// The pool after this trade: (base vault, quote vault, virtual quote).
    pub fn after(&self) -> (u64, u64, i128) {
        let vq = self.virtual_quote.unwrap_or(0);
        if self.is_buy {
            (self.pool_base.saturating_sub(self.base_amount), self.pool_quote.saturating_add(self.quote_vault_change), vq)
        } else {
            (self.pool_base.saturating_add(self.base_amount), self.pool_quote.saturating_sub(self.quote_vault_change), vq)
        }
    }

    /// The pool's price before the trade (lamports per token unit, on vault + virtual quote).
    pub fn price_before(&self) -> f64 {
        price(self.pool_base, self.pool_quote, self.virtual_quote.unwrap_or(0))
    }
}

/// The price of a pool: (quote vault + virtual quote) / base, lamports per token unit.
pub fn price(base: u64, quote: u64, virtual_quote: i128) -> f64 {
    if base == 0 { 0.0 } else { (quote as i128 + virtual_quote).max(0) as f64 / base as f64 }
}

/// One event body (after the 8-byte event discriminator).
pub fn parse(is_buy: bool, body: &[u8]) -> Result<PoolTrade> {
    let mut r = BorshReader::new(body);
    let _timestamp = r.i64()?;
    let base_amount = r.u64()?;
    let _limit = r.u64()?;
    let _user_base = r.u64()?;
    let _user_quote = r.u64()?;
    let pool_base = r.u64()?;
    let pool_quote = r.u64()?;
    let quote_amount = r.u64()?;
    let _lp_bps = r.u64()?;
    let lp_fee = r.u64()?;
    let _protocol_bps = r.u64()?;
    let _protocol_fee = r.u64()?;
    let with_lp = r.u64()?;
    let user_quote = r.u64()?;
    let pool = r.pubkey()?;
    let user = r.pubkey()?;
    for _ in 0..4 {
        r.pubkey()?;
    }
    let _coin_creator = r.pubkey()?;
    let _creator_bps = r.u64()?;
    let _creator_fee = r.u64()?;
    // The newer tail: each field optional (an older, shorter event stops early).
    let mut tail = || -> Option<i128> {
        if is_buy {
            r.bool().ok()?; // track_volume
            r.u64().ok()?;
            r.u64().ok()?;
            r.u64().ok()?;
            r.i64().ok()?;
            r.u64().ok()?; // min_base_amount_out
            r.string().ok()?; // ix_name
        }
        r.u64().ok()?; // cashback bps
        r.u64().ok()?; // cashback
        r.u64().ok()?; // buyback bps
        r.u64().ok()?; // buyback fee
        let lo = r.u64().ok()? as u128;
        let hi = r.u64().ok()? as u128;
        Some((lo | (hi << 64)) as i128)
    };
    let virtual_quote = tail();
    Ok(PoolTrade { is_buy, pool, user, base_amount, pool_base, pool_quote, quote_amount, lp_fee, quote_vault_change: with_lp, user_quote, virtual_quote })
}

/// Every PumpSwap trade event of a transaction, in order.
pub fn trades(tx: &TransactionMetadata) -> Vec<PoolTrade> {
    let mut out = Vec::new();
    for (disc, is_buy) in [(BUY_EVENT_DISCRIMINATOR, true), (SELL_EVENT_DISCRIMINATOR, false)] {
        for body in crate::processors::common::cpi_event_payloads(tx, &PUMPSWAP_PROGRAM_ID, &disc) {
            if let Ok(t) = parse(is_buy, &body) {
                out.push(t);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real mainnet sell (2026-10-13, slot 456272855, pool HwFoZDpy…): 22,370,163 tokens in;
    /// the pool before it 25,519,150,336,034 base and 676,506,324,016 lamports in the vault, virtual
    /// quote 17,535,865,607; raw out 608,398, the seller got 603,529 (LP 20, protocol 5, creator 55 bps).
    #[test]
    fn the_virtual_quote_reproduces_a_real_sell() {
        // The fixed part of the real event (numbers above), then the accounts and the tail.
        let mut body = Vec::new();
        for v in [0i64 as u64, 22_370_163, 0, 0, 0, 25_519_150_336_034, 676_506_324_016, 608_398, 20, 1_217, 5, 305, 607_181, 603_529] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&[7u8; 32 * 7]);
        for v in [55u64, 3_347, 0, 0, 5_000, 152] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&17_535_865_607u128.to_le_bytes());
        let t = parse(false, &body).unwrap();
        assert_eq!((t.base_amount, t.pool_base, t.pool_quote, t.virtual_quote), (22_370_163, 25_519_150_336_034, 676_506_324_016, Some(17_535_865_607)));
        let raw = ((t.pool_quote as i128 + t.virtual_quote.unwrap()) as u128 * t.base_amount as u128 / (t.pool_base + t.base_amount) as u128) as u64;
        assert_eq!(raw, 608_398, "the chain's raw output");
        let fee = |bps: u64| (raw as u128 * bps as u128).div_ceil(10_000) as u64;
        assert_eq!(raw - fee(20) - fee(5) - fee(55), 603_529, "what the seller got");
        let (b, q, vq) = t.after();
        assert_eq!((b, q, vq), (25_519_150_336_034 + 22_370_163, 676_506_324_016 - 607_181, 17_535_865_607));
        // An older event without the tail still reads, without a virtual quote.
        let short = parse(false, &body[..body.len() - 16 - 32]).unwrap();
        assert_eq!(short.virtual_quote, None);
    }
}
