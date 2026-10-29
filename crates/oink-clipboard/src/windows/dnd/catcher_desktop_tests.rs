//! Catcher lifetime checks without an installed Oink. Visibility acceptance
//! tests require a locked interactive desktop; the cancellation regression
//! uses only an invisible HWND on an isolated noninteractive station.
use super::*;
use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, PostQuitMessage};

struct Fixture(WinDnd);
impl Fixture {
    fn new() -> Self {
        let backend = WinDnd {
            shared: Arc::new(Shared {
                native_jobs: OnceLock::new(),
                sink: OnceLock::new(),
                inject: OnceLock::new(),
                source_press: OnceLock::new(),
                source_start: OnceLock::new(),
                st: Mutex::new(St::default()),
                cv: Condvar::new(),
            }),
            thread: OnceLock::new(),
        };
        // Deliberately reject injection: terminal window cleanup must still run.
        backend
            .start(
                Arc::new(|_| {}),
                Arc::new(|_| Err("fixture rejects native input".into())),
            )
            .unwrap();
        Self(backend)
    }
    fn visible(&self) -> bool {
        // SAFETY: only a read of this test process's owned catcher handle.
        unsafe { IsWindowVisible(HWND(self.0.thread().unwrap().catcher as _)).as_bool() }
    }
    fn held(&self) -> DragOperation {
        {
            let mut st = self.0.shared.lock();
            st.held_source = None;
            st.probing = false;
        }
        self.0.probe(Point::new(64., 64.));
        let operation = DragOperation::new();
        let shared = self.0.shared.clone();
        self.0
            .run(move || {
                let mut st = shared.lock();
                st.source_present = true;
                st.source_operation = Some(operation);
                st.probe_operation = Some(operation);
                st.held_source = Some(operation);
            })
            .unwrap();
        assert!(self.visible());
        operation
    }
    fn flush(&self) {
        self.0.run(|| ()).expect("private STA drained its cleanup");
    }
    fn block_sta(&self) -> mpsc::Sender<()> {
        let (entered, waiting) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let thread = self.0.thread().unwrap();
        thread
            .jobs
            .send(Box::new(move || {
                let _ = entered.send(());
                let _ = gate.recv();
            }))
            .unwrap();
        // SAFETY: private test STA, asynchronous job wake.
        unsafe {
            PostMessageW(Some(HWND(thread.catcher as _)), WM_APP_JOB, WPARAM(0), LPARAM(0)).unwrap();
        }
        waiting.recv_timeout(WAIT).unwrap();
        release
    }
}

#[test]
fn successful_cancel_keeps_catcher_until_native_end_on_private_station() {
    const CHILD: &str = "OINK_CANCEL_PRIVATE_STATION";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::dnd::catcher_desktop_tests::successful_cancel_keeps_catcher_until_native_end_on_private_station",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "private native cancellation test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _station = super::super::settle_tests::PrivateStation::new();
    // Create and drain the real cleanup queue on this private-desktop thread.
    // No input is sent and the HWND stays hidden. This proves ownership/queue
    // lifetime, not native input delivery or third-party application behavior.
    // SAFETY: an invisible window owned by this isolated station/thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!("private cancellation fixture"),
            WS_POPUP,
            0,
            0,
            SIZE,
            SIZE,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    };
    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            // SAFETY: only this test thread's private-station HWND.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
            }
        }
    }
    let _window = Window(hwnd);
    let operation = DragOperation::new();
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let observed = inputs.clone();
    let reject = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rejecting = reject.clone();
    let (jobs, queued) = mpsc::channel::<Job>();
    let shared = Arc::new(Shared {
        native_jobs: OnceLock::from((jobs, hwnd.0 as isize)),
        sink: OnceLock::new(),
        inject: OnceLock::from(Arc::new(move |event: &InjectEvent| {
            observed.lock().unwrap().push(event.clone());
            if rejecting.load(Ordering::Acquire) {
                Err("rejected Escape".into())
            } else {
                Ok(())
            }
        }) as Injector),
        source_press: OnceLock::new(),
        source_start: OnceLock::new(),
        st: Mutex::new(St {
            probing: true,
            source_present: true,
            source_operation: Some(operation),
            probe_operation: Some(operation),
            held_source: Some(operation),
            ..St::default()
        }),
        cv: Condvar::new(),
    });
    let backend = WinDnd {
        shared: shared.clone(),
        thread: OnceLock::new(),
    };
    assert!(shared.lock().probing);
    assert!(backend.finish_source(operation, false, &|| Ok(())));
    for job in queued.try_iter() {
        job();
    }
    assert!(
        shared.lock().probing,
        "injection submission is not native cancellation: a queued positioned UP still needs the catcher"
    );
    assert_eq!(shared.lock().probe_operation, Some(operation));
    assert_eq!(inputs.lock().unwrap().len(), 2);
    shared.source_ended(false, 3);
    for job in queued.try_iter() {
        job();
    }
    assert!(!shared.lock().probing, "native end owes catcher teardown");
    assert_eq!(shared.lock().probe_operation, None);
    for reject_escape in [true, false] {
        let operation = DragOperation::new();
        {
            let mut st = shared.lock();
            st.probing = true;
            st.source_present = true;
            st.source_operation = Some(operation);
            st.probe_operation = Some(operation);
            st.held_source = Some(operation);
        }
        reject.store(reject_escape, Ordering::Release);
        assert!(!backend.finish_source(operation, false, &|| Err("rejected release".into())));
        for job in queued.try_iter() {
            job();
        }
        assert!(
            !shared.lock().probing,
            "failed cancellation must still retire its catcher"
        );
        assert_eq!(shared.lock().probe_operation, None);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.run(|| {
            // SAFETY: terminates only this fixture's private OLE thread.
            unsafe {
                PostQuitMessage(0);
            }
        });
    }
}

