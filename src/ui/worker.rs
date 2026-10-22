//! Running blocking work off the GTK main thread (tasks 5.4, 10.3; architecture §8).
//!
//! GTK is single-threaded: widgets may only be touched from the main thread, and
//! anything that blocks that thread freezes the whole window. Work that may block —
//! the startup load (detection plus parsing every backing file), the laptop-display
//! toggle script — therefore runs on relm4's blocking thread pool, and only its
//! *result* comes back to the main thread, through a channel the main loop awaits.
//! [`run_off_main_thread`] is that one hop, shared so every caller hands results
//! back the same way.
//!
//! The work itself stays in the GTK-free `core/` layer (which may not import relm4
//! or glib, R6.2); this module only moves it between threads.

/// Runs `work` on relm4's blocking thread pool and hands its result to `on_done` on
/// the thread that called this — the GTK main thread in the app.
///
/// Returns at once: the calling thread is never blocked by `work`, which is what
/// keeps the window responsive while a slow command runs. `on_done` runs later,
/// from the main loop, once the result has arrived; that is where widgets may be
/// touched again. It receives `None` when the worker ended without sending a result
/// (it panicked), which every caller must treat as "no result", never as an error to
/// unwrap. If the window closes while the work runs, the main loop that would
/// deliver the result is gone: the result is discarded and `on_done` never runs.
///
/// The calling thread must own a running GLib main context (in the app, the GTK main
/// loop), because `on_done` is scheduled on it.
pub(super) fn run_off_main_thread<T, W, D>(work: W, on_done: D)
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    D: FnOnce(Option<T>) + 'static,
{
    let (sender, receiver) = relm4::channel::<T>();
    relm4::spawn_blocking(move || {
        // A send failure only means the receiver was dropped — the window closed
        // before the work finished, which is harmless, so it is not an error.
        if sender.send(work()).is_err() {
            tracing::debug!("worker result discarded; the window closed before it completed");
        }
    });
    relm4::spawn_local(async move { on_done(receiver.recv().await) });
}

/// Iterates `context` until `done` holds, failing the test after ten seconds so a
/// lost result shows up as a failure rather than a hung test run.
///
/// Tests stand in for the GTK main loop with their own [`gtk4::glib::MainContext`]:
/// building widgets needs a display server the test suite does not have, but the
/// main-loop hop of [`run_off_main_thread`] needs only a GLib context, which works
/// headlessly.
#[cfg(test)]
pub(super) fn drive_until(context: &gtk4::glib::MainContext, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker result never arrived on the main context"
        );
        if !context.iteration(false) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::thread::{self, ThreadId};
    use std::time::Duration;

    use gtk4::glib::MainContext;

    use super::{drive_until, run_off_main_thread};

    /// The thread the work ran on (`None` if no result arrived) and the thread the
    /// result was delivered on.
    type Delivery = (Option<ThreadId>, ThreadId);

    #[test]
    fn the_work_runs_elsewhere_and_its_result_arrives_on_the_calling_thread() {
        // Task 10.3: the caller returns while the work is still blocked, and the
        // result is delivered by the main loop on the caller's thread, not on the
        // worker's.
        let context = MainContext::new();
        context
            .with_thread_default(|| {
                let caller = thread::current().id();
                let (release, gate) = mpsc::channel::<()>();
                let delivered: Rc<RefCell<Option<Delivery>>> = Rc::default();

                let sink = Rc::clone(&delivered);
                run_off_main_thread(
                    move || {
                        gate.recv_timeout(Duration::from_secs(10))
                            .expect("the test releases the work");
                        thread::current().id()
                    },
                    move |worker| *sink.borrow_mut() = Some((worker, thread::current().id())),
                );

                // The call returned although the work cannot have finished: it is
                // still waiting for the release below.
                assert!(delivered.borrow().is_none());
                release.send(()).expect("the worker is waiting");

                drive_until(&context, || delivered.borrow().is_some());
                let (worker, receiver) = delivered.borrow().expect("delivered");
                assert_ne!(worker, Some(caller), "the work ran on another thread");
                assert_eq!(receiver, caller, "the result came back to the caller");
            })
            .expect("the test thread owns its main context");
    }

    #[test]
    fn a_worker_that_panics_delivers_no_result() {
        // A panicking worker drops its sender, so `on_done` sees `None` instead of
        // never running — the caller can always re-arm whatever it disabled.
        let context = MainContext::new();
        context
            .with_thread_default(|| {
                let delivered: Rc<RefCell<Option<Option<()>>>> = Rc::default();
                let sink = Rc::clone(&delivered);
                run_off_main_thread(
                    || panic!("worker failure under test"),
                    move |result: Option<()>| *sink.borrow_mut() = Some(result),
                );
                drive_until(&context, || delivered.borrow().is_some());
                assert_eq!(*delivered.borrow(), Some(None));
            })
            .expect("the test thread owns its main context");
    }
}
