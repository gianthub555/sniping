




use {
    sniper_bot::{
        config::RPC_CLIENT,
        instructions::pumpfun_buy_ix::{init_pumpfun_config, pumpfun_config},
        processors::shred_feed::{curve_after, launches_in_tx},
    },
    std::io::BufRead,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_pumpfun_config(RPC_CLIENT.clone()).await?;
    let config = pumpfun_config()?;
    let fee_bps = config.fee_tiers.first().map(|(_, f)| f.protocol_bps + f.creator_bps).unwrap_or(125);
    use base64::Engine;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let Some((sig, b64)) = line.split_once(' ') else { continue };
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
        let launches = launches_in_tx(&bytes);
        if launches.is_empty() {
            println!("{sig} none");
        }
        for l in launches {
            let r = curve_after(&l, config.initial_reserves, fee_bps);
            println!(
                "{sig} {} {} {} {} {} {} {} {} {}",
                l.mint, l.deployer, l.creator, l.mayhem, l.dev_buy_lamports, r.real_sol_reserves, r.virtual_token_reserves,
                solana_pubkey::Pubkey::find_program_address(
                    &[b"sharing-config", l.mint.as_ref()],
                    &sniper_bot::instructions::pumpswap_buy_ix::PUMP_FEE_PROGRAM_ID
                )
                .0,
                l.creator_set_onchain
            );
        }
    }
    Ok(())
}
