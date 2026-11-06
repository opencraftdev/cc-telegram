//! One FIFO worker per chat; different chats run in parallel. `/stop` kills + clears.

use crate::bot::App;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

pub struct Job {
    pub chat_id: i64,
    pub user_id: u64,
    pub prompt: String,
    /// Cron runs: fresh session, output prefixed with ⏰.
    pub scheduled: bool,
}

struct Queued {
    job: Job,
    generation: u64,
}

/// The running job's stop handle and start time.
type Running = Arc<Mutex<Option<(Arc<Notify>, Instant)>>>;

#[derive(Clone)]
struct ChatQueue {
    tx: mpsc::UnboundedSender<Queued>,
    pending: Arc<AtomicUsize>,
    /// Bumped by /stop: queued jobs from an older generation are skipped.
    generation: Arc<AtomicU64>,
    running: Running,
}

#[derive(Default)]
pub struct Queues {
    chats: Mutex<HashMap<i64, ChatQueue>>,
}

impl Queues {
    pub fn push(&self, app: &Arc<App>, job: Job) {
        let q = self
            .chats
            .lock()
            .unwrap()
            .entry(job.chat_id)
            .or_insert_with(|| spawn_worker(app.clone()))
            .clone();
        q.pending.fetch_add(1, Ordering::SeqCst);
        let generation = q.generation.load(Ordering::SeqCst);
        let _ = q.tx.send(Queued { job, generation });
    }

    /// Kill the running job and drop everything queued: (was something running, jobs cleared).
    pub fn stop(&self, chat: i64) -> (bool, usize) {
        let Some(q) = self.chats.lock().unwrap().get(&chat).cloned() else {
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
    pub fn info(&self, chat: i64) -> (usize, Option<Duration>) {
        match self.chats.lock().unwrap().get(&chat) {
            None => (0, None),
            Some(q) => (
                q.pending.load(Ordering::SeqCst),
                q.running.lock().unwrap().as_ref().map(|(_, t)| t.elapsed()),
            ),
        }
    }
}

fn spawn_worker(app: Arc<App>) -> ChatQueue {
    let (tx, mut rx) = mpsc::unbounded_channel::<Queued>();
    let q = ChatQueue {
        tx,
        pending: Arc::default(),
        generation: Arc::default(),
        running: Arc::default(),
    };
    let w = q.clone();
    tokio::spawn(async move {
        while let Some(Queued { job, generation }) = rx.recv().await {
            let stop = Arc::new(Notify::new());
            {
                let mut running = w.running.lock().unwrap();
                w.pending.fetch_sub(1, Ordering::SeqCst);
                if generation != w.generation.load(Ordering::SeqCst) {
                    continue; // cleared by /stop
                }
                *running = Some((stop.clone(), Instant::now()));
            }
            crate::bot::run_job(&app, job, stop).await;
            *w.running.lock().unwrap() = None;
        }
    });
    q
}
