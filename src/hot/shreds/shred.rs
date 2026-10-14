//! Zero-copy view over a raw Merkle shred as it arrives off the wire. Ported from pumpfeed
//! (`solana/shred.rs`), field reads by offset instead of `bytemuck`.
//!
//! Wire layout (all integers little-endian), per Agave `ledger/src/shred`:
//!
//! ```text
//! common header (83 bytes)
//!   0..64   signature        (leader's signature over the Merkle root)
//!   64      shred_variant    (high nibble = type, low nibble = Merkle proof size)
//!   65..73  slot             u64
//!   73..77  index            u32
//!   77..79  shred_version    u16
//!   79..83  fec_set_index    u32
//! data header (5 bytes)                 | coding header (6 bytes)
//!   83..85  parent_offset   u16         |   83..85 num_data_shreds   u16
//!   85      flags           u8          |   85..87 num_coding_shreds u16
//!   86..88  size            u16         |   87..89 position          u16
//! ```
//!
//! After the headers come the data (or parity) bytes, then optionally a
//! chained Merkle root (32 B), then the Merkle proof (20 B * proof_size),
//! then optionally the retransmitter signature (64 B).
//!
//! Only Merkle shreds are handled. Legacy shreds (variants 0x5A/0xA5) were
//! retired from mainnet and are rejected.

use std::ops::Range;

pub const SIZE_OF_SIGNATURE: usize = 64;
pub const SIZE_OF_COMMON_HEADER: usize = 83;
pub const SIZE_OF_DATA_HEADERS: usize = 88;
pub const SIZE_OF_CODE_HEADERS: usize = 89;

/// Payload sizes of Merkle shreds. The packet may carry trailing bytes (for
/// example a repair nonce), which are ignored.
pub const DATA_SHRED_PAYLOAD: usize = 1203;
pub const CODE_SHRED_PAYLOAD: usize = 1228;

const SIZE_OF_MERKLE_PROOF_ENTRY: usize = 20;
const SIZE_OF_MERKLE_ROOT: usize = 32;

/// Last shred of a serialized `Vec<Entry>` batch.
pub const FLAG_DATA_COMPLETE: u8 = 0b0100_0000;
/// Last shred of the slot. Includes the DATA_COMPLETE bit, so test with
/// `flags & FLAG_LAST_IN_SLOT == FLAG_LAST_IN_SLOT`.
pub const FLAG_LAST_IN_SLOT: u8 = 0b1100_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShredKind {
    Data { flags: u8, size: u16 },
    Code { num_data: u16, num_code: u16, position: u16 },
}

/// A validated, borrowed shred. Holds no heap memory; `payload` points into
/// the receive buffer.
#[derive(Clone, Copy, Debug)]
pub struct Shred<'a> {
    /// Exactly `DATA_SHRED_PAYLOAD` or `CODE_SHRED_PAYLOAD` bytes.
    pub payload: &'a [u8],
    pub slot: u64,
    pub index: u32,
    pub fec_set_index: u32,
    pub proof_size: u8,
    pub resigned: bool,
    pub kind: ShredKind,
}

