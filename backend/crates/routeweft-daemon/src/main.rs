use routeweft_dataset::{LoadOptions, Point};
use routeweft_routing::{Engine, QueryOptions, SearchBudget};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};
const HELP:&str="RouteWeft offline driving router\n\nrouteweft build --input FILE.osm.pbf --profile driving --output DIR\nrouteweft inspect --dataset DIR [--json]\nrouteweft verify --dataset DIR [--json]\nrouteweft nearest --dataset DIR --coordinate 'lon,lat' [--radius-m 1000] [--json]\nrouteweft route --dataset DIR --coordinates 'lon,lat;lon,lat' [--radius-m 1000] [--json]\n\nQuery limits: --max-work N --max-states N --max-queue N --timeout-ms N\nAll results and errors use JSON schema 1. Legacy positional routing is removed.";
#[derive(Debug)]
struct Failure {
    kind: String,
    message: String,
    code: u8,
}
fn bad(message: impl Into<String>) -> Failure {
    Failure {
        kind: "InvalidInput".into(),
        message: message.into(),
        code: 2,
    }
}
fn data_error(error: routeweft_dataset::Error) -> Failure {
    let kind = match &error {
        routeweft_dataset::Error::Io(_) => "Io",
        routeweft_dataset::Error::IncompatibleFormat => "IncompatibleFormat",
        routeweft_dataset::Error::LimitExceeded => "BudgetExceeded",
        _ => "InvalidDataset",
    };
    Failure {
        kind: kind.into(),
        message: error.to_string(),
        code: 4,
    }
}
fn query_error(error: routeweft_routing::Error) -> Failure {
    Failure {
        kind: format!("{error:?}"),
        message: error.to_string(),
        code: match error {
            routeweft_routing::Error::InvalidInput
            | routeweft_routing::Error::UnsupportedProfile => 2,
            routeweft_routing::Error::NoRoute | routeweft_routing::Error::NoSegment => 3,
            routeweft_routing::Error::BudgetExceeded | routeweft_routing::Error::Cancelled => 5,
            _ => 4,
        },
    }
}
fn point(input: &str) -> Result<Point, Failure> {
    let (lon, lat) = input
        .split_once(',')
        .ok_or_else(|| bad("expected lon,lat"))?;
    let p = Point {
        lon: lon.trim().parse().map_err(|_| bad("longitude"))?,
        lat: lat.trim().parse().map_err(|_| bad("latitude"))?,
    };
    p.coordinate().map_err(|_| bad("coordinate range"))?;
    Ok(p)
}
fn execute(args: Vec<String>) -> Result<Value, Failure> {
    let command = &args[0];
    if !matches!(
        command.as_str(),
        "build" | "inspect" | "verify" | "nearest" | "route"
    ) {
        return Err(bad("unknown command"));
    }
    let allowed: &[&str] = match command.as_str() {
        "build" => &["--input", "--profile", "--output"],
        "route" => &[
            "--dataset",
            "--coordinates",
            "--radius-m",
            "--max-work",
            "--max-states",
            "--max-queue",
            "--timeout-ms",
        ],
        "nearest" => &[
            "--dataset",
            "--coordinate",
            "--radius-m",
            "--max-work",
            "--max-states",
            "--max-queue",
            "--timeout-ms",
        ],
        _ => &["--dataset"],
    };
    let mut flags = BTreeMap::new();
    let mut i = 1;
    let mut json_seen = false;
    while i < args.len() {
        let key = &args[i];
        if key == "--json" {
            if json_seen {
                return Err(bad("duplicate --json"));
            }
            json_seen = true;
            i += 1;
            continue;
        }
        if !allowed.contains(&key.as_str()) {
            return Err(bad(format!("unknown option {key}")));
        }
        let value = args
            .get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| bad(format!("missing value {key}")))?;
        if flags.insert(key.as_str(), value.as_str()).is_some() {
            return Err(bad("duplicate option"));
        }
        i += 2;
    }
    let required = |key: &str| {
        flags
            .get(key)
            .copied()
            .ok_or_else(|| bad(format!("missing {key}")))
    };
    if command == "build" {
        if required("--profile")? != "driving" {
            return Err(Failure {
                kind: "UnsupportedProfile".into(),
                message: "only driving is supported".into(),
                code: 2,
            });
        }
        let m = routeweft_preprocess::build(
            Path::new(required("--input")?),
            Path::new(required("--output")?),
        )
        .map_err(data_error)?;
        return Ok(json!({"schema":1,"status":"ok","manifest":m}));
    }
    let dataset = Path::new(required("--dataset")?);
    if command == "inspect" || command == "verify" {
        let (m, _) =
            routeweft_dataset::open(dataset, LoadOptions::default()).map_err(data_error)?;
        return Ok(json!({"schema":1,"status":"ok","verified":true,"manifest":m}));
    }
    let engine = Engine::open(dataset, LoadOptions::default()).map_err(query_error)?;
    let number = |key: &str, default: u64| -> Result<u64, Failure> {
        flags
            .get(key)
            .map(|v| v.parse().map_err(|_| bad(format!("invalid {key}"))))
            .unwrap_or(Ok(default))
    };
    let budget = SearchBudget {
        max_work: usize::try_from(number("--max-work", 2_000_000)?)
            .map_err(|_| bad("work limit"))?,
        max_states: usize::try_from(number("--max-states", 250_000)?)
            .map_err(|_| bad("state limit"))?,
        max_queue: usize::try_from(number("--max-queue", 500_000)?)
            .map_err(|_| bad("queue limit"))?,
        deadline: Some(
            Instant::now()
                .checked_add(Duration::from_millis(number("--timeout-ms", 10_000)?))
                .ok_or_else(|| bad("timeout"))?,
        ),
        ..Default::default()
    };
    let options = QueryOptions {
        radius_m: flags
            .get("--radius-m")
            .map(|v| v.parse().map_err(|_| bad("radius")))
            .unwrap_or(Ok(1000.))?,
        ..Default::default()
    };
    if command == "nearest" {
        let found = engine
            .nearest(point(required("--coordinate")?)?, options, &budget)
            .map_err(query_error)?;
        Ok(
            json!({"schema":1,"status":"ok","dataset_id":engine.manifest().dataset_id,"profile":engine.manifest().profile,"format":engine.manifest().format,"matches":found}),
        )
    } else {
        let points = required("--coordinates")?
            .split(';')
            .map(point)
            .collect::<Result<Vec<_>, _>>()?;
        let route = engine
            .route(&points, options, &budget)
            .map_err(query_error)?;
        Ok(json!({"schema":1,"status":"ok","route":route}))
    }
}
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match execute(args) {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!(
                "{}",
                json!({"schema":1,"status":"error","error":{"kind":e.kind,"message":e.message}})
            );
            ExitCode::from(e.code)
        }
    }
}
