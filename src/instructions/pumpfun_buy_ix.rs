


use {
    super::{
        pumpswap_buy_ix::{PumpSwapMarket, PUMP_FEE_PROGRAM_ID, PUMP_PROGRAM_ID},
        types::fetch_accounts,
    },
    crate::{
        error::SniperError,
        utils::{
            parse::{read_bool, read_pubkey, read_u128, read_u32, read_u64},
            utils::{
                create_associated_token_account_idempotent, get_associated_token_address, SYSTEM_PROGRAM_ID,
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

pub const PUMP_GLOBAL: Pubkey = Pubkey::from_str_const("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
pub const PUMP_EVENT_AUTHORITY: Pubkey = Pubkey::from_str_const("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1");
pub const PUMP_GLOBAL_VOLUME_ACCUMULATOR: Pubkey =
    Pubkey::from_str_const("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y");
pub const PUMP_FEE_CONFIG: Pubkey = Pubkey::from_str_const("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");

pub const CREATE_EVENT_DISCRIMINATOR: [u8; 8] = [0x1b, 0x72, 0xa9, 0x4d, 0xde, 0xeb, 0x63, 0x76];
pub const TRADE_EVENT_DISCRIMINATOR: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];
const BUY_EXACT_SOL_IN_DISCRIMINATOR: [u8; 8] = [0x38, 0xfc, 0x74, 0x08, 0x9e, 0xdf, 0xcd, 0x5f];


const MAYHEM_SUPPLY: u64 = 1_000_000_000_000_000;
const CONFIG_REFRESH: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurveFees {
    pub protocol_bps: u64,
    pub creator_bps: u64,
}

#[derive(Debug, Clone)]
pub struct PumpFunConfig {
    
    pub fee_recipients: Vec<Pubkey>,
    
    pub reserved_fee_recipients: Vec<Pubkey>,
    pub buyback_fee_recipients: Vec<Pubkey>,
    pub creator_fee_configurable: bool,
    
    pub fee_tiers: Vec<(u128, CurveFees)>,
    
    pub initial_reserves: CurveReserves,
}

static CONFIG: LazyLock<RwLock<Option<Arc<PumpFunConfig>>>> = LazyLock::new(|| RwLock::new(None));

pub fn pumpfun_config() -> Result<Arc<PumpFunConfig>> {
    CONFIG
        .read()
        .expect("config lock poisoned")
        .clone()
        .ok_or_else(|| anyhow!("pump.fun config not loaded"))
}


pub async fn init_pumpfun_config(rpc: Arc<RpcClient>) -> Result<()> {
    let config = load_config(&rpc).await?;
    *CONFIG.write().expect("config lock poisoned") = Some(Arc::new(config));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CONFIG_REFRESH).await;
            match load_config(&rpc).await {
                Ok(config) => *CONFIG.write().expect("config lock poisoned") = Some(Arc::new(config)),
                Err(e) => warn!("pump.fun config refresh failed: {e:#}"),
            }
        }
    });
    Ok(())
}

