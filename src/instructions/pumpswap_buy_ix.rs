


use {
    super::types::{fetch_accounts, sol_buy_wrapper},
    crate::{
        error::SniperError,
        utils::{
            parse::{read_bool, read_pubkey, read_u128, read_u32, read_u64},
            utils::{
                get_associated_token_address, ASSOCIATED_TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID,
                TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT,
            },
        },
    },
    anyhow::{anyhow, bail, Result},
    log::warn,
    rand::seq::SliceRandom,
    solana_instruction::{AccountMeta, Instruction},
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{
        sync::{Arc, LazyLock, RwLock},
        time::Duration,
    },
};

pub const PUMPSWAP_PROGRAM_ID: Pubkey = Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const PUMP_FEE_PROGRAM_ID: Pubkey = Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
pub const PUMPSWAP_GLOBAL_CONFIG: Pubkey = Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
pub const PUMPSWAP_EVENT_AUTHORITY: Pubkey = Pubkey::from_str_const("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");
pub const PUMPSWAP_GLOBAL_VOLUME_ACCUMULATOR: Pubkey =
    Pubkey::from_str_const("C2aFPdENg4A2HQsmrd5rTw5TaYBX5Ku887cWjbFKtZpw");
pub const PUMPSWAP_FEE_CONFIG: Pubkey = Pubkey::from_str_const("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");

pub const PUMP_PROGRAM_ID: Pubkey = Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

pub const CREATE_POOL_DISCRIMINATOR: [u8; 8] = [0xe9, 0x92, 0xd1, 0x8e, 0xcf, 0x68, 0x40, 0xbc];
pub const CREATE_POOL_EVENT_DISCRIMINATOR: [u8; 8] = [0xb1, 0x31, 0x0c, 0xd2, 0xa0, 0x76, 0xa7, 0x74];
pub const INIT_BOOST_EVENT_DISCRIMINATOR: [u8; 8] = [0xae, 0x7c, 0x4a, 0xf9, 0x04, 0x51, 0xf6, 0x11];
const BUY_EXACT_QUOTE_IN_DISCRIMINATOR: [u8; 8] = [0xc6, 0x2e, 0x15, 0x52, 0xb4, 0xd9, 0xe8, 0x70];


const MAYHEM_SUPPLY: u64 = 1_000_000_000_000_000;
const CONFIG_REFRESH: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fees {
    pub lp_bps: u64,
    pub protocol_bps: u64,
    pub creator_bps: u64,
}

#[derive(Debug, Clone)]
pub struct PumpSwapConfig {
    pub protocol_fee_recipients: Vec<Pubkey>,
    
    pub reserved_fee_recipients: Vec<Pubkey>,
    pub buyback_fee_recipients: Vec<Pubkey>,
    pub creator_fee_configurable: bool,
    pub flat_fees: Fees,
    
    pub fee_tiers: Vec<(u128, Fees)>,
}

static CONFIG: LazyLock<RwLock<Option<Arc<PumpSwapConfig>>>> = LazyLock::new(|| RwLock::new(None));

pub fn pumpswap_config() -> Result<Arc<PumpSwapConfig>> {
    CONFIG
        .read()
        .expect("config lock poisoned")
        .clone()
        .ok_or_else(|| anyhow!("PumpSwap config not loaded"))
}


pub async fn init_pumpswap_config(rpc: Arc<RpcClient>) -> Result<()> {
    let config = load_config(&rpc).await?;
    *CONFIG.write().expect("config lock poisoned") = Some(Arc::new(config));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CONFIG_REFRESH).await;
            match load_config(&rpc).await {
                Ok(config) => *CONFIG.write().expect("config lock poisoned") = Some(Arc::new(config)),
                Err(e) => warn!("PumpSwap config refresh failed: {e:#}"),
            }
        }
    });
    Ok(())
}

