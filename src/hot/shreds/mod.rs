//! The shred engine in the bot's own process (ENGINE_INPROC), ported from pumpfeed: shreds from
//! the ShredStream proxy (its UDP output, and packet capture of its input port, whichever copy
//! arrives first) and, when a seat sends them, Helius raw shreds (ENGINE_HELIUS_SHRED_BIND), slot reassembly with Reed-Solomon recovery, entry batches, and from them the
//! hot path's launches and buys (`hot::engine::run_inproc`), the slow path's batches and the
//! blockhash from shreds (`book`). No hop through another process.

pub mod book;
pub mod deshred;
pub mod net;
pub mod shred;
pub mod walk;

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// pumpfeed's ingest counters (its dashboard's feed figures), for `hot_stats.log` and the bot's
/// dashboard.
pub mod counters {
    use std::sync::atomic::AtomicU64;
    /// Packets by path: the proxy's output (UDP), capture of its input, Helius raw shreds
    /// (ENGINE_HELIUS_SHRED_BIND).
    pub static PACKETS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
    /// Which path delivered a shred first.
    pub static FIRST: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
    pub static BAD_PACKETS: AtomicU64 = AtomicU64::new(0);
    pub static BATCHES: AtomicU64 = AtomicU64::new(0);
    pub static BAD_BATCHES: AtomicU64 = AtomicU64::new(0);
    pub static PUMP_BATCHES: AtomicU64 = AtomicU64::new(0);
    pub static RECOVERED_SHREDS: AtomicU64 = AtomicU64::new(0);
    pub static INCOMPLETE_SLOTS: AtomicU64 = AtomicU64::new(0);
    pub static LAST_PACKET_NS: AtomicU64 = AtomicU64::new(0);
}

/// Shreds both AllenHark's stream (the proxy's output or capture of its input) and Helius raw
/// shreds delivered: which came first and by how much, by the slot leader's client class. Counted
/// only while Helius raw shreds arrive.
pub mod source_race {
    use {crate::hot::clients::CLASSES, std::sync::atomic::AtomicU64};
    /// Upper bounds of the lead buckets (µs); the last bucket is everything above.
    pub const BUCKETS_US: [u64; 7] = [100, 500, 1_000, 2_000, 5_000, 10_000, 20_000];
    /// [class][0: AllenHark first, 1: Helius first]
    pub static FIRST: [[AtomicU64; 2]; CLASSES] = [const { [const { AtomicU64::new(0) }; 2] }; CLASSES];
    pub static LEAD_US: [[AtomicU64; 2]; CLASSES] = [const { [const { AtomicU64::new(0) }; 2] }; CLASSES];
    pub static HIST: [[[AtomicU64; 8]; 2]; CLASSES] = [const { [const { [const { AtomicU64::new(0) }; 8] }; 2] }; CLASSES];

    /// One shred both sources delivered: `helius_first`, by `lead_ns`, in a slot of `class`.
    #[inline]
    pub fn note(class: usize, helius_first: bool, lead_ns: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let w = usize::from(helius_first);
        let us = lead_ns / 1_000;
        FIRST[class][w].fetch_add(1, Relaxed);
        LEAD_US[class][w].fetch_add(us, Relaxed);
        let b = BUCKETS_US.iter().position(|&up| us < up).unwrap_or(BUCKETS_US.len());
        HIST[class][w][b].fetch_add(1, Relaxed);
    }

    /// One line per class with shreds from both sources (for the stats log).
    pub fn report() -> Option<String> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut parts = Vec::new();
        for (c, name) in crate::hot::clients::NAMES.iter().enumerate() {
            let (a, h) = (FIRST[c][0].load(Relaxed), FIRST[c][1].load(Relaxed));
            if a + h == 0 {
                continue;
            }
            let mean = |w: usize, n: u64| if n == 0 { 0.0 } else { LEAD_US[c][w].load(Relaxed) as f64 / n as f64 / 1e3 };
            parts.push(format!("{name}: AllenHark first {a} (by {:.2} ms on average), Helius first {h} (by {:.2} ms)", mean(0, a), mean(1, h)));
        }
        (!parts.is_empty()).then(|| format!("shred sources: {}", parts.join("; ")))
    }
}

/// Feed health as pumpfeed's dashboard showed it: ok while a shred came in the last second.
pub fn feed_ok(now: u64) -> (bool, Option<f64>) {
    let last = counters::LAST_PACKET_NS.load(Relaxed);
    if last == 0 {
        return (false, None);
    }
    let age_ms = now.saturating_sub(last) as f64 / 1e6;
    (age_ms < 1_000.0, Some(age_ms))
}

