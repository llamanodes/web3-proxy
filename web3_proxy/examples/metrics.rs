use metered::{metered, HitCount, Throughput};
use serde::Serialize;

#[derive(Default, Debug, Serialize)]
pub struct Biz {
    metrics: BizMetrics,
}

#[metered(registry = BizMetrics)]
impl Biz {
    #[measure([HitCount, Throughput])]
    pub fn biz(&self) {
        let delay = std::time::Duration::from_millis(rand::random::<u64>() % 200);
        std::thread::sleep(delay);
    }
}

fn main() {
    let buz = Biz::default();

    for _ in 0..100 {
        buz.biz();
    }

    let mut globals = std::collections::HashMap::new();
    globals.insert("service", "web3_proxy_prometheus_example");

    let serialized = serde_prometheus::to_string(&buz.metrics, Some("example"), globals).unwrap();

    println!("{}", serialized);
}
