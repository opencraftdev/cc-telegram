//! FIFO queues keyed by conversation ("tg:<chat>", "api:<key>:<session>"): one worker per key,
//! different keys run in parallel. Workers exit when idle. `/stop` kills + clears a key.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

/// A queued unit of work; it receives its own stop handle.
pub type Task = Box<dyn FnOnce(Arc<Notify>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

struct Queued {
    task: Task,
    stop: Arc<Notify>,
    generation: u64,
}

/// The running job's stop handle and start time.
type Running = Arc<Mutex<Option<(Arc<Notify>, Instant)>>>;

#[derive(Clone)]
struct KeyQueue {
    tx: mpsc::UnboundedSender<Queued>,
    pending: Arc<AtomicUsize>,
    /// Bumped by stop(): queued jobs from an older generation are skipped.
    generation: Arc<AtomicU64>,
    running: Running,
}

#[derive(Default)]
pub struct Queues {
    keys: Mutex<HashMap<String, KeyQueue>>,
}

impl Queues {
    /// Enqueue `task` on `key`. The returned handle stops this job (queued or running).
    pub fn push(self: &Arc<Self>, key: &str, task: Task) -> Arc<Notify> {
        let stop = Arc::new(Notify::new());
        // Count under the map lock so an idle worker can't exit between our lookup and send.
        let mut keys = self.keys.lock().unwrap();
        let q = keys
            .entry(key.to_string())
            .or_insert_with(|| spawn_worker(self.clone(), key.to_string()));
        q.pending.fetch_add(1, Ordering::SeqCst);
        let generation = q.generation.load(Ordering::SeqCst);
        let _ = q.tx.send(Queued {
            task,
            stop: stop.clone(),
            generation,
        });
        stop
    }

    /// Kill the running job and drop everything queued: (was something running, jobs cleared).
    pub fn stop(&self, key: &str) -> (bool, usize) {
        let Some(q) = self.keys.lock().unwrap().get(key).cloned() else {
            return (false, 0);
        };
        // Same lock the worker holds while checking the generation and claiming a job.
        let running = q.running.lock().unwrap();
        q.generation.fetch_add(1, Ordering::SeqCst);
        let cleared = q.pending.load(Ordering::SeqCst);
        if let Some((stop, _)) = running.as_ref() {
            stop.notify_one();
        }
        (running.is_some(), cleared)
    }

    /// (queued jobs, how long the current job has been running)
    pub fn info(&self, key: &str) -> (usize, Option<Duration>) {
        match self.keys.lock().unwrap().get(key) {
            None => (0, None),
            Some(q) => (
                q.pending.load(Ordering::SeqCst),
                q.running.lock().unwrap().as_ref().map(|(_, t)| t.elapsed()),
            ),
        }
    }
}

fn spawn_worker(queues: Arc<Queues>, key: String) -> KeyQueue {
    let (tx, mut rx) = mpsc::unbounded_channel::<Queued>();
    let q = KeyQueue {
        tx,
        pending: Arc::default(),
        generation: Arc::default(),
        running: Arc::default(),
    };
    let w = q.clone();
    tokio::spawn(async move {
        while let Some(Queued {
            task,
            stop,
            generation,
        }) = rx.recv().await
        {
            let live = {
                let mut running = w.running.lock().unwrap();
                w.pending.fetch_sub(1, Ordering::SeqCst);
                let live = generation == w.generation.load(Ordering::SeqCst);
                if live {
                    *running = Some((stop.clone(), Instant::now()));
                }
                live
            };
            if live {
                task(stop).await;
                *w.running.lock().unwrap() = None;
            }
            let mut keys = queues.keys.lock().unwrap();
            if w.pending.load(Ordering::SeqCst) == 0 {
                keys.remove(&key);
                break;
            }
        }
    });
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(log: Arc<Mutex<Vec<String>>>, tag: &'static str, ms: u64) -> Task {
        Box::new(move |stop: Arc<Notify>| {
            Box::pin(async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => log.lock().unwrap().push(tag.into()),
                    _ = stop.notified() => log.lock().unwrap().push(format!("{tag}:stopped")),
                }
            })
        })
    }

    #[tokio::test]
    async fn fifo_per_key_parallel_across_keys_and_stop() {
        let q = Arc::new(Queues::default());
        let log = Arc::new(Mutex::new(vec![]));
        q.push("a", task(log.clone(), "a1", 150));
        q.push("a", task(log.clone(), "a2", 10));
        q.push("b", task(log.clone(), "b1", 50));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(*log.lock().unwrap(), vec!["b1", "a1", "a2"]);
        assert!(q.keys.lock().unwrap().is_empty(), "idle workers exit");

        log.lock().unwrap().clear();
        q.push("c", task(log.clone(), "c1", 5_000));
        q.push("c", task(log.clone(), "c2", 10));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(q.stop("c"), (true, 1));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*log.lock().unwrap(), vec!["c1:stopped"]);

        // per-job handle stops a job that is still queued
        log.lock().unwrap().clear();
        q.push("d", task(log.clone(), "d1", 50));
        let h = q.push("d", task(log.clone(), "d2", 5_000));
        h.notify_one();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(*log.lock().unwrap(), vec!["d1", "d2:stopped"]);
    }
}
