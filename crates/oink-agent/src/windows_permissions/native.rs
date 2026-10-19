use super::PermissionState;
use std::path::Path;
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SERVICE_DOES_NOT_EXIST, RPC_E_CHANGED_MODE},
        NetworkManagement::WindowsFirewall::*,
        System::{Com::*, Services::*},
    },
    core::{BSTR, Result, w},
};

#[cfg(test)]
thread_local! {
    pub(super) static OBSERVATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static FIREWALL_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static SERVICE_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn firewall_state() -> PermissionState {
    #[cfg(test)]
    {
        OBSERVATIONS.set(OBSERVATIONS.get() + 1);
        FIREWALL_QUERIES.set(FIREWALL_QUERIES.get() + 1);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| firewall(&exe).ok())
        .unwrap_or(PermissionState::NotDetermined)
}

pub(super) fn service_state() -> PermissionState {
    #[cfg(test)]
    {
        OBSERVATIONS.set(OBSERVATIONS.get() + 1);
        SERVICE_QUERIES.set(SERVICE_QUERIES.get() + 1);
    }
    // Protected installed backends already register NotifyServiceStatusChangeW
    // for OinkInput STOP_PENDING/STOPPED before starting their platform owners.
    // That read-only SCM notification shuts the backend down; its next startup
    // samples the new service state. No duplicate watcher or periodic query is
    // needed for this per-backend observation. Development sessions retain the
    // startup sample too, without adding a watcher or polling fallback.
    service().unwrap_or(PermissionState::NotDetermined)
}

struct Com(bool);
impl Com {
    fn new() -> Result<Self> {
        // INetFwPolicy2 works in either apartment. A caller's existing STA must
        // not be uninitialized when MTA initialization reports CHANGED_MODE.
        // Read-only COM/SCM queries work for medium-integrity users on Windows 11.
        // SAFETY: initialization is scoped to this thread and balanced by Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            Ok(Self(false))
        } else {
            hr.ok()?;
            Ok(Self(true))
        }
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances this thread's successful initialization, after
            // all firewall COM interfaces have been dropped.
            unsafe { CoUninitialize() };
        }
    }
}

fn firewall(exe: &Path) -> Result<PermissionState> {
    let _com = Com::new()?;
    // SAFETY: initialized apartment, read-only methods, owned interface values.
    unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)?;
        let active = policy.CurrentProfileTypes()?;
        if active == 0 {
            return Ok(PermissionState::NotDetermined);
        }
        let mut required_profiles = 0;
        for profile in [NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC] {
            if active & profile.0 == 0 || !policy.get_FirewallEnabled(profile)?.as_bool() {
                continue;
            }
            if policy.get_BlockAllInboundTraffic(profile)?.as_bool() {
                return Ok(PermissionState::Denied);
            }
            required_profiles |= profile.0;
        }
        if required_profiles == 0 {
            return Ok(PermissionState::Granted);
        }
        // Group Policy may make local installer rules ineffective. Do not
        // promote their mere presence to a grant in that case.
        if policy.LocalPolicyModifyState()? != NET_FW_MODIFY_STATE_OK {
            return Ok(PermissionState::NotDetermined);
        }
        let rules = policy.Rules()?;
        for (name, protocol) in [
            ("Oink - Discovery and QUIC (UDP)", 17),
            ("Oink - Bulk transfer (TCP)", 6),
        ] {
            let rule = match rules.Item(&BSTR::from(name)) {
                Ok(rule) => rule,
                Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                    return Ok(PermissionState::NotGranted);
                }
                Err(error) => return Err(error),
            };
            let observation = Rule {
                app: rule.ApplicationName()?.to_string(),
                protocol: rule.Protocol()?,
                profiles: rule.Profiles()?,
                enabled: rule.Enabled()?.as_bool(),
                inbound_allow: rule.Direction()? == NET_FW_RULE_DIR_IN && rule.Action()? == NET_FW_ACTION_ALLOW,
                unrestricted: rule.LocalPorts()? == "*"
                    && rule.RemotePorts()? == "*"
                    && rule.LocalAddresses()? == "*"
                    && rule.RemoteAddresses()? == "*"
                    && rule.ServiceName()?.is_empty()
                    && rule.InterfaceTypes()?.to_string().eq_ignore_ascii_case("All"),
            };
            if !observation.permits(exe, protocol, required_profiles) {
                return Ok(PermissionState::NotGranted);
            }
        }
        Ok(PermissionState::Granted)
    }
}

