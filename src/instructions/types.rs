use {
    super::{
        pumpfun_buy_ix::PumpFunMarket, pumpswap_buy_ix::PumpSwapMarket, raydium_amm_swap_base_in::RaydiumAmmMarket,
        raydium_clmm_swap::RaydiumClmmMarket,
        raydium_cpmm_swap_base_in::RaydiumCpmmMarket, raydium_launchpad_buy_ix::RaydiumLaunchpadMarket,
    },
    crate::{
        config::Dex,
        error::SniperError,
        utils::utils::{
            create_associated_token_account_idempotent, unwrap_sol_instruction, wrap_sol_instructions,
            close_token_account, get_associated_token_address, TOKEN_PROGRAM_ID,
        },
    },
    anyhow::Result,
    solana_commitment_config::CommitmentConfig,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
};



#[derive(Debug, Clone)]
pub enum Market {
    PumpFun(PumpFunMarket),
    PumpSwap(PumpSwapMarket),
    RaydiumAmm(RaydiumAmmMarket),
    RaydiumCpmm(RaydiumCpmmMarket),
    RaydiumClmm(RaydiumClmmMarket),
    RaydiumLaunchpad(RaydiumLaunchpadMarket),
}

macro_rules! each {
    ($self:expr, $m:ident => $body:expr) => {
        match $self {
            Market::PumpFun($m) => $body,
            Market::PumpSwap($m) => $body,
            Market::RaydiumAmm($m) => $body,
            Market::RaydiumCpmm($m) => $body,
            Market::RaydiumClmm($m) => $body,
            Market::RaydiumLaunchpad($m) => $body,
        }
    };
}

impl Market {
    pub fn dex(&self) -> Dex {
        match self {
            Market::PumpFun(_) => Dex::PumpFun,
            Market::PumpSwap(_) => Dex::PumpSwap,
            Market::RaydiumAmm(_) => Dex::RaydiumAmm,
            Market::RaydiumCpmm(_) => Dex::RaydiumCpmm,
            Market::RaydiumClmm(_) => Dex::RaydiumClmm,
            Market::RaydiumLaunchpad(_) => Dex::RaydiumLaunchpad,
        }
    }

    pub fn mint(&self) -> Pubkey {
        each!(self, m => m.mint())
    }

    pub fn token_program(&self) -> Pubkey {
        match self {
            Market::PumpFun(m) => m.token_program,
            Market::PumpSwap(m) => m.base_token_program,
            Market::RaydiumAmm(_) => TOKEN_PROGRAM_ID,
            Market::RaydiumCpmm(m) => m.token_program(),
            Market::RaydiumClmm(m) => m.token_program(),
            Market::RaydiumLaunchpad(m) => m.base_token_program,
        }
    }

    
    pub fn sol_liquidity(&self) -> u64 {
        each!(self, m => m.sol_liquidity())
    }

    
    pub fn open_time(&self) -> Option<u64> {
        match self {
            Market::RaydiumAmm(m) => Some(m.open_time),
            Market::RaydiumCpmm(m) => Some(m.open_time),
            Market::RaydiumClmm(m) => m.open_time(),
            Market::PumpFun(_) | Market::PumpSwap(_) | Market::RaydiumLaunchpad(_) => None,
        }
        .filter(|t| *t > 0)
    }

    
    pub fn quote_buy(&self, lamports_in: u64) -> Result<u64> {
        each!(self, m => m.quote_buy(lamports_in))
    }

    
    
    pub fn quote_buy_after(&self, ahead_lamports: u64, lamports_in: u64) -> Option<u64> {
        match self {
            Market::PumpFun(m) => m.after_buy(ahead_lamports).ok()?.quote_buy(lamports_in).ok(),
            _ => None,
        }
    }

    
    pub fn quote_sell(&self, tokens_in: u64) -> Result<u64> {
        each!(self, m => m.quote_sell(tokens_in))
    }

