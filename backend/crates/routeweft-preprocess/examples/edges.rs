//! Development entry point for streaming direction edges and their way index.
use routeweft_preprocess::staging::{emit_edges, StageOptions};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: edges STAGE_DIRECTORY JOIN_DIRECTORY NEW_OUTPUT_DIRECTORY".into());
    }
    let report = emit_edges(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
        StageOptions::default(),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
