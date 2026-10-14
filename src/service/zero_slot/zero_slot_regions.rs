use crate::service::utils::services::Region;


const HOSTS: [(&str, &str); 5] = [
    ("new-york", "ny1"),
    ("frankfurt", "de1"),
    ("amsterdam", "ams1"),
    ("tokyo", "jp1"),
    ("los-angeles", "la1"),
];

pub fn zero_slot_regions() -> Vec<Region> {
    HOSTS
        .iter()
        .map(|(name, host)| {
            let base_url = format!("http://{host}.0slot.trade");
            Region { name, probe_url: zero_slot_probe_url(&base_url), base_url }
        })
        .collect()
}


pub fn zero_slot_probe_url(base_url: &str) -> String {
    format!("{}/health", base_url.trim_end_matches('/'))
}
