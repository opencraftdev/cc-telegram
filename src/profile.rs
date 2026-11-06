//! Profiles: one directory per bot instance, its config.toml, and pair codes.

use crate::state::atomic_write;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_SOUL: &str = "\
# Soul

You are a helpful personal assistant running on the owner's computer, reachable over Telegram.
You are direct, friendly and concise. You can use tools (shell, files, web) to get things done.
";

// No Debug derive on purpose: the token must never end up in logs.
#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub bot_token: String,
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
    #[serde(default = "d_model")]
    pub default_model: String,
    #[serde(default = "d_max_turns")]
    pub max_turns: u32,
    #[serde(default = "d_timeout")]
    pub run_timeout_secs: u64,
    #[serde(default = "d_claude")]
    pub claude_bin: String,
    #[serde(default)]
    pub extra_claude_args: Vec<String>,
    #[serde(default)]
    pub voice: Voice,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Voice {
    pub enabled: bool,
    pub whisper_bin: String,
    pub whisper_model: String,
    pub language: String,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            enabled: false,
            whisper_bin: "whisper-cli".into(),
            whisper_model: "~/.cc-telegram/models/ggml-base.bin".into(),
            language: "auto".into(),
        }
    }
}

fn d_model() -> String {
    "sonnet".into()
}
fn d_max_turns() -> u32 {
    30
}
fn d_timeout() -> u64 {
    900
}
fn d_claude() -> String {
    "claude".into()
}

impl Config {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            allowed_user_ids: vec![],
            default_model: d_model(),
            max_turns: d_max_turns(),
            run_timeout_secs: d_timeout(),
            claude_bin: d_claude(),
            extra_claude_args: vec![],
            voice: Voice::default(),
        }
    }

    pub fn is_allowed(&self, user_id: u64) -> bool {
        self.allowed_user_ids.contains(&user_id)
    }
}

pub fn home() -> PathBuf {
    dirs::home_dir()
        .expect("no home directory")
        .join(".cc-telegram")
}

pub fn profiles_dir() -> PathBuf {
    home().join("profiles")
}

/// `~/x` -> `$HOME/x`.
pub fn expand_tilde(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(p),
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("✗ invalid profile name {name:?} → use letters, digits, '-' or '_'");
    }
    Ok(())
}

pub fn list() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profiles_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("config.toml").exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

#[derive(Clone)]
pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
}

impl Profile {
    pub fn open(name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = profiles_dir().join(name);
        if !dir.join("config.toml").exists() {
            bail!("✗ profile {name:?} not found → run: cc-telegram profile create {name}");
        }
        Ok(Self {
            name: name.into(),
            dir,
        })
    }

    /// Create the directory tree, config.toml (0600) and a default SOUL.md.
    pub fn create(name: &str, cfg: &Config) -> Result<Self> {
        validate_name(name)?;
        let p = Self {
            name: name.into(),
            dir: profiles_dir().join(name),
        };
        for d in [p.logs_dir(), p.dir.join("users"), p.workspace()] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        std::fs::set_permissions(&p.dir, std::fs::Permissions::from_mode(0o700))?;
        if !p.soul_path().exists() {
            std::fs::write(p.soul_path(), DEFAULT_SOUL)?;
        }
        p.save_config(cfg)?;
        Ok(p)
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }
    pub fn soul_path(&self) -> PathBuf {
        self.dir.join("SOUL.md")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    pub fn pair_path(&self) -> PathBuf {
        self.dir.join("pair.json")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }
    pub fn user_dir(&self, uid: u64) -> PathBuf {
        self.dir.join("users").join(uid.to_string())
    }
    /// Imported (e.g. Hermes) memory waiting for its owner to pair.
    pub fn imported_dir(&self) -> PathBuf {
        self.dir.join("imported")
    }

    /// Move unclaimed imported memory files to `uid` (only where `uid` has none yet).
    pub fn claim_imported(&self, uid: u64) -> Result<()> {
        let src = self.imported_dir();
        if !src.is_dir() {
            return Ok(());
        }
        let dst = self.user_dir(uid);
        std::fs::create_dir_all(&dst)?;
        for f in ["MEMORY.md", "USER.md"] {
            let (from, to) = (src.join(f), dst.join(f));
            let empty = std::fs::read_to_string(&to).map_or(true, |s| s.trim().is_empty());
            if from.is_file() && empty {
                std::fs::rename(&from, &to)?;
            }
        }
        let _ = std::fs::remove_dir(&src); // only succeeds once empty
        Ok(())
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.join("workspace")
    }
    pub fn inbox(&self, uid: u64) -> PathBuf {
        self.workspace().join("inbox").join(uid.to_string())
    }
    pub fn outbox(&self, chat: i64) -> PathBuf {
        self.workspace().join("outbox").join(chat.to_string())
    }

    /// Exclusive flock on the profile dir; hold it around every read-modify-write of
    /// config.toml / pair.json (bot and CLI both do this). Released on drop.
    pub fn lock(&self) -> Result<ProfileLock> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.dir.join(".lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!(
                "locking {}: {}",
                self.dir.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(ProfileLock(f))
    }

    pub fn load_config(&self) -> Result<Config> {
        let path = self.config_path();
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&s).with_context(|| {
            format!(
                "✗ invalid {} → run: cc-telegram profile edit {}",
                path.display(),
                self.name
            )
        })
    }

