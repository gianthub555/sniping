


use {
    super::types::{fetch_accounts, sol_buy_wrapper, sol_sell_wrapper},
    crate::{
        error::SniperError,
        utils::{
            parse::{read_pubkey, read_u64},
            swap_quote::{constant_product_out, mul_div_ceil},
            utils::{get_associated_token_address, TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    anyhow::{bail, Result},
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
};

pub const RAYDIUM_AMM_V4_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const RAYDIUM_AMM_V4_AUTHORITY: Pubkey =
    Pubkey::from_str_const("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");

const AMM_INFO_LEN: usize = 752;
const SWAP_BASE_IN_V2_TAG: u8 = 16;

const SWAPPABLE_STATUSES: [u64; 3] = [1, 6, 7];

#[derive(Debug, Clone)]
pub struct RaydiumAmmMarket {
    pub pool: Pubkey,
    pub coin_mint: Pubkey,
    pub pc_mint: Pubkey,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    
    pub coin_reserve: u64,
    pub pc_reserve: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
    pub status: u64,
    pub open_time: u64,
}

impl RaydiumAmmMarket {
    
    
    #[allow(clippy::too_many_arguments)]
    pub fn from_initialize(
        pool: Pubkey,
        coin_mint: Pubkey,
        pc_mint: Pubkey,
        coin_vault: Pubkey,
        pc_vault: Pubkey,
        init_coin_amount: u64,
        init_pc_amount: u64,
        open_time: u64,
    ) -> Option<Self> {
        if (coin_mint == WSOL_MINT) == (pc_mint == WSOL_MINT) {
            return None;
        }
        Some(Self {
            pool,
            coin_mint,
            pc_mint,
            coin_vault,
            pc_vault,
            coin_reserve: init_coin_amount,
            pc_reserve: init_pc_amount,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            status: 6,
            open_time,
        })
    }

    fn sol_is_pc(&self) -> bool {
        self.pc_mint == WSOL_MINT
    }

    pub fn mint(&self) -> Pubkey {
        if self.sol_is_pc() { self.coin_mint } else { self.pc_mint }
    }

    
    fn reserves(&self) -> (u64, u64) {
        if self.sol_is_pc() {
            (self.pc_reserve, self.coin_reserve)
        } else {
            (self.coin_reserve, self.pc_reserve)
        }
    }

    pub fn sol_liquidity(&self) -> u64 {
        self.reserves().0
    }

    fn amount_out(&self, amount_in: u64, sol_in: bool) -> u64 {
        let fee = mul_div_ceil(amount_in, self.swap_fee_numerator, self.swap_fee_denominator);
        let net = amount_in.saturating_sub(fee);
        let (sol, token) = self.reserves();
        if sol_in {
            constant_product_out(net, sol, token)
        } else {
            constant_product_out(net, token, sol)
        }
    }

    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        Ok(self.amount_out(lamports_in, true))
    }

    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        Ok(self.amount_out(tokens_in, false))
    }

    fn swap_instruction(
        &self,
        owner: &Pubkey,
        source: Pubkey,
        destination: Pubkey,
        amount_in: u64,
        min_out: u64,
    ) -> Instruction {
        let mut data = Vec::with_capacity(17);
        data.push(SWAP_BASE_IN_V2_TAG);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        Instruction {
            program_id: RAYDIUM_AMM_V4_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new(self.pool, false),
                AccountMeta::new_readonly(RAYDIUM_AMM_V4_AUTHORITY, false),
                AccountMeta::new(self.coin_vault, false),
                AccountMeta::new(self.pc_vault, false),
                AccountMeta::new(source, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(*owner, true),
            ],
            data,
        }
    }

    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_out: u64) -> Vec<Instruction> {
        let wsol = get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID);
        let token = get_associated_token_address(payer, &self.mint(), &TOKEN_PROGRAM_ID);
        let swap = self.swap_instruction(payer, wsol, token, lamports_in, min_out);
        sol_buy_wrapper(payer, lamports_in, &self.mint(), &TOKEN_PROGRAM_ID, swap)
    }

    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_out: u64) -> Vec<Instruction> {
        let wsol = get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID);
        let token = get_associated_token_address(payer, &self.mint(), &TOKEN_PROGRAM_ID);
        let swap = self.swap_instruction(payer, token, wsol, tokens_in, min_out);
        sol_sell_wrapper(payer, &self.mint(), &TOKEN_PROGRAM_ID, true, swap)
    }

    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        let accounts = fetch_accounts(rpc, &[self.pool, self.coin_vault, self.pc_vault]).await?;
        let info = &accounts[0].data;
        if info.len() != AMM_INFO_LEN {
            return Err(SniperError::InvalidAccountData {
                account: self.pool,
                reason: format!("AmmInfo is {} bytes", info.len()),
            }
            .into());
        }
        if read_pubkey(info, 336)? != self.coin_vault || read_pubkey(info, 368)? != self.pc_vault {
            bail!("AmmInfo vaults for {} do not match", self.pool);
        }
        self.status = read_u64(info, 0)?;
        if !SWAPPABLE_STATUSES.contains(&self.status) {
            return Err(SniperError::VenueClosed { mint: self.mint(), venue: "raydium_amm" }.into());
        }
        self.swap_fee_numerator = read_u64(info, 176)?;
        self.swap_fee_denominator = read_u64(info, 184)?;
        let need_take_pnl_coin = read_u64(info, 192)?;
        let need_take_pnl_pc = read_u64(info, 200)?;
        self.open_time = read_u64(info, 224)?;
        self.coin_reserve = read_u64(&accounts[1].data, 64)?.saturating_sub(need_take_pnl_coin);
        self.pc_reserve = read_u64(&accounts[2].data, 64)?.saturating_sub(need_take_pnl_pc);
        Ok(())
    }
}

impl RaydiumAmmMarket {
    
    pub async fn from_pool(rpc: &RpcClient, pool: Pubkey) -> Result<Self> {
        let info = fetch_accounts(rpc, &[pool]).await?.remove(0).data;
        let mut market = Self::from_initialize(
            pool,
            read_pubkey(&info, 400)?,
            read_pubkey(&info, 432)?,
            read_pubkey(&info, 336)?,
            read_pubkey(&info, 368)?,
            0,
            0,
            0,
        )
        .ok_or_else(|| anyhow::anyhow!("AMM pool {pool} is not paired with SOL"))?;
        market.refresh(rpc).await?;
        Ok(market)
    }
}
