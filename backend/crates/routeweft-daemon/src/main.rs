use routeweft_dataset::{GraphCapacity, Point};
use routeweft_routing::acceleration::{self, CustomizationOptions};
use routeweft_routing::{Engine, QueryDiagnostics, QueryOptions, RoutingAlgorithm, SearchBudget};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};
const HELP:&str="RouteWeft offline driving router\n\nrouteweft build --input FILE.osm.pbf --profile driving --output DIR [--format 1|2] [--builder memory|external]\nrouteweft preflight --input FILE.osm.pbf --profile driving [--invalid-restrictions reject|block_ways] [--json]\nrouteweft version [--json]\nrouteweft inspect --dataset DIR [--mode full|metadata] [--json]\nrouteweft verify --dataset DIR [--json]\nrouteweft nearest --dataset DIR --coordinate 'lon,lat' [--radius-m 1000] [--json]\nrouteweft route --dataset DIR --coordinates 'lon,lat;lon,lat' [--radius-m 1000] [--json]\n\nrouteweft customize --dataset DIR [--graph-capacity regional] [--max-load-bytes N]\nrouteweft serve --dataset DIR [query limits] (JSONL stdin/stdout, resident engine)\nQuery route: --algorithm auto|dijkstra (default auto)\nQuery limits: --max-work N --max-states N --max-queue N --timeout-ms N\nPreflight: --mode full|structure (default full)\nBuild/preflight: --invalid-restrictions reject|block_ways (default reject); --invalid-maxspeed reject|use_default (default reject)\nAudit: --max-restriction-diagnostics N --max-restriction-audit-bytes N --max-blockways-attempts N (block_ways only)\nGraph capacity: --graph-capacity city|regional (default city); regional build requires external/v2; query --max-load-bytes N (estimated allocations, default 2 GiB)\nExternal builder: requires --format 2; --progress off|json (stderr JSONL); --scratch-dir DIR --max-scratch-bytes N --max-scratch-files N --build-timeout-ms N (cooperative phase checks)\nAll results and errors use JSON schema 1. Legacy positional routing is removed.";
#[derive(Debug)]
struct Failure {
    kind: String,
    message: String,
    code: u8,
    details: Option<Box<routeweft_preprocess::InputReport>>,
    resource: Option<Box<routeweft_dataset::ResourceLimit>>,
    diagnostics: Option<Box<QueryDiagnostics>>,
}
fn bad(message: impl Into<String>) -> Failure {
    Failure {
        kind: "InvalidInput".into(),
        message: message.into(),
        code: 2,
        details: None,
        resource: None,
        diagnostics: None,
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
        diagnostics: None,
    }
}
fn build_error(error: routeweft_preprocess::Error) -> Failure {
    let message = error.to_string();
    let mut failure = data_error(error.source);
    failure.message = message;
    if matches!(
        error.report.failure_code.as_deref(),
        Some("BuildCancelled" | "BuildTimeout")
    ) {
        failure.kind = if error.report.failure_code.as_deref() == Some("BuildCancelled") {
            "Cancelled"
        } else {
            "BudgetExceeded"
        }
        .into();
        failure.code = 5;
    }
    failure.details = Some(error.report);
    failure
}
fn query_error(error: routeweft_routing::Error) -> Failure {
    Failure {
        kind: format!("{error:?}"),
        message: error.to_string(),
        details: None,
        resource: None,
        diagnostics: None,
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
        "build"
            | "preflight"
            | "version"
            | "inspect"
            | "verify"
            | "nearest"
            | "route"
            | "customize"
            | "serve"
    ) {
        return Err(bad("unknown command"));
    }
    let allowed: &[&str] = match command.as_str() {
        "build" => &[
            "--input",
            "--profile",
            "--output",
            "--invalid-restrictions",
            "--invalid-maxspeed",
            "--format",
            "--builder",
            "--graph-capacity",
            "--scratch-dir",
            "--max-scratch-bytes",
            "--max-scratch-files",
            "--build-timeout-ms",
            "--progress",
            "--max-restriction-diagnostics",
            "--max-restriction-audit-bytes",
            "--max-blockways-attempts",
        ],
        "preflight" => &[
            "--input",
            "--profile",
            "--invalid-restrictions",
            "--invalid-maxspeed",
            "--mode",
            "--max-restriction-diagnostics",
            "--max-restriction-audit-bytes",
            "--max-blockways-attempts",
        ],
        "version" => &[],
        "inspect" => &["--dataset", "--mode", "--graph-capacity"],
        "customize" => &[
            "--dataset",
            "--graph-capacity",
            "--max-load-bytes",
            "--max-work",
            "--timeout-ms",
        ],
        "serve" => &[
            "--dataset",
            "--graph-capacity",
            "--max-load-bytes",
            "--algorithm",
            "--max-work",
            "--max-states",
            "--max-queue",
            "--timeout-ms",
        ],
        "route" => &[
            "--algorithm",
            "--dataset",
            "--coordinates",
            "--graph-capacity",
            "--max-load-bytes",
            "--radius-m",
            "--max-work",
            "--max-states",
            "--max-queue",
            "--timeout-ms",
        ],
        "nearest" => &[
            "--dataset",
            "--coordinate",
            "--graph-capacity",
            "--max-load-bytes",
            "--radius-m",
            "--max-work",
            "--max-states",
            "--max-queue",
            "--timeout-ms",
        ],
        _ => &["--dataset", "--graph-capacity"],
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
            "build":serde_json::from_str::<Value>(include_str!(concat!(env!("OUT_DIR"), "/build-identity.json"))).expect("build identity"),
            "cli_schema":1,"dataset_formats":[1,2],"profiles":["driving-v1"],
            "commands":["build","preflight","inspect","verify","nearest","route","customize","serve","version"],
            "invalid_restriction_policies":["reject","block_ways"],
            "invalid_maxspeed_policies":["reject","use_default"],
            "capabilities": {
                "builder":"in_memory", "build_modes":["memory","external"],
                "external_build_formats":[2], "external_memory_build":false,
                "graph_capacity_profiles": {"city":{"nodes":1000000,"edges":2000000,"bytes":268435456,"span_degrees":5},
                    "regional":{"nodes":8000000,"edges":16000000,"bytes":2147483648_u64,"span_degrees":10,"builder":"external","format":2}},
                "routing_acceleration":{"algorithm":"mld","file":"routing.mld","version":1,"optional":true,"restriction_edges":"explicit"},
                "resident_jsonl":true,
                "v2_verify":"streaming", "runtime_memory_budget_kind":"estimated_allocations_not_rss",
                "large_pbf_3gib":false, "dataset_access":"in_memory",
                "preflight":"full_semantic_build", "preflight_cache":false,
                "preflight_modes":["full","structure"],
                "inspect_modes":["full","metadata"],
                "graph_io":"streaming_json_or_binary_v2", "dataset_write_formats":[1,2], "build_revalidates_input":true,
                "limits": {"input_bytes":536870912,"decoded_bytes":536870912,
                    "estimated_osm_bytes":536870912,"osm_objects":2000000,
                    "blob_bytes":33554432,"graph_bytes":268435456,
                    "graph_nodes":1000000,"graph_edges":2000000,
                    "restriction_edges":1000000,"v2_graph_string_bytes":268435456},
                "external_decode_limits": {"input_bytes":536870912,
                    "osm_objects":routeweft_preprocess::pbf::STREAM_OBJECTS,
                    "osm_work_bytes":routeweft_preprocess::pbf::STREAM_OBJECT_BYTES,
                    "decoded_bytes":routeweft_preprocess::pbf::STREAM_DECODED_BYTES,
                    "kind":"cumulative_work_not_retained_memory"},
                "restriction_audit": {"default_records":4096,"max_records":32768,"default_bytes":16777216,
                    "max_bytes":16777216,"memory_charge_bytes":67108864,"default_blockways_attempts":64,"max_blockways_attempts":257},
                "memory_limit_kind":"estimated_objects_not_rss"
            }}),
        );
    }
    let graph_capacity = match flags.get("--graph-capacity").copied().unwrap_or("city") {
        "city" => GraphCapacity::City,
        "regional" => GraphCapacity::Regional,
        _ => return Err(bad("invalid graph capacity; use city or regional")),
    };
    let mut load_limits = graph_capacity.limits();
    if let Some(value) = flags.get("--max-load-bytes") {
        load_limits.max_resident_bytes =
            value.parse().map_err(|_| bad("invalid --max-load-bytes"))?;
    }
    if command == "build" || command == "preflight" {
        if required("--profile")? != "driving" {
            return Err(Failure {
                kind: "UnsupportedProfile".into(),
                message: "only driving is supported".into(),
                code: 2,
                details: None,
                resource: None,
                diagnostics: None,
            });
        }
        let mut options = routeweft_preprocess::BuildOptions {
            invalid_maxspeed: match flags.get("--invalid-maxspeed").copied().unwrap_or("reject") {
                "reject" => routeweft_preprocess::InvalidMaxspeedPolicy::Reject,
                "use_default" => routeweft_preprocess::InvalidMaxspeedPolicy::UseDefault,
                _ => return Err(bad("invalid maxspeed policy; use reject or use_default")),
            },
            invalid_restrictions: match flags
                .get("--invalid-restrictions")
                .copied()
                .unwrap_or("reject")
            {
                "reject" => routeweft_preprocess::InvalidRestrictionPolicy::Reject,
                "block_ways" => routeweft_preprocess::InvalidRestrictionPolicy::BlockWays,
                _ => return Err(bad("invalid restriction policy; use reject or block_ways")),
            },
            restriction_audit: Default::default(),
        };
        if let Some(value) = flags.get("--max-restriction-diagnostics") {
            options.restriction_audit.max_diagnostics = value
                .parse()
                .map_err(|_| bad("invalid --max-restriction-diagnostics"))?;
        }
        if let Some(value) = flags.get("--max-restriction-audit-bytes") {
            options.restriction_audit.max_audit_bytes = value
                .parse()
                .map_err(|_| bad("invalid --max-restriction-audit-bytes"))?;
        }
        if let Some(value) = flags.get("--max-blockways-attempts") {
            if options.invalid_restrictions
                != routeweft_preprocess::InvalidRestrictionPolicy::BlockWays
            {
                return Err(bad(
                    "--max-blockways-attempts requires --invalid-restrictions block_ways",
                ));
            }
            options.restriction_audit.max_blockways_attempts = value
                .parse()
                .map_err(|_| bad("invalid --max-blockways-attempts"))?;
        }
        options
            .restriction_audit
            .validate()
            .map_err(|_| bad("restriction audit limits out of range"))?;
        let input = Path::new(required("--input")?);
        if command == "preflight" {
            match flags.get("--mode").copied().unwrap_or("full") {
                "structure" => {
                    if flags.contains_key("--invalid-restrictions")
                        || flags.contains_key("--invalid-maxspeed")
                        || flags.contains_key("--max-restriction-diagnostics")
                        || flags.contains_key("--max-restriction-audit-bytes")
                        || flags.contains_key("--max-blockways-attempts")
                    {
                        return Err(bad("structure mode does not apply build policies"));
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
        let format = match flags.get("--format").copied().unwrap_or("1") {
            "1" => routeweft_dataset::DatasetFormat::JsonV1,
            "2" => routeweft_dataset::DatasetFormat::BinaryV2,
            _ => return Err(bad("invalid dataset format; use 1 or 2")),
        };
        let builder = flags.get("--builder").copied().unwrap_or("memory");
        if !matches!(builder, "memory" | "external") {
            return Err(bad("invalid builder; use memory or external"));
        }
        let external_flags = [
            "--scratch-dir",
            "--max-scratch-bytes",
            "--max-scratch-files",
            "--build-timeout-ms",
            "--progress",
        ];
        if builder == "memory" && external_flags.iter().any(|key| flags.contains_key(key)) {
            return Err(bad("external build options require --builder external"));
        }
        if graph_capacity == GraphCapacity::Regional && builder != "external" {
            return Err(bad(
                "regional capacity requires --builder external --format 2",
            ));
        }
        if builder == "external" {
            if format != routeweft_dataset::DatasetFormat::BinaryV2 {
                return Err(bad("external builder requires --format 2"));
            }
            let number = |key: &str, default: u64| -> Result<u64, Failure> {
                flags
                    .get(key)
                    .map(|v| v.parse().map_err(|_| bad(format!("invalid {key}"))))
                    .unwrap_or(Ok(default))
            };
            let output = Path::new(required("--output")?);
            let scratch = flags
                .get("--scratch-dir")
                .map(Path::new)
                .unwrap_or_else(|| {
                    output
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new("."))
                });
            let mut external = routeweft_preprocess::staging::ExternalBuildOptions {
                build: options,
                ..Default::default()
            };
            match flags.get("--progress").copied().unwrap_or("off") {
                "off" => {}
                "json" => {
                    external.stage.progress = Some(|event| {
                        use std::io::Write;
                        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
                        let elapsed_ms =
                            START.get_or_init(Instant::now).elapsed().as_millis() as u64;
                        let mut stderr = std::io::stderr().lock();
                        let _ = writeln!(
                            stderr,
                            "{}",
                            json!({"schema":1, "event":"build_progress",
                        "stage":event.stage, "attempt":event.attempt, "completed":event.completed,
                                "diagnostics_observed":event.diagnostics_observed,
                                "scratch_peak_bytes":event.scratch_peak_bytes, "scratch_scope":event.scratch_scope,
                                "cancellation_checkpoint":event.cancellation_checkpoint, "elapsed_ms":elapsed_ms})
                        );
                    })
                }
                _ => return Err(bad("invalid progress; use off or json")),
            }
            external.stage.graph_capacity = graph_capacity;
            external.writer.graph_capacity = graph_capacity;
            external.writer.output_bytes = graph_capacity.limits().max_bytes + 1024 * 1024;
            external.stage.scratch_bytes =
                number("--max-scratch-bytes", external.stage.scratch_bytes)?;
            external.stage.run_files = usize::try_from(number(
                "--max-scratch-files",
                external.stage.run_files as u64,
            )?)
            .map_err(|_| bad("scratch file limit"))?;
            let deadline = if flags.contains_key("--build-timeout-ms") {
                Some(
                    Instant::now()
                        .checked_add(Duration::from_millis(number("--build-timeout-ms", 0)?))
                        .ok_or_else(|| bad("build timeout"))?,
                )
            } else {
                None
            };
            let built = routeweft_preprocess::staging::build_external(
                input,
                output,
                scratch,
                external,
                routeweft_preprocess::staging::ExternalBuildControl {
                    deadline,
                    ..Default::default()
                },
            )
            .map_err(build_error)?;
            return Ok(
                json!({"schema":1,"status":"ok","manifest":built.manifest,"report":built.report,
                "build":{"builder":"external","resources":built.resources}}),
            );
        }
        let (m, report) = routeweft_preprocess::build_with_format(
            input,
            Path::new(required("--output")?),
            options,
            format,
        )
        .map_err(build_error)?;
        return Ok(json!({"schema":1,"status":"ok","manifest":m,"report":report}));
    }

    let dataset = Path::new(required("--dataset")?);
    if command == "inspect" {
        match flags.get("--mode").copied().unwrap_or("full") {
            "metadata" => {
                let manifest = routeweft_dataset::inspect_manifest(dataset, load_limits)
                    .map_err(data_error)?;
                return Ok(json!({"schema":1,"status":"ok","verified":false,
                    "verification_scope":"manifest_only","manifest":manifest}));
            }
            "full" => {}
            _ => return Err(bad("invalid inspect mode; use full or metadata")),
        }
    }
    if command == "inspect" || command == "verify" {
        let metadata =
            routeweft_dataset::inspect_manifest(dataset, load_limits).map_err(data_error)?;
        let m = if metadata.format == 2 {
            routeweft_dataset::validate_binary_streaming(dataset, load_limits, 64 * 1024 * 1024)
                .map_err(data_error)?
        } else {
            routeweft_dataset::open(dataset, load_limits)
                .map_err(data_error)?
                .0
        };
        return Ok(json!({"schema":1,"status":"ok","verified":true,"manifest":m}));
    }
    let number = |key: &str, default: u64| -> Result<u64, Failure> {
        flags
            .get(key)
            .map(|v| v.parse().map_err(|_| bad(format!("invalid {key}"))))
            .unwrap_or(Ok(default))
    };
    if command == "customize" {
        let timeout = number("--timeout-ms", 1_800_000)?;
        let options = CustomizationOptions {
            max_work: usize::try_from(number("--max-work", 2_000_000_000)?)
                .map_err(|_| bad("work limit"))?,
            deadline: Some(
                Instant::now()
                    .checked_add(Duration::from_millis(timeout))
                    .ok_or_else(|| bad("timeout"))?,
            ),
            ..Default::default()
        };
        let start = Instant::now();
        let info = acceleration::customize(dataset, load_limits, options).map_err(query_error)?;
        return Ok(
            json!({"schema":1,"status":"ok","acceleration":info,"elapsed_ms":start.elapsed().as_secs_f64()*1000.}),
        );
    }
    let load = Instant::now();
    let metadata = routeweft_dataset::inspect_manifest(dataset, load_limits).map_err(data_error)?;
    routeweft_dataset::check_resident_budget(&metadata, load_limits, true).map_err(data_error)?;
    let engine =
        Engine::open_with_capacity(dataset, load_limits, graph_capacity).map_err(query_error)?;
    let load_ms = load.elapsed().as_secs_f64() * 1000.;
    let algorithm = match flags.get("--algorithm").copied().unwrap_or("auto") {
        "auto" => RoutingAlgorithm::Auto,
        "dijkstra" => RoutingAlgorithm::Dijkstra,
        _ => return Err(bad("algorithm must be auto or dijkstra")),
    };
    let budget = SearchBudget {
        max_work: usize::try_from(number("--max-work", 2_000_000)?)
            .map_err(|_| bad("work limit"))?,
        max_states: usize::try_from(number("--max-states", 250_000)?)
            .map_err(|_| bad("state limit"))?,
        max_queue: usize::try_from(number("--max-queue", 500_000)?)
            .map_err(|_| bad("queue limit"))?,
        deadline: None,
        ..Default::default()
    };
    let timeout = number("--timeout-ms", 10_000)?;
    if command == "serve" {
        serve(&engine, load_ms, budget, timeout, algorithm)?;
        return Ok(Value::Null);
    }
    let budget = SearchBudget {
        deadline: Some(
            Instant::now()
                .checked_add(Duration::from_millis(timeout))
                .ok_or_else(|| bad("timeout"))?,
        ),
        ..budget
    };
    let options = QueryOptions {
        radius_m: flags
            .get("--radius-m")
            .map(|v| v.parse().map_err(|_| bad("radius")))
            .unwrap_or(Ok(1000.))?,
        ..Default::default()
    };
    let (result, diagnostics) = if command == "nearest" {
        let (result, diagnostics) =
            engine.nearest_detailed(point(required("--coordinate")?)?, options, &budget);
        (
            result.map(|found| {
                json!({"schema":1,"status":"ok","dataset_id":engine.manifest().dataset_id,
            "profile":engine.manifest().profile,"format":engine.manifest().format,"matches":found})
            }),
            diagnostics,
        )
    } else {
        let points = required("--coordinates")?
            .split(';')
            .map(point)
            .collect::<Result<Vec<_>, _>>()?;
        let outcome = engine.route_detailed(&points, options, &budget, algorithm);
        (
            outcome
                .result
                .map(|route| json!({"schema":1,"status":"ok","route":route})),
            outcome.diagnostics,
        )
    };
    match result {
        Ok(mut value) => {
            value["diagnostics"] = json!(diagnostics);
            value["load_ms"] = json!(load_ms);
            value["load_diagnostics"] = json!(engine.load_diagnostics());
            value["acceleration"] = json!(engine.acceleration_info());
            Ok(value)
        }
        Err(error) => {
            let mut failure = query_error(error);
            failure.diagnostics = Some(Box::new(diagnostics));
            Err(failure)
        }
    }
}

/// One immutable engine per process. Requests are serial and bounded; callers can
/// use a process pool instead of spawning a process (and reloading) per request.
fn serve(
    engine: &Engine,
    load_ms: f64,
    defaults: SearchBudget<'_>,
    timeout_ms: u64,
    algorithm: RoutingAlgorithm,
) -> Result<(), Failure> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stdout().lock();
    writeln!(output, "{}", json!({"schema":1,"status":"ready","dataset_id":engine.manifest().dataset_id,
        "load_ms":load_ms,"load_diagnostics":engine.load_diagnostics(),"acceleration":engine.acceleration_info()})).map_err(|_| query_error(routeweft_routing::Error::Io))?;
    output
        .flush()
        .map_err(|_| query_error(routeweft_routing::Error::Io))?;
    loop {
        // Bound each request before JSON decoding, including a missing newline.
        let mut bytes = Vec::new();
        let count = (&mut input)
            .take(65_537)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| query_error(routeweft_routing::Error::Io))?;
        if count == 0 {
            break;
        }
        if count > 65_536 {
            return Err(bad("JSONL request exceeds 65536 bytes"));
        }
        let request = serde_json::from_slice::<Value>(&bytes);
        let id = request
            .as_ref()
            .ok()
            .and_then(|r| r.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        let mut response = (|| -> Result<Value, Failure> {
            let request = request.map_err(|_| bad("invalid JSON request"))?;
            if !request.is_object() || request.get("limits").is_some_and(|v| !v.is_object()) {
                return Err(bad("request and limits must be JSON objects"));
            }
            let number = |key: &str, default: u64| -> Result<u64, Failure> {
                request
                    .get("limits")
                    .and_then(|l| l.get(key))
                    .map(|v| v.as_u64().ok_or_else(|| bad(format!("invalid {key}"))))
                    .unwrap_or(Ok(default))
            };
            let budget = SearchBudget {
                max_work: usize::try_from(
                    number("max_work", defaults.max_work as u64)?.min(defaults.max_work as u64),
                )
                .map_err(|_| bad("work limit"))?,
                max_states: usize::try_from(
                    number("max_states", defaults.max_states as u64)?
                        .min(defaults.max_states as u64),
                )
                .map_err(|_| bad("state limit"))?,
                max_queue: usize::try_from(
                    number("max_queue", defaults.max_queue as u64)?.min(defaults.max_queue as u64),
                )
                .map_err(|_| bad("queue limit"))?,
                deadline: Some(
                    Instant::now()
                        .checked_add(Duration::from_millis(
                            number("timeout_ms", timeout_ms)?.min(timeout_ms),
                        ))
                        .ok_or_else(|| bad("timeout"))?,
                ),
                ..Default::default()
            };
            let points: Vec<Point> = serde_json::from_value(
                request
                    .get("coordinates")
                    .cloned()
                    .ok_or_else(|| bad("missing coordinates"))?,
            )
            .map_err(|_| bad("coordinates must be [{lon,lat}, ...]"))?;
            let radius_m = request
                .get("radius_m")
                .map(|v| v.as_f64().ok_or_else(|| bad("invalid radius_m")))
                .unwrap_or(Ok(1000.))?;
            let options = QueryOptions {
                radius_m,
                ..Default::default()
            };
            let (result, diagnostics) = match request
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or("route")
            {
                "route" => {
                    let outcome = engine.route_detailed(&points, options, &budget, algorithm);
                    (
                        outcome.result.map(|route| json!({"route":route})),
                        outcome.diagnostics,
                    )
                }
                "nearest" if points.len() == 1 => {
                    let (result, diagnostics) =
                        engine.nearest_detailed(points[0], options, &budget);
                    (
                        result.map(|matches| json!({"matches":matches})),
                        diagnostics,
                    )
                }
                _ => {
                    return Err(bad(
                        "operation must be route, or nearest with one coordinate",
                    ))
                }
            };
            let mut response = match result {
                Ok(value) => value,
                Err(error) => {
                    json!({"error":{"kind":format!("{error:?}"),"message":error.to_string()}})
                }
            };
            response["schema"] = json!(1);
            response["status"] = json!(if response.get("error").is_some() {
                "error"
            } else {
                "ok"
            });
            response["diagnostics"] = json!(diagnostics);
            Ok(response)
        })()
        .unwrap_or_else(failure_json);
        response["id"] = id;
        writeln!(output, "{response}").map_err(|_| query_error(routeweft_routing::Error::Io))?;
        output
            .flush()
            .map_err(|_| query_error(routeweft_routing::Error::Io))?;
    }
    Ok(())
}
fn failure_json(e: Failure) -> Value {
    let mut value =
        json!({"schema":1,"status":"error","error":{"kind":e.kind,"message":e.message}});
    if let Some(resource) = e.resource {
        value["error"]["resource"] = json!(resource);
    }
    if let Some(report) = e.details {
        value["error"]["details"] = json!(report);
    }
    if let Some(diagnostics) = e.diagnostics {
        value["diagnostics"] = json!(diagnostics);
    }
    value
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] || args == ["-V"] {
        let build: Value = serde_json::from_str(include_str!(concat!(
            env!("OUT_DIR"),
            "/build-identity.json"
        )))
        .expect("build identity");
        println!(
            "routeweft {} (commit={} dirty={} source={})",
            env!("CARGO_PKG_VERSION"),
            build["git_commit"].as_str().unwrap_or("unknown"),
            build["source_dirty"],
            build["source_identity_kind"].as_str().unwrap()
        );
        return ExitCode::SUCCESS;
    }
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match execute(args) {
        Ok(value) => {
            if !value.is_null() {
                println!("{value}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let code = e.code;
            println!("{}", failure_json(e));
            ExitCode::from(code)
        }
    }
}
