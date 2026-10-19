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
            let frames: Vec<Value> = c
                .frames()
                .filter_map(|f| match f {
                    // Relative native addresses require validated symbolication. Until
                    // that release gate is met, retain numeric code data as attributes,
                    // never invent synthetic load addresses or absolute PCs.
                    Frame::Native { .. } => None,
                    Frame::Script {
                        script: ScriptSlot::Main,
                        line,
                        column,
                    } => Some(json!({"filename":"app:///main.js","lineno":line,"colno":column,"in_app":true})),
                })
                .collect();
            event["level"] = json!("fatal");
            let mut exception = json!({"type":c.exception_class,"mechanism":{"type":"generic","handled":false}});
            // Sentry discards an empty `stacktrace.frames` as invalid and flags the
            // event with a processing error, so a crash without script frames
            // carries no stacktrace object at all.
            if !frames.is_empty() {
                exception["stacktrace"] = json!({"frames":frames});
            }
            event["exception"] = json!({"values":[exception]});
            let native: Vec<Value> = c
                .frames()
                .filter_map(|f| match f {
                    Frame::Native {
                        image,
                        offset,
                        debug_id,
                    } => Some(json!({"image":image,"offset":offset,"debug_id":uuid(debug_id)})),
                    _ => None,
                })
                .collect();
            event["contexts"] = json!({"oink_crash":{"type":"oink_crash","exception_code":c.exception_code,"stack_available":!native.is_empty() || !frames.is_empty(),"native_frames":native,"callsite":c.callsite,"line":c.line,"column":c.column}});
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
