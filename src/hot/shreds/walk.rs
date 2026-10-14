//! Walking a deshredded entry batch (a bincode `Vec<Entry>`) without decoding it: where each
//! transaction starts and ends, the pump.fun filter pumpfeed applied before it pushed a batch to
//! the bot (`filter_batch`, so the hot path gets exactly the bytes it got over UDP), and the hash
//! of the batch's last entry (in a slot's last batch, the final tick: the slot's blockhash).
//! Ported from pumpfeed (`solana/entries.rs`): its structural transaction walker, which takes
//! every format it can bound (legacy, v0, v1) without judging the contents, so one unusual
//! transaction never loses a batch.
//!
//! ```text
//! Vec<Entry>            u64 len, then entries
//! Entry                 num_hashes u64 | hash [u8;32] | u64 len, then VersionedTransaction
//! VersionedTransaction  short_vec<[u8;64]> signatures | VersionedMessage (legacy or v0)
//! v1 (SIMD-0385)        0x81 | header [u8;3] | config_mask u32 | lifetime [u8;32] | num_ix u8
//!                       | num_addrs u8 | addrs | config values | ix headers | ix payloads
//!                       | signatures (last)
//! ```

/// Bounds-checked forward reader: every method returns `None` instead of panicking.
#[derive(Clone, Copy)]
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline(always)]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline(always)]
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    #[inline(always)]
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    #[inline(always)]
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    #[inline(always)]
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// Solana compact-u16: 7 bits per byte, at most 3 bytes.
    #[inline(always)]
    fn compact_u16(&mut self) -> Option<u16> {
        let mut val: u32 = 0;
        for i in 0..3 {
            let b = self.u8()?;
            val |= ((b & 0x7F) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return u16::try_from(val).ok();
            }
        }
        None
    }

    #[inline(always)]
    fn short_vec(&mut self, elem_size: usize) -> Option<&'a [u8]> {
        let n = self.compact_u16()? as usize;
        self.take(n * elem_size)
    }
}

/// A transaction's static account keys (`n * 32` bytes; lookup-table keys are not here).
#[derive(Clone, Copy, Debug)]
pub struct TxKeys<'a> {
    pub account_keys: &'a [u8],
}

impl TxKeys<'_> {
    /// Whether `key` is among the static keys (how pumpfeed tells a pump.fun transaction).
    #[inline(always)]
    pub fn has(&self, key: &[u8; 32]) -> bool {
        self.account_keys.chunks_exact(32).any(|k| k == key)
    }
}

const V1_PREFIX: u8 = 0x81;

#[inline(always)]
fn skip_ix(c: &mut Cursor<'_>) -> Option<()> {
    c.u8()?;
    c.short_vec(1)?;
    c.short_vec(1)?;
    Some(())
}

/// Moves the cursor past one transaction; its static keys.
#[inline(always)]
fn parse_tx<'a>(c: &mut Cursor<'a>) -> Option<TxKeys<'a>> {
    match c.buf.get(c.pos).copied() {
        Some(V1_PREFIX) => {
            c.u8()?;
            let header = [c.u8()?, c.u8()?, c.u8()?];
            let config_mask = c.u32()?;
            c.take(32)?; // lifetime
            let num_ix = c.u8()? as usize;
            let num_addrs = c.u8()? as usize;
            let account_keys = c.take(num_addrs * 32)?;
            c.take(config_mask.count_ones() as usize * 4)?;
            let headers = c.take(num_ix * 4)?;
            let payload_len: usize = headers.chunks_exact(4).map(|h| h[1] as usize + u16::from_le_bytes([h[2], h[3]]) as usize).sum();
            c.take(payload_len)?;
            c.take(header[0] as usize * 64)?;
            return Some(TxKeys { account_keys });
        }
        // Any other versioned prefix is a format nobody can bound.
        Some(b) if b & 0x80 != 0 => return None,
        _ => {}
    }
    c.short_vec(64)?; // signatures
    let first = c.u8()?;
    let is_v0 = if first & 0x80 != 0 {
        if first & 0x7F != 0 {
            return None;
        }
        c.take(3)?;
        true
    } else {
        c.take(2)?;
        false
    };
    let account_keys = c.short_vec(32)?;
    c.take(32)?; // recent blockhash
    let count = c.compact_u16()?;
    for _ in 0..count {
        skip_ix(c)?;
    }
    if is_v0 {
        let n = c.compact_u16()?;
        for _ in 0..n {
            c.take(32)?;
            c.short_vec(1)?;
            c.short_vec(1)?;
        }
    }
    Some(TxKeys { account_keys })
}

