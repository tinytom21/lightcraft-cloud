//! Live progress of the job that is running: the route from the sync crate's progress hook to the published status.
//!
//! A job runs blocking on the engine thread, so nothing on that thread can publish while it works. The worker hands the sync crate a
//! hook that calls [`Feed::report`] (a lock and a copy; the time left is estimated there). A [`Pump`] on a thread of its own looks at
//! the feed about once a second and publishes the status (status.json for the windows, the sink for the tray). Whatever the pump
//! does, however long a write takes or whether it fails, the sync is not waiting for it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lightcraft_cloud_client::{Estimator, Progress, to_ms};

use crate::clock::Clock;

struct Inner {
    latest: Option<Progress>,
    est: Estimator,
}

/// What the running job last reported, with the time left worked out. Shared between the worker's hook and the pump.
pub struct Feed {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
}

impl Feed {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Feed> {
        Arc::new(Feed { inner: Mutex::new(Inner { latest: None, est: Estimator::new() }), clock })
    }

    /// A new job: forget the last one.
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g = Inner { latest: None, est: Estimator::new() };
    }

    /// The job's progress hook. Quick: it runs inside the sync's loops.
    pub fn report(&self, p: &Progress) {
        let now = to_ms(self.clock.now());
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.est.observe(p, now);
        g.latest = Some(*p);
    }

    /// The latest report, with the predicted end of its stage.
    pub fn snapshot(&self) -> Option<Progress> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.latest.map(|p| Progress { finish_ms: g.est.finish_ms(), ..p })
    }
}

#[derive(Default)]
struct Signals {
    stop: Mutex<bool>,
    wake: Condvar,
    done: Mutex<bool>,
    done_wake: Condvar,
}

/// The thread that publishes the feed while a job runs. At most one publish at a time, one per `every`, and only when something
/// changed (or a few intervals have passed, so the file shows the app is alive).
pub struct Pump {
    signals: Arc<Signals>,
    cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        *self.signals.stop.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signals.wake.notify_all();
    }
}

impl Pump {
    /// `publish` is called on the pump's thread with each snapshot worth publishing.
    pub fn start(feed: Arc<Feed>, every: Duration, mut publish: impl FnMut(Progress) + Send + 'static) -> Pump {
        let signals = Arc::new(Signals::default());
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sig, cancel) = (signals.clone(), cancelled.clone());
        let handle = std::thread::spawn(move || {
            let mut last: Option<Progress> = None;
            let mut quiet = 0u32;
            loop {
                {
                    let stop = sig.stop.lock().unwrap_or_else(|e| e.into_inner());
                    let (stop, _) = sig.wake.wait_timeout_while(stop, every, |s| !*s).unwrap_or_else(|e| e.into_inner());
                    if *stop {
                        break;
                    }
                }
                let Some(now) = feed.snapshot() else { continue };
                quiet += 1;
                if last != Some(now) || quiet >= 5 {
                    quiet = 0;
                    last = Some(now);
                    if !cancel.load(Ordering::SeqCst) {
                        // (a publish that panics must not take the pump down with it)
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| publish(now)));
                    }
                }
            }
            *sig.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
            sig.done_wake.notify_all();
        });
        Pump { signals, cancelled, handle: Some(handle) }
    }

    /// Stop publishing. Waits up to `wait` for a publish that is in the middle of a write; a write that takes longer is left to finish
    /// on its own (it publishes nothing more after it).
    pub fn stop(mut self, wait: Duration) {
        self.cancelled.store(true, Ordering::SeqCst);
        *self.signals.stop.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.signals.wake.notify_all();
        let done = self.signals.done.lock().unwrap_or_else(|e| e.into_inner());
        let (done, _) = self.signals.done_wake.wait_timeout_while(done, wait, |d| !*d).unwrap_or_else(|e| e.into_inner());
        if *done && let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
