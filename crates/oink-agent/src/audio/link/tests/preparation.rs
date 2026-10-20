//! Real v1 M0 peers with fake capture/output endpoints.
use super::*;

#[tokio::test]
async fn unresponsive_candidate_dial_does_not_block_live_media() {
    let running = Running::start(false).await;
    let source = &running.owners[0].links;
    let exit = running.owners[1].links.id();
    let link = source.links().by_peer(exit).unwrap().link;
    let read = ForwardTable::sink_reader(running.owners[1].streams.forward_table(), source.id());
    // A socket that never answers keeps the real QUIC candidate pending for its
    // entire deadline. A different, authenticated peer must still carry M1.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    source
        .dial(
            DeviceId([200; 32]),
            vec![silent.local_addr().unwrap()],
            oink_network::link::DialMode::PreferDirect,
        )
        .unwrap();
    for _ in 0..16 {
        running.supply(0, 0.2);
        tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                if read().1.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("a pending dial must not block another peer's datagrams");
    }
    assert_eq!(source.links().by_peer(exit).unwrap().link, link);
    running.stop().await;
}

#[tokio::test]
async fn packet_outage_during_refill_resumes_on_the_same_live_quic_session() {
    let running = Running::start(false).await;
    let settings = running.owners[0].settings.clone();
    let session = running.owners[0].source_session.prepared().unwrap().clone();
    let read = ForwardTable::sink_reader(running.owners[1].streams.forward_table(), running.owners[0].links.id());
    async fn receive(
        running: &Running,
        read: &dyn Fn() -> (bool, Option<oink_audio::sink::Packet>),
    ) -> oink_audio::sink::Packet {
        running.supply(0, 0.2);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(packet) = read().1 {
                    return packet;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("real QUIC delivers the fake capture packet")
    }
    let mut config = session.profile.config();
    config.jitter.adaptive = false;
    let mut dsp = dsp::Receiver::new_with_config(config).unwrap();
    let mut pcm = [0.0; dsp::FRAME_SAMPLES];
    let origin = Instant::now();
    let packet = receive(&running, &*read).await;
    assert!(dsp.push_slice_at(
        packet.sequence,
        &packet.payload[..packet.len],
        packet.arrived,
        packet.source_emit
    ));
    // Render through the initial loss until the receiver enters refill.
    for tick in 0..40 {
        dsp.render_into(
            origin + Duration::from_millis(tick * 10),
            config.frame_size(),
            &mut pcm[..config.frame_samples()],
            None,
        );
    }
    assert!(dsp.stats().rebufferings > 0);
    for _ in 0..20 {
        let _ = receive(&running, &*read).await;
    }
    let partial = receive(&running, &*read).await;
    assert!(dsp.push_slice_at(
        partial.sequence,
        &partial.payload[..partial.len],
        partial.arrived,
        partial.source_emit
    ));
    // Drop media while the same authenticated link, claim and source are live.
    // 300 packets exceeds every negotiated jitter window without a wall-clock wait.
    for _ in 0..300 {
        let _ = receive(&running, &*read).await;
    }
    let resumed = receive(&running, &*read).await;
    assert!(
        dsp.push_slice_at(
            resumed.sequence,
            &resumed.payload[..resumed.len],
            resumed.arrived,
            resumed.source_emit
        ),
        "restored media must unpin the partial refill window"
    );
    for _ in 0..config.jitter.initial_frames {
        let packet = receive(&running, &*read).await;
        assert!(dsp.push_slice_at(
            packet.sequence,
            &packet.payload[..packet.len],
            packet.arrived,
            packet.source_emit
        ));
    }
    dsp.render_into(
        origin + Duration::from_secs(2),
        config.frame_size(),
        &mut pcm[..config.frame_samples()],
        None,
    );
    assert!(pcm[..config.frame_samples()].iter().any(|sample| sample.abs() > 0.1));
    assert_eq!(running.owners[0].source_session.prepared(), Some(&session));
    assert_eq!(running.owners[0].settings, settings);
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn link_loss_retires_source_offer_and_resumes_without_preference_changes() {
    let mut running = Running::start(true).await;
    let source = running.owners[0].links.clone();
    let exit = running.owners[1].links.clone();
    let old = running.owners[0].source_session.prepared().unwrap().clone();
    let settings = running.owners[0].settings.clone();
    let link = source.links().by_peer(exit.id()).unwrap().link;
    oink_network::link::testing::set_direct_blocked(&source, exit.id(), true);
    oink_network::link::testing::set_direct_blocked(&exit, source.id(), true);
    source.disconnect(link, oink_network::link::Disconnect::ControlStreamFailed);
    running
        .until(|r| r.owners[0].capture.is_none() && r.owners[0].selected.is_none())
        .await;
    assert_eq!(running.owners[0].settings, settings);
    assert!(
        running.owners[0].source_session.offer().is_none(),
        "a lost link must not retain an offer whose M0 opening was discarded"
    );
    oink_network::link::testing::set_direct_blocked(&source, exit.id(), false);
    oink_network::link::testing::set_direct_blocked(&exit, source.id(), false);
    running.until(Running::prepared_route_ready).await;
    let current = running.owners[0].source_session.prepared().unwrap();
    assert_ne!(current.session(), old.session());
    assert_eq!(running.owners[0].settings, settings);
    let sent = running.owners[0].sent.load(Ordering::Relaxed);
    running.supply(0, 0.1);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > sent);
    assert_eq!(pauses(&running.platforms[0]), 0);
    running.stop().await;
}

#[tokio::test]
async fn prepared_ack_is_required_and_claim_retake_never_revives_old_session() {
    let mut running = Running::new().await;
    running.held_prepared_acks = Some(Vec::new());
    running.claim(1, true);
    running
        .until(|r| r.owners[0].capture.is_some() && !r.held_prepared_acks.as_ref().unwrap().is_empty())
        .await;
    // Source capture follows sending the ACK, not its remote consumption.
    // Keep the real M0 ACK pending to force the CI observation order.
    let source = running.owners[0].links.id();
    assert!(!running.owners[1].receive_sessions[&source].ready());
    assert!(
        !running.prepared_route_ready(),
        "source capture alone cannot establish acknowledged receiver readiness"
    );
    for (index, peer, message) in running.held_prepared_acks.take().unwrap() {
        running.owners[index].message(peer, message);
    }
    running.until(Running::prepared_route_ready).await;
    running.supply(0, 0.1);
    assert!(running.owners[0].sent.load(Ordering::Relaxed) > 0);
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
    running.until(Running::prepared_route_ready).await;
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
                && r.prepared_route_ready()
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
