//! Development entry point for the external node/reference join.
use routeweft_preprocess::staging::{connect, StageOptions};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: connect STAGE_DIRECTORY NEW_OUTPUT_DIRECTORY".into());
    }
    let report = connect(
        Path::new(&args[1]),
        Path::new(&args[2]),
        StageOptions::default(),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
