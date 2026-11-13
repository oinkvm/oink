//! Closed numeric copy/read boundaries. Device identity is split into exact u32
//! words because the diagnostic recorder stores numbers as f64. No names, paths,
//! application identifiers, arbitrary errors or clipboard bytes enter this API.
use crate::model::{OfferId, RepresentationId, StandardFormat, Unavailable};
use std::{
    io,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Stage {
    Observed = 1,
    CaptureBegin = 2,
    CaptureEnd = 3,
    OfferReady = 4,
    OfferSent = 5,
    OfferReceived = 6,
    DeclareBegin = 7,
    DeclareEnd = 8,
    NativeRead = 9,
    SourceRead = 10,
    RemoteRead = 11,
    FilePreparation = 12,
    FileRead = 13,
    NativeDeclared = 14,
    CaptureDiscarded = 15,
    ClipboardKeyBegin = 16,
    ClipboardKeyEnd = 17,
    CapturedSent = 18,
    CapturedReceived = 19,
}
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum Channel {
    Native = 0,
    Reliable = 1,
    Low = 2,
    Cache = 3,
    FileProvider = 4,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Values {
    pub count: u64,
    pub request: u64,
    pub representation: u64,
    pub node: u64,
    pub format: u8,
    pub channel: u8,
    pub elapsed_us: u64,
    pub bytes: u64,
    pub result: u8,
    pub reason: u16,
}
pub fn event(id: Option<OfferId>, stage: Stage, values: Values) {
    let origin = id.map_or([0; 32], |id| id.clip.origin.0);
    let word = |n| u32::from_le_bytes(origin[n..n + 4].try_into().unwrap());
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    tracing::info!(target: "oink_copy_diagnostics",
        copy_origin_0 = word(0), copy_origin_1 = word(4), copy_origin_2 = word(8), copy_origin_3 = word(12),
        copy_origin_4 = word(16), copy_origin_5 = word(20), copy_origin_6 = word(24), copy_origin_7 = word(28),
        copy_offer = id.map_or(0, |id| id.clip.seq as u32), copy_offer_hi = id.map_or(0, |id| (id.clip.seq >> 32) as u32),
        copy_generation = id.map_or(0, |id| id.generation as u32), copy_generation_hi = id.map_or(0, |id| (id.generation >> 32) as u32),
        copy_stage = stage as u8, copy_count = values.count, copy_request = values.request,
        copy_representation = values.representation, copy_node = values.node, copy_format = values.format, copy_channel = values.channel,
        copy_elapsed_us = values.elapsed_us, copy_bytes = values.bytes, copy_result = values.result,
        copy_reason = values.reason, copy_mono_us = EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64,
        "clipboard boundary");
}
/// Worker/actor handoff boundaries contain only the native generation, outcome
/// and duration. The key itself and backend errors must never enter diagnostics.
/// A separate budget keeps pathological handoffs from suppressing stage 15.
pub fn capture_handoff(stage: Stage, count: u64, result: u8, elapsed_us: u64) {
    static WINDOW: OnceLock<std::sync::Mutex<(Instant, u32)>> = OnceLock::new();
    let mut window = WINDOW
        .get_or_init(|| std::sync::Mutex::new((Instant::now(), 0)))
        .lock()
        .unwrap();
    if window.0.elapsed().as_secs() >= 60 {
        *window = (Instant::now(), 0);
    }
    if window.1 > 4096 {
        return;
    }
    let truncated = window.1 == 4096;
    window.1 += 1;
    drop(window);
    tracing::info!(target: "oink_copy_diagnostics",
        copy_stage = stage as u8, copy_count = count,
        copy_result = result, copy_elapsed_us = elapsed_us,
        copy_reason = if truncated { u16::MAX } else { 0_u16 },
        "clipboard capture handoff");
}
/// Reasons are scoped to CaptureDiscarded; they never contain native strings.
#[derive(Clone, Copy, Debug)]
#[repr(u16)]
pub enum CaptureDiscard {
    Inactive = 1,
    CaptureAgain = 2,
    BeforeChanged = 3,
    Busy = 4,
    ContentUnavailable = 5,
    BackendFailure = 6,
    SnapshotChanged = 7,
    MissingProvider = 8,
    InvalidInventory = 9,
    OrderingFailure = 10,
    OwnWrite = 11,
    EmptyInventory = 12,
    PersistFailure = 13,
    InvalidOfferId = 14,
    RegistrationFailure = 15,
    /// A newer group offer was accepted while this capture ran (no local
    /// change was pending): the captured clipboard is older than that offer.
    Superseded = 16,
}
/// At most 1024 discarded captures plus one truncation record per minute.
/// A pathological Busy backend cannot flood ordinary logs or the recorder.
pub fn capture_discard(reason: CaptureDiscard, count: u64, current_count: u64, written_count: u64) {
    static WINDOW: OnceLock<std::sync::Mutex<(Instant, u32)>> = OnceLock::new();
    let mut window = WINDOW
        .get_or_init(|| std::sync::Mutex::new((Instant::now(), 0)))
        .lock()
        .unwrap();
    if window.0.elapsed().as_secs() >= 60 {
        *window = (Instant::now(), 0);
    }
    if window.1 > 1024 {
        return;
    }
    let truncated = window.1 == 1024;
    window.1 += 1;
    drop(window);
    // Keep this event compact: the recorder admits at most 24 scalars per row.
    // There is no offer identity yet; count joins it to CaptureBegin/End.
    tracing::info!(target: "oink_copy_diagnostics",
        copy_stage = Stage::CaptureDiscarded as u8,
        copy_count = count, copy_current_count = current_count, copy_written_count = written_count,
        copy_result = 2_u8, copy_reason = if truncated { u16::MAX } else { reason as u16 },
        "clipboard capture discarded");
}
/// 0..=9 are Unavailable discriminants; 256+ is a FileProviderReason.
pub fn backend_reason(error: &crate::ClipboardError) -> u16 {
    match error {
        crate::ClipboardError::Busy => Unavailable::Superseded as u16,
        crate::ClipboardError::Unsupported => Unavailable::Unsupported as u16,
        crate::ClipboardError::ContentUnavailable(reason) => *reason as u16,
        crate::ClipboardError::FileProvider(reason) => 256 + *reason as u16,
        _ => Unavailable::ReadFailed as u16,
    }
}
pub fn format(format: Option<StandardFormat>) -> u8 {
    match format {
        None => 0,
        Some(StandardFormat::Text) => 1,
        Some(StandardFormat::Html) => 2,
        Some(StandardFormat::Rtf) => 3,
        Some(StandardFormat::Png) => 4,
        Some(StandardFormat::Privacy) => 5,
        Some(StandardFormat::Url) => 6,
    }
}
#[derive(Debug)]
pub struct Read {
    id: OfferId,
    stage: Stage,
    values: Values,
    start: Instant,
}
impl Read {
    pub fn begin(id: OfferId, stage: Stage, representation: RepresentationId, format: u8, channel: Channel) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let values = Values {
            request: NEXT.fetch_add(1, Ordering::Relaxed),
            representation: representation.0.into(),
            format,
            channel: channel as u8,
            ..Values::default()
        };
        event(Some(id), stage, values);
        Self {
            id,
            stage,
            values,
            start: Instant::now(),
        }
    }
    pub fn file(id: OfferId, node: crate::model::NodeId, channel: Channel) -> Self {
        let mut read = Self::begin(id, Stage::FileRead, RepresentationId(0), 8, channel);
        read.values.node = node.0;
        read
    }
    pub fn success(&mut self, bytes: u64) {
        if self.values.result != 2 {
            self.values.result = 1;
        }
        self.values.bytes = bytes;
    }
    pub fn backend_failure(&mut self, error: &crate::ClipboardError) {
        self.values.result = 2;
        self.values.reason = backend_reason(error);
    }
    pub fn failure(&mut self, error: &io::Error) {
        self.values.result = 2;
        self.values.reason = crate::content::reason(error) as u16;
    }
}
impl Drop for Read {
    fn drop(&mut self) {
        if self.values.result == 0 {
            self.values.result = 2;
            self.values.reason = Unavailable::ReadFailed as u16;
        }
        self.values.elapsed_us = self.start.elapsed().as_micros() as u64;
        event(Some(self.id), self.stage, self.values);
    }
}
