mod api;
mod bot;
mod hermes;
mod media;
mod profile;
mod prompt;
mod queue;
mod runner;
mod schedule;
mod service;
mod setup;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};
use profile::Profile;
use std::io::IsTerminal;

#[derive(Parser)]
#[command(
    name = "cc-gateway",
    version,
    about = "Telegram bot + OpenAI-compatible API in front of Claude Code (claude -p) on your machine"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Guided setup wizard (non-interactive with --token, --api or --import-hermes)
    Setup(setup::SetupArgs),
    /// Print a one-time code that adds another user: they send /pair <code>
    Pair { name: String },
    /// Update to the latest release
    Update,
    /// Remove services and the binary (asks before deleting ~/.cc-gateway)
    Uninstall,
    /// Manage profiles
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Add a Telegram user id to the allowlist
    Allow { name: String, user_id: u64 },
    /// Remove a Telegram user id from the allowlist
    Deny { name: String, user_id: u64 },
    /// Run the profile's enabled gateways (Telegram, API) in the foreground
    Run(Which),
    /// API keys for the HTTP API gateway
    #[command(subcommand)]
    Key(KeyCmd),
    /// Background service (launchd / systemd)
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Check claude, login, tokens, service, ffmpeg, whisper, permissions
    Doctor,
    /// Print the system prompt a chat would get right now (debugging)
    Prompt {
        profile: String,
        #[arg(long)]
        chat: i64,
        /// Sender's Telegram user id (default: the chat id, i.e. a DM)
        #[arg(long)]
        user: Option<u64>,
    },
    /// Run one turn for a chat exactly like Telegram would, print the reply
    Ask {
        profile: String,
        #[arg(long)]
        chat: i64,
        /// Sender's Telegram user id (default: the chat id, i.e. a DM)
        #[arg(long)]
        user: Option<u64>,
        /// No state changes: forked, unsaved session; no file or shell tools
        #[arg(long)]
        dry: bool,
        #[arg(long)]
        model: Option<String>,
        message: String,
    },
    /// Import from another agent
    #[command(subcommand)]
    Import(ImportCmd),
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Import Hermes Agent (token, allowlist, SOUL.md, memories, model, skills)
    Hermes {
        /// cc-gateway profile name for the Hermes root profile (default: "default")
        #[arg(long)]
        profile: Option<String>,
        /// Hermes home (default: $HERMES_HOME or ~/.hermes)
        #[arg(long)]
        hermes_home: Option<std::path::PathBuf>,
        /// No prompts; overwrite existing files. Without a terminal or with --yes, nothing is
        /// imported unless profiles are picked with --only
        #[arg(long)]
        yes: bool,
        /// Hermes profile to import (repeatable; `root` = the top-level ~/.hermes)
        #[arg(long, value_name = "NAME")]
        only: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// Create a profile (token, pairing, model, voice, service)
    Create {
        name: String,
    },
    List,
    /// Open config.toml in $EDITOR
    Edit {
        name: String,
    },
    /// Delete a profile (asks first)
    Delete {
        name: String,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Create a key (printed once; only its sha256 is stored)
    Create {
        profile: String,
        name: String,
    },
    List {
        profile: String,
    },
    Revoke {
        profile: String,
        name: String,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    Install(Which),
    Uninstall(Which),
    Status(Which),
}

/// `<profile>` or `--all` (every profile, one process).
#[derive(clap::Args)]
#[group(required = true, multiple = false)]
struct Which {
    name: Option<String>,
    /// Every profile in one process (shared global run cap)
    #[arg(long)]
    all: bool,
}

impl Which {
    fn target(self) -> Result<service::Target> {
        match self.name {
            _ if self.all => Ok(service::Target::All),
            Some(n) => {
                profile::validate_name(&n)?;
                Ok(service::Target::Profile(n))
            }
            None => anyhow::bail!("give a profile name or --all"),
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("{} {e:#}", console::style("error:").red().bold());
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    profile::migrate_home();
    match cli.cmd {
        None if profile::list().is_empty() => setup::setup(setup::SetupArgs::default()).await,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            Ok(())
        }
        Some(Cmd::Setup(a)) => setup::setup(a).await,
        Some(Cmd::Pair { name }) => setup::pair(&Profile::open(&name)?),
        Some(Cmd::Update) => setup::update(),
        Some(Cmd::Uninstall) => setup::uninstall(),
        Some(Cmd::Profile(p)) => match p {
            ProfileCmd::Create { name } => {
                profile::validate_name(&name)?;
                setup::create_profile(Some(name)).await.map(|_| ())
            }
            ProfileCmd::List => {
                for n in profile::list() {
                    let running = if service::is_running(&service::Target::Profile(n.clone()))
                        || service::is_running(&service::Target::All)
                    {
                        "running"
                    } else {
                        "stopped"
                    };
                    println!(
                        "{n}\t{running}\t{}",
                        profile::profiles_dir().join(&n).display()
                    );
                }
                Ok(())
            }
            ProfileCmd::Edit { name } => setup::edit(&Profile::open(&name)?),
            ProfileCmd::Delete { name } => setup::delete(&Profile::open(&name)?),
        },
        Some(Cmd::Allow { name, user_id }) => {
            let p = Profile::open(&name)?;
            let _lock = p.lock()?;
            let mut cfg = p.load_config()?;
            if !cfg.is_allowed(user_id) {
                cfg.allowed_user_ids.push(user_id);
                p.save_config(&cfg)?;
            }
            println!("✅ {user_id} allowed on {name}");
            Ok(())
        }
        Some(Cmd::Deny { name, user_id }) => {
            let p = Profile::open(&name)?;
            let _lock = p.lock()?;
            let mut cfg = p.load_config()?;
            cfg.allowed_user_ids.retain(|u| *u != user_id);
            p.save_config(&cfg)?;
            println!("✅ {user_id} removed from {name}");
            Ok(())
        }
        Some(Cmd::Run(w)) => match w.target()? {
            service::Target::Profile(name) => {
                let p = Profile::open(&name)?;
                let _guard = init_logging(&p.logs_dir())?;
                let cfg = p.load_config()?;
                runner::set_max_runs(cfg.max_concurrent_runs);
                tokio::spawn(bot::shutdown_on_signal());
                run_gateways(p, 0).await
            }
            service::Target::All => run_all().await,
        },
        Some(Cmd::Key(k)) => setup::key(match k {
            KeyCmd::Create { profile, name } => (profile, Some(name), true),
            KeyCmd::List { profile } => (profile, None, false),
            KeyCmd::Revoke { profile, name } => (profile, Some(name), false),
        }),
        Some(Cmd::Service(s)) => match s {
            ServiceCmd::Install(w) => {
                let t = w.target()?;
                if let service::Target::Profile(n) = &t {
                    Profile::open(n)?;
                }
                service::install(&t)?;
                println!("✅ service installed and started for {}", t.describe());
                Ok(())
            }
            ServiceCmd::Uninstall(w) => {
                let t = w.target()?;
                service::uninstall(&t)?;
                println!("✅ service removed for {}", t.describe());
                Ok(())
            }
            ServiceCmd::Status(w) => {
                let t = w.target()?;
                let d = t.describe();
                let state = match (service::is_installed(&t), service::is_running(&t)) {
                    (false, _) => format!("not installed → run: cc-gateway service install {d}"),
                    (true, true) => "running".into(),
                    (true, false) => "installed but not running → check the logs/ dir".into(),
                };
                println!("{d}: {state}");
                Ok(())
            }
        },
        Some(Cmd::Doctor) => setup::doctor().await,
        Some(Cmd::Prompt {
            profile,
            chat,
            user,
        }) => bot::print_prompt(&Profile::open(&profile)?, chat, user),
        Some(Cmd::Ask {
            profile,
            chat,
            user,
            dry,
            model,
            message,
        }) => bot::ask(&Profile::open(&profile)?, chat, user, &message, model, dry).await,
        Some(Cmd::Import(ImportCmd::Hermes {
            profile,
            hermes_home,
            yes,
            only,
        })) => {
            let home = match hermes_home {
                Some(h) if h.is_dir() => h,
                Some(h) => anyhow::bail!("✗ {} is not a directory", h.display()),
                None => hermes::detect().ok_or_else(|| {
                    anyhow::anyhow!("✗ no Hermes Agent found → pass: --hermes-home <path>")
                })?,
            };
            let interactive = std::io::stdin().is_terminal();
            let done = setup::import_hermes(&home, profile, interactive, yes, None, &only).await?;
            for (p, bot) in &done {
                println!(
                    "\n✅ {} → https://t.me/{bot}  (start: cc-gateway run {0} or service install {0})",
                    p.name
                );
            }
            Ok(())
        }
    }
}

/// Start every enabled gateway of a profile; they share one set of queues.
/// Every profile with a gateway enabled, in this one process: one global run cap (the
/// smallest `max_concurrent_runs`), separate state/queues/locks per profile. A profile that
/// fails (bad token, port in use) is logged and doesn't stop the others.
async fn run_all() -> Result<()> {
    let _guard = init_logging(&profile::home().join("logs"))?;
    let mut profiles = vec![];
    for name in profile::list() {
        let p = match Profile::open(&name) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(profile = %name, "skipped: {e:#}");
                continue;
            }
        };
        match p.load_config() {
            Ok(c) if c.telegram_enabled() || c.api.enabled => profiles.push((p, c)),
            Ok(_) => tracing::info!(profile = %name, "no gateway enabled; skipped"),
            Err(e) => tracing::error!(profile = %name, "{e:#}"),
        }
    }
    // Two profiles polling one token would 409 each other forever: keep the first.
    let mut seen = std::collections::HashMap::new();
    profiles.retain(|(p, c)| {
        if !c.telegram_enabled() {
            return true;
        }
        match seen.insert(c.bot_token.clone(), p.name.clone()) {
            None => true,
            Some(first) => {
                seen.insert(c.bot_token.clone(), first.clone());
                tracing::error!(
                    profile = %p.name,
                    "same Telegram bot token as profile {first}; skipped (would 409) → run: cc-gateway profile edit {}",
                    p.name
                );
                false
            }
        }
    });
    if profiles.is_empty() {
        anyhow::bail!("✗ no profile has a gateway enabled → run: cc-gateway setup");
    }
    let cap = profiles
        .iter()
        .map(|(_, c)| c.max_concurrent_runs)
        .min()
        .unwrap_or(runner::DEFAULT_MAX_RUNS);
    runner::set_max_runs(cap);
    tracing::info!(
        profiles = profiles.len(),
        max_concurrent_runs = cap,
        "running all profiles"
    );
    tokio::spawn(bot::shutdown_on_signal());
    let mut set = tokio::task::JoinSet::new();
    for (p, _) in profiles {
        let span = tracing::info_span!("profile", name = %p.name);
        set.spawn(tracing::Instrument::instrument(
            async move {
                let name = p.name.clone();
                (name, run_gateways(p, ALL_RETRIES).await)
            },
            span,
        ));
    }
    let mut failed = 0;
    let total = set.len();
    while let Some(r) = set.join_next().await {
        match r {
            Ok((name, Err(e))) => {
                failed += 1;
                tracing::error!(profile = %name, "profile stopped: {e:#}");
            }
            Ok((_, Ok(()))) => {}
            Err(e) => {
                failed += 1;
                tracing::error!("a profile task panicked: {e}");
            }
        }
    }
    if failed == total {
        anyhow::bail!(
            "✗ every profile failed or panicked; see the log in {}",
            profile::home().join("logs").display()
        );
    }
    Ok(())
}

/// `run --all`: retries per gateway after a startup failure (e.g. a network blip at getMe):
/// 5s, 10s, 20s … capped at 5 min, about 25 min in total.
const ALL_RETRIES: u32 = 10;

fn retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs((5u64 << attempt.min(6)).min(300))
}

