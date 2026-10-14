//! Where each slot's block producer is, for `LEADER_REGIONS`: skip a launch whose producer is
//! too far from this server for our buy to be first. Measured on 76 real landings from Frankfurt
//! (25-26 Sep 2026): with another sniper racing, we were first on 36% of launches produced in
//! Europe, 11% in the US and 20% in Asia; on the live buys 24% in Europe against 1 in 13
//! elsewhere. Every lost race still pays the priority fee on Nozomi.
//!
//! The epoch's leader schedule comes from RPC (reloaded when the epoch changes), each producer's
//! address from the cluster's gossip list, and its location from ip-api.com (cached in
//! `REGIONS_FILE`). A producer whose location is not known yet never blocks a buy.

use {
    crate::config::{HTTP_CLIENT, RPC_CLIENT},
    log::{info, warn},
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        str::FromStr,
        sync::{LazyLock, RwLock},
        time::Duration,
    },
};

/// Cached locations by IP address.
const REGIONS_FILE: &str = "logs/validator_regions.json";
/// Every leader produces four consecutive slots.
const LEADER_SLOTS: u64 = 4;
/// ip-api.com's free batch endpoint: 100 addresses a request, 15 requests a minute.
const GEO_BATCH: usize = 100;
const GEO_PAUSE: Duration = Duration::from_secs(5);
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Place {
    /// Two-letter continent code (EU, NA, AS, ...).
    pub continent: String,
    pub country: String,
    pub city: String,
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}, {}", self.city, self.country)
    }
}

#[derive(Default)]
struct Schedule {
    epoch_start: u64,
    epoch_len: u64,
    /// Producer of each slot of the epoch, as an index into `producers`.
    by_slot: Vec<u32>,
    producers: Vec<Pubkey>,
}

static SCHEDULE: LazyLock<RwLock<Schedule>> = LazyLock::new(Default::default);
/// Node identity -> (TPU QUIC address, software version, client), from gossip.
type Node = (Option<std::net::SocketAddr>, Option<String>, Option<String>, Option<std::net::SocketAddr>);
static NODES: LazyLock<RwLock<HashMap<Pubkey, Node>>> = LazyLock::new(Default::default);

/// The producer of `slot`, from the cached schedule.
pub fn leader(slot: u64) -> Option<Pubkey> {
    let schedule = SCHEDULE.read().unwrap_or_else(|e| e.into_inner());
    let index = slot.checked_sub(schedule.epoch_start).filter(|i| *i < schedule.epoch_len)?;
    schedule.producers.get(*schedule.by_slot.get(index as usize)? as usize).copied()
}

/// A node's TPU QUIC address, from gossip.
pub fn tpu_quic(identity: &Pubkey) -> Option<std::net::SocketAddr> {
    NODES.read().unwrap_or_else(|e| e.into_inner()).get(identity).and_then(|n| n.0)
}

/// A node's software version, from gossip (Agave and Jito report 2.x/3.x, Firedancer 0.x).
pub fn version(identity: &Pubkey) -> Option<String> {
    NODES.read().unwrap_or_else(|e| e.into_inner()).get(identity).and_then(|n| n.1.clone())
}

/// A node's client as gossip names it (Agave, JitoLabs, AgaveBam, Firedancer, Frankendancer, ...).
pub fn client(identity: &Pubkey) -> Option<String> {
    NODES.read().unwrap_or_else(|e| e.into_inner()).get(identity).and_then(|n| n.2.clone())
}

/// Whether a node's TPU sits behind a Jito relayer: its gossip TPU QUIC port is in the relayer's
/// 11000-11999 range, or at another address than its gossip. Of the 141 JitoLabs leaders of 8 Oct,
/// 29 had a relayer on their own host (11xxx ports), 12 at another address, 99 none.
pub fn behind_relayer(identity: &Pubkey) -> Option<bool> {
    let nodes = NODES.read().unwrap_or_else(|e| e.into_inner());
    let (tpu, _, _, gossip) = nodes.get(identity)?;
    let tpu = (*tpu)?;
    Some((11_000..12_000).contains(&tpu.port()) || gossip.is_some_and(|g| g.ip() != tpu.ip()))
}

