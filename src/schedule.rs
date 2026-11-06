//! Per-user schedules.json + a once-a-minute cron loop (local time, no backfill).

use crate::bot::App;
use crate::bot::Job;
use crate::profile::Profile;
use crate::state::atomic_write;
use anyhow::{Result, bail};
use chrono::{DateTime, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Sched {
    pub id: u32,
    /// 5-field cron; empty when `every_minutes` is used.
    #[serde(default)]
    pub cron: String,
    /// Interval schedule: due when the minute since the Unix epoch is a multiple of this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_minutes: Option<u32>,
    #[serde(default)]
    pub prompt: String,
    /// Command job: argv run directly (no shell, no claude) instead of `prompt`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Command working directory, relative to the profile workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub chat_id: i64,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Post the output to `chat_id` (false = run silently; failures are still logged).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub deliver: bool,
    pub created_at: String,
}

fn yes() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

impl Sched {
    pub fn is_due<Z: TimeZone>(&self, at: DateTime<Z>) -> bool {
        self.enabled
            && match self.every_minutes {
                Some(m) => m > 0 && (at.timestamp() / 60) % i64::from(m) == 0,
                None => due(&self.cron, at),
            }
    }

    /// One line for /schedules.
    pub fn describe(&self) -> String {
        let when = match self.every_minutes {
            Some(m) => format!("every {m}m"),
            None => format!("`{}`", self.cron),
        };
        let what = if self.command.is_empty() {
            crate::runner::truncate(&self.prompt, 60)
        } else {
            format!("$ {}", crate::runner::truncate(&self.command.join(" "), 60))
        };
        let off = if self.enabled { "" } else { " (disabled)" };
        format!("#{} {when} → {what}{off}", self.id)
    }
}

const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// A weekday token as 0..=7 (names: sun=0 .. sat=6; 7 is Sunday too).
fn day(tok: &str) -> Result<u32> {
    let t = tok.to_ascii_lowercase();
    if t.len() >= 3
        && let Some(i) = DAYS.iter().position(|d| t.starts_with(d))
    {
        return Ok(i as u32);
    }
    match t.parse::<u32>() {
        Ok(n) if n <= 7 => Ok(n),
        _ => bail!("bad day of week {tok:?}"),
    }
}

/// Standard cron day-of-week (0-7, Sun=0/7) -> the `cron` crate's 1-7 (Sun=1), as an explicit list.
fn convert_dow(field: &str) -> Result<String> {
    if field == "*" {
        return Ok("*".into());
    }
    let mut set = [false; 7];
    for item in field.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => match s.parse::<usize>() {
                Ok(s) if s > 0 => (r, s),
                _ => bail!("bad step in {item:?}"),
            },
            None => (item, 1),
        };
        let (lo, hi) = match (range, range.split_once('-')) {
            ("*", _) => (0, 6),
            (_, Some((a, b))) => (day(a)?, day(b)?),
            (r, None) if item.contains('/') => (day(r)?, 6),
            (r, None) => (day(r)?, day(r)?),
        };
        if lo > hi {
            bail!("bad range {range:?}");
        }
        for d in (lo..=hi).step_by(step) {
            set[(d % 7) as usize] = true;
        }
    }
    let list: Vec<String> = (0..7)
        .filter(|d| set[*d])
        .map(|d| (d + 1).to_string())
        .collect();
    Ok(list.join(","))
}

/// Validate a 5-field cron expression and convert it for the `cron` crate (seconds = 0).
/// Returns one schedule, or two when both day-of-month and day-of-week are restricted:
/// standard cron ORs them, while the `cron` crate would AND them.
pub fn parse_cron(expr: &str) -> Result<Vec<cron::Schedule>> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        bail!(
            "cron needs 5 fields (min hour day month weekday), got {}",
            f.len()
        );
    }
    let dow = convert_dow(f[4])?;
    let one = |dom: &str, dow: &str| {
        let six = format!("0 {} {} {dom} {} {dow}", f[0], f[1], f[3]);
        cron::Schedule::from_str(&six).map_err(|e| anyhow::anyhow!("bad cron {expr:?}: {e}"))
    };
    if f[2] != "*" && dow != "*" {
        Ok(vec![one(f[2], "*")?, one("*", &dow)?])
    } else {
        Ok(vec![one(f[2], &dow)?])
    }
}

pub fn due<Z: TimeZone>(expr: &str, at: DateTime<Z>) -> bool {
    parse_cron(expr).is_ok_and(|list| list.iter().any(|s| s.includes(at.clone())))
}

