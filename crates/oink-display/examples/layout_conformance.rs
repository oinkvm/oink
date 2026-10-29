//! Executable oracle for regenerating the shared facing-geometry test corpus.
//! Test-only JSON stdin/stdout; this is not an application wire or persisted format.
use oink_display::layout::facing;
use oink_foundation::Rect;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    rects: Vec<Rect>,
}
fn main() {
    let cases: Vec<Case> = serde_json::from_reader(std::io::stdin()).unwrap();
    let results: Vec<_> = cases
        .into_iter()
        .map(|case| {
            let eps = facing::tolerance(&case.rects);
            serde_json::json!({ "projections": facing::edge_projections(&case.rects, eps) })
        })
        .collect();
    serde_json::to_writer(std::io::stdout(), &results).unwrap();
}
