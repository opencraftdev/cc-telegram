//! Read a Hermes Agent home (~/.hermes or $HERMES_HOME) and copy what maps onto a profile.
//! Read-only on the Hermes side. Secret values are never printed or logged.

use crate::profile::{DEFAULT_SOUL, Profile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Files that make a directory a Hermes home / profile (per the Hermes profiles docs).
const IDENTITY: [&str; 6] = [
    "config.yaml",
    ".env",
    "SOUL.md",
    "profile.yaml",
    "auth.json",
    "state.db",
];

pub fn detect() -> Option<PathBuf> {
    let p = match std::env::var_os("HERMES_HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => dirs::home_dir()?.join(".hermes"),
    };
    p.is_dir().then_some(p)
}

pub struct Source {
    /// None = the root (default) Hermes profile.
    pub name: Option<String>,
    pub dir: PathBuf,
}

fn is_home(dir: &Path) -> bool {
    IDENTITY.iter().any(|f| dir.join(f).exists()) || dir.join("skills").is_dir()
}

/// The root home plus every `profiles/<name>/` that looks like a Hermes home.
pub fn sources(home: &Path) -> Vec<Source> {
    let mut out = vec![];
    // `--hermes-home ~/.hermes/profiles/<name>` is that profile, not the root.
    let in_profiles = home
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "profiles");
    if is_home(home) {
        out.push(Source {
            name: in_profiles
                .then(|| home.file_name().map(|n| n.to_string_lossy().into_owned()))
                .flatten(),
            dir: home.to_path_buf(),
        });
    }
    if in_profiles {
        return out;
    }
    let mut profiles: Vec<_> = std::fs::read_dir(home.join("profiles"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && is_home(p))
        .collect();
    profiles.sort();
    for dir in profiles {
        if let Some(n) = dir.file_name().and_then(|n| n.to_str()) {
            out.push(Source {
                name: Some(n.to_string()),
                dir: dir.clone(),
            });
        }
    }
    out
}

#[derive(Default)]
pub struct Found {
    pub token: Option<String>,
    /// Derived like Hermes' authz: TELEGRAM_ALLOWED_USERS ∪ GATEWAY_ALLOWED_USERS ∪ approved
    /// pairings ∪ owner. When Hermes allows everyone, only the owner.
    pub allowed: Vec<u64>,
    /// Hermes was configured to allow every Telegram user (not imported).
    pub allow_all: bool,
    /// `platforms.telegram.home_channel.user_id`, else a private TELEGRAM_HOME_CHANNEL, else the
    /// single allowed user. Default memory owner.
    pub owner: Option<u64>,
    /// Where cron output goes: home_channel.chat_id / TELEGRAM_HOME_CHANNEL / owner's DM.
    pub home_chat: Option<i64>,
    /// Has a Discord bot but no Telegram bot.
    pub discord_only: bool,
    /// Non-numeric allowlist entries that were dropped (e.g. usernames), not counting `*`.
    pub dropped_ids: usize,
    /// HERMES_TIMEZONE or config.yaml `timezone`, if Hermes evaluated cron in a fixed zone.
    pub timezone: Option<String>,
    pub soul: Option<PathBuf>,
    pub memory: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub model: Option<String>,
    pub skills: Vec<PathBuf>,
    pub jobs: Vec<HermesJob>,
    /// `<home>/scripts`, where Hermes resolves cron `script` paths.
    pub scripts: Option<PathBuf>,
}

/// A Hermes cron job (`cron/jobs.json`), already mapped to what we can run.
#[derive(Debug, Clone, PartialEq)]
pub struct HermesJob {
    pub name: String,
    pub prompt: String,
    /// `no_agent` jobs: the script is the whole job.
    pub script: Option<String>,
    pub workdir: Option<String>,
    /// Ok(schedule) or why it can't be imported.
    pub schedule: Result<JobSchedule, String>,
    pub enabled: bool,
    pub dest: Dest,
    pub deliver: bool,
    pub notes: Vec<String>,
}

/// Where a job's output goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// `deliver: telegram`: the Telegram home channel.
    Home,
    /// `telegram:<chat_id>`, or the Telegram chat the job was created in (`origin`).
    Chat(i64),
    /// `origin` without a recorded origin: the owner's DM (never a group by guess).
    OwnerDm,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobSchedule {
    Cron(String),
    EveryMinutes(u32),
}

fn file(p: PathBuf) -> Option<PathBuf> {
    p.is_file().then_some(p)
}

fn truthy(v: Option<&String>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
}

/// Approved Telegram pairings, from the same place Hermes reads them: legacy `pairing/` if it
/// has content, else `platforms/pairing/`. The file is an object keyed by user id.
fn approved_pairings(dir: &Path) -> Vec<String> {
    let legacy = dir.join("pairing");
    let has_content = std::fs::read_dir(&legacy).is_ok_and(|mut d| d.next().is_some());
    let base = if has_content {
        legacy
    } else {
        dir.join("platforms/pairing")
    };
    std::fs::read_to_string(base.join("telegram-approved.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&s).ok())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn scan(dir: &Path) -> Found {
    let env = std::fs::read_to_string(dir.join(".env"))
        .map(|s| parse_env(&s))
        .unwrap_or_default();
    let yaml = std::fs::read_to_string(dir.join("config.yaml")).unwrap_or_default();
    let get = |k: &str| env.get(k).filter(|v| !v.trim().is_empty());
    let token = get("TELEGRAM_BOT_TOKEN").cloned();

    // Allowlist, in Hermes' authz order (gateway/authz_mixin.py).
    let mut raw: Vec<String> = ["TELEGRAM_ALLOWED_USERS", "GATEWAY_ALLOWED_USERS"]
        .iter()
        .filter_map(|k| get(k))
        .flat_map(|v| v.split(',').map(|x| x.trim().to_string()))
        .filter(|x| !x.is_empty())
        .collect();
    raw.extend(approved_pairings(dir));
    let allow_all = truthy(get("TELEGRAM_ALLOW_ALL_USERS"))
        || truthy(get("GATEWAY_ALLOW_ALL_USERS"))
        || raw.iter().any(|x| x == "*");
    let dropped_ids = raw
        .iter()
        .filter(|x| *x != "*" && x.parse::<u64>().is_err())
        .count();
    let mut ids: Vec<u64> = raw.iter().filter_map(|x| x.parse().ok()).collect();
    ids.sort();
    ids.dedup();

    let hc = |k: &str| yaml_get(&yaml, &["platforms", "telegram", "home_channel", k]);
    let env_home = get("TELEGRAM_HOME_CHANNEL").and_then(|v| v.trim().parse::<i64>().ok());
    let owner = hc("user_id")
        .and_then(|v| v.parse::<u64>().ok())
        // a private chat's id is the user's id
        .or(env_home.filter(|c| *c > 0).map(|c| c as u64))
        .or(if ids.len() == 1 { Some(ids[0]) } else { None });
    let home_chat = hc("chat_id")
        .and_then(|v| v.parse::<i64>().ok())
        .or(env_home)
        .or(owner.map(|o| o as i64));
    let allowed = if allow_all {
        owner.into_iter().collect()
    } else {
        let mut a = ids;
        if let Some(o) = owner
            && !a.contains(&o)
        {
            a.push(o);
        }
        a
    };
    let scripts = dir.join("scripts");
    Found {
        discord_only: token.is_none() && get("DISCORD_BOT_TOKEN").is_some(),
        dropped_ids,
        token,
        allowed,
        allow_all,
        owner,
        home_chat,
        timezone: get("HERMES_TIMEZONE")
            .cloned()
            .or_else(|| yaml_get(&yaml, &["timezone"])),
        soul: file(dir.join("SOUL.md")),
        memory: file(dir.join("memories/MEMORY.md")),
        user: file(dir.join("memories/USER.md")),
        model: model_from_yaml(&yaml),
        skills: find_skills(&dir.join("skills")),
        jobs: std::fs::read_to_string(dir.join("cron/jobs.json"))
            .map(|s| parse_jobs(&s))
            .unwrap_or_default(),
        scripts: scripts.is_dir().then_some(scripts),
    }
}

/// Map Hermes `cron/jobs.json` (`{"jobs": [...]}`) to importable jobs.
pub fn parse_jobs(text: &str) -> Vec<HermesJob> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return vec![];
    };
    let jobs = v["jobs"].as_array().cloned().unwrap_or_default();
    jobs.iter()
        .map(|j| {
            let s = |k: &str| j[k].as_str().map(str::to_string).filter(|x| !x.is_empty());
            let mut notes = vec![];
            let sched = &j["schedule"];
            let schedule = match sched["kind"].as_str() {
                Some("cron") => match sched["expr"].as_str() {
                    Some(e) if crate::schedule::parse_cron(e).is_ok() => Ok(JobSchedule::Cron(
                        e.split_whitespace().collect::<Vec<_>>().join(" "),
                    )),
                    e => Err(format!("cron expression {e:?} isn't standard 5-field cron")),
                },
                Some("interval") => match sched["minutes"].as_u64() {
                    Some(m) if (1..=u32::MAX as u64).contains(&m) => {
                        Ok(JobSchedule::EveryMinutes(m as u32))
                    }
                    _ => Err("interval without minutes".into()),
                },
                Some("once") => Err("one-shot ('once') schedules aren't supported".into()),
                k => Err(format!("schedule kind {k:?} isn't supported")),
            };
            let times = j["repeat"]["times"].as_u64();
            let done = j["repeat"]["completed"].as_u64().unwrap_or(0);
            let finished = times.is_some_and(|t| done >= t);
            if times.is_some() && !finished {
                notes.push("repeat limit not imported (runs until removed)".into());
            }
            let paused = !j["paused_at"].is_null() || s("state").as_deref() == Some("paused");
            let enabled = j["enabled"].as_bool() != Some(false) && !paused && !finished;
            let deliver_to = s("deliver").unwrap_or_else(|| "origin".into());
            // Hermes `origin` = {platform, chat_id, thread_id} of the chat the job was made in.
            let origin = &j["origin"];
            let origin_chat = origin["chat_id"].as_i64().or_else(|| {
                origin["chat_id"]
                    .as_str()
                    .and_then(|c| c.trim().parse().ok())
            });
            let mut unsupported = || {
                notes.push(format!(
                    "delivery {deliver_to:?} not supported; runs silently"
                ));
                (false, Dest::Home)
            };
            let (deliver, dest) = match deliver_to.split_once(':') {
                Some(("telegram", id)) => match id.trim().parse::<i64>() {
                    Ok(c) => (true, Dest::Chat(c)),
                    Err(_) => unsupported(),
                },
                _ if deliver_to == "telegram" => (true, Dest::Home),
                _ if deliver_to == "local" => (false, Dest::Home),
                _ if deliver_to == "origin" => match (origin["platform"].as_str(), origin_chat) {
                    (Some("telegram"), Some(c)) => (true, Dest::Chat(c)),
                    (None, _) => (true, Dest::OwnerDm),
                    _ => unsupported(),
                },
                _ => unsupported(),
            };
            let no_agent = j["no_agent"].as_bool() == Some(true);
            let script = s("script");
            if !no_agent && script.is_some() {
                notes.push("pre-run script not imported; the prompt runs on its own".into());
            }
            let skills =
                j["skills"].as_array().map_or(0, |a| a.len()) + usize::from(s("skill").is_some());
            if !no_agent && skills > 0 {
                notes.push("per-job skills not pinned; all workspace skills are available".into());
            }
            HermesJob {
                name: s("name").unwrap_or_else(|| s("id").unwrap_or_else(|| "job".into())),
                prompt: s("prompt").unwrap_or_default(),
                script: if no_agent { script } else { None },
                workdir: s("workdir"),
                schedule: if no_agent && s("script").is_none() {
                    Err("no_agent job without a script".into())
                } else {
                    schedule
                },
                enabled,
                dest,
                deliver,
                notes,
            }
        })
        .collect()
}

impl HermesJob {
    /// Our schedule entry. Script jobs run like Hermes ran them: `bash` for .sh/.bash, else
    /// `python3`, on `<workspace>/scripts/<script>` (absolute), from the job's `workdir` if it
    /// exists, else the script's folder.
    pub fn to_sched(
        &self,
        home_chat: i64,
        owner_dm: i64,
        workspace: &Path,
    ) -> Result<crate::schedule::Sched, String> {
        let schedule = self.schedule.clone()?;
        let chat = match self.dest {
            Dest::Home => home_chat,
            Dest::Chat(c) => c,
            Dest::OwnerDm => owner_dm,
        };
        let mut s = crate::schedule::Sched::new(chat);
        match schedule {
            JobSchedule::Cron(c) => s.cron = c,
            JobSchedule::EveryMinutes(m) => s.every_minutes = Some(m),
        }
        s.name = Some(self.name.clone());
        s.enabled = self.enabled;
        s.deliver = self.deliver;
        match &self.script {
            Some(script) => {
                let rel = Path::new(script);
                if rel.is_absolute()
                    || rel
                        .components()
                        .any(|c| c == std::path::Component::ParentDir)
                {
                    return Err(format!("script path {script:?} is outside scripts/"));
                }
                let interp = match rel.extension().and_then(|e| e.to_str()) {
                    Some("sh" | "bash") => "bash",
                    _ => "python3",
                };
                let abs = workspace.join("scripts").join(rel);
                let parent = match rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                    Some(dir) => Path::new("scripts").join(dir),
                    None => PathBuf::from("scripts"),
                };
                s.command = vec![interp.into(), abs.to_string_lossy().into_owned()];
                s.cwd = Some(match &self.workdir {
                    Some(w) if Path::new(w).is_dir() => w.clone(),
                    _ => parent.to_string_lossy().into_owned(),
                });
            }
            None if self.prompt.trim().is_empty() => return Err("job has no prompt".into()),
            None => s.prompt = self.prompt.clone(),
        }
        Ok(s)
    }
}