async fn load_config(rpc: &RpcClient) -> Result<PumpFunConfig> {
    let accounts = fetch_accounts(rpc, &[PUMP_GLOBAL, PUMP_FEE_CONFIG]).await?;
    let global = &accounts[0].data;
    let pubkeys = |offset: usize, count: usize| -> Result<Vec<Pubkey>> {
        (0..count).map(|i| read_pubkey(global, offset + 32 * i)).collect()
    };
    let mut fee_recipients = vec![read_pubkey(global, 41)?];
    fee_recipients.extend(pubkeys(162, 7)?);
    let mut reserved_fee_recipients = vec![read_pubkey(global, 483)?];
    reserved_fee_recipients.extend(pubkeys(516, 7)?);

    let fee = &accounts[1].data;
    let tier_count = read_u32(fee, 65)? as usize;
    let fee_tiers = (0..tier_count)
        .map(|i| {
            let at = 69 + 40 * i;
            Ok((
                read_u128(fee, at)?,
                CurveFees { protocol_bps: read_u64(fee, at + 24)?, creator_bps: read_u64(fee, at + 32)? },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    if fee_tiers.is_empty() {
        bail!("pump.fun FeeConfig has no fee tiers");
    }

    Ok(PumpFunConfig {
        fee_recipients,
        reserved_fee_recipients,
        buyback_fee_recipients: pubkeys(741, 8)?,
        creator_fee_configurable: read_bool(global, 1045)?,
        fee_tiers,
        initial_reserves: CurveReserves {
            virtual_token_reserves: read_u64(global, 73)?,
            virtual_sol_reserves: read_u64(global, 81)?,
            real_token_reserves: read_u64(global, 89)?,
            real_sol_reserves: 0,
        },
    })
}

#[cfg(test)]
pub fn set_test_config() {
    seed_test_config();
}

/// A pump.fun config for tests and benchmarks (no RPC): made-up fee recipients, one fee tier.
#[doc(hidden)]
pub fn seed_test_config() {
    let keys = |n: usize| (0..n).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
    *CONFIG.write().expect("config lock poisoned") = Some(Arc::new(PumpFunConfig {
        fee_recipients: keys(8),
        reserved_fee_recipients: keys(8),
        buyback_fee_recipients: keys(8),
        creator_fee_configurable: false,
        fee_tiers: vec![(0, CurveFees { protocol_bps: 95, creator_bps: 30 })],
        initial_reserves: CurveReserves {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_sol_reserves: 30_000_000_000,
            real_token_reserves: 793_100_000_000_000,
            real_sol_reserves: 0,
        },
    }));
}

pub fn bonding_curve_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP_PROGRAM_ID).0
}


#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurveReserves {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
}

#[derive(Debug, Clone)]
pub struct PumpFunMarket {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub token_program: Pubkey,
    
    pub creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    pub creator_fee_bps: u64,
    pub reserves: CurveReserves,
    pub mint_supply: u64,
    pub complete: bool,
}

impl PumpFunMarket {
    pub fn new(
        mint: Pubkey,
        token_program: Pubkey,
        creator: Pubkey,
        is_mayhem_mode: bool,
        reserves: CurveReserves,
    ) -> Self {
        Self::with_curve(mint, bonding_curve_address(&mint), token_program, creator, is_mayhem_mode, reserves)
    }

    /// `new` with the bonding curve already known (no address derivation).
    pub fn with_curve(
        mint: Pubkey,
        bonding_curve: Pubkey,
        token_program: Pubkey,
        creator: Pubkey,
        is_mayhem_mode: bool,
        reserves: CurveReserves,
    ) -> Self {
        Self {
            mint,
            bonding_curve,
            token_program,
            creator,
            is_mayhem_mode,
            is_cashback_coin: false,
            creator_fee_bps: 0,
            reserves,
            mint_supply: MAYHEM_SUPPLY,
            complete: false,
        }
    }

    pub fn mint(&self) -> Pubkey {
        self.mint
    }

    pub fn sol_liquidity(&self) -> u64 {
        self.reserves.real_sol_reserves
    }

    fn fees(&self, config: &PumpFunConfig) -> CurveFees {
        let r = &self.reserves;
        let supply = if self.is_mayhem_mode { MAYHEM_SUPPLY } else { self.mint_supply };
        let market_cap = if r.virtual_token_reserves == 0 {
            0
        } else {
            r.virtual_sol_reserves as u128 * supply as u128 / r.virtual_token_reserves as u128
        };
        let mut fees = config
            .fee_tiers
            .iter()
            .rev()
            .find(|(threshold, _)| *threshold <= market_cap)
            .map_or(config.fee_tiers[0].1, |(_, f)| *f);
        if config.creator_fee_configurable && self.creator_fee_bps != 0 {
            fees.creator_bps = self.creator_fee_bps;
        }
        if self.creator == Pubkey::default() {
            fees.creator_bps = 0;
        }
        fees
    }

    fn ensure_open(&self) -> Result<()> {
        if self.complete {
            return Err(SniperError::VenueClosed { mint: self.mint, venue: "pumpfun" }.into());
        }
        Ok(())
    }

    
    
    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        self.ensure_open()?;
        let fees = self.fees(&*pumpfun_config()?);
        let spendable = lamports_in as u128;
        let fee = |x: u128, bps: u64| (x * bps as u128).div_ceil(10_000);
        let mut net = spendable * 10_000 / (10_000 + (fees.protocol_bps + fees.creator_bps) as u128);
        let total = net + fee(net, fees.protocol_bps) + fee(net, fees.creator_bps);
        if total > spendable {
            net -= total - spendable;
        }
        let input = net.saturating_sub(1);
        let r = &self.reserves;
        let tokens = input * r.virtual_token_reserves as u128 / (r.virtual_sol_reserves as u128 + input);
        if tokens > r.real_token_reserves as u128 {
            bail!("buy of {lamports_in} lamports exceeds the tokens left on the {} curve", self.mint);
        }
        Ok(tokens as u64)
    }

    
    pub fn after_buy(&self, lamports_in: u64) -> Result<Self> {
        let tokens = self.quote_buy(lamports_in)?;
        let fees = self.fees(&*pumpfun_config()?);
        let net = (lamports_in as u128 * 10_000 / (10_000 + (fees.protocol_bps + fees.creator_bps) as u128)) as u64;
        let mut next = self.clone();
        let r = &mut next.reserves;
        r.virtual_token_reserves -= tokens;
        r.real_token_reserves -= tokens;
        r.virtual_sol_reserves += net;
        r.real_sol_reserves += net;
        Ok(next)
    }

    
    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        self.ensure_open()?;
        let fees = self.fees(&*pumpfun_config()?);
        let r = &self.reserves;
        let sol_out = tokens_in as u128 * r.virtual_sol_reserves as u128
            / (r.virtual_token_reserves as u128 + tokens_in as u128);
        let fee = |bps: u64| (sol_out * bps as u128).div_ceil(10_000);
        Ok(sol_out.saturating_sub(fee(fees.protocol_bps) + fee(fees.creator_bps)) as u64)
    }

    pub(super) fn fee_recipient(&self, config: &PumpFunConfig) -> Result<Pubkey> {
        let set = if self.is_mayhem_mode { &config.reserved_fee_recipients } else { &config.fee_recipients };
        set.choose(&mut rand::thread_rng()).copied().ok_or_else(|| anyhow!("no pump.fun fee recipients"))
    }

    pub(super) fn buyback_fee_recipient(config: &PumpFunConfig) -> Result<Pubkey> {
        config
            .buyback_fee_recipients
            .choose(&mut rand::thread_rng())
            .copied()
            .ok_or_else(|| anyhow!("no pump.fun buyback fee recipients"))
    }

    pub(super) fn associated_bonding_curve(&self) -> Pubkey {
        get_associated_token_address(&self.bonding_curve, &self.mint, &self.token_program)
    }

    pub(super) fn creator_vault(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"creator-vault", self.creator.as_ref()], &PUMP_PROGRAM_ID).0
    }

    pub(super) fn bonding_curve_v2(&self) -> Pubkey {
        Pubkey::find_program_address(&[b"bonding-curve-v2", self.mint.as_ref()], &PUMP_PROGRAM_ID).0
    }

    pub(super) fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PUMP_PROGRAM_ID).0
    }

    
    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_out: u64) -> Result<Vec<Instruction>> {
        let config = pumpfun_config()?;
        let accounts = vec![
            AccountMeta::new_readonly(PUMP_GLOBAL, false),
            AccountMeta::new(self.fee_recipient(&config)?, false),
            AccountMeta::new_readonly(self.mint, false),
            AccountMeta::new(self.bonding_curve, false),
            AccountMeta::new(self.associated_bonding_curve(), false),
            AccountMeta::new(get_associated_token_address(payer, &self.mint, &self.token_program), false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(self.token_program, false),
            AccountMeta::new(self.creator_vault(), false),
            AccountMeta::new_readonly(PUMP_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
            AccountMeta::new_readonly(PUMP_GLOBAL_VOLUME_ACCUMULATOR, false),
            AccountMeta::new(Self::user_volume_accumulator(payer), false),
            AccountMeta::new_readonly(PUMP_FEE_CONFIG, false),
            AccountMeta::new_readonly(PUMP_FEE_PROGRAM_ID, false),
            AccountMeta::new_readonly(self.bonding_curve_v2(), false),
            AccountMeta::new(Self::buyback_fee_recipient(&config)?, false),
        ];
        let mut data = Vec::with_capacity(25);
        data.extend_from_slice(&BUY_EXACT_SOL_IN_DISCRIMINATOR);
        data.extend_from_slice(&lamports_in.to_le_bytes());
        data.extend_from_slice(&min_out.to_le_bytes());
        data.push(1); 
        Ok(vec![
            create_associated_token_account_idempotent(payer, payer, &self.mint, &self.token_program),
            Instruction { program_id: PUMP_PROGRAM_ID, accounts, data },
        ])
    }

    
    pub async fn from_mint(rpc: &RpcClient, mint: Pubkey) -> Result<Self> {
        let mut market = Self::new(mint, TOKEN_2022_PROGRAM_ID, Pubkey::default(), false, CurveReserves::default());
        market.refresh(rpc).await?;
        Ok(market)
    }

    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        let accounts = fetch_accounts(rpc, &[self.bonding_curve, self.mint]).await?;
        let curve = &accounts[0].data;
        
        self.reserves = CurveReserves {
            virtual_token_reserves: read_u64(curve, 8)?,
            virtual_sol_reserves: read_u64(curve, 16)?,
            real_token_reserves: read_u64(curve, 24)?,
            real_sol_reserves: read_u64(curve, 32)?,
        };
        self.complete = read_bool(curve, 48)?;
        self.creator = read_pubkey(curve, 49)?;
        self.is_mayhem_mode = read_bool(curve, 81).unwrap_or(false);
        self.is_cashback_coin = read_bool(curve, 82).unwrap_or(false);
        let quote_mint = read_pubkey(curve, 83).unwrap_or_default();
        if quote_mint != Pubkey::default() && quote_mint != WSOL_MINT {
            bail!("{} is quoted in {quote_mint}, not SOL", self.mint);
        }
        self.creator_fee_bps = read_u64(curve, 115).unwrap_or(0);
        self.token_program = accounts[1].owner;
        if self.token_program != TOKEN_PROGRAM_ID && self.token_program != TOKEN_2022_PROGRAM_ID {
            bail!("unexpected token program {} for {}", self.token_program, self.mint);
        }
        self.mint_supply = read_u64(&accounts[1].data, 36)?;
        if self.complete {
            return Err(SniperError::VenueClosed { mint: self.mint, venue: "pumpfun" }.into());
        }
        Ok(())
    }

    
    pub async fn migrated(&self, rpc: &RpcClient) -> Result<Option<PumpSwapMarket>> {
        PumpSwapMarket::from_mint(rpc, &self.mint).await
    }
}
