//! Exit identity, normal pressure and landing have one lifetime across native
//! detection, hosted drags, and delayed hold/export acknowledgements.
use super::*;
use crate::router::ExitEdge;

#[derive(Clone, Debug)]
pub(in crate::router) struct EdgeGesture {
    pub edge: ExitEdge,
    pub landing: ScreenPos,
    pub at: Point,
    probe_at: Point,
    pressure: DampingDepth,
    pub crossing: Option<std::time::Instant>,
}
impl EdgeGesture {
    #[cfg(test)]
    pub fn fixture(source: DeviceId, target: DeviceId, at: Point, normal: (f64, f64), pressure: f64) -> Self {
        let mut gesture = Self::new(
            ExitEdge::fixture(crate::ScreenId::new(source, "screen"), normal),
            ScreenPos::new(crate::ScreenId::new(target, "screen"), at),
            at,
        );
        gesture.push(pressure, 1.);
        gesture
    }

    pub fn new(edge: ExitEdge, landing: ScreenPos, at: Point) -> Self {
        Self {
            edge,
            landing,
            at,
            probe_at: at,
            pressure: DampingDepth::default(),
            crossing: None,
        }
    }
    pub fn same_edge(&self, edge: &ExitEdge, landing: &ScreenPos) -> bool {
        self.edge == *edge && self.landing.screen == landing.screen
    }
    pub fn select(&mut self, edge: ExitEdge, landing: ScreenPos, at: Point) {
        if !self.same_edge(&edge, &landing) {
            self.pressure = DampingDepth::default();
            self.crossing = None;
        }
        self.edge = edge;
        self.landing = landing;
        self.at = at;
    }
    pub fn push(&mut self, amount: f64, threshold: f64) {
        self.pressure.push(amount);
        if self.crossed(threshold) {
            self.crossing.get_or_insert_with(std::time::Instant::now);
        }
    }
    pub fn crossed(&self, threshold: f64) -> bool {
        self.pressure.crossed(threshold)
    }
    pub fn follow(&mut self, at: Point, landing: ScreenPos) {
        self.at = at;
        // Tangent movement never adds pressure or replays normal travel already
        // paid when the gesture selected this portal.
        if self.edge.normal.0 == 0. {
            self.landing.local.x = landing.local.x;
        } else {
            self.landing.local.y = landing.local.y;
        }
    }
    pub fn follow_probe(&mut self) -> bool {
        let moved = (self.probe_at.x - self.at.x).abs() > PROBE_FOLLOW_UNITS
            || (self.probe_at.y - self.at.y).abs() > PROBE_FOLLOW_UNITS;
        if moved {
            self.probe_at = self.at;
        }
        moved
    }
}
