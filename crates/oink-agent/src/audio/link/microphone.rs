//! Microphone ownership is serialized by the existing audio control worker.
use super::*;
use dto::{ControlFailure as Failure, LocalOption};

#[derive(Default)]
pub(super) struct State {
    pub native: bool,
    #[cfg(any(target_os = "macos", windows))]
    runtime: Option<Runtime>,
    /// The microphone sender register as this device executes it.
    pub resolution: Resolution,
    /// The local shared-microphone capture runs Case A AEC against this
    /// device's exit output, so the exit keeps its output open through
    /// source silence (`tick_exit`). Refreshed after every runtime call.
    pub echo_holds_exit: bool,
}
impl Owner {
    fn microphone_members(&self) -> BTreeSet<DeviceId> {
        self.members()
            .into_iter()
            .filter(|peer| {
                *peer == self.links.id()
                    || self
                        .group
                        .peer_permits(*peer, oink_protocol::v2::registry::type_id::MicrophoneDemand)
            })
            .collect()
    }
    /// Resolve the microphone sender register (see `apply_locked`). A sender
    /// that is online but asleep, paused or cannot capture is "unavailable";
    /// receivers wait for it and never pick another sender. An offline one is
    /// no sender at all (it dropped out of the overview).
    pub(super) fn apply_microphone(&mut self, available: &BTreeSet<DeviceId>) {
        if self.active_group.is_none() {
            self.microphone.resolution = Resolution::None;
            return;
        }
        let members = self.microphone_members();
        let available: BTreeSet<_> = available.intersection(&members).copied().collect();
        self.microphone.resolution = self.audio.resolve(Role::Microphone, &available);
    }
    /// The virtual microphone is installed, so this device can receive.
    pub(super) fn virtual_microphone_ready(&self) -> bool {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.virtual_ready();
        }
        false
    }
    pub(super) fn set_microphone_option(
        &mut self,
        option: LocalOption,
        expected: u64,
        patch: RawValue,
    ) -> Result<OwnerOptionApplied, Failure> {
        let before = lock(&self.store)
            .owner()
            .item(option)
            .ok_or(Failure::UnsupportedOption)?;
        if before.revision() != Ok(expected) {
            return Err(Failure::StaleRevision);
        }
        lock(&self.store).edit_owner(option, expected, patch)?;
        let published = self.publish_preferences();
        Ok(self.option_receipt(option, &before, published))
    }
}

#[cfg(any(target_os = "macos", windows))]
mod runtime;
#[cfg(any(target_os = "macos", windows))]
use runtime::Runtime;
impl Owner {
    pub(super) fn microphone_inputs(&self) -> Vec<oink_audio::microphone::InputDevice> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.selectable_inputs();
        }
        Vec::new()
    }
    pub(super) fn microphone_active(&self) -> bool {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.active();
        }
        false
    }
    pub(super) fn microphone_live(&self) -> bool {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.live();
        }
        false
    }
    pub(super) fn output_quality_snapshot(
        &self,
        at_ms: u64,
    ) -> [Option<oink_audio::sink::DropoutBucket>; oink_audio::sink::QUALITY_SECONDS] {
        let mut output = self.output_quality.snapshot(at_ms);
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            for bucket in runtime.output_quality(at_ms).into_iter().flatten() {
                let index = (bucket.at_ms / 1000) as usize % output.len();
                if let Some(old) = output[index].as_mut().filter(|old| old.at_ms == bucket.at_ms) {
                    old.add(bucket);
                } else {
                    output[index] = Some(bucket);
                }
            }
        }
        output
    }
    pub(super) fn microphone_quality(&self, now: Instant) -> Option<crate::audio_quality::Observation> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.quality(now);
        }
        None
    }
    pub(super) fn microphone_deadline(&self) -> Option<Instant> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(runtime) = &self.microphone.runtime {
            return runtime.deadline();
        }
        None
    }
    pub(super) fn tick_microphone(&mut self) {
        if self.driver_change_active() {
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        if self.microphone.native {
            let mut runtime = self.microphone.runtime.take().unwrap_or_default();
            runtime.tick(self);
            self.microphone.echo_holds_exit = runtime.echo_holds_exit();
            self.microphone.runtime = Some(runtime);
        }
    }
    pub(super) fn refresh_microphone(&mut self) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(mut runtime) = self.microphone.runtime.take() {
            runtime.refresh(self);
            self.microphone.echo_holds_exit = runtime.echo_holds_exit();
            self.microphone.runtime = Some(runtime);
        }
    }
    pub(super) fn stop_microphone(&mut self) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(mut runtime) = self.microphone.runtime.take() {
            runtime.stop(self);
            self.microphone.echo_holds_exit = runtime.echo_holds_exit();
            self.microphone.runtime = Some(runtime);
        }
    }
    pub(super) fn microphone_message(&mut self, peer: DeviceId, message: &Message) {
        if self.driver_change_active() {
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        if self.microphone.native {
            let mut runtime = self.microphone.runtime.take().unwrap_or_default();
            runtime.message(self, peer, message);
            self.microphone.echo_holds_exit = runtime.echo_holds_exit();
            self.microphone.runtime = Some(runtime);
        }
    }
}