impl Found {
    /// Counts and flags only: no token, no ids, no file contents.
    pub fn summary(&self) -> String {
        let yn = |b: bool| if b { "yes" } else { "no" };
        if self.discord_only {
            return "Discord-only (Discord isn't supported by cc-gateway)".into();
        }
        let model = match &self.model {
            Some(m) => match map_model(m) {
                Some(c) => format!("{m} (→ {c})"),
                None => format!("{m} (not a Claude model: keeps the cc-gateway default)"),
            },
            None => "none".into(),
        };
        format!(
            "bot token: {} · allowed users: {}{} · owner: {} · SOUL.md: {} · MEMORY.md: {} · USER.md: {} · model: {model} · skills: {} · cron jobs: {}",
            yn(self.token.is_some()),
            self.allowed.len(),
            if self.allow_all {
                " (Hermes allowed everyone: owner only)"
            } else {
                ""
            },
            yn(self.owner.is_some()),
            yn(self.soul.is_some()),
            yn(self.memory.is_some()),
            yn(self.user.is_some()),
            self.skills.len(),
            self.jobs.len(),
        )
    }
}

/// Minimal dotenv: `KEY=value`, `export KEY=value`, '#' comments, single/double quotes.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
            _ => v.split(" #").next().unwrap_or("").trim(),
        };
        m.insert(k.trim().to_string(), v.to_string());
    }
    m
}

