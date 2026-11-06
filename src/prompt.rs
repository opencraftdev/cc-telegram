//! System prompt assembly, rebuilt every run so memory edits apply on the next turn.
//!
//! Mirrors Hermes Agent's prompt (agent/system_prompt.py + prompt_builder.py): SOUL.md as the
//! identity slot, platform hint, project context file, profile-wide MEMORY.md / USER.md
//! rendered as `§`-delimited blocks with usage headers, then the date. On top of Hermes we
//! keep per-person notes (one bot, several users) and optional imported history.

use crate::profile::Profile;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Hermes' default limits (memory.memory_char_limit / user_char_limit).
pub const MEMORY_LIMIT: usize = 2200;
pub const USER_LIMIT: usize = 1375;
/// Per-person notes (cc-gateway only).
pub const PERSON_LIMIT: usize = 1375;
/// Hermes' fallback cap for a context file.
const CONTEXT_FILE_MAX: usize = 20_000;
/// Context files Hermes reads from the working dir (first found wins). CLAUDE.md is not
/// listed: claude loads it by itself.
const CONTEXT_FILES: [&str; 4] = [".hermes.md", "HERMES.md", "AGENTS.md", "agents.md"];

/// Where a run comes from.
pub enum Channel {
    /// Sender's display name / @username, and the chat outbox for files.
    Telegram {
        sender: String,
        outbox: PathBuf,
    },
    Api {
        key: String,
    },
}

/// Everything the prompt is built from (read from disk by `assemble`).
pub struct Parts {
    pub soul: String,
    pub channel: Channel,
    pub shared_memory: (PathBuf, String),
    pub shared_user: (PathBuf, String),
    pub person_dir: PathBuf,
    pub person_user: String,
    pub person_memory: String,
    pub context_file: Option<(String, String)>,
    pub history: Option<String>,
    pub now: chrono::DateTime<chrono::Local>,
}

/// A Hermes-style memory block: separator, header with usage, entries.
fn block(title: &str, path: &Path, content: &str, limit: usize) -> String {
    let content = content.trim();
    let n = content.chars().count();
    let pct = (n * 100 / limit.max(1)).min(100);
    let sep = "═".repeat(46);
    let body = if content.is_empty() {
        "(empty)"
    } else {
        content
    };
    format!(
        "{sep}\n{title} [{pct}% — {n}/{limit} chars] — file: {}\n{sep}\n{body}",
        path.display()
    )
}

pub fn build(p: &Parts) -> String {
    let mut out = vec![];
    out.push(format!(
        "# Identity\n\
         The persona below (SOUL.md) defines who you are: your name, personality, tone and language. \
         It overrides any default assistant identity given earlier — do not introduce yourself as \
         Claude or Claude Code unless SOUL.md says so. Your tools and how to use them stay as \
         described above.\n\n{}",
        p.soul.trim()
    ));
    out.push(match &p.channel {
        Channel::Telegram { sender, outbox } => format!(
            "# Platform\n\
             You are on a text messaging platform, Telegram, talking with {sender}. Standard \
             Markdown is converted to Telegram formatting (bold, italic, `code`, code blocks, \
             links). Keep replies concise; prefer bullet lists and labeled key: value pairs for \
             structured data. To send a file, write it into {}/ — it is delivered after your reply.",
            outbox.display()
        ),
        Channel::Api { key } => format!(
            "# Platform\nYou are answering a request through an HTTP API (API key \"{key}\"). \
             Answer in Markdown."
        ),
    });
    if let Some((name, content)) = &p.context_file {
        out.push(format!("# Project context ({name})\n\n{}", content.trim()));
    }
    out.push(format!(
        "# Memory\n\
         You have persistent memory across sessions, in plain files you edit yourself with your \
         file tools. It is injected into every turn, so keep it compact and focused on facts that \
         will still matter later.\n\
         - Shared memory (MEMORY.md, USER.md below) is common to everyone who uses this bot: facts \
         about the work, the owner, the environment, tool quirks and stable conventions.\n\
         - Notes about the person you're talking to (their own USER.md / MEMORY.md in {pd}) are only \
         shown when that person writes: their preferences, how they want to be addressed, recurring \
         corrections.\n\
         - Prioritize what reduces future steering: preferences and recurring corrections matter \
         more than task details. Do not save task progress, completed-work logs, PR/issue numbers \
         or anything stale within a week.\n\
         - Write declarative facts, not instructions to yourself (\"User prefers concise answers\" ✓, \
         \"Always answer concisely\" ✗).\n\
         - Format: one entry per fact; entries are separated by a line containing only `§`. Keep \
         that format when editing. Stay under each file's limit shown in its header: prune or merge \
         old entries instead of appending forever.\n\
         - Never store secrets (passwords, tokens, keys).",
        pd = p.person_dir.display()
    ));
    out.push(block(
        "MEMORY (shared notes for this bot)",
        &p.shared_memory.0,
        &p.shared_memory.1,
        MEMORY_LIMIT,
    ));
    out.push(block(
        "USER PROFILE (who the user/owner is)",
        &p.shared_user.0,
        &p.shared_user.1,
        USER_LIMIT,
    ));
    out.push(block(
        "THIS PERSON (who is writing now)",
        &p.person_dir.join("USER.md"),
        &p.person_user,
        PERSON_LIMIT,
    ));
    out.push(block(
        "NOTES ABOUT THIS PERSON",
        &p.person_dir.join("MEMORY.md"),
        &p.person_memory,
        PERSON_LIMIT,
    ));
    if let Some(h) = &p.history {
        out.push(format!(
            "# Earlier conversation (imported from the previous assistant, for context only)\n\
             This chat was moved from another assistant. These are its last messages; continue \
             naturally from them, don't repeat or answer them again.\n\n{}",
            h.trim()
        ));
    }
    out.push(format!(
        "Current date: {} (timezone UTC{})",
        p.now.format("%A, %B %d, %Y %H:%M"),
        p.now.format("%:z")
    ));
    out.join("\n\n") + "\n"
}

