use {
    anyhow::{anyhow, bail, Result},
    futures::StreamExt,
    std::{collections::HashMap, time::Duration},
    yellowstone_grpc_client::GeyserGrpcClient,
    yellowstone_grpc_proto::{
        geyser::{SubscribeRequest, SubscribeRequestFilterSlots},
        tonic::transport::ClientTlsConfig,
    },
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(15);




pub async fn check_grpc_endpoint(url: &str, token: Option<String>) -> Result<()> {
    let mut client = GeyserGrpcClient::build_from_shared(url.to_string())
        .map_err(|e| anyhow!("invalid LOCAL_GRPC_URL: {e}"))?
        .x_token(token)
        .map_err(|e| anyhow!("invalid GRPC_TOKEN: {e}"))?
        .tls_config(ClientTlsConfig::new().with_enabled_roots())?
        .connect_timeout(CHECK_TIMEOUT)
        .timeout(CHECK_TIMEOUT)
        .connect()
        .await
        .map_err(|e| anyhow!("cannot connect to gRPC endpoint {url}: {e}"))?;

    let request = SubscribeRequest {
        slots: HashMap::from([("startup_check".to_string(), SubscribeRequestFilterSlots::default())]),
        ..Default::default()
    };
    let (_sink, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .map_err(|e| anyhow!("gRPC subscription failed: {e}"))?;
    match tokio::time::timeout(CHECK_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(_))) => Ok(()),
        Ok(Some(Err(status))) => bail!(
            "gRPC endpoint rejected the subscription ({:?}): {}. Check LOCAL_GRPC_URL and GRPC_TOKEN",
            status.code(),
            status.message()
        ),
        Ok(None) => bail!("gRPC endpoint closed the stream immediately; check LOCAL_GRPC_URL and GRPC_TOKEN"),
        Err(_) => bail!("gRPC endpoint sent nothing within {CHECK_TIMEOUT:?}"),
    }
}
