//! Development entry point for the intermediate staging API, not a dataset build.
use routeweft_preprocess::staging::{stage, StageOptions};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: stage INPUT.osm.pbf NEW_OUTPUT_DIRECTORY".into());
    }
    let report = stage(
        Path::new(&args[1]),
        Path::new(&args[2]),
        StageOptions::default(),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