async fn load_config(rpc: &RpcClient) -> Result<PumpSwapConfig> {
    let accounts = fetch_accounts(rpc, &[PUMPSWAP_GLOBAL_CONFIG, PUMPSWAP_FEE_CONFIG]).await?;
    let global = &accounts[0].data;
    let pubkeys = |offset: usize, count: usize| -> Result<Vec<Pubkey>> {
        (0..count).map(|i| read_pubkey(global, offset + 32 * i)).collect()
    };
    let mut reserved = vec![read_pubkey(global, 385)?];
    reserved.extend(pubkeys(418, 7)?);

    let fee = &accounts[1].data;
    let read_fees = |offset: usize| -> Result<Fees> {
        Ok(Fees {
            lp_bps: read_u64(fee, offset)?,
            protocol_bps: read_u64(fee, offset + 8)?,
            creator_bps: read_u64(fee, offset + 16)?,
        })
    };
    let tier_count = read_u32(fee, 65)? as usize;
    let fee_tiers = (0..tier_count)
        .map(|i| {
            let at = 69 + 40 * i;
            Ok((read_u128(fee, at)?, read_fees(at + 16)?))
        })
        .collect::<Result<Vec<_>>>()?;
    if fee_tiers.is_empty() {
        bail!("PumpSwap FeeConfig has no fee tiers");
    }

    Ok(PumpSwapConfig {
        protocol_fee_recipients: pubkeys(57, 8)?,
        reserved_fee_recipients: reserved,
        buyback_fee_recipients: pubkeys(643, 8)?,
        creator_fee_configurable: read_bool(global, 940)?,
        flat_fees: read_fees(41)?,
        fee_tiers,
    })
}

pub fn pool_authority(base_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-authority", base_mint.as_ref()], &PUMP_PROGRAM_ID).0
}


pub fn canonical_pool(base_mint: &Pubkey) -> Pubkey {
    let authority = pool_authority(base_mint);
    Pubkey::find_program_address(
        &[b"pool", &0u16.to_le_bytes(), authority.as_ref(), base_mint.as_ref(), WSOL_MINT.as_ref()],
        &PUMPSWAP_PROGRAM_ID,
    )
    .0
}

#[derive(Debug, Clone)]
pub struct PumpSwapMarket {
    pub pool: Pubkey,
    
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub base_token_program: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    
    pub creator_fee_bps: u64,
    
    pub base_reserve: u64,
    
    pub quote_reserve: u64,
    
    pub virtual_quote_reserves: i128,
    pub base_supply: u64,
}

impl PumpSwapMarket {
    pub fn mint(&self) -> Pubkey {
        self.base_mint
    }

    
    fn effective_quote(&self) -> u128 {
        (self.quote_reserve as i128 + self.virtual_quote_reserves).max(0) as u128
    }

    pub fn sol_liquidity(&self) -> u64 {
        self.effective_quote().min(u64::MAX as u128) as u64
    }

    pub fn is_canonical(&self) -> bool {
        self.creator == pool_authority(&self.base_mint)
    }

    fn fees(&self, config: &PumpSwapConfig) -> Fees {
        let mut fees = if !self.is_canonical() {
            config.flat_fees
        } else {
            let supply = if self.is_mayhem_mode { MAYHEM_SUPPLY } else { self.base_supply };
            let market_cap = if self.base_reserve == 0 {
                0
            } else {
                self.effective_quote() * supply as u128 / self.base_reserve as u128
            };
            config
                .fee_tiers
                .iter()
                .rev()
                .find(|(threshold, _)| *threshold <= market_cap)
                .map_or(config.fee_tiers[0].1, |(_, f)| *f)
        };
        if config.creator_fee_configurable && self.creator_fee_bps != 0 {
            fees.creator_bps = self.creator_fee_bps;
        }
        if self.coin_creator == Pubkey::default() {
            fees.creator_bps = 0;
        }
        fees
    }

    
    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        let fees = self.fees(&*pumpswap_config()?);
        let total_bps = fees.lp_bps + fees.protocol_bps + fees.creator_bps;
        let spendable = lamports_in as u128;
        let fee = |x: u128, bps: u64| (x * bps as u128).div_ceil(10_000);
        let all_fees = |x: u128| fee(x, fees.lp_bps) + fee(x, fees.protocol_bps) + fee(x, fees.creator_bps);

