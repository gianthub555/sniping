







use {
    log::{info, warn},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        fs::OpenOptions,
        io::Write,
        str::FromStr,
        sync::{LazyLock, Mutex},
    },
};

#[cfg(not(test))]
pub const CREATOR_HISTORY_PATH: &str = "creator_history.csv";
/// Tests write here, never to the bot's own history.
#[cfg(test)]
pub const CREATOR_HISTORY_PATH: &str = "target/test-creator_history.csv";
const HEADER: &str = "unix_time,deployer,mint,dev_buy_lamports";

const RETENTION_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, Default)]
struct DeployerStats {
    launches: u32,
    first_launch: u64,
    last_launch: u64,
    last_mint: Option<Pubkey>,
    
    previous_mint: Option<Pubkey>,
    
    recent: Vec<u64>,
}

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

impl DeployerStats {
    fn launches_last_hour(&mut self, now: u64) -> u32 {
        self.recent.retain(|t| now.saturating_sub(*t) < DAY);
        self.recent.iter().filter(|t| now.saturating_sub(**t) < HOUR).count() as u32
    }

    fn launches_last_day(&mut self, now: u64) -> u32 {
        self.recent.retain(|t| now.saturating_sub(*t) < DAY);
        self.recent.len() as u32
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorLaunches {
    
    pub count: u32,
    
    pub secs_since_previous: Option<u64>,
    
    pub last_hour: u32,
    
    pub last_day: u32,
    
    pub first_seen_secs_ago: Option<u64>,
    
    pub previous_mint: Option<Pubkey>,
}

impl PriorLaunches {
    pub fn describe(&self) -> String {
        match self.secs_since_previous {
            None => "first launch seen from this deployer".to_string(),
            Some(secs) => format!(
                "deployer's {} earlier launch{}, previous {} ago{}",
                self.count,
                if self.count == 1 { "" } else { "es" },
                human_duration(secs),
                if self.last_hour > 1 { format!(" ({} in the last hour)", self.last_hour) } else { String::new() }
            ),
        }
    }
}

struct Registry {
    deployers: HashMap<Pubkey, DeployerStats>,
    
    mint_deployer: HashMap<Pubkey, Pubkey>,
    
    since: u64,
}

static REGISTRY: LazyLock<Mutex<Registry>> =
    LazyLock::new(|| Mutex::new(Registry { deployers: HashMap::new(), mint_deployer: HashMap::new(), since: 0 }));


/// How far past the clock a row's time may be before the row counts as damaged.
const MAX_FUTURE_SECS: u64 = 3600;

/// A well-formed row: exactly the four fields and a time not in the future. Rows written by two
/// launches at once used to interleave (29-30 Sep: 1,061 damaged rows), and a merged time such as
/// `17903961761790396176` still parses, as a launch in the far future: its deployer then read as
/// relaunching 0 s ago, with a launch in every 24 h, after each restart.
fn parse_row(line: &str, now: u64) -> Option<(u64, Pubkey, Pubkey)> {
    let fields: Vec<&str> = line.split(',').collect();
    let [time, deployer, mint, dev_buy] = fields[..] else { return None };
    let time = time.parse::<u64>().ok().filter(|t| *t <= now + MAX_FUTURE_SECS)?;
    dev_buy.parse::<u64>().ok()?;
    Some((time, Pubkey::from_str(deployer).ok()?, Pubkey::from_str(mint).ok()?))
}

pub fn load(now: u64) {
    let started = std::time::Instant::now();
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry.since = now;
    let cutoff = now.saturating_sub(RETENTION_SECS);
    // The store (STORE_FILES=off): the retention window's rows by an indexed query; else the file.
    let rows: Vec<(u64, Pubkey, Pubkey)> = if crate::store::read_from_db() {
        crate::store::read::creator_history(cutoff)
            .into_iter()
            .filter(|(t, _, _)| *t <= now + MAX_FUTURE_SECS)
            .filter_map(|(t, d, m)| Some((t, Pubkey::from_str(&d).ok()?, Pubkey::from_str(&m).ok()?)))
            .collect()
    } else {
        let Ok(text) = std::fs::read_to_string(CREATOR_HISTORY_PATH) else {
            return;
        };
        text.lines().skip(1).filter_map(|line| parse_row(line, now)).collect()
    };
    let mut loaded = 0usize;
    for (time, deployer, mint) in rows {
        if time < cutoff {
            continue;
        }
        registry.mint_deployer.insert(mint, deployer);
        let mint = Some(mint);
        let stats = registry.deployers.entry(deployer).or_default();
        
        if time >= stats.last_launch && mint.is_some() && mint != stats.last_mint {
            stats.previous_mint = stats.last_mint;
            stats.last_mint = mint;
        }
        stats.launches += 1;
        stats.first_launch = if stats.first_launch == 0 { time } else { stats.first_launch.min(time) };
        stats.last_launch = stats.last_launch.max(time);
        if now.saturating_sub(time) < DAY {
            stats.recent.push(time);
        }
        registry.since = registry.since.min(time);
        loaded += 1;
    }
    info!(
        "creator history: {loaded} launches by {} deployers since {} ago, loaded from {} in {:.1} ms",
        registry.deployers.len(),
        human_duration(now.saturating_sub(registry.since)),
        if crate::store::read_from_db() { "the store" } else { CREATOR_HISTORY_PATH },
        started.elapsed().as_secs_f64() * 1e3
    );
}



pub fn latest_mints_since(cutoff: u64) -> Vec<Pubkey> {
    let registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry
        .deployers
        .values()
        .filter(|d| d.launches >= 2 && d.last_launch >= cutoff)
        .filter_map(|d| d.last_mint)
        .collect()
}


/// Records a launch in memory only, without writing the history file (benchmarks and tests).
#[doc(hidden)]
pub fn seed_launch(deployer: &Pubkey, mint: &Pubkey, now: u64) {
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry.mint_deployer.insert(*mint, *deployer);
    let stats = registry.deployers.entry(*deployer).or_default();
    stats.launches += 1;
    if stats.first_launch == 0 {
        stats.first_launch = now;
    }
    stats.last_launch = now;
    stats.previous_mint = stats.last_mint;
    stats.last_mint = Some(*mint);
    stats.recent.push(now);
}

/// What `observe_launch` would return for this launch, without recording it or allocating (the
/// hot path decides on this; the slow path still records the launch).
pub fn prior(deployer: &Pubkey, mint: &Pubkey, now: u64) -> PriorLaunches {
    let registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(stats) = registry.deployers.get(deployer) else {
        return PriorLaunches { count: 0, secs_since_previous: None, last_hour: 0, last_day: 0, first_seen_secs_ago: None, previous_mint: None };
    };
    let last_day = stats.recent.iter().filter(|t| now.saturating_sub(**t) < DAY).count() as u32;
    let last_hour = stats.recent.iter().filter(|t| now.saturating_sub(**t) < DAY && now.saturating_sub(**t) < HOUR).count() as u32;
    if stats.last_mint == Some(*mint) {
        return PriorLaunches {
            count: stats.launches.saturating_sub(1),
            secs_since_previous: None,
            last_hour: last_hour.saturating_sub(1),
            last_day: last_day.saturating_sub(1),
            first_seen_secs_ago: (stats.launches > 1).then(|| now.saturating_sub(stats.first_launch)),
            previous_mint: stats.previous_mint,
        };
    }
    PriorLaunches {
        count: stats.launches,
        secs_since_previous: (stats.launches > 0).then(|| now.saturating_sub(stats.last_launch)),
        last_hour,
        last_day,
        first_seen_secs_ago: (stats.launches > 0).then(|| now.saturating_sub(stats.first_launch)),
        previous_mint: stats.last_mint,
    }
}

pub fn observe_launch(deployer: &Pubkey, mint: &Pubkey, dev_buy_lamports: u64, now: u64) -> PriorLaunches {
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    registry.mint_deployer.insert(*mint, *deployer);
    let stats = registry.deployers.entry(*deployer).or_default();
    let last_hour = stats.launches_last_hour(now);
    let last_day = stats.launches_last_day(now);
    if stats.last_mint == Some(*mint) {
        
        return PriorLaunches {
            count: stats.launches.saturating_sub(1),
            secs_since_previous: None,
            last_hour: last_hour.saturating_sub(1),
            last_day: last_day.saturating_sub(1),
            first_seen_secs_ago: (stats.launches > 1).then(|| now.saturating_sub(stats.first_launch)),
            previous_mint: stats.previous_mint,
        };
    }
    let prior = PriorLaunches {
        count: stats.launches,
        secs_since_previous: (stats.launches > 0).then(|| now.saturating_sub(stats.last_launch)),
        last_hour,
        last_day,
        first_seen_secs_ago: (stats.launches > 0).then(|| now.saturating_sub(stats.first_launch)),
        previous_mint: stats.last_mint,
    };
    stats.launches += 1;
    if stats.first_launch == 0 {
        stats.first_launch = now;
    }
    stats.last_launch = now;
    stats.previous_mint = stats.last_mint;
    stats.last_mint = Some(*mint);
    stats.recent.push(now);
    drop(registry);

    // Written by its own thread: this runs on the launch path, before the buy.
    let _ = WRITER.send(format!("{now},{deployer},{mint},{dev_buy_lamports}\n"));
    prior
}

/// Appends rows to `CREATOR_HISTORY_PATH` off the launch path (an open, a size check and a write
/// per launch, which can stall for milliseconds when the disk is busy). One writer: whole rows
/// never interleave.
static WRITER: LazyLock<std::sync::mpsc::Sender<String>> = LazyLock::new(|| {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let spawned = std::thread::Builder::new().name("creator-history".into()).spawn(move || {
        for row in rx {
            if crate::store::db_on() {
                let f = crate::store::split_csv(row.trim_end());
                if let [time, deployer, mint, dev] = &f[..] {
                    crate::store::write(crate::store::Row::Exec {
                        sql: "INSERT INTO creator_history (unix_time, deployer, mint, dev_buy_lamports) VALUES (?1, ?2, ?3, ?4)",
                        params: vec![
                            time.parse::<i64>().unwrap_or(0).into(),
                            deployer.as_str().into(),
                            mint.as_str().into(),
                            dev.parse::<i64>().unwrap_or(0).into(),
                        ],
                    });
                }
            }
            if !crate::store::files_on() {
                continue;
            }
            let result = (|| -> std::io::Result<()> {
                let mut file = OpenOptions::new().create(true).append(true).open(CREATOR_HISTORY_PATH)?;
                let header = if file.metadata()?.len() == 0 { format!("{HEADER}\n") } else { String::new() };
                file.write_all(format!("{header}{row}").as_bytes())
            })();
            if let Err(e) = result {
                warn!("could not write {CREATOR_HISTORY_PATH}: {e}");
            }
        }
    });
    if let Err(e) = spawned {
        warn!("creator history writer not started, rows are not saved: {e}");
    }
    tx
});


pub fn history_span_secs(now: u64) -> u64 {
    let registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if registry.since == 0 { 0 } else { now.saturating_sub(registry.since) }
}

pub fn known_deployers() -> usize {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).deployers.len()
}

pub fn human_duration(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{:.1}h", s as f64 / 3600.0),
        s => format!("{:.1}d", s as f64 / 86_400.0),
    }
}


