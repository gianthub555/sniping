use {
    super::{parse::mint_decimals, utils::WSOL_MINT},
    crate::config::{HTTP_CLIENT, RPC_CLIENT, TRADE_SETTING},
    anyhow::{anyhow, bail, Context, Result},
    serde::Deserialize,
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::{LazyLock, Mutex},
    },
};

const BIRDEYE_PRICE_URL: &str = "https://public-api.birdeye.so/defi/price";

static DECIMALS: LazyLock<Mutex<HashMap<Pubkey, u8>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
struct BirdeyeResponse {
    success: bool,
    data: Option<BirdeyePrice>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BirdeyePrice {
    value: Option<f64>,
}


pub async fn birdeye_price_usd(mint: &Pubkey) -> Result<f64> {
    let api_key = TRADE_SETTING
        .birdeye_api_key
        .as_deref()
        .ok_or_else(|| anyhow!("BIRDEYE_API_KEY is not set"))?;
    let response: BirdeyeResponse = HTTP_CLIENT
        .get(BIRDEYE_PRICE_URL)
        .query(&[("address", mint.to_string())])
        .header("X-API-KEY", api_key)
        .header("x-chain", "solana")
        .header("accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("unexpected Birdeye response")?;
    if !response.success {
        bail!("Birdeye error: {}", response.message.unwrap_or_default());
    }
    match response.data.and_then(|d| d.value) {
        Some(price) if price > 0.0 && price.is_finite() => Ok(price),
        _ => bail!("Birdeye has no price for {mint}"),
    }
}


pub async fn birdeye_value_lamports(mint: &Pubkey, raw_amount: u64) -> Result<u64> {
    let (token_usd, sol_usd, decimals) =
        tokio::try_join!(birdeye_price_usd(mint), birdeye_price_usd(&WSOL_MINT), decimals(mint))?;
    let whole_tokens = raw_amount as f64 / 10f64.powi(decimals as i32);
    Ok((whole_tokens * token_usd / sol_usd * 1e9) as u64)
}

async fn decimals(mint: &Pubkey) -> Result<u8> {
    if let Some(d) = DECIMALS.lock().unwrap_or_else(|e| e.into_inner()).get(mint) {
        return Ok(*d);
    }
    let account = RPC_CLIENT.get_account(mint).await?;
    let d = mint_decimals(&account.data)?;
    DECIMALS.lock().unwrap_or_else(|e| e.into_inner()).insert(*mint, d);
    Ok(d)
}