/// Run `start` until it succeeds, retrying failures up to `retries` times with backoff.
async fn with_retry<F, Fut>(profile: &str, what: &str, retries: u32, start: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut attempt = 0;
    loop {
        match start().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::error!(profile = %profile, "{what} gateway: {e:#}");
                if attempt >= retries {
                    return Err(e);
                }
                let wait = retry_delay(attempt);
                attempt += 1;
                tracing::warn!(
                    profile = %profile,
                    "{what} gateway: retrying in {}s (attempt {attempt}/{retries})",
                    wait.as_secs()
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Run one profile's enabled gateways. A failing gateway is logged right away (and retried
/// `retries` times); the other keeps running.
async fn run_gateways(p: Profile, retries: u32) -> Result<()> {
    let cfg = p.load_config()?;
    let (tg, api) = (cfg.telegram_enabled(), cfg.api.enabled);
    if !tg && !api {
        anyhow::bail!(
            "✗ profile {0} has no gateway enabled (no bot_token, [api] enabled = false) → run: cc-gateway profile edit {0}",
            p.name
        );
    }
    let queues = std::sync::Arc::new(queue::Queues::default());
    let owners = std::sync::Arc::new(state::SessionOwners::open(p.sessions_path()));
    let telegram = async {
        if !tg {
            return Ok(());
        }
        with_retry(&p.name, "telegram", retries, || {
            bot::run(p.clone(), queues.clone(), owners.clone())
        })
        .await
    };
    let http = async {
        if !api {
            return Ok(());
        }
        with_retry(&p.name, "api", retries, || {
            api::serve(p.clone(), queues.clone(), owners.clone())
        })
        .await
    };
    let (a, b) = tokio::join!(telegram, http);
    a.and(b)
}

/// Log to `<dir>/bot.log` (and stdout when attached to a terminal).
fn init_logging(dir: &std::path::Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    std::fs::create_dir_all(dir)?;
    let (file, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, "bot.log"));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    if std::io::stdout().is_terminal() {
        builder.with_writer(file.and(std::io::stdout)).init();
    } else {
        builder.with_writer(file).init(); // under launchd/systemd: bot.log only
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_capped() {
        let d: Vec<u64> = (0..ALL_RETRIES).map(|a| retry_delay(a).as_secs()).collect();
        assert_eq!(d, vec![5, 10, 20, 40, 80, 160, 300, 300, 300, 300]);
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_retries_then_gives_up_or_succeeds() {
        let n = std::sync::atomic::AtomicU32::new(0);
        let r = with_retry("p", "x", 2, || async {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("down")
        })
        .await;
        assert!(r.is_err());
        assert_eq!(n.load(std::sync::atomic::Ordering::SeqCst), 3); // first try + 2 retries
        let m = std::sync::atomic::AtomicU32::new(0);
        let r = with_retry("p", "x", 5, || async {
            if m.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                anyhow::bail!("blip")
            }
            Ok(())
        })
        .await;
        assert!(r.is_ok());
        assert_eq!(m.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
