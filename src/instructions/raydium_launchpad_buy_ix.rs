


use {
    super::types::{fetch_accounts, sol_buy_wrapper},
    crate::{
        error::SniperError,
        utils::{
            blockhash::current_epoch,
            parse::{read_pubkey, read_u64, read_u8, transfer_fee_config, TransferFee},
            swap_quote::mul_div_ceil,
            utils::{get_associated_token_address, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    anyhow::{bail, Result},
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
    },
};

pub const RAYDIUM_LAUNCHPAD_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
pub const RAYDIUM_LAUNCHPAD_AUTHORITY: Pubkey =
    Pubkey::from_str_const("WLHv2UAZm6z4KyaaELi5pjdbJh6RESMva1Rnn8pJVVh");
pub const RAYDIUM_LAUNCHPAD_EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("2DPAtwB8L12vrMRExbLuyGnC7n2J5LNoZQSejeQGpwkr");

const BUY_EXACT_IN_DISCRIMINATOR: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236];
const FEE_RATE_DENOMINATOR: u64 = 1_000_000;
const POOL_STATE_LEN: usize = 429;

const STATUS_FUND: u8 = 0;

pub const CURVE_CONSTANT_PRODUCT: u8 = 0;


static GLOBAL_TRADE_FEE: LazyLock<Mutex<HashMap<Pubkey, u64>>> = LazyLock::new(Default::default);
static PLATFORM_FEES: LazyLock<Mutex<HashMap<Pubkey, (u64, u64)>>> = LazyLock::new(Default::default);

#[derive(Debug, Clone)]
pub struct RaydiumLaunchpadMarket {
    pub pool: Pubkey,
    pub global_config: Pubkey,
    pub platform_config: Pubkey,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_token_program: Pubkey,
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub real_base: u64,
    pub real_quote: u64,
    pub total_base_sell: u64,
    pub status: u8,
    pub trade_fee_rate: u64,
    pub platform_fee_rate: u64,
    pub creator_fee_rate: u64,
    pub base_transfer_fee: Option<TransferFee>,
    pub fees_loaded: bool,
}


#[derive(Debug, Clone, Copy)]
pub struct LaunchpadCurve {
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub real_base: u64,
    pub real_quote: u64,
    pub total_base_sell: u64,
}

impl RaydiumLaunchpadMarket {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Pubkey,
        global_config: Pubkey,
        platform_config: Pubkey,
        creator: Pubkey,
        base_mint: Pubkey,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        base_token_program: Pubkey,
        curve: LaunchpadCurve,
        base_transfer_fee: Option<TransferFee>,
    ) -> Self {
        Self {
            pool,
            global_config,
            platform_config,
            creator,
            base_mint,
            base_vault,
            quote_vault,
            base_token_program,
            virtual_base: curve.virtual_base,
            virtual_quote: curve.virtual_quote,
            real_base: curve.real_base,
            real_quote: curve.real_quote,
            total_base_sell: curve.total_base_sell,
            status: STATUS_FUND,
            trade_fee_rate: 0,
            platform_fee_rate: 0,
            creator_fee_rate: 0,
            base_transfer_fee,
            fees_loaded: false,
        }
    }

    pub fn mint(&self) -> Pubkey {
        self.base_mint
    }

    pub fn sol_liquidity(&self) -> u64 {
        self.real_quote
    }

    fn total_fee_rate(&self) -> u64 {
        self.trade_fee_rate + self.platform_fee_rate + self.creator_fee_rate
    }

    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        self.ensure_tradable()?;
        let fee = mul_div_ceil(lamports_in, self.total_fee_rate(), FEE_RATE_DENOMINATOR);
        let net = lamports_in.saturating_sub(fee) as u128;
        let base_left = self.virtual_base.saturating_sub(self.real_base) as u128;
        let quote_total = self.virtual_quote as u128 + self.real_quote as u128;
        let mut out = (net * base_left / (quote_total + net)) as u64;
        
        out = out.min(self.total_base_sell.saturating_sub(self.real_base));
        let tf = self.base_transfer_fee.map_or(0, |f| f.fee(current_epoch(), out));
        Ok(out - tf)
    }

    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        self.ensure_tradable()?;
        let tf = self.base_transfer_fee.map_or(0, |f| f.fee(current_epoch(), tokens_in));
        let in_b = (tokens_in - tf) as u128;
        let base_left = self.virtual_base.saturating_sub(self.real_base) as u128;
        let quote_total = self.virtual_quote as u128 + self.real_quote as u128;
        let gross = (in_b * quote_total / (base_left + in_b)) as u64;
        let fee = mul_div_ceil(gross, self.total_fee_rate(), FEE_RATE_DENOMINATOR);
        Ok(gross.saturating_sub(fee))
    }

    fn ensure_tradable(&self) -> Result<()> {
        if !self.fees_loaded {
            bail!("LaunchLab fee configs for {} not loaded", self.pool);
        }
        if self.status != STATUS_FUND {
            return Err(SniperError::VenueClosed { mint: self.base_mint, venue: "raydium_launchpad" }.into());
        }
        Ok(())
    }

    pub fn platform_fee_vault(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[self.platform_config.as_ref(), WSOL_MINT.as_ref()],
            &RAYDIUM_LAUNCHPAD_PROGRAM_ID,
        )
        .0
    }

    pub fn creator_fee_vault(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[self.creator.as_ref(), WSOL_MINT.as_ref()],
            &RAYDIUM_LAUNCHPAD_PROGRAM_ID,
        )
        .0
    }

    
    pub(super) fn swap_accounts(&self, payer: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(RAYDIUM_LAUNCHPAD_AUTHORITY, false),
            AccountMeta::new_readonly(self.global_config, false),
            AccountMeta::new_readonly(self.platform_config, false),
            AccountMeta::new(self.pool, false),
            AccountMeta::new(get_associated_token_address(payer, &self.base_mint, &self.base_token_program), false),
            AccountMeta::new(get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID), false),
            AccountMeta::new(self.base_vault, false),
            AccountMeta::new(self.quote_vault, false),
            AccountMeta::new_readonly(self.base_mint, false),
            AccountMeta::new_readonly(WSOL_MINT, false),
            AccountMeta::new_readonly(self.base_token_program, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(RAYDIUM_LAUNCHPAD_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(RAYDIUM_LAUNCHPAD_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new(self.platform_fee_vault(), false),
            AccountMeta::new(self.creator_fee_vault(), false),
        ]
    }

    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_out: u64) -> Vec<Instruction> {
        let mut data = Vec::with_capacity(32);
        data.extend_from_slice(&BUY_EXACT_IN_DISCRIMINATOR);
        data.extend_from_slice(&lamports_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); 
        let swap = Instruction {
            program_id: RAYDIUM_LAUNCHPAD_PROGRAM_ID,
            accounts: self.swap_accounts(payer),
            data,
        };
        sol_buy_wrapper(payer, lamports_in, &self.base_mint, &self.base_token_program, swap)
    }

    
    pub async fn load_fees(&mut self, rpc: &RpcClient) -> Result<()> {
        let cached_global = GLOBAL_TRADE_FEE.lock().unwrap_or_else(|e| e.into_inner()).get(&self.global_config).copied();
        let cached_platform = PLATFORM_FEES.lock().unwrap_or_else(|e| e.into_inner()).get(&self.platform_config).copied();
        let (trade_fee, platform) = match (cached_global, cached_platform) {
            (Some(g), Some(p)) => (g, p),
            _ => {
                let accounts = fetch_accounts(rpc, &[self.global_config, self.platform_config]).await?;
                let curve_type = read_u8(&accounts[0].data, 16)?;
                if curve_type != CURVE_CONSTANT_PRODUCT {
                    bail!("LaunchLab config {} uses curve type {curve_type}; only constant product is supported", self.global_config);
                }
                let trade_fee = read_u64(&accounts[0].data, 27)?;
                let platform = (read_u64(&accounts[1].data, 104)?, read_u64(&accounts[1].data, 720)?);
                GLOBAL_TRADE_FEE.lock().unwrap_or_else(|e| e.into_inner()).insert(self.global_config, trade_fee);
                PLATFORM_FEES.lock().unwrap_or_else(|e| e.into_inner()).insert(self.platform_config, platform);
                (trade_fee, platform)
            }
        };
        self.trade_fee_rate = trade_fee;
        (self.platform_fee_rate, self.creator_fee_rate) = platform;
        if self.total_fee_rate() >= FEE_RATE_DENOMINATOR {
            bail!("LaunchLab fee rates for {} add up to 100% or more", self.pool);
        }
        self.fees_loaded = true;
        Ok(())
    }

    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        let accounts = fetch_accounts(rpc, &[self.pool, self.base_mint]).await?;
        let pool = &accounts[0].data;
        if pool.len() != POOL_STATE_LEN {
            return Err(SniperError::InvalidAccountData {
                account: self.pool,
                reason: format!("PoolState is {} bytes", pool.len()),
            }
            .into());
        }
        if read_pubkey(pool, 205)? != self.base_mint || read_pubkey(pool, 237)? != WSOL_MINT {
            bail!("LaunchLab pool {} mints do not match", self.pool);
        }
        self.creator = read_pubkey(pool, 333)?;
        self.status = read_u8(pool, 17)?;
        self.total_base_sell = read_u64(pool, 29)?;
        self.virtual_base = read_u64(pool, 37)?;
        self.virtual_quote = read_u64(pool, 45)?;
        self.real_base = read_u64(pool, 53)?;
        self.real_quote = read_u64(pool, 61)?;
        self.base_transfer_fee = transfer_fee_config(&accounts[1].data);
        if !self.fees_loaded {
            self.load_fees(rpc).await?;
        }
        Ok(())
    }
}

impl RaydiumLaunchpadMarket {
    
    pub async fn from_pool(rpc: &RpcClient, pool: Pubkey) -> Result<Self> {
        let data = fetch_accounts(rpc, &[pool]).await?.remove(0).data;
        if read_pubkey(&data, 237)? != WSOL_MINT {
            bail!("LaunchLab pool {pool} is not quoted in SOL");
        }
        let base_token_program = if read_u8(&data, 365)? & 1 == 1 {
            crate::utils::utils::TOKEN_2022_PROGRAM_ID
        } else {
            TOKEN_PROGRAM_ID
        };
        let empty = LaunchpadCurve { virtual_base: 0, virtual_quote: 0, real_base: 0, real_quote: 0, total_base_sell: 0 };
        let mut market = Self::new(
            pool,
            read_pubkey(&data, 141)?,
            read_pubkey(&data, 173)?,
            read_pubkey(&data, 333)?,
            read_pubkey(&data, 205)?,
            read_pubkey(&data, 269)?,
            read_pubkey(&data, 301)?,
            base_token_program,
            empty,
            None,
        );
        market.refresh(rpc).await?;
        Ok(market)
    }
}
