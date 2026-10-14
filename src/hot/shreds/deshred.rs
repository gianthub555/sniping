//! Slot reassembly and Reed-Solomon recovery. Ported from pumpfeed (`solana/deshred.rs`) as it is.
//!
//! Each slot in flight gets a preallocated arena indexed by shred index, so
//! inserting a shred is one bounds check and one ~1.2 KB memcpy. When a run of
//! data shreds ending in a DATA_COMPLETE flag is contiguous, the bytes are one
//! complete bincode `Vec<Entry>` and go straight to the sink. This happens
//! mid-slot, as soon as the leader's batch is complete. Nothing waits for the
//! slot to finish, for replay, or for commitment.
//!
//! With streaming on (`Deshredder::streaming`), the batch in progress also
//! goes to the sink each time its in-order prefix grows (`BatchInfo::complete`
//! false), so a `BatchParser` can pass on each transaction as soon as its own
//! bytes are in.
//!
//! When data shreds are missing but the FEC set has enough coding shreds,
//! the missing ones are rebuilt with Reed-Solomon before the reassembly step.
//!
//! Hot-path allocations: none on the common path. Recovery builds one small
//! `Vec` of shard references; it only runs on packet loss.

use super::shred::{
    erasure_shard_range, Shred, ShredKind, CODE_SHRED_PAYLOAD, DATA_SHRED_PAYLOAD, FLAG_DATA_COMPLETE, FLAG_LAST_IN_SLOT, SIZE_OF_DATA_HEADERS,
};
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::collections::hash_map::{Entry, HashMap};

const EMPTY_SLOT: u64 = u64::MAX;
/// Ignore slots this far ahead of the highest seen, so one corrupt header
/// cannot make every later shred look stale.
const MAX_SLOT_JUMP: u64 = 10_000;

#[derive(Default, Clone, Copy, Debug)]
pub struct DeshredStats {
    pub data_shreds: u64,
    pub code_shreds: u64,
    pub duplicates: u64,
    pub stale: u64,
    pub out_of_range: u64,
    pub inconsistent: u64,
    pub recovered_fec_sets: u64,
    pub recovered_shreds: u64,
    pub recovery_failures: u64,
    pub batches: u64,
    /// Slots evicted without their LAST_IN_SLOT batch decoded: either
    /// shreds were lost beyond RS recovery (the rest of the slot after the
    /// gap is never decoded) or the leader abandoned the slot.
    pub incomplete_slots: u64,
    /// FEC sets evicted with data shreds still missing and not recoverable.
    pub stuck_fec_sets: u64,
}

/// Per-FEC-set bookkeeping, indexed by `fec_set_index` inside a slot.
#[derive(Default, Clone, Copy)]
struct FecMeta {
    touched: bool,
    /// Layout is known once one coding shred has arrived.
    known: bool,
    /// Either fully present or recovery was already attempted.
    done: bool,
    num_data: u16,
    num_code: u16,
    first_code_index: u32,
    data_have: u16,
    code_have: u16,
    proof_size: u8,
    resigned: bool,
}

struct SlotState {
    slot: u64,
    /// `max` * DATA_SHRED_PAYLOAD. Stores full payloads because RS works on
    /// the whole shard, not only the data bytes.
    data: Box<[u8]>,
    data_present: Box<[u64]>,
    data_flags: Box<[u8]>,
    data_size: Box<[u16]>,
    /// `max` * CODE_SHRED_PAYLOAD, indexed by coding-shred index.
    code: Box<[u8]>,
    code_present: Box<[u64]>,
    fec: Box<[FecMeta]>,
    /// FEC entries written this slot, so a reset clears only those instead
    /// of the whole table.
    fec_touched: Vec<u32>,
    /// First data-shred index not yet emitted as part of a batch.
    consumed: u32,
    /// Next index to check for contiguity (always >= consumed).
    scan_pos: u32,
    /// The data bytes of shreds `consumed..scan_pos`: the batch in progress,
    /// appended shred by shred as they become contiguous.
    asm: Vec<u8>,
    /// Receive time of the first shred of this slot, from any FEC set. This
    /// is our view of when the leader started broadcasting it.
    first_seen_ns: u64,
    /// The LAST_IN_SLOT batch has been emitted (slot fully decoded).
    last_emitted: bool,
}