fn path(profile: &Profile, uid: u64) -> PathBuf {
    profile.user_dir(uid).join("schedules.json")
}

pub fn load(profile: &Profile, uid: u64) -> Vec<Sched> {
    std::fs::read_to_string(path(profile, uid))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(profile: &Profile, uid: u64, list: &[Sched]) -> Result<()> {
    std::fs::create_dir_all(profile.user_dir(uid))?;
    atomic_write(
        &path(profile, uid),
        &serde_json::to_vec_pretty(list)?,
        0o600,
    )
}

pub fn add(profile: &Profile, uid: u64, chat_id: i64, cron: &str, prompt: &str) -> Result<Sched> {
    parse_cron(cron)?;
    let s = Sched {
        cron: cron.split_whitespace().collect::<Vec<_>>().join(" "),
        prompt: prompt.to_string(),
        ..Sched::new(chat_id)
    };
    push(profile, uid, s)
}

impl Sched {
    /// An enabled, delivering job with no schedule/body yet (id assigned by `push`).
    pub fn new(chat_id: i64) -> Self {
        Self {
            id: 0,
            cron: String::new(),
            every_minutes: None,
            prompt: String::new(),
            command: vec![],
            cwd: None,
            name: None,
            chat_id,
            enabled: true,
            deliver: true,
            created_at: Local::now().to_rfc3339(),
        }
    }
}

/// Append a job with the next free id.
pub fn push(profile: &Profile, uid: u64, mut s: Sched) -> Result<Sched> {
    let mut list = load(profile, uid);
    s.id = list.iter().map(|s| s.id).max().unwrap_or(0) + 1;
    list.push(s.clone());
    save(profile, uid, &list)?;
    Ok(s)
}

pub fn remove(profile: &Profile, uid: u64, id: u32) -> Result<bool> {
    let mut list = load(profile, uid);
    let before = list.len();
    list.retain(|s| s.id != id);
    if list.len() == before {
        return Ok(false);
    }
    save(profile, uid, &list)?;
    Ok(true)
}

/// Hermes-compatible output handling for command jobs: failure → alert with the output tail;
/// empty stdout or a last line `{"wakeAgent": false}` → nothing; otherwise stdout.
pub fn command_message(name: &str, ok: bool, stdout: &str, stderr: &str) -> Option<String> {
    if !ok {
        let all = format!("{}\n{}", stdout.trim(), stderr.trim());
        let all = all.trim();
        let tail: String = {
            let chars: Vec<char> = all.chars().collect();
            chars[chars.len().saturating_sub(3000)..].iter().collect()
        };
        return Some(format!("⚠ {name} failed\n\n{tail}"));
    }
    let out = stdout.trim();
    let silent = out
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| {
            serde_json::from_str::<serde_json::Value>(l.trim())
                .is_ok_and(|v| v.get("wakeAgent") == Some(&serde_json::Value::Bool(false)))
        });
    (!out.is_empty() && !silent).then(|| out.to_string())
}

/// Max bytes kept from each of stdout/stderr; the rest is read and discarded.
pub const OUTPUT_CAP: u64 = 1024 * 1024;

async fn read_capped(r: Option<impl tokio::io::AsyncRead + Unpin>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut r) = r else {
        return String::new();
    };
    let mut buf = vec![];
    let _ = (&mut r).take(OUTPUT_CAP).read_to_end(&mut buf).await;
    let _ = tokio::io::copy(&mut r, &mut tokio::io::sink()).await; // keep the pipe drained
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run argv (no shell) in `cwd` in its own process group; kill the group on timeout.
/// Returns (success, stdout, stderr), each output capped at `OUTPUT_CAP`.
pub async fn exec(
    argv: &[String],
    cwd: &std::path::Path,
    timeout: Duration,
) -> (bool, String, String) {
    let child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return (
                false,
                String::new(),
                format!("could not start {:?}: {e}", argv[0]),
            );
        }
    };
    let pid = child.id();
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let run = async {
        let (o, e, st) = tokio::join!(read_capped(out), read_capped(err), child.wait());
        (st.is_ok_and(|s| s.success()), o, e)
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(r) => r,
        Err(_) => {
            crate::runner::kill_group(pid);
            (
                false,
                String::new(),
                format!("timed out after {}s", timeout.as_secs()),
            )
        }
    }
}