struct Rule {
    app: String,
    protocol: i32,
    profiles: i32,
    enabled: bool,
    inbound_allow: bool,
    unrestricted: bool,
}
impl Rule {
    fn permits(&self, exe: &Path, protocol: i32, profiles: i32) -> bool {
        self.enabled
            && self.inbound_allow
            && self.unrestricted
            && self.protocol == protocol
            && self.profiles & profiles == profiles
            && self.app.eq_ignore_ascii_case(&exe.to_string_lossy())
    }
}

struct Service(SC_HANDLE);
impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: owned SCM handle; CloseHandle cannot close service handles.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}
fn service() -> Result<PermissionState> {
    // Query the SCM service object, not the SYSTEM process/token (which medium
    // integrity callers cannot open). Never request start/stop/change rights.
    // SAFETY: local SCM, fixed name, valid handles and writable status buffer.
    unsafe {
        let manager = Service(OpenSCManagerW(None, None, SC_MANAGER_CONNECT)?);
        let service = match OpenServiceW(manager.0, w!("OinkInput"), SERVICE_QUERY_STATUS) {
            Ok(handle) => Service(handle),
            Err(error) if error.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
                return Ok(PermissionState::NotGranted);
            }
            Err(error) => return Err(error),
        };
        let mut status = SERVICE_STATUS_PROCESS::default();
        let buffer = std::slice::from_raw_parts_mut(
            std::ptr::from_mut(&mut status).cast::<u8>(),
            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
        );
        let mut needed = 0;
        QueryServiceStatusEx(service.0, SC_STATUS_PROCESS_INFO, Some(buffer), &mut needed)?;
        Ok(if status.dwCurrentState == SERVICE_RUNNING {
            PermissionState::Granted
        } else {
            PermissionState::NotGranted
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Explicit acceptance probe: reads OS configuration, never launches the
    /// installed program or changes firewall/service state.
    #[test]
    #[ignore = "requires an installed MSI and OINK_TEST_INSTALLED_EXE"]
    fn installed_access_read_only_probe() {
        let exe =
            std::path::PathBuf::from(std::env::var_os("OINK_TEST_INSTALLED_EXE").expect("installed executable path"));
        let firewall = firewall(&exe).expect("read firewall policy");
        let service = service().expect("query SCM status");
        println!("native installed permissions: firewall={firewall:?}, system_service={service:?}");
        assert_eq!(firewall, PermissionState::Granted);
        assert_eq!(service, PermissionState::Granted);
    }

    #[test]
    fn installer_rule_must_cover_this_executable_protocol_and_profiles() {
        let exe = Path::new(r"C:\Program Files\Oink\oink.exe");
        let mut rule = Rule {
            app: r"c:\program files\oink\OINK.EXE".into(),
            protocol: 17,
            profiles: i32::MAX,
            enabled: true,
            inbound_allow: true,
            unrestricted: true,
        };
        assert!(rule.permits(exe, 17, 4));
        assert!(!rule.permits(Path::new(r"C:\Development\oink.exe"), 17, 4));
        assert!(!rule.permits(exe, 6, 4));
        rule.profiles = 2;
        assert!(!rule.permits(exe, 17, 4));
        rule.profiles = i32::MAX;
        rule.enabled = false;
        assert!(!rule.permits(exe, 17, 4));
        rule.enabled = true;
        rule.inbound_allow = false;
        assert!(!rule.permits(exe, 17, 4));
        rule.inbound_allow = true;
        rule.unrestricted = false;
        assert!(!rule.permits(exe, 17, 4));
    }
}
