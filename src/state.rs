//! state.json: chat_id -> { session_id, model }, written atomically.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct ChatState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cost_usd: Option<f64>,
    /// Bumped by /new; a running job only saves its session if this is unchanged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub epoch: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

pub type State = HashMap<i64, ChatState>;

/// Write `data` to a temp file next to `path` (with `mode`), then rename over `path`.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    static N: AtomicUsize = AtomicUsize::new(0);
    // Unique per writer, so concurrent writers never rename each other's half-written file.
    let n = N.fetch_add(1, Ordering::SeqCst);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let _ = std::fs::remove_file(&tmp); // so `mode` applies to a fresh file
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, state: &State) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(state)?, 0o600)
}

pub struct StateStore {
    path: PathBuf,
    inner: Mutex<State>,
}

impl StateStore {
    pub fn open(path: PathBuf) -> Self {
        let inner = Mutex::new(load(&path));
        Self { path, inner }
    }

    pub fn get(&self, chat: i64) -> ChatState {
        self.inner
            .lock()
            .unwrap()
            .get(&chat)
            .cloned()
            .unwrap_or_default()
    }

    /// /new: drop the session and invalidate any run still in flight.
    pub fn reset_session(&self, chat: i64) {
        self.update(chat, |c| {
            c.session_id = None;
            c.epoch += 1;
        });
    }

    /// Store `new` only if no /new happened since the job read `epoch` (first runs included).
    pub fn set_session_if(&self, chat: i64, epoch: u64, new: String) -> bool {
        let mut swapped = false;
        self.update(chat, |c| {
            if c.epoch == epoch {
                c.session_id = Some(new);
                swapped = true;
            }
        });
        swapped
    }

    pub fn update(&self, chat: i64, f: impl FnOnce(&mut ChatState)) {
        let mut st = self.inner.lock().unwrap();
        f(st.entry(chat).or_default());
        if let Err(e) = save(&self.path, &st) {
            tracing::error!("saving state.json: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = StateStore::open(path.clone());
        store.update(-100123, |c| c.session_id = Some("abc".into()));
        store.update(42, |c| c.model = Some("opus".into()));
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "temp file left behind");
        let reloaded = load(&path);
        assert_eq!(reloaded[&-100123].session_id.as_deref(), Some("abc"));
        assert_eq!(reloaded[&42].model.as_deref(), Some("opus"));
        assert_eq!(
            StateStore::open(path).get(42).model.as_deref(),
            Some("opus")
        );
    }

    #[test]
    fn session_compare_and_set() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path().join("state.json"));
        let e = store.get(1).epoch;
        assert!(store.set_session_if(1, e, "a".into()));
        store.reset_session(1); // user sent /new mid-run
        assert!(!store.set_session_if(1, e, "a2".into()));
        assert_eq!(store.get(1).session_id, None);
    }

    #[test]
    fn new_during_first_run_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path().join("state.json"));
        let e = store.get(7).epoch; // job starts: no session yet
        store.reset_session(7); // /new while it runs: still no session
        assert!(!store.set_session_if(7, e, "first".into()));
        assert_eq!(store.get(7).session_id, None);
        // the next job sees the new epoch and may store
        assert!(store.set_session_if(7, store.get(7).epoch, "second".into()));
        assert_eq!(store.get(7).session_id.as_deref(), Some("second"));
    }

    #[test]
    fn missing_or_corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(load(&path).is_empty());
        std::fs::write(&path, "{nope").unwrap();
        assert!(load(&path).is_empty());
    }
}
