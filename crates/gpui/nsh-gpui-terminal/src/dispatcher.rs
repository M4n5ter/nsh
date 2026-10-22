use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle, ThreadId},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use gpui::{PlatformDispatcher, Priority, RunnableVariant};

type Wake = Arc<dyn Fn() + Send + Sync>;

enum Work {
    Task(RunnableVariant),
    Realtime(Box<dyn FnOnce() + Send>),
}

struct Timer {
    due: Instant,
    runnable: RunnableVariant,
}

/// GPUI's production scheduler, with the embedding thread owning foreground work.
/// Timers are drained by that same pump; there is no terminal or per-timer thread.
pub(crate) struct Dispatcher {
    owner: ThreadId,
    main_tx: Mutex<Option<Sender<RunnableVariant>>>,
    main_rx: Receiver<RunnableVariant>,
    background: Mutex<Option<Sender<Work>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    timers: Mutex<Vec<Timer>>,
    stopped: AtomicBool,
    wake: Wake,
}

impl Dispatcher {
    pub(crate) fn new(wake: Wake) -> std::io::Result<Arc<Self>> {
        let (main_tx, main_rx) = unbounded();
        let (tx, rx) = unbounded();
        let dispatcher = Arc::new(Self {
            owner: thread::current().id(),
            main_tx: Mutex::new(Some(main_tx)),
            main_rx,
            background: Mutex::new(Some(tx)),
            workers: Mutex::new(Vec::new()),
            timers: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            wake,
        });
        // Account for the foreground owner when sizing the shared executor.
        let worker_count = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1);
        for index in 0..worker_count {
            let rx = rx.clone();
            let worker = thread::Builder::new()
                .name(format!("nsh-gpui-background-{index}"))
                .spawn(move || {
                    while let Ok(work) = rx.recv() {
                        match work {
                            Work::Task(runnable) => {
                                runnable.run();
                            }
                            Work::Realtime(run) => run(),
                        }
                    }
                });
            match worker {
                Ok(worker) => dispatcher.workers.lock().unwrap().push(worker),
                Err(error) => {
                    dispatcher.shutdown();
                    return Err(error);
                }
            }
        }
        Ok(dispatcher)
    }

    pub(crate) fn pump(&self) -> bool {
        assert!(
            self.is_main_thread(),
            "GPUI foreground work belongs to its creating thread"
        );
        let now = Instant::now();
        let due = {
            let mut timers = self.timers.lock().unwrap();
            let mut due = Vec::new();
            while due.len() < 64 && timers.first().is_some_and(|timer| timer.due <= now) {
                due.push(timers.remove(0).runnable);
            }
            due
        };
        for runnable in due {
            self.dispatch_on_main_thread(runnable, Priority::default());
        }
        let mut worked = false;
        for _ in 0..64 {
            let Ok(runnable) = self.main_rx.try_recv() else {
                break;
            };
            runnable.run();
            worked = true;
        }
        if self.has_pending() {
            (self.wake)();
        }
        worked
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.main_rx.is_empty()
            || self
                .next_deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.timers.lock().unwrap().first().map(|timer| timer.due)
    }

    pub(crate) fn shutdown(&self) {
        assert!(
            self.is_main_thread(),
            "GPUI local tasks must be cancelled on their owner thread"
        );
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.main_tx.lock().unwrap().take();
        self.background.lock().unwrap().take();
        let timers = std::mem::take(&mut *self.timers.lock().unwrap());
        drop(timers);
        while self.main_rx.try_recv().is_ok() {}
        // Existing background work may contain blocking application code.
        // Closing its queue lets the fixed workers finish without delaying
        // the external host's terminal restoration.
        self.workers.lock().unwrap().clear();
    }
}

impl PlatformDispatcher for Dispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.owner
    }

    fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
        let sender = self.background.lock().unwrap().clone();
        if let Some(sender) = sender {
            let _ = sender.send(Work::Task(runnable));
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let main = self.main_tx.lock().unwrap();
        let Some(sender) = main.as_ref() else {
            // A local runnable can own !Send state. During shutdown a worker
            // must not destroy it on the wrong thread (as in LinuxDispatcher).
            std::mem::forget(runnable);
            return;
        };
        if let Err(error) = sender.send(runnable) {
            std::mem::forget(error.0);
            return;
        }
        drop(main);
        (self.wake)();
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let Some(due) = Instant::now().checked_add(duration) else {
            std::mem::forget(runnable);
            return;
        };
        let mut timers = self.timers.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            std::mem::forget(runnable);
            return;
        }
        let index = timers.partition_point(|timer| timer.due <= due);
        timers.insert(index, Timer { due, runnable });
        drop(timers);
        (self.wake)();
    }

    fn spawn_realtime(&self, run: Box<dyn FnOnce() + Send>) {
        // This terminal host does not promise an OS audio scheduling class.
        // Keep the work bounded by the existing pool rather than spawning
        // an unbounded number of dedicated threads.
        let sender = self.background.lock().unwrap().clone();
        if let Some(sender) = sender {
            let _ = sender.send(Work::Realtime(run));
        }
    }
}
