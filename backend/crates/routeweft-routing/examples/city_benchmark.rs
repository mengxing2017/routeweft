//! Explicit opt-in city benchmark. No network or built-in local dataset path.
use routeweft_dataset::{open, LoadOptions, Point};
use routeweft_routing::{Engine, QueryOptions, SearchBudget};
use serde::Serialize;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
#[derive(Serialize)]
struct Record {
    index: usize,
    kind: String,
    elapsed_ms: f64,
    status: String,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: city_benchmark DATASET OUTPUT_DIRECTORY".into());
    }
    let path = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    std::fs::create_dir_all(output)?;
    let load = Instant::now();
    let engine = Arc::new(Engine::open(path, LoadOptions::default())?);
    let load_ms = load.elapsed().as_secs_f64() * 1000.;
    let (_, graph) = open(path, LoadOptions::default())?;
    let mut pairs = Vec::new();
    let mut seed = 20261019_u64;
    for i in 0..1000 {
        let mut choose = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let e = &graph.edges[(seed >> 32) as usize % graph.edges.len()];
            let a = graph.nodes[e.from];
            let b = graph.nodes[e.to];
            let fraction = if i % 3 == 0 {
                0.0
            } else if i % 3 == 1 {
                0.5
            } else {
                1.0
            };
            Point {
                lon: a.lon + (b.lon - a.lon) * fraction,
                lat: a.lat + (b.lat - a.lat) * fraction,
            }
        };
        pairs.push([choose(), choose()]);
    }
    std::fs::write(output.join("od.json"), serde_json::to_vec_pretty(&pairs)?)?;
    let mut od = Vec::new();
    for (i, pair) in pairs.iter().enumerate() {
        let now = Instant::now();
        let status = match engine.route(pair, QueryOptions::default(), &SearchBudget::default()) {
            Ok(r) => format!("ok:{:.3}m", r.distance_m),
            Err(e) => format!("{e:?}"),
        };
        od.push(Record {
            index: i,
            kind: "route".into(),
            elapsed_ms: now.elapsed().as_secs_f64() * 1000.,
            status,
        });
    }
    std::fs::write(
        output.join("od-results.json"),
        serde_json::to_vec_pretty(&od)?,
    )?;
    eprintln!("1000 OD queries complete; starting 30s warmup + 300s at 5 requests/s per operation");
    let mut workers = Vec::new();
    let mut senders = Vec::new();
    for kind in ["route", "nearest"] {
        let (tx, rx) = mpsc::channel::<(usize, Instant, [Point; 2])>();
        senders.push(tx);
        let engine = engine.clone();
        workers.push(std::thread::spawn(move || {
            let mut records = Vec::new();
            for (i, scheduled, pair) in rx {
                let status = if kind == "route" {
                    engine
                        .route(&pair, QueryOptions::default(), &SearchBudget::default())
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}"))
                } else {
                    engine
                        .nearest(pair[0], QueryOptions::default(), &SearchBudget::default())
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}"))
                };
                if i >= 150 {
                    records.push(Record {
                        index: i - 150,
                        kind: kind.into(),
                        elapsed_ms: scheduled.elapsed().as_secs_f64() * 1000.,
                        status: status.err().unwrap_or_else(|| "ok".into()),
                    });
                }
            }
            records
        }));
    }
    let start = Instant::now();
    for i in 0..1650 {
        let scheduled = start + Duration::from_millis(i as u64 * 200);
        if let Some(delay) = scheduled.checked_duration_since(Instant::now()) {
            std::thread::sleep(delay);
        }
        for sender in &senders {
            sender.send((i, scheduled, pairs[i % pairs.len()]))?;
        }
    }
    drop(senders);
    let mut records = Vec::new();
    for worker in workers {
        records.extend(worker.join().map_err(|_| "worker panic")?);
    }
    let mut summary = serde_json::json!({"dataset_id":engine.manifest().dataset_id,"load_ms":load_ms,"warmup_s":30,"measure_s":300,"qps_per_operation":5,"queue_included":true});
    for kind in ["route", "nearest"] {
        let mut times: Vec<_> = records
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| r.elapsed_ms)
            .collect();
        times.sort_by(f64::total_cmp);
        let failures = records
            .iter()
            .filter(|r| r.kind == kind && r.status != "ok")
            .count();
        summary[kind] = serde_json::json!({"count":times.len(),"p95_ms":times[(times.len()*95).div_ceil(100)-1],"failures":failures});
    }
    std::fs::write(
        output.join("load-results.json"),
        serde_json::to_vec_pretty(&records)?,
    )?;
    std::fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!("{summary}");
    Ok(())
}
