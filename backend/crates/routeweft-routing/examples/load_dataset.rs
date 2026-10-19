use routeweft_dataset::{LoadOptions, Point};
use routeweft_routing::{Engine, QueryOptions, SearchBudget};
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: load_dataset DATASET")?;
    let engine = Engine::open(Path::new(&path), LoadOptions::default())?;
    let query = [
        Point {
            lon: 7.4206,
            lat: 43.7311,
        },
        Point {
            lon: 7.4269,
            lat: 43.7384,
        },
    ];
    let result = engine.route(&query, QueryOptions::default(), &SearchBudget::default())?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
