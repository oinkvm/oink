//! Replay focus delivery ahead of the ordinary configuration owner's reconnect facts.
use super::*;
use oink_foundation::{
    Point,
    input::{InputEvent, SourceKind},
};
use oink_routing::control::FocusScope;

fn attach(a: &mut Router, b: &mut Router) {
    a.attach_input(
        fixture_boot(b.device()),
        b.routing_session(),
        true,
        false,
        Default::default(),
        0,
    );
    b.attach_input(
        fixture_boot(a.device()),
        a.routing_session(),
        true,
        false,
        Default::default(),
        0,
    );
}

fn deliver(from: &mut Router, to: &mut Router) {
    for output in from.drain_outputs() {
        if let Output::Send { to: recipient, msg, .. } = output
            && recipient == to.device()
        {
            to.input_message(from.device(), msg, 1);
        }
    }
}

#[test]
fn reconnect_claim_survives_stale_layout_republication_on_another_member() {
    for geometry_changed in [false, true] {
        let mut resident = Harness::new();
        let observer_id = DeviceId([3; 32]);
        let layout = Arc::new(SimpleLayout::new());
        let mut observer = Router::new(
            RouterConfig::new(fixture_boot(observer_id), "observer", Os::MacOS),
            layout.clone(),
        );
        let mut restarted = Router::new(
            RouterConfig::new(fixture_boot(peer()), "restarted", Os::Windows),
            layout,
        );
        restarted.set_local_displays(vec![fixture_display()]);
        restarted.set_cursor(Point::new(500., 300.));
        attach(resident.driver.adapter.router_mut(), &mut observer);

        // While the third member is offline, the two resident members share a pointer.
        let old_scope = FocusScope {
            members: vec![
                (resident.driver.local, resident.driver.adapter.routing_session()),
                (observer_id, observer.routing_session()),
            ],
        };
        let old_claim = ControlMsg::FocusState {
            epoch: 100,
            holder: observer_id,
            scope: Some(old_scope),
            session: Some(observer.routing_session()),
            lock_state: None,
        };
        resident
            .driver
            .adapter
            .router_mut()
            .input_message(observer_id, Message::Control(old_claim.clone()), 0);
        let mut stale = (*resident.driver.layout).clone();
        stale.devices.remove(&peer());
        resident.driver.snapshots.layout.publish(stale.clone());
        resident.driver.refresh();
        resident.driver.adapter.router_mut().drain_outputs();

        // The restarted member reaches one resident first and learns the historical
        // claim. Its first local motion then legitimately selects its own cursor.
        attach(resident.driver.adapter.router_mut(), &mut restarted);
        let mut historical = old_claim;
        if let ControlMsg::FocusState { session, .. } = &mut historical {
            *session = Some(resident.driver.adapter.routing_session());
        }
        restarted.input_message(resident.driver.local, Message::Control(historical), 1);
        restarted.drain_outputs();
        restarted.on_local_event(
            &CapturedEvent {
                event: InputEvent::PointerDelta { dx: 0., dy: 1. },
                physical: true,
                source: SourceKind::Mouse,
                time_us: 2,
                cursor: Some(Point::new(500., 301.)),
            },
            2,
        );
        assert_eq!(restarted.focus_claim(), (101, peer()));
        deliver(&mut restarted, resident.driver.adapter.router_mut());
        assert_eq!(resident.driver.adapter.router().focus().holder, peer());
        resident.driver.adapter.router_mut().drain_outputs();

        // An ordinary facts refresh still excludes the reconnecting member. This
        // is not a removal, even if an unrelated geometry edit accompanies it.
        if geometry_changed {
            let mut geometry = (*stale.layout).clone();
            geometry.screens[0].rect.x += 1.;
            stale.layout = Arc::new(geometry);
        }
        resident.driver.snapshots.layout.publish(stale.clone());
        resident.driver.refresh();
        let preserved = resident.driver.adapter.router().focus_claim();
        let holder = resident.driver.adapter.router().focus().holder;
        deliver(resident.driver.adapter.router_mut(), &mut observer);
        assert_ne!(
            observer.focus_claim().1,
            resident.driver.local,
            "a lagging snapshot must not broadcast a new claim to the third member"
        );
        assert_eq!(preserved, (101, peer()));
        assert_eq!(holder, peer());

        // Once facts catch up, a subsequent actual removal still recalls focus.
        let mut online = stale.clone();
        online.devices.insert(peer());
        resident.driver.snapshots.layout.publish(online);
        resident.driver.refresh();
        assert_eq!(resident.driver.adapter.router().focus_claim(), (101, peer()));
        resident.driver.snapshots.layout.publish(stale);
        resident.driver.refresh();
        assert_eq!(
            resident.driver.adapter.router().focus_claim(),
            (102, resident.driver.local)
        );
    }
}
