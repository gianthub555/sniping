

use {
    super::types::{fetch_accounts, sol_buy_wrapper, sol_sell_wrapper},
    crate::{
        error::SniperError,
        utils::{
            blockhash::current_epoch,
            parse::{read_bool, read_pubkey, read_u64, read_u8, transfer_fee_config, TransferFee},
            swap_quote::{constant_product_out, mul_div_ceil},
            utils::{get_associated_token_address, TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    anyhow::{bail, Result},
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
};

pub const RAYDIUM_CPMM_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const RAYDIUM_CPMM_AUTHORITY: Pubkey =
    Pubkey::from_str_const("GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL");

const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];
const POOL_STATE_LEN: usize = 637;
const FEE_RATE_DENOMINATOR: u64 = 1_000_000;

const STATUS_SWAP_DISABLED: u8 = 1 << 2;

#[derive(Debug, Clone)]
pub struct RaydiumCpmmMarket {
    pub pool: Pubkey,
    pub amm_config: Pubkey,
    pub observation: Pubkey,
    pub token0_mint: Pubkey,
    pub token1_mint: Pubkey,
    pub token0_vault: Pubkey,
    pub token1_vault: Pubkey,
    pub token0_program: Pubkey,
    pub token1_program: Pubkey,
    
    pub reserve0: u64,
    pub reserve1: u64,
    pub trade_fee_rate: u64,
    
    pub creator_fee_rate: u64,
    
    pub creator_fee_on: u8,
    pub open_time: u64,
    pub transfer_fee0: Option<TransferFee>,
    pub transfer_fee1: Option<TransferFee>,
    
    pub loaded: bool,
}

impl RaydiumCpmmMarket {
    
    
    #[allow(clippy::too_many_arguments)]
    pub fn from_initialize(
        pool: Pubkey,
        amm_config: Pubkey,
        observation: Pubkey,
        token0_mint: Pubkey,
        token1_mint: Pubkey,
        token0_vault: Pubkey,
        token1_vault: Pubkey,
        token0_program: Pubkey,
        token1_program: Pubkey,
        open_time: u64,
    ) -> Option<Self> {
        if (token0_mint == WSOL_MINT) == (token1_mint == WSOL_MINT) {
            return None;
        }
        Some(Self {
            pool,
            amm_config,
            observation,
            token0_mint,
            token1_mint,
            token0_vault,
            token1_vault,
            token0_program,
            token1_program,
            reserve0: 0,
            reserve1: 0,
            trade_fee_rate: 0,
            creator_fee_rate: 0,
            creator_fee_on: 0,
            open_time,
            transfer_fee0: None,
            transfer_fee1: None,
            loaded: false,
        })
    }

    fn sol_is_token0(&self) -> bool {
        self.token0_mint == WSOL_MINT
    }

    pub fn mint(&self) -> Pubkey {
        if self.sol_is_token0() { self.token1_mint } else { self.token0_mint }
    }

    pub fn token_program(&self) -> Pubkey {
        if self.sol_is_token0() { self.token1_program } else { self.token0_program }
    }

    fn token_transfer_fee(&self) -> Option<TransferFee> {
        if self.sol_is_token0() { self.transfer_fee1 } else { self.transfer_fee0 }
    }

    pub fn sol_liquidity(&self) -> u64 {
        if self.sol_is_token0() { self.reserve0 } else { self.reserve1 }
    }

    
    fn amount_out(&self, amount_in: u64, input_is_token0: bool) -> Result<u64> {
        if !self.loaded {
            bail!("CPMM pool {} state not loaded", self.pool);
        }
        let epoch = current_epoch();
        let (r_in, r_out, fee_in, fee_out) = if input_is_token0 {
            (self.reserve0, self.reserve1, self.transfer_fee0, self.transfer_fee1)
        } else {
            (self.reserve1, self.reserve0, self.transfer_fee1, self.transfer_fee0)
        };
        let in_after_tf = amount_in - fee_in.map_or(0, |f| f.fee(epoch, amount_in));
        let creator_on_input = match self.creator_fee_on {
            0 => true,
            1 => input_is_token0,
            _ => !input_is_token0,
        };

        let out = if creator_on_input {
            let total_rate = self.trade_fee_rate + self.creator_fee_rate;
            let total_fee = mul_div_ceil(in_after_tf, total_rate, FEE_RATE_DENOMINATOR);
            let net = in_after_tf.saturating_sub(total_fee);
            constant_product_out(net, r_in, r_out)
        } else {
            let trade_fee = mul_div_ceil(in_after_tf, self.trade_fee_rate, FEE_RATE_DENOMINATOR);
            let net = in_after_tf.saturating_sub(trade_fee);
            let gross = constant_product_out(net, r_in, r_out);
            gross - mul_div_ceil(gross, self.creator_fee_rate, FEE_RATE_DENOMINATOR)
        };
        Ok(out - fee_out.map_or(0, |f| f.fee(epoch, out)))
    }

    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        self.amount_out(lamports_in, self.sol_is_token0())
    }

    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        self.amount_out(tokens_in, !self.sol_is_token0())
    }

    fn swap_instruction(&self, payer: &Pubkey, sol_in: bool, amount_in: u64, min_out: u64) -> Instruction {
        let wsol_account = get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID);
        let token_account = get_associated_token_address(payer, &self.mint(), &self.token_program());
        let (sol_vault, token_vault) = if self.sol_is_token0() {
            (self.token0_vault, self.token1_vault)
        } else {
            (self.token1_vault, self.token0_vault)
        };
        let sol_side = (wsol_account, sol_vault, TOKEN_PROGRAM_ID, WSOL_MINT);
        let token_side = (token_account, token_vault, self.token_program(), self.mint());
        let (input, output) = if sol_in { (sol_side, token_side) } else { (token_side, sol_side) };

        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&SWAP_BASE_INPUT_DISCRIMINATOR);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        Instruction {
            program_id: RAYDIUM_CPMM_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new_readonly(RAYDIUM_CPMM_AUTHORITY, false),
                AccountMeta::new_readonly(self.amm_config, false),
                AccountMeta::new(self.pool, false),
                AccountMeta::new(input.0, false),
                AccountMeta::new(output.0, false),
                AccountMeta::new(input.1, false),
                AccountMeta::new(output.1, false),
                AccountMeta::new_readonly(input.2, false),
                AccountMeta::new_readonly(output.2, false),
                AccountMeta::new_readonly(input.3, false),
                AccountMeta::new_readonly(output.3, false),
                AccountMeta::new(self.observation, false),
            ],
            data,
        }
    }

    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_out: u64) -> Vec<Instruction> {
        let swap = self.swap_instruction(payer, true, lamports_in, min_out);
        sol_buy_wrapper(payer, lamports_in, &self.mint(), &self.token_program(), swap)
    }

    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_out: u64) -> Vec<Instruction> {
        let swap = self.swap_instruction(payer, false, tokens_in, min_out);
        let close_token_account = self.token_transfer_fee().is_none();
        sol_sell_wrapper(payer, &self.mint(), &self.token_program(), close_token_account, swap)
    }

    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        let keys = [
            self.pool,
            self.amm_config,
            self.token0_vault,
            self.token1_vault,
            self.token0_mint,
            self.token1_mint,
        ];
        let accounts = fetch_accounts(rpc, &keys).await?;
        let pool = &accounts[0].data;
        if pool.len() != POOL_STATE_LEN {
            return Err(SniperError::InvalidAccountData {
                account: self.pool,
                reason: format!("PoolState is {} bytes", pool.len()),
            }
            .into());
        }
        if read_pubkey(pool, 72)? != self.token0_vault || read_pubkey(pool, 104)? != self.token1_vault {
            bail!("CPMM pool {} vaults do not match", self.pool);
        }
        if read_u8(pool, 329)? & STATUS_SWAP_DISABLED != 0 {
            return Err(SniperError::VenueClosed { mint: self.mint(), venue: "raydium_cpmm" }.into());
        }
        let protocol0 = read_u64(pool, 341)?;
        let protocol1 = read_u64(pool, 349)?;
        let fund0 = read_u64(pool, 357)?;
        let fund1 = read_u64(pool, 365)?;
        self.open_time = read_u64(pool, 373)?;
        self.creator_fee_on = read_u8(pool, 389)?;
        let enable_creator_fee = read_bool(pool, 390)?;
        let creator0 = read_u64(pool, 397)?;
        let creator1 = read_u64(pool, 405)?;

        let config = &accounts[1].data;
        self.trade_fee_rate = read_u64(config, 12)?;
        self.creator_fee_rate = if enable_creator_fee { read_u64(config, 108)? } else { 0 };

        let vault0 = read_u64(&accounts[2].data, 64)?;
        let vault1 = read_u64(&accounts[3].data, 64)?;
        self.reserve0 = vault0.saturating_sub(protocol0 + fund0 + creator0);
        self.reserve1 = vault1.saturating_sub(protocol1 + fund1 + creator1);
        self.transfer_fee0 = transfer_fee_config(&accounts[4].data);
        self.transfer_fee1 = transfer_fee_config(&accounts[5].data);
        self.loaded = true;
        Ok(())
    }
}

impl RaydiumCpmmMarket {
    
    pub async fn from_pool(rpc: &RpcClient, pool: Pubkey) -> Result<Self> {
        let data = fetch_accounts(rpc, &[pool]).await?.remove(0).data;
        let mut market = Self::from_initialize(
            pool,
            read_pubkey(&data, 8)?,
            read_pubkey(&data, 296)?,
            read_pubkey(&data, 168)?,
            read_pubkey(&data, 200)?,
            read_pubkey(&data, 72)?,
            read_pubkey(&data, 104)?,
            read_pubkey(&data, 232)?,
            read_pubkey(&data, 264)?,
            read_u64(&data, 373)?,
        )
        .ok_or_else(|| anyhow::anyhow!("CPMM pool {pool} is not paired with SOL"))?;
        market.refresh(rpc).await?;
        Ok(market)
    }
}
