//! Telegram side: dispatcher, allowlist filter, chat commands, and running one job end to end.

use crate::profile::{PairStore, Profile, Redeem};
use crate::queue::Queues;
use crate::runner::{self, Outcome, RunOpts, truncate};
use crate::state::{SessionOwners, StateStore};
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
    pub queues: Arc<Queues>,
    pub owners: Arc<SessionOwners>,
}

pub struct Job {
    pub chat_id: i64,
    pub user_id: u64,
    pub prompt: String,
    /// Cron runs: fresh session, output prefixed with ⏰.
    pub scheduled: bool,
    /// "Full Name (@username)" of the sender, for the prompt.
    pub sender: String,
    /// Typing indicator started when the message was accepted; lives until the job ends.
    pub typing: Option<TypingGuard>,
}

/// Shows a chat action ("typing…") now and every 4s until dropped. Each request is capped at
/// 3s and errors are ignored, so a slow or failing request never delays the next one.
pub struct TypingGuard {
    task: tokio::task::JoinHandle<()>,
    action: Arc<std::sync::Mutex<ChatAction>>,
}

impl TypingGuard {
    pub fn start(bot: Bot, chat: ChatId) -> Self {
        Self::start_with(move |a| {
            let bot = bot.clone();
            async move {
                let _ = bot.send_chat_action(chat, a).await;
            }
        })
    }

    pub fn start_with<F, Fut>(send: F) -> Self
    where
        F: Fn(ChatAction) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let action = Arc::new(std::sync::Mutex::new(ChatAction::Typing));
        let a = action.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(4));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await; // first tick fires immediately
                let now = *a.lock().unwrap();
                let _ = tokio::time::timeout(Duration::from_secs(3), send(now)).await;
            }
        });
        Self { task, action }
    }

    /// Switch the action shown from the next tick on (e.g. UploadDocument).
    pub fn set(&self, a: ChatAction) {
        *self.action.lock().unwrap() = a;
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn tg_key(chat: i64) -> String {
    format!("tg:{chat}")
}

/// Put a job on its chat's FIFO queue.
pub fn enqueue(app: &Arc<App>, job: Job) {
    let a = app.clone();
    app.queues.push(
        &tg_key(job.chat_id),
        Box::new(move |stop| Box::pin(async move { run_job(&a, job, stop).await })),
    );
}

/// Telegram API addresses (149.154.160.0/20) tried after the DNS answer, so a broken route
/// or flaky DNS for api.telegram.org isn't fatal. Same seeds as Hermes Agent.
const TELEGRAM_FALLBACK_IPS: [&str; 2] = ["149.154.166.110", "149.154.167.220"];

/// A Bot whose HTTP client resolves api.telegram.org to the system DNS answer (if any, 4s
/// max) followed by the fallback IPs; the connector tries them in order.
pub async fn telegram_bot(token: &str) -> Bot {
    let lookup = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::net::lookup_host("api.telegram.org:443"),
    )
    .await;
    let mut addrs: Vec<std::net::SocketAddr> = match lookup {
        Ok(Ok(found)) => found.collect(),
        _ => vec![],
    };
    for ip in TELEGRAM_FALLBACK_IPS {
        if let Ok(a) = format!("{ip}:443").parse()
            && !addrs.contains(&a)
        {
            addrs.push(a);
        }
    }
    match teloxide::net::default_reqwest_settings()
        .resolve_to_addrs("api.telegram.org", &addrs)
        .build()
    {
        Ok(client) => Bot::with_client(token, client),
        Err(_) => Bot::new(token),
    }
}

/// Why getMe failed: Telegram said no (HTTP 401/404), or we couldn't reach it.
#[derive(Debug, PartialEq)]
pub enum MeError {
    Rejected(String),
    Network(String),
}

impl std::fmt::Display for MeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeError::Rejected(e) => write!(f, "✗ token rejected by Telegram ({e})"),
            MeError::Network(e) => write!(f, "✗ can't reach Telegram (network): {e}"),
        }
    }
}

/// Only 401/404 (teloxide: `InvalidToken`) mean the token is bad; everything else
/// (connect/timeout/IO/5xx/429) is worth retrying.
pub fn is_rejected(e: &teloxide::RequestError) -> bool {
    matches!(
        e,
        teloxide::RequestError::Api(teloxide::ApiError::InvalidToken)
    )
}