/// Context passed to the sink with each entry batch.
#[derive(Clone, Copy, Debug)]
pub struct BatchInfo {
    pub slot: u64,
    pub slot_first_seen_ns: u64,
    /// The batch ends with the slot's final shred, so its last entry is the
    /// slot's final tick, and that entry's hash is the slot's blockhash.
    pub last_in_slot: bool,
    /// Index of the batch's first data shred: with `slot`, names the batch.
    pub batch_start: u32,
    /// The bytes are the whole batch. False: the batch's in-order prefix so
    /// far (streaming only); later calls for the same batch extend it.
    pub complete: bool,
}

#[inline(always)]
fn bit(bits: &[u64], i: usize) -> bool {
    (bits[i >> 6] >> (i & 63)) & 1 != 0
}

#[inline(always)]
fn set_bit(bits: &mut [u64], i: usize) {
    bits[i >> 6] |= 1u64 << (i & 63);
}

/// Touch every page so the first shred of a slot doesn't take page faults.
/// With `mlockall(MCL_FUTURE)` active the pages also stay resident.
fn prefault(buf: &mut [u8]) {
    for i in (0..buf.len()).step_by(4096) {
        // Volatile so the compiler cannot drop writes to zeroed memory.
        unsafe { std::ptr::write_volatile(buf.as_mut_ptr().add(i), 0) };
    }
}

impl SlotState {
    fn new(max: usize) -> Self {
        let mut data = vec![0u8; max * DATA_SHRED_PAYLOAD].into_boxed_slice();
        let mut code = vec![0u8; max * CODE_SHRED_PAYLOAD].into_boxed_slice();
        prefault(&mut data);
        prefault(&mut code);
        Self {
            slot: EMPTY_SLOT,
            data,
            data_present: vec![0u64; max / 64].into_boxed_slice(),
            data_flags: vec![0u8; max].into_boxed_slice(),
            data_size: vec![0u16; max].into_boxed_slice(),
            code,
            code_present: vec![0u64; max / 64].into_boxed_slice(),
            fec: vec![FecMeta::default(); max].into_boxed_slice(),
            fec_touched: Vec::with_capacity(max),
            consumed: 0,
            scan_pos: 0,
            asm: {
                // Room for a 256-shred batch without reallocating; larger ones are rare.
                let mut asm = vec![0u8; 256 * DATA_SHRED_PAYLOAD];
                prefault(&mut asm);
                asm.clear();
                asm
            },
            first_seen_ns: 0,
            last_emitted: false,
        }
    }

    fn reset(&mut self, slot: u64, now_ns: u64, stats: &mut DeshredStats) {
        // Account for what the evicted slot never finished.
        if self.slot != EMPTY_SLOT && !self.fec_touched.is_empty() {
            if !self.last_emitted {
                stats.incomplete_slots += 1;
            }
            for &f in &self.fec_touched {
                let m = &self.fec[f as usize];
                let need = if m.known { m.num_data } else { 32 };
                if m.data_have > 0 && m.data_have < need {
                    stats.stuck_fec_sets += 1;
                }
            }
        }
        self.slot = slot;
        self.first_seen_ns = now_ns;
        self.last_emitted = false;
        self.data_present.fill(0);
        self.code_present.fill(0);
        for &f in &self.fec_touched {
            self.fec[f as usize] = FecMeta::default();
        }
        self.fec_touched.clear();
        self.consumed = 0;
        self.scan_pos = 0;
        self.asm.clear();
    }

    #[inline(always)]
    fn fec_meta(&mut self, f: usize) -> &mut FecMeta {
        let m = &mut self.fec[f];
        if !m.touched {
            m.touched = true;
            // Capacity is `max` and `f < max`, so this never reallocates.
            self.fec_touched.push(f as u32);
        }
        &mut self.fec[f]
    }

