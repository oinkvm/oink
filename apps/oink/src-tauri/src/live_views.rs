//! Live telemetry demand follows the actual native window and its selected view.
use crate::app::Backend;
use oink_agent::api::LiveViews;
use std::{collections::HashMap, sync::Mutex};
use tauri::{AppHandle, Manager};

#[derive(Default)]
pub(crate) struct Demand {
    requested: Mutex<HashMap<String, LiveViews>>,
    effective: Mutex<LiveViews>,
    sending: Mutex<()>,
}

#[tauri::command]
pub(crate) fn live_views(window: tauri::WebviewWindow, app: AppHandle, views: LiveViews) {
    if crate::shell_policy::View::from_label(window.label()).is_none() {
        return;
    }
    app.state::<Demand>()
        .requested
        .lock()
        .unwrap()
        .insert(window.label().to_owned(), views);
    crate::app::on_main(&app, refresh);
}

pub(crate) fn refresh(app: &AppHandle) {
    let demand = app.state::<Demand>();
    let requested = demand.requested.lock().unwrap();
    let mut effective = LiveViews::default();
    for (label, views) in requested.iter() {
        let visible = if label == "panel" {
            crate::shell::panel_visible(app)
        } else {
            let visible = app
                .get_webview_window(label)
                .is_some_and(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(true));
            #[cfg(target_os = "macos")]
            let visible = visible
                && objc2::MainThreadMarker::new()
                    .is_some_and(|m| !objc2_app_kit::NSApplication::sharedApplication(m).isHidden());
            visible
        };
        if visible {
            effective = union(effective, *views);
        }
    }
    drop(requested);
    set_effective(app, effective);
}

pub(crate) fn closed(app: &AppHandle, label: &str) {
    app.state::<Demand>().requested.lock().unwrap().remove(label);
    refresh(app);
}

fn union(a: LiveViews, b: LiveViews) -> LiveViews {
    LiveViews {
        stats: a.stats || b.stats,
        group_quality: a.group_quality || b.group_quality,
        audio_quality: a.audio_quality || b.audio_quality,
        diagnostics: a.diagnostics || b.diagnostics,
    }
}

fn set_effective(app: &AppHandle, views: LiveViews) {
    let demand = app.state::<Demand>();
    let mut effective = demand.effective.lock().unwrap();
    if *effective == views {
        return;
    }
    *effective = views;
    drop(effective);
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Serialize writes and read current demand after any older worker.
        // Never hold the native visibility lock during blocking IPC writes.
        let demand = app.state::<Demand>();
        let _sending = demand.sending.lock().unwrap();
        let views = *demand.effective.lock().unwrap();
        if let Some(remote) = &app.state::<Backend>().remote
            && let Err(error) = remote.set_live_views(views)
        {
            tracing::debug!(%error, "could not update live view subscription");
        }
    });
}

pub(crate) fn stats_enabled(app: &AppHandle) -> bool {
    let views = *app.state::<Demand>().effective.lock().unwrap();
    views.stats || views.group_quality
}

#[cfg(target_os = "macos")]
pub(crate) fn watch_application(
    app: &AppHandle,
) -> Vec<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>> {
    use block2::RcBlock;
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSOperationQueue, NSString};
    let center = NSNotificationCenter::defaultCenter();
    ["NSApplicationDidHideNotification", "NSApplicationDidUnhideNotification"]
        .into_iter()
        .map(|name| {
            let handle = app.clone();
            let block = RcBlock::new(move |_: std::ptr::NonNull<NSNotification>| refresh(&handle));
            // SAFETY: the block is sendable and explicitly delivered on the main
            // queue; these observers perform only window/application state reads.
            unsafe {
                center.addObserverForName_object_queue_usingBlock(
                    Some(&NSString::from_str(name)),
                    None,
                    Some(&NSOperationQueue::mainQueue()),
                    &block,
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panel_and_main_demands_are_combined_without_losing_either() {
        let panel = LiveViews {
            group_quality: true,
            ..Default::default()
        };
        let main = LiveViews {
            stats: true,
            diagnostics: true,
            ..Default::default()
        };
        assert_eq!(
            union(panel, main),
            LiveViews {
                stats: true,
                group_quality: true,
                diagnostics: true,
                audio_quality: false
            }
        );
        assert_eq!(union(panel, LiveViews::default()), panel);
    }
}
