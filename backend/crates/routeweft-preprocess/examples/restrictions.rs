//! Development-only disk restriction resolution. Reject policy, not a dataset builder.
use routeweft_preprocess::staging::{resolve_restrictions, RestrictionOptions};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 {
        return Err("usage: restrictions STAGE JOIN EDGES ADJACENCY NEW_OUTPUT".into());
    }
    match resolve_restrictions(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
        Path::new(&args[4]),
        Path::new(&args[5]),
        RestrictionOptions::default(),
    ) {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report)?),
        Err(error) => {
            eprintln!("{}", serde_json::to_string_pretty(&error.report)?);
            return Err(error.into());
        }
    }
    Ok(())
}