/// Command jobs currently running (profile dir, user, job id): a job still running when it
/// comes due again is skipped, not started twice.
static RUNNING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Claims `key` until dropped; None if it's already running.
struct Running(String);
impl Running {
    fn claim(key: String) -> Option<Self> {
        let mut r = RUNNING.lock().unwrap();
        if r.contains(&key) {
            return None;
        }
        r.push(key.clone());
        Some(Self(key))
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.lock().unwrap().retain(|k| *k != self.0);
    }
}

/// Run a command job in the workspace with a timeout; post its output.
async fn run_command(app: &App, uid: u64, s: &Sched, timeout: Duration) {
    let key = format!("{}:{uid}:{}", app.profile.dir.display(), s.id);
    let Some(_running) = Running::claim(key) else {
        tracing::warn!(
            id = s.id,
            "command job still running from its last run; skipped"
        );
        return;
    };
    let name = s
        .name
        .clone()
        .unwrap_or_else(|| format!("schedule #{}", s.id));
    let cwd = app.profile.workspace().join(s.cwd.as_deref().unwrap_or(""));
    let (ok, stdout, stderr) = exec(&s.command, &cwd, timeout).await;
    tracing::info!(id = s.id, ok, "command job finished");
    match command_message(&name, ok, &stdout, &stderr) {
        Some(msg) if s.deliver => {
            crate::bot::send_long(&app.bot, teloxide::types::ChatId(s.chat_id), &msg).await
        }
        Some(msg) if !ok => tracing::warn!(id = s.id, "{msg}"),
        _ => {}
    }
}