    /// Returns the FEC set index if the shred was new.
    #[inline(always)]
    fn insert_data(&mut self, s: &Shred<'_>, flags: u8, size: u16, max: usize, stats: &mut DeshredStats) -> Option<usize> {
        let idx = s.index as usize;
        let f = s.fec_set_index as usize;
        if idx >= max || f >= max {
            stats.out_of_range += 1;
            return None;
        }
        if bit(&self.data_present, idx) {
            // Normal: shredstream fans in from several regions.
            stats.duplicates += 1;
            return None;
        }
        let off = idx * DATA_SHRED_PAYLOAD;
        self.data[off..off + DATA_SHRED_PAYLOAD].copy_from_slice(s.payload);
        self.data_flags[idx] = flags;
        self.data_size[idx] = size;
        set_bit(&mut self.data_present, idx);

        let m = self.fec_meta(f);
        m.data_have += 1;
        m.proof_size = s.proof_size;
        m.resigned = s.resigned;
        stats.data_shreds += 1;
        Some(f)
    }

    #[inline(always)]
    fn insert_code(&mut self, s: &Shred<'_>, max: usize, stats: &mut DeshredStats) -> Option<usize> {
        let ShredKind::Code { num_data, num_code, position } = s.kind else {
            return None;
        };
        let idx = s.index as usize;
        let f = s.fec_set_index as usize;
        if idx >= max || f >= max {
            stats.out_of_range += 1;
            return None;
        }
        if bit(&self.code_present, idx) {
            stats.duplicates += 1;
            return None;
        }
        let first_code_index = s.index - position as u32;
        let needed = {
            let m = self.fec_meta(f);
            if m.known {
                if m.num_data != num_data || m.num_code != num_code || m.first_code_index != first_code_index {
                    stats.inconsistent += 1;
                    return None;
                }
            } else {
                m.known = true;
                m.num_data = num_data;
                m.num_code = num_code;
                m.first_code_index = first_code_index;
            }
            m.code_have += 1;
            m.proof_size = s.proof_size;
            m.resigned = s.resigned;
            // Coding shreds only serve recovery: once the set's data is complete
            // (or recovery ran), its payload is never read. About half of all
            // packets are coding shreds, so this saves a 1.2 KB copy for most of them.
            !(m.done || m.data_have >= m.num_data)
        };
        if needed {
            let off = idx * CODE_SHRED_PAYLOAD;
            self.code[off..off + CODE_SHRED_PAYLOAD].copy_from_slice(s.payload);
        }
        set_bit(&mut self.code_present, idx);
        stats.code_shreds += 1;
        Some(f)
    }

    /// Rebuilds missing data shreds of FEC set `f` if enough shards are
    /// present. Returns true if any data shred was recovered.
    #[cold]
    #[inline(never)]
    fn recover(&mut self, f: usize, max: usize, rs_cache: &mut HashMap<(usize, usize), ReedSolomon>, stats: &mut DeshredStats) -> bool {
        let m = self.fec[f];
        let (nd, nc) = (m.num_data as usize, m.num_code as usize);
        let fc = m.first_code_index as usize;
        self.fec[f].done = true; // One attempt per set, whatever the outcome.

        if f + nd > max || fc + nc > max {
            stats.out_of_range += 1;
            return false;
        }
        let (Some(rd), Some(rc)) = (erasure_shard_range(true, m.proof_size, m.resigned), erasure_shard_range(false, m.proof_size, m.resigned)) else {
            stats.recovery_failures += 1;
            return false;
        };
        let rs = match rs_cache.entry((nd, nc)) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => match ReedSolomon::new(nd, nc) {
                Ok(rs) => v.insert(rs),
                Err(_) => {
                    stats.recovery_failures += 1;
                    return false;
                }
            },
        };

