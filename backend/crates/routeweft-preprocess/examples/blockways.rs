//! Development-only staged BlockWays rebuild; output is not an Engine dataset.
use routeweft_preprocess::staging::{prepare_block_ways, BlockWaysOptions};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: blockways STAGE_DIRECTORY NEW_OUTPUT_DIRECTORY".into());
    }
    match prepare_block_ways(
        Path::new(&args[1]),
        Path::new(&args[2]),
        BlockWaysOptions::default(),
    ) {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report)?),
        Err(error) => {
            eprintln!("{}", serde_json::to_string_pretty(&error.report)?);
            return Err(error.into());
        }
    }
    Ok(())
}
