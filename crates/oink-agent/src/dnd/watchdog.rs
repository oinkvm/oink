//! Failure-only integration probes, absent from release builds. Every native
//! state lock is try_lock: diagnostics must survive the deadlock being diagnosed.
use super::*;

impl Node {
    /// A weak, read-only probe for integration watchdogs. Calling it may query
    /// the input owner for at most 50 ms; it never calls a native backend.
    pub fn dnd_watchdog_probe_for_test(&self) -> impl Fn() -> String + Send + Sync + 'static {
        let weak = Arc::downgrade(&self.inner);
        move || {
            let Some(inner) = weak.upgrade() else {
                return "node=gone".into();
            };
            // A probe is invoked immediately before process exit. Retain this
            // upgrade until then: dropping the last node/provider here could
            // start teardown and block the watchdog itself.
            let node = std::mem::ManuallyDrop::new(Node { inner });
            let input = node.inner.input.get().map(|input| {
                format!(
                    "input_status={:?}\n{}",
                    input.status(),
                    input.dnd_watchdog_snapshot_for_test()
                )
            });
            let native = node.inner.dnd.get().map(|handle| match handle.glue.try_lock() {
                Ok(glue) => glue
                    .as_ref()
                    .map_or_else(|| "glue=none".into(), |glue| glue.watchdog_snapshot()),
                Err(error) => format!("glue unavailable: {error}"),
            });
            format!(
                "node={:?}\n{}\n{}",
                node.id(),
                input.as_deref().unwrap_or("input=none"),
                native.as_deref().unwrap_or("native=none")
            )
        }
    }
}

impl DndGlue {
    fn watchdog_snapshot(&self) -> String {
        let local = self.local.try_lock().map(|local| {
            local
                .iter()
                .map(|(id, drag)| {
                    (
                        *id,
                        drag.operation,
                        drag.owned,
                        drag.exported,
                        drag._update_activity.is_some(),
                        drag._native.as_ref().map(|lease| lease.watchdog_snapshot_for_test()),
                    )
                })
                .collect::<Vec<_>>()
        });
        let targets = self.native_targets.try_lock().map(|targets| {
            targets
                .iter()
                .map(|(key, remote)| (*key, remote.watchdog_snapshot_for_test()))
                .collect::<Vec<_>>()
        });
        let preparations = {
            #[cfg(target_os = "macos")]
            {
                format!(
                    "{:?}",
                    self.preparations
                        .try_lock()
                        .map(|slots| slots.keys().copied().collect::<Vec<_>>())
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                "not applicable".to_owned()
            }
        };
        format!(
            "glue source={:?} export_epoch={:?} source_epoch={} physical={} original_up={:?}\nactive_operation={:?} target_grant={:?}\nlocal_sources={local:?}\nnative_publications={targets:?}\nadmission_leases={:?} preparations={preparations} delivery_receipts={:?}\n{}",
            self.active_source.try_lock(),
            self.export_epoch.try_lock(),
            self.source_epoch.load(Ordering::Acquire),
            self.source_physical.load(Ordering::Acquire),
            self.source_releases.try_lock(),
            self.active_operation.try_lock(),
            self.active_target.try_lock(),
            self.update_targets
                .try_lock()
                .map(|leases| leases.keys().copied().collect::<Vec<_>>()),
            self.files.receipts.try_lock().map(|receipts| receipts.len()),
            self.files.native.watchdog_snapshot_for_test(),
        )
    }
}
