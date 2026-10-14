



use {
    super::{
        raydium_clmm_math::{
            initialized_tick_array_starts, parse_amm_config, parse_bitmap_extension, parse_pool,
            parse_tick_array, quote_exact_input_with_ext, AmmConfigState, BitmapExtension, PoolSnapshot,
            TickArray,
        },
        types::{fetch_accounts, sol_buy_wrapper, sol_sell_wrapper},
    },
    crate::{
        error::SniperError,
        utils::{
            blockhash::current_epoch,
            parse::{read_u64, transfer_fee_config, TransferFee},
            utils::{get_associated_token_address, MEMO_PROGRAM_ID, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT},
        },
    },
    anyhow::{anyhow, bail, Result},
    solana_commitment_config::CommitmentConfig,
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::time::{SystemTime, UNIX_EPOCH},
};

pub const RAYDIUM_CLMM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

const SWAP_V2_DISCRIMINATOR: [u8; 8] = [0x2b, 0x04, 0xed, 0x0b, 0x1a, 0xc9, 0x1e, 0x62];

const STATUS_SWAP_DISABLED: u8 = 1 << 4;

const TICK_ARRAYS_PER_DIRECTION: usize = 4;

#[derive(Debug, Clone)]
struct ClmmState {
    pool: PoolSnapshot,
    config: AmmConfigState,
    extension: Option<BitmapExtension>,
    
    arrays_zero_for_one: Vec<TickArray>,
    
    arrays_one_for_zero: Vec<TickArray>,
    sol_vault_balance: u64,
}

#[derive(Debug, Clone)]
pub struct RaydiumClmmMarket {
    pub pool: Pubkey,
    pub amm_config: Pubkey,
    pub observation: Pubkey,
    pub token_mint_0: Pubkey,
    pub token_mint_1: Pubkey,
    pub token_vault_0: Pubkey,
    pub token_vault_1: Pubkey,
    pub token_program_0: Pubkey,
    pub token_program_1: Pubkey,
    pub transfer_fee_0: Option<TransferFee>,
    pub transfer_fee_1: Option<TransferFee>,
    
    state: Option<Box<ClmmState>>,
}

pub fn tick_array_address(pool: &Pubkey, start_index: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start_index.to_be_bytes()],
        &RAYDIUM_CLMM_PROGRAM_ID,
    )
    .0
}

