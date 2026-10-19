use routeweft_domain::model::{Coordinate, TravelProfile};
use routeweft_routing::{Edge, NodeEngine as Engine, SearchBudget};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nodes = vec![Coordinate::new(0.0, 0.0)?, Coordinate::new(0.01, 0.0)?];
    let engine = Engine::new(
        nodes,
        vec![Edge {
            from: 0,
            to: 1,
            duration_ms: 1000,
            distance_mm: 10000,
        }],
        vec![],
    )?;
    let path = engine.route(0, 1, TravelProfile::Driving, &SearchBudget::default())?;
    println!(
        "edges={:?} duration_ms={} distance_mm={}",
        path.edges, path.duration_ms, path.distance_mm
    );
    Ok(())
}