impl<'a> Shred<'a> {
    /// Parses and bounds-checks a packet. Returns `None` for anything that is
    /// not a well-formed Merkle shred. No allocation, no copies.
    #[inline(always)]
    pub fn parse(pkt: &'a [u8]) -> Option<Self> {
        if pkt.len() < SIZE_OF_CODE_HEADERS {
            return None;
        }
        let variant = pkt[64];
        // High nibble: 0x4 code, 0x6 chained code, 0x7 chained+resigned code,
        //              0x8 data, 0x9 chained data, 0xB chained+resigned data.
        let (is_data, chained, resigned) = match variant & 0xF0 {
            0x40 => (false, false, false),
            0x60 => (false, true, false),
            0x70 => (false, true, true),
            0x80 => (true, false, false),
            0x90 => (true, true, false),
            0xB0 => (true, true, true),
            _ => return None,
        };
        let proof_size = variant & 0x0F;
        let slot = u64::from_le_bytes(pkt[65..73].try_into().ok()?);
        let index = u32::from_le_bytes(pkt[73..77].try_into().ok()?);
        let fec_set_index = u32::from_le_bytes(pkt[79..83].try_into().ok()?);
        // Bytes after the data region: [chained root] + proof + [retransmitter sig].
        let trailer = proof_size as usize * SIZE_OF_MERKLE_PROOF_ENTRY
            + if chained { SIZE_OF_MERKLE_ROOT } else { 0 }
            + if resigned { SIZE_OF_SIGNATURE } else { 0 };

        if is_data {
            let payload = pkt.get(..DATA_SHRED_PAYLOAD)?;
            let flags = payload[85];
            let size = u16::from_le_bytes([payload[86], payload[87]]);
            // `size` = headers + data bytes; it must stay inside the data capacity.
            let data_end_max = DATA_SHRED_PAYLOAD.checked_sub(trailer)?;
            if (size as usize) < SIZE_OF_DATA_HEADERS || size as usize > data_end_max {
                return None;
            }
            if index < fec_set_index {
                return None;
            }
            Some(Self { payload, slot, index, fec_set_index, proof_size, resigned, kind: ShredKind::Data { flags, size } })
        } else {
            let payload = pkt.get(..CODE_SHRED_PAYLOAD)?;
            let num_data = u16::from_le_bytes([payload[83], payload[84]]);
            let num_code = u16::from_le_bytes([payload[85], payload[86]]);
            let position = u16::from_le_bytes([payload[87], payload[88]]);
            // GF(2^8) Reed-Solomon caps the total shard count at 256.
            if num_data == 0 || num_code == 0 || num_data as usize + num_code as usize > 256 || position >= num_code || index < position as u32 {
                return None;
            }
            erasure_shard_range(false, proof_size, resigned)?;
            Some(Self { payload, slot, index, fec_set_index, proof_size, resigned, kind: ShredKind::Code { num_data, num_code, position } })
        }
    }
}

/// The byte range of a payload that is Reed-Solomon coded.
///
/// For data shreds the shard is everything after the signature up to the
/// Merkle proof, so recovering it also recovers the data headers. For coding
/// shreds it is everything after the coding headers up to the proof. The
/// chained Merkle root is inside the shard; the retransmitter signature is
/// not. Both ranges have the same length, `1139 - 20*proof - 64*resigned`.
#[inline(always)]
pub fn erasure_shard_range(is_data: bool, proof_size: u8, resigned: bool) -> Option<Range<usize>> {
    let (start, payload) = if is_data { (SIZE_OF_SIGNATURE, DATA_SHRED_PAYLOAD) } else { (SIZE_OF_CODE_HEADERS, CODE_SHRED_PAYLOAD) };
    let end = payload.checked_sub(proof_size as usize * SIZE_OF_MERKLE_PROOF_ENTRY)?.checked_sub(if resigned { SIZE_OF_SIGNATURE } else { 0 })?;
    (end > start).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_and_code_headers_and_refuses_the_rest() {
        let mut d = vec![0u8; DATA_SHRED_PAYLOAD + 40];
        d[64] = 0x93;
        d[65..73].copy_from_slice(&7u64.to_le_bytes());
        d[73..77].copy_from_slice(&33u32.to_le_bytes());
        d[79..83].copy_from_slice(&32u32.to_le_bytes());
        d[85] = FLAG_DATA_COMPLETE;
        d[86..88].copy_from_slice(&200u16.to_le_bytes());
        let s = Shred::parse(&d).unwrap();
        assert_eq!((s.slot, s.index, s.fec_set_index, s.proof_size, s.resigned), (7, 33, 32, 3, false));
        assert_eq!(s.kind, ShredKind::Data { flags: FLAG_DATA_COMPLETE, size: 200 });
        assert_eq!(s.payload.len(), DATA_SHRED_PAYLOAD, "trailing bytes ignored");
        d[86..88].copy_from_slice(&40u16.to_le_bytes());
        assert!(Shred::parse(&d).is_none(), "size below the headers");
        let mut c = vec![0u8; CODE_SHRED_PAYLOAD];
        c[64] = 0x63;
        c[73..77].copy_from_slice(&5u32.to_le_bytes());
        c[83..85].copy_from_slice(&32u16.to_le_bytes());
        c[85..87].copy_from_slice(&32u16.to_le_bytes());
        c[87..89].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(Shred::parse(&c).unwrap().kind, ShredKind::Code { num_data: 32, num_code: 32, position: 5 });
        c[64] = 0x5A;
        assert!(Shred::parse(&c).is_none(), "legacy shreds are refused");
    }
}