#[test]
#[ignore = "interactive Windows desktop; run on a reserved desktop"]
fn terminal_catcher_cleanup_survives_slow_sta_and_preserves_new_capture() {
    let fixture = Fixture::new();
    for keyed in [true, false] {
        let operation = fixture.held();
        let release = fixture.block_sta();
        if keyed {
            fixture.0.unprobe_source(operation);
        } else {
            fixture.0.unprobe();
        }
        // The native queue remains blocked beyond the old run() expiry. Both
        // cleanup requests must still hide the real window after it resumes.
        std::thread::sleep(WAIT + Duration::from_millis(100));
        release.send(()).unwrap();
        fixture.flush();
        assert!(!fixture.visible(), "queued teardown expired: keyed={keyed}");
        eprintln!("catcher cleanup keyed={keyed} after slow STA: hidden");
    }
    let old = fixture.held();
    let newer = DragOperation::new();
    {
        let mut st = fixture.0.shared.lock();
        st.source_operation = Some(newer);
        st.probe_operation = Some(newer);
        st.held_source = Some(newer);
    }
    fixture.0.unprobe_source(old);
    fixture.flush();
    assert!(fixture.visible(), "old source cleanup hid a newer capture");
    assert_eq!(fixture.0.shared.lock().probe_operation, Some(newer));
    eprintln!("obsolete keyed cleanup: newer catcher remains visible");

    for (accepted, reason) in [(false, 3), (false, 4), (true, 2)] {
        fixture.held();
        let shared = fixture.0.shared.clone();
        fixture.0.run(move || shared.source_ended(accepted, reason)).unwrap();
        fixture.flush();
        assert!(!fixture.visible(), "terminal callback left catcher visible: {reason}");
        eprintln!("native end reason={reason} accepted={accepted}: hidden");
    }
    let operation = fixture.held();
    assert!(!fixture.0.finish_source(operation, false, &|| Ok(())));
    fixture.flush();
    assert!(!fixture.visible(), "rejected cancel injection left catcher visible");
    eprintln!("cancel injection rejected: hidden");

    let operation = fixture.held();
    assert!(fixture.0.finish_source(operation, true, &|| Ok(())));
    fixture.flush();
    assert!(
        fixture.visible(),
        "COPY must retain catcher until its actual native Drop"
    );
    let shared = fixture.0.shared.clone();
    fixture.0.run(move || shared.source_ended(true, 2)).unwrap();
    fixture.flush();
    assert!(!fixture.visible());
    eprintln!("COPY: visible until actual Drop, then hidden");
}