        {
            // Borrows disjoint fields: data/code mutably, bitsets shared.
            let data_region = &mut self.data[f * DATA_SHRED_PAYLOAD..(f + nd) * DATA_SHRED_PAYLOAD];
            let code_region = &mut self.code[fc * CODE_SHRED_PAYLOAD..(fc + nc) * CODE_SHRED_PAYLOAD];
            let mut shards: Vec<(&mut [u8], bool)> = Vec::with_capacity(nd + nc);
            for (i, chunk) in data_region.chunks_exact_mut(DATA_SHRED_PAYLOAD).enumerate() {
                shards.push((&mut chunk[rd.clone()], bit(&self.data_present, f + i)));
            }
            for (i, chunk) in code_region.chunks_exact_mut(CODE_SHRED_PAYLOAD).enumerate() {
                shards.push((&mut chunk[rc.clone()], bit(&self.code_present, fc + i)));
            }
            // Writes recovered shards in place, straight into the slot arena.
            if rs.reconstruct_data(&mut shards).is_err() {
                stats.recovery_failures += 1;
                return false;
            }
        }

        // The shard covers everything after the signature, so the recovered
        // payload has valid headers. Parse them to get flags and size, and
        // reject output whose slot or index doesn't match.
        let mut recovered = 0u64;
        for idx in f..f + nd {
            if bit(&self.data_present, idx) {
                continue;
            }
            let off = idx * DATA_SHRED_PAYLOAD;
            let parsed = Shred::parse(&self.data[off..off + DATA_SHRED_PAYLOAD]).and_then(|s| match s.kind {
                ShredKind::Data { flags, size } if s.slot == self.slot && s.index as usize == idx => Some((flags, size)),
                _ => None,
            });
            let Some((flags, size)) = parsed else {
                stats.recovery_failures += 1;
                continue;
            };
            self.data_flags[idx] = flags;
            self.data_size[idx] = size;
            set_bit(&mut self.data_present, idx);
            recovered += 1;
        }
        self.fec[f].data_have += recovered as u16;
        stats.recovered_fec_sets += 1;
        stats.recovered_shreds += recovered;
        recovered > 0
    }

    #[inline(always)]
    fn maybe_recover(&mut self, f: usize, max: usize, rs_cache: &mut HashMap<(usize, usize), ReedSolomon>, stats: &mut DeshredStats) -> bool {
        let m = &self.fec[f];
        if !m.known || m.done {
            return false;
        }
        let nd = m.num_data as usize;
        let have = m.data_have as usize;
        if have >= nd {
            self.fec[f].done = true;
            return false;
        }
        if have + (m.code_have as usize) < nd {
            return false;
        }
        self.recover(f, max, rs_cache, stats)
    }

    /// Emits every newly completed `Vec<Entry>` batch, in order; with
    /// `streaming`, then the grown prefix of the batch still in progress.
    #[inline(always)]
    fn advance<F: FnMut(&BatchInfo, &[u8])>(&mut self, max: usize, streaming: bool, stats: &mut DeshredStats, sink: &mut F) {
        let mut grew = false;
        loop {
            let i = self.scan_pos as usize;
            if i >= max || !bit(&self.data_present, i) {
                break;
            }
            self.scan_pos += 1;
            // Entries cross shred boundaries: the batch's data bytes are joined
            // here as each shred becomes contiguous, so no step copies them twice.
            let off = i * DATA_SHRED_PAYLOAD;
            self.asm.extend_from_slice(&self.data[off + SIZE_OF_DATA_HEADERS..off + self.data_size[i] as usize]);
            let flags = self.data_flags[i];
            if flags & FLAG_DATA_COMPLETE == 0 {
                grew = true;
                continue;
            }
            let batch_start = self.consumed;
            self.consumed = i as u32 + 1;
            grew = false;
            stats.batches += 1;
            let info = BatchInfo {
                slot: self.slot,
                slot_first_seen_ns: self.first_seen_ns,
                last_in_slot: flags & FLAG_LAST_IN_SLOT == FLAG_LAST_IN_SLOT,
                batch_start,
                complete: true,
            };
            self.last_emitted |= info.last_in_slot;
            sink(&info, &self.asm);
            self.asm.clear();
        }
        if streaming && grew {
            let info = BatchInfo {
                slot: self.slot,
                slot_first_seen_ns: self.first_seen_ns,
                last_in_slot: false,
                batch_start: self.consumed,
                complete: false,
            };
            sink(&info, &self.asm);
        }
    }
}

