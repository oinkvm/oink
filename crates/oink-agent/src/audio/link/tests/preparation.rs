//! Real v1 M0 peers with fake capture/output endpoints.
use super::*;

#[tokio::test]
async fn prepared_ack_is_required_and_claim_retake_never_revives_old_session() {
    let mut running = Running::start(true).await;
    let peer = running.owners[1].links.id();
    let old = running.owners[0].source_session.prepared().unwrap().wire.clone();
    assert!(
        running.owners[1]
            .receive_sessions
            .values()
            .all(|session| session.ready())
    );
    running.claim(1, false);
    running
        .until(|r| matches!(r.owners[0].resolution, Resolution::None { .. }))
        .await;
    assert!(running.owners[0].capture.is_none());
    running.claim(1, true);
    running.until(|r| r.owners[0].capture.is_some()).await;
    let current = running.owners[0].source_session.prepared().unwrap().clone();
    assert_ne!(current.session().exit_claim(), old.session().unwrap().exit_claim());
    running.owners[0].message(peer, Message::AudioPrepared(old));
    assert_eq!(running.owners[0].source_session.prepared(), Some(&current));
    assert!(running.owners[0].capture.is_some());
    running.stop().await;
}

#[tokio::test]
async fn duplicate_preparation_is_idempotent_and_wrong_boot_cannot_change_native_state() {
    let mut running = Running::start(true).await;
    let peer = running.owners[1].links.id();
    let prepared = running.owners[0].source_session.prepared().unwrap().wire.clone();
    let before = lock(&running.platforms[0].0).events.clone();
    running.owners[0].message(peer, Message::AudioPrepared(prepared.clone()));
    assert_eq!(lock(&running.platforms[0].0).events, before);
    let mut stale = prepared;
    stale
        .edit(|raw| {
            let RawValue::Map(session) = raw.get_mut(&1).unwrap() else {
                unreachable!()
            };
            let RawValue::Map(source) = session.get_mut(&1).unwrap() else {
                unreachable!()
            };
            source.insert(2, FixedBytes([99; 16]).write());
        })
        .unwrap();
    running.owners[0].message(peer, Message::AudioPrepared(stale));
    assert_eq!(lock(&running.platforms[0].0).events, before);
    running.stop().await;
}

#[tokio::test]
async fn receiver_output_generation_changes_without_reusing_old_media() {
    let mut running = Running::start(true).await;
    let old = running.owners[0].source_session.prepared().unwrap().clone();
    running.owners[1].refresh_exit_binding(true).unwrap();
    running
        .until(|r| {
            r.owners[0]
                .source_session
                .prepared()
                .is_some_and(|current| current.stream_id() != old.stream_id())
                && r.owners[0].capture.is_some()
        })
        .await;
    let current = running.owners[0].source_session.prepared().unwrap();
    assert!(
        current.session().receiver_output().unwrap().generation().unwrap()
            > old.session().receiver_output().unwrap().generation().unwrap()
    );
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn stale_group_and_requester_boot_refuse_before_persist_or_native_write() {
    let mut running = Running::start(false).await;
    let owner = &mut running.owners[0];
    let current = boot(owner.links.boot());
    let group = owner.group.applied().group;
    let option = dto::LocalOption::ForwardingEnabled;
    let revision = lock(&owner.store).owner().revision(option).unwrap();
    let settings = owner.settings.clone();
    assert_eq!(
        owner
            .set_option(&current, GroupId([0; 16]), option, revision, false.write())
            .unwrap_err(),
        dto::ControlFailure::NotMember
    );
    let mut stale = current;
    stale
        .edit(|raw| {
            raw.insert(2, FixedBytes([99; 16]).write());
        })
        .unwrap();
    assert_eq!(
        owner
            .set_option(&stale, group, option, revision, false.write())
            .unwrap_err(),
        dto::ControlFailure::StaleBoot
    );
    assert_eq!(owner.settings, settings);
    assert_eq!(lock(&owner.store).owner().revision(option), Some(revision));
    running.stop().await;
}
