









use {
    anyhow::{anyhow, Result},
    async_trait::async_trait,
    carbon_core::{
        datasource::{Datasource, DatasourceId, TransactionUpdate, Update, UpdateType},
        error::{CarbonResult, Error as CarbonError},
        pipeline::{Pipeline, ShutdownStrategy},
        transformers::transaction_metadata_from_original_meta,
    },
    carbon_pump_swap_decoder::PumpSwapDecoder,
    carbon_pumpfun_decoder::PumpfunDecoder,
    carbon_raydium_amm_v4_decoder::RaydiumAmmV4Decoder,
    carbon_raydium_clmm_decoder::RaydiumClmmDecoder,
    carbon_raydium_cpmm_decoder::RaydiumCpmmDecoder,
    carbon_raydium_launchpad_decoder::RaydiumLaunchpadDecoder,
    carbon_rpc_transaction_crawler_datasource::{ConnectionConfig, Filters, RetryConfig, RpcTransactionCrawler},
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client_api::config::RpcTransactionConfig,
    solana_transaction_status_client_types::UiTransactionEncoding,
    tokio::sync::mpsc::Sender,
    tokio_util::sync::CancellationToken,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    sniper_bot::{
        config::{rpc_endpoint, Dex, RPC_CLIENT, TRADE_SETTING},
        instructions::{pumpfun_buy_ix::init_pumpfun_config, pumpswap_buy_ix::init_pumpswap_config},
        processors::{
            PumpSwapProcess, PumpfunProcess, RayClmmProcess, RayCpmmProcess, RayLaunchPadProcess, RaydiumV4Process,
        },
        utils::blockhash::{recent_blockhash_handler, wait_for_blockhash},
    },
    std::{str::FromStr, time::Duration},
};



fn creation_marker(dex: Dex) -> Result<Pubkey> {
    let key = match dex {
        
        Dex::PumpFun => "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM",
        
        Dex::PumpSwap => "39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg",
        
        Dex::RaydiumAmm => "7YttLkHDoNj9wyDur5pM1ejNaAvT9X4eqaYcHQqtj2G5",
        
        Dex::RaydiumCpmm => "DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8",
        
        Dex::RaydiumLaunchpad => "6s1xP3hpbAfFoNtUNF8mfHsjr2Bd97JxFJRWLbL6aHuX",
        
        Dex::RaydiumClmm => "E9wKBsFAEkmFKXcHUTfXrxTmNw5E8Vw45BtPwKMg8b7z",
    };
    Ok(Pubkey::from_str(key)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    
    std::env::set_var("DRY_RUN", "true");
    sniper_bot::utils::logging::init_logger();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut args = std::env::args().skip(1);
    let dex: Dex = args
        .next()
        .ok_or_else(|| anyhow!("usage: replay <dex> [count]"))?
        .parse()?;
    let rest: Vec<String> = args.collect();
    let signatures: Vec<Signature> = match rest.first().map(String::as_str) {
        Some("--tx") => rest[1..].iter().map(|s| Signature::from_str(s)).collect::<Result<_, _>>()?,
        _ => Vec::new(),
    };
    let count: usize = if signatures.is_empty() { rest.first().map(|c| c.parse()).transpose()?.unwrap_or(5) } else { 0 };
    let setting = &*TRADE_SETTING;
    // The bot starts paused until the dashboard's "Start trading"; replay has no dashboard.
    sniper_bot::utils::control::set_trading(true);
    if setting.dry_run_wallet.is_none() {
        log::warn!("DRY_RUN_WALLET is not set; simulations use PRIVATE_KEY's wallet, which needs SOL");
    }

    tokio::spawn(recent_blockhash_handler(RPC_CLIENT.clone()));
    wait_for_blockhash(Duration::from_secs(20)).await?;
    if matches!(dex, Dex::PumpFun | Dex::PumpSwap) {
        init_pumpfun_config(RPC_CLIENT.clone()).await?;
        init_pumpswap_config(RPC_CLIENT.clone()).await?;
    }

    let builder = Pipeline::builder().shutdown_strategy(ShutdownStrategy::Immediate);
    let fixed_list = !signatures.is_empty();
    let builder = if signatures.is_empty() {
        
        let marker = creation_marker(dex)?;
        let recent = RPC_CLIENT.get_signatures_for_address(&marker).await?;
        let until = recent
            .get(count.min(recent.len().saturating_sub(1)))
            .map(|s| Signature::from_str(&s.signature))
            .transpose()?;
        log::info!("replaying {dex} creations via {marker} (backfill {count}, then polling)");
        builder.datasource(RpcTransactionCrawler::new(
            rpc_endpoint(),
            marker,
            ConnectionConfig::new(25, Duration::from_secs(3), 2, RetryConfig::default(), None, None, false),
            Filters::new(None, None, until),
            Some(CommitmentConfig::confirmed()),
        ))
    } else {
        log::info!("replaying {} transaction(s) through the {dex} processor", signatures.len());
        builder.datasource(SignatureList(signatures))
    };
    let builder = match dex {
        Dex::PumpFun => builder.instruction(PumpfunDecoder, PumpfunProcess),
        Dex::PumpSwap => builder.instruction(PumpSwapDecoder, PumpSwapProcess),
        Dex::RaydiumAmm => builder.instruction(RaydiumAmmV4Decoder, RaydiumV4Process),
        Dex::RaydiumCpmm => builder.instruction(RaydiumCpmmDecoder, RayCpmmProcess),
        Dex::RaydiumLaunchpad => builder.instruction(RaydiumLaunchpadDecoder, RayLaunchPadProcess),
        Dex::RaydiumClmm => builder.instruction(RaydiumClmmDecoder, RayClmmProcess),
    };
    builder.build()?.run().await?;
    if fixed_list {
        
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
    Ok(())
}


struct SignatureList(Vec<Signature>);

#[async_trait]
impl Datasource for SignatureList {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: Sender<(Update, DatasourceId)>,
        _cancellation_token: CancellationToken,
    ) -> CarbonResult<()> {
        let config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(1),
        };
        for signature in &self.0 {
            let fetched = RPC_CLIENT
                .get_transaction_with_config(signature, config)
                .await
                .map_err(|e| CarbonError::Custom(format!("getTransaction {signature}: {e}")))?;
            let transaction = fetched
                .transaction
                .transaction
                .decode()
                .ok_or_else(|| CarbonError::Custom(format!("cannot decode {signature}")))?;
            let meta = fetched
                .transaction
                .meta
                .ok_or_else(|| CarbonError::Custom(format!("{signature} has no meta")))?;
            let update = TransactionUpdate {
                signature: *signature,
                transaction,
                meta: transaction_metadata_from_original_meta(meta)?,
                is_vote: false,
                slot: fetched.slot,
                index: None,
                block_time: fetched.block_time,
                block_hash: None,
            };
            let _ = sender.send((Update::Transaction(Box::new(update)), id.clone())).await;
        }
        Ok(())
    }

    fn update_types(&self) -> Vec<UpdateType> {
        vec![UpdateType::Transaction]
    }
}
