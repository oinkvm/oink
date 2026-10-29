use super::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

fn descriptor(device: &cpal::Device) -> Result<InputDevice, String> {
    Ok(InputDevice {
        id: device.id().map_err(|e| e.to_string())?.to_string(),
        name: device_name(device)?,
    })
}

fn device_name(device: &cpal::Device) -> Result<String, String> {
    let description = device.description().map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    return Ok(description.name().to_owned());
    #[cfg(not(target_os = "macos"))]
    Ok(crate::device_name(&description).to_owned())
}

/// Query the OS default without opening an input stream. The owner freezes the
/// returned stable ID for the session; unplugging it never captures another mic.
pub fn default_input() -> Result<Option<InputDevice>, String> {
    #[cfg(target_os = "macos")]
    return crate::macos_devices::default_input();
    #[cfg(not(target_os = "macos"))]
    cpal::default_host()
        .default_input_device()
        .as_ref()
        .map(descriptor)
        .transpose()
}
pub fn inputs() -> Result<Vec<InputDevice>, String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos_devices::inputs()
    }
    #[cfg(not(target_os = "macos"))]
    {
        cpal::default_host()
            .input_devices()
            .map_err(|e| e.to_string())?
            .map(|device| descriptor(&device))
            .collect()
    }
}

/// Enumerates only; no playback stream is opened and no defaults are changed.
pub fn virtual_output() -> Result<VirtualOutput, String> {
    #[cfg(target_os = "macos")]
    {
        Ok(match super::macos::device_id(MAC_FEED_UID)? {
            Some(_) => VirtualOutput::Ready {
                render_id: format!("coreaudio:{MAC_FEED_UID}"),
                recording_name: "Oink Microphone".into(),
            },
            None => VirtualOutput::Missing,
        })
    }
    #[cfg(windows)]
    {
        let mut matches = Vec::new();
        for device in cpal::default_host().output_devices().map_err(|e| e.to_string())? {
            let description = descriptor(&device)?;
            if description.name == "CABLE Input (VB-Audio Virtual Cable)" {
                matches.push(description);
            }
        }
        Ok(match matches.len() {
            0 => VirtualOutput::Missing,
            1 => VirtualOutput::Ready {
                render_id: matches.remove(0).id,
                recording_name: "CABLE Output (VB-Audio Virtual Cable)".into(),
            },
            _ => VirtualOutput::Ambiguous(matches),
        })
    }
}

