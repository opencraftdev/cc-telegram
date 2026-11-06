//! Telegram side: dispatcher, allowlist filter, chat commands, and running one job end to end.

use crate::profile::{PairStore, Profile, Redeem};
use crate::queue::{Job, Queues};
use crate::runner::{self, Outcome, RunOpts, truncate};
use crate::state::StateStore;
use crate::{media, prompt, schedule};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, MessageId, ParseMode,
};
use tokio::sync::{Notify, mpsc};
use tracing::{info, warn};

pub struct App {
    pub bot: Bot,
    pub profile: Profile,
    pub state: StateStore,
    pub queues: Queues,
}

pub async fn run(profile: Profile) -> Result<()> {
    let cfg = profile.load_config()?;
    let bot = Bot::new(&cfg.bot_token);
    let me = bot.get_me().await.map_err(|e| {
        anyhow::anyhow!(
            "✗ token rejected by Telegram ({e}) → run: cc-telegram profile edit {}",
            profile.name
        )
    })?;
    info!(profile = %profile.name, bot = %me.username(), "bot running");
    let app = Arc::new(App {
        bot: bot.clone(),
        state: StateStore::open(profile.state_path()),
        profile,
        queues: Queues::default(),
    });
    tokio::spawn(schedule::run_loop(app.clone()));
    tokio::spawn(shutdown_on_signal());

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_callback_query().endpoint(on_callback));
    let listener = teloxide::update_listeners::polling_default(bot.clone()).await;
    let name = app.profile.name.clone();
    let on_poll_error = move |e: teloxide::RequestError| {
        let name = name.clone();
        async move {
            match e {
                teloxide::RequestError::Api(teloxide::ApiError::TerminatedByOtherGetUpdates) => {
                    warn!("{}", conflict_hint(&name))
                }
                e => warn!("polling error: {e}"),
            }
        }
    };
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app])
        .default_handler(|_| async {})
        .build()
        .dispatch_with_listener(listener, Arc::new(on_poll_error))
        .await;
    Ok(())
}

/// Telegram 409: another process (often a Hermes gateway) polls with the same token.
pub fn conflict_hint(profile: &str) -> String {
    format!(
        "✗ Telegram 409 Conflict: another bot instance is polling with this token. If it's Hermes \
         Agent, stop its gateway → run: hermes gateway stop (or: hermes -p <name> gateway stop); \
         or give profile {profile} its own bot → run: cc-telegram profile edit {profile}"
    )
}

/// launchd/systemd stop us with SIGTERM: claude runs live in their own process groups, so
/// kill those first or they'd outlive the daemon.
async fn shutdown_on_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        warn!("could not install signal handlers");
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    info!("shutting down; killing running claude jobs");
    runner::kill_all();
    std::process::exit(0);
}

