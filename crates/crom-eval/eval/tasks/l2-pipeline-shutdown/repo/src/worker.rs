use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::job::{Done, Job};

/// The function every worker applies to its inputs.
pub type Task = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Start worker `id`. It pulls jobs from the shared queue until the queue is
/// closed and sends each result back on `results`.
pub fn spawn(
    id: usize,
    jobs: Arc<Mutex<Receiver<Job>>>,
    results: Sender<Done>,
    task: Task,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("worker-{id}"))
        .spawn(move || {
            while let Ok(job) = jobs.lock().expect("queue lock").recv() {
                let output = task(&job.input);
                let done = Done { worker: id, output };
                if results.send(done).is_err() {
                    break;
                }
            }
        })
        .expect("spawn worker thread")
}