pub fn bitmap_extension_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool_tick_array_bitmap_extension", pool.as_ref()], &RAYDIUM_CLMM_PROGRAM_ID).0
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl RaydiumClmmMarket {
    
    #[allow(clippy::too_many_arguments)]
    pub fn from_create(
        pool: Pubkey,
        amm_config: Pubkey,
        observation: Pubkey,
        token_mint_0: Pubkey,
        token_mint_1: Pubkey,
        token_vault_0: Pubkey,
        token_vault_1: Pubkey,
        token_program_0: Pubkey,
        token_program_1: Pubkey,
    ) -> Option<Self> {
        if (token_mint_0 == WSOL_MINT) == (token_mint_1 == WSOL_MINT) {
            return None;
        }
        Some(Self {
            pool,
            amm_config,
            observation,
            token_mint_0,
            token_mint_1,
            token_vault_0,
            token_vault_1,
            token_program_0,
            token_program_1,
            transfer_fee_0: None,
            transfer_fee_1: None,
            state: None,
        })
    }

    
    pub async fn from_pool(rpc: &RpcClient, pool: Pubkey) -> Result<Self> {
        let data = fetch_accounts(rpc, &[pool]).await?.remove(0).data;
        let snapshot = parse_pool(&data).map_err(|e| anyhow!("CLMM pool {pool}: {e}"))?;
        let mints = [Pubkey::new_from_array(snapshot.token_mint_0), Pubkey::new_from_array(snapshot.token_mint_1)];
        let accounts = fetch_accounts(rpc, &mints).await?;
        let mut market = Self::from_create(
            pool,
            Pubkey::new_from_array(snapshot.amm_config),
            Pubkey::new_from_array(snapshot.observation_key),
            mints[0],
            mints[1],
            Pubkey::new_from_array(snapshot.token_vault_0),
            Pubkey::new_from_array(snapshot.token_vault_1),
            accounts[0].owner,
            accounts[1].owner,
        )
        .ok_or_else(|| anyhow!("CLMM pool {pool} is not paired with SOL"))?;
        market.refresh(rpc).await?;
        Ok(market)
    }

    fn sol_is_token0(&self) -> bool {
        self.token_mint_0 == WSOL_MINT
    }

    pub fn mint(&self) -> Pubkey {
        if self.sol_is_token0() { self.token_mint_1 } else { self.token_mint_0 }
    }

    pub fn token_program(&self) -> Pubkey {
        if self.sol_is_token0() { self.token_program_1 } else { self.token_program_0 }
    }

    fn token_transfer_fee(&self) -> Option<TransferFee> {
        if self.sol_is_token0() { self.transfer_fee_1 } else { self.transfer_fee_0 }
    }

    pub fn sol_liquidity(&self) -> u64 {
        self.state.as_ref().map_or(0, |s| s.sol_vault_balance)
    }

    pub fn open_time(&self) -> Option<u64> {
        self.state.as_ref().map(|s| s.pool.open_time)
    }

    fn state(&self) -> Result<&ClmmState> {
        self.state.as_deref().ok_or_else(|| anyhow!("CLMM pool {} state not loaded", self.pool))
    }

    
    fn amount_out(&self, amount_in: u64, input_is_token0: bool) -> Result<u64> {
        let state = self.state()?;
        let epoch = current_epoch();
        let (fee_in, fee_out) = if input_is_token0 {
            (self.transfer_fee_0, self.transfer_fee_1)
        } else {
            (self.transfer_fee_1, self.transfer_fee_0)
        };
        let net_in = amount_in - fee_in.map_or(0, |f| f.fee(epoch, amount_in));
        let arrays = if input_is_token0 { &state.arrays_zero_for_one } else { &state.arrays_one_for_zero };
        let quote = quote_exact_input_with_ext(
            &state.pool,
            &state.config,
            state.extension.as_ref(),
            arrays,
            input_is_token0,
            net_in,
            now_unix(),
        )
        .map_err(|e| anyhow!("CLMM quote on {}: {e}", self.pool))?;
        Ok(quote.amount_out - fee_out.map_or(0, |f| f.fee(epoch, quote.amount_out)))
    }

    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        self.amount_out(lamports_in, self.sol_is_token0())
    }

    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        self.amount_out(tokens_in, !self.sol_is_token0())
    }

    fn swap_instruction(&self, payer: &Pubkey, sol_in: bool, amount_in: u64, min_out: u64) -> Result<Instruction> {
        let state = self.state()?;
        let wsol_account = get_associated_token_address(payer, &WSOL_MINT, &TOKEN_PROGRAM_ID);
        let token_account = get_associated_token_address(payer, &self.mint(), &self.token_program());
        let (sol_vault, token_vault) = if self.sol_is_token0() {
            (self.token_vault_0, self.token_vault_1)
        } else {
            (self.token_vault_1, self.token_vault_0)
        };
        let (input_account, output_account, input_vault, output_vault, input_mint, output_mint) = if sol_in {
            (wsol_account, token_account, sol_vault, token_vault, WSOL_MINT, self.mint())
        } else {
            (token_account, wsol_account, token_vault, sol_vault, self.mint(), WSOL_MINT)
        };
        let zero_for_one = input_mint == self.token_mint_0;
        let arrays = if zero_for_one { &state.arrays_zero_for_one } else { &state.arrays_one_for_zero };
        if arrays.is_empty() {
            bail!("CLMM pool {} has no initialized tick arrays for this direction", self.pool);
        }

        let mut accounts = vec![
            AccountMeta::new_readonly(*payer, true),
            AccountMeta::new_readonly(self.amm_config, false),
            AccountMeta::new(self.pool, false),
            AccountMeta::new(input_account, false),
            AccountMeta::new(output_account, false),
            AccountMeta::new(input_vault, false),
            AccountMeta::new(output_vault, false),
            AccountMeta::new(self.observation, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
            AccountMeta::new_readonly(MEMO_PROGRAM_ID, false),
            AccountMeta::new_readonly(input_mint, false),
            AccountMeta::new_readonly(output_mint, false),
            AccountMeta::new(bitmap_extension_address(&self.pool), false),
        ];
        
        
        accounts.extend(
            arrays
                .iter()
                .map(|array| AccountMeta::new(tick_array_address(&self.pool, array.start_tick_index), false)),
        );

        let mut data = Vec::with_capacity(41);
        data.extend_from_slice(&SWAP_V2_DISCRIMINATOR);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        data.extend_from_slice(&0u128.to_le_bytes()); 
        data.push(1); 
        Ok(Instruction { program_id: RAYDIUM_CLMM_PROGRAM_ID, accounts, data })
    }

    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_out: u64) -> Result<Vec<Instruction>> {
        let swap = self.swap_instruction(payer, true, lamports_in, min_out)?;
        Ok(sol_buy_wrapper(payer, lamports_in, &self.mint(), &self.token_program(), swap))
    }

    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_out: u64) -> Result<Vec<Instruction>> {
        let swap = self.swap_instruction(payer, false, tokens_in, min_out)?;
        let close_token_account = self.token_transfer_fee().is_none();
        Ok(sol_sell_wrapper(payer, &self.mint(), &self.token_program(), close_token_account, swap))
    }

    
    
    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        let extension_key = bitmap_extension_address(&self.pool);
        let sol_vault = if self.sol_is_token0() { self.token_vault_0 } else { self.token_vault_1 };
        let keys = [self.pool, self.amm_config, self.token_mint_0, self.token_mint_1, sol_vault, extension_key];
        let response = rpc.get_multiple_accounts_with_commitment(&keys, CommitmentConfig::processed()).await?.value;
        let account = |i: usize| {
            response[i].as_ref().ok_or_else(|| anyhow::Error::from(SniperError::AccountNotFound(keys[i])))
        };

        let pool = parse_pool(&account(0)?.data).map_err(|e| anyhow!("CLMM pool {}: {e}", self.pool))?;
        if pool.status & STATUS_SWAP_DISABLED != 0 {
            return Err(SniperError::VenueClosed { mint: self.mint(), venue: "raydium_clmm" }.into());
        }
        let config =
            parse_amm_config(&account(1)?.data).map_err(|e| anyhow!("CLMM config {}: {e}", self.amm_config))?;
        self.transfer_fee_0 = transfer_fee_config(&account(2)?.data);
        self.transfer_fee_1 = transfer_fee_config(&account(3)?.data);
        let sol_vault_balance = read_u64(&account(4)?.data, 64)?;
        let extension = match &response[5] {
            Some(ext) => {
                Some(parse_bitmap_extension(&ext.data).map_err(|e| anyhow!("CLMM bitmap extension: {e}"))?)
            }
            None => None,
        };

        let starts_zero_for_one =
            initialized_tick_array_starts(&pool, extension.as_ref(), true, TICK_ARRAYS_PER_DIRECTION);
        let starts_one_for_zero =
            initialized_tick_array_starts(&pool, extension.as_ref(), false, TICK_ARRAYS_PER_DIRECTION);
        let mut starts: Vec<i32> = starts_zero_for_one.iter().chain(&starts_one_for_zero).copied().collect();
        starts.sort_unstable();
        starts.dedup();
        let array_keys: Vec<Pubkey> = starts.iter().map(|s| tick_array_address(&self.pool, *s)).collect();
        let fetched = if array_keys.is_empty() { Vec::new() } else { fetch_accounts(rpc, &array_keys).await? };
        let mut loaded = Vec::with_capacity(fetched.len());
        for (start, account) in starts.iter().zip(fetched) {
            loaded.push(parse_tick_array(&account.data).map_err(|e| anyhow!("CLMM tick array {start}: {e}"))?);
        }
        let pick = |order: &[i32]| -> Vec<TickArray> {
            order.iter().filter_map(|s| loaded.iter().find(|a| a.start_tick_index == *s).cloned()).collect()
        };

        self.state = Some(Box::new(ClmmState {
            arrays_zero_for_one: pick(&starts_zero_for_one),
            arrays_one_for_zero: pick(&starts_one_for_zero),
            pool,
            config,
            extension,
            sol_vault_balance,
        }));
        Ok(())
    }
}
