//! A process watchdog also bounds synchronous API calls, thread joins and
//! runtime teardown. Tokio timeouts cannot run when their executor is blocked.
use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

type Probe = Arc<dyn Fn() -> String + Send + Sync>;
type State = (Mutex<(bool, String)>, Condvar, Mutex<Vec<(String, Probe)>>);
thread_local! {
    static CURRENT: RefCell<Weak<State>> = const { RefCell::new(Weak::new()) };
}

pub struct Scenario {
    state: Arc<State>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Scenario {
    pub fn new(module: &str, name: &str) -> Self {
        // This is a deadlock watchdog, not a product performance budget. It
        // allows the 60-second collection deadline plus setup and teardown.
        Self::with_limit(format!("{module}::{name}"), Duration::from_secs(180), "scenario entry")
    }
    pub(crate) fn with_limit(name: String, limit: Duration, initial_phase: &str) -> Self {
        let state = Arc::new((
            Mutex::new((false, String::new())),
            Condvar::new(),
            Mutex::new(Vec::new()),
        ));
        CURRENT.with(|current| *current.borrow_mut() = Arc::downgrade(&state));
        phase(initial_phase);
        let waiting = state.clone();
        let worker = std::thread::Builder::new()
            .name("scenario-watchdog".into())
            .spawn(move || {
                let (lock, wake, diagnostics) = &*waiting;
                let (finished, _) = wake
                    .wait_timeout_while(lock.lock().unwrap(), limit, |state| !state.0)
                    .unwrap();
                if !finished.0 {
                    // Bypass libtest capture: exit() cannot return its buffered
                    // output to the harness, so eprintln! loses the last phase.
                    use std::io::Write;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "SCENARIO WATCHDOG: {name}; pid={}; limit={limit:?}; last phase={}; synchronous call, blocked worker or teardown did not finish",
                        std::process::id(), finished.1
                    );
                    drop(finished);
                    // Probes run only on failure, outside the executor and the
                    // phase lock. A blocked/poisoned diagnostic must not hide the
                    // original watchdog or prevent later probes from being read.
                    match diagnostics.try_lock() {
                        Ok(probes) => {
                            for (label, probe) in probes.iter() {
                                let _ = writeln!(std::io::stderr().lock(), "WATCHDOG DIAGNOSTIC: {label}");
                                let dump = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe()))
                                    .unwrap_or_else(|_| "diagnostic panicked".into());
                                let _ = writeln!(std::io::stderr().lock(), "{dump}");
                            }
                        }
                        Err(error) => {
                            let _ = writeln!(std::io::stderr().lock(), "watchdog diagnostics unavailable: {error}");
                        }
                    }
                    // A panic here would only kill this watchdog thread. Exiting
                    // makes the test fail even when runtime Drop is waiting for
                    // an uninterruptible spawn_blocking task or native thread.
                    std::process::exit(101);
                }
            })
            .unwrap();
        Self {
            state,
            worker: Some(worker),
        }
    }
}
impl Drop for Scenario {
    fn drop(&mut self) {
        self.state.0.lock().unwrap().0 = true;
        self.state.1.notify_one();
        self.worker.take().unwrap().join().unwrap();
        CURRENT.with(|current| *current.borrow_mut() = Weak::new());
    }
}

/// Observe an asynchronous boundary without asserting machine scheduling time.
/// The enclosing Scenario bounds the whole executor and teardown independently.
#[allow(dead_code)] // Shared with binaries that only use synchronous guards.
pub async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    phase(what);
    while !condition() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Keep the watchdog outside the runtime, including unwind/teardown. A timeout
/// panic inside a future can otherwise wait forever for a blocking file reader.
pub fn run(module: &str, name: &str, workers: Option<usize>, scenario: impl std::future::Future<Output = ()>) {
    let _scenario = Scenario::new(module, name);
    run_runtime(workers, scenario);
}

