//! System prompt assembly from SOUL.md + per-user USER.md / MEMORY.md (rebuilt every run).

use crate::profile::Profile;
use anyhow::Result;
use std::path::Path;

pub fn build(
    soul: &str,
    user_path: &Path,
    user: &str,
    memory_path: &Path,
    memory: &str,
    outbox: Option<&Path>,
) -> String {
    // Telegram can deliver files and wants short replies; the API returns text only.
    let channel = match outbox {
        Some(ob) => format!(
            "- To send a file back, write it into {}/.\n- You are replying in Telegram: be concise, use plain Markdown.",
            ob.display()
        ),
        None => "- You are replying through an HTTP API: answer in Markdown.".into(),
    };
    format!(
        "{soul}

## About this user (USER.md — path: {up})
{user}

## Your memory of this user (MEMORY.md — path: {mp})
{memory}

## Memory rules
- When you learn something durable about the user or their preferences, Edit USER.md / MEMORY.md at the paths above.
- Keep each file under 2000 characters; prune or merge old entries instead of appending forever.
- Never store secrets (passwords, tokens, keys).
{channel}
",
        soul = soul.trim_end(),
        up = user_path.display(),
        user = user.trim_end(),
        mp = memory_path.display(),
        memory = memory.trim_end(),
    )
}

/// Telegram: the user's memory + the chat's outbox.
pub fn for_user(profile: &Profile, user_id: u64, chat_id: i64) -> Result<String> {
    let outbox = profile.outbox(chat_id);
    std::fs::create_dir_all(&outbox)?;
    for_ns(profile, &user_id.to_string(), Some(&outbox))
}

/// Ensure the namespace's memory files exist, then build the prompt.
pub fn for_ns(profile: &Profile, ns: &str, outbox: Option<&Path>) -> Result<String> {
    let dir = profile.ns_dir(ns);
    std::fs::create_dir_all(&dir)?;
    let (user_path, mem_path) = (dir.join("USER.md"), dir.join("MEMORY.md"));
    for p in [&user_path, &mem_path] {
        if !p.exists() {
            std::fs::write(p, "")?;
        }
    }
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    Ok(build(
        &read(&profile.soul_path()),
        &user_path,
        &read(&user_path),
        &mem_path,
        &read(&mem_path),
        outbox,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_sections_in_order() {
        let p = build(
            "I am soul.\n",
            Path::new("/p/users/1/USER.md"),
            "Name: Ana",
            Path::new("/p/users/1/MEMORY.md"),
            "- likes tea",
            Some(Path::new("/p/workspace/outbox/9")),
        );
        let i_soul = p.find("I am soul.").unwrap();
        let i_user = p
            .find("## About this user (USER.md — path: /p/users/1/USER.md)\nName: Ana")
            .unwrap();
        let i_mem = p
            .find(
                "## Your memory of this user (MEMORY.md — path: /p/users/1/MEMORY.md)\n- likes tea",
            )
            .unwrap();
        let i_rules = p.find("## Memory rules").unwrap();
        assert!(i_soul < i_user && i_user < i_mem && i_mem < i_rules);
        assert!(p.contains("write it into /p/workspace/outbox/9/."));
        assert!(p.contains("under 2000 characters"));
        assert!(p.contains("replying in Telegram"));
        let api = build("s", Path::new("/u"), "", Path::new("/m"), "", None);
        assert!(!api.contains("outbox") && api.contains("HTTP API"));
    }
}