/// Starts the schedule and gossip refresh unless it already runs (LEADER_REGIONS starts it too).
pub fn ensure_spawned() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        spawn();
    }
}

/// Producer identity -> place.
static PLACES: LazyLock<RwLock<HashMap<Pubkey, Place>>> = LazyLock::new(Default::default);

/// Where a node is, when known.
pub fn place(identity: &Pubkey) -> Option<Place> {
    PLACES.read().unwrap_or_else(|e| e.into_inner()).get(identity).cloned()
}

/// The producer of `slot` and where it is, when known.
pub fn producer(slot: u64) -> Option<(Pubkey, Option<Place>)> {
    let schedule = SCHEDULE.read().unwrap_or_else(|e| e.into_inner());
    let index = slot.checked_sub(schedule.epoch_start).filter(|i| *i < schedule.epoch_len)?;
    let producer = *schedule.producers.get(*schedule.by_slot.get(index as usize)? as usize)?;
    let place = PLACES.read().unwrap_or_else(|e| e.into_inner()).get(&producer).cloned();
    Some((producer, place))
}

/// Why a launch in `slot` should be skipped: its producer is known to be outside `allowed`
/// (continent codes), or the launch is in its producer's last slot and the next producer is.
/// Our buy lands in the launch slot or the next one (73% and most of the rest, from Europe).
pub fn far_producer(slot: u64, allowed: &[String]) -> Option<String> {
    let far = |s: u64| -> Option<Place> {
        let (_, place) = producer(s)?;
        let place = place?;
        (!allowed.iter().any(|c| c.eq_ignore_ascii_case(&place.continent))).then_some(place)
    };
    if let Some(place) = far(slot) {
        return Some(format!("block producer in {place}, outside LEADER_REGIONS={}", allowed.join(",")));
    }
    if slot % LEADER_SLOTS == LEADER_SLOTS - 1 {
        if let Some(place) = far(slot + 1) {
            return Some(format!(
                "launch in its producer's last slot and the next producer is in {place}, outside LEADER_REGIONS={}",
                allowed.join(",")
            ));
        }
    }
    None
}

/// Whether `far_producer` would skip a launch in `slot`, without formatting or cloning (the hot
/// path). Same decision.
pub fn far_producer_skips(slot: u64, allowed: &[String]) -> bool {
    let far = |s: u64| -> bool {
        let schedule = SCHEDULE.read().unwrap_or_else(|e| e.into_inner());
        let Some(index) = s.checked_sub(schedule.epoch_start).filter(|i| *i < schedule.epoch_len) else { return false };
        let Some(producer) = schedule.by_slot.get(index as usize).and_then(|p| schedule.producers.get(*p as usize)) else { return false };
        let places = PLACES.read().unwrap_or_else(|e| e.into_inner());
        places.get(producer).is_some_and(|place| !allowed.iter().any(|c| c.eq_ignore_ascii_case(&place.continent)))
    };
    far(slot) || (slot % LEADER_SLOTS == LEADER_SLOTS - 1 && far(slot + 1))
}

pub fn spawn() {
    tokio::spawn(async {
        let mut ips = load_cache();
        loop {
            if let Err(e) = refresh(&mut ips).await {
                warn!("leader regions: {e:#}");
            }
            tokio::time::sleep(REFRESH).await;
        }
    });
}

