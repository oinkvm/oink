//! A committed group can move again while a noninitiating member's catalog lags.
use super::*;

#[test]
fn second_merge_adopts_membership_before_a_noninitiating_members_catalog_arrives() {
    crate::test_watchdog::run(
        module_path!(),
        "catalog handoff across successive merges",
        Some(6),
        async {
            let dirs = [
                tempfile::tempdir().unwrap(),
                tempfile::tempdir().unwrap(),
                tempfile::tempdir().unwrap(),
            ];
            let displays = [Displays::new(), Displays::new(), Displays::new()];
            let a = start(dirs[0].path(), displays[0].clone()).await;
            let b = start(dirs[1].path(), displays[1].clone()).await;
            let c = start(dirs[2].path(), displays[2].clone()).await;
            let original = query(&a).await;
            query(&b).await;
            query(&c).await;
            let retained = lock(&a.inner.profiles.data).catalog.clone().unwrap();
            a.inner.profiles.defer_catalogs.store(true, Ordering::SeqCst);
            common::pair(&a, &b).await;
            let intermediate = b.group_id().unwrap();
            common::until("first acceptor has merged the saved list", || {
                lock(&b.inner.profiles.data)
                    .catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.group.0 == intermediate && !catalog.completed_joins.is_empty())
            })
            .await;
            assert!(
                super::super::store::load(dirs[0].path(), intermediate.into())
                    .unwrap()
                    .is_none()
            );
            common::until("first membership owner facts are visible", || {
                a.profile_snapshot().is_ok()
            })
            .await;
            assert_eq!(query(&a).await.readiness, Readiness::Syncing);
            // B owns the converged source list; A is only an observing member.
            // Holding catalog delivery makes this ordering independent of CPU speed.
            common::pair(&b, &c).await;
            let target = c.group_id().unwrap();
            common::until(
                "observer adopts the second committed membership without its intermediate catalog",
                || a.group_id() == Some(target),
            )
            .await;
            common::until("second membership owner facts are visible", || {
                a.profile_snapshot().is_ok()
            })
            .await;
            let awaiting = query(&a).await;
            assert_eq!(awaiting.readiness, Readiness::Syncing);
            assert_eq!(awaiting.active_profile_id, original.active_profile_id);
            assert_eq!(lock(&a.inner.profiles.data).catalog.as_ref(), Some(&retained));
            assert!(
                super::super::store::load(dirs[0].path(), target.into())
                    .unwrap()
                    .is_none()
            );
            a.inner.profiles.defer_catalogs.store(false, Ordering::SeqCst);
            common::until("final target catalog reaches the observer", || {
                let catalogs = [&a, &b, &c].map(|n| lock(&n.inner.profiles.data).catalog.clone());
                catalogs[0].as_ref().is_some_and(|catalog| catalog.group.0 == target)
                    && catalogs.iter().all(|catalog| catalog == &catalogs[0])
            })
            .await;
            assert!(displays.iter().all(|d| d.calls.load(Ordering::SeqCst) == 0));
            for node in [&a, &b, &c] {
                node.shutdown().await;
            }
        },
    );
}
