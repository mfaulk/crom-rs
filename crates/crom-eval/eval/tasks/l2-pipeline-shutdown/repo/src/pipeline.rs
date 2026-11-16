use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::job::Job;
use crate::worker::{self, Task};

/// Per-run statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Number of jobs each worker completed, indexed by worker id.
    pub per_worker: Vec<usize>,
}

impl Stats {
    pub fn total(&self) -> usize {
        self.per_worker.iter().sum()
    }
}

/// Apply `f` to every input on `workers` threads and return the outputs **in
/// input order**. Jobs run concurrently, so `f` may be called from several
/// threads at once. Panics if `workers == 0`.
pub fn run<F>(inputs: Vec<String>, workers: usize, f: F) -> Vec<String>
where
    F: Fn(&str) -> String + Send + Sync + 'static,
{
    run_with_stats(inputs, workers, f).0
}

/// Like [`run`], also reporting how many jobs each worker completed.
pub fn run_with_stats<F>(inputs: Vec<String>, workers: usize, f: F) -> (Vec<String>, Stats)
where
    F: Fn(&str) -> String + Send + Sync + 'static,
{
    assert!(workers > 0, "need at least one worker");
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (res_tx, res_rx) = mpsc::channel();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let task: Task = Arc::new(f);

    let handles: Vec<_> = (0..workers)
        .map(|id| worker::spawn(id, Arc::clone(&job_rx), res_tx.clone(), Arc::clone(&task)))
        .collect();

    for (index, input) in inputs.into_iter().enumerate() {
        job_tx.send(Job { index, input }).expect("workers alive");
    }
    drop(job_tx);

    let mut stats = Stats {
        per_worker: vec![0; workers],
    };
    let mut outputs = Vec::new();
    for done in res_rx.iter() {
        stats.per_worker[done.worker] += 1;
        outputs.push(done.output);
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
    (outputs, stats)
}
