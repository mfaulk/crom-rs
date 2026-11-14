use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Run `f` on a helper thread and fail (instead of hanging) if it takes
/// longer than five seconds.
fn within_5s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("pipeline did not finish within 5s")
}

fn inputs(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("item{i}")).collect()
}

#[test]
fn single_worker_maps_every_input() {
    let out = within_5s(|| pipeline::run(inputs(3), 1, |s| s.to_uppercase()));
    assert_eq!(out, ["ITEM0", "ITEM1", "ITEM2"]);
}

#[test]
fn empty_input_finishes() {
    let out = within_5s(|| pipeline::run(Vec::new(), 4, |s| s.to_string()));
    assert!(out.is_empty());
}

#[test]
fn stats_count_every_job() {
    let (out, stats) =
        within_5s(|| pipeline::run_with_stats(inputs(20), 3, |s| s.len().to_string()));
    assert_eq!(out.len(), 20);
    assert_eq!(stats.per_worker.len(), 3);
    assert_eq!(stats.total(), 20);
}
