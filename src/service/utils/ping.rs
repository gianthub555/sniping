use {
    crate::config::HTTP_CLIENT,
    std::time::{Duration, Instant},
};

const SAMPLES: usize = 4;






pub async fn http_latency(url: &str) -> Option<Duration> {
    
    HTTP_CLIENT.get(url).timeout(Duration::from_secs(3)).send().await.ok()?;
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        if HTTP_CLIENT.get(url).timeout(Duration::from_secs(2)).send().await.is_ok() {
            samples.push(started.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    samples.sort();
    samples.get(samples.len() / 2).copied()
}