fn unquote(s: &str) -> &str {
    let s = s.split(" #").next().unwrap_or("").trim();
    s.trim_matches(|c| c == '"' || c == '\'')
}

/// Look up a scalar at `path` in block-style YAML (enough for Hermes' config.yaml: nested
/// mappings, comments, quoted values; list items and flow style are ignored).
pub fn yaml_get(yaml: &str, path: &[&str]) -> Option<String> {
    let mut stack: Vec<(usize, String)> = vec![];
    for line in yaml.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') || t.starts_with('-') {
            continue;
        }
        let indent = line.len() - t.len();
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        if !(v.is_empty() || v.starts_with([' ', '\t'])) {
            continue; // e.g. a URL inside a scalar
        }
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        let k = k.trim().trim_matches(|c| c == '"' || c == '\'');
        let depth = stack.len();
        if depth + 1 == path.len()
            && k == path[depth]
            && stack
                .iter()
                .map(|(_, k)| k.as_str())
                .eq(path[..depth].iter().copied())
        {
            let v = unquote(v);
            return (!v.is_empty() && v != "null" && v != "~").then(|| v.to_string());
        }
        stack.push((indent, k.to_string()));
    }
    None
}

/// `model: <id>` or `model:\n  default: <id>`.
pub fn model_from_yaml(yaml: &str) -> Option<String> {
    yaml_get(yaml, &["model", "default"]).or_else(|| yaml_get(yaml, &["model"]))
}

