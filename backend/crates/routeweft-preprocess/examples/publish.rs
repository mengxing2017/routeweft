//! Development-only final v2 publication from completed BlockWays artifacts.
use routeweft_preprocess::staging::publish_block_ways;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: publish BLOCKWAYS_DIRECTORY NEW_DATASET_DIRECTORY".into());
    }
    match publish_block_ways(
        std::path::Path::new(&args[0]),
        std::path::Path::new(&args[1]),
        Default::default(),
    ) {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report)?),
        Err(error) => {
            eprintln!("{}", serde_json::to_string_pretty(&error.report)?);
            return Err(error.into());
        }
    }
    Ok(())
}
