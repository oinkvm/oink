use super::{Endpoint, ExitVolumeStatus, Fault, KeyAction, Reading};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::Win32::{
    Foundation::{E_ACCESSDENIED, E_NOINTERFACE},
    Media::Audio::{
        AUDCLNT_E_DEVICE_INVALIDATED, AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE_ACTIVE,
        Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl},
        IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
    },
    System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateGuid, CoCreateInstance, CoInitializeEx, CoUninitialize},
    UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_VOLUME_DOWN, VK_VOLUME_MUTE,
        VK_VOLUME_UP,
    },
};
use windows::core::{GUID, PCWSTR};

fn fault(error: windows::core::Error) -> Fault {
    let code = error.code();
    Fault {
        status: if code == E_ACCESSDENIED {
            ExitVolumeStatus::PermissionDenied
        } else if code == E_NOINTERFACE {
            ExitVolumeStatus::Unsupported
        } else if code == AUDCLNT_E_DEVICE_INVALIDATED || code.0 == 0x80070490_u32 as i32 {
            ExitVolumeStatus::Unavailable
        } else {
            ExitVolumeStatus::BackendFailed
        },
        native_code: Some(i64::from(code.0)),
    }
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: paired with this control thread's successful CoInitializeEx.
        unsafe { CoUninitialize() };
    }
}
#[windows::core::implement(IAudioEndpointVolumeCallback)]
struct Callback(Arc<AtomicBool>, Arc<dyn Fn() + Send + Sync>);
impl IAudioEndpointVolumeCallback_Impl for Callback_Impl {
    fn OnNotify(&self, _: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        // Native callbacks never perform a device query or block the controller.
        self.0.store(true, Ordering::Release);
        (self.1)();
        Ok(())
    }
}
struct Native {
    enumerator: IMMDeviceEnumerator,
    device: IMMDevice,
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
    context: GUID,
    dirty: Arc<AtomicBool>,
    _apartment: Apartment,
}
pub(super) fn open(output: &str, wake: Arc<dyn Fn() + Send + Sync>) -> Result<Box<dyn Endpoint>, Fault> {
    let id = output
        .strip_prefix("wasapi:")
        .filter(|id| !id.is_empty() && !id.contains('\0'))
        .ok_or_else(Fault::unavailable)?;
    // SAFETY: only this dedicated audio control thread owns these COM interfaces.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(fault)?;
        let apartment = Apartment;
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(fault)?;
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        let device = enumerator.GetDevice(PCWSTR(wide.as_ptr())).map_err(fault)?;
        if device.GetState().map_err(fault)? != DEVICE_STATE_ACTIVE {
            return Err(Fault::unavailable());
        }
        let volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).map_err(fault)?;
        let dirty = Arc::new(AtomicBool::new(true));
        let callback: IAudioEndpointVolumeCallback = Callback(dirty.clone(), wake).into();
        let context = CoCreateGuid().map_err(fault)?;
        volume.RegisterControlChangeNotify(&callback).map_err(fault)?;
        Ok(Box::new(Native {
            enumerator,
            device,
            volume,
            callback,
            context,
            dirty,
            _apartment: apartment,
        }))
    }
}
fn endpoint_id(device: &IMMDevice) -> Result<Vec<u16>, Fault> {
    // SAFETY: GetId allocates a terminated COM string; copy before freeing it.
    unsafe {
        let id = device.GetId().map_err(fault)?;
        let value = id.as_wide().to_vec();
        windows::Win32::System::Com::CoTaskMemFree(Some(id.0.cast()));
        Ok(value)
    }
}
fn native_key_pair(action: KeyAction) -> [INPUT; 2] {
    let vk = match action {
        KeyAction::Up => VK_VOLUME_UP,
        KeyAction::Down => VK_VOLUME_DOWN,
        KeyAction::Mute => VK_VOLUME_MUTE,
    };
    [Default::default(), KEYEVENTF_KEYUP].map(|flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: flags,
                dwExtraInfo: 0x4F49_4E4B, // Same injection tag as the input hook.
                ..Default::default()
            },
        },
    })
}
impl Endpoint for Native {
    fn changed(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }
    fn read(&mut self) -> Result<Reading, Fault> {
        self.dirty.store(false, Ordering::Release);
        // SAFETY: live COM endpoint, all calls on its owning control thread.
        unsafe {
            if self.device.GetState().map_err(fault)? != DEVICE_STATE_ACTIVE {
                return Err(Fault::unavailable());
            }
            let volume = self.volume.GetMasterVolumeLevelScalar().map_err(fault)?;
            if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                return Err(Fault {
                    status: ExitVolumeStatus::BackendFailed,
                    native_code: None,
                });
            }
            Ok(Reading {
                percent: (volume * 100.0).round() as u8,
                muted: self.volume.GetMute().map_err(fault)?.as_bool(),
            })
        }
    }
    fn key(&mut self, action: KeyAction) -> Result<(), Fault> {
        // SAFETY: retained exact endpoint, used only by its owning COM thread.
        unsafe {
            if self.device.GetState().map_err(fault)? != DEVICE_STATE_ACTIVE {
                return Err(Fault::unavailable());
            }
            let default = self
                .enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(fault)?;
            let same = endpoint_id(&self.device)? == endpoint_id(&default)?;
            if same {
                // Endpoint writes bypass the Windows flyout. A tagged VK pair
                // uses the shell's native feedback and our keyboard hook passes
                // it before routing, even when focus belongs to another peer.
                // Media VKs target only the OS default; other selected outputs
                // keep exact-endpoint control without changing system settings.
                let inputs = native_key_pair(action);
                if SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) != inputs.len() as u32 {
                    return Err(fault(windows::core::Error::from_thread()));
                }
                return Ok(()); // Actual readback arrives via OnNotify.
            }
            match action {
                KeyAction::Up => {
                    self.volume.VolumeStepUp(&self.context).map_err(fault)?;
                    self.volume.SetMute(false, &self.context).map_err(fault)?;
                }
                KeyAction::Down => {
                    self.volume.VolumeStepDown(&self.context).map_err(fault)?;
                    self.volume.SetMute(false, &self.context).map_err(fault)?;
                }
                KeyAction::Mute => self
                    .volume
                    .SetMute(!self.volume.GetMute().map_err(fault)?.as_bool(), &self.context)
                    .map_err(fault)?,
            }
        }
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }
    fn write(&mut self, reading: Reading) -> Result<(), Fault> {
        // SAFETY: the bound IMMDevice and endpoint never follow the default.
        // Endpoint invalidation fails rather than redirecting either write.
        unsafe {
            if self.device.GetState().map_err(fault)? != DEVICE_STATE_ACTIVE {
                return Err(Fault::unavailable());
            }
            self.volume
                .SetMasterVolumeLevelScalar(f32::from(reading.percent) / 100.0, &self.context)
                .map_err(fault)?;
            self.volume.SetMute(reading.muted, &self.context).map_err(fault)?;
        }
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: subscription and endpoint remain live; COM releases the callback
        // after in-flight calls. Its only state is an independently owned Arc.
        if let Err(error) = unsafe { self.volume.UnregisterControlChangeNotify(&self.callback) } {
            tracing::warn!(native_code = error.code().0, "exit volume observer removal failed");
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_volume_pair_uses_media_vk_and_hook_bypass_tag() {
        for (action, vk) in [
            (KeyAction::Up, VK_VOLUME_UP),
            (KeyAction::Down, VK_VOLUME_DOWN),
            (KeyAction::Mute, VK_VOLUME_MUTE),
        ] {
            let pair = native_key_pair(action);
            for (index, input) in pair.iter().enumerate() {
                assert_eq!(input.r#type, INPUT_KEYBOARD);
                // SAFETY: native_key_pair initializes the keyboard union member.
                let key = unsafe { input.Anonymous.ki };
                assert_eq!(key.wVk, vk);
                assert_eq!(key.dwExtraInfo, 0x4F49_4E4B);
                assert_eq!(
                    key.dwFlags,
                    if index == 0 {
                        Default::default()
                    } else {
                        KEYEVENTF_KEYUP
                    }
                );
            }
        }
    }
    #[test]
    fn callback_only_marks_live_readback_dirty_without_opening_any_endpoint() {
        let dirty = Arc::new(AtomicBool::new(false));
        let callback: IAudioEndpointVolumeCallback = Callback(dirty.clone(), Arc::new(|| {})).into();
        // SAFETY: the callback intentionally does not read event data.
        unsafe {
            callback.OnNotify(std::ptr::null_mut()).unwrap();
        }
        assert!(dirty.swap(false, Ordering::AcqRel));
        // SAFETY: the callback intentionally ignores the event pointer.
        unsafe {
            callback.OnNotify(std::ptr::null_mut()).unwrap();
        }
        assert!(dirty.load(Ordering::Acquire));
    }
}