        let mut net = spendable * 10_000 / (10_000 + total_bps as u128);
        let charged = net + all_fees(net);
        if charged > spendable {
            net -= charged - spendable;
        }
        let input = net.saturating_sub(1);
        let b = self.base_reserve as u128;
        let q = self.effective_quote();
        Ok((b * input / (q + input)) as u64)
    }

    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        let fees = self.fees(&*pumpswap_config()?);
        let b = self.base_reserve as u128;
        let q = self.effective_quote();
        let raw = q * tokens_in as u128 / (b + tokens_in as u128);
        let fee = |bps: u64| (raw * bps as u128).div_ceil(10_000);
        let lp_fee = fee(fees.lp_bps);
        if (self.quote_reserve as u128) < raw.saturating_sub(lp_fee) {
            bail!("PumpSwap pool {} lacks real quote reserves for this sell", self.pool);
        }
        let user = raw.saturating_sub(lp_fee + fee(fees.protocol_bps) + fee(fees.creator_bps));
        Ok(user as u64)
    }

    fn coin_creator_vault_authority(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"creator_vault", self.coin_creator.as_ref()], &PUMPSWAP_PROGRAM_ID).0
    }

    pub fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PUMPSWAP_PROGRAM_ID).0
    }

    fn pool_v2(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"pool-v2", self.base_mint.as_ref()], &PUMPSWAP_PROGRAM_ID).0
    }

    
    pub(super) fn common_accounts(&self, user: &Pubkey, config: &PumpSwapConfig) -> Result<Vec<AccountMeta>> {
        let mut rng = rand::thread_rng();
        let recipients = if self.is_mayhem_mode {
            &config.reserved_fee_recipients
        } else {
            &config.protocol_fee_recipients
        };
        let protocol_fee_recipient =
            *recipients.choose(&mut rng).ok_or_else(|| anyhow!("no PumpSwap protocol fee recipients"))?;
        let creator_vault_authority = self.coin_creator_vault_authority();
        Ok(vec![
            AccountMeta::new(self.pool, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(PUMPSWAP_GLOBAL_CONFIG, false),
            AccountMeta::new_readonly(self.base_mint, false),
            AccountMeta::new_readonly(WSOL_MINT, false),
            AccountMeta::new(get_associated_token_address(user, &self.base_mint, &self.base_token_program), false),
            AccountMeta::new(get_associated_token_address(user, &WSOL_MINT, &TOKEN_PROGRAM_ID), false),
            AccountMeta::new(self.pool_base_token_account, false),
            AccountMeta::new(self.pool_quote_token_account, false),
            AccountMeta::new_readonly(protocol_fee_recipient, false),
            AccountMeta::new(get_associated_token_address(&protocol_fee_recipient, &WSOL_MINT, &TOKEN_PROGRAM_ID), false),
            AccountMeta::new_readonly(self.base_token_program, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(PUMPSWAP_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PUMPSWAP_PROGRAM_ID, false),
            AccountMeta::new(get_associated_token_address(&creator_vault_authority, &WSOL_MINT, &TOKEN_PROGRAM_ID), false),
            AccountMeta::new_readonly(creator_vault_authority, false),
        ])
    }

    
    pub(super) fn pool_v2_and_buyback_accounts(&self, config: &PumpSwapConfig) -> Result<Vec<AccountMeta>> {
        let mut accounts = Vec::with_capacity(3);
        if self.coin_creator != Pubkey::default() {
            accounts.push(AccountMeta::new_readonly(self.pool_v2(), false));
        }
        let buyback = *config
            .buyback_fee_recipients
            .choose(&mut rand::thread_rng())
            .ok_or_else(|| anyhow!("no PumpSwap buyback fee recipients"))?;
        accounts.push(AccountMeta::new_readonly(buyback, false));
        accounts.push(AccountMeta::new(get_associated_token_address(&buyback, &WSOL_MINT, &TOKEN_PROGRAM_ID), false));
        Ok(accounts)
    }

    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_out: u64) -> Result<Vec<Instruction>> {
        let config = pumpswap_config()?;
        let user_volume_accumulator = Self::user_volume_accumulator(payer);
        let mut accounts = self.common_accounts(payer, &config)?;
        accounts.extend([
            AccountMeta::new_readonly(PUMPSWAP_GLOBAL_VOLUME_ACCUMULATOR, false),
            AccountMeta::new(user_volume_accumulator, false),
            AccountMeta::new_readonly(PUMPSWAP_FEE_CONFIG, false),
            AccountMeta::new_readonly(PUMP_FEE_PROGRAM_ID, false),
        ]);
        if self.is_cashback_coin {
            accounts.push(AccountMeta::new(
                get_associated_token_address(&user_volume_accumulator, &WSOL_MINT, &TOKEN_PROGRAM_ID),
                false,
            ));
        }
        accounts.extend(self.pool_v2_and_buyback_accounts(&config)?);

        let mut data = Vec::with_capacity(25);
        data.extend_from_slice(&BUY_EXACT_QUOTE_IN_DISCRIMINATOR);
        data.extend_from_slice(&lamports_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        data.push(1); 
        let swap = Instruction { program_id: PUMPSWAP_PROGRAM_ID, accounts, data };
        Ok(sol_buy_wrapper(payer, lamports_in, &self.base_mint, &self.base_token_program, swap))
    }

    
    pub async fn from_mint(rpc: &RpcClient, base_mint: &Pubkey) -> Result<Option<Self>> {
        let pool = canonical_pool(base_mint);
        match rpc.get_account_with_commitment(&pool, solana_commitment_config::CommitmentConfig::processed()).await?.value {
            None => Ok(None),
            Some(account) => {
                let mut market = Self::from_pool_data(pool, &account.data)?;
                market.refresh(rpc).await?;
                Ok(Some(market))
            }
        }
    }

    
    pub async fn from_pool(rpc: &RpcClient, pool: Pubkey) -> Result<Self> {
        let data = fetch_accounts(rpc, &[pool]).await?.remove(0).data;
        let mut market = Self::from_pool_data(pool, &data)?;
        market.refresh(rpc).await?;
        Ok(market)
    }

    fn from_pool_data(pool: Pubkey, data: &[u8]) -> Result<Self> {
        let quote_mint = read_pubkey(data, 75)?;
        if quote_mint != WSOL_MINT {
            bail!("PumpSwap pool {pool} is not quoted in SOL");
        }
        let mut market = Self {
            pool,
            creator: read_pubkey(data, 11)?,
            base_mint: read_pubkey(data, 43)?,
            base_token_program: TOKEN_PROGRAM_ID,
            pool_base_token_account: read_pubkey(data, 139)?,
            pool_quote_token_account: read_pubkey(data, 171)?,
            coin_creator: Pubkey::default(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            creator_fee_bps: 0,
            base_reserve: 0,
            quote_reserve: 0,
            virtual_quote_reserves: 0,
            base_supply: 0,
        };
        market.apply_pool_data(data)?;
        Ok(market)
    }

    
    fn apply_pool_data(&mut self, data: &[u8]) -> Result<()> {
        let optional_u64 = |offset: usize| read_u64(data, offset).unwrap_or(0);
        self.coin_creator = read_pubkey(data, 211).unwrap_or_default();
        self.is_mayhem_mode = read_bool(data, 243).unwrap_or(false);
        self.is_cashback_coin = read_bool(data, 244).unwrap_or(false);
        self.virtual_quote_reserves = read_u128(data, 245).map(|v| v as i128).unwrap_or(0);
        self.creator_fee_bps = optional_u64(261);
        Ok(())
    }

    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        let keys = [self.pool, self.pool_base_token_account, self.pool_quote_token_account, self.base_mint];
        let accounts = fetch_accounts(rpc, &keys).await?;
        let pool = &accounts[0].data;
        if read_pubkey(pool, 43)? != self.base_mint {
            return Err(SniperError::InvalidAccountData { account: self.pool, reason: "base mint mismatch".into() }.into());
        }
        self.apply_pool_data(pool)?;
        self.base_reserve = read_u64(&accounts[1].data, 64)?;
        self.quote_reserve = read_u64(&accounts[2].data, 64)?;
        self.base_token_program = accounts[3].owner;
        self.base_supply = read_u64(&accounts[3].data, 36)?;
        if self.base_token_program != TOKEN_PROGRAM_ID && self.base_token_program != TOKEN_2022_PROGRAM_ID {
            bail!("unexpected token program {} for {}", self.base_token_program, self.base_mint);
        }
        Ok(())
    }
}


pub const PUMP_TOKEN_SUPPLY: u64 = 1_000_000_000_000_000;

impl PumpSwapMarket {
    
    
    #[allow(clippy::too_many_arguments)]
    pub fn from_create_pool(
        pool: Pubkey,
        creator: Pubkey,
        base_mint: Pubkey,
        base_token_program: Pubkey,
        pool_base_token_account: Pubkey,
        pool_quote_token_account: Pubkey,
        coin_creator: Pubkey,
        is_mayhem_mode: bool,
        is_cashback_coin: bool,
        pool_base_amount: u64,
        pool_quote_amount: u64,
        boost: i128,
    ) -> Self {
        Self {
            pool,
            creator,
            base_mint,
            base_token_program,
            pool_base_token_account,
            pool_quote_token_account,
            coin_creator,
            is_mayhem_mode,
            is_cashback_coin,
            creator_fee_bps: 0,
            base_reserve: pool_base_amount,
            quote_reserve: (pool_quote_amount as i128 - boost).max(0) as u64,
            virtual_quote_reserves: boost,
            base_supply: PUMP_TOKEN_SUPPLY,
        }
    }
}