pub struct Deshredder {
    ring: Box<[SlotState]>,
    max_shreds: usize,
    rs_cache: HashMap<(usize, usize), ReedSolomon>,
    highest_slot: u64,
    /// Also pass on the in-order prefix of the batch in progress (see the module docs).
    streaming: bool,
    pub stats: DeshredStats,
}

impl Deshredder {
    /// `ring_len` slots in flight, each holding up to `max_shreds_per_slot`
    /// data and coding shreds. Allocates and prefaults
    /// ring_len * max * (1203 + 1228) bytes up front, about 160 MB for 8 x 8192.
    pub fn new(ring_len: usize, max_shreds_per_slot: usize) -> Self {
        let max = max_shreds_per_slot.max(64).next_multiple_of(64);
        let ring = (0..ring_len.max(2)).map(|_| SlotState::new(max)).collect();
        let mut rs_cache = HashMap::new();
        // Mainnet Merkle FEC sets are 32 data : 32 coding.
        if let Ok(rs) = ReedSolomon::new(32, 32) {
            rs_cache.insert((32, 32), rs);
        }
        Self { ring, max_shreds: max, rs_cache, highest_slot: 0, streaming: false, stats: DeshredStats::default() }
    }

    /// Also call the sink with the batch in progress each time its in-order
    /// prefix grows (`BatchInfo::complete` false).
    pub fn streaming(mut self, on: bool) -> Self {
        self.streaming = on;
        self
    }

    /// Stores one shred received at `now_ns` and calls
    /// `sink(batch_info, entry_batch_bytes)` for each `Vec<Entry>` batch it
    /// completes (and, streaming, for the batch in progress when it grew).
    /// `sink` runs on this thread, before `insert` returns.
    #[inline]
    pub fn insert<F: FnMut(&BatchInfo, &[u8])>(&mut self, shred: &Shred<'_>, now_ns: u64, sink: &mut F) {
        let ring_len = self.ring.len() as u64;
        if shred.slot.saturating_add(ring_len) <= self.highest_slot {
            self.stats.stale += 1;
            return;
        }
        if self.highest_slot == 0 || shred.slot <= self.highest_slot.saturating_add(MAX_SLOT_JUMP) {
            self.highest_slot = self.highest_slot.max(shred.slot);
        } else {
            self.stats.out_of_range += 1;
            return;
        }

        let Self { ring, rs_cache, stats, max_shreds, streaming, .. } = self;
        let (max, streaming) = (*max_shreds, *streaming);
        let st = &mut ring[(shred.slot % ring_len) as usize];
        if st.slot != shred.slot {
            if st.slot != EMPTY_SLOT && st.slot > shred.slot {
                stats.stale += 1;
                return;
            }
            st.reset(shred.slot, now_ns, stats);
        }

        let (fec, new_data) = match shred.kind {
            ShredKind::Data { flags, size } => (st.insert_data(shred, flags, size, max, stats), true),
            ShredKind::Code { .. } => (st.insert_code(shred, max, stats), false),
        };
        let Some(fec) = fec else { return };
        let recovered = st.maybe_recover(fec, max, rs_cache, stats);
        if new_data || recovered {
            st.advance(max, streaming, stats, sink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VARIANT_DATA: u8 = 0x93; // chained Merkle data, proof size 3
    const VARIANT_CODE: u8 = 0x63; // chained Merkle code, proof size 3

    fn common(p: &mut [u8], variant: u8, slot: u64, index: u32, fec: u32) {
        p[64] = variant;
        p[65..73].copy_from_slice(&slot.to_le_bytes());
        p[73..77].copy_from_slice(&index.to_le_bytes());
        p[77..79].copy_from_slice(&1u16.to_le_bytes());
        p[79..83].copy_from_slice(&fec.to_le_bytes());
    }

    fn data_shred(slot: u64, index: u32, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; DATA_SHRED_PAYLOAD];
        common(&mut p, VARIANT_DATA, slot, index, 0);
        p[83..85].copy_from_slice(&1u16.to_le_bytes());
        p[85] = flags;
        p[86..88].copy_from_slice(&((SIZE_OF_DATA_HEADERS + body.len()) as u16).to_le_bytes());
        p[88..88 + body.len()].copy_from_slice(body);
        p
    }

    #[test]
    fn reassembles_and_recovers_lost_data_shreds() {
        let (slot, nd, nc) = (42u64, 4usize, 4usize);
        let bodies: Vec<Vec<u8>> = (0..nd).map(|i| vec![i as u8 + 1; 700 + i * 10]).collect();
        let data: Vec<Vec<u8>> = bodies
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let flags = if i == nd - 1 { FLAG_LAST_IN_SLOT } else { 0 };
                data_shred(slot, i as u32, flags, b)
            })
            .collect();

        // Encode parity the same way a leader does.
        let rd = erasure_shard_range(true, 3, false).unwrap();
        let rc = erasure_shard_range(false, 3, false).unwrap();
        let mut shards: Vec<Vec<u8>> = data.iter().map(|d| d[rd.clone()].to_vec()).collect();
        shards.extend((0..nc).map(|_| vec![0u8; rd.len()]));
        ReedSolomon::new(nd, nc).unwrap().encode(&mut shards).unwrap();
        let code: Vec<Vec<u8>> = (0..nc)
            .map(|pos| {
                let mut p = vec![0u8; CODE_SHRED_PAYLOAD];
                common(&mut p, VARIANT_CODE, slot, pos as u32, 0);
                p[83..85].copy_from_slice(&(nd as u16).to_le_bytes());
                p[85..87].copy_from_slice(&(nc as u16).to_le_bytes());
                p[87..89].copy_from_slice(&(pos as u16).to_le_bytes());
                p[rc.clone()].copy_from_slice(&shards[nd + pos]);
                p
            })
            .collect();

        let mut d = Deshredder::new(4, 256);
        let mut out: Vec<(BatchInfo, Vec<u8>)> = Vec::new();
        let mut sink = |i: &BatchInfo, b: &[u8]| out.push((*i, b.to_vec()));
        // Data shreds 0 and 2 are "lost". The first packet sets the slot's
        // first-seen time.
        for (t, pkt) in [&data[1], &data[3], &code[0], &code[3]].into_iter().enumerate() {
            d.insert(&Shred::parse(pkt).unwrap(), 1_000 + t as u64, &mut sink);
        }

        assert_eq!(d.stats.recovered_shreds, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.slot, slot);
        assert_eq!(out[0].0.slot_first_seen_ns, 1_000);
        assert!(out[0].0.last_in_slot);
        assert!(out[0].0.complete);
        assert_eq!(out[0].1, bodies.concat());
    }

