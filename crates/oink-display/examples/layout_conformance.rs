//! Executable oracle for the frontend's generated geometry conformance tests.
//! Test-only JSON stdin/stdout; this is not an application wire or persisted format.
use oink_display::layout::{Layout, ScreenId, ScreenPlacement, SizeSource, facing};
use oink_foundation::{DeviceId, Rect};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    rects: Vec<Rect>,
    groups: Vec<u8>,
}
fn main() {
    let cases: Vec<Case> = serde_json::from_reader(std::io::stdin()).unwrap();
    let results: Vec<_> = cases
        .into_iter()
        .map(|case| {
            let eps = facing::tolerance(&case.rects);
            let before = facing::edge_projections(&case.rects, eps);
            let mut layout = Layout {
                screens: case
                    .rects
                    .iter()
                    .enumerate()
                    .map(|(i, &rect)| ScreenPlacement {
                        id: ScreenId::new(DeviceId([case.groups[i]; 32]), i.to_string()),
                        name: "Synthetic".into(),
                        rect,
                        local: rect,
                        size_source: SizeSource::Manual,
                        primary: false,
                        builtin: false,
                    })
                    .collect(),
                ..Default::default()
            };
            layout.compact();
            let rects: Vec<_> = layout.screens.iter().map(|s| s.rect).collect();
            let after = facing::edge_projections(&rects, eps);
            assert!(facing::same_projections(&before, &after, eps));
            serde_json::json!({ "before": before, "after": after, "rects": rects })
        })
        .collect();
    serde_json::to_writer(std::io::stdout(), &results).unwrap();
}
