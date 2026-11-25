//! Background file writer. Serializing and writing thousands of annotations
//! takes tens of milliseconds; doing it on the UI thread froze drawing. Jobs
//! run on one worker thread; if several pile up only the newest is written.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub type Job = Box<dyn FnOnce() -> std::io::Result<()> + Send>;

pub struct Saver {
    /// Newest job waiting for the worker (older ones are superseded).
    next: Arc<Mutex<Option<(u64, Job)>>>,
    wake: Sender<()>,
    done: Receiver<(u64, Result<(), String>)>,
    /// Revisions submitted / confirmed written.
    pub submitted: u64,
}

impl Saver {
    pub fn new(ctx: egui::Context) -> Self {
        let next: Arc<Mutex<Option<(u64, Job)>>> = Arc::default();
        let (wake, wake_rx) = channel::<()>();
        let (done_tx, done) = channel();
        let slot = next.clone();
        std::thread::Builder::new()
            .name("saver".into())
            .spawn(move || {
                while wake_rx.recv().is_ok() {
                    let job = slot.lock().unwrap().take();
                    if let Some((rev, job)) = job {
                        let r = job().map_err(|e| e.to_string());
                        let _ = done_tx.send((rev, r));
                        ctx.request_repaint();
                    }
                }
            })
            .expect("saver thread");
        Saver { next, wake, done, submitted: 0 }
    }

    pub fn submit(&mut self, rev: u64, job: Job) {
        *self.next.lock().unwrap() = Some((rev, job));
        self.submitted = rev;
        let _ = self.wake.send(());
    }

    /// Finished writes since the last poll: (revision, result).
    pub fn poll(&self) -> Vec<(u64, Result<(), String>)> {
        self.done.try_iter().collect()
    }
}
