use {
    super::zero_slot_regions::{zero_slot_probe_url, zero_slot_regions},
    crate::{
        config::{env_opt, TRADE_SETTING},
        service::utils::services::{fastest_region, send_json_rpc_transaction, spawn_keep_alive},
    },
    anyhow::{anyhow, bail, Result},
    std::sync::OnceLock,
};

static ZERO_SLOT_URL: OnceLock<String> = OnceLock::new();



pub async fn init_zero_slot() -> Result<()> {
    let url = match (&TRADE_SETTING.zero_slot_api_url, env_opt("ZERO_SLOT_API_KEY")) {
        (Some(url), _) => {
            if !url.contains("api-key=") {
                bail!("ZERO_SLOT_API_URL must include the API key as ?api-key=<KEY>");
            }
            url.clone()
        }
        (None, Some(key)) => {
            let region = fastest_region("0slot", zero_slot_regions()).await?;
            format!("{}/?api-key={key}", region.base_url)
        }
        (None, None) => {
            bail!("CONFIRM_SERVICE=ZERO_SLOT needs ZERO_SLOT_API_URL or ZERO_SLOT_API_KEY")
        }
    };
    let base = url.split('?').next().unwrap_or(&url);
    spawn_keep_alive(zero_slot_probe_url(base));
    let _ = ZERO_SLOT_URL.set(url);
    Ok(())
}


pub async fn send_zero_slot(tx_base64: &str) -> Result<String> {
    let url = ZERO_SLOT_URL.get().ok_or_else(|| anyhow!("0slot is not initialised"))?;
    send_json_rpc_transaction("0slot", url, tx_base64, &[]).await
}