pub fn deployer_of(mint: &Pubkey) -> Option<Pubkey> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).mint_deployer.get(mint).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_answers_like_observe_launch_before_and_after_it() {
        let deployer = Pubkey::new_unique();
        let now = 2_000_000_000;
        for (k, at) in [now - 2 * DAY, now - 5 * HOUR, now - 100].iter().enumerate() {
            let mint = Pubkey::new_unique();
            let before = prior(&deployer, &mint, *at);
            assert_eq!(before, observe_launch(&deployer, &mint, 1, *at), "launch {k}: as observe_launch, before it");
            assert_eq!(prior(&deployer, &mint, *at), observe_launch(&deployer, &mint, 1, *at), "launch {k}: the same mint again");
        }
        let unknown = Pubkey::new_unique();
        assert_eq!(prior(&unknown, &Pubkey::new_unique(), now).count, 0);
    }

    #[test]
    fn damaged_rows_are_left_out() {
        let now = 1_790_760_000;
        let good = "1790697193,6tyjMaJxS46dXXhffB5w2G1Tp5owD1J9pCdhenwexykP,FErnzgGTNo1kuJCTUZ7or6Twg1tH5Rd6EHP2fV5eS1SJ,9000000000";
        assert!(parse_row(good, now).is_some());
        // Rows from creator_history.csv that two writers interleaved.
        for bad in [
            "17903961761790396176,6tyjMaJxS46dXXhffB5w2G1Tp5owD1J9pCdhenwexykP,,5jBejMq7pW9WQGEirusT9wvMHBgWynScc4u7tGn7bArc",
            "17903961761790396176,6tyjMaJxS46dXXhffB5w2G1Tp5owD1J9pCdhenwexykP,FErnzgGTNo1kuJCTUZ7or6Twg1tH5Rd6EHP2fV5eS1SJ,9000000000",
            "1790356694,AkRTHcct8GP1vxUZW76LYUdL4aTQQvq5fq63SRRyi5Sg,1790356694HEdiSG7iGdvUCgm5sVN4qsstbYqTuBZ7gLwUU9xnpump,,39506171",
            ",2962961",
            "99012169",
        ] {
            assert_eq!(parse_row(bad, now), None, "{bad}");
        }
        assert_eq!(parse_row(good, 1_790_697_193 - 2 * MAX_FUTURE_SECS), None, "in the future");
    }
}
