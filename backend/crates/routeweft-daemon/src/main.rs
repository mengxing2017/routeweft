use routeweft_dataset::{LoadOptions, Point};
use routeweft_routing::{Engine, QueryOptions, SearchBudget};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};
const HELP:&str="RouteWeft offline driving router\n\nrouteweft build --input FILE.osm.pbf --profile driving --output DIR\nrouteweft preflight --input FILE.osm.pbf --profile driving [--invalid-restrictions reject|block_ways] [--json]\nrouteweft version [--json]\nrouteweft inspect --dataset DIR [--mode full|metadata] [--json]\nrouteweft verify --dataset DIR [--json]\nrouteweft nearest --dataset DIR --coordinate 'lon,lat' [--radius-m 1000] [--json]\nrouteweft route --dataset DIR --coordinates 'lon,lat;lon,lat' [--radius-m 1000] [--json]\n\nQuery limits: --max-work N --max-states N --max-queue N --timeout-ms N\nPreflight: --mode full|structure (default full)\nBuild/preflight: --invalid-restrictions reject|block_ways (default reject)\nAll results and errors use JSON schema 1. Legacy positional routing is removed.";
#[derive(Debug)]
struct Failure {
    kind: String,
    message: String,
    code: u8,
    details: Option<Box<routeweft_preprocess::InputReport>>,
    resource: Option<Box<routeweft_dataset::ResourceLimit>>,
}
fn bad(message: impl Into<String>) -> Failure {
    Failure {
        kind: "InvalidInput".into(),
        message: message.into(),
        code: 2,
        details: None,
        resource: None,
    }
}
fn data_error(error: routeweft_dataset::Error) -> Failure {
    let kind = match &error {
        routeweft_dataset::Error::Io(_) => "Io",
        routeweft_dataset::Error::IncompatibleFormat => "IncompatibleFormat",
        routeweft_dataset::Error::LimitExceeded | routeweft_dataset::Error::ResourceLimit(_) => {
            "BudgetExceeded"
        }
        _ => "InvalidDataset",
    };
    let resource = match &error {
        routeweft_dataset::Error::ResourceLimit(r) => Some(Box::new(r.clone())),
        _ => None,
    };
    Failure {
        kind: kind.into(),
        message: error.to_string(),
        code: 4,
        details: None,
        resource,
    }
}
fn build_error(error: routeweft_preprocess::Error) -> Failure {
    let message = error.to_string();
    let mut failure = data_error(error.source);
    failure.message = message;
    failure.details = Some(error.report);
    failure
}
fn query_error(error: routeweft_routing::Error) -> Failure {
    Failure {
        kind: format!("{error:?}"),
        message: error.to_string(),
        details: None,
        resource: None,
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
        "build" | "preflight" | "version" | "inspect" | "verify" | "nearest" | "route"
    ) {
        return Err(bad("unknown command"));
    }
    let allowed: &[&str] = match command.as_str() {
        "build" => &["--input", "--profile", "--output", "--invalid-restrictions"],
        "preflight" => &["--input", "--profile", "--invalid-restrictions", "--mode"],
        "version" => &[],
        "inspect" => &["--dataset", "--mode"],
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
    if command == "version" {
        return Ok(
            json!({"schema":1,"status":"ok","version":env!("CARGO_PKG_VERSION"),
            "cli_schema":1,"dataset_formats":[1],"profiles":["driving-v1"],
            "commands":["build","preflight","inspect","verify","nearest","route","version"],
            "invalid_restriction_policies":["reject","block_ways"],
            "capabilities": {
                "builder":"in_memory", "external_memory_build":false,
                "large_pbf_3gib":false, "dataset_access":"in_memory",
                "preflight":"full_semantic_build", "preflight_cache":false,
                "preflight_modes":["full","structure"],
                "inspect_modes":["full","metadata"],
                "graph_io":"streaming_json", "build_revalidates_input":true,
                "limits": {"input_bytes":536870912,"decoded_bytes":536870912,
                    "estimated_osm_bytes":536870912,"osm_objects":2000000,
                    "blob_bytes":33554432,"graph_bytes":268435456,
                    "graph_nodes":1000000,"graph_edges":2000000,
                    "restriction_edges":1000000},
                "memory_limit_kind":"estimated_objects_not_rss"
            }}),
        );
    }
    if command == "build" || command == "preflight" {
        if required("--profile")? != "driving" {
            return Err(Failure {
                kind: "UnsupportedProfile".into(),
                message: "only driving is supported".into(),
                code: 2,
                details: None,
                resource: None,
            });
        }
        let options = routeweft_preprocess::BuildOptions {
            invalid_restrictions: match flags
                .get("--invalid-restrictions")
                .copied()
                .unwrap_or("reject")
            {
                "reject" => routeweft_preprocess::InvalidRestrictionPolicy::Reject,
                "block_ways" => routeweft_preprocess::InvalidRestrictionPolicy::BlockWays,
                _ => return Err(bad("invalid restriction policy; use reject or block_ways")),
            },
        };
        let input = Path::new(required("--input")?);
        if command == "preflight" {
            match flags.get("--mode").copied().unwrap_or("full") {
                "structure" => {
                    if flags.contains_key("--invalid-restrictions") {
                        return Err(bad("structure mode does not apply restriction policies"));
                    }
                    let report = routeweft_preprocess::pbf::scan(input).map_err(data_error)?;
                    return Ok(json!({"schema":1,"status":"ok","report":report}));
                }
                "full" => {}
                _ => return Err(bad("invalid preflight mode; use full or structure")),
            }
            let report = routeweft_preprocess::preflight(input, options).map_err(build_error)?;
            return Ok(json!({"schema":1,"status":"ok","report":report}));
        }
        let (m, report) = routeweft_preprocess::build_with_options(
            input,
            Path::new(required("--output")?),
            options,
        )
        .map_err(build_error)?;
        return Ok(json!({"schema":1,"status":"ok","manifest":m,"report":report}));
    }

    let dataset = Path::new(required("--dataset")?);
    if command == "inspect" {
        match flags.get("--mode").copied().unwrap_or("full") {
            "metadata" => {
                let manifest = routeweft_dataset::inspect_manifest(dataset, LoadOptions::default())
                    .map_err(data_error)?;
                return Ok(json!({"schema":1,"status":"ok","verified":false,
                    "verification_scope":"manifest_only","manifest":manifest}));
            }
            "full" => {}
            _ => return Err(bad("invalid inspect mode; use full or metadata")),
        }
    }
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
            let mut value =
                json!({"schema":1,"status":"error","error":{"kind":e.kind,"message":e.message}});
            if let Some(resource) = e.resource {
                value["error"]["resource"] = json!(resource);
            }
            if let Some(report) = e.details {
                value["error"]["details"] = json!(report);
            }
            println!("{value}");
            ExitCode::from(e.code)
        }
    }
}
