//! Development-only intermediate adjacency indexing and complete access validation.
use routeweft_preprocess::staging::{index_adjacency, AccessBudget, DiskEdgeAccess, StageOptions};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: adjacency EDGE_DIRECTORY NEW_OUTPUT_DIRECTORY".into());
    }
    let report = index_adjacency(
        Path::new(&args[1]),
        Path::new(&args[2]),
        StageOptions::default(),
    )?;
    let mut access = DiskEdgeAccess::open(Path::new(&args[1]), Path::new(&args[2]))?;
    let mut budget = AccessBudget::new(u64::MAX);
    for node in 0..report.nodes {
        access.outgoing(node, &mut budget, |_| Ok(()))?;
    }
    for edge in 0..report.edges {
        access.edge(edge, &mut budget)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
