use super::*;
use std::sync::OnceLock;
use tracing::field::{Field, Visit};
use tracing::{Event as TraceEvent, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
#[derive(Default)]
struct Registry {
    publication: Mutex<()>,
    recorders: arc_swap::ArcSwap<Vec<Weak<Inner>>>,
}
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}
pub(super) fn register(r: &Recorder) {
    let registry = registry();
    let _publication = lock(&registry.publication);
    let mut rs = (**registry.recorders.load()).clone();
    rs.retain(|r| r.strong_count() > 0);
    rs.push(Arc::downgrade(&r.inner));
    registry.recorders.store(Arc::new(rs));
}
pub(crate) struct DiagnosticsLayer;
#[derive(Default)]
struct Scalars(
    Vec<(String, f64)>,
    Vec<(String, String)>,
    &'static str,
    Vec<(String, DeviceId)>,
    &'static str, // tracing target, distinct from the source module in .2
);
// Process-local numeric identities and typed discriminants only. Keep these
// scoped to the provider target; names, paths, tokens and Debug/string values
// never qualify as a generation or a reason.
fn clipboard_numeric_field(name: &str) -> bool {
    matches!(
        name,
        "clipboard_object"
            | "clipboard_offer"
            | "clipboard_generation"
            | "clipboard_representation"
            | "clipboard_stage"
            | "clipboard_reason"
            | "clipboard_hresult_class"
            | "clipboard_hresult"
            | "clipboard_state_begin"
            | "clipboard_state"
            | "clipboard_replacement"
    )
}

fn provider_numeric_field(name: &str) -> bool {
    matches!(
        name,
        "reason"
            | "placeholder_generation"
            | "preparation_generation"
            | "provider_preparation"
            | "placeholder_refill"
            | "placeholder_blocked"
    )
}

