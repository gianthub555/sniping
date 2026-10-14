use {
    super::trade_setting::env_opt,
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{
        sync::{Arc, LazyLock},
        time::Duration,
    },
};

pub const DEFAULT_RPC_ENDPOINT: &str = "https://api.mainnet-beta.solana.com";



pub static RPC_CLIENT: LazyLock<Arc<RpcClient>> = LazyLock::new(|| {
    Arc::new(RpcClient::new_with_timeout_and_commitment(
        rpc_endpoint(),
        Duration::from_secs(15),
        CommitmentConfig::processed(),
    ))
});



pub static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client")
});

pub fn rpc_endpoint() -> String {
    env_opt("RPC_ENDPOINT").unwrap_or_else(|| DEFAULT_RPC_ENDPOINT.to_string())
}