    pub fn save_config(&self, cfg: &Config) -> Result<()> {
        atomic_write(&self.config_path(), toml::to_string(cfg)?.as_bytes(), 0o600)
    }
}

pub struct ProfileLock(#[allow(dead_code)] std::fs::File);

// ---------- pair codes ----------

pub const PAIR_TTL_SECS: i64 = 600;
pub const PAIR_MAX_FAILURES: u32 = 5;

#[derive(Serialize, Deserialize, Default)]
pub struct PairStore {
    #[serde(default)]
    codes: Vec<PairCode>,
    #[serde(default)]
    failures: HashMap<u64, u32>,
}

#[derive(Serialize, Deserialize)]
struct PairCode {
    code: String,
    expires_at: i64,
}

#[derive(Debug, PartialEq)]
pub enum Redeem {
    Ok,
    Invalid,
    Locked,
}

impl PairStore {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, &serde_json::to_vec(self)?, 0o600)
    }

    pub fn issue(&mut self, now: i64) -> Result<String> {
        let code = format!("{:06}", random_u32()? % 1_000_000);
        self.codes.push(PairCode {
            code: code.clone(),
            expires_at: now + PAIR_TTL_SECS,
        });
        Ok(code)
    }

    /// Single-use, expiring codes; `PAIR_MAX_FAILURES` wrong tries lock the user out.
    pub fn redeem(&mut self, user_id: u64, code: &str, now: i64) -> Redeem {
        self.codes.retain(|c| c.expires_at > now);
        let fails = self.failures.entry(user_id).or_default();
        if *fails >= PAIR_MAX_FAILURES {
            return Redeem::Locked;
        }
        match self.codes.iter().position(|c| c.code == code.trim()) {
            Some(i) => {
                self.codes.remove(i);
                self.failures.remove(&user_id);
                Redeem::Ok
            }
            None => {
                *fails += 1;
                Redeem::Invalid
            }
        }
    }
}

fn random_u32() -> Result<u32> {
    use std::io::Read;
    let mut b = [0u8; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        let mut c = Config::new("t".into());
        assert!(!c.is_allowed(1));
        c.allowed_user_ids.push(1);
        assert!(c.is_allowed(1));
        assert!(!c.is_allowed(2));
    }

    #[test]
    fn config_defaults_from_minimal_toml() {
        let c: Config = toml::from_str("bot_token = \"x\"").unwrap();
        assert_eq!(c.default_model, "sonnet");
        assert_eq!(c.max_turns, 30);
        assert_eq!(c.run_timeout_secs, 900);
        assert!(!c.voice.enabled);
        assert!(c.allowed_user_ids.is_empty());
    }

    #[test]
    fn names() {
        assert!(validate_name("work-2_a").is_ok());
        for bad in ["", "../x", "a b", "a/b", "é"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pair_single_use_and_expiry() {
        let mut s = PairStore::default();
        let code = s.issue(1000).unwrap();
        assert_eq!(code.len(), 6);
        assert_eq!(s.redeem(7, &code, 1001), Redeem::Ok);
        assert_eq!(s.redeem(8, &code, 1002), Redeem::Invalid); // single use
        let code = s.issue(2000).unwrap();
        assert_eq!(s.redeem(9, &code, 2000 + PAIR_TTL_SECS), Redeem::Invalid); // expired
    }

    #[test]
    fn pair_lockout_after_five_failures() {
        let mut s = PairStore::default();
        let code = s.issue(0).unwrap();
        let wrong = if code == "000000" { "000001" } else { "000000" };
        for _ in 0..PAIR_MAX_FAILURES {
            assert_eq!(s.redeem(5, wrong, 1), Redeem::Invalid);
        }
        assert_eq!(s.redeem(5, &code, 1), Redeem::Locked); // even the right code
        assert_eq!(s.redeem(6, &code, 1), Redeem::Ok); // other users unaffected
    }

    #[test]
    fn lock_serializes_writers() {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile {
            name: "t".into(),
            dir: dir.path().to_path_buf(),
        };
        let first = p.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let p2 = p.clone();
        let h = std::thread::spawn(move || {
            let _l = p2.lock().unwrap(); // separate fd: blocks until `first` drops
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err()
        );
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn pair_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pair.json");
        let mut s = PairStore::default();
        let code = s.issue(0).unwrap();
        s.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(PairStore::load(&path).redeem(1, &code, 1), Redeem::Ok);
    }
}
