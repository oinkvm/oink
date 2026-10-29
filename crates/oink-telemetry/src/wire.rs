//! Hand-written Sentry envelope. One root per request satisfies both Sentry's
//! one-event-per-envelope rule and the design's upper bound of eight roots.
use crate::*;
use serde_json::{Value, json};
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
fn uuid(bytes: &[u8; 16]) -> String {
    let s = hex(bytes);
    format!("{}-{}-{}-{}-{}", &s[..8], &s[8..12], &s[12..16], &s[16..20], &s[20..])
}
pub fn encode(report: &AdmittedReport) -> Result<Vec<u8>> {
    report.validate()?;
    let build = report.report.build();
    let os = match build.os_version_class {
        OsVersionClass::Windows10 => "windows10".into(),
        OsVersionClass::Windows11 => "windows11".into(),
        OsVersionClass::MacOSMajor(v) => format!("macos_{v}"),
        OsVersionClass::Unknown => "unknown".into(),
    };
    let mut tags = json!({"schema_version":3,"policy_version":3,"os_version_class":os});
    let release = match report.source_revision {
        Some(revision) => format!(
            "com.hachiwii.oink@{}.{}.{}-beta.{}+{}",
            build.app_version.major,
            build.app_version.minor,
            build.app_version.patch,
            hex(&revision),
            build.app_version.build
        ),
        None => build.app_version.release(),
    };
    let mut event = json!({"event_id":hex(&report.event_id),"timestamp":report.occurrence_hour,"release":release,"platform":"native"});
    // Relay's legacy mode infers connection IPs for JavaScript even when user
    // and ip_address are missing/null. Use its explicit never-infer setting on
    // every report. This fixed protocol metadata does not enable an SDK.
    event["sdk"] = json!({"name":"oink.reports","version":"3.0.0","settings":{"infer_ip":"never"}});
    let kind = match &report.report {
        Report::SafeCrash(c) => {
            if c.component == Component::UiWebview {
                event["platform"] = json!("javascript");
            }
            tags["component"] = json!(c.component);
            tags["crash_kind"] = json!(c.crash_kind);
            let mut images = Vec::<Value>::new();
            let mut frames: Vec<Value> = c.frames().map(|f| match f {
                Frame::Native { image, offset, debug_id, debug_age, code_id, image_size } => {
                    let mut id = uuid(debug_id);
                    if *debug_age != 0 { id.push_str(&format!("-{debug_age:x}")); }
                    let index = images.iter().position(|i| i["debug_id"] == id).unwrap_or_else(|| {
                        let windows = code_id.is_some() || matches!(build.os_version_class, OsVersionClass::Windows10 | OsVersionClass::Windows11);
                        // Labels come exclusively from shipped-code enums. Relative mode
                        // identifies a debug image without exporting an ASLR load address.
                        let mut value = json!({"type":if windows {"pe"} else {"macho"}, "debug_id":id,
                            "code_file":image, "image_addr":"0x0", "image_size":image_size});
                        if let Some(code) = code_id {
                            value["code_id"] = json!(format!("{:08x}{:x}", code >> 32, code & 0xffff_ffff));
                        } else if !windows { value["code_id"] = json!(hex(debug_id)); }
                        images.push(value);
                        images.len()-1
                    });
                    json!({"instruction_addr":format!("0x{offset:x}"), "addr_mode":format!("rel:{index}"), "in_app":true})
                }
                Frame::Script { script: ScriptSlot::Main, line, column } =>
                    json!({"filename":"app:///main.js","lineno":line,"colno":column,"in_app":true}),
            }).collect();
            // Capture order is fault -> callers; Sentry expects oldest -> newest.
            frames.reverse();
            if !images.is_empty() {
                event["debug_meta"] = json!({"images":images});
            }
            event["level"] = json!("fatal");
            let mut exception = json!({"type":c.exception_class,"mechanism":{"type":"generic","handled":false}});
            // Sentry discards an empty `stacktrace.frames` as invalid and flags the
            // event with a processing error, so a crash without script frames
            // carries no stacktrace object at all.
            if !frames.is_empty() {
                // Windows unwind already adjusts PCs. macOS normalizes before
                // filtering; Relay must never adjust this shortened stack again.
                exception["stacktrace"] = json!({"frames":frames,"instruction_addr_adjustment":"none"});
            }
            event["exception"] = json!({"values":[exception]});
            event["contexts"] = json!({"oink_crash":{"type":"oink_crash","exception_code":c.exception_code,"stack_available":!frames.is_empty(),"callsite":c.callsite,"line":c.line,"column":c.column}});
            "event"
        }
        Report::MeasuredInterval(_) | Report::PerformanceDistribution(_) => {
            let c = report.report.context().ok_or(Error::Invalid)?;
            tags["metric"] = json!(c.metric.name());
            tags["revision"] = json!(1);
            tags["sampling_scheme"] = json!(SamplingScheme::IndependentV3);
            // Flatten only fields generated from the closed metric enum.
            if let Some(fields) = serde_json::to_value(c.metric).map_err(|_| Error::Invalid)?.as_object() {
                for (key, value) in fields {
                    if key != "metric" {
                        tags[key] = value.clone();
                    }
                }
            }
            if let Some(pair) = c.platform_pair {
                tags["source_platform"] = json!(pair.source);
                tags["destination_platform"] = json!(pair.destination);
            }
            if let Some(v) = c.network_path {
                tags["network_path"] = json!(v);
            }
            if let Some(v) = c.transport {
                tags["transport"] = json!(v);
            }
            if let Some(v) = c.codec {
                tags["codec"] = json!(v);
            }
            let op = if matches!(report.report, Report::MeasuredInterval(_)) {
                "oink.performance.interval"
            } else {
                "oink.performance.distribution"
            };
            let mut data = json!({});
            if let Report::PerformanceDistribution(d) = &report.report {
                data = json!({"sample_count":d.sample_count,"missing":d.missing,"saturated":d.saturated});
                if let Some(w) = d.window_us {
                    data["window_us"] = json!(w);
                }
                for (i, bin) in d.bins.iter().enumerate() {
                    data[format!("bin_{i:02}")] = json!(bin);
                }
                tags["histogram"] = json!(d.histogram);
            }
            event["type"] = json!("transaction");
            event["transaction"] = json!(c.metric.name());
            event["start_timestamp"] = json!(report.occurrence_hour);
            event["timestamp"] =
                json!(report.occurrence_hour as f64 + report.report.duration_us() as f64 / 1_000_000.0);
            event["contexts"] = json!({"trace":{"type":"trace","trace_id":hex(&report.trace_id),"span_id":hex(&report.span_id),"op":op,"data":data}});
            event["spans"] = json!([]);
            "transaction"
        }
    };
    event["tags"] = tags;
    let payload = serde_json::to_vec(&event).map_err(|_| Error::Invalid)?;
    let mut out = serde_json::to_vec(&json!({"event_id":hex(&report.event_id)})).map_err(|_| Error::Invalid)?;
    out.push(b'\n');
    out.extend(
        serde_json::to_vec(&json!({"type":kind,"length":payload.len(),"content_type":"application/json"}))
            .map_err(|_| Error::Invalid)?,
    );
    out.push(b'\n');
    out.extend(payload);
    out.push(b'\n');
    if out.len() > if report.report.crash() { 32768 } else { 8192 } {
        return Err(Error::Limit);
    }
    Ok(out)
}
/// Independently decode persisted input with deny_unknown_fields and regenerate
/// the entire envelope. No stored SDK/envelope bytes are ever trusted or forwarded.
pub fn encode_persisted(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > 32768 {
        return Err(Error::Limit);
    }
    let raw: Value = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
    let report: AdmittedReport = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
    // Serde's unit variants can otherwise ignore extra map fields. An exact
    // typed round-trip closes that gap independently of derive behavior.
    if serde_json::to_value(&report).map_err(|_| Error::Invalid)? != raw {
        return Err(Error::Invalid);
    }
    encode(&report)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_drag_intervals_reach_sentry_with_fixed_stage_and_boundary_tags() {
        for metric in [
            Metric::DragSourceStage {
                stage: SourceStage::ProbeRecognition,
            },
            Metric::DragSourceStage {
                stage: SourceStage::NativeRead,
            },
            Metric::DragSourceStage {
                stage: SourceStage::MetadataCompletion,
            },
            Metric::DragSourceStage {
                stage: SourceStage::Inventory,
            },
            Metric::DragSourceStage {
                stage: SourceStage::InventoryToLearned,
            },
            Metric::DragSourceStage {
                stage: SourceStage::LearnedToNotice,
            },
            Metric::DragSourceStage {
                stage: SourceStage::RecognizedToNotice,
            },
            Metric::DragTargetStart {
                boundary: TargetStartBoundary::AppkitSessionReturned,
            },
            Metric::DragTargetStart {
                boundary: TargetStartBoundary::OleCallEnter,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::Metadata,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::EntryToBegin,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::TargetPreparation,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::PreparationQueue,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::BeginDispatch,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::PendingWindow,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::SourcePress,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::NativeAcknowledgement,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::WritersPreview,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::NativeBegin,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::Preview,
            },
            Metric::DragPrepareStage {
                stage: PrepareStage::Namespace,
            },
        ] {
            let report = Report::MeasuredInterval(MeasuredInterval {
                context: MeasurementContext { metric, ..context() },
                elapsed_us: 123_456,
            });
            assert_eq!(metric.family(), Family::Drag);
            let bytes = encode(&AdmittedReport::new(report, 12345678).unwrap()).unwrap();
            let event: Value = serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(2).unwrap()).unwrap();
            assert_eq!(event["tags"]["metric"], metric.name());
            let fields = serde_json::to_value(metric).unwrap();
            for (key, value) in fields.as_object().unwrap() {
                assert_eq!(event["tags"][key], *value);
            }
            assert!(event.get("user").is_none());
            assert!(event.get("spans").unwrap().as_array().unwrap().is_empty());
        }
    }
    #[test]
    fn native_frames_are_symbolicatable_without_absolute_addresses_or_paths() {
        let mut crash = SafeCrash::empty(
            context().build,
            Component::Agent,
            CrashKind::NativeException,
            ExceptionClass::BadAccess,
        );
        crash.frames[0][0] = Some(Frame::Native {
            image: ImageSlot::Agent,
            offset: 24,
            debug_id: [1; 16],
            debug_age: 0,
            code_id: None,
            image_size: 4096,
        });
        crash.frames[0][1] = Some(Frame::Native {
            image: ImageSlot::Agent,
            offset: 48,
            debug_id: [1; 16],
            debug_age: 0,
            code_id: None,
            image_size: 4096,
        });
        let bytes = encode(&AdmittedReport::new(Report::SafeCrash(crash), 3600).unwrap()).unwrap();
        let event: Value = serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(2).unwrap()).unwrap();
        assert_eq!(
            event["debug_meta"]["images"][0]["debug_id"],
            "01010101-0101-0101-0101-010101010101"
        );
        let frames = event["exception"]["values"][0]["stacktrace"]["frames"]
            .as_array()
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            event["exception"]["values"][0]["stacktrace"]["instruction_addr_adjustment"],
            "none"
        );
        assert_eq!(frames[0]["instruction_addr"], "0x30");
        assert_eq!(frames[0]["addr_mode"], "rel:0");
        assert!(event["contexts"]["oink_crash"].get("native_frames").is_none());
    }
    #[test]
    fn pe_frames_keep_pdb_age_and_code_identity() {
        let mut crash = SafeCrash::empty(
            BuildContext {
                os_version_class: OsVersionClass::Windows11,
                ..context().build
            },
            Component::Agent,
            CrashKind::NativeException,
            ExceptionClass::BadAccess,
        );
        crash.frames[0][0] = Some(Frame::Native {
            image: ImageSlot::Agent,
            offset: 24,
            debug_id: [1; 16],
            debug_age: 0x1a,
            code_id: Some(0x12345678_00004000),
            image_size: 4096,
        });
        let admitted = AdmittedReport::new(Report::SafeCrash(crash), 3600).unwrap();
        let bytes = encode(&admitted).unwrap();
        let event: Value = serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(2).unwrap()).unwrap();
        assert_eq!(event["debug_meta"]["images"][0]["type"], "pe");
        assert_eq!(
            event["debug_meta"]["images"][0]["debug_id"],
            "01010101-0101-0101-0101-010101010101-1a"
        );
        assert_eq!(event["debug_meta"]["images"][0]["code_id"], "123456784000");
        let mut poison = serde_json::to_value(&admitted).unwrap();
        poison["report"]["data"]["frames"][0][0]["module_path"] = json!("PRIVATE");
        assert!(encode_persisted(&serde_json::to_vec(&poison).unwrap()).is_err());
    }
    #[test]
    fn queued_crash_retains_its_source_suffix_independently_of_the_sender_build() {
        let mut admitted = AdmittedReport::new(
            Report::SafeCrash(SafeCrash::empty(
                context().build,
                Component::Agent,
                CrashKind::RustPanic,
                ExceptionClass::Panic,
            )),
            3600,
        )
        .unwrap();
        admitted.source_revision = Some([0x01, 0x23, 0xab, 0xcd]);
        let persisted = serde_json::to_vec(&admitted).unwrap();
        let restored: AdmittedReport = serde_json::from_slice(&persisted).unwrap();
        let bytes = encode(&restored).unwrap();
        let event: Value = serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(2).unwrap()).unwrap();
        assert_eq!(event["release"], "com.hachiwii.oink@1.2.3-beta.0123abcd+4");
        assert_eq!(encode_persisted(&persisted).unwrap(), bytes);
    }
    fn context() -> MeasurementContext {
        MeasurementContext {
            build: BuildContext {
                app_version: AppVersion {
                    major: 1,
                    minor: 2,
                    patch: 3,
                    build: 4,
                },
                os_version_class: OsVersionClass::MacOSMajor(14),
            },
            metric: Metric::InputQueueWaitWindowMax,
            platform_pair: None,
            network_path: None,
            transport: None,
            codec: None,
        }
    }
    #[test]
    fn clipboard_and_drop_records_only_emit_closed_performance_fields() {
        let metrics = [
            Metric::ClipboardPublish,
            Metric::ClipboardApply {
                result: OperationResult::Completed,
            },
            Metric::NativeRepresentationRead {
                result: OperationResult::TimedOut,
            },
            Metric::ClipboardTransfer {
                result: OperationResult::Failed,
                reason: TransferFailure::Permission,
            },
            Metric::FileFirstByte {
                size: SizeBucket::UpTo1MiB,
            },
            Metric::FileComplete {
                result: OperationResult::Completed,
                reason: TransferFailure::None,
                size: SizeBucket::UpTo16MiB,
                throughput: ThroughputBucket::UpTo10MiBps,
            },
            Metric::DragDropAccepted {
                result: OperationResult::Cancelled,
            },
            Metric::DragTransfer {
                result: OperationResult::Failed,
                reason: TransferFailure::Io,
            },
        ];
        for metric in metrics {
            let report = Report::MeasuredInterval(MeasuredInterval {
                context: MeasurementContext { metric, ..context() },
                elapsed_us: 123_456,
            });
            let admitted = AdmittedReport::new(report, 1_234_567).unwrap();
            let spool = serde_json::to_vec(&admitted).unwrap();
            let bytes = encode_persisted(&spool).unwrap();
            let event: Value = serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(2).unwrap()).unwrap();
            assert_eq!(event["type"], "transaction");
            assert_eq!(event["transaction"], metric.name());
            assert_eq!(event["spans"], json!([]));
            assert_eq!(event["contexts"]["trace"]["data"], json!({}));
            let tags = event["tags"].as_object().unwrap();
            assert!(tags.keys().all(|key| matches!(
                key.as_str(),
                "schema_version"
                    | "policy_version"
                    | "os_version_class"
                    | "metric"
                    | "revision"
                    | "sampling_scheme"
                    | "result"
                    | "reason"
                    | "size"
                    | "throughput"
            )));
            assert!(event.get("user").is_none());
            for (field, poison) in [
                ("name", "private-file.txt"),
                ("path", "/Users/private/secret"),
                ("hash", "0123456789abcdef"),
                ("device_id", "private-peer"),
                ("content", "private clipboard contents"),
                ("error", "arbitrary error text"),
                ("mime", "private/format"),
                ("bytes", "123456789"),
            ] {
                let mut forged = serde_json::to_value(&admitted).unwrap();
                forged["report"]["data"]["context"]["metric"][field] = json!(poison);
                assert!(
                    encode_persisted(&serde_json::to_vec(&forged).unwrap()).is_err(),
                    "{field}"
                );
                assert!(!String::from_utf8_lossy(&bytes).contains(poison));
            }
        }
    }

    #[test]
    fn every_variant_has_only_event_or_transaction_no_children_no_runtime_strings() {
        let reports = [
            Report::SafeCrash(SafeCrash::empty(
                context().build,
                Component::Agent,
                CrashKind::RustPanic,
                ExceptionClass::Panic,
            )),
            Report::SafeCrash(SafeCrash::empty(
                context().build,
                Component::UiWebview,
                CrashKind::JsUnhandled,
                ExceptionClass::Unhandled,
            )),
            Report::MeasuredInterval(MeasuredInterval {
                context: MeasurementContext {
                    metric: Metric::DragQueueWait,
                    ..context()
                },
                elapsed_us: 12345,
            }),
            Report::PerformanceDistribution(PerformanceDistribution {
                context: context(),
                histogram: HistogramSchema::Duration14,
                bins: crate::sampling::singleton(8000),
                sample_count: 1,
                missing: 0,
                saturated: false,
                window_us: Some(1_000_000),
                seal_us: 1,
            }),
        ];
        for report in reports {
            let admitted = AdmittedReport::new(report, 12345678).unwrap();
            let bytes = encode(&admitted).unwrap();
            let lines: Vec<_> = bytes.split(|b| *b == b'\n').collect();
            assert_eq!(lines.len(), 4);
            let header: Value = serde_json::from_slice(lines[1]).unwrap();
            assert!(matches!(header["type"].as_str(), Some("event" | "transaction")));
            let event: Value = serde_json::from_slice(lines[2]).unwrap();
            // Missing/null user.ip_address permits legacy IP inference for
            // JavaScript events. Every report must explicitly disable it.
            assert_eq!(
                event["sdk"],
                json!({"name":"oink.reports","version":"3.0.0","settings":{"infer_ip":"never"}})
            );
            for key in [
                "user",
                "server_name",
                "request",
                "breadcrumbs",
                "message",
                "logentry",
                "environment",
                "threads",
                "attachments",
                "session",
                "profile",
                "replay_id",
            ] {
                assert!(event.get(key).is_none(), "{key}");
            }
            if !admitted.report.crash() {
                assert_eq!(event["spans"], json!([]));
                assert_eq!(event["start_timestamp"], 12344400);
            }
            assert_eq!(event["release"], "com.hachiwii.oink@1.2.3+4");
            if admitted.report.crash() {
                assert_eq!(event["level"], "fatal");
                assert_eq!(event["exception"]["values"][0]["mechanism"]["handled"], false);
            }
            // Every possible object location rejects injected free-string fields
            // when crossing the independently checked persisted encoder boundary.
            fn poison(value: &Value) -> Vec<Value> {
                let mut result = Vec::new();
                match value {
                    Value::Object(map) => {
                        let mut v = value.clone();
                        v["arbitrary_user_path"] = json!("poison-content");
                        result.push(v);
                        for (k, child) in map {
                            for changed in poison(child) {
                                let mut v = value.clone();
                                v[k] = changed;
                                result.push(v);
                            }
                        }
                    }
                    Value::Array(array) => {
                        for (i, child) in array.iter().enumerate() {
                            for changed in poison(child) {
                                let mut v = value.clone();
                                v[i] = changed;
                                result.push(v);
                            }
                        }
                    }
                    _ => {}
                }
                result
            }
            for poisoned in poison(&serde_json::to_value(&admitted).unwrap()) {
                assert!(encode_persisted(&serde_json::to_vec(&poisoned).unwrap()).is_err());
            }
        }
    }
    #[test]
    fn crash_without_script_frames_omits_the_empty_stacktrace() {
        let report = Report::SafeCrash(SafeCrash::empty(
            context().build,
            Component::Agent,
            CrashKind::RustPanic,
            ExceptionClass::Panic,
        ));
        let bytes = encode(&AdmittedReport::new(report, 12345678).unwrap()).unwrap();
        let event: Value = serde_json::from_slice(bytes.split(|b| *b == b'\n').nth(2).unwrap()).unwrap();
        let exception = &event["exception"]["values"][0];
        assert_eq!(exception["type"], "panic");
        assert!(exception.get("stacktrace").is_none(), "{exception}");
    }
    #[test]
    fn offsets_never_become_intervals_and_histogram_schemas_are_metric_specific() {
        for stage in [
            PrepareStage::ParentVisible,
            PrepareStage::ChildrenEnumerated,
            PrepareStage::FirstUrl,
        ] {
            let report = Report::MeasuredInterval(MeasuredInterval {
                context: MeasurementContext {
                    metric: Metric::DragPrepareStage { stage },
                    ..context()
                },
                elapsed_us: 10,
            });
            assert!(report.validate().is_err());
        }
        let mut distribution = PerformanceDistribution {
            context: context(),
            histogram: HistogramSchema::Duration14,
            bins: sampling::singleton(100),
            sample_count: 1,
            missing: 0,
            saturated: false,
            window_us: Some(1000),
            seal_us: 1,
        };
        assert!(Report::PerformanceDistribution(distribution).validate().is_ok());
        distribution.context.metric = Metric::InputIngressVariation;
        assert!(Report::PerformanceDistribution(distribution).validate().is_err());
        distribution.context.metric = Metric::DragQueueWait;
        assert!(Report::PerformanceDistribution(distribution).validate().is_err());
    }

    #[test]
    fn numeric_validation_rejects_impossible_histogram_and_non_interval_metric() {
        let bad = Report::MeasuredInterval(MeasuredInterval {
            context: context(),
            elapsed_us: 100,
        });
        assert!(bad.validate().is_err());
        let mut d = PerformanceDistribution {
            context: context(),
            histogram: HistogramSchema::Jitter6,
            bins: [1; 14],
            sample_count: 14,
            missing: 0,
            saturated: false,
            window_us: None,
            seal_us: 1,
        };
        assert!(Report::PerformanceDistribution(d).validate().is_err());
        d.bins = [0; 14];
        assert!(Report::PerformanceDistribution(d).validate().is_err());
        assert!(serde_json::from_str::<Report>(r#"{"report":"session","data":{}}"#).is_err());
        assert!(
            serde_json::from_str::<AppVersion>(r#"{"major":"poison-name","minor":0,"patch":0,"build":0}"#).is_err()
        );
    }
}
