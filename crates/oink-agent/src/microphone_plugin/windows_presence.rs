//! Windows presence and readiness from one inspection. Driver-store/service
//! residue alone is intentionally not an input: it cannot supply microphone IO.
use super::*;
use oink_audio::microphone::VirtualOutput;

pub(super) fn apply(inspection: &mut Inspection, registered: bool, managed: bool, output: VirtualOutput) {
    let endpoint_exists = matches!(&output, VirtualOutput::Ready { .. } | VirtualOutput::Ambiguous(_));
    inspection.render_id = match output {
        VirtualOutput::Ready { render_id, .. } => Some(render_id),
        _ => None,
    };
    let ready = inspection.render_id.is_some();
    let installed = registered || endpoint_exists;
    let status = &mut inspection.status;
    status.managed = installed && managed;
    status.input_loaded = ready;
    status.feed_loaded = ready;
    status.state = if !installed {
        MicrophonePluginState::NotInstalled
    } else if !status.managed {
        MicrophonePluginState::Unmanaged
    } else if !ready {
        MicrophonePluginState::NotLoaded
    } else {
        MicrophonePluginState::Installed
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> VirtualOutput {
        VirtualOutput::Ready {
            render_id: "fixture-render".into(),
            recording_name: "fixture-recording".into(),
        }
    }
    #[test]
    fn removed_device_with_retained_package_is_not_installed() {
        // Regression fixture: no present PnP device, no MMDevice endpoints;
        // stopped service and staged INF remain after vendor -u and reboot.
        let mut snapshot = Inspection::default();
        apply(&mut snapshot, false, false, VirtualOutput::Missing);
        assert_eq!(snapshot.status.state, MicrophonePluginState::NotInstalled);
        assert!(!snapshot.status.managed && !snapshot.status.input_loaded && !snapshot.status.feed_loaded);
        assert_eq!(snapshot.render_id, None);
    }
    #[test]
    fn settings_and_runtime_share_the_same_ready_endpoint() {
        let mut snapshot = Inspection::default();
        apply(&mut snapshot, true, true, ready());
        assert_eq!(snapshot.status.state, MicrophonePluginState::Installed);
        assert!(snapshot.status.input_loaded && snapshot.status.feed_loaded);
        assert_eq!(snapshot.render_id.as_deref(), Some("fixture-render"));
        apply(&mut snapshot, false, false, VirtualOutput::Missing);
        assert_eq!(snapshot.status.state, MicrophonePluginState::NotInstalled);
        assert!(!snapshot.status.feed_loaded);
        assert_eq!(snapshot.render_id, None);
    }
    #[test]
    fn present_disabled_and_unmanaged_devices_are_not_automatically_claimed() {
        let mut snapshot = Inspection::default();
        apply(&mut snapshot, true, true, VirtualOutput::Missing);
        assert_eq!(snapshot.status.state, MicrophonePluginState::NotLoaded);
        assert!(snapshot.status.managed);
        assert_eq!(snapshot.render_id, None);
        apply(&mut snapshot, true, false, VirtualOutput::Missing);
        assert_eq!(snapshot.status.state, MicrophonePluginState::Unmanaged);
        apply(&mut snapshot, false, false, ready());
        assert_eq!(snapshot.status.state, MicrophonePluginState::Unmanaged);
        assert!(!snapshot.status.managed);
    }
    #[test]
    fn ambiguous_endpoints_are_present_but_not_ready() {
        let mut snapshot = Inspection::default();
        let endpoints = ["one", "two"].map(|id| oink_audio::microphone::InputDevice {
            id: id.into(),
            name: id.into(),
        });
        apply(&mut snapshot, false, false, VirtualOutput::Ambiguous(endpoints.into()));
        assert_eq!(snapshot.status.state, MicrophonePluginState::Unmanaged);
        assert!(!snapshot.status.feed_loaded && !snapshot.status.input_loaded);
        assert_eq!(snapshot.render_id, None);
    }
}