    pub fn buy_instructions(&self, payer: &Pubkey, lamports_in: u64, min_tokens_out: u64) -> Result<Vec<Instruction>> {
        match self {
            Market::PumpFun(m) => m.buy_instructions(payer, lamports_in, min_tokens_out),
            Market::PumpSwap(m) => m.buy_instructions(payer, lamports_in, min_tokens_out),
            Market::RaydiumAmm(m) => Ok(m.buy_instructions(payer, lamports_in, min_tokens_out)),
            Market::RaydiumCpmm(m) => Ok(m.buy_instructions(payer, lamports_in, min_tokens_out)),
            Market::RaydiumClmm(m) => m.buy_instructions(payer, lamports_in, min_tokens_out),
            Market::RaydiumLaunchpad(m) => Ok(m.buy_instructions(payer, lamports_in, min_tokens_out)),
        }
    }

    pub fn sell_instructions(&self, payer: &Pubkey, tokens_in: u64, min_lamports_out: u64) -> Result<Vec<Instruction>> {
        match self {
            Market::PumpFun(m) => m.sell_instructions(payer, tokens_in, min_lamports_out),
            Market::PumpSwap(m) => m.sell_instructions(payer, tokens_in, min_lamports_out),
            Market::RaydiumAmm(m) => Ok(m.sell_instructions(payer, tokens_in, min_lamports_out)),
            Market::RaydiumCpmm(m) => Ok(m.sell_instructions(payer, tokens_in, min_lamports_out)),
            Market::RaydiumClmm(m) => m.sell_instructions(payer, tokens_in, min_lamports_out),
            Market::RaydiumLaunchpad(m) => Ok(m.sell_instructions(payer, tokens_in, min_lamports_out)),
        }
    }

    
    
    pub async fn prepare(&mut self, rpc: &RpcClient) -> Result<()> {
        match self {
            Market::RaydiumLaunchpad(m) if !m.fees_loaded => m.load_fees(rpc).await,
            _ => Ok(()),
        }
    }

    
    
    pub async fn refresh(&mut self, rpc: &RpcClient) -> Result<()> {
        each!(self, m => m.refresh(rpc).await)
    }

    
    pub async fn migrated(&self, rpc: &RpcClient) -> Result<Option<Market>> {
        match self {
            Market::PumpFun(m) => Ok(m.migrated(rpc).await?.map(Market::PumpSwap)),
            _ => Ok(None),
        }
    }

    
    
    pub fn authorities_revoked_by_program(&self) -> bool {
        match self {
            Market::PumpFun(_) => true,
            Market::PumpSwap(m) => m.is_canonical(),
            _ => false,
        }
    }
}


pub async fn fetch_accounts(rpc: &RpcClient, keys: &[Pubkey]) -> Result<Vec<solana_account::Account>> {
    let response = rpc
        .get_multiple_accounts_with_commitment(keys, CommitmentConfig::processed())
        .await?;
    response
        .value
        .into_iter()
        .zip(keys)
        .map(|(account, key)| account.ok_or_else(|| SniperError::AccountNotFound(*key).into()))
        .collect()
}



pub fn sol_buy_wrapper(
    payer: &Pubkey,
    lamports_in: u64,
    mint: &Pubkey,
    token_program: &Pubkey,
    swap: Instruction,
) -> Vec<Instruction> {
    let mut instructions = wrap_sol_instructions(payer, lamports_in);
    instructions.push(create_associated_token_account_idempotent(payer, payer, mint, token_program));
    instructions.push(swap);
    instructions.push(unwrap_sol_instruction(payer));
    instructions
}




pub fn sol_sell_wrapper(
    payer: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    close_token_account_after: bool,
    swap: Instruction,
) -> Vec<Instruction> {
    let mut instructions = vec![
        create_associated_token_account_idempotent(payer, payer, &crate::utils::utils::WSOL_MINT, &TOKEN_PROGRAM_ID),
        swap,
        unwrap_sol_instruction(payer),
    ];
    if close_token_account_after {
        let token_account = get_associated_token_address(payer, mint, token_program);
        instructions.push(close_token_account(&token_account, payer, payer, token_program));
    }
    instructions
}