/// First Hermes-style context file in `cwd`, capped.
fn context_file(cwd: &Path) -> Option<(String, String)> {
    CONTEXT_FILES.iter().find_map(|n| {
        let s = std::fs::read_to_string(cwd.join(n)).ok()?;
        let s: String = s.chars().take(CONTEXT_FILE_MAX).collect();
        (!s.trim().is_empty()).then(|| (n.to_string(), s))
    })
}

fn ensure(p: &Path) -> Result<String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    if !p.exists() {
        std::fs::write(p, "")?;
    }
    Ok(std::fs::read_to_string(p).unwrap_or_default())
}

/// Read SOUL, shared memory (profile-level), the person's notes (`users/<ns>/`) and the
/// context file in `cwd`, creating empty memory files as needed.
pub fn assemble(
    profile: &Profile,
    ns: &str,
    channel: Channel,
    cwd: &Path,
    history: Option<String>,
) -> Result<String> {
    let person_dir = profile.ns_dir(ns);
    let shared_memory = profile.dir.join("MEMORY.md");
    let shared_user = profile.dir.join("USER.md");
    Ok(build(&Parts {
        soul: std::fs::read_to_string(profile.soul_path()).unwrap_or_default(),
        channel,
        shared_memory: (shared_memory.clone(), ensure(&shared_memory)?),
        shared_user: (shared_user.clone(), ensure(&shared_user)?),
        person_user: ensure(&person_dir.join("USER.md"))?,
        person_memory: ensure(&person_dir.join("MEMORY.md"))?,
        person_dir,
        context_file: context_file(cwd),
        history,
        now: chrono::Local::now(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Parts {
        Parts {
            soul: "You are Budi.".into(),
            channel: Channel::Telegram {
                sender: "Ana (@ana)".into(),
                outbox: "/p/workspace/outbox/9".into(),
            },
            shared_memory: (
                "/p/MEMORY.md".into(),
                "Office is in Bandung\n§\nUses Odoo".into(),
            ),
            shared_user: ("/p/USER.md".into(), String::new()),
            person_dir: "/p/users/9".into(),
            person_user: "Prefers Indonesian".into(),
            person_memory: String::new(),
            context_file: Some(("AGENTS.md".into(), "Repo rules".into())),
            history: Some("**User**: hi".into()),
            now: chrono::Local::now(),
        }
    }

    #[test]
    fn soul_first_then_platform_context_memory_history_date() {
        let p = build(&parts());
        let at = |s: &str| p.find(s).unwrap_or_else(|| panic!("missing {s:?} in\n{p}"));
        let order = [
            at("# Identity"),
            at("You are Budi."),
            at("talking with Ana (@ana)"),
            at("# Project context (AGENTS.md)"),
            at("# Memory"),
            at("MEMORY (shared notes for this bot) ["),
            at("USER PROFILE (who the user/owner is)"),
            at("THIS PERSON (who is writing now)"),
            at("# Earlier conversation"),
            at("Current date: "),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{p}");
        assert!(p.contains("overrides any default assistant identity"));
        assert!(p.contains("write it into /p/workspace/outbox/9/"));
        assert!(p.contains("Office is in Bandung\n§\nUses Odoo"));
        assert!(p.contains("/2200 chars] — file: /p/MEMORY.md"));
        assert!(p.contains("/1375 chars] — file: /p/USER.md\n══"));
        assert!(p.contains("separated by a line containing only `§`"));
        assert!(p.contains("file: /p/users/9/USER.md"));
    }

    #[test]
    fn api_has_no_outbox_and_history_is_optional() {
        let mut pt = parts();
        pt.channel = Channel::Api {
            key: "laptop".into(),
        };
        pt.history = None;
        pt.context_file = None;
        let p = build(&pt);
        assert!(p.contains("HTTP API (API key \"laptop\")"));
        assert!(
            !p.contains("outbox")
                && !p.contains("Earlier conversation")
                && !p.contains("Project context")
        );
    }

    #[test]
    fn usage_header_counts_chars() {
        let b = block("MEMORY", Path::new("/m"), &"é".repeat(220), 2200);
        assert!(b.contains("[10% — 220/2200 chars]"));
    }
}
