//! Per-user schedules.json + a once-a-minute cron loop (local time, no backfill).

use crate::bot::App;
use crate::profile::Profile;
use crate::queue::Job;
use crate::state::atomic_write;
use anyhow::{Result, bail};
use chrono::{DateTime, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Sched {
    pub id: u32,
    pub cron: String,
    pub prompt: String,
    pub chat_id: i64,
    pub created_at: String,
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
    let mut list = load(profile, uid);
    let s = Sched {
        id: list.iter().map(|s| s.id).max().unwrap_or(0) + 1,
        cron: cron.split_whitespace().collect::<Vec<_>>().join(" "),
        prompt: prompt.to_string(),
        chat_id,
        created_at: Local::now().to_rfc3339(),
    };
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
                .filter(|s| due(&s.cron, tick))
            {
                tracing::info!(user_id = uid, id = s.id, "running schedule");
                app.queues.push(
                    &app,
                    Job {
                        chat_id: s.chat_id,
                        user_id: uid,
                        prompt: s.prompt,
                        scheduled: true,
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
}