    /// Streaming: the batch in progress goes out each time its in-order prefix
    /// grows, then whole; a shred ahead of a gap waits for the gap.
    #[test]
    fn streaming_passes_on_the_growing_prefix_then_the_whole_batch() {
        let slot = 7u64;
        let bodies: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i + 1; 500]).collect();
        let pkts: Vec<Vec<u8>> = (0..4u32).map(|i| data_shred(slot, i, if i == 3 { FLAG_DATA_COMPLETE } else { 0 }, &bodies[i as usize])).collect();
        let full = bodies.concat();
        for streaming in [true, false] {
            let mut d = Deshredder::new(4, 256).streaming(streaming);
            let mut out: Vec<(bool, u32, Vec<u8>)> = Vec::new();
            let mut sink = |i: &BatchInfo, b: &[u8]| out.push((i.complete, i.batch_start, b.to_vec()));
            for (t, i) in [1usize, 0, 2, 3].into_iter().enumerate() {
                d.insert(&Shred::parse(&pkts[i]).unwrap(), t as u64, &mut sink);
            }
            let seen: Vec<(bool, usize)> = out.iter().map(|o| (o.0, o.2.len())).collect();
            let want = if streaming { vec![(false, 1000), (false, 1500), (true, 2000)] } else { vec![(true, 2000)] };
            assert_eq!(seen, want, "streaming {streaming}");
            assert!(out.iter().all(|o| full.starts_with(&o.2) && o.1 == 0));
        }
    }
}
