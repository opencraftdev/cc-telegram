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
    name = "cc-telegram",
    version,
    about = "Telegram bot that forwards messages to Claude Code (claude -p) on your machine"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Guided setup wizard (or non-interactive with --token)
    Setup(setup::SetupArgs),
    /// Print a one-time code that adds another user: they send /pair <code>
    Pair { name: String },
    /// Update to the latest release
    Update,
    /// Remove services and the binary (asks before deleting ~/.cc-telegram)
    Uninstall,
    /// Manage profiles
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Add a Telegram user id to the allowlist
    Allow { name: String, user_id: u64 },
    /// Remove a Telegram user id from the allowlist
    Deny { name: String, user_id: u64 },
    /// Run the bot in the foreground
    Run { name: String },
    /// Background service (launchd / systemd)
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Check claude, login, tokens, service, ffmpeg, whisper, permissions
    Doctor,
    /// Import from another agent
    #[command(subcommand)]
    Import(ImportCmd),
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Import Hermes Agent (token, allowlist, SOUL.md, memories, model, skills)
    Hermes {
        /// cc-telegram profile name for the Hermes root profile (default: "default")
        #[arg(long)]
        profile: Option<String>,
        /// Hermes home (default: $HERMES_HOME or ~/.hermes)
        #[arg(long)]
        hermes_home: Option<std::path::PathBuf>,
        /// No prompts: import everything, overwrite existing files
        #[arg(long)]
        yes: bool,
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
enum ServiceCmd {
    Install { name: String },
    Uninstall { name: String },
    Status { name: String },
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
            ProfileCmd::Create { name } => setup::create_profile(Some(name)).await.map(|_| ()),
            ProfileCmd::List => {
                for n in profile::list() {
                    let running = if service::is_running(&n) {
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
        Some(Cmd::Run { name }) => {
            let p = Profile::open(&name)?;
            let _guard = init_logging(&p)?;
            bot::run(p).await
        }
        Some(Cmd::Service(s)) => match s {
            ServiceCmd::Install { name } => {
                service::install(&Profile::open(&name)?)?;
                println!("✅ service installed and started for {name}");
                Ok(())
            }
            ServiceCmd::Uninstall { name } => {
                profile::validate_name(&name)?;
                service::uninstall(&name)?;
                println!("✅ service removed for {name}");
                Ok(())
            }
            ServiceCmd::Status { name } => {
                profile::validate_name(&name)?;
                let state = match (service::is_installed(&name), service::is_running(&name)) {
                    (false, _) => {
                        format!("not installed → run: cc-telegram service install {name}")
                    }
                    (true, true) => "running".into(),
                    (true, false) => {
                        "installed but not running → check logs in the profile's logs/ dir".into()
                    }
                };
                println!("{name}: {state}");
                Ok(())
            }
        },
        Some(Cmd::Doctor) => setup::doctor().await,
        Some(Cmd::Import(ImportCmd::Hermes {
            profile,
            hermes_home,
            yes,
        })) => {
            let home = match hermes_home {
                Some(h) if h.is_dir() => h,
                Some(h) => anyhow::bail!("✗ {} is not a directory", h.display()),
                None => hermes::detect().ok_or_else(|| {
                    anyhow::anyhow!("✗ no Hermes Agent found → pass: --hermes-home <path>")
                })?,
            };
            let interactive = std::io::stdin().is_terminal();
            let done = setup::import_hermes(&home, profile, interactive, yes, None).await?;
            for (p, bot) in &done {
                println!(
                    "\n✅ {} → https://t.me/{bot}  (start: cc-telegram run {0} or service install {0})",
                    p.name
                );
            }
            Ok(())
        }
    }
}

/// Log to logs/bot.log (and stdout when attached to a terminal).
fn init_logging(p: &Profile) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    std::fs::create_dir_all(p.logs_dir())?;
    let (file, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(p.logs_dir(), "bot.log"));
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
