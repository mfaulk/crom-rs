use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

fn within_10s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("pipeline did not finish within 10s")
}

#[test]
fn outputs_follow_input_order_even_when_completion_order_differs() {
    // Earlier inputs sleep longer, so they finish last.
    let inputs: Vec<String> = (0..6).map(|i| i.to_string()).collect();
    let out = within_10s(move || {
        pipeline::run(inputs, 6, |s| {
            let i: u64 = s.parse().unwrap();
            thread::sleep(Duration::from_millis(40 * (6 - i)));
            format!("<{s}>")
        })
    });
    assert_eq!(out, ["<0>", "<1>", "<2>", "<3>", "<4>", "<5>"]);
}

#[test]
fn jobs_actually_run_concurrently() {
    // Each job waits (up to 3s) until all four jobs have started. If workers
    // are serialized, the jobs time out and report "alone".
    let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let g = Arc::clone(&gate);
    let inputs: Vec<String> = (0..4).map(|i| i.to_string()).collect();
    let out = within_10s(move || {
        pipeline::run(inputs, 4, move |_| {
            let (lock, cv) = &*g;
            let mut started = lock.lock().unwrap();
            *started += 1;
            cv.notify_all();
            let (started, res) = cv
                .wait_timeout_while(started, Duration::from_secs(3), |n| *n < 4)
                .unwrap();
            drop(started);
            if res.timed_out() {
                "alone".to_string()
            } else {
                "together".to_string()
            }
        })
    });
    assert_eq!(out, ["together"; 4]);
}

#[test]
fn many_jobs_more_workers_than_jobs_and_stats() {
    let inputs: Vec<String> = (0..200).map(|i| i.to_string()).collect();
    let expected: Vec<String> = (0..200).map(|i| (i * 2).to_string()).collect();
    let (out, stats) = within_10s(move || {
        pipeline::run_with_stats(inputs, 8, |s| (s.parse::<u32>().unwrap() * 2).to_string())
    });
    assert_eq!(out, expected);
    assert_eq!(stats.total(), 200);

    let out = within_10s(|| pipeline::run(vec!["a".into(), "b".into()], 16, |s| s.repeat(2)));
    assert_eq!(out, ["aa", "bb"]);
}
