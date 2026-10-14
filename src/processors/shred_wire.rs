//! Transactions straight from wire bytes: a deshredded entry batch (bincode `Vec<Entry>`, as the
//! Jito ShredStream proxy serves it) or a single transaction. Legacy, v0 and v1 messages. Only
//! what the launch parser needs: the first signature, the static account keys, the top-level
//! instructions and the v0 lookup-table positions (resolved from `lookup_tables`' cache).
//!
//! v1 layout (solana-message 4.x): `0x81`, header (3), config mask (u32 LE), lifetime (32),
//! instruction count (u8), address count (u8), addresses, config values, 4-byte instruction
//! headers (program index, account count, data length u16 LE), each instruction's accounts and
//! data, then the signatures (64 bytes each, as many as the header's required signatures).

use {crate::instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID, solana_pubkey::Pubkey};

/// pump.fun `create` and `create_v2`: a launch whose lookup tables must be loaded to read it.
const CREATE_DISCRIMINATOR: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
const CREATE_V2_DISCRIMINATOR: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];

const V0_PREFIX: u8 = 0x80;
const V1_PREFIX: u8 = 0x81;
/// v1 config mask bits: priority fee (both of bits 0-1, u64), compute unit limit (u32),
/// loaded accounts data size (u32), heap size (u32).
const V1_PRIORITY_FEE: u32 = 0b11;
const V1_COMPUTE_UNIT_LIMIT: u32 = 0b100;
const V1_LOADED_DATA_SIZE: u32 = 0b1000;
const V1_HEAP_SIZE: u32 = 0b1_0000;
const V1_KNOWN_BITS: u32 = 0b1_1111;

pub struct WireInstruction<'a> {
    pub program_index: u8,
    pub accounts: &'a [u8],
    pub data: &'a [u8],
}

/// One v0 lookup: the table and the positions of the accounts it loads.
pub struct WireLookup<'a> {
    pub table: Pubkey,
    pub writable: &'a [u8],
    pub readonly: &'a [u8],
}

pub struct WireTx<'a> {
    pub signature: [u8; 64],
    pub keys: Vec<Pubkey>,
    pub instructions: Vec<WireInstruction<'a>>,
    pub lookups: Vec<WireLookup<'a>>,
}

impl WireTx<'_> {
    pub fn mentions(&self, program: &Pubkey) -> bool {
        self.keys.contains(program)
    }

    /// Top-level instructions as (program, accounts, data). Accounts from lookup tables come
    /// from the tables' cache; one not cached yet is `None` (and its table gets fetched).
    pub fn top_instructions(&self) -> Vec<super::shred_feed::TopInstruction<'_>> {
        if !self.lookups.is_empty() && self.creates_a_pump_coin() {
            self.lookups.iter().for_each(|l| super::lookup_tables::ensure(&l.table));
        }
        self.instructions
            .iter()
            .filter_map(|ix| {
                let program = *self.keys.get(ix.program_index as usize)?;
                let accounts = ix.accounts.iter().map(|i| self.account(*i as usize)).collect();
                Some((program, accounts, ix.data))
            })
            .collect()
    }

    fn creates_a_pump_coin(&self) -> bool {
        self.instructions.iter().any(|ix| {
            self.keys.get(ix.program_index as usize) == Some(&PUMP_PROGRAM_ID)
                && ix.data.get(..8).is_some_and(|d| d == CREATE_V2_DISCRIMINATOR || d == CREATE_DISCRIMINATOR)
        })
    }

    /// The account at `index`: the static keys, then every lookup's writable positions, then
    /// every lookup's read-only positions (the v0 order).
    fn account(&self, index: usize) -> Option<Pubkey> {
        if let Some(key) = self.keys.get(index) {
            return Some(*key);
        }
        let mut rest = index - self.keys.len();
        for readonly in [false, true] {
            for lookup in &self.lookups {
                let positions = if readonly { lookup.readonly } else { lookup.writable };
                match positions.get(rest) {
                    Some(position) => return super::lookup_tables::resolve(&lookup.table, *position),
                    None => rest -= positions.len(),
                }
            }
        }
        None
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// Compact-u16 length.
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

    fn keys(&mut self, n: usize) -> Option<Vec<Pubkey>> {
        (0..n).map(|_| Pubkey::try_from(self.take(32)?).ok()).collect()
    }
}

/// Every transaction of a bincode `Vec<Entry>` payload (entry: num_hashes u64, hash 32,
/// transactions as a u64-counted list). `None` when the bytes do not parse.
pub fn entry_transactions(payload: &[u8]) -> Option<Vec<WireTx<'_>>> {
    let mut r = Reader { bytes: payload, at: 0 };
    let entries = r.u64()?;
    let mut out = Vec::new();
    for _ in 0..entries {
        r.take(8 + 32)?;
        let count = r.u64()?;
        // A 4 KB transaction limit bounds what a batch can hold; anything larger is garbage.
        if count as usize > payload.len() / 64 {
            return None;
        }
        for _ in 0..count {
            out.push(transaction(&mut r)?);
        }
    }
    Some(out)
}

