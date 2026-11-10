//! Sync unlock on the native-loop owner with real bounded stream queues and a
//! fake injector, as in the sibling modules. No hook, tap or platform input call.
use super::*;
use crate::auto_unlock::{sync::SourceAnswer, typing::Job};
use std::sync::mpsc::sync_channel;

struct Attached {
    h: Harness,
    read: oink_network::link::RecvStream,
    _writer: oink_network::link::SendStream,
}
fn attached() -> Attached {
    let mut h = Harness::new();
    let (mut read, _) = h.outbound(1);
    let (writer, _) = h.inbound(1);
    opening(&writer, &remote(), true);
    h.attach();
    drain(&mut read);
    while h._main.try_recv().is_ok() {}
    Attached {
        h,
        read,
        _writer: writer,
    }
}
fn sync_requests(h: &Harness) -> Vec<u64> {
    std::iter::from_fn(|| h._main.try_recv().ok())
        .filter_map(|m| match m {
            MainCommand::SyncUnlock { source, epoch } if source == peer() => Some(epoch),
            _ => None,
        })
        .collect()
}
fn queries(read: &mut oink_network::link::RecvStream) -> Vec<u64> {
    drain(read)
        .into_iter()
        .filter_map(|m| match m {
            Message::Control(ControlMsg::UnlockQuery { epoch }) => Some(epoch),
            _ => None,
        })
        .collect()
}
fn probe_job() -> (Job, std::sync::mpsc::Receiver<Result<(), &'static str>>) {
    let (reply, result) = sync_channel(1);
    let job = Job::new(
        Box::new(|allowed| {
            if allowed.load(std::sync::atomic::Ordering::Acquire) {
                Ok(())
            } else {
                Err("admission revoked")
            }
        }),
        reply,
    );
    (job, result)
}

