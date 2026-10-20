//! Fixed SYSTEM recovery task, with the actual scheduler DACL read back over COM.
use oink_update::Error;
use windows::{
    Win32::System::{Com::*, TaskScheduler::*, Variant::VARIANT},
    core::BSTR,
};
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: pairs one successful CoInitializeEx on this same worker thread.
        unsafe {
            CoUninitialize();
        }
    }
}
fn with_service<T>(f: impl FnOnce(&ITaskService) -> Result<T, Error>) -> Result<T, Error> {
    // SAFETY: updater workers use a dedicated ordinary thread, no COM objects cross it.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.map_err(|_| Error::Storage)?;
    let _apartment = Apartment;
    // SAFETY: local Task Scheduler service COM class; no remote machine or credentials.
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }.map_err(|_| Error::Storage)?;
    let empty = VARIANT::default();
    // SAFETY: default variants mean local machine/current SYSTEM identity.
    unsafe { service.Connect(&empty, &empty, &empty, &empty) }.map_err(|_| Error::Storage)?;
    f(&service)
}
fn with_folder<T>(f: impl FnOnce(&ITaskFolder) -> Result<T, Error>) -> Result<T, Error> {
    with_service(|service| {
        // SAFETY: fixed local root folder, returns an owned interface.
        let folder = unsafe { service.GetFolder(&BSTR::from("\\")) }.map_err(|_| Error::Storage)?;
        f(&folder)
    })
}
#[cfg(test)]
pub(super) fn validate_definition(xml: &str) -> Result<(), Error> {
    with_service(|service| {
        // SAFETY: an in-memory definition only; no task is registered or run.
        let definition = unsafe { service.NewTask(0) }.map_err(|_| Error::Storage)?;
        // SAFETY: native scheduler parses the exact UTF-16 XML passed at registration.
        unsafe { definition.SetXmlText(&BSTR::from(xml)) }.map_err(|_| Error::Storage)
    })
}
pub(super) fn register(tx: &str, xml: &str) -> Result<(), Error> {
    if !super::valid_transaction(tx) {
        return Err(Error::Policy);
    }
    with_folder(|folder| {
        let name = BSTR::from(format!("OinkUpdate-{tx}"));
        let user = VARIANT::from("S-1-5-18");
        let security = VARIANT::from("O:SYG:SYD:P(A;;GA;;;SY)");
        // SAFETY: XML is generated only from fixed worker action + validated hex tx;
        // SYSTEM principal and explicit SYSTEM-only execution/control DACL.
        let task = unsafe {
            folder.RegisterTask(
                &name,
                &BSTR::from(xml),
                TASK_CREATE_OR_UPDATE.0,
                &user,
                &VARIANT::default(),
                TASK_LOGON_SERVICE_ACCOUNT,
                &security,
            )
        }
        .map_err(|_| Error::Storage)?;
        // SAFETY: read the registered object's effective DACL, not just its XML.
        let actual = unsafe { task.GetSecurityDescriptor(4) }.map_err(|_| Error::Storage)?;
        super::protected::system_only_sddl(&actual.to_string()).map_err(|_| Error::Policy)
    })
}
pub(super) fn remove(tx: &str) -> Result<(), Error> {
    if !super::valid_transaction(tx) {
        return Err(Error::Policy);
    }
    with_folder(|folder| {
        // SAFETY: only this fixed transaction's task may be deleted.
        match unsafe { folder.DeleteTask(&BSTR::from(format!("OinkUpdate-{tx}")), 0) } {
            Ok(()) => Ok(()),
            Err(error) if error.code().0 == 0x80070002u32 as i32 => Ok(()),
            Err(_) => Err(Error::Storage),
        }
    })
}