/// `/cmd@bot args` -> ("cmd", "args").
pub fn parse_command(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix('/')?;
    let (head, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let cmd = head.split('@').next()?.to_ascii_lowercase();
    (!cmd.is_empty()).then_some((cmd, args.trim()))
}

async fn on_message(app: Arc<App>, msg: Message) -> Result<()> {
    let Some(user) = msg.from.as_ref() else {
        return Ok(());
    };
    let (uid, chat) = (user.id.0, msg.chat.id);
    let cfg = app.profile.load_config()?; // re-read so `allow`/`deny`/pairing apply live
    let cmd = msg.text().and_then(parse_command);

    // Allowlist gate: nothing below runs (no download, no spawn) for strangers.
    if !cfg.is_allowed(uid) {
        if let Some((c, code)) = &cmd
            && c == "pair"
        {
            let res = {
                let _lock = app.profile.lock()?; // CLI may be issuing codes / editing the allowlist
                let mut store = PairStore::load(&app.profile.pair_path());
                let res = store.redeem(uid, code, chrono::Utc::now().timestamp());
                store.save(&app.profile.pair_path())?;
                if res == Redeem::Ok {
                    let mut cfg = app.profile.load_config()?;
                    cfg.allowed_user_ids.push(uid);
                    app.profile.save_config(&cfg)?;
                    app.profile.claim_imported(uid)?;
                }
                res
            };
            match res {
                Redeem::Ok => {
                    info!(user_id = uid, username = ?user.username, "paired");
                    app.bot
                        .send_message(chat, "Paired ✅ Send me anything.")
                        .await?;
                }
                Redeem::Invalid => {
                    warn!(user_id = uid, username = ?user.username, "wrong pair code");
                    app.bot
                        .send_message(chat, "✗ invalid or expired code")
                        .await?;
                }
                Redeem::Locked => {
                    warn!(user_id = uid, username = ?user.username, "pair attempt while locked out")
                }
            }
            return Ok(());
        }
        warn!(user_id = uid, username = ?user.username, "not allowed: dropped message");
        return Ok(());
    }

    let reply = |t: String| app.bot.send_message(chat, t);
    if let Some((c, args)) = cmd {
        match c.as_str() {
            "start" => {
                reply(format!(
                    "👋 Hi {}! Your user id is {uid}. Send me anything.",
                    user.first_name
                ))
                .await?;
                return Ok(());
            }
            "pair" => {
                reply("You're already paired ✅".into()).await?;
                return Ok(());
            }
            "new" => {
                app.state.reset_session(chat.0);
                reply("🆕 New session. Memory files are kept.".into()).await?;
                return Ok(());
            }
            "stop" => {
                let text = match app.queues.stop(chat.0) {
                    (false, 0) => "Nothing running.".to_string(),
                    (running, n) => format!(
                        "🛑 {}cleared {n} queued job(s).",
                        if running {
                            "Stopped the running job, "
                        } else {
                            ""
                        }
                    ),
                };
                reply(text).await?;
                return Ok(());
            }
            "status" => {
                let cs = app.state.get(chat.0);
                let (queued, running) = app.queues.info(chat.0);
                reply(format!(
                    "Model: {}\nSession: {}\nQueued: {queued}\nRunning: {}\nLast run cost: {}",
                    cs.model.as_deref().unwrap_or(&cfg.default_model),
                    cs.session_id
                        .as_deref()
                        .map_or("none".into(), |s| truncate(s, 8)),
                    running.map_or("no".into(), |d| format!("{}s", d.as_secs())),
                    cs.last_cost_usd.map_or("–".into(), |c| format!("${c:.4}")),
                ))
                .await?;
                return Ok(());
            }
            "model" => {
                if args.is_empty() {
                    let cur = app.state.get(chat.0).model.unwrap_or(cfg.default_model);
                    reply(format!(
                        "Model: {cur}\nUsage: /model opus|sonnet|haiku|<full-id>"
                    ))
                    .await?;
                } else {
                    app.state
                        .update(chat.0, |c| c.model = Some(args.to_string()));
                    reply(format!("Model set to {args} for this chat.")).await?;
                }
                return Ok(());
            }
            "memory" => {
                let dir = app.profile.user_dir(uid);
                let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
                let (m, u) = (read("MEMORY.md"), read("USER.md"));
                let text = format!(
                    "MEMORY.md\n{}\n\nUSER.md\n{}",
                    if m.trim().is_empty() {
                        "(empty)"
                    } else {
                        m.trim()
                    },
                    if u.trim().is_empty() {
                        "(empty)"
                    } else {
                        u.trim()
                    },
                );
                for chunk in split_message(&text, 4096) {
                    reply(chunk).await?;
                }
                return Ok(());
            }
            "forget" => {
                let kb = InlineKeyboardMarkup::new([[
                    InlineKeyboardButton::callback("Yes, wipe", "forget:yes"),
                    InlineKeyboardButton::callback("Cancel", "forget:no"),
                ]]);
                app.bot
                    .send_message(chat, "Wipe your MEMORY.md and USER.md?")
                    .reply_markup(kb)
                    .await?;
                return Ok(());
            }
            "schedule" => {
                let parts: Vec<&str> = args.splitn(6, char::is_whitespace).collect();
                let res = match parts.as_slice() {
                    [m, h, dom, mon, dow, p] if !p.trim().is_empty() => schedule::add(
                        &app.profile,
                        uid,
                        chat.0,
                        &[*m, h, dom, mon, dow].join(" "),
                        p.trim(),
                    ),
                    _ => Err(anyhow::anyhow!(
                        "usage: /schedule <m h dom mon dow> <prompt>"
                    )),
                };
                reply(match res {
                    Ok(s) => format!("⏰ Scheduled #{}: `{}` → {}", s.id, s.cron, s.prompt),
                    Err(e) => format!("✗ {e}\nExample: /schedule */5 * * * * say hi"),
                })
                .await?;
                return Ok(());
            }
            "schedules" => {
                let list = schedule::load(&app.profile, uid);
                let text = if list.is_empty() {
                    "No schedules.".into()
                } else {
                    list.iter()
                        .map(|s| format!("#{} `{}` → {}", s.id, s.cron, truncate(&s.prompt, 60)))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                reply(text).await?;
                return Ok(());
            }
            "unschedule" => {
                let text = match args.trim_start_matches('#').parse::<u32>() {
                    Ok(id) if schedule::remove(&app.profile, uid, id)? => format!("Removed #{id}."),
                    Ok(id) => format!("No schedule #{id}."),
                    Err(_) => "usage: /unschedule <id>".into(),
                };
                reply(text).await?;
                return Ok(());
            }
            _ => {} // unknown commands go to Claude as text
        }
    }

    let prompt = match media::to_prompt(&app.bot, &app.profile, &cfg, &msg, uid).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(()),
        Err(e) => {
            reply(format!("✗ couldn't read that attachment: {e:#}")).await?;
            return Ok(());
        }
    };
    app.queues.push(
        &app,
        Job {
            chat_id: chat.0,
            user_id: uid,
            prompt,
            scheduled: false,
        },
    );
    Ok(())
}

async fn on_callback(app: Arc<App>, q: CallbackQuery) -> Result<()> {
    let uid = q.from.id.0;
    if !app.profile.load_config()?.is_allowed(uid) {
        warn!(user_id = uid, username = ?q.from.username, "not allowed: dropped callback");
        return Ok(());
    }
    app.bot.answer_callback_query(q.id.clone()).await?;
    let text = match q.data.as_deref() {
        Some("forget:yes") => {
            let dir = app.profile.user_dir(uid);
            for f in ["MEMORY.md", "USER.md"] {
                if dir.join(f).exists() {
                    std::fs::write(dir.join(f), "")?;
                }
            }
            "🧹 Memory wiped."
        }
        Some("forget:no") => "Cancelled.",
        _ => return Ok(()),
    };
    if let Some(m) = &q.message {
        app.bot.edit_message_text(m.chat().id, m.id(), text).await?;
    }
    Ok(())
}

/// Run one queued job: status message, claude run, reply, outbox, persist session.
pub async fn run_job(app: &Arc<App>, job: Job, stop: Arc<Notify>) {
    if let Err(e) = try_run_job(app, &job, stop).await {
        warn!("job failed: {e:#}");
        let _ = app
            .bot
            .send_message(ChatId(job.chat_id), format!("✗ {e:#}"))
            .await;
    }
}

async fn try_run_job(app: &Arc<App>, job: &Job, stop: Arc<Notify>) -> Result<()> {
    let cfg = app.profile.load_config()?;
    let chat = ChatId(job.chat_id);
    let cs = app.state.get(job.chat_id);
    let cs_epoch = cs.epoch;
    let header = if job.scheduled {
        format!("⏰ {}\n", truncate(&job.prompt, 40))
    } else {
        String::new()
    };
    let status = app
        .bot
        .send_message(chat, format!("{header}⏳ working…"))
        .await
        .ok()
        .map(|m| m.id);
    let started = Instant::now();
    let mut opts = RunOpts {
        claude_bin: crate::profile::expand_tilde(&cfg.claude_bin)
            .to_string_lossy()
            .into(),
        prompt: job.prompt.clone(),
        system_prompt: prompt::for_user(&app.profile, job.user_id, job.chat_id)?,
        model: cs.model.unwrap_or(cfg.default_model.clone()),
        max_turns: cfg.max_turns,
        // cron runs get a fresh session so they don't pollute the chat context
        resume: if job.scheduled { None } else { cs.session_id },
        extra_args: cfg.extra_claude_args.clone(),
        cwd: app.profile.workspace(),
        timeout: Duration::from_secs(cfg.run_timeout_secs),
    };
    let mut note = "";
    let mut outcome = run_with_status(app, chat, status, &header, &opts, stop.clone()).await;
    let stale = match &outcome {
        Outcome::Done(r) => r.is_stale_session(),
        Outcome::Failed(err) => runner::is_stale_text(err), // stderr-only failure
        _ => false,
    };
    if stale && opts.resume.is_some() {
        info!(chat = chat.0, "stale session; retrying without --resume");
        app.state.update(chat.0, |c| c.session_id = None);
        opts.resume = None;
        note = "🆕 started a new session\n\n";
        outcome = run_with_status(app, chat, status, &header, &opts, stop).await;
    }

    let secs = started.elapsed().as_secs();
    // If the user sent /new while this ran (epoch changed), don't store this run's session.
    let keep_session = |sid: Option<String>| {
        if !job.scheduled
            && let Some(sid) = sid
        {
            app.state.set_session_if(chat.0, cs_epoch, sid);
        }
    };
    let status_text = match outcome {
        Outcome::Done(r) => {
            app.state
                .update(chat.0, |c| c.last_cost_usd = Some(r.cost_usd));
            keep_session(r.session_id.clone());
            let body = if r.is_error {
                let why = if r.errors.is_empty() {
                    r.text.clone()
                } else {
                    r.errors.join("\n")
                };
                format!(
                    "✗ claude error: {}",
                    if why.is_empty() {
                        "unknown".into()
                    } else {
                        why
                    }
                )
            } else if r.text.trim().is_empty() {
                "(empty reply)".into()
            } else {
                r.text
            };
            let prefix = if job.scheduled {
                format!("{header}\n")
            } else {
                String::new()
            };
            send_long(&app.bot, chat, &format!("{prefix}{note}{body}")).await;
            format!(
                "{header}{} · ${:.2} · {secs}s",
                if r.is_error { "✗ failed" } else { "✅ done" },
                r.cost_usd
            )
        }
        Outcome::TimedOut(sid) => {
            keep_session(sid);
            let t = format!("⏱️ timed out after {}s", cfg.run_timeout_secs);
            app.bot.send_message(chat, &t).await?;
            format!("{header}{t}")
        }
        Outcome::Stopped(sid) => {
            keep_session(sid);
            format!("{header}🛑 stopped after {secs}s")
        }
        Outcome::Failed(err) => {
            app.bot
                .send_message(
                    chat,
                    format!("✗ claude failed: {err}\n→ run: cc-telegram doctor"),
                )
                .await?;
            format!("{header}✗ failed")
        }
    };
    if let Some(id) = status {
        let _ = app.bot.edit_message_text(chat, id, status_text).await;
    }
    send_outbox(&app.bot, chat, &app.profile.outbox(chat.0)).await;
    Ok(())
}

/// Run claude while keeping the status message (≥1.5s between edits) and typing action alive.
async fn run_with_status(
    app: &App,
    chat: ChatId,
    status: Option<MessageId>,
    header: &str,
    opts: &RunOpts,
    stop: Arc<Notify>,
) -> Outcome {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let ui = async {
        let mut lines: Vec<String> = vec![];
        let mut dirty = false;
        let mut edit = tokio::time::interval(Duration::from_millis(1500));
        let mut typing = tokio::time::interval(Duration::from_secs(4));
        loop {
            tokio::select! {
                step = rx.recv() => match step {
                    Some(l) => {
                        lines.push(l);
                        if lines.len() > 10 { lines.remove(0); }
                        dirty = true;
                    }
                    None => break,
                },
                _ = edit.tick() => if dirty && let Some(id) = status {
                    let text = format!("{header}⏳ working…\n{}", lines.join("\n"));
                    let _ = app.bot.edit_message_text(chat, id, text).await;
                    dirty = false;
                },
                _ = typing.tick() => {
                    let _ = app.bot.send_chat_action(chat, ChatAction::Typing).await;
                }
            }
        }
    };
    let (outcome, ()) = tokio::join!(runner::run(opts, tx, stop), ui);
    outcome
}

/// Send one message, honoring Telegram's 429 RetryAfter (a few times).
async fn send_text(
    bot: &Bot,
    chat: ChatId,
    text: &str,
    md: bool,
) -> Result<(), teloxide::RequestError> {
    let mut tries = 0;
    loop {
        let req = bot.send_message(chat, text);
        let req = if md {
            req.parse_mode(ParseMode::MarkdownV2)
        } else {
            req
        };
        match req.await {
            Err(teloxide::RequestError::RetryAfter(s)) if tries < 3 => {
                tries += 1;
                tokio::time::sleep(s.duration()).await;
            }
            r => return r.map(|_| ()),
        }
    }
}

async fn send_long(bot: &Bot, chat: ChatId, text: &str) {
    for chunk in split_message(text, 4096) {
        let md = to_markdown_v2(&chunk);
        let res = if utf16_len(&md) <= 4096 {
            match send_text(bot, chat, &md, true).await {
                // Only a formatting rejection (API error) falls back to plain text.
                Err(teloxide::RequestError::Api(_)) => send_text(bot, chat, &chunk, false).await,
                r => r,
            }
        } else {
            send_text(bot, chat, &chunk, false).await
        };
        if let Err(e) = res {
            warn!("sending reply: {e}");
        }
    }
}

async fn send_outbox(bot: &Bot, chat: ChatId, dir: &Path) {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    for path in files {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_photo = matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp");
        let photo_ok = is_photo && bot.send_photo(chat, InputFile::file(&path)).await.is_ok();
        let ok = photo_ok
            || bot
                .send_document(chat, InputFile::file(&path))
                .await
                .is_ok();
        if ok {
            let _ = std::fs::remove_file(&path);
        } else {
            warn!(file = %path.display(), "could not send outbox file");
        }
    }
}

// ---------- formatting ----------

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Split into chunks of at most `max` UTF-16 units (Telegram's measure), preferring newlines.
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    let mut out = vec![];
    let mut rest = text;
    while utf16_len(rest) > max {
        let mut n = 0;
        let cut = rest
            .char_indices()
            .find(|(_, c)| {
                n += c.len_utf16();
                n > max
            })
            .map_or(rest.len(), |(i, _)| i);
        let at = rest[..cut].rfind('\n').filter(|&i| i > 0).unwrap_or(cut);
        out.push(rest[..at].to_string());
        rest = rest[at..].strip_prefix('\n').unwrap_or(&rest[at..]);
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

fn escape_text(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if "_*[]()~`>#+-=|{}.!\\".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

fn escape_code(s: &str) -> String {
    s.replace('\\', "\\\\").replace('`', "\\`")
}

/// Claude's Markdown -> Telegram MarkdownV2: keeps ``` blocks, `code` and **bold**; escapes the rest.
pub fn to_markdown_v2(s: &str) -> String {
    let mut o = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("```")
            && let Some(end) = after.find("```")
        {
            let body = &after[..end];
            let (lang, code) = match body.split_once('\n') {
                Some((l, c)) if !l.contains(' ') => (l, c),
                _ => ("", body),
            };
            o.push_str(&format!("```{lang}\n{}```", escape_code(code)));
            rest = &after[end + 3..];
        } else if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find(['`', '\n'])
            && after[end..].starts_with('`')
            && end > 0
        {
            o.push_str(&format!("`{}`", escape_code(&after[..end])));
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
            && end > 0
        {
            o.push_str(&format!("*{}*", escape_text(&after[..end])));
            rest = &after[end + 2..];
        } else {
            let c = rest.chars().next().unwrap();
            o.push_str(&escape_text(&c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(parse_command("/start"), Some(("start".into(), "")));
        assert_eq!(
            parse_command("/Model@my_bot  opus "),
            Some(("model".into(), "opus"))
        );
        assert_eq!(
            parse_command("/schedule */5 * * * * say hi"),
            Some(("schedule".into(), "*/5 * * * * say hi"))
        );
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command("/"), None);
    }

    #[test]
    fn markdown_v2_escaping() {
        assert_eq!(to_markdown_v2("a.b-c!"), "a\\.b\\-c\\!");
        assert_eq!(to_markdown_v2("**bold** x"), "*bold* x");
        assert_eq!(to_markdown_v2("use `a_b.c`"), "use `a_b.c`");
        assert_eq!(
            to_markdown_v2("```rust\nlet x = a`b\\;\n```"),
            "```rust\nlet x = a\\`b\\\\;\n```"
        );
        assert_eq!(to_markdown_v2("1 * 2 (x) [y]"), "1 \\* 2 \\(x\\) \\[y\\]");
        assert_eq!(to_markdown_v2("lone ` tick"), "lone \\` tick");
        assert_eq!(to_markdown_v2("**"), "\\*\\*");
        assert_eq!(
            to_markdown_v2("_a_ #h >q |p| {b} ~s~ = +"),
            "\\_a\\_ \\#h \\>q \\|p\\| \\{b\\} \\~s\\~ \\= \\+"
        );
    }

    #[test]
    fn splitting() {
        assert_eq!(split_message("short", 4096), vec!["short"]);
        assert!(split_message("", 4096).is_empty());
        let text = format!("{}\n{}", "a".repeat(3000), "b".repeat(3000));
        let parts = split_message(&text, 4096);
        assert_eq!(parts, vec!["a".repeat(3000), "b".repeat(3000)]);
        let long = "x".repeat(10000);
        let parts = split_message(&long, 4096);
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![4096, 4096, 1808]
        );
        // emoji are 2 UTF-16 units; never split inside a char
        let emoji = "😀".repeat(3000);
        let parts = split_message(&emoji, 4096);
        assert!(parts.iter().all(|p| utf16_len(p) <= 4096));
        assert_eq!(parts.concat(), emoji);
    }
}
