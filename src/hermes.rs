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
    if is_home(home) {
        out.push(Source {
            name: None,
            dir: home.to_path_buf(),
        });
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
    pub allowed: Vec<u64>,
    pub soul: Option<PathBuf>,
    pub memory: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub model: Option<String>,
    pub skills: Vec<PathBuf>,
}

fn file(p: PathBuf) -> Option<PathBuf> {
    p.is_file().then_some(p)
}

pub fn scan(dir: &Path) -> Found {
    let env = std::fs::read_to_string(dir.join(".env"))
        .map(|s| parse_env(&s))
        .unwrap_or_default();
    let yaml = std::fs::read_to_string(dir.join("config.yaml")).unwrap_or_default();
    Found {
        token: env
            .get("TELEGRAM_BOT_TOKEN")
            .filter(|t| !t.is_empty())
            .cloned(),
        allowed: env
            .get("TELEGRAM_ALLOWED_USERS")
            .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_default(),
        soul: file(dir.join("SOUL.md")),
        memory: file(dir.join("memories/MEMORY.md")),
        user: file(dir.join("memories/USER.md")),
        model: model_from_yaml(&yaml),
        skills: find_skills(&dir.join("skills")),
    }
}

impl Found {
    /// Counts and flags only: no token, no ids, no file contents.
    pub fn summary(&self) -> String {
        let yn = |b: bool| if b { "yes" } else { "no" };
        let model = match &self.model {
            Some(m) => match map_model(m) {
                Some(c) => format!("{m} (→ {c})"),
                None => format!("{m} (not Claude, keeps cc-telegram default)"),
            },
            None => "none".into(),
        };
        format!(
            "bot token: {} · allowed users: {} · SOUL.md: {} · MEMORY.md: {} · USER.md: {} · model: {model} · skills: {}",
            yn(self.token.is_some()),
            self.allowed.len(),
            yn(self.soul.is_some()),
            yn(self.memory.is_some()),
            yn(self.user.is_some()),
            self.skills.len()
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

/// `model: <id>` or `model:\n  default: <id>` at the top level of config.yaml.
pub fn model_from_yaml(yaml: &str) -> Option<String> {
    let mut in_model = false;
    for line in yaml.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let top = !line.starts_with([' ', '\t']);
        if top {
            in_model = false;
            if let Some(v) = line.strip_prefix("model:") {
                let v = unquote(v);
                if !v.is_empty() {
                    return Some(v.to_string());
                }
                in_model = true;
            }
        } else if in_model && let Some(v) = line.trim_start().strip_prefix("default:") {
            let v = unquote(v);
            return (!v.is_empty()).then(|| v.to_string());
        }
    }
    None
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
    for s in &f.skills {
        let name = s
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let dst = skills_dir.join(&name);
        let item = format!("skill {name}");
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
}
