//! Application-focus evidence leaves the realtime owner through the bounded
//! InputTrace queue. Formatting tracing there is intentionally disabled.
use super::{Event, Recorder};
use oink_routing::application_focus::Diagnostic;

pub(super) fn export(record: Diagnostic, recorder: &Recorder) -> Event {
    let mut fields = Vec::new();
    let mut labels = Vec::new();
    let kind = match record {
        Diagnostic::State {
            epoch,
            revision,
            pointer,
            keyboard,
            phase,
            reason,
        } => {
            fields.extend([("epoch".into(), epoch as f64), ("revision".into(), revision as f64)]);
            labels.push(("pointer".into(), recorder.anonymizer().device(pointer)));
            if let Some(keyboard) = keyboard {
                labels.push(("keyboard".into(), recorder.anonymizer().device(keyboard)));
            }
            labels.extend([
                ("phase".into(), format!("{phase:?}")),
                ("reason".into(), format!("{reason:?}")),
            ]);
            "state"
        }
        Diagnostic::Observation {
            epoch,
            observer,
            observation,
            accepted,
        } => {
            fields.extend([
                ("epoch".into(), epoch as f64),
                ("generation".into(), observation.stamp.generation as f64),
                ("sequence".into(), observation.stamp.sequence as f64),
                ("hidden_lost".into(), f64::from(observation.hidden_lost)),
                ("accepted".into(), f64::from(accepted)),
            ]);
            if let Some(operation) = observation.park_operation {
                fields.push(("operation".into(), operation as f64));
            }
            labels.extend([
                ("observer".into(), recorder.anonymizer().device(observer)),
                ("foreground".into(), format!("{:?}", observation.foreground)),
            ]);
            "observation"
        }
        Diagnostic::Park {
            epoch,
            revision,
            operation,
            target,
            expected,
            stage,
            result,
        } => {
            fields.extend([
                ("epoch".into(), epoch as f64),
                ("revision".into(), revision as f64),
                ("operation".into(), operation as f64),
                ("generation".into(), expected.generation as f64),
                ("sequence".into(), expected.sequence as f64),
            ]);
            labels.extend([
                ("target".into(), recorder.anonymizer().device(target)),
                ("stage".into(), format!("{stage:?}")),
            ]);
            if let Some(result) = result {
                labels.push(("result".into(), format!("{result:?}")));
            }
            "park"
        }
        Diagnostic::Dismiss { epoch } => {
            fields.push(("epoch".into(), epoch as f64));
            "dismiss"
        }
        Diagnostic::Key {
            epoch,
            revision,
            destination,
            stage,
        } => {
            fields.extend([("epoch".into(), epoch as f64), ("revision".into(), revision as f64)]);
            labels.extend([
                ("destination".into(), recorder.anonymizer().device(destination)),
                ("stage".into(), format!("{stage:?}")),
            ]);
            "key_route"
        }
    };
    labels.push(("kind".into(), kind.into()));
    Event::Log {
        module: "oink_application_focus".into(),
        line: 0,
        level: 2,
        fields,
        labels,
    }
}