fn run_runtime(workers: Option<usize>, scenario: impl std::future::Future<Output = ()>) {
    let mut builder = match workers {
        Some(count) => {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(count);
            builder
        }
        None => tokio::runtime::Builder::new_current_thread(),
    };
    let runtime = builder.enable_all().build().unwrap();
    runtime.block_on(scenario);
    phase("runtime teardown");
    drop(runtime);
}

pub fn phase(phase: &str) {
    CURRENT.with(|current| {
        if let Some(state) = current.borrow().upgrade() {
            state.0.lock().unwrap().1 = phase.into();
        }
    });
}

/// Register a failure-only, bounded probe. Capture weak handles so diagnostics
/// cannot keep a node/provider alive through normal shutdown or runtime Drop.
pub fn on_timeout(label: impl Into<String>, probe: impl Fn() -> String + Send + Sync + 'static) {
    CURRENT.with(|current| {
        if let Some(state) = current.borrow().upgrade() {
            state.2.lock().unwrap().push((label.into(), Arc::new(probe)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_watchdog_bounds_a_blocked_executor() {
        const CHILD: &str = "OINK_TEST_WATCHDOG_CHILD";
        if let Ok(case) = std::env::var(CHILD) {
            // Build the executor and enter the blocking task before arming the
            // short fixture deadline. Runtime/thread startup is not under test.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            if case == "executor" {
                runtime.block_on(async {
                    let _scenario = Scenario::with_limit(
                        format!("{}::watchdog fixture", module_path!()),
                        Duration::from_millis(50),
                        "blocked synchronous call",
                    );
                    let locked = Arc::new(Mutex::new(()));
                    let read = locked.clone();
                    on_timeout("fixture state", move || {
                        format!("native_lock_unavailable={}", read.try_lock().is_err())
                    });
                    on_timeout("broken probe", || panic!("diagnostic fixture panic"));
                    on_timeout("after broken probe", || "routing=blocked; grant=pending".into());
                    let _held = locked.lock().unwrap();
                    loop {
                        std::thread::park();
                    }
                });
            } else {
                runtime.block_on(async {
                    let (ready, entered) = tokio::sync::oneshot::channel();
                    tokio::task::spawn_blocking(move || {
                        ready.send(()).unwrap();
                        loop {
                            std::thread::park();
                        }
                    });
                    entered.await.unwrap();
                });
                let _scenario = Scenario::with_limit(
                    format!("{}::watchdog fixture", module_path!()),
                    Duration::from_millis(50),
                    "blocked runtime teardown",
                );
                on_timeout("fixture teardown", || "native publication=pending".into());
                // Drop the runtime before its watchdog when unwinding.
                let _runtime = runtime;
                panic!("force unwind with an unfinished blocking task");
            }
            unreachable!();
        }
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_called = called.clone();
        run(module_path!(), "normal completion", None, async {
            on_timeout("must stay silent", move || {
                probe_called.store(true, std::sync::atomic::Ordering::Relaxed);
                "unexpected diagnostic".into()
            });
        });
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
        let test = format!("{}::process_watchdog_bounds_a_blocked_executor", module_path!());
        let test = test.split_once("::").unwrap().1;
        for case in ["executor", "teardown"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test])
                .env(CHILD, case)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!("watchdog failed to end the blocked test process");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(101));
            let message = String::from_utf8(output.stderr).unwrap();
            assert!(message.contains("SCENARIO WATCHDOG"), "{message}");
            if case == "executor" {
                assert!(message.contains("native_lock_unavailable=true"), "{message}");
                assert!(message.contains("diagnostic panicked"), "{message}");
                assert!(message.contains("routing=blocked; grant=pending"), "{message}");
            } else {
                assert!(message.contains("native publication=pending"), "{message}");
            }
            assert!(
                message.contains(if case == "executor" {
                    "blocked synchronous call"
                } else {
                    "blocked runtime teardown"
                }),
                "{message}"
            );
        }
    }
}