/// One transaction's wire bytes.
pub fn decode_transaction(bytes: &[u8]) -> Option<WireTx<'_>> {
    transaction(&mut Reader { bytes, at: 0 })
}

fn transaction<'a>(r: &mut Reader<'a>) -> Option<WireTx<'a>> {
    if r.peek()? == V1_PREFIX {
        r.at += 1;
        let header = r.take(3)?;
        let mask = r.u32()?;
        let priority_bits = mask & V1_PRIORITY_FEE;
        if mask & !V1_KNOWN_BITS != 0 || (priority_bits != 0 && priority_bits != V1_PRIORITY_FEE) {
            return None;
        }
        r.take(32)?;
        let instruction_count = r.u8()? as usize;
        let key_count = r.u8()? as usize;
        let keys = r.keys(key_count)?;
        let config = [(V1_PRIORITY_FEE, 8), (V1_COMPUTE_UNIT_LIMIT, 4), (V1_LOADED_DATA_SIZE, 4), (V1_HEAP_SIZE, 4)]
            .iter()
            .filter(|(bit, _)| mask & bit == *bit)
            .map(|(_, size)| size)
            .sum();
        r.take(config)?;
        let headers = r.take(4 * instruction_count)?;
        let mut instructions = Vec::with_capacity(instruction_count);
        for h in headers.chunks_exact(4) {
            let accounts = r.take(h[1] as usize)?;
            let data = r.take(u16::from_le_bytes([h[2], h[3]]) as usize)?;
            instructions.push(WireInstruction { program_index: h[0], accounts, data });
        }
        let signatures = r.take(64 * header[0] as usize)?;
        return Some(WireTx { signature: signatures.get(..64)?.try_into().ok()?, keys, instructions, lookups: Vec::new() });
    }
    let signature_count = r.short()?;
    let signatures = r.take(64 * signature_count)?;
    let versioned = match r.peek()? {
        V0_PREFIX => {
            r.at += 1;
            true
        }
        b if b & 0x80 != 0 => return None,
        _ => false,
    };
    r.take(3)?;
    let key_count = r.short()?;
    let keys = r.keys(key_count)?;
    r.take(32)?;
    let instruction_count = r.short()?;
    let mut instructions = Vec::with_capacity(instruction_count);
    for _ in 0..instruction_count {
        let program_index = r.u8()?;
        let n = r.short()?;
        let accounts = r.take(n)?;
        let n = r.short()?;
        let data = r.take(n)?;
        instructions.push(WireInstruction { program_index, accounts, data });
    }
    let mut lookups = Vec::new();
    if versioned {
        for _ in 0..r.short()? {
            let table = Pubkey::try_from(r.take(32)?).ok()?;
            let n = r.short()?;
            let writable = r.take(n)?;
            let n = r.short()?;
            let readonly = r.take(n)?;
            lookups.push(WireLookup { table, writable, readonly });
        }
    }
    Some(WireTx { signature: signatures.get(..64)?.try_into().ok()?, keys, instructions, lookups })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{instructions::pumpswap_buy_ix::PUMP_PROGRAM_ID, processors::shred_feed::parse_launches},
        base64::Engine,
        std::str::FromStr,
    };

    /// Real pump.fun create transactions from 24 Sep 2026, one per wire format.
    fn fixtures() -> Vec<(String, serde_json::Value, Vec<u8>)> {
        let all: serde_json::Value = serde_json::from_str(include_str!("testdata/pump_creates.json")).unwrap();
        all.as_object()
            .unwrap()
            .iter()
            .map(|(version, f)| {
                let bytes = base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap();
                (version.clone(), f.clone(), bytes)
            })
            .collect()
    }

    fn key(f: &serde_json::Value, name: &str) -> Pubkey {
        Pubkey::from_str(f[name].as_str().unwrap()).unwrap()
    }

    #[test]
    fn every_wire_format_decodes_to_its_launch() {
        let fixtures = fixtures();
        assert_eq!(fixtures.len(), 3, "legacy, v0 and v1");
        for (version, f, bytes) in &fixtures {
            let tx = decode_transaction(bytes).unwrap_or_else(|| panic!("{version} did not decode"));
            assert!(tx.mentions(&PUMP_PROGRAM_ID), "{version}");
            let launches = parse_launches(&tx.top_instructions());
            assert_eq!(launches.len(), 1, "{version}");
            assert_eq!(launches[0].mint, key(f, "mint"), "{version}");
            assert_eq!(launches[0].deployer, key(f, "deployer"), "{version}");
            // Shreds only show top-level instructions: a dev buy made through another program is 0.
            assert_eq!(launches[0].dev_buy_lamports > 0, f["dev_buy_top_level"] == true, "{version}");
            if let Some(signature) = f["signature"].as_str() {
                assert_eq!(bs58::encode(tx.signature).into_string(), signature, "{version}");
            }
        }
    }

    /// A 19-account create_v2 (26 Sep 2026) whose wrapped SOL, a PDA and the token program come
    /// from a lookup table: unreadable until the table is cached, then a normal launch.
    #[test]
    fn a_launch_using_a_lookup_table_parses_once_the_table_is_cached() {
        // Other tests cache this table too: take turns, and start from an empty cache.
        let _state = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let f: serde_json::Value = serde_json::from_str(include_str!("testdata/pump_create_lookup.json")).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(f["tx_base64"].as_str().unwrap()).unwrap();
        let tx = decode_transaction(&bytes).expect("decodes");
        assert_eq!(tx.lookups.len(), 1);
        for table in f["tables"].as_object().unwrap().keys() {
            crate::processors::lookup_tables::forget(&Pubkey::from_str(table).unwrap());
        }
        assert!(parse_launches(&tx.top_instructions()).is_empty(), "the quote mint is unknown without the table");
        for (table, addresses) in f["tables"].as_object().unwrap() {
            let addresses = addresses.as_array().unwrap().iter().map(|a| Pubkey::from_str(a.as_str().unwrap()).unwrap()).collect();
            crate::processors::lookup_tables::insert(Pubkey::from_str(table).unwrap(), addresses);
        }
        let launches = parse_launches(&tx.top_instructions());
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].mint, key(&f, "mint"));
        assert_eq!(launches[0].deployer, key(&f, "deployer"));
        assert!(launches[0].dev_buy_lamports > 0, "the dev buy is a top-level buy_exact_sol_in");
    }

    #[test]
    fn a_udp_batch_is_the_slot_then_the_entries() {
        let fixtures = fixtures();
        let mut payload = 1u64.to_le_bytes().to_vec();
        payload.extend(7u64.to_le_bytes());
        payload.extend([9u8; 32]);
        payload.extend((fixtures.len() as u64).to_le_bytes());
        for (_, _, bytes) in &fixtures {
            payload.extend(bytes);
        }
        let mut datagram = 450_747_501u64.to_le_bytes().to_vec();
        datagram.extend(&payload);
        let (slot, entries) = crate::processors::shred_feed::udp_batch(&datagram).unwrap();
        assert_eq!(slot, 450_747_501);
        assert_eq!(entry_transactions(entries).unwrap().len(), fixtures.len());
        assert!(crate::processors::shred_feed::udp_batch(&[1, 2, 3]).is_none());
    }

    #[test]
    fn an_entry_batch_splits_into_its_transactions() {
        let fixtures = fixtures();
        // Two entries: the first with all three transactions, the second empty.
        let mut payload = 2u64.to_le_bytes().to_vec();
        payload.extend(7u64.to_le_bytes());
        payload.extend([9u8; 32]);
        payload.extend((fixtures.len() as u64).to_le_bytes());
        for (_, _, bytes) in &fixtures {
            payload.extend(bytes);
        }
        payload.extend(0u64.to_le_bytes());
        payload.extend([1u8; 32]);
        payload.extend(0u64.to_le_bytes());
        let txs = entry_transactions(&payload).unwrap();
        assert_eq!(txs.len(), fixtures.len());
        for (tx, (_, f, bytes)) in txs.iter().zip(&fixtures) {
            assert_eq!(tx.signature, decode_transaction(bytes).unwrap().signature);
            assert_eq!(parse_launches(&tx.top_instructions())[0].mint, key(f, "mint"));
        }
        // A cut batch does not parse.
        assert!(entry_transactions(&payload[..payload.len() - 50]).is_none());
    }

    #[test]
    fn buys_later_in_the_batch_move_the_quote() {
        use crate::{
            instructions::pumpfun_buy_ix::CurveReserves,
            processors::shred_feed::{apply_buy, curve_after, pump_buys},
        };
        let (_, f, bytes) = fixtures().into_iter().find(|(v, _, _)| v == "1").unwrap();
        let tx = decode_transaction(&bytes).unwrap();
        let ixs = tx.top_instructions();
        let launch = parse_launches(&ixs).remove(0);
        let buys = pump_buys(&ixs, &key(&f, "mint"));
        assert_eq!(buys.iter().map(|b| b.1).sum::<u64>(), launch.dev_buy_lamports);
        let initial = CurveReserves {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_sol_reserves: 30_000_000_000,
            real_token_reserves: 793_100_000_000_000,
            real_sol_reserves: 0,
        };
        let after_dev = curve_after(&launch, initial, 125);
        // The same buy again, as if a bundle wallet repeated it: the price only goes up.
        let after_bundle = buys.iter().fold(after_dev, |r, (t, l)| apply_buy(r, *t, *l, 125));
        let price = |r: &CurveReserves| r.virtual_sol_reserves as f64 / r.virtual_token_reserves as f64;
        assert!(price(&after_bundle) > price(&after_dev));
        assert!(pump_buys(&ixs, &Pubkey::new_unique()).is_empty());
    }

    #[test]
    fn v1_signatures_come_after_the_message() {
        let (_, _, bytes) = fixtures().into_iter().find(|(v, _, _)| v == "1").unwrap();
        let tx = decode_transaction(&bytes).unwrap();
        // Two signers: the fee payer's signature is the first of the two at the end.
        let signers = bytes[1] as usize;
        assert_eq!(&tx.signature[..], &bytes[bytes.len() - 64 * signers..][..64]);
    }
}
