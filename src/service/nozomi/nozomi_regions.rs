use crate::service::utils::services::Region;


const HOSTS: [(&str, &str); 9] = [
    ("pittsburgh", "pit1"),
    ("newark", "ewr1"),
    ("ashburn", "ash1"),
    ("los-angeles", "lax1"),
    ("frankfurt", "fra2"),
    ("amsterdam", "ams1"),
    ("london", "lon1"),
    ("tokyo", "tyo1"),
    ("singapore", "sgp1"),
];

pub fn nozomi_regions() -> Vec<Region> {
    HOSTS
        .iter()
        .map(|(name, host)| {
            let base_url = format!("http://{host}.nozomi.temporal.xyz");
            Region { name, probe_url: nozomi_probe_url(&base_url), base_url }
        })
        .collect()
}

pub fn nozomi_probe_url(base_url: &str) -> String {
    format!("{}/ping", base_url.trim_end_matches('/'))
}