/// Appends to `out` the batch with only the transactions `keep` accepts, and only the entries that
/// still hold one (pumpfeed's `filter_batch`, so the result is the batch it pushed). How many
/// transactions were kept, or None when the batch does not parse (`out` is then cut back).
/// Allocation-free while `out` has room.
pub fn filter_batch(batch: &[u8], out: &mut Vec<u8>, mut keep: impl FnMut(&TxKeys<'_>) -> bool) -> Option<usize> {
    let base = out.len();
    let mut kept_entries = 0u64;
    let mut kept = 0usize;
    out.extend_from_slice(&[0u8; 8]);
    let mut walk = || -> Option<()> {
        let mut c = Cursor::new(batch);
        let num_entries = c.u64()?;
        for _ in 0..num_entries {
            let header = c.take(8 + 32)?;
            let num_txs = c.u64()?;
            let entry_at = out.len();
            out.extend_from_slice(header);
            out.extend_from_slice(&[0u8; 8]);
            let mut n = 0u64;
            for _ in 0..num_txs {
                let start = c.pos;
                let tx = parse_tx(&mut c)?;
                if keep(&tx) {
                    out.extend_from_slice(&batch[start..c.pos]);
                    n += 1;
                }
            }
            if n == 0 {
                out.truncate(entry_at);
            } else {
                out[entry_at + 40..entry_at + 48].copy_from_slice(&n.to_le_bytes());
                kept_entries += 1;
                kept += n as usize;
            }
        }
        Some(())
    };
    if walk().is_none() {
        out.truncate(base);
        return None;
    }
    out[base..base + 8].copy_from_slice(&kept_entries.to_le_bytes());
    Some(kept)
}

/// The hash of the batch's last entry, when the whole batch parses.
pub fn last_entry_hash(batch: &[u8]) -> Option<[u8; 32]> {
    let mut c = Cursor::new(batch);
    let num_entries = c.u64()?;
    let mut last = None;
    for _ in 0..num_entries {
        c.u64()?;
        last = Some(<[u8; 32]>::try_from(c.take(32)?).ok()?);
        let num_txs = c.u64()?;
        for _ in 0..num_txs {
            parse_tx(&mut c)?;
        }
    }
    last
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Entry batches built from transaction bytes, as a leader serializes them.

    /// A legacy transaction with one signature (its bytes `sig`), `keys` and no instructions.
    pub fn filler(sig: u8, keys: &[[u8; 32]]) -> Vec<u8> {
        let mut t = vec![1u8];
        t.extend_from_slice(&[sig; 64]);
        t.extend_from_slice(&[1, 0, 0]);
        t.push(keys.len() as u8);
        keys.iter().for_each(|k| t.extend_from_slice(k));
        t.extend_from_slice(&[9; 32]);
        t.push(0);
        t
    }

    /// One entry: (hash, transactions).
    pub fn batch(entries: &[([u8; 32], Vec<Vec<u8>>)]) -> Vec<u8> {
        let mut b = (entries.len() as u64).to_le_bytes().to_vec();
        for (hash, txs) in entries {
            b.extend_from_slice(&1u64.to_le_bytes());
            b.extend_from_slice(hash);
            b.extend_from_slice(&(txs.len() as u64).to_le_bytes());
            txs.iter().for_each(|t| b.extend_from_slice(t));
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use {super::*, testkit::*};

    #[test]
    fn the_filter_keeps_the_watched_transactions_and_drops_empty_entries() {
        let pump = [7u8; 32];
        let other = [3u8; 32];
        let b = batch(&[([1; 32], vec![filler(1, &[other]), filler(2, &[other, pump])]), ([2; 32], vec![filler(3, &[other])]), ([4; 32], vec![filler(4, &[pump])])]);
        let mut out = Vec::with_capacity(4096);
        assert_eq!(filter_batch(&b, &mut out, |t| t.has(&pump)), Some(2));
        assert_eq!(out, batch(&[([1; 32], vec![filler(2, &[other, pump])]), ([4; 32], vec![filler(4, &[pump])])]));
        assert_eq!(last_entry_hash(&b), Some([4; 32]));
        let mut cut = Vec::new();
        assert_eq!(filter_batch(&b[..b.len() - 3], &mut cut, |_| true), None);
        assert!(cut.is_empty(), "cut back");
        assert_eq!(last_entry_hash(&b[..b.len() - 3]), None);
    }
}