/// Map only obvious Claude models; anything else keeps our default.
pub fn map_model(raw: &str) -> Option<&'static str> {
    let m = raw.to_ascii_lowercase();
    if !m.contains("claude") {
        return None;
    }
    ["opus", "sonnet", "haiku"]
        .into_iter()
        .find(|k| m.contains(k))
}

/// Skill dirs (containing SKILL.md) under `skills/`, including category subfolders and
/// symlinked skills. Hidden dirs (e.g. `.hub`) are skipped. Bounded depth, no loops.
pub fn find_skills(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in entries {
            let hidden = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden || !p.is_dir() {
                continue; // is_dir follows symlinks
            }
            if p.join("SKILL.md").is_file() {
                out.push(p);
            } else {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = vec![];
    walk(root, 0, &mut out);
    out
}

fn copy_dir(src: &Path, dst: &Path, depth: u32) -> std::io::Result<()> {
    if depth > 10 {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let (from, to) = (e.path(), dst.join(e.file_name()));
        match std::fs::metadata(&from) {
            // metadata() follows symlinks; broken links are skipped
            Ok(m) if m.is_dir() => copy_dir(&from, &to, depth + 1)?,
            Ok(_) => {
                std::fs::copy(&from, &to)?;
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// A destination we may write without asking: missing, empty, or our untouched default.
fn replaceable(p: &Path) -> bool {
    match std::fs::read_to_string(p) {
        Err(_) => !p.exists(),
        Ok(s) => s.trim().is_empty() || s == DEFAULT_SOUL,
    }
}

pub type Row = (String, String);

/// Copy SOUL.md, memories and skills into `p`. Memory goes to `users/<owner>/`, or to
/// `imported/` (claimed on first pair) when there's no owner yet. `confirm` is asked before
/// overwriting anything that already has content.
pub fn import_files(
    f: &Found,
    p: &Profile,
    owner: Option<u64>,
    confirm: &mut dyn FnMut(&Path) -> bool,
) -> Vec<Row> {
    let mut rows = vec![];
    let mut put = |item: &str, src: &Option<PathBuf>, dst: PathBuf, rows: &mut Vec<Row>| {
        let Some(src) = src else {
            rows.push((item.into(), "skipped (not in Hermes)".into()));
            return;
        };
        if !replaceable(&dst) && !confirm(&dst) {
            rows.push((item.into(), "skipped (exists)".into()));
            return;
        }
        let res = dst
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::copy(src, &dst));
        rows.push((
            item.into(),
            match res {
                Ok(_) => format!("imported → {}", dst.display()),
                Err(e) => format!("failed: {e}"),
            },
        ));
    };
    put("SOUL.md", &f.soul, p.soul_path(), &mut rows);
    let mem_dir = owner.map_or_else(|| p.imported_dir(), |u| p.user_dir(u));
    put("MEMORY.md", &f.memory, mem_dir.join("MEMORY.md"), &mut rows);
    put("USER.md", &f.user, mem_dir.join("USER.md"), &mut rows);

    let skills_dir = p.workspace().join(".claude/skills");
    let mut used = std::collections::HashSet::new();
    for s in &f.skills {
        let base = s
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        // Skills are flat in .claude/skills: a second `<category>/<name>` with the same name
        // becomes `<category>-<name>` instead of replacing the first.
        let name = if used.contains(&base) {
            let cat = s
                .parent()
                .and_then(|c| c.file_name())
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dup".into());
            format!("{cat}-{base}")
        } else {
            base.clone()
        };
        used.insert(name.clone());
        let dst = skills_dir.join(&name);
        let item = if name == base {
            format!("skill {name}")
        } else {
            format!("skill {name} (renamed: clashes with another {base})")
        };
        if dst.exists() && !confirm(&dst) {
            rows.push((item, "skipped (exists)".into()));
            continue;
        }
        let _ = std::fs::remove_dir_all(&dst);
        rows.push((
            item,
            match copy_dir(s, &dst, 0) {
                Ok(()) => format!("imported → {}", dst.display()),
                Err(e) => format!("failed: {e}"),
            },
        ));
    }
    rows
}

/// Import Hermes cron jobs as the owner's schedules (output to the home chat). Script jobs
/// become command jobs; `<hermes>/scripts` is copied to `workspace/scripts/`. Jobs already
/// imported (same name and body) are skipped, so re-running is safe.
pub fn import_jobs(
    f: &Found,
    p: &Profile,
    owner: Option<u64>,
    confirm: &mut dyn FnMut(&Path) -> bool,
) -> Vec<Row> {
    let mut rows = vec![];
    if f.jobs.is_empty() {
        return rows;
    }
    let (Some(owner), Some(chat)) = (owner, f.home_chat.or(owner.map(|o| o as i64))) else {
        for j in &f.jobs {
            rows.push((
                format!("cron {}", j.name),
                "skipped (no owner/home chat to post to)".into(),
            ));
        }
        return rows;
    };
    let existing = crate::schedule::load(p, owner);
    let mut python = false;
    let mut need_scripts = false;
    for j in &f.jobs {
        let item = format!("cron {}", j.name);
        let s = match j.to_sched(chat, owner as i64, &p.workspace()) {
            Ok(s) => s,
            Err(why) => {
                rows.push((item, format!("skipped ({why})")));
                continue;
            }
        };
        if existing
            .iter()
            .any(|e| e.name == s.name && e.prompt == s.prompt && e.command == s.command)
        {
            rows.push((item, "skipped (already imported)".into()));
            continue;
        }
        python |= s.command.first().is_some_and(|c| c == "python3");
        need_scripts |= !s.command.is_empty();
        let label = if s.enabled { "" } else { ", disabled" };
        rows.push((
            item,
            match crate::schedule::push(p, owner, s) {
                Ok(s) => format!("imported (#{}{label})", s.id),
                Err(e) => format!("failed: {e}"),
            },
        ));
        for n in &j.notes {
            rows.push(("".into(), format!("note: {n}")));
        }
    }
    if need_scripts {
        let dst = p.workspace().join("scripts");
        rows.push((
            "scripts/".into(),
            match &f.scripts {
                None => "missing in Hermes; command jobs will fail until you add them".into(),
                Some(_) if dst.exists() && !confirm(&dst) => "skipped (exists)".into(),
                Some(src) => match copy_dir(src, &dst, 0) {
                    Ok(()) => format!("imported → {}", dst.display()),
                    Err(e) => format!("failed: {e}"),
                },
            },
        ));
    }
    if python {
        rows.push((
            "".into(),
            "note: Python scripts run with python3, not Hermes' venv; install their packages or edit `command` in schedules.json".into(),
        ));
    }
    if let Some(tz) = &f.timezone {
        rows.push((
            "".into(),
            format!("note: Hermes ran cron in {tz}; cc-gateway uses this machine's local time (set TZ={tz} for the service)"),
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn profile(dir: &Path) -> Profile {
        Profile {
            name: "t".into(),
            dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn env_parsing() {
        let e = parse_env(
            "# comment\nexport TELEGRAM_BOT_TOKEN=\"123:abc # not a comment\"\nTELEGRAM_ALLOWED_USERS = '1, 2,x'\nPLAIN=v # trailing\nEMPTY=\nnoequals\n",
        );
        assert_eq!(e["TELEGRAM_BOT_TOKEN"], "123:abc # not a comment");
        assert_eq!(e["TELEGRAM_ALLOWED_USERS"], "1, 2,x");
        assert_eq!(e["PLAIN"], "v");
        assert_eq!(e["EMPTY"], "");
        assert!(!e.contains_key("noequals"));
    }

    #[test]
    fn model_parsing_and_mapping() {
        let nested = "plugins:\n  enabled: [a]\nmodel:\n  default: \"anthropic/claude-sonnet-4\"\n  provider: openrouter\n";
        assert_eq!(
            model_from_yaml(nested).as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(
            model_from_yaml("model: claude-opus-4 # x\n").as_deref(),
            Some("claude-opus-4")
        );
        assert_eq!(model_from_yaml("model:\n  streaming: false\n"), None);
        assert_eq!(model_from_yaml("plugins:\n  default: x\n"), None);
        assert_eq!(map_model("anthropic/claude-sonnet-4"), Some("sonnet"));
        assert_eq!(map_model("claude-3-5-haiku"), Some("haiku"));
        assert_eq!(map_model("MiniMax-M2.7"), None);
        assert_eq!(map_model("opus-like-model"), None);
    }

    #[test]
    fn full_layout_with_profiles_and_symlinked_skill() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("hermes");
        write(
            &h.join(".env"),
            "TELEGRAM_BOT_TOKEN='1:tok'\nTELEGRAM_ALLOWED_USERS=42\n",
        );
        write(&h.join("SOUL.md"), "hermes soul");
        write(&h.join("memories/MEMORY.md"), "- likes tea");
        write(&h.join("memories/USER.md"), "Name: Ana");
        write(&h.join("config.yaml"), "model:\n  default: claude-opus-4\n");
        write(&h.join("skills/devops/deploy/SKILL.md"), "deploy");
        write(&h.join("skills/devops/deploy/scripts/run.sh"), "echo");
        write(&h.join("skills/.hub/x/SKILL.md"), "hub");
        write(&h.join("skills/notes/README.md"), "not a skill");
        let ext = t.path().join("ext-skill");
        write(&ext.join("SKILL.md"), "linked");
        write(&ext.join("references/a.md"), "ref");
        std::os::unix::fs::symlink(&ext, h.join("skills/linked")).unwrap();
        write(&h.join("profiles/coder/.env"), "TELEGRAM_BOT_TOKEN=2:tok\n");
        fs::create_dir_all(h.join("profiles/empty")).unwrap();

        let srcs = sources(&h);
        let names: Vec<_> = srcs.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec![None, Some("coder".into())]);

        let f = scan(&h);
        assert_eq!(f.token.as_deref(), Some("1:tok"));
        assert_eq!(f.allowed, vec![42]);
        assert_eq!(f.model.as_deref(), Some("claude-opus-4"));
        let skill_names: Vec<_> = f
            .skills
            .iter()
            .map(|s| s.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(skill_names, vec!["deploy", "linked"]);
        assert!(!f.summary().contains("1:tok") && !f.summary().contains("42"));

        let p = profile(&t.path().join("cct"));
        write(&p.soul_path(), DEFAULT_SOUL); // fresh profile default: replaceable
        let rows = import_files(&f, &p, Some(42), &mut |_| panic!("no prompt expected"));
        assert!(
            rows.iter().all(|(_, r)| r.starts_with("imported")),
            "{rows:?}"
        );
        assert_eq!(fs::read_to_string(p.soul_path()).unwrap(), "hermes soul");
        assert_eq!(
            fs::read_to_string(p.user_dir(42).join("USER.md")).unwrap(),
            "Name: Ana"
        );
        let sk = p.workspace().join(".claude/skills");
        assert!(sk.join("deploy/scripts/run.sh").is_file());
        let linked = sk.join("linked/references/a.md");
        assert!(
            linked.is_file()
                && !fs::symlink_metadata(sk.join("linked"))
                    .unwrap()
                    .is_symlink()
        );

        // re-import: existing content is not overwritten without confirmation
        write(&p.soul_path(), "edited");
        let rows = import_files(&f, &p, Some(42), &mut |_| false);
        assert_eq!(fs::read_to_string(p.soul_path()).unwrap(), "edited");
        assert!(
            rows.iter().all(|(_, r)| r.starts_with("skipped")),
            "{rows:?}"
        );
    }

    #[test]
    fn partial_layout_and_unowned_memory() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("hermes");
        write(&h.join("config.yaml"), "plugins:\n  enabled:\n    - x\n");
        write(&h.join("memories/MEMORY.md"), "- fact");
        let f = scan(&h);
        assert!(f.token.is_none() && f.allowed.is_empty() && f.model.is_none());
        let p = profile(&t.path().join("cct"));
        let rows = import_files(&f, &p, None, &mut |_| true);
        assert_eq!(
            fs::read_to_string(p.imported_dir().join("MEMORY.md")).unwrap(),
            "- fact"
        );
        let get = |k: &str| rows.iter().find(|(i, _)| i == k).unwrap().1.clone();
        assert!(get("SOUL.md").starts_with("skipped"));
        assert!(get("USER.md").starts_with("skipped"));

        // first successful pair claims it
        p.claim_imported(7).unwrap();
        assert_eq!(
            fs::read_to_string(p.user_dir(7).join("MEMORY.md")).unwrap(),
            "- fact"
        );
        assert!(!p.imported_dir().exists());
    }

    #[test]
    fn yaml_lookup() {
        let y = "model:\n  default: gpt-5.6-sol  # c\n  provider: openai-codex\nplatforms:\n  telegram:\n    enabled: true\n    home_channel:\n      platform: telegram\n      chat_id: '111'\n      user_id: 222\n      name: Someone\n  discord:\n    home_channel:\n      user_id: 999\nhooks:\n  on_start:\n    - command: \"x: y\"\n      timeout: 10\ntimezone: Asia/Jakarta\n";
        let hc = |k| yaml_get(y, &["platforms", "telegram", "home_channel", k]);
        assert_eq!(hc("chat_id").as_deref(), Some("111"));
        assert_eq!(hc("user_id").as_deref(), Some("222"));
        assert_eq!(
            yaml_get(y, &["platforms", "discord", "home_channel", "user_id"]).as_deref(),
            Some("999")
        );
        assert_eq!(yaml_get(y, &["timezone"]).as_deref(), Some("Asia/Jakarta"));
        assert_eq!(model_from_yaml(y).as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(yaml_get(y, &["platforms", "telegram"]), None); // a mapping, not a scalar
        assert_eq!(yaml_get("a:\n  b: null\n", &["a", "b"]), None);
    }

    /// The real server layout: home channel owner + approved pairings, no TELEGRAM_ALLOWED_USERS,
    /// an empty legacy `pairing/` dir.
    #[test]
    fn allowlist_from_pairings_and_home_channel_owner() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        write(
            &h.join(".env"),
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_HOME_CHANNEL=222\nTELEGRAM_HOME_CHANNEL_THREAD_ID=0\n",
        );
        write(
            &h.join("config.yaml"),
            "platforms:\n  telegram:\n    enabled: true\n    home_channel:\n      platform: telegram\n      chat_id: '222'\n      name: X\n      user_id: '222'\n",
        );
        fs::create_dir_all(h.join("pairing")).unwrap();
        write(
            &h.join("platforms/pairing/telegram-approved.json"),
            r#"{"222": {"user_name": "a", "approved_at": 1}, "333": {"user_name": "b", "approved_at": 2}}"#,
        );
        let f = scan(h);
        assert_eq!(f.allowed, vec![222, 333]);
        assert_eq!((f.owner, f.home_chat), (Some(222), Some(222)));
        assert!(!f.allow_all && !f.discord_only);
        let sum = f.summary();
        assert!(!sum.contains("222") && !sum.contains("1:x"), "{sum}");

        // legacy pairing/ with content wins, like Hermes' get_hermes_dir
        write(&h.join("pairing/telegram-approved.json"), r#"{"444": {}}"#);
        assert_eq!(scan(h).allowed, vec![444, 222]);
    }

    #[test]
    fn owner_from_private_home_channel_env_and_env_allowlists() {
        let t = tempfile::tempdir().unwrap();
        write(
            &t.path().join(".env"),
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_HOME_CHANNEL=555\nGATEWAY_ALLOWED_USERS=7\nTELEGRAM_ALLOWED_USERS=8, 9\n",
        );
        let f = scan(t.path());
        assert_eq!(f.owner, Some(555));
        assert_eq!(f.allowed, vec![7, 8, 9, 555]);
        // a group home channel is not a user id
        write(
            &t.path().join(".env"),
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_HOME_CHANNEL=-100123\nTELEGRAM_ALLOWED_USERS=8\n",
        );
        let f = scan(t.path());
        assert_eq!((f.owner, f.home_chat), (Some(8), Some(-100123)));
    }

    #[test]
    fn allow_all_imports_owner_only() {
        let t = tempfile::tempdir().unwrap();
        for env in [
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_ALLOW_ALL_USERS=true\nTELEGRAM_ALLOWED_USERS=8,9\nTELEGRAM_HOME_CHANNEL=5\n",
            "TELEGRAM_BOT_TOKEN=1:x\nGATEWAY_ALLOW_ALL_USERS=yes\nTELEGRAM_HOME_CHANNEL=5\n",
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_ALLOWED_USERS=*\nTELEGRAM_HOME_CHANNEL=5\n",
        ] {
            write(&t.path().join(".env"), env);
            let f = scan(t.path());
            assert!(f.allow_all, "{env}");
            assert_eq!(f.allowed, vec![5]);
        }
    }

    #[test]
    fn discord_only_and_tokenless_profiles() {
        let t = tempfile::tempdir().unwrap();
        write(
            &t.path().join(".env"),
            "DISCORD_BOT_TOKEN=abc\nDISCORD_ALLOW_ALL_USERS=true\n",
        );
        let f = scan(t.path());
        assert!(f.discord_only && f.token.is_none());
        assert!(f.summary().starts_with("Discord-only"));
        write(&t.path().join(".env"), "SOME_KEY=1\n");
        let f = scan(t.path());
        assert!(!f.discord_only && f.token.is_none());
    }

    const JOBS: &str = r#"{"jobs": [
      {"id": "a1", "name": "daily summary", "prompt": "", "script": "finance/summary.py", "no_agent": true,
       "schedule": {"kind": "cron", "expr": "0 14 * * *", "display": "0 14 * * *"},
       "repeat": {"times": null, "completed": 5}, "enabled": true, "state": "scheduled", "paused_at": null,
       "deliver": "telegram", "workdir": null},
      {"id": "b2", "name": "watch", "prompt": "x", "script": "check.sh", "no_agent": true,
       "schedule": {"kind": "interval", "minutes": 5, "display": "every 5m"},
       "repeat": {"times": null, "completed": 2}, "enabled": true, "paused_at": null, "deliver": "local"},
      {"id": "c3", "name": "morning brief", "prompt": "Summarize my day", "no_agent": false,
       "schedule": {"kind": "cron", "expr": "30 7 * * 1-5"}, "repeat": {"times": null, "completed": 0},
       "enabled": true, "paused_at": "2026-10-01T00:00:00", "deliver": "telegram:-100777", "skills": ["x"]},
      {"id": "d4", "name": "once", "prompt": "p", "schedule": {"kind": "once", "run_at": "2026-12-01T00:00:00"},
       "repeat": {"times": 1, "completed": 0}, "enabled": true},
      {"id": "e5", "name": "done", "prompt": "p", "schedule": {"kind": "cron", "expr": "0 0 * * *"},
       "repeat": {"times": 3, "completed": 3}, "enabled": true},
      {"id": "f6", "name": "from chat", "prompt": "p", "schedule": {"kind": "cron", "expr": "0 1 * * *"},
       "deliver": "origin", "origin": {"platform": "telegram", "chat_id": "-100555", "thread_id": null}},
      {"id": "g7", "name": "from discord", "prompt": "p", "schedule": {"kind": "cron", "expr": "0 1 * * *"},
       "deliver": "origin", "origin": {"platform": "discord", "chat_id": "1"}},
      {"id": "h8", "name": "gone workdir", "prompt": "", "script": "a/b.py", "no_agent": true,
       "schedule": {"kind": "cron", "expr": "0 2 * * *"}, "deliver": "telegram", "workdir": "/nonexistent/wd"}
    ]}"#;

    #[test]
    fn cron_jobs_map_to_schedules() {
        let jobs = parse_jobs(JOBS);
        assert_eq!(jobs.len(), 8);
        let ws = Path::new("/ws");
        let to = |j: &HermesJob| j.to_sched(42, 7, ws);
        let a = to(&jobs[0]).unwrap();
        assert_eq!(
            (a.cron.as_str(), a.chat_id, a.enabled, a.deliver),
            ("0 14 * * *", 42, true, true)
        );
        assert_eq!(a.command, vec!["python3", "/ws/scripts/finance/summary.py"]);
        assert_eq!(a.cwd.as_deref(), Some("scripts/finance"));
        let b = to(&jobs[1]).unwrap();
        assert_eq!((b.every_minutes, b.deliver), (Some(5), false));
        assert_eq!(b.command, vec!["bash", "/ws/scripts/check.sh"]);
        assert_eq!(b.cwd.as_deref(), Some("scripts"));
        let c = to(&jobs[2]).unwrap();
        assert_eq!(
            (c.prompt.as_str(), c.enabled, c.chat_id),
            ("Summarize my day", false, -100777)
        );
        assert!(c.command.is_empty());
        assert!(jobs[2].notes.iter().any(|n| n.contains("skills")));
        assert!(to(&jobs[3]).unwrap_err().contains("once"));
        let e = to(&jobs[4]).unwrap();
        assert!(!e.enabled); // repeat limit reached
        assert_eq!(e.chat_id, 7); // origin without a recorded chat → owner DM, not the group
        assert_eq!(to(&jobs[5]).unwrap().chat_id, -100555); // telegram origin
        assert!(!to(&jobs[6]).unwrap().deliver); // discord origin: silent
        let h = to(&jobs[7]).unwrap();
        assert_eq!(h.command[1], "/ws/scripts/a/b.py"); // absolute even with a workdir
        assert_eq!(h.cwd.as_deref(), Some("scripts/a")); // missing workdir → script folder
        let bad = parse_jobs(
            r#"{"jobs":[{"name":"x","no_agent":true,"script":"../etc/passwd","schedule":{"kind":"cron","expr":"0 0 * * *"}}]}"#,
        );
        assert!(bad[0].to_sched(1, 1, ws).is_err());
    }

    #[test]
    fn import_jobs_writes_schedules_copies_scripts_and_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("hermes");
        write(
            &h.join(".env"),
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_HOME_CHANNEL=42\n",
        );
        write(&h.join("cron/jobs.json"), JOBS);
        write(&h.join("scripts/finance/summary.py"), "print('hi')");
        write(&h.join("scripts/check.sh"), "echo ok");
        let f = scan(&h);
        assert_eq!(f.jobs.len(), 8);
        let p = profile(&t.path().join("cct"));
        let rows = import_jobs(&f, &p, f.owner, &mut |_| false);
        let list = crate::schedule::load(&p, 42);
        assert_eq!(list.len(), 7, "{rows:?}");
        assert!(p.workspace().join("scripts/finance/summary.py").is_file());
        assert!(
            rows.iter()
                .any(|(i, r)| i == "cron once" && r.contains("skipped"))
        );
        assert!(rows.iter().any(|(_, r)| r.contains("python3")));
        // again: nothing duplicated
        import_jobs(&f, &p, f.owner, &mut |_| false);
        assert_eq!(crate::schedule::load(&p, 42).len(), 7);
        // no owner → nothing imported
        let none = Found {
            jobs: f.jobs.clone(),
            ..Found::default()
        };
        let rows = import_jobs(&none, &p, None, &mut |_| false);
        assert!(rows.iter().all(|(_, r)| r.contains("no owner")));
    }

    #[test]
    fn hermes_home_pointing_at_a_profile_dir_is_named() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("profiles/admin-x");
        write(&p.join(".env"), "TELEGRAM_BOT_TOKEN=1:x\n");
        let srcs = sources(&p);
        assert_eq!(srcs.len(), 1);
        assert_eq!(srcs[0].name.as_deref(), Some("admin-x"));
        assert!(
            sources(t.path())
                .iter()
                .any(|s| s.name.as_deref() == Some("admin-x"))
        );
    }

    #[test]
    fn skill_name_clash_is_renamed_not_replaced() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("h");
        write(&h.join("skills/apple/notes/SKILL.md"), "apple");
        write(&h.join("skills/linux/notes/SKILL.md"), "linux");
        let f = scan(&h);
        let p = profile(&t.path().join("cct"));
        let rows = import_files(&f, &p, None, &mut |_| true);
        let sk = p.workspace().join(".claude/skills");
        assert_eq!(
            fs::read_to_string(sk.join("notes/SKILL.md")).unwrap(),
            "apple"
        );
        assert_eq!(
            fs::read_to_string(sk.join("linux-notes/SKILL.md")).unwrap(),
            "linux"
        );
        assert!(rows.iter().any(|(i, _)| i.contains("renamed")));
    }

    #[test]
    fn non_numeric_allowlist_entries_are_counted() {
        let t = tempfile::tempdir().unwrap();
        write(
            &t.path().join(".env"),
            "TELEGRAM_BOT_TOKEN=1:x\nTELEGRAM_ALLOWED_USERS=12,@bob,alice\n",
        );
        let f = scan(t.path());
        assert_eq!((f.allowed.clone(), f.dropped_ids), (vec![12], 2));
    }
}
