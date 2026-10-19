//! Linux-only optional probe. Kernel VmHWM is read after exec, not inherited
//! pre-exec fork RSS. Arguments: DATASET OD_JSON.
use routeweft_dataset::{LoadOptions, Point};
use routeweft_routing::{Engine, QueryOptions, SearchBudget};
use std::path::Path;
use std::time::Instant;
fn memory() -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter(|s| s.starts_with("VmHWM:") || s.starts_with("VmRSS:"))
        .collect::<Vec<_>>()
        .join("\n")
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: memory_probe DATASET OD_JSON".into());
    }
    let load = Instant::now();
    let engine = Engine::open(Path::new(&args[1]), LoadOptions::default())?;
    let load_ms = load.elapsed().as_secs_f64() * 1000.;
    let loaded = memory();
    let pairs: Vec<[Point; 2]> = serde_json::from_slice(&std::fs::read(&args[2])?)?;
    let mut status = std::collections::BTreeMap::new();
    for pair in &pairs {
        let result = engine.route(pair, QueryOptions::default(), &SearchBudget::default());
        let key = result
            .err()
            .map(|e| format!("{e:?}"))
            .unwrap_or_else(|| "ok".into());
        *status.entry(key).or_insert(0_usize) += 1;
    }
    println!(
        "{}",
        serde_json::json!({"dataset_id":engine.manifest().dataset_id,"load_ms":load_ms,"loaded":loaded,"after_queries":memory(),"od_count":pairs.len(),"status":status})
    );
    Ok(())
}