fn allowed(name: &str) -> bool {
    matches!(
        name,
        "windows_source_hint_resends"
            | "windows_source_hint_segmented_resends"
            | "dnd_hold_sequence"
            | "dnd_hold_end"
            | "dnd_park_window"
            | "from_x"
            | "from_y"
            | "park_x"
            | "park_y"
            | "end_x"
            | "end_y"
            | "catcher_hit"
            | "dnd_hold_items"
            | "dnd_hold_updates"
            | "dnd_hold_providers"
            | "dnd_hold_first_ms"
            | "dnd_hold_last_ms"
            | "dnd_hold_cursor_known"
            | "dnd_hold_x"
            | "dnd_hold_y"
            | "dnd_hold_end_x"
            | "dnd_hold_end_y"
            | "dnd_hold_catcher_x"
            | "dnd_hold_catcher_y"
            | "dnd_hold_scale"
            | "dnd_hold_screens"
            | "dnd_cancel_session"
            | "dnd_cancel_delivery"
            | "dnd_cancel_x"
            | "dnd_cancel_y"
            | "dnd_preview_stage"
            | "dnd_preview_source_image"
            | "dnd_preview_source_w"
            | "dnd_preview_source_h"
            | "dnd_preview_png_w"
            | "dnd_preview_png_h"
            | "dnd_preview_width"
            | "dnd_preview_height"
            | "dnd_preview_hotspot_x"
            | "dnd_preview_hotspot_y"
            | "dnd_preview_scale"
            | "dnd_phase"
            | "dnd_formats"
            | "dnd_formats_complete"
            | "dnd_representation_schema"
            | "dnd_representation_stage"
            | "dnd_representation_items"
            | "dnd_representation_complete"
            | "audio_level_window_sequence"
            | "audio_level_window_ms"
            | "pre_peak_dbfs"
            | "pre_rms_dbfs"
            | "pre_zero_frame_fraction"
            | "pre_frames"
            | "pre_nonfinite_samples"
            | "pre_digital_silence"
            | "post_measured"
            | "post_peak_dbfs"
            | "post_rms_dbfs"
            | "post_zero_frame_fraction"
            | "post_frames"
            | "post_nonfinite_samples"
            | "post_digital_silence"
            | "wasapi_buffers"
            | "wasapi_silent_buffers"
            | "route_count"
            | "stream_id"
            | "peer_ready"
            | "stream_open"
            | "capture_running"
            | "source_muted"
            | "playback_running"
            | "playback_dormant"
            | "prepared_peers"
            | "connected_sources"
            | "sleeping"
            | "burst"
            | "startup_ms"
            | "diagnostic_caps"
            | "diagnostic_secure"
            | "nonmodifier_flags"
            | "remote"
            | "tap_enabled"
            | "secure_input_cached"
            | "captured_local"
            | "captured_remote"
            | "own_session_echo"
            | "inject_requests"
            | "inject_errors"
            | "captured_held"
            | "injected_held"
            | "injected_modifiers"
            | "last_post_modifiers"
            | "last_button_modifiers"
            | "last_post_alpha"
            | "hid_modifiers"
            | "session_modifiers"
            | "passed"
            | "swallowed"
            | "nonmodifier_with_ctrl_cmd"
            | "caps_on"
            | "clipped"
            | "mouse_callbacks"
            | "keyboard_callbacks"
            | "key_captured"
            | "key_passed"
            | "key_swallowed"
            | "raw_mouse_reports"
            | "raw_without_device"
            | "raw_keyboard_reports"
            | "hook_reinstalls"
            | "hook_silent_ms"
            | "modifier"
            | "injected"
            | "mapped"
            | "held"
            | "stateless"
            | "epoch"
            | "known"
            | "draw"
            | "sources"
            | "points"
            | "size_scale"
            | "plane_units_per_reference"
            | "seq"
            | "generation"
            | "count"
            | "bytes"
            | "size"
            | "active"
            | "enabled"
            | "asleep"
            | "visible"
            | "hidden"
            | "application_hidden"
            | "success"
            | "ok"
            | "pressed"
            | "repeat"
            | "x"
            | "y"
            | "dx"
            | "dy"
            | "held_ms"
            | "duration_ms"
            | "elapsed_us"
            | "elapsed_ms"
            | "rtt_ms"
            | "depth"
            | "energy"
            | "velocity"
            | "acceleration"
            | "granted"
            | "respond"
            | "locked"
            | "native_ok"
            | "wanted"
            | "before"
            | "dropped"
            | "detected"
            | "stationary_us"
            | "movement_samples"
            | "enter_pending"
            | "exit_pending"
            | "hidden_motion_us"
            | "remote_moves"
            | "released_us"
            | "motion_events"
            | "in_callback"
            | "at_ns"
            | "done_ns"
            | "dissociated_before"
            | "retry"
            | "transition"
            | "e0"
            | "e1"
            | "e2"
            | "associate_called"
            | "associate_code"
            | "show_called"
            | "show_code"
            | "flush_called"
            | "flush_code"
            | "provider_path"
            | "placeholder_enabled"
            | "placeholder_used"
            | "placeholder_ready"
            | "placeholder_preparing"
            | "placeholder_prepared_files"
            | "placeholder_prepared_folders"
            | "placeholder_used_files"
            | "placeholder_used_folders"
            | "placeholder_prepare_us"
            | "placeholder_final_us"
            | "placeholder_fallback"
    )
}
impl Scalars {
    fn add(&mut self, f: &Field, v: f64) {
        let permitted = if self.2 == "oink_overlay_diagnostics" {
            matches!(
                f.name(),
                "overlay_object"
                    | "overlay_kind"
                    | "overlay_event"
                    | "overlay_reason"
                    | "overlay_worker"
                    | "overlay_flags"
                    | "overlay_desired"
                    | "overlay_frames"
                    | "overlay_applied"
                    | "overlay_posted"
                    | "overlay_started"
                    | "overlay_completed"
                    | "overlay_dropped"
                    | "overlay_pending"
                    | "overlay_busy_ms"
                    | "overlay_apply_known"
                    | "overlay_apply_age_ms"
                    | "overlay_panics"
                    | "overlay_native_owner"
                    | "overlay_generation"
                    | "overlay_windows"
                    | "overlay_cached_shown"
                    | "overlay_content"
                    | "overlay_visible"
                    | "overlay_on_space"
                    | "overlay_onscreen_known"
                    | "overlay_onscreen"
                    | "overlay_application_hidden"
                    | "overlay_screens"
                    | "overlay_space_epoch"
                    | "overlay_display_epoch"
                    | "overlay_session_epoch"
                    | "overlay_session"
            )
        } else if self.2 == "oink_link_diagnostics" {
            matches!(
                f.name(),
                "elapsed_ms"
                    | "os_error"
                    | "os_error_known"
                    | "duration_ms"
                    | "choice_counter"
                    | "choice_watermark"
                    | "path_counter"
                    | "revision"
                    | "writers_pending"
                    | "drain_counts_known"
                    | "readers_pending"
                    | "local_drained"
                    | "peer_drained"
                    | "commit_queued"
                    | "drained_queued"
                    | "reverse"
                    | "opening_received"
                    | "wait_outbound"
                    | "wait_inbound"
                    | "wait_release"
            )
        } else if bundle_only_target(self.2) {
            matches!(
                f.name(),
                "transfer_id"
                    | "collection_id"
                    | "limited_ms"
                    | "segment_work_ms"
                    | "success"
                    | "link_id"
                    | "stream_id"
                    | "received"
                    | "late"
                    | "concealed"
                    | "empty_queue_plc_frames"
                    | "gap_plc_frames"
                    | "decode_errors"
                    | "buffered_ms"
                    | "arrival_jitter_ms"
                    | "sample_mono_us"
                    | "pacing_id"
                    | "duration_ms"
                    | "limited"
                    | "local_input"
                    | "group_input"
                    | "audio"
                    | "probe_timeout"
                    | "rtt_ms"
                    | "target_ms"
                    | "bytes"
                    | "total_bytes"
                    | "file_count"
            )
        } else {
            allowed(f.name())
                || (self.4 == "oink_dnd_read" && oink_foundation::drag_trace::FIELDS.contains(&f.name()))
                // This callsite emits a typed FileProviderReason discriminant.
                // Do not admit numeric `reason` from unrelated targets.
                || (self.4 == "oink_clipboard::macos::file_provider" && provider_numeric_field(f.name()))
                || (self.4 == "oink_clipboard::windows::content::target" && clipboard_numeric_field(f.name()))
                || (self.4 == "oink_file_diagnostics" && matches!(f.name(),
                    "file_stage" | "file_request" | "file_index" | "file_code" | "file_reason"
                    | "file_object" | "file_format" | "file_offer" | "file_generation" | "file_node"))
        };
        if permitted && v.is_finite() && self.0.len() < 24 {
            self.0.push((f.name().into(), v));
        }
    }
}
impl Visit for Scalars {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // Only these audited Option<bool>/Option<u32>/CGError callsite fields use Debug.
        // Do not format any other Debug value: it can be an input event, a PID, path or payload.
        if field.name() == "message" && self.2.starts_with("oink_routing::router") {
            let message = format!("{value:?}");
            if routing_message(&message) && self.1.len() < 8 {
                self.1.push(("message".into(), message));
            }
        }
        if (matches!(
            field.name(),
            "hidden" | "application_hidden" | "locked" | "motion_events" | "caps_on"
        ) || (self.2.ends_with("::macos::cursor") && matches!(field.name(), "e0" | "e1" | "e2")))
            && let Some(value) = debug_scalar(self.2, field.name(), &format!("{value:?}"))
        {
            self.add(field, value);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if self.2 == "oink_overlay_diagnostics" {
            return;
        }
        if clipboard_numeric_field(field.name())
            || (self.4 == "oink_clipboard::macos::file_provider" && provider_numeric_field(field.name()))
        {
            return;
        }
        if field.name() == "dnd_representation_classes" {
            // A..Q are reviewed type CLASSES, ';' is an item boundary. No
            // arbitrary UTI, URL, name or data may enter this label. Preserve
            // order/multiplicity instead of reducing the drag to a bit mask.
            if matches!(
                self.2,
                "oink_clipboard::macos::dnd"
                    | "oink_clipboard::macos::dnd::representation_trace"
                    | "oink_clipboard::windows::dnd"
                    | "oink_clipboard::windows::dnd::representation_trace"
            ) && value.len() <= 131_070
                && value.bytes().all(|b| matches!(b, b'A'..=b'Q' | b';'))
            {
                self.1.push((field.name().into(), value.into()));
            }
            return;
        }
        if self.2 == "oink_link_diagnostics" {
            if matches!(field.name(), "peer_id" | "via_id") && value.len() == 64 {
                let mut bytes = [0; 32];
                if value
                    .as_bytes()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(bytes.iter_mut())
                    .all(|(pair, byte)| {
                        match std::str::from_utf8(pair)
                            .ok()
                            .and_then(|p| u8::from_str_radix(p, 16).ok())
                        {
                            Some(v) => {
                                *byte = v;
                                true
                            }
                            None => false,
                        }
                    })
                {
                    self.3.push((
                        if field.name() == "peer_id" { "peer" } else { "via" }.into(),
                        DeviceId(bytes),
                    ));
                }
                return;
            }
            let safe = match field.name() {
                "kind" => matches!(
                    value,
                    "path_transition"
                        | "udp_send"
                        | "candidate_dial"
                        | "connection_step"
                        | "migration_step"
                        | "low_step"
                        | "low_control"
                        | "input_wait"
                ),
                "operation" | "connection" | "local_boot" | "peer_boot" => {
                    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
                }
                "network_path" => matches!(value, "offline" | "establishing" | "relay" | "direct"),
                "initiator" => matches!(value, "local" | "peer" | "unknown"),
                "arbiter" | "waiting_on" => matches!(value, "local" | "peer" | "none"),
                "candidate_class" => matches!(
                    value,
                    "ipv4_loopback"
                        | "ipv4_link_local"
                        | "ipv4_private"
                        | "ipv4_global"
                        | "ipv6_loopback"
                        | "ipv6_link_local"
                        | "ipv6_private"
                        | "ipv6_global"
                ),
                "stage" | "waiting" => matches!(
                    value,
                    "instance"
                        | "kernel"
                        | "none"
                        | "connection_select"
                        | "connection_selected"
                        | "other_candidate"
                        | "local_selection"
                        | "selection"
                        | "path_install"
                        | "config_ready"
                        | "quic"
                        | "begin_received"
                        | "begin_queued"
                        | "local_drain"
                        | "peer_drained"
                        | "commit_queued"
                        | "committed_queued"
                        | "drained_queued"
                        | "peer_committed"
                        | "peer_commit"
                        | "cutover"
                        | "tcp_accept"
                        | "tcp_connect"
                        | "tls"
                        | "bind_request"
                        | "bind_reply"
                        | "deliver"
                        | "direction"
                        | "peer_info"
                        | "warm_end"
                        | "warm_send_end"
                        | "warm_peer_end"
                        | "warm_end_ack"
                        | "pool_slot"
                        | "open_request"
                        | "open_reply"
                        | "reverse_request"
                        | "reverse_socket"
                        | "race_winner"
                        | "refresh"
                        | "cancel"
                        | "input_opening"
                ),
                "result" => matches!(
                    value,
                    "entered" | "completed" | "failed" | "received" | "long" | "ended" | "dropped"
                ),
                "reason" => matches!(
                    value,
                    "none"
                        | "host_unreachable"
                        | "wrong_route"
                        | "connection_mismatch"
                        | "boot_mismatch"
                        | "zero_counter"
                        | "stale_counter"
                        | "selected_reply"
                        | "unexpected_reply"
                        | "route_missing"
                        | "wrong_role"
                        | "path_mismatch"
                        | "revision_mismatch"
                        | "already_migrating"
                        | "not_selected"
                        | "migration_mismatch"
                        | "io_error"
                        | "connect_error"
                        | "version_mismatch"
                        | "transport_error"
                        | "peer_connection_closed"
                        | "peer_application_closed"
                        | "peer_reset"
                        | "locally_closed"
                        | "cids_exhausted"
                        | "dial"
                        | "dial_failed"
                        | "accepted"
                        | "ready"
                        | "cutover"
                        | "offer"
                        | "selected"
                        | "superseded"
                        | "cutover_timeout"
                        | "connection_lost"
                        | "heartbeat_timeout"
                        | "peer_restarted"
                        | "relay_lost"
                        | "local_disconnect"
                        | "member_removed"
                        | "leaving_group"
                        | "sleeping"
                        | "handshake_failed"
                        | "handshake_timeout"
                        | "control_stream_failed"
                        | "stopped"
                        | "timeout"
                        | "cancelled"
                        | "error"
                        | "authenticated"
                        | "identity_rejected"
                        | "rejected"
                        | "attached"
                        | "paused"
                        | "link_lost"
                        | "opening_incomplete"
                ),
                _ => false,
            };
            if safe {
                self.1.push((field.name().into(), value.into()));
            }
            return;
        }
        if field.name() == "destination_class"
            && self.2.starts_with("oink_network::qos::source_hint")
            && matches!(
                value,
                "ipv4_loopback"
                    | "ipv4_link_local"
                    | "ipv4_private"
                    | "ipv4_global"
                    | "ipv6_loopback"
                    | "ipv6_link_local"
                    | "ipv6_private"
                    | "ipv6_global"
            )
            && self.1.len() < 8
        {
            self.1.push((field.name().into(), value.into()));
        }
        if bundle_only_target(self.2) {
            let safe = match field.name() {
                "kind" => matches!(
                    value,
                    "transfer_start"
                        | "transfer_end"
                        | "transfer_progress"
                        | "pacing_state"
                        | "pacing_sample"
                        | "collection_stage"
                        | "collection_end"
                        | "audio_receiver_sample"
                ),
                "network_path" => matches!(value, "direct" | "relayed" | "unknown"),
                "channel" => value == "low_tcp",
                "role" => matches!(value, "sender" | "receiver" | "save" | "export"),
                "stage" => matches!(
                    value,
                    "start"
                        | "prepared"
                        | "request"
                        | "response"
                        | "zip"
                        | "serialized"
                        | "device_timelines"
                        | "merged"
                        | "synced"
                ),
                "result" => matches!(value, "pending" | "completed" | "cancelled" | "error"),
                _ => false,
            };
            if safe {
                self.1.push((field.name().into(), value.into()));
            }
            return;
        }
        if field.name() == "os_version" && safe_os_version(self.2, value) && self.1.len() < 8 {
            self.1.push((field.name().into(), value.into()));
        }
        if field.name() == "phase"
            && matches!(
                value,
                "disabled"
                    | "waiting_for_sound"
                    | "source_silent"
                    | "waiting_for_sources"
                    | "waking_output"
                    | "playing"
                    | "forwarding"
                    | "verifying_capture_permission_local_playback"
                    | "waiting_for_exit_local_playback"
                    | "forwarding_disabled_local_playback"
                    | "sharing_paused_local_playback"
                    | "system_sleep_local_playback"
                    | "blocked"
                    | "capture_cleanup_failed"
            )
            && self.1.len() < 8
        {
            self.1.push((field.name().into(), value.into()));
        }
        if matches!(field.name(), "reason" | "why" | "state")
            && matches!(
                value,
                "sample_unavailable"
                    | "hide_reason_unavailable"
                    | "hide_reason_available"
                    | "unsupported_architecture"
                    | "visibility_symbol_missing"
                    | "visibility_pattern_changed"
                    | "scoreboard_pattern_changed"
                    | "scoreboard_unavailable"
                    | "memory_read_failed"
                    | "inconsistent_visibility"
                    | "foreground_changed"
                    | "cursor_visible"
                    | "cursor_obscured"
                    | "hidden_during_remote_absolute_motion"
                    | "manual_mode"
                    | "not_auto_focus_or_paused"
                    | "not_focus_or_paused"
                    | "focus_changed_or_paused"
                    | "hidden_confined"
                    | "hidden_warp"
                    | "hidden_stationary_motion"
                    | "hidden_without_capture_evidence"
                    | "local pointer crossed display edge"
                    | "local pointer crossed edge"
                    | "pointer crossed edge"
                    | "idle"
                    | "probing"
                    | "ready"
                    | "leaving"
                    | "hosting"
            )
            && self.1.len() < 8
        {
            self.1.push((field.name().into(), value.into()));
        }
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.add(f, v as f64);
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.add(f, v as f64);
    }
    fn record_f64(&mut self, f: &Field, v: f64) {
        self.add(f, v);
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        if !provider_numeric_field(f.name())
            && !clipboard_numeric_field(f.name())
            && !oink_foundation::drag_trace::FIELDS.contains(&f.name())
        {
            self.add(f, f64::from(v));
        }
    }
}
fn local_only_target(target: &str) -> bool {
    // The typed VolumeRouting transition is queued with the source provenance;
    // keep its local log without creating a second, partial generic Log record.
    if target == "oink_volume_routing" {
        return true;
    }
    target == "oink_local_transfer"
}
pub(crate) fn bundle_only_target(target: &str) -> bool {
    matches!(
        target,
        "oink_transfer_diagnostics"
            | "oink_pacing_diagnostics"
            | "oink_collection_diagnostics"
            | "oink_audio_diagnostics"
            | "oink_overlay_diagnostics"
    )
}
fn safe_os_version(module: &str, value: &str) -> bool {
    module.ends_with("::macos::cursor_visibility")
        && (value == "unknown"
            || (!value.is_empty() && value.len() <= 32 && value.bytes().all(|b| b.is_ascii_digit() || b == b'.')))
}
// Only literal, reviewed routing messages. Never store a formatted event, key or error.
fn routing_message(message: &str) -> bool {
    matches!(
        message,
        "focus handoff"
            | "focus handoff requested"
            | "cursor latency: FocusEnter received"
            | "cursor trail handoff"
            | "cursor trail"
            | "cursor trail source stopped"
            | "FocusEnter arrived after another device announced this handoff: entering now"
            | "ignoring stale FocusEnter; sending our focus back"
            | "sharing paused: ignoring FocusEnter"
            | "focus belongs to a previous routing session: staying local"
            | "focus holder is back and still holds focus: rejoining"
            | "historical focus learned without joining: routing session differs"
            | "focus holder never confirmed the handoff: taking focus back locally"
            | "peer lost: releasing its keys and buttons"
            | "emergency stop on this device: pausing sharing on every device"
            | "resuming after an emergency stop: telling the other devices"
            | "fresh join: other input resumes existing group pointer"
            | "adopting keyboard state of the previous holder"
    )
}
fn detailed_callsite(module: &str, has_field: impl Fn(&str) -> bool) -> bool {
    (module == "oink_routing::router" && has_field("diagnostic"))
        || module == "oink_routing::router::trail"
        || module.ends_with("::resistance")
        || module.ends_with("::dnd")
        || module == "oink_clipboard::macos::dnd::representation_trace"
        || module == "oink_clipboard::windows::dnd::representation_trace"
        || (module.ends_with("::windows::drag_image") && has_field("dnd_preview_stage"))
        || (module.ends_with("::macos::file_provider") && has_field("dnd_phase"))
        || module.ends_with("::cursor")
        || module.ends_with("::cursor_owner")
        || (module == "oink_agent::audio::report" && has_field("audio_level_window_sequence"))
        || (module == "oink_agent::audio::link" && has_field("sample_mono_us") && has_field("concealed"))
        || module.ends_with("::macos::keyboard_diagnostics")
        || module.ends_with("::macos::capslock")
        || (module.ends_with("::macos::tap") && has_field("stateless"))
        || (module.ends_with("::windows::hook") && (has_field("key_captured") || has_field("mapped")))
        || (module.ends_with("::macos::tap")
            && (has_field("in_callback") || (has_field("at_ns") && has_field("done_ns"))))
}
fn debug_scalar(module: &str, field: &str, text: &str) -> Option<f64> {
    let inner = text
        .strip_prefix("Some(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(text);
    match field {
        "hidden" | "application_hidden" | "locked" | "caps_on" => match inner {
            "true" => Some(1.0),
            "false" => Some(0.0),
            _ => None,
        },
        "motion_events" => inner.parse::<u32>().ok().map(f64::from),
        "e0" | "e1" | "e2" if module.ends_with("::macos::cursor") => inner
            .strip_prefix("CGError(")?
            .strip_suffix(')')?
            .parse::<i32>()
            .ok()
            .map(f64::from),
        _ => None,
    }
}
fn keyboard_event(module: &str, values: &[(String, f64)]) -> Option<Event> {
    if module == "oink_input::diagnostics" {
        values.iter().find_map(|(name, value)| {
            let kind = match name.as_str() {
                "diagnostic_caps" => StateKind::CapsLock,
                "diagnostic_secure" => StateKind::SecureInput,
                "nonmodifier_flags" => {
                    return Some(Event::KeyboardAnomaly {
                        nonmodifier_flags: *value == 1.0,
                    });
                }
                _ => return None,
            };
            Some(Event::State {
                kind,
                active: *value == 1.0,
                count: 0,
                value: 0.0,
            })
        })
    } else {
        None
    }
}

impl<S: Subscriber> Layer<S> for DiagnosticsLayer {
    fn on_event(&self, event: &TraceEvent<'_>, _: Context<'_, S>) {
        // Producers read immutable snapshots while registry publication proceeds.
        let rs = registry().recorders.load();
        record_event(event, rs.as_slice());
    }
}

fn record_event(event: &TraceEvent<'_>, rs: &[Weak<Inner>]) {
    let m = event.metadata();
    if local_only_target(m.target()) {
        return;
    }
    let detailed = m
        .module_path()
        .is_some_and(|module| detailed_callsite(module, |f| m.fields().field(f).is_some()));
    if (*m.level() > tracing::Level::INFO && !detailed)
        || *m.level() > tracing::Level::DEBUG
        || (*m.level() > tracing::Level::WARN && !m.target().starts_with("oink"))
    {
        return;
    }
    if !rs
        .iter()
        .any(|r| r.upgrade().is_some_and(|i| i.enabled.load(Ordering::Relaxed)))
    {
        return;
    }
    let module = if bundle_only_target(m.target()) || matches!(m.target(), "oink_link_diagnostics" | "oink_dnd_read") {
        m.target()
    } else {
        m.module_path().unwrap_or("external")
    };
    let mut values = Scalars(Vec::new(), Vec::new(), module, Vec::new(), m.target());
    event.record(&mut values);
    let identities = std::mem::take(&mut values.3);
    let keyboard = keyboard_event(m.module_path().unwrap_or("external"), &values.0);
    let ev = keyboard.unwrap_or_else(|| Event::Log {
        module: module.into(),
        line: m.line().unwrap_or(0),
        level: match *m.level() {
            tracing::Level::ERROR => 0,
            tracing::Level::WARN => 1,
            tracing::Level::INFO => 2,
            _ => 3,
        },
        fields: values.0,
        labels: values.1,
    });
    for weak in rs.iter() {
        if let Some(inner) = weak.upgrade() {
            let recorder = Recorder { inner };
            let mut ev = ev.clone();
            if let Event::Log { labels, .. } = &mut ev {
                let anon = recorder.anonymizer();
                labels.extend(identities.iter().map(|(name, peer)| (name.clone(), anon.device(*peer))));
            }
            recorder.record(ev);
        }
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) struct ScopedLayer(Weak<Inner>);
#[cfg(all(test, debug_assertions))]
pub(super) fn scoped(recorder: &Recorder) -> ScopedLayer {
    ScopedLayer(Arc::downgrade(&recorder.inner))
}
#[cfg(all(test, debug_assertions))]
impl<S: Subscriber> Layer<S> for ScopedLayer {
    fn on_event(&self, event: &TraceEvent<'_>, _: Context<'_, S>) {
        record_event(event, std::slice::from_ref(&self.0));
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn registry_publication_does_not_drop_a_trace_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([91; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let publishing = lock(&registry().publication);
        let subscriber = tracing_subscriber::registry().with(DiagnosticsLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_registry_health", count = 1234567_u64, "boundary");
        });
        drop(publishing);
        let queue = recorder.inner.queue.load();
        let recorded = std::iter::from_fn(|| queue.pop()).any(|row| {
            matches!(row.event, Event::Log { fields, .. }
                if fields.iter().any(|(name, value)| name == "count" && *value == 1234567.0))
        });
        recorder.shutdown();
        assert!(
            recorded,
            "a concurrent recorder registration lost a diagnostic boundary"
        );
    }
    use tracing_subscriber::layer::SubscriberExt;

    struct Collect(Arc<Mutex<Vec<Scalars>>>);
    impl<S: Subscriber> Layer<S> for Collect {
        fn on_event(&self, event: &TraceEvent<'_>, _: Context<'_, S>) {
            let mut values = Scalars(
                Vec::new(),
                Vec::new(),
                event.metadata().target(),
                Vec::new(),
                event.metadata().target(),
            );
            event.record(&mut values);
            lock(&self.0).push(values);
        }
    }
    #[test]
    fn overlay_health_keeps_typed_counters_without_native_or_desktop_identity() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_overlay_diagnostics",
                overlay_object=3_u64, overlay_kind=2_u8, overlay_event=10_u8,
                overlay_pending=2_u64, overlay_busy_ms=3000_u64,
                overlay_visible=0_u64, overlay_onscreen_known=true, overlay_onscreen=0_u64,
                window_id=123456_u64, pid=42_u64, name="PRIVATE", title="PRIVATE TITLE",
                path="/private/example", message="PRIVATE CONTENT");
            tracing::info!(target: "oink_overlay_diagnostics",
                overlay_object="PRIVATE", overlay_event="PRIVATE", reason="idle",
                overlay_pending=?Some(123_u64));
            tracing::info!(target: "unrelated", overlay_object=3_u64, overlay_pending=2_u64);
        });
        let rows = lock(&rows);
        assert_eq!(rows[0].0.len(), 8);
        assert!(rows[0].0.contains(&("overlay_busy_ms".into(), 3000.)));
        assert!(rows.iter().all(|r| r.1.is_empty()));
        assert!(rows[1].0.is_empty());
        assert!(rows[2].0.is_empty());
    }
    #[test]
    fn overlay_health_counters_survive_export_without_private_fields() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = crate::diagnostics::tests::isolated(temp.path().into(), DeviceId([40; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let subscriber = tracing_subscriber::registry().with(scoped(&recorder));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_overlay_diagnostics", overlay_object=7_u64,
                overlay_kind=2_u8, overlay_event=10_u8, overlay_onscreen_known=true,
                overlay_onscreen=0_u64, overlay_visible=2_u64, window_id=987654_u64,
                name="PRIVATE-NAME", title="PRIVATE-TITLE", path="/PRIVATE/PATH", reason="idle");
        });
        let bundle = recorder.save(Reason::Manual).unwrap();
        recorder.shutdown();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut found = false;
        for index in 0..zip.len() {
            use std::io::Read;
            let mut entry = zip.by_index(index).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("PRIVATE") && !text.contains("987654"));
            if entry.name().unwrap() != "merged.ndjson" {
                continue;
            }
            for line in text.lines() {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                if let Ok(Event::Log {
                    module, fields, labels, ..
                }) = serde_json::from_value(row["event"].clone())
                    && module == "oink_overlay_diagnostics"
                {
                    assert_eq!(fields.len(), 6);
                    assert!(fields.contains(&("overlay_onscreen".into(), 0.)));
                    assert!(fields.contains(&("overlay_visible".into(), 2.)));
                    assert!(labels.is_empty());
                    found = true;
                }
            }
        }
        assert!(found);
    }
    #[test]
    fn held_drag_geometry_projection_keeps_numeric_summary_without_content() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_clipboard::macos::dnd",
                dnd_hold_items = 3_u64, dnd_hold_updates = 2_u64, dnd_hold_providers = 1_u64,
                dnd_hold_first_ms = 40_i64, dnd_hold_x = 99.5, dnd_hold_scale = 2.,
                path = "/private/example", contents = "private content", key = 42_u64,
                "held source artwork summary");
        });
        let rows = lock(&rows);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.len(), 6);
        assert!(
            rows[0]
                .0
                .iter()
                .any(|(name, value)| name == "dnd_hold_providers" && *value == 1.)
        );
        assert!(rows[0].1.is_empty());
    }

    #[test]
    fn windows_hold_end_keeps_park_class_and_geometry_without_window_names() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_clipboard::windows::dnd",
                dnd_hold_end = 3_u64, elapsed_us = 3000_u64,
                from_x = 100_i32, from_y = 200_i32, park_x = 3839_i32, park_y = 1031_i32,
                end_x = 3839_i32, end_y = 1031_i32, catcher_hit = false, dnd_park_window = 1_u64,
                window_class = "PRIVATE WINDOW CLASS", title = "PRIVATE WINDOW TITLE",
                "Windows held source summary");
        });
        let rows = lock(&rows);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.len(), 10);
        assert!(rows[0].0.contains(&("dnd_park_window".into(), 1.)));
        assert!(rows[0].0.contains(&("park_y".into(), 1031.)));
        assert!(rows[0].1.is_empty());
    }

    #[test]
    fn placeholder_phases_keep_only_typed_numeric_fields() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 50_u64, provider_path = 2_u64,
                placeholder_enabled = true, placeholder_used = true,
                placeholder_ready = 2_u64, placeholder_preparing = false,
                placeholder_prepared_files = 5_u64, placeholder_prepared_folders = 5_u64,
                placeholder_used_files = 2_u64, placeholder_used_folders = 1_u64,
                placeholder_prepare_us = 123_u64, placeholder_final_us = 456_u64,
                placeholder_fallback = 1_u64,
                path = "/private/file", name = "PRIVATE", endpoint = "PRIVATE ENDPOINT",
                placeholder_identity = 12345_u64,
                "File Provider namespace path");
            tracing::debug!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 51_u64, count = 1_u64, success = false, reason = 3_u64,
                path = "/private/file", "File Provider placeholder cleanup");
        });
        let rows = lock(&rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.len(), 13);
        assert_eq!(rows[1].0.len(), 4);
        assert!(rows[1].0.contains(&("reason".into(), 3.)));
        for (name, value) in [
            ("placeholder_used", 1.),
            ("placeholder_final_us", 456.),
            ("placeholder_fallback", 1.),
        ] {
            assert!(rows[0].0.contains(&(name.into(), value)));
        }
        assert!(rows.iter().all(|row| row.1.is_empty()));
    }

    #[test]
    fn drag_read_diagnostics_preserve_exact_request_and_timing_without_payloads() {
        use oink_foundation::drag_trace::{Context, Trace, stage};
        let temp = tempfile::tempdir().unwrap();
        let recorder = crate::diagnostics::tests::isolated(temp.path().into(), DeviceId([42; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let subscriber = tracing_subscriber::registry().with(scoped(&recorder));
        tracing::subscriber::with_default(subscriber, || {
            let trace = Trace::new(
                stage::GET_DATA,
                Context {
                    offer: u64::MAX,
                    representation: 4,
                    format: 13,
                    aspect: 1,
                    tymed: 1,
                    index: -1,
                    ..Default::default()
                }
                .with_request([255; 16]),
            );
            trace.result(0);
            drop(trace);
            tracing::info!(target:"oink_dnd_read", dr_format="PRIVATE-FORMAT", dr_request0=true, payload="PRIVATE-TEXT", path="PRIVATE-PATH", error="PRIVATE-ERROR");
            tracing::warn!(target:"unrelated", dr_id=10_u64, dr_result=0_i64);
        });
        let bundle = recorder.save(Reason::Manual).unwrap();
        recorder.shutdown();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let logs: Vec<_> = rows.iter().filter_map(|r| r.pointer("/event/Log")).collect();
        assert_eq!(logs.len(), 4);
        for log in &logs[..2] {
            let fields: Vec<(String, f64)> = serde_json::from_value(log["fields"].clone()).unwrap();
            assert_eq!(fields.len(), 24);
            for key in [
                "dr_request0",
                "dr_request1",
                "dr_request2",
                "dr_request3",
                "dr_offer0",
                "dr_offer1",
            ] {
                assert!(fields.contains(&(key.into(), u32::MAX as f64)));
            }
            assert!(fields.contains(&("dr_format".into(), 13.)));
            assert!(fields.contains(&("dr_index".into(), -1.)));
            assert!(fields.iter().any(|(key, value)| key == "dr_at_us" && *value > 0.));
            assert!(fields.iter().any(|(key, value)| key == "dr_thread" && *value > 0.));
            assert_eq!(log["labels"], serde_json::json!([]));
        }
        for log in &logs[2..] {
            assert_eq!(log["fields"], serde_json::json!([]));
            assert_eq!(log["labels"], serde_json::json!([]));
        }
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        for index in 0..zip.len() {
            use std::io::Read;
            let mut text = String::new();
            zip.by_index(index).unwrap().read_to_string(&mut text).unwrap();
            assert!(!text.contains("PRIVATE"));
        }
    }

    #[test]
    fn file_hydration_diagnostics_keep_numeric_stages_without_paths_or_error_text() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "oink_file_diagnostics",
                file_stage = 11_u8, file_request = 3_u64, file_index = 1_u64,
                file_code = 0x80070185_u32, file_reason = 8_u8,
                file_object = 4_u64, file_format = 1_u8,
                file_offer = 8_u64, file_generation = 2_u64, file_node = 9_u64,
                name = "PRIVATE-NAME", path = "PRIVATE-PATH", error = "PRIVATE-ERROR");
            tracing::warn!(target: "unrelated", file_stage = 11_u8, file_code = 12_u32);
        });
        let rows = lock(&rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0.len(), 10);
        assert!(rows[0].0.contains(&("file_code".into(), 0x80070185_u32 as f64)));
        assert!(rows[0].1.is_empty());
        assert!(rows[1].0.is_empty());
    }

    #[test]
    fn clipboard_failure_correlation_survives_package_without_names_or_untyped_fields() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = crate::diagnostics::tests::isolated(temp.path().into(), DeviceId([42; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let subscriber = tracing_subscriber::registry().with(scoped(&recorder));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "oink_clipboard::windows::content::target",
                clipboard_object = 7_u64, clipboard_offer = 8_u64,
                clipboard_generation = 9_u64, clipboard_representation = 10_u64,
                clipboard_stage = 1_u8, clipboard_reason = 3_u8,
                clipboard_hresult_class = 1_u8, clipboard_hresult = 2147680286_u32,
                clipboard_state_begin = 2_u8, clipboard_state = 3_u8,
                clipboard_replacement = 11_u64, name = "PRIVATE-NAME", content = "PRIVATE-CONTENT");
            tracing::warn!(target: "oink_clipboard::windows::content::target",
                clipboard_object = "PRIVATE", clipboard_generation = true, clipboard_state = "ready");
            tracing::warn!(target: "unrelated", clipboard_object = 7_u64, clipboard_reason = 3_u8);
        });
        let bundle = recorder.save(Reason::Manual).unwrap();
        recorder.shutdown();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let logs: Vec<_> = rows.iter().filter_map(|r| r.pointer("/event/Log")).collect();
        assert_eq!(logs.len(), 3);
        let fields: Vec<(String, f64)> = serde_json::from_value(logs[0]["fields"].clone()).unwrap();
        assert_eq!(fields.len(), 11);
        assert!(fields.contains(&("clipboard_generation".into(), 9.)));
        assert!(fields.contains(&("clipboard_state".into(), 3.)));
        assert!(fields.contains(&("clipboard_replacement".into(), 11.)));
        assert_eq!(logs[0]["level"], 1); // Still WARN.
        for log in &logs[1..] {
            assert_eq!(log["fields"], serde_json::json!([]));
        }
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        for index in 0..zip.len() {
            use std::io::Read;
            let mut text = String::new();
            zip.by_index(index).unwrap().read_to_string(&mut text).unwrap();
            assert!(!text.contains("PRIVATE"));
        }
    }

    #[test]
    fn typed_pool_generations_survive_saved_package_with_numeric_scope_only() {
        let temp = tempfile::tempdir().unwrap();
        // Only the scoped layer feeds this recorder; the global registry would
        // also capture unrelated events from concurrent tests.
        let recorder =
            crate::diagnostics::tests::isolated(temp.path().join("recorder"), DeviceId([41; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let subscriber = tracing_subscriber::registry().with(scoped(&recorder));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 52_u64, placeholder_generation = 7_u64,
                preparation_generation = 9_u64, provider_preparation = 1_u64,
                placeholder_refill = 3_u64, placeholder_blocked = 6_u64,
                name = "PRIVATE", path = "/PRIVATE/PATH");
            tracing::info!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 50_u64, placeholder_generation = 7_u64,
                preparation_generation = 9_u64, provider_preparation = 1_u64, epoch = 3_u64);
            tracing::info!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 53_u64, placeholder_generation = 7_u64,
                preparation_generation = 9_u64, provider_preparation = 1_u64, epoch = 3_u64);
            tracing::info!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 52_u64, placeholder_generation = "ready",
                preparation_generation = true, provider_preparation = "PRIVATE",
                placeholder_refill = false, placeholder_blocked = "ready");
            tracing::info!(target: "oink_clipboard::windows::dnd",
                dnd_phase = 52_u64, placeholder_generation = 7_u64,
                preparation_generation = 9_u64, provider_preparation = 1_u64,
                placeholder_refill = 1_u64, placeholder_blocked = 0_u64);
        });
        let bundle = recorder.save(Reason::Manual).unwrap();
        recorder.shutdown();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut logs = Vec::new();
        for i in 0..zip.len() {
            use std::io::Read;
            let mut bytes = Vec::new();
            let mut entry = zip.by_index(i).unwrap();
            let merged = entry.name().unwrap() == "merged.ndjson";
            entry.read_to_end(&mut bytes).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("PRIVATE"));
            if !merged {
                continue;
            }
            for line in text.lines() {
                if let Ok(row) = serde_json::from_str::<serde_json::Value>(line)
                    && let Ok(Event::Log { fields, labels, .. }) = serde_json::from_value(row["event"].clone())
                {
                    assert!(labels.is_empty());
                    logs.push(fields);
                }
            }
        }
        assert_eq!(logs.len(), 5);
        for (name, value) in [
            ("placeholder_generation", 7.),
            ("preparation_generation", 9.),
            ("provider_preparation", 1.),
            ("placeholder_refill", 3.),
            ("placeholder_blocked", 6.),
        ] {
            assert!(logs[0].contains(&(name.into(), value)), "missing {name}");
        }
        assert!(logs[1].contains(&("placeholder_generation".into(), 7.)));
        assert!(logs[1].contains(&("preparation_generation".into(), 9.)));
        assert!(logs[2].contains(&("placeholder_generation".into(), 7.)));
        assert!(logs[2].contains(&("preparation_generation".into(), 9.)));
        assert_eq!(logs[3], [("dnd_phase".into(), 52.)]);
        assert_eq!(logs[4], [("dnd_phase".into(), 52.)]);
    }

    #[test]
    fn placeholder_reason_rejects_strings_booleans_and_other_targets() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            // Even a label accepted elsewhere must not enter this typed field.
            tracing::debug!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 51_u64, reason = "ready", path = "/private/file");
            tracing::debug!(target: "oink_clipboard::macos::file_provider",
                dnd_phase = 51_u64, reason = true);
            tracing::debug!(target: "oink_clipboard::windows::dnd",
                dnd_phase = 51_u64, reason = 3_u64, name = "PRIVATE");
        });
        let rows = lock(&rows);
        assert_eq!(rows.len(), 3);
        for row in rows.iter() {
            assert_eq!(row.0, [("dnd_phase".into(), 51.)]);
            assert!(row.1.is_empty());
        }
    }

    #[test]
    fn link_projection_anonymizes_peers_and_keeps_only_reviewed_fields_in_zip() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(
            temp.path().join("recorder"),
            DeviceId([41; 32]),
            Some(GroupId([42; 16])),
            true,
        )
        .unwrap();
        let peer = DeviceId([43; 32]);
        let via = DeviceId([44; 32]);
        let subscriber = tracing_subscriber::registry().with(DiagnosticsLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_link_diagnostics", kind = "migration_step", peer_id = peer.to_hex().as_str(), via_id = via.to_hex().as_str(),
                operation = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", stage = "peer_committed", result = "failed", reason = "cutover_timeout", waiting = "peer_committed",
                local_drained = true, peer_drained = true, writers_pending = 0u64, elapsed_ms = 500.,
                candidate = "203.0.113.7:47470", error = "PRIVATE ERROR /PRIVATE/PATH", name = "PRIVATE DEVICE", message = "PRIVATE MESSAGE");
            tracing::info!(target: "oink_link_diagnostics", kind = "connection_step", peer_id = peer.to_hex().as_str(),
                operation = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", stage = "selection", result = "received", reason = "stale_counter",
                waiting = "connection_select", waiting_on = "peer", arbiter = "peer", choice_counter = 1u64, choice_watermark = 6u64,
                local_boot = "cccccccccccccccccccccccccccccccc", peer_boot = "dddddddddddddddddddddddddddddddd");
            tracing::warn!(target: "oink_link_diagnostics", kind = "udp_send", stage = "kernel",
                result = "dropped", reason = "host_unreachable", os_error = 65i32, os_error_known = true,
                destination = "203.0.113.7:47470", src_ip = "203.0.113.8", error = "PRIVATE ERROR /PRIVATE/PATH");
            tracing::info!(target: "oink_link_diagnostics", stage = "/PRIVATE/PATH", reason = "PRIVATE ERROR", operation = "203.0.113.7:47470", network_path = "/PRIVATE/PATH");
        });
        let mut rows = Vec::new();
        while let Some(row) = recorder.inner.queue.load().pop() {
            rows.push(row);
        }
        let row = rows.iter().find(|row| matches!(&row.event, Event::Log {module,labels,..} if module == "oink_link_diagnostics" && labels.contains(&("kind".into(),"migration_step".into())))).expect("projected row");
        let text = serde_json::to_string(row).unwrap();
        assert!(text.contains(&recorder.anonymizer().device(peer)));
        assert!(text.contains(&recorder.anonymizer().device(via)));
        assert!(text.contains("peer_committed"));
        assert!(text.contains("cutover_timeout"));
        let selection = rows.iter().find(|row| matches!(&row.event, Event::Log {labels,..} if labels.contains(&("reason".into(),"stale_counter".into())))).expect("selection transition survives projection");
        let text = serde_json::to_string(selection).unwrap();
        for field in [
            "connection_select",
            "waiting_on",
            "arbiter",
            "choice_counter",
            "choice_watermark",
            "local_boot",
            "peer_boot",
        ] {
            assert!(text.contains(field), "missing {field}");
        }
        let send = rows.iter().find(|row| matches!(&row.event, Event::Log {labels,..} if labels.contains(&("kind".into(),"udp_send".into())))).expect("UDP send error survives projection");
        let text = serde_json::to_string(send).unwrap();
        for field in ["kernel", "dropped", "host_unreachable", "os_error", "65"] {
            assert!(text.contains(field), "missing {field}");
        }
        recorder.shutdown();
        let mut store = store::Store::open(temp.path().join("store"), "a".repeat(24), true).unwrap();
        store.recover_for_test().unwrap();
        store.append(rows, 0).unwrap();
        let bundle = store.export(Reason::Manual, 0, 0).unwrap();
        let file = std::fs::File::open(bundle.path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        for i in 0..zip.len() {
            use std::io::Read;
            let mut bytes = Vec::new();
            zip.by_index(i).unwrap().read_to_end(&mut bytes).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            for private in ["PRIVATE", "203.0.113.7", &peer.to_hex(), &via.to_hex()] {
                assert!(!text.contains(private), "private value in ZIP");
            }
        }
    }

    #[test]
    fn source_hint_counter_and_class_keep_no_addresses() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_network::qos::source_hint::diagnostics",
                windows_source_hint_resends = 7_u64, destination_class = "ipv4_private",
                destination = "private-address", src_ip = "private-source", contents = "private-payload");
            tracing::info!(target: "oink_network::qos::source_hint::diagnostics",
                destination_class = "private-address");
        });
        let rows = lock(&rows);
        assert_eq!(rows[0].0, [("windows_source_hint_resends".into(), 7.0)]);
        assert_eq!(rows[0].1, [("destination_class".into(), "ipv4_private".into())]);
        assert!(rows[1].0.is_empty());
        assert!(rows[1].1.is_empty());
    }
    #[test]
    fn collection_and_receiver_counters_survive_without_payload_or_identity() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "oink_collection_diagnostics", kind="collection_end",
                collection_id=1_u64, role="sender", bytes=50_000_000_u64,
                duration_ms=15_000.0, limited_ms=14_000.0, segment_work_ms=100.0, success=true,
                peer="private-peer", path="private-path", payload="private-payload");
            tracing::info!(target: "oink_audio_diagnostics", kind="audio_receiver_sample",
                link_id=1_u64, stream_id=2_u64, sample_mono_us=300_u64,
                received=1000_u64, late=10_u64, concealed=10_u64,
                empty_queue_plc_frames=10_u64, gap_plc_frames=0_u64,
                decode_errors=0_u64, buffered_ms=10.0, arrival_jitter_ms=0.2,
                peer="private-peer", pcm="private-payload");
        });
        let rows = lock(&rows);
        assert_eq!(rows[0].0.len(), 6);
        assert_eq!(
            rows[0].1,
            [
                ("kind".into(), "collection_end".into()),
                ("role".into(), "sender".into())
            ]
        );
        assert_eq!(rows[1].0.len(), 11);
        assert_eq!(rows[1].1, [("kind".into(), "audio_receiver_sample".into())]);
        assert!(rows[1].0.contains(&("late".into(), 10.0)));
    }
    #[test]
    fn file_provider_phase_is_detailed_without_enabling_general_provider_debug() {
        assert!(detailed_callsite("oink_clipboard::macos::file_provider", |field| field == "dnd_phase"));
        assert!(!detailed_callsite("oink_clipboard::macos::file_provider", |_| false));
    }

    #[test]
    fn drag_phase_and_format_inventory_keep_only_numeric_metadata() {
        let values = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(values.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(
                dnd_phase = 1_u64,
                dnd_formats = 511_u64,
                dnd_formats_complete = true,
                elapsed_us = 750_000_u64,
                success = false,
                native_type = "private/custom-type",
                filename = "private-image.png",
                url = "https://private.invalid/image",
                "native drag metadata"
            );
        });
        let values = lock(&values);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0.len(), 5);
        assert!(values[0].1.is_empty());
        assert!(values[0].0.iter().any(|(k, v)| k == "dnd_formats" && *v == 511.));
    }
    #[test]
    fn ordered_drag_class_diagnostics_reject_names_payload_and_unreviewed_producers() {
        let values = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(values.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "oink_clipboard::macos::dnd::representation_trace",
                dnd_hold_sequence = 42_u64,
                dnd_representation_schema = 1_u64,
                dnd_representation_stage = 1_u64,
                dnd_representation_items = 2_u64,
                dnd_representation_complete = true,
                dnd_representation_classes = "FHIGLKCNOA;AF;",
                url = "https://secret.invalid", filename = "secret.jpg", native_type = "private.secret");
            for bad in ["public.jpeg", "https://secret.invalid", "private.jpg", "R;", "A\nB;"] {
                tracing::debug!(target: "oink_clipboard::macos::dnd::representation_trace",
                    dnd_representation_classes = bad);
            }
            tracing::debug!(target: "unreviewed", dnd_representation_classes = "AB;");
        });
        let values = lock(&values);
        assert_eq!(values[0].0.len(), 5);
        assert_eq!(
            values[0].1,
            [("dnd_representation_classes".into(), "FHIGLKCNOA;AF;".into())]
        );
        assert!(values[1..].iter().all(|row| row.1.is_empty()));
        assert!(detailed_callsite(
            "oink_clipboard::macos::dnd::representation_trace",
            |_| false
        ));
    }
    #[test]
    fn preview_geometry_keeps_scalars_without_pixels_or_text() {
        assert!(detailed_callsite("oink_clipboard::windows::drag_image", |field| field
            == "dnd_preview_stage"));
        assert!(!detailed_callsite("oink_clipboard::windows::drag_image", |_| false));
        let values = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(values.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(
                dnd_preview_stage = 1_u64,
                dnd_preview_source_w = 768_u64,
                dnd_preview_source_h = 96_u64,
                dnd_preview_png_w = 512_u64,
                dnd_preview_png_h = 64_u64,
                dnd_preview_width = 384.,
                dnd_preview_height = 48.,
                dnd_preview_hotspot_x = 48.,
                dnd_preview_hotspot_y = 12.,
                dnd_preview_scale = 2.,
                dnd_preview_source_image = true,
                png = "private image",
                text = "private words",
                filename = "private path",
                "artwork geometry"
            );
        });
        let values = lock(&values);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0.len(), 11);
        assert!(values[0].1.is_empty());
    }
    #[test]
    fn audio_level_scalars_preserve_digital_silence_and_exclude_source_identity() {
        // These source records are DEBUG so they do not wake UI log listeners;
        // the recorder must still retain their reviewed scalar measurements.
        assert!(detailed_callsite("oink_agent::audio::report", |field| field
            == "audio_level_window_sequence"));
        assert!(detailed_callsite("oink_agent::audio::link", |field| [
            "sample_mono_us",
            "concealed"
        ]
        .contains(&field)));
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(
                audio_level_window_sequence = 1_u64,
                audio_level_window_ms = 1000.0,
                pre_peak_dbfs = Some(-72.0),
                pre_rms_dbfs = Some(-78.0),
                pre_zero_frame_fraction = 0.5,
                pre_frames = 48000_u64,
                pre_nonfinite_samples = 0_u64,
                pre_digital_silence = false,
                post_measured = true,
                post_peak_dbfs = None::<f64>,
                post_rms_dbfs = None::<f64>,
                post_zero_frame_fraction = 1.0,
                post_frames = 48000_u64,
                post_nonfinite_samples = 0_u64,
                post_digital_silence = true,
                wasapi_buffers = Some(100_u64),
                wasapi_silent_buffers = Some(50_u64),
                source = "private-source-identity",
                "audio capture levels"
            );
        });
        let rows = lock(&rows);
        let fields: std::collections::BTreeMap<_, _> = rows[0].0.iter().cloned().collect();
        assert_eq!(fields.len(), 15);
        assert_eq!(fields["pre_peak_dbfs"], -72.0);
        assert_eq!(fields["post_digital_silence"], 1.0);
        assert_eq!(fields["wasapi_silent_buffers"], 50.0);
        assert!(!fields.contains_key("post_peak_dbfs"));
        assert!(!fields.contains_key("source"));
        assert!(rows[0].1.is_empty());
    }
    #[test]
    fn audio_lifecycle_survives_bundle_projection_without_peer_identity() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                phase = "source_silent",
                route_count = 4_u64,
                stream_id = 7_u64,
                peer_ready = false,
                stream_open = false,
                capture_running = false,
                source_muted = false,
                playback_running = false,
                playback_dormant = true,
                prepared_peers = 2_u64,
                connected_sources = 1_u64,
                sleeping = true,
                burst = 1_u64,
                startup_ms = 10.0,
                peer = "private-peer",
                "audio lifecycle"
            );
            tracing::info!(phase = "private-unknown-phase", peer = "private-peer");
        });
        let rows = lock(&rows);
        assert_eq!(rows[0].0.len(), 13);
        assert_eq!(rows[0].1, [("phase".into(), "source_silent".into())]);
        assert!(
            rows[0]
                .0
                .iter()
                .any(|(key, value)| key == "peer_ready" && *value == 0.0)
        );
        assert!(
            rows[0]
                .0
                .iter()
                .any(|(key, value)| key == "connected_sources" && *value == 1.0)
        );
        assert!(rows[1].0.is_empty());
        assert!(rows[1].1.is_empty());
    }
    #[test]
    fn keyboard_state_and_anomaly_projection_have_no_free_form_fields() {
        for (name, kind) in [
            ("diagnostic_caps", StateKind::CapsLock),
            ("diagnostic_secure", StateKind::SecureInput),
        ] {
            for active in [false, true] {
                let values = vec![(name.into(), f64::from(active)), ("key_code".into(), 458756.0)];
                let actual = keyboard_event("oink_input::diagnostics", &values).unwrap();
                let expected = Event::State {
                    kind,
                    active,
                    count: 0,
                    value: 0.0,
                };
                assert_eq!(
                    serde_json::to_vec(&actual).unwrap(),
                    serde_json::to_vec(&expected).unwrap()
                );
                assert!(keyboard_event("unreviewed_module", &values).is_none());
            }
        }
        let anomaly = keyboard_event("oink_input::diagnostics", &[("nonmodifier_flags".into(), 1.0)]).unwrap();
        assert_eq!(
            serde_json::to_value(anomaly).unwrap(),
            serde_json::json!({"KeyboardAnomaly": {"nonmodifier_flags": true}})
        );
    }

    #[test]
    fn capture_handoff_and_restore_fields_survive_without_identity() {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Collect(rows.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                reason = "hidden_during_remote_absolute_motion",
                hidden_motion_us = 250_000u64,
                remote_moves = 12u32,
                foreground = 987654321u64,
                "remote hidden pointer capture detected"
            );
            tracing::info!(
                reason = "cursor_visible",
                released_us = 200_000u64,
                "remote hidden pointer capture ended"
            );
            let mut detector = oink_input::pointer_capture::CaptureDetector::default();
            let sample = oink_input::pointer_capture::CaptureSample {
                windows: None,
                hidden: false,
                application_hidden: Some(false),
                confined: false,
                position: oink_foundation::Point::new(100.0, 100.0),
                motion_serial: 10,
                motion_observable: true,
                foreground: 987654321,
            };
            detector.update(Some(sample), true, true, 0);
            detector.update(
                Some(oink_input::pointer_capture::CaptureSample {
                    hidden: true,
                    application_hidden: Some(true),
                    motion_serial: 11,
                    ..sample
                }),
                true,
                true,
                100_000,
            );
            tracing::info!(
                reason = "pointer crossed edge",
                epoch = 77u64,
                peer = "secret-peer",
                "focus handoff requested"
            );
            tracing::debug!(in_callback = true, "restore_local");
            tracing::debug!(
                associate_called = true,
                associate_code = 0i32,
                show_called = true,
                show_code = 1001i32,
                flush_called = false,
                flush_code = 0i32,
                "restore cursor: native results"
            );
        });
        let rows = lock(&rows);
        let fields = rows
            .iter()
            .flat_map(|r| r.0.iter().cloned())
            .collect::<std::collections::BTreeMap<_, _>>();
        for key in [
            "hidden_motion_us",
            "remote_moves",
            "released_us",
            "detected",
            "hidden",
            "application_hidden",
            "motion_events",
            "stationary_us",
            "movement_samples",
            "epoch",
            "in_callback",
            "associate_code",
            "show_code",
            "flush_called",
        ] {
            assert!(fields.contains_key(key), "missing {key}");
        }
        assert_eq!(fields["show_code"], 1001.0);
        assert!(!fields.contains_key("foreground"));
        assert!(!fields.contains_key("peer"));
        let labels = rows
            .iter()
            .flat_map(|r| r.1.iter().map(|(_, v)| v.as_str()))
            .collect::<Vec<_>>();
        assert!(labels.contains(&"pointer crossed edge"));
        assert!(labels.contains(&"hidden_during_remote_absolute_motion"));
    }
    #[test]
    fn debug_projection_only_accepts_audited_scalar_grammar() {
        let module = "oink_input::macos::cursor";
        for (field, text, value) in [
            ("e0", "CGError(0)", 0.0),
            ("e1", "Some(CGError(-1000))", -1000.0),
            ("hidden", "Some(true)", 1.0),
            ("application_hidden", "Some(false)", 0.0),
            ("motion_events", "Some(42)", 42.0),
        ] {
            assert_eq!(debug_scalar(module, field, text), Some(value));
        }
        for (field, text) in [
            ("e0", "CGError(0) secret-user"),
            ("e1", "None"),
            ("hidden", "Some(secret-user)"),
            ("motion_events", "Some(42) password"),
            ("foreground", "123"),
            ("keycode", "458756"),
        ] {
            assert_eq!(debug_scalar(module, field, text), None);
        }
        assert_eq!(debug_scalar("external", "e0", "CGError(0)"), None);
        assert!(detailed_callsite(module, |_| false));
        assert!(detailed_callsite("oink_input::macos::tap", |f| f == "in_callback"));
        assert!(detailed_callsite("oink_input::macos::tap", |f| matches!(
            f,
            "at_ns" | "done_ns"
        )));
        assert!(!detailed_callsite("oink_input::macos::tap", |_| false));
    }

    #[test]
    fn cursor_visibility_version_label_is_narrowly_scoped() {
        let module = "oink_input::macos::cursor_visibility";
        for value in ["14.7.8", "26.6.2", "unknown"] {
            assert!(safe_os_version(module, value));
            assert!(!safe_os_version("external", value));
        }
        for value in [
            "",
            "/Users/private",
            "26.6.2 host-name",
            "26.6.2\n",
            "12345678901234567890123456789012345",
        ] {
            assert!(!safe_os_version(module, value));
        }
    }
}
