

use {
    anyhow::{anyhow, Result},
    solana_pubkey::Pubkey,
};

fn slice(data: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    data.get(offset..offset + len).ok_or_else(|| {
        anyhow!("account data too short: need {} bytes at offset {offset}, have {}", len, data.len())
    })
}

pub fn read_u8(data: &[u8], offset: usize) -> Result<u8> {
    Ok(slice(data, offset, 1)?[0])
}

pub fn read_bool(data: &[u8], offset: usize) -> Result<bool> {
    Ok(read_u8(data, offset)? != 0)
}

pub fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(slice(data, offset, 2)?.try_into()?))
}

pub fn read_i32(data: &[u8], offset: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(slice(data, offset, 4)?.try_into()?))
}

pub fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(slice(data, offset, 4)?.try_into()?))
}

pub fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(slice(data, offset, 8)?.try_into()?))
}

pub fn read_u128(data: &[u8], offset: usize) -> Result<u128> {
    Ok(u128::from_le_bytes(slice(data, offset, 16)?.try_into()?))
}

pub fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey> {
    let bytes: [u8; 32] = slice(data, offset, 32)?.try_into()?;
    Ok(Pubkey::new_from_array(bytes))
}


pub fn mint_decimals(mint_data: &[u8]) -> Result<u8> {
    read_u8(mint_data, 44)
}


pub fn mint_authority(mint_data: &[u8]) -> Result<Option<Pubkey>> {
    match read_u32(mint_data, 0)? {
        0 => Ok(None),
        _ => Ok(Some(read_pubkey(mint_data, 4)?)),
    }
}



#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferFee {
    pub older: TransferFeeParams,
    pub newer: TransferFeeParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferFeeParams {
    pub epoch: u64,
    pub maximum_fee: u64,
    pub basis_points: u16,
}

impl TransferFee {
    
    pub fn fixed(basis_points: u16, maximum_fee: u64) -> Self {
        let params = TransferFeeParams { epoch: 0, maximum_fee, basis_points };
        Self { older: params, newer: params }
    }

    
    pub fn fee(&self, epoch: u64, amount: u64) -> u64 {
        let p = if epoch >= self.newer.epoch { self.newer } else { self.older };
        if p.basis_points == 0 || amount == 0 {
            return 0;
        }
        let fee = (amount as u128 * p.basis_points as u128).div_ceil(10_000);
        fee.min(p.maximum_fee as u128) as u64
    }
}

const MINT_BASE_LEN: usize = 82;
const ACCOUNT_TYPE_OFFSET: usize = 165;
const EXTENSION_TRANSFER_FEE_CONFIG: u16 = 1;


pub fn transfer_fee_config(mint_data: &[u8]) -> Option<TransferFee> {
    if mint_data.len() <= MINT_BASE_LEN || mint_data.get(ACCOUNT_TYPE_OFFSET) != Some(&1) {
        return None;
    }
    let mut offset = ACCOUNT_TYPE_OFFSET + 1;
    while offset + 4 <= mint_data.len() {
        let kind = read_u16(mint_data, offset).ok()?;
        let len = read_u16(mint_data, offset + 2).ok()? as usize;
        let value = mint_data.get(offset + 4..offset + 4 + len)?;
        if kind == 0 {
            return None;
        }
        if kind == EXTENSION_TRANSFER_FEE_CONFIG && len >= 108 {
            let params = |at: usize| -> Option<TransferFeeParams> {
                Some(TransferFeeParams {
                    epoch: read_u64(value, at).ok()?,
                    maximum_fee: read_u64(value, at + 8).ok()?,
                    basis_points: read_u16(value, at + 16).ok()?,
                })
            };
            let fee = TransferFee { older: params(72)?, newer: params(90)? };
            let active = fee.older.basis_points > 0 || fee.newer.basis_points > 0;
            return active.then_some(fee);
        }
        offset += 4 + len;
    }
    None
}


pub fn freeze_authority(mint_data: &[u8]) -> Result<Option<Pubkey>> {
    match read_u32(mint_data, 46)? {
        0 => Ok(None),
        _ => Ok(Some(read_pubkey(mint_data, 50)?)),
    }
}


pub struct BorshReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> BorshReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let bytes = slice(self.data, self.offset, len)?;
        self.offset += len;
        Ok(bytes)
    }

    pub fn string(&mut self) -> Result<String> {
        let len = u32::from_le_bytes(self.take(4)?.try_into()?) as usize;
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    pub fn pubkey(&mut self) -> Result<Pubkey> {
        Ok(Pubkey::new_from_array(self.take(32)?.try_into()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.take(1)?[0] != 0)
    }
}