#[derive(Default)]
struct Health {
    closing: AtomicBool,
    invalidated: AtomicBool,
}
pub struct Capture {
    // Option permits an explicit stop which releases the device, rather than pause.
    stream: Option<cpal::Stream>,
    info: CaptureInfo,
    health: Arc<Health>,
    service_generation: Option<u64>,
    error: Arc<Mutex<Option<String>>>,
}
impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCapture")
            .field("info", &self.info)
            .field("running", &self.stream.is_some())
            .finish()
    }
}
impl Capture {
    pub fn start(id: &str, permit: StartPermit, encoder: crate::source::Encoder) -> Result<Self, String> {
        permit.check()?;
        match permission() {
            Permission::Granted | Permission::SystemControlled => (),
            state => {
                return Err(format!(
                    "microphone permission is {state:?}; authorization requires user action"
                ));
            }
        }
        let service_generation = crate::device_notifications::service_generation();
        // No implicit default-device fallback, including after unplug or rename.
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.id().is_ok_and(|device_id| device_id.to_string() == id))
            .ok_or("selected microphone is unavailable")?;
        let device_info = descriptor(&device)?;
        let forwarding_loopback =
            device_info.is_forwarding_loopback() || device_info.name.contains("VB-Audio Virtual Cable");
        #[cfg(windows)]
        let forwarding_loopback =
            forwarding_loopback || super::windows_default::is_forwarding_endpoint(id).unwrap_or(false);
        if forwarding_loopback {
            return Err("select a hardware microphone, not a forwarding loopback device".into());
        }
        // Native default format: do not change the physical device's sample rate.
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        let rate = config.sample_rate();
        let channels = config.channels();
        if !(8_000..=192_000).contains(&rate) || channels == 0 || channels > 32 {
            return Err(format!(
                "unsupported microphone format: {rate} Hz / {channels} channels"
            ));
        }
        let info = CaptureInfo {
            device: device_info,
            backend: if cfg!(windows) {
                "wasapi_shared"
            } else {
                "coreaudio_input"
            },
            hardware_sample_rate: rate,
            hardware_channels: channels,
            resampler_delay_frames: (32_u64 * 48_000).div_ceil(u64::from(rate)) as u32,
        };
        let error = Arc::new(Mutex::new(None));
        let health = Arc::new(Health::default());
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config.into(), encoder, error.clone(), health.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config.into(), encoder, error.clone(), health.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config.into(), encoder, error.clone(), health.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config.into(), encoder, error.clone(), health.clone()),
            format => return Err(format!("unsupported microphone sample format: {format}")),
        }?;
        let capture = Self {
            stream: Some(stream),
            info,
            health,
            service_generation,
            error,
        };
        // Establish the owner before play: a reset during startup must use the
        // same conservative retirement as a reset of an already running stream.
        capture.stream.as_ref().unwrap().play().map_err(|e| e.to_string())?;
        Ok(capture)
    }
    pub fn info(&self) -> &CaptureInfo {
        &self.info
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn device_invalidated(&self) -> bool {
        self.health.invalidated.load(Ordering::Acquire)
    }
    /// Retire an input confirmed missing by a device-notification snapshot.
    pub fn stop_after_device_invalidation(&mut self) {
        if cfg!(target_os = "macos") {
            self.health.invalidated.store(true, Ordering::Release);
        }
        self.stop();
    }
    pub fn stop(&mut self) {
        self.health.closing.store(true, Ordering::Release);
        if let Some(stream) = self.stream.take() {
            // CPAL 0.17's HAL listener Drop frees its callback even when listener
            // removal fails. After srst/unavailable we cannot prove quiescence:
            // retain the old stream and its callbacks; closing prevents delivery.
            // Normal stops still release the device. No default device fallback.
            if cfg!(target_os = "macos")
                && (self.device_invalidated()
                    || self.service_generation != crate::device_notifications::service_generation())
            {
                let _ = stream.pause();
                std::mem::forget(stream);
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    encoder: crate::source::Encoder,
    error: Arc<Mutex<Option<String>>>,
    health: Arc<Health>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels);
    let mut resampler = super::resample::Resampler::new(config.sample_rate);
    let mut block = [0.0; FRAMES_PER_BLOCK * 2];
    let mut filled = 0;
    let mut encoder = encoder;
    let error_health = health.clone();
    device
        .build_input_stream(
            config,
            move |samples: &[T], _| {
                if health.closing.load(Ordering::Acquire) {
                    return;
                }
                let callback_at = std::time::Instant::now();
                for frame in samples.chunks_exact(channels) {
                    let mono = frame.iter().map(|value| value.to_sample::<f32>()).sum::<f32>() / channels as f32;
                    resampler.push(mono, |sample| {
                        block[filled] = sample;
                        block[filled + 1] = sample;
                        filled += 2;
                        if filled == block.len() {
                            // Encode/post on the native callback with fixed storage, just
                            // like speaker capture; no allocating PCM channel to an actor.
                            encoder.supply(
                                &block,
                                crate::source::Stamp {
                                    first_sample_at: None,
                                    callback_at: Some(callback_at),
                                    delivered_at: callback_at,
                                    // CPAL does not expose the native frame position
                                    // here. A count of delivered blocks cannot detect
                                    // capture loss and must not certify continuity.
                                    source_frame_position: None,
                                    discontinuity: false,
                                },
                            );
                            filled = 0;
                        }
                    });
                }
            },
            move |failure| {
                if error_health.closing.load(Ordering::Acquire) {
                    return;
                }
                // Preserve the first error classification. Later callback noise
                // cannot turn a real I/O fault into a recoverable invalidation.
                let mut error = error.lock().unwrap_or_else(|e| e.into_inner());
                if error.is_none() {
                    error_health.invalidated.fetch_or(
                        cfg!(target_os = "macos") && matches!(failure, cpal::StreamError::DeviceNotAvailable),
                        Ordering::Release,
                    );
                    *error = Some(failure.to_string());
                }
            },
            None,
        )
        .map_err(|e| e.to_string())
}

pub fn input_candidates() -> Result<Vec<InputCandidate>, String> {
    #[cfg(target_os = "macos")]
    return crate::macos_devices::input_candidates();
    #[cfg(windows)]
    cpal::default_host()
        .input_devices()
        .map_err(|e| e.to_string())?
        .map(|device| {
            let description = device.description().map_err(|e| e.to_string())?;
            let interface = description.interface_type();
            let physical = matches!(
                interface,
                cpal::InterfaceType::BuiltIn
                    | cpal::InterfaceType::Usb
                    | cpal::InterfaceType::Bluetooth
                    | cpal::InterfaceType::Pci
                    | cpal::InterfaceType::FireWire
                    | cpal::InterfaceType::Thunderbolt
                    | cpal::InterfaceType::Line
            );
            Ok(InputCandidate {
                input: descriptor(&device)?,
                physical,
                built_in: interface == cpal::InterfaceType::BuiltIn,
                forwarding_loopback: super::windows_default::is_forwarding_endpoint(
                    &device.id().map_err(|e| e.to_string())?.to_string(),
                )
                .unwrap_or(false),
            })
        })
        .collect()
}
/// Changes only the system capture default after revalidating a follow-default
/// loopback repair plan. No capture stream or TCC request is created here.
pub fn switch_default_input(expected: &str, replacement: &str) -> Result<(), String> {
    let candidates = input_candidates()?;
    if !candidates
        .iter()
        .any(|c| c.input.id == replacement && c.physical && !c.forwarding_loopback && !c.input.is_forwarding_loopback())
    {
        return Err("replacement hardware microphone is unavailable".into());
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        unsafe extern "C" {
            fn oink_audio_set_default_input(
                expected: *const std::ffi::c_char,
                replacement: *const std::ffi::c_char,
            ) -> i32;
        }
        let expected = CString::new(
            expected
                .strip_prefix("coreaudio:")
                .ok_or("invalid CoreAudio input ID")?,
        )
        .map_err(|e| e.to_string())?;
        let replacement = CString::new(
            replacement
                .strip_prefix("coreaudio:")
                .ok_or("invalid CoreAudio input ID")?,
        )
        .map_err(|e| e.to_string())?;
        // SAFETY: live terminated strings, synchronous metadata setter; no audio I/O.
        let result = unsafe { oink_audio_set_default_input(expected.as_ptr(), replacement.as_ptr()) };
        if result != 0 {
            return Err(format!("CoreAudio default microphone switch failed: {result}"));
        }
        Ok(())
    }
    #[cfg(windows)]
    super::windows::switch_default_input(expected, replacement)
}
