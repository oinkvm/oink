//! Event-driven taskbar commands; Shell links carry only a fixed command ID.
use oink_agent::native_controls::{ARGUMENT, Item, Model};
use windows::{
    Win32::{
        Storage::EnhancedStorage::PKEY_Title,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
            StructuredStorage::PROPVARIANT,
        },
        UI::Shell::{
            Common::{IObjectArray, IObjectCollection},
            DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW,
            PropertiesSystem::{IPropertyStore, PSCoerceToCanonicalValue},
            ShellLink,
        },
    },
    core::{HSTRING, Interface, Result},
};
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: created only after successful CoInitializeEx on this thread;
        // all apartment-bound objects have been released before this guard.
        unsafe {
            CoUninitialize();
        }
    }
}

pub fn update(app: &tauri::AppHandle, model: &Model) -> Result<()> {
    // SAFETY: COM objects stay on this thread. Only balance successful COM init;
    // an existing apartment is reused and all Shell parameters own their storage.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let _apartment = initialized.then_some(Apartment);
        let list: ICustomDestinationList = CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        list.SetAppID(&HSTRING::from(&app.config().identifier))?;
        let mut slots = 0;
        let removed: IObjectArray = list.BeginList(&mut slots)?;
        let mut removed_args = Vec::new();
        for index in 0..removed.GetCount()? {
            if let Ok(link) = removed.GetAt::<IShellLinkW>(index) {
                let mut args = [0u16; 256];
                if link.GetArguments(&mut args).is_ok() {
                    let end = args.iter().position(|v| *v == 0).unwrap_or(args.len());
                    removed_args.push(String::from_utf16_lossy(&args[..end]));
                }
            }
        }
        let result = (|| {
            let executable = std::env::current_exe().map_err(|_| windows::core::Error::from_thread())?;
            for group in &model.groups {
                let items = collection(&group.items, &executable, &removed_args)?;
                if items.GetCount()? > 0 {
                    list.AppendCategory(&HSTRING::from(&group.label), &items)?;
                }
            }
            let tasks: Vec<_> = model
                .tasks
                .iter()
                .filter(|i| matches!(i.id, "lock" | "unlock" | "show" | "pause" | "resume" | "quit"))
                .cloned()
                .collect();
            let items = collection(&tasks, &executable, &[])?;
            list.AddUserTasks(&items)?;
            list.CommitList()
        })();
        if result.is_err() {
            let _ = list.AbortList();
        }
        result
    }
}
unsafe fn collection(items: &[Item], executable: &std::path::Path, removed: &[String]) -> Result<IObjectArray> {
    // SAFETY: called with COM initialized; only generated fixed IDs become args.
    unsafe {
        let collection: IObjectCollection = CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        for item in items.iter().filter(|item| item.enabled) {
            let args = format!("{ARGUMENT} {}", item.id);
            if removed.contains(&args) {
                continue;
            }
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(executable))?;
            link.SetArguments(&HSTRING::from(args))?;
            let title = if item.checked == Some(true) {
                format!("✓ {}", item.label)
            } else {
                item.label.clone()
            };
            let properties: IPropertyStore = link.cast()?;
            let mut title = PROPVARIANT::from(title.as_str());
            PSCoerceToCanonicalValue(&PKEY_Title, &mut title)?;
            properties.SetValue(&PKEY_Title, &title)?;
            properties.Commit()?;
            collection.AddObject(&link)?;
        }
        collection.cast()
    }
}
