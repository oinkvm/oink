//! The UI and Rust suites independently check this same exhaustive projection
//! corpus. Neither test invokes the other language's build tool or artifact lock.
//! Inputs retain the UI's seeded generated cases. The layout_conformance example
//! can regenerate projections from those inputs when the geometry changes.
use oink_display::layout::facing;
use oink_foundation::Rect;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    rects: Vec<Rect>,
    projections: Vec<Projection>,
}
#[derive(Deserialize)]
struct Projection {
    kind: String,
    screen: usize,
    side: usize,
    from: f64,
    to: f64,
    target: Option<usize>,
}

#[test]
fn generated_pairs_and_visible_intervals_match_shared_corpus() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("facing-conformance.json")).unwrap();
    assert_eq!(cases.len(), 201);
    for (index, case) in cases.into_iter().enumerate() {
        let eps = facing::tolerance(&case.rects);
        let actual = facing::edge_projections(&case.rects, eps);
        assert_eq!(actual.len(), case.projections.len(), "case {index}");
        for (actual, expected) in actual.iter().zip(case.projections) {
            assert_eq!(actual.kind, expected.kind, "case {index}");
            assert_eq!(actual.screen, expected.screen, "case {index}");
            assert_eq!(actual.side, expected.side, "case {index}");
            assert_eq!(actual.target, expected.target, "case {index}");
            assert!((actual.from - expected.from).abs() <= eps, "case {index}: from");
            assert!((actual.to - expected.to).abs() <= eps, "case {index}: to");
        }
    }
}