/// Every minute, enqueue due jobs of allowlisted users onto their chat's queue.
pub async fn run_loop(app: Arc<App>) {
    loop {
        let now = Local::now();
        let tick = now
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(now)
            + chrono::Duration::minutes(1);
        tokio::time::sleep((tick - now).to_std().unwrap_or_default()).await;
        let Ok(cfg) = app.profile.load_config() else {
            continue;
        };
        let users = std::fs::read_dir(app.profile.dir.join("users"))
            .into_iter()
            .flatten()
            .flatten();
        for uid in users.filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok()) {
            if !cfg.is_allowed(uid) {
                continue;
            }
            for s in load(&app.profile, uid)
                .into_iter()
                .filter(|s| s.is_due(tick))
            {
                tracing::info!(user_id = uid, id = s.id, "running schedule");
                if !s.command.is_empty() {
                    let (app, timeout) = (app.clone(), Duration::from_secs(cfg.run_timeout_secs));
                    tokio::spawn(async move { run_command(&app, uid, &s, timeout).await });
                    continue;
                }
                crate::bot::enqueue(
                    &app,
                    Job {
                        chat_id: s.chat_id,
                        user_id: uid,
                        prompt: s.prompt,
                        scheduled: true,
                        sender: "a scheduled task (no one is waiting for a reply)".into(),
                        typing: None,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn five_fields_required() {
        assert!(parse_cron("*/5 * * * *").is_ok());
        assert!(parse_cron("0 9 * * 1-5").is_ok());
        assert!(parse_cron("* * * *").is_err());
        assert!(parse_cron("0 * * * * *").is_err());
        assert!(parse_cron("61 * * * *").is_err());
        assert!(parse_cron("x y z a b").is_err());
        assert!(parse_cron("0 0 * * 8").is_err());
    }

    #[test]
    fn every_five_minutes() {
        assert!(due("*/5 * * * *", at(2026, 11, 6, 10, 15)));
        assert!(!due("*/5 * * * *", at(2026, 11, 6, 10, 16)));
    }

    #[test]
    fn weekday_numbers_follow_standard_cron() {
        // 2026-11-06 is a Friday, 2026-11-08 a Sunday.
        assert!(due("0 9 * * 5", at(2026, 11, 6, 9, 0)));
        assert!(due("0 9 * * fri", at(2026, 11, 6, 9, 0)));
        assert!(due("0 9 * * 1-5", at(2026, 11, 6, 9, 0)));
        assert!(!due("0 9 * * 1-5", at(2026, 11, 8, 9, 0)));
        assert!(due("0 9 * * 0", at(2026, 11, 8, 9, 0)));
        assert!(due("0 9 * * 7", at(2026, 11, 8, 9, 0)));
        assert!(due("0 9 * * 5-7", at(2026, 11, 8, 9, 0)));
        assert!(!due("0 9 * * 5", at(2026, 11, 6, 9, 1)));
    }

    #[test]
    fn dom_and_dow_are_ored_like_standard_cron() {
        // 2026-12-01 is a Tuesday, 2026-11-09 a Monday, 2026-11-10 a Tuesday.
        assert!(due("0 9 1 * 1", at(2026, 12, 1, 9, 0))); // 1st of month
        assert!(due("0 9 1 * 1", at(2026, 11, 9, 9, 0))); // a Monday
        assert!(!due("0 9 1 * 1", at(2026, 11, 10, 9, 0))); // neither
        assert!(due("0 9 1 * *", at(2026, 12, 1, 9, 0)));
        assert!(!due("0 9 1 * *", at(2026, 11, 9, 9, 0)));
    }

    #[test]
    fn interval_disabled_and_old_json() {
        let mut s = Sched::new(1);
        s.every_minutes = Some(5);
        assert!(s.is_due(at(2026, 11, 6, 10, 15)));
        assert!(!s.is_due(at(2026, 11, 6, 10, 16)));
        s.enabled = false;
        assert!(!s.is_due(at(2026, 11, 6, 10, 15)));
        // schedules.json written before these fields existed still loads
        let old: Sched = serde_json::from_str(
            r#"{"id":1,"cron":"*/5 * * * *","prompt":"hi","chat_id":7,"created_at":"x"}"#,
        )
        .unwrap();
        assert!(old.enabled && old.deliver && old.command.is_empty());
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("enabled") && !json.contains("command"));
    }

    #[test]
    fn command_output_like_hermes() {
        assert_eq!(
            command_message("j", true, "  report\n", ""),
            Some("report".into())
        );
        assert_eq!(command_message("j", true, "  \n", ""), None);
        assert_eq!(
            command_message("j", true, "x\n{\"wakeAgent\": false}\n", ""),
            None
        );
        assert_eq!(
            command_message("j", true, "{\"wakeAgent\": true}", ""),
            Some("{\"wakeAgent\": true}".into())
        );
        let fail = command_message("j", false, "partial", "Traceback: boom").unwrap();
        assert!(fail.starts_with("⚠ j failed") && fail.contains("boom"));
    }

    #[test]
    fn dow_conversion() {
        assert_eq!(convert_dow("*").unwrap(), "*");
        assert_eq!(convert_dow("0").unwrap(), "1");
        assert_eq!(convert_dow("1-5").unwrap(), "2,3,4,5,6");
        assert_eq!(convert_dow("5-7").unwrap(), "1,6,7");
        assert_eq!(convert_dow("*/2").unwrap(), "1,3,5,7");
        assert_eq!(convert_dow("mon,wed").unwrap(), "2,4");
        assert!(convert_dow("5-3").is_err());
        assert!(convert_dow("*/0").is_err());
    }

    #[tokio::test]
    async fn exec_runs_argv_without_shell_and_times_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.sh"), "echo \"$1\"; pwd").unwrap();
        let argv: Vec<String> = ["sh", "s.sh", "a; rm -rf /"].map(String::from).into();
        let (ok, out, _) = exec(&argv, dir.path(), Duration::from_secs(5)).await;
        assert!(ok);
        assert!(out.starts_with("a; rm -rf /\n"), "{out}");
        let slow: Vec<String> = ["sh", "-c", "sleep 30"].map(String::from).into();
        let t = std::time::Instant::now();
        let (ok, _, err) = exec(&slow, dir.path(), Duration::from_millis(300)).await;
        assert!(!ok && err.contains("timed out") && t.elapsed() < Duration::from_secs(5));
        let (ok, _, err) = exec(
            &["/nonexistent".to_string()],
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        assert!(!ok && err.contains("could not start"));
        // huge output is capped, and the process still finishes (pipe drained)
        let big: Vec<String> = [
            "sh",
            "-c",
            "head -c 3000000 /dev/zero | tr '\\0' x; echo done >&2",
        ]
        .map(String::from)
        .into();
        let (ok, out, err) = exec(&big, dir.path(), Duration::from_secs(20)).await;
        assert!(ok, "{err}");
        assert_eq!(out.len() as u64, OUTPUT_CAP);
        assert_eq!(err.trim(), "done");
    }

    #[test]
    fn overlapping_runs_are_refused() {
        let a = Running::claim("p:1:9".into()).unwrap();
        assert!(Running::claim("p:1:9".into()).is_none());
        assert!(Running::claim("p:1:10".into()).is_some());
        drop(a);
        assert!(Running::claim("p:1:9".into()).is_some());
    }
}