#[test]
fn user_unlock_edge_sends_one_hint_per_edge_and_nothing_while_paused() {
    let Attached {
        mut h,
        mut read,
        _writer,
    } = attached();
    h.driver.command(InputCommand::SyncUnlockBroadcast { epoch: 7 });
    h.driver.command(InputCommand::SyncUnlockBroadcast { epoch: 8 });
    let sent: Vec<_> = drain(&mut read)
        .into_iter()
        .filter_map(|m| match m {
            Message::Control(ControlMsg::SyncUnlock { epoch }) => Some(epoch),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [7, 8]);
    h.driver.command(InputCommand::Paused(true));
    h.driver.command(InputCommand::SyncUnlockBroadcast { epoch: 9 });
    assert!(
        !drain(&mut read)
            .iter()
            .any(|m| matches!(m, Message::Control(ControlMsg::SyncUnlock { .. })))
    );
}

#[test]
fn target_admits_each_source_boot_epoch_once_and_nothing_while_paused() {
    let Attached {
        mut h,
        read: _read,
        _writer,
    } = attached();
    for epoch in [5, 5, 4, 6] {
        h.driver
            .message(peer(), Message::Control(ControlMsg::SyncUnlock { epoch }));
    }
    assert_eq!(sync_requests(&h), [5, 6]);
    h.driver.command(InputCommand::Paused(true));
    h.driver
        .message(peer(), Message::Control(ControlMsg::SyncUnlock { epoch: 7 }));
    assert!(sync_requests(&h).is_empty());
}

#[test]
fn only_the_sync_challenge_reply_confirms_and_never_starts_a_pointer_attempt() {
    let Attached {
        mut h,
        mut read,
        _writer,
    } = attached();
    h.driver.command(InputCommand::SessionLocked(Some(true)));
    let (reply, answer) = sync_channel(1);
    h.driver
        .command(InputCommand::SyncUnlockQuery { source: peer(), reply });
    let sent = queries(&mut read);
    assert_eq!(sent.len(), 1);
    // A pointer-entry reply (focus epoch) cannot satisfy the sync challenge.
    let focus = h.driver.adapter.router().focus().epoch;
    h.driver.message(
        peer(),
        Message::Control(ControlMsg::UnlockReply {
            epoch: focus,
            unlocked: true,
        }),
    );
    assert!(answer.try_recv().is_err());
    for _ in 0..2 {
        h.driver.message(
            peer(),
            Message::Control(ControlMsg::UnlockReply {
                epoch: sent[0],
                unlocked: true,
            }),
        );
    }
    assert_eq!(answer.try_recv(), Ok(SourceAnswer::Unlocked));
    assert!(answer.try_recv().is_err(), "one answer per challenge");
    assert!(!std::iter::from_fn(|| h._main.try_recv().ok()).any(|m| matches!(m, MainCommand::UnlockAttempt { .. })));

    let (reply, answer) = sync_channel(1);
    h.driver
        .command(InputCommand::SyncUnlockQuery { source: peer(), reply });
    let query = queries(&mut read)[0];
    h.driver.message(
        peer(),
        Message::Control(ControlMsg::UnlockReply {
            epoch: query,
            unlocked: false,
        }),
    );
    assert_eq!(answer.try_recv(), Ok(SourceAnswer::Locked));
}

#[test]
fn challenge_needs_a_session_and_resolves_on_session_end_or_local_pause() {
    let Attached {
        mut h,
        mut read,
        _writer,
    } = attached();
    h.driver.command(InputCommand::SessionLocked(Some(true)));
    let (reply, answer) = sync_channel(1);
    h.driver.command(InputCommand::SyncUnlockQuery {
        source: DeviceId([9; 32]),
        reply,
    });
    assert_eq!(answer.try_recv(), Ok(SourceAnswer::Disconnected));
    assert!(queries(&mut read).is_empty());

    let (reply, answer) = sync_channel(1);
    h.driver
        .command(InputCommand::SyncUnlockQuery { source: peer(), reply });
    h.driver.detach(DeviceId([9; 32]));
    assert!(answer.try_recv().is_err(), "another peer's session is unrelated");
    h.driver.detach(peer());
    assert_eq!(answer.try_recv(), Ok(SourceAnswer::Disconnected));

    let Attached {
        mut h,
        read: _read,
        _writer,
    } = attached();
    h.driver.command(InputCommand::SessionLocked(Some(true)));
    let (reply, answer) = sync_channel(1);
    h.driver
        .command(InputCommand::SyncUnlockQuery { source: peer(), reply });
    h.driver.command(InputCommand::Paused(true));
    assert_eq!(
        answer.try_recv(),
        Ok(SourceAnswer::Cancelled),
        "a local pause is not a lost source"
    );
    let (reply, answer) = sync_channel(1);
    h.driver
        .command(InputCommand::SyncUnlockQuery { source: peer(), reply });
    assert_eq!(answer.try_recv(), Ok(SourceAnswer::Cancelled));
}

#[test]
fn sync_password_transaction_ignores_focus_but_needs_its_attached_source() {
    let Attached {
        mut h,
        read: _read,
        _writer,
    } = attached();
    // The pointer is on the peer: this device is not the focus holder.
    h.driver.command(InputCommand::MovePointerTo(peer()));
    h.driver.command(InputCommand::SessionLocked(Some(true)));
    let focus = h.driver.adapter.router().focus();
    assert_ne!(focus.holder, h.driver.local);
    for (admission, admitted) in [
        (LocalUnlockAdmission::Sync(peer()), true),
        (LocalUnlockAdmission::Sync(DeviceId([9; 32])), false),
        (LocalUnlockAdmission::Focus(focus.epoch), false),
    ] {
        let (job, result) = probe_job();
        h.driver.command(InputCommand::LocalUnlock { admission, job });
        assert_eq!(
            result.recv_timeout(Duration::from_secs(1)).unwrap().is_ok(),
            admitted,
            "{admission:?}"
        );
    }
    h.driver.command(InputCommand::SessionLocked(Some(false)));
    let (job, result) = probe_job();
    h.driver.command(InputCommand::LocalUnlock {
        admission: LocalUnlockAdmission::Sync(peer()),
        job,
    });
    assert!(
        result.recv_timeout(Duration::from_secs(1)).unwrap().is_err(),
        "unlocked"
    );
}

#[test]
fn only_the_sync_source_leaving_revokes_a_queued_sync_transaction() {
    for (leaving, admitted) in [(DeviceId([9; 32]), true), (peer(), false)] {
        let Attached {
            mut h,
            read: _read,
            _writer,
        } = attached();
        h.driver.command(InputCommand::SessionLocked(Some(true)));
        // Hold the injection FIFO so the transaction is still queued.
        let (busy, started) = sync_channel(1);
        let (release, wait) = sync_channel::<()>(1);
        let (done, _) = sync_channel(1);
        h.driver.inject.push(InjectItem::LocalUnlock(Job::new(
            Box::new(move |_| {
                busy.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(1)).unwrap();
                Ok(())
            }),
            done,
        )));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let (job, result) = probe_job();
        h.driver.command(InputCommand::LocalUnlock {
            admission: LocalUnlockAdmission::Sync(peer()),
            job,
        });
        h.driver.detach(leaving);
        release.send(()).unwrap();
        assert_eq!(result.recv_timeout(Duration::from_secs(1)).unwrap().is_ok(), admitted);
    }
}