/// 1s, 2s, 4s … capped at `cap_secs`.
pub fn me_delay(attempt: u32, cap_secs: u64) -> Duration {
    Duration::from_secs((1u64 << attempt.min(20)).min(cap_secs))
}

/// Run `op` until it succeeds or the error is a rejection; network errors are retried with
/// backoff, `attempts` times in total (None = forever). `on_retry(attempt, error, wait)`.
pub async fn retry_network<T, F, Fut>(
    attempts: Option<u32>,
    cap_secs: u64,
    op: F,
    on_retry: &(dyn Fn(u32, &str, Duration) + Sync),
) -> std::result::Result<T, MeError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, teloxide::RequestError>>,
{
    let mut n = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if is_rejected(&e) => return Err(MeError::Rejected(e.to_string())),
            Err(e) => {
                n += 1;
                if attempts.is_some_and(|a| n >= a) {
                    return Err(MeError::Network(e.to_string()));
                }
                let wait = me_delay(n - 1, cap_secs);
                on_retry(n, &e.to_string(), wait);
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// getMe for CLI flows: 8 attempts, 1s→16s (~1 min), progress on stderr.
pub async fn get_me_cli(token: &str) -> std::result::Result<String, MeError> {
    let bot = telegram_bot(token).await;
    retry_network(Some(8), 16, || bot.get_me().into_future(), &|n, e, wait| {
        eprintln!(
            "  … can't reach Telegram ({e}); retrying in {}s (attempt {n}/8)",
            wait.as_secs()
        )
    })
    .await
    .map(|me| me.username().to_string())
}

/// The Telegram gateway: long polling until the process exits.
pub async fn run(profile: Profile, queues: Arc<Queues>, owners: Arc<SessionOwners>) -> Result<()> {
    let cfg = profile.load_config()?;
    let bot = telegram_bot(&cfg.bot_token).await;
    // Network trouble at startup: keep trying in-process (cap 5 min) instead of exiting into a
    // systemd restart loop. Only a rejected token is fatal.
    let name = profile.name.clone();
    let me = retry_network(None, 300, || bot.get_me().into_future(), &|n, e, wait| {
        warn!(profile = %name, "can't reach Telegram ({e}); retrying getMe in {}s (attempt {n})", wait.as_secs())
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e} → run: cc-gateway profile edit {}", profile.name))?;
    info!(profile = %profile.name, bot = %me.username(), "bot running");
    let app = Arc::new(App {
        bot: bot.clone(),
        state: StateStore::open(profile.state_path()),
        profile,
        queues,
        owners,
    });
    tokio::spawn(schedule::run_loop(app.clone()));

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
        "✗ [{profile}] Telegram 409 Conflict: another process is polling this bot token (retrying \
         with backoff). If it's Hermes Agent, its unit is likely hermes-gateway-{profile}.service \
         → run: systemctl --user disable --now hermes-gateway-{profile}.service (or: hermes -p \
         {profile} gateway stop); or give profile {profile} its own bot → run: cc-gateway profile \
         edit {profile}"
    )
}

/// launchd/systemd stop us with SIGTERM: claude runs live in their own process groups, so
/// kill those first or they'd outlive the daemon.
pub async fn shutdown_on_signal() {
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

/// What `/start` sends to claude for an allowlisted user.
pub const START_PROMPT: &str = "/start (user opened the chat — greet them)";

fn sender_label(user: &teloxide::types::User) -> String {
    match &user.username {
        Some(u) => format!("{} (@{u})", user.full_name()),
        None => user.full_name(),
    }
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
                // A normal turn, so the greeting is in character (SOUL) and can pick up where
                // the last session / imported history left off.
                enqueue(
                    &app,
                    Job {
                        chat_id: chat.0,
                        user_id: uid,
                        prompt: START_PROMPT.into(),
                        scheduled: false,
                        sender: sender_label(user),
                        typing: Some(TypingGuard::start(app.bot.clone(), chat)),
                    },
                );
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
                let text = match app.queues.stop(&tg_key(chat.0)) {
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
                let (queued, running) = app.queues.info(&tg_key(chat.0));
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
                let read = |p: std::path::PathBuf| {
                    let s = std::fs::read_to_string(p).unwrap_or_default();
                    if s.trim().is_empty() {
                        "(empty)".to_string()
                    } else {
                        s.trim().to_string()
                    }
                };
                let (shared, mine) = (&app.profile.dir, app.profile.user_dir(uid));
                let text = format!(
                    "Shared (everyone using this bot)\n\nMEMORY.md\n{}\n\nUSER.md\n{}\n\nAbout you\n\nUSER.md\n{}\n\nMEMORY.md\n{}",
                    read(shared.join("MEMORY.md")),
                    read(shared.join("USER.md")),
                    read(mine.join("USER.md")),
                    read(mine.join("MEMORY.md")),
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
                        .map(|s| s.describe())
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

    // Typing from now on: through media download/transcription, queueing, the run and the reply.
    let typing = TypingGuard::start(app.bot.clone(), chat);
    let prompt = match media::to_prompt(&app.bot, &app.profile, &cfg, &msg, uid).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(()),
        Err(e) => {
            reply(format!("✗ couldn't read that attachment: {e:#}")).await?;
            return Ok(());
        }
    };
    enqueue(
        &app,
        Job {
            chat_id: chat.0,
            user_id: uid,
            prompt,
            scheduled: false,
            sender: sender_label(user),
            typing: Some(typing),
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

/// Whose turn a run is.
pub struct TurnFor<'a> {
    pub chat_id: i64,
    pub user_id: u64,
    pub sender: &'a str,
    pub message: &'a str,
    pub scheduled: bool,
}

pub struct ChatTurn {
    pub opts: RunOpts,
    /// HISTORY.md was injected (mark it done once a session is stored).
    pub with_history: bool,
}

/// The claude run a Telegram message gets right now: prompt (SOUL, memory, notes, history),
/// resume, cwd + skills dir, model. Shared by the bot and `cc-gateway prompt` / `ask`.
pub fn chat_turn(
    profile: &Profile,
    cfg: &crate::profile::Config,
    cs: &crate::state::ChatState,
    t: &TurnFor,
    extra_prompt: &str,
) -> Result<ChatTurn> {
    let (cwd, cwd_args) = profile.claude_cwd(cfg);
    // Imported chat history: only until this chat's first session is stored, then never.
    let history_path = profile.ns_dir(&t.chat_id.to_string()).join("HISTORY.md");
    let history = (!t.scheduled && cs.session_id.is_none() && !cs.history_done)
        .then(|| std::fs::read_to_string(&history_path).ok())
        .flatten()
        .filter(|h| !h.trim().is_empty());
    let with_history = history.is_some();
    let outbox = profile.outbox(t.chat_id);
    std::fs::create_dir_all(&outbox)?;
    let system_prompt = prompt::assemble(
        profile,
        &t.user_id.to_string(),
        prompt::Channel::Telegram {
            sender: t.sender.to_string(),
            outbox,
        },
        &cwd,
        history,
    )? + extra_prompt;
    Ok(ChatTurn {
        opts: RunOpts {
            claude_bin: crate::profile::expand_tilde(&cfg.claude_bin)
                .to_string_lossy()
                .into(),
            prompt: t.message.to_string(),
            system_prompt,
            model: cs.model.clone().unwrap_or(cfg.default_model.clone()),
            max_turns: cfg.max_turns,
            // cron runs get a fresh session so they don't pollute the chat context
            resume: if t.scheduled {
                None
            } else {
                cs.session_id.clone()
            },
            extra_args: [cwd_args, cfg.extra_claude_args.clone()].concat(),
            cwd,
            timeout: Duration::from_secs(cfg.run_timeout_secs),
        },
        with_history,
    })
}

/// The prompt line `ask --dry` adds.
pub const DRY_RUN_PROMPT: &str = "\n\n# Dry run\nThis is a test turn from the operator's terminal \
     (cc-gateway ask --dry). Do not create, edit or delete any files, including the memory files, \
     and don't run commands that change anything; just answer.";

/// Load what `prompt` / `ask` need for a chat. `user` defaults to the chat id (a DM).
fn cli_turn(
    profile: &Profile,
    chat: i64,
    user: Option<u64>,
    message: &str,
    extra: &str,
) -> Result<(crate::profile::Config, StateStore, ChatTurn, u64)> {
    let cfg = profile.load_config()?;
    let uid = match user {
        Some(u) => u,
        None if chat > 0 => chat as u64,
        None => {
            anyhow::bail!("✗ chat {chat} is a group → pass the sender: --user <telegram user id>")
        }
    };
    if !cfg.is_allowed(uid) {
        eprintln!(
            "note: user {uid} is not on {}'s allowlist; the bot would ignore them",
            profile.name
        );
    }
    let state = StateStore::open(profile.state_path());
    let cs = state.get(chat);
    let turn = chat_turn(
        profile,
        &cfg,
        &cs,
        &TurnFor {
            chat_id: chat,
            user_id: uid,
            sender: "the user of this chat (via cc-gateway ask; name unknown)",
            message,
            scheduled: false,
        },
        extra,
    )?;
    Ok((cfg, state, turn, uid))
}

/// `cc-gateway prompt <profile> --chat <id>`: the system prompt that chat gets right now.
pub fn print_prompt(profile: &Profile, chat: i64, user: Option<u64>) -> Result<()> {
    let (_, state, turn, _) = cli_turn(profile, chat, user, "", "")?;
    let cs = state.get(chat);
    eprintln!(
        "# chat {chat}: session {}, model {}, cwd {}{}",
        cs.session_id
            .as_deref()
            .unwrap_or("none (next message starts one)"),
        turn.opts.model,
        turn.opts.cwd.display(),
        if turn.with_history {
            ", HISTORY.md injected"
        } else {
            ""
        }
    );
    print!("{}", turn.opts.system_prompt);
    Ok(())
}

/// `cc-gateway ask`: one headless turn as the Telegram path would run it, reply on stdout.
/// `dry`: forks the session without persisting it, blocks file/shell tools, stores nothing.
pub async fn ask(
    profile: &Profile,
    chat: i64,
    user: Option<u64>,
    message: &str,
    model: Option<String>,
    dry: bool,
) -> Result<()> {
    let extra = if dry { DRY_RUN_PROMPT } else { "" };
    let (cfg, state, turn, _) = cli_turn(profile, chat, user, message, extra)?;
    let mut opts = turn.opts;
    if let Some(m) = model {
        opts.model = m;
    }
    if dry {
        opts.extra_args.extend(dry_args(opts.resume.is_some()));
    }
    runner::set_max_runs(cfg.max_concurrent_runs);
    let epoch = state.get(chat).epoch;
    let (tx, _rx) = mpsc::unbounded_channel();
    match runner::run(&opts, tx, Arc::new(Notify::new())).await {
        Outcome::Done(r) if !r.is_error => {
            println!("{}", r.text);
            if !dry && let Some(sid) = r.session_id {
                SessionOwners::open(profile.sessions_path()).claim(&sid, &format!("tg:{chat}"));
                state.set_session_if(chat, epoch, sid);
                state.update(chat, |c| {
                    c.last_cost_usd = Some(r.cost_usd);
                    c.history_done |= turn.with_history;
                });
            }
            Ok(())
        }
        Outcome::Done(r) if opts.resume.is_some() && r.is_stale_session() => anyhow::bail!(
            "✗ the chat's saved session no longer exists → send /new in the chat, or run without it"
        ),
        Outcome::Done(r) => anyhow::bail!("✗ claude error: {} {}", r.text, r.errors.join("; ")),
        Outcome::TimedOut(_) => anyhow::bail!("✗ timed out after {}s", cfg.run_timeout_secs),
        Outcome::Stopped(_) => anyhow::bail!("✗ stopped"),
        Outcome::Failed(e) => anyhow::bail!("✗ claude failed: {e} → run: cc-gateway doctor"),
    }
}

/// Extra claude args for a dry run: no file/shell tools, nothing persisted, and a resumed
/// session is forked so the real one isn't advanced.
pub fn dry_args(resuming: bool) -> Vec<String> {
    let mut a: Vec<String> = [
        "--disallowedTools",
        "Edit",
        "Write",
        "MultiEdit",
        "NotebookEdit",
        "Bash",
        "--no-session-persistence",
    ]
    .map(String::from)
    .into();
    if resuming {
        a.push("--fork-session".into());
    }
    a
}

/// Run one queued job: status message, claude run, reply, outbox, persist session.
pub async fn run_job(app: &Arc<App>, mut job: Job, stop: Arc<Notify>) {
    // Messages arrive with typing already on; cron jobs get it only while running.
    let typing = job
        .typing
        .take()
        .unwrap_or_else(|| TypingGuard::start(app.bot.clone(), ChatId(job.chat_id)));
    if let Err(e) = try_run_job(app, &job, &typing, stop).await {
        warn!("job failed: {e:#}");
        let _ = app
            .bot
            .send_message(ChatId(job.chat_id), format!("✗ {e:#}"))
            .await;
    }
}

async fn try_run_job(
    app: &Arc<App>,
    job: &Job,
    typing: &TypingGuard,
    stop: Arc<Notify>,
) -> Result<()> {
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
        .send_message(
            chat,
            match runner::slots().queued_ahead() {
                // all global run slots busy: say so instead of looking stuck
                Some(n) => format!("{header}⏳ queued ({n} ahead)"),
                None => format!("{header}⏳ working…"),
            },
        )
        .await
        .ok()
        .map(|m| m.id);
    let started = Instant::now();
    let turn = chat_turn(
        &app.profile,
        &cfg,
        &cs,
        &TurnFor {
            chat_id: job.chat_id,
            user_id: job.user_id,
            sender: &job.sender,
            message: &job.prompt,
            scheduled: job.scheduled,
        },
        "",
    )?;
    let with_history = turn.with_history;
    let mut opts = turn.opts;
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
        let Some(sid) = sid else { return };
        // Mark the session as this chat's so no API key can resume it.
        app.owners.claim(&sid, &format!("tg:{}", chat.0));
        if !job.scheduled {
            app.state.set_session_if(chat.0, cs_epoch, sid);
        }
    };
    let status_text = match outcome {
        Outcome::Done(r) => {
            app.state
                .update(chat.0, |c| c.last_cost_usd = Some(r.cost_usd));
            if with_history && !r.is_error && r.session_id.is_some() {
                app.state.update(chat.0, |c| c.history_done = true);
            }
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
                    format!("✗ claude failed: {err}\n→ run: cc-gateway doctor"),
                )
                .await?;
            format!("{header}✗ failed")
        }
    };
    if let Some(id) = status {
        let _ = app.bot.edit_message_text(chat, id, status_text).await;
    }
    send_outbox(&app.bot, chat, &app.profile.outbox(chat.0), typing).await;
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
    let (tx, mut rx) = mpsc::unbounded_channel::<runner::Event>();
    let ui = async {
        let mut lines: Vec<String> = vec![];
        let mut dirty = false;
        let mut edit = tokio::time::interval(Duration::from_millis(1500));
        loop {
            tokio::select! {
                step = rx.recv() => match step {
                    Some(runner::Event::Tool { name, input }) => {
                        lines.push(runner::Event::tool_line(&name, &input));
                        if lines.len() > 10 { lines.remove(0); }
                        dirty = true;
                    }
                    Some(runner::Event::Init(_)) => dirty = true, // started: drop "queued"
                    Some(_) => {}
                    None => break,
                },
                _ = edit.tick() => if dirty && let Some(id) = status {
                    let text = format!("{header}⏳ working…\n{}", lines.join("\n"));
                    // bounded, so a hung edit on a flaky network can't stall the loop
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        app.bot.edit_message_text(chat, id, text).into_future(),
                    )
                    .await;
                    dirty = false;
                },
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

pub async fn send_long(bot: &Bot, chat: ChatId, text: &str) {
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

async fn send_outbox(bot: &Bot, chat: ChatId, dir: &Path, typing: &TypingGuard) {
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
        typing.set(if is_photo {
            ChatAction::UploadPhoto
        } else {
            ChatAction::UploadDocument
        });
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
    typing.set(ChatAction::Typing);
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

    #[tokio::test(start_paused = true)]
    async fn typing_guard_sends_now_repeats_and_stops_on_drop() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::<ChatAction>::new()));
        let s2 = sent.clone();
        let g = TypingGuard::start_with(move |a| {
            let s = s2.clone();
            async move { s.lock().unwrap().push(a) }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(*sent.lock().unwrap(), vec![ChatAction::Typing]); // immediately
        g.set(ChatAction::UploadDocument);
        tokio::time::sleep(Duration::from_secs(8)).await;
        assert_eq!(sent.lock().unwrap().len(), 3); // 0s, 4s, 8s
        assert_eq!(
            *sent.lock().unwrap().last().unwrap(),
            ChatAction::UploadDocument
        );
        drop(g);
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(sent.lock().unwrap().len(), 3); // stopped

        // a hanging request is cut at 3s and the next one still goes out on time
        let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let n2 = n.clone();
        let _g = TypingGuard::start_with(move |_| {
            let n = n2.clone();
            async move {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<()>().await
            }
        });
        tokio::time::sleep(Duration::from_millis(8010)).await;
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn dry_args_block_writes_and_fork_only_when_resuming() {
        let a = dry_args(false);
        assert!(a.contains(&"--no-session-persistence".to_string()));
        assert!(!a.contains(&"--fork-session".to_string()));
        assert!(
            ["Edit", "Write", "Bash"]
                .iter()
                .all(|t| a.contains(&t.to_string()))
        );
        assert!(dry_args(true).contains(&"--fork-session".to_string()));
    }

    #[test]
    fn chat_turn_injects_history_until_done_and_resumes() {
        let t = tempfile::tempdir().unwrap();
        let cfg = crate::profile::Config::new(String::new());
        let p = Profile::create_at(t.path().join("p"), "t", &cfg).unwrap();
        std::fs::create_dir_all(p.ns_dir("42")).unwrap();
        std::fs::write(
            p.ns_dir("42").join("HISTORY.md"),
            "**User**: about the boat",
        )
        .unwrap();
        let tf = TurnFor {
            chat_id: 42,
            user_id: 42,
            sender: "Ana",
            message: "halo",
            scheduled: false,
        };
        let mut cs = crate::state::ChatState::default();
        let turn = chat_turn(&p, &cfg, &cs, &tf, DRY_RUN_PROMPT).unwrap();
        assert!(turn.with_history && turn.opts.system_prompt.contains("about the boat"));
        assert!(turn.opts.system_prompt.ends_with("just answer."));
        assert_eq!(
            (turn.opts.prompt.as_str(), turn.opts.resume.clone()),
            ("halo", None)
        );
        cs.session_id = Some("s1".into());
        let turn = chat_turn(&p, &cfg, &cs, &tf, "").unwrap();
        assert!(!turn.with_history && turn.opts.resume.as_deref() == Some("s1"));
        cs.session_id = None;
        cs.history_done = true;
        assert!(!chat_turn(&p, &cfg, &cs, &tf, "").unwrap().with_history);
    }

    fn io_err() -> teloxide::RequestError {
        teloxide::RequestError::Io(Arc::new(std::io::Error::other("connection reset")))
    }

    #[test]
    fn only_invalid_token_is_a_rejection() {
        assert!(is_rejected(&teloxide::RequestError::Api(
            teloxide::ApiError::InvalidToken
        )));
        assert!(!is_rejected(&io_err()));
        assert!(!is_rejected(&teloxide::RequestError::RetryAfter(
            teloxide::types::Seconds::from_seconds(1)
        )));
        assert!(!is_rejected(&teloxide::RequestError::Api(
            teloxide::ApiError::Unknown("Bad Gateway".into())
        )));
        let d: Vec<u64> = (0..8).map(|a| me_delay(a, 16).as_secs()).collect();
        assert_eq!(d, vec![1, 2, 4, 8, 16, 16, 16, 16]);
        assert_eq!(me_delay(30, 300).as_secs(), 300);
    }

    #[tokio::test(start_paused = true)]
    async fn network_errors_retry_rejections_dont() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let count = || calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // a blip: fails twice, then works
        let r = retry_network(
            Some(8),
            16,
            || async { if count() < 2 { Err(io_err()) } else { Ok(7) } },
            &|_, _, _| {},
        )
        .await;
        assert_eq!(r, Ok(7));
        // down for good: gives up after 8 attempts as Network
        calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let r: std::result::Result<(), _> = retry_network(
            Some(8),
            16,
            || async {
                count();
                Err(io_err())
            },
            &|_, _, _| {},
        )
        .await;
        assert!(matches!(r, Err(MeError::Network(_))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 8);
        // rejected: no retry
        calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let r: std::result::Result<(), _> = retry_network(
            Some(8),
            16,
            || async {
                count();
                Err(teloxide::RequestError::Api(
                    teloxide::ApiError::InvalidToken,
                ))
            },
            &|_, _, _| {},
        )
        .await;
        assert!(matches!(r, Err(MeError::Rejected(_))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            MeError::Network("x".into())
                .to_string()
                .starts_with("✗ can't reach Telegram (network)")
        );
        assert!(
            MeError::Rejected("x".into())
                .to_string()
                .starts_with("✗ token rejected")
        );
    }

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