/// One line for the stats log.
pub fn report(now: u64) -> String {
    let g = |a: &AtomicU64| a.load(Relaxed);
    use counters::*;
    let (ok, age) = feed_ok(now);
    format!(
        "shreds in-process: feed {} (last shred {}), packets proxy-output {} / capture {} / Helius raw {} (first: {} / {} / {}), bad {}, batches {} ({} with pump.fun, {} bad), recovered shreds {}, incomplete slots {}",
        if ok { "ok" } else { "DOWN" },
        age.map_or("never".into(), |a| format!("{a:.1} ms ago")),
        g(&PACKETS[0]),
        g(&PACKETS[1]),
        g(&PACKETS[2]),
        g(&FIRST[0]),
        g(&FIRST[1]),
        g(&FIRST[2]),
        g(&BAD_PACKETS),
        g(&BATCHES),
        g(&PUMP_BATCHES),
        g(&BAD_BATCHES),
        g(&RECOVERED_SHREDS),
        g(&INCOMPLETE_SLOTS)
    )
}

#[cfg(test)]
pub mod testkit {
    //! Shreds cut from an entry batch the way a leader cuts them: chained Merkle data shreds of one
    //! FEC set (the last one flagged last-in-slot) and as many Reed-Solomon coding shreds.

    use super::shred::{erasure_shard_range, CODE_SHRED_PAYLOAD, DATA_SHRED_PAYLOAD, FLAG_DATA_COMPLETE, FLAG_LAST_IN_SLOT, SIZE_OF_DATA_HEADERS};
    use reed_solomon_erasure::galois_8::ReedSolomon;

    const VARIANT_DATA: u8 = 0x93; // chained Merkle data, proof size 3
    const VARIANT_CODE: u8 = 0x63;
    /// Data bytes per shred: payload less headers, the chained root and a 3-entry proof.
    const PER_SHRED: usize = DATA_SHRED_PAYLOAD - 92 - SIZE_OF_DATA_HEADERS;

    fn common(p: &mut [u8], variant: u8, slot: u64, index: u32) {
        p[64] = variant;
        p[65..73].copy_from_slice(&slot.to_le_bytes());
        p[73..77].copy_from_slice(&index.to_le_bytes());
        p[77..79].copy_from_slice(&1u16.to_le_bytes());
        p[79..83].copy_from_slice(&0u32.to_le_bytes());
    }

    /// (data shreds, coding shreds) for `batch` in `slot`.
    pub fn shreds(slot: u64, batch: &[u8], last_in_slot: bool) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let chunks: Vec<&[u8]> = batch.chunks(PER_SHRED).collect();
        let nd = chunks.len();
        let data: Vec<Vec<u8>> = chunks
            .iter()
            .enumerate()
            .map(|(i, body)| {
                let mut p = vec![0u8; DATA_SHRED_PAYLOAD];
                common(&mut p, VARIANT_DATA, slot, i as u32);
                p[83..85].copy_from_slice(&1u16.to_le_bytes());
                p[85] = if i + 1 < nd { 0 } else if last_in_slot { FLAG_LAST_IN_SLOT } else { FLAG_DATA_COMPLETE };
                p[86..88].copy_from_slice(&((SIZE_OF_DATA_HEADERS + body.len()) as u16).to_le_bytes());
                p[88..88 + body.len()].copy_from_slice(body);
                p
            })
            .collect();
        let nc = nd;
        let rd = erasure_shard_range(true, 3, false).unwrap();
        let rc = erasure_shard_range(false, 3, false).unwrap();
        let mut shards: Vec<Vec<u8>> = data.iter().map(|d| d[rd.clone()].to_vec()).collect();
        shards.extend((0..nc).map(|_| vec![0u8; rd.len()]));
        ReedSolomon::new(nd, nc).unwrap().encode(&mut shards).unwrap();
        let code = (0..nc)
            .map(|pos| {
                let mut p = vec![0u8; CODE_SHRED_PAYLOAD];
                common(&mut p, VARIANT_CODE, slot, pos as u32);
                p[83..85].copy_from_slice(&(nd as u16).to_le_bytes());
                p[85..87].copy_from_slice(&(nc as u16).to_le_bytes());
                p[87..89].copy_from_slice(&(pos as u16).to_le_bytes());
                p[rc.clone()].copy_from_slice(&shards[nd + pos]);
                p
            })
            .collect();
        (data, code)
    }
}
