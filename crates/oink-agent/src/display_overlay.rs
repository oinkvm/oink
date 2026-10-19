//! Short-lived screen identification, independent of layout editing and photo input.
//! Identification uses the normal input stream and never opens a separate listener.
use crate::api::{self, Overlay, OverlayContent, OverlayKind, OverlayScreen};
use crate::layout_sync::now_ms;
use crate::node::{Node, lock};
use crate::threads::input::InputCommand;
use std::time::{Duration, Instant};

pub(crate) fn overlay(node: &Node) -> Option<Overlay> {
    lock(&node.inner.overlay).clone()
}

fn set_overlay(node: &Node, overlay: Option<Overlay>) {
    let mut current = lock(&node.inner.overlay);
    if let Ok(input) = node.input_thread() {
        input.send(InputCommand::Overlay(overlay.as_ref().map(|overlay| overlay.id)));
    }
    match &overlay {
        Some(overlay) => crate::backend::overlay::show(overlay),
        None => crate::backend::overlay::hide(),
    }
    *current = overlay;
    node.inner.overlay_wake.notify_one();
}

pub(crate) fn hide(node: &Node) {
    set_overlay(node, None);
}

pub(crate) fn dismiss(node: &Node, id: u64) {
    let mut current = lock(&node.inner.overlay);
    if current.as_ref().is_none_or(|overlay| overlay.id != id) {
        return;
    }
    *current = None;
    if let Ok(input) = node.input_thread() {
        input.send(InputCommand::Overlay(None));
    }
    crate::backend::overlay::hide();
    node.inner.overlay_wake.notify_one();
}

pub(crate) fn identify_local(node: &Node, id: u64) {
    if node.fully_paused() {
        return;
    }
    let Ok(group) = node.link_config() else { return };
    if group.applied().paused {
        return;
    }
    let screens = group
        .local_displays()
        .displays
        .iter()
        .map(|display| OverlayScreen {
            display: display.key.clone(),
            device_name: node.device_name(node.id()),
            display_name: display.name.clone(),
            content: OverlayContent::Identify,
        })
        .collect();
    set_overlay(
        node,
        Some(Overlay {
            id,
            kind: OverlayKind::Identify,
            screens,
            expires_at_ms: Some(now_ms() as i64 + 3000),
        }),
    );
}

pub(crate) fn identify(node: &Node) {
    let id = now_ms();
    identify_local(node, id);
    if let Ok(input) = node.input_thread() {
        input.send(InputCommand::IdentifyScreens(id));
    }
}

pub(crate) fn shown(node: &Node, id: u64, screens: Vec<api::OverlayShown>) {
    if overlay(node).is_none_or(|overlay| overlay.id != id) {
        return;
    }
    for screen in screens {
        if let Some(error) = screen.error {
            tracing::warn!(display = %screen.display, %error, "screen identification failed");
        }
    }
}

fn expiry_deadline(node: &Node, now: Instant, wall_ms: i64) -> Option<Instant> {
    overlay(node)
        .and_then(|overlay| overlay.expires_at_ms)
        .map(|end| now + Duration::from_millis(end.saturating_sub(wall_ms).max(0) as u64))
}

fn expire(node: &Node, wall_ms: i64) {
    if let Some(overlay) = overlay(node)
        && overlay.expires_at_ms.is_some_and(|end| end <= wall_ms)
    {
        dismiss(node, overlay.id);
    }
}

async fn wait_expiry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

pub(crate) fn start(node: Node) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let group = node.link_config()?;
    let mut changes = group.subscribe();
    let mut stop = node.stop_receiver();
    Ok(tokio::spawn(async move {
        loop {
            let deadline = expiry_deadline(&node, Instant::now(), now_ms() as i64);
            tokio::select! {
                _ = stop.changed() => break,
                change = changes.next() => {
                    if change.is_none() { break; }
                    if group.applied().paused { hide(&node); }
                },
                _ = node.inner.overlay_wake.notified() => {},
                _ = wait_expiry(deadline) => {},
            }
            expire(&node, now_ms() as i64);
        }
        hide(&node);
    }))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn identification_expires_at_its_deadline_and_ignores_stale_dismissals() {
        use std::future::Future;
        let dir = tempfile::tempdir().unwrap();
        let node = Node::prepare(crate::node::NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "identify", 0),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .unwrap();
        *lock(&node.inner.overlay) = Some(Overlay {
            id: 7,
            kind: OverlayKind::Identify,
            screens: vec![],
            expires_at_ms: Some(1050),
        });
        dismiss(&node, 6);
        assert_eq!(overlay(&node).unwrap().id, 7);
        let now = tokio::time::Instant::now().into_std();
        let deadline = expiry_deadline(&node, now, 1000);
        assert_eq!(deadline, Some(now + Duration::from_millis(50)));
        let mut sleep = Box::pin(wait_expiry(deadline));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(sleep.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(Duration::from_millis(49)).await;
        expire(&node, 1049);
        assert!(overlay(&node).is_some());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(sleep.as_mut().poll(&mut cx).is_ready());
        expire(&node, 1050);
        assert!(overlay(&node).is_none());
        assert!(expiry_deadline(&node, now, 1050).is_none());
    }
}
