use crate::service::utils::services::Region;

const REGIONS: [(&str, &str); 9] = [
    ("global", "mainnet"),
    ("amsterdam", "amsterdam.mainnet"),
    ("dublin", "dublin.mainnet"),
    ("frankfurt", "frankfurt.mainnet"),
    ("london", "london.mainnet"),
    ("ny", "ny.mainnet"),
    ("slc", "slc.mainnet"),
    ("singapore", "singapore.mainnet"),
    ("tokyo", "tokyo.mainnet"),
];

pub fn jito_regions() -> Vec<Region> {
    REGIONS
        .iter()
        .map(|(name, host)| {
            let base_url = format!("https://{host}.block-engine.jito.wtf");
            Region { name, probe_url: jito_probe_url(&base_url), base_url }
        })
        .collect()
}



pub fn jito_probe_url(base_url: &str) -> String {
    format!("{}/api/v1/getTipAccounts", base_url.trim_end_matches('/'))
}