fn load_cache() -> HashMap<String, Place> {
    if crate::store::read_from_db() {
        return crate::store::read::validator_regions()
            .into_iter()
            .map(|(ip, continent, country, city)| {
                (ip, Place { continent: continent.unwrap_or_default(), country: country.unwrap_or_default(), city: city.unwrap_or_default() })
            })
            .collect();
    }
    std::fs::read_to_string(REGIONS_FILE).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// The located addresses to the store (a snapshot: the table is replaced).
fn store_regions(ips: &HashMap<String, Place>) {
    if crate::store::db_on() {
        let rows = ips.iter().map(|(ip, p)| vec![ip.as_str().into(), p.continent.as_str().into(), p.country.as_str().into(), p.city.as_str().into()]).collect();
        crate::store::write(crate::store::Row::Snapshot { table: "validator_regions", header: "ip,continent,country,city", rows, rx_ns: None });
    }
}

async fn refresh(ips: &mut HashMap<String, Place>) -> anyhow::Result<()> {
    let epoch = RPC_CLIENT.get_epoch_info().await?;
    let epoch_start = epoch.absolute_slot - epoch.slot_index;
    let loaded = SCHEDULE.read().unwrap_or_else(|e| e.into_inner()).epoch_start;
    if loaded != epoch_start || SCHEDULE.read().unwrap_or_else(|e| e.into_inner()).by_slot.is_empty() {
        let schedule = RPC_CLIENT
            .get_leader_schedule(Some(epoch.absolute_slot))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no leader schedule for slot {}", epoch.absolute_slot))?;
        let mut by_slot = vec![u32::MAX; epoch.slots_in_epoch as usize];
        let mut producers = Vec::with_capacity(schedule.len());
        for (identity, slots) in schedule {
            let Ok(key) = Pubkey::from_str(&identity) else { continue };
            let index = producers.len() as u32;
            producers.push(key);
            for s in slots {
                if let Some(entry) = by_slot.get_mut(s) {
                    *entry = index;
                }
            }
        }
        info!("leader regions: epoch {} schedule loaded ({} producers)", epoch.epoch, producers.len());
        *SCHEDULE.write().unwrap_or_else(|e| e.into_inner()) =
            Schedule { epoch_start, epoch_len: epoch.slots_in_epoch, by_slot, producers };
    }
    // Where each producer is: its gossip address, located once and cached.
    let producers: Vec<Pubkey> = SCHEDULE.read().unwrap_or_else(|e| e.into_inner()).producers.clone();
    let nodes = RPC_CLIENT.get_cluster_nodes().await?;
    *NODES.write().unwrap_or_else(|e| e.into_inner()) =
        nodes.iter().filter_map(|n| Some((Pubkey::from_str(&n.pubkey).ok()?, (n.tpu_quic, n.version.clone(), n.client_id.clone(), n.gossip)))).collect();
    let addr: HashMap<Pubkey, String> = nodes
        .iter()
        .filter_map(|n| Some((Pubkey::from_str(&n.pubkey).ok()?, n.gossip?.ip().to_string())))
        .collect();
    let missing: Vec<String> = {
        let mut m: Vec<String> = producers.iter().filter_map(|p| addr.get(p)).filter(|ip| !ips.contains_key(*ip)).cloned().collect();
        m.sort();
        m.dedup();
        m
    };
    if !missing.is_empty() {
        info!("leader regions: locating {} producer addresses", missing.len());
        for (i, batch) in missing.chunks(GEO_BATCH).enumerate() {
            if i > 0 {
                tokio::time::sleep(GEO_PAUSE).await;
            }
            match locate(batch).await {
                Ok(found) => ips.extend(found),
                Err(e) => {
                    warn!("leader regions: locating addresses failed: {e:#}");
                    break;
                }
            }
        }
        store_regions(ips);
        if let (true, Ok(json)) = (crate::store::files_on(), serde_json::to_string(&*ips)) {
            let _ = std::fs::write(REGIONS_FILE, json);
        }
    }
    let places: HashMap<Pubkey, Place> =
        producers.iter().filter_map(|p| Some((*p, ips.get(addr.get(p)?)?.clone()))).collect();
    let known = places.len();
    *PLACES.write().unwrap_or_else(|e| e.into_inner()) = places;
    if !missing.is_empty() {
        info!("leader regions: {known} of {} producers located", producers.len());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Located {
    query: String,
    status: String,
    #[serde(default)]
    continent_code: String,
    #[serde(default)]
    country_code: String,
    #[serde(default)]
    city: String,
}

/// Locations of `batch` from ip-api.com (the free endpoint is plain HTTP; only public validator
/// addresses are sent).
async fn locate(batch: &[String]) -> anyhow::Result<Vec<(String, Place)>> {
    let reply: Vec<Located> = HTTP_CLIENT
        .post("http://ip-api.com/batch?fields=query,status,continentCode,countryCode,city")
        .json(batch)
        .timeout(Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(reply
        .into_iter()
        .filter(|l| l.status == "success")
        .map(|l| (l.query, Place { continent: l.continent_code, country: l.country_code, city: l.city }))
        .collect())
}

#[cfg(test)]
pub fn set_for_test(epoch_start: u64, slots: Vec<(Pubkey, Place)>) {
    let producers: Vec<Pubkey> = slots.iter().map(|(p, _)| *p).collect();
    *SCHEDULE.write().unwrap_or_else(|e| e.into_inner()) =
        Schedule { epoch_start, epoch_len: slots.len() as u64, by_slot: (0..slots.len() as u32).collect(), producers };
    *PLACES.write().unwrap_or_else(|e| e.into_inner()) = slots.into_iter().collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(continent: &str) -> Place {
        Place { continent: continent.into(), country: "XX".into(), city: "Somewhere".into() }
    }

    #[test]
    fn far_producers_are_skipped_and_near_or_unknown_ones_are_not() {
        // The leader schedule is process-wide; tests that set it take turns.
        let _state = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let eu = vec!["EU".to_string()];
        let (a, b, c, d, e) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        // Slots 100-104: Europe, Europe, Europe, Europe (last of its four), then Asia.
        set_for_test(100, vec![(a, place("EU")), (b, place("EU")), (c, place("EU")), (d, place("EU")), (e, place("AS"))]);
        assert_eq!(far_producer(100, &eu), None);
        assert!(far_producer(104, &eu).unwrap().contains("outside LEADER_REGIONS=EU"));
        // Slot 103 is a producer's last slot (103 % 4 == 3) and the next one is in Asia.
        assert!(far_producer(103, &eu).unwrap().contains("last slot"));
        // Outside the loaded epoch: unknown, never blocks.
        assert_eq!(far_producer(99, &eu), None);
        assert_eq!(far_producer(500, &eu), None);
    }
}

#[cfg(test)]
mod hot_tests {
    use super::*;

    #[test]
    fn the_quick_check_skips_exactly_what_far_producer_skips() {
        let _state = crate::hot::testkit::GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let (eu, us) = (Pubkey::new_unique(), Pubkey::new_unique());
        let start = 1_000_000;
        // Slots start.. : EU, EU, EU, EU (last slot of the leader), then US.
        let mut by_slot = vec![0u32; 4];
        by_slot.extend([1u32; 4]);
        by_slot.push(u32::MAX);
        *SCHEDULE.write().unwrap() = Schedule { epoch_start: start, epoch_len: by_slot.len() as u64, by_slot, producers: vec![eu, us] };
        let place = |c: &str| Place { continent: c.into(), country: "X".into(), city: "Y".into() };
        PLACES.write().unwrap().insert(eu, place("EU"));
        PLACES.write().unwrap().insert(us, place("NA"));
        let allowed = vec!["EU".to_string()];
        for slot in start - 2..start + 12 {
            assert_eq!(far_producer_skips(slot, &allowed), far_producer(slot, &allowed).is_some(), "slot {}", slot - start);
        }
        assert!(far_producer_skips(start + 3, &allowed), "the EU leader's last slot, a US leader next");
        assert!(!far_producer_skips(start, &allowed));
    }
}
