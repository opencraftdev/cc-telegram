//! Setup wizard, doctor, pairing, update/uninstall and other interactive CLI flows.

use crate::media::which;
use crate::profile::{self, Config, PairStore, Profile, expand_tilde};
use crate::{hermes, service};
use anyhow::{Context, Result, bail};
use console::style;
use dialoguer::{Confirm, Input, Select};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::UpdateKind;

const INSTALL_SH: &str =
    "https://raw.githubusercontent.com/opencraftdev/cc-telegram/main/install.sh";
const WHISPER_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";
const MODELS: [&str; 3] = ["sonnet", "opus", "haiku"];

#[derive(clap::Args, Default)]
pub struct SetupArgs {
    /// Bot token from @BotFather (enables non-interactive mode)
    #[arg(long)]
    token: Option<String>,
    /// Profile name
    #[arg(long)]
    profile: Option<String>,
    /// sonnet | opus | haiku | full model id
    #[arg(long)]
    model: Option<String>,
    /// Install and start the background service
    #[arg(long)]
    service: bool,
    /// Don't enable voice notes
    #[arg(long)]
    no_voice: bool,
    /// Import a detected Hermes Agent install (non-interactive)
    #[arg(long, conflicts_with = "no_import_hermes")]
    import_hermes: bool,
    /// Don't offer to import Hermes Agent
    #[arg(long)]
    no_import_hermes: bool,
}

fn ok(msg: impl std::fmt::Display) {
    println!("{} {msg}", style("✅").green());
}
fn bad(msg: impl std::fmt::Display) {
    println!("{} {msg}", style("✗").red().bold());
}
fn step(n: u32, title: &str) {
    println!(
        "\n{} {}",
        style(format!("[{n}/7]")).dim(),
        style(title).bold()
    );
}

// ---------- checks shared by setup + doctor ----------

fn claude_version(bin: &str) -> Option<String> {
    let out = Command::new(expand_tilde(bin))
        .arg("--version")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `claude auth status` prints JSON with `loggedIn`. None = couldn't tell.
fn claude_logged_in(bin: &str) -> Option<bool> {
    let out = Command::new(expand_tilde(bin))
        .args(["auth", "status"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v["loggedIn"].as_bool()
}

async fn get_me(token: &str) -> Result<String> {
    let me = Bot::new(token).get_me().await?;
    Ok(me.username().to_string())
}

fn voice_deps(cfg: &Config) -> (bool, bool, bool) {
    (
        which("ffmpeg").is_some(),
        which(&cfg.voice.whisper_bin).is_some(),
        expand_tilde(&cfg.voice.whisper_model).is_file(),
    )
}

// ---------- setup ----------

pub async fn setup(a: SetupArgs) -> Result<()> {
    if a.token.is_some() || a.import_hermes {
        return non_interactive(a).await;
    }
    println!("{}", style("cc-telegram setup").bold().cyan());
    println!("Chat with Claude Code on this machine from Telegram.\n");
    step(1, "Claude Code");
    check_claude_interactive()?;
    if !a.no_import_hermes
        && let Some(home) = hermes::detect()
    {
        let imported = import_hermes(&home, a.profile.clone(), true, false, None).await?;
        if !imported.is_empty() {
            for (p, bot_name) in imported {
                let cfg = p.load_config()?;
                println!(
                    "\n{}",
                    style(format!("Finishing profile {}", p.name)).bold()
                );
                finish_profile(&p, cfg, &bot_name).await?;
            }
            return Ok(());
        }
    }
    create_profile(a.profile).await?;
    Ok(())
}

fn check_claude_interactive() -> Result<()> {
    if claude_version("claude").is_none() {
        bad("claude not found → run: npm i -g @anthropic-ai/claude-code");
        if which("npm").is_some()
            && Confirm::new()
                .with_prompt("Run it now?")
                .default(true)
                .interact()?
        {
            Command::new("npm")
                .args(["i", "-g", "@anthropic-ai/claude-code"])
                .status()?;
        }
        if claude_version("claude").is_none() {
            bail!(
                "✗ claude still not found → run: npm i -g @anthropic-ai/claude-code, then: cc-telegram setup"
            );
        }
    }
    ok(format!(
        "Claude Code {}",
        claude_version("claude").unwrap_or_default()
    ));
    if claude_logged_in("claude") == Some(false) {
        bad("claude not logged in → run: claude   (log in once, then come back here)");
        Input::<String>::new()
            .with_prompt("Press Enter when you're logged in")
            .allow_empty(true)
            .interact_text()?;
        match claude_logged_in("claude") {
            Some(false) => bad("still not logged in; the bot will fail until you run: claude"),
            _ => ok("logged in"),
        }
    }
    Ok(())
}

/// Ask for a token until Telegram accepts it: (token, bot username).
async fn ask_token() -> Result<(String, String)> {
    println!("  1. Open Telegram and message @BotFather");
    println!("  2. Send /newbot and follow the prompts");
    println!("  3. Copy the token it gives you and paste it here");
    let (token, bot_name) = loop {
        let t: String = Input::new().with_prompt("Bot token").interact_text()?;
        let t = t.trim().to_string();
        match get_me(&t).await {
            Ok(u) => break (t, u),
            Err(e) => bad(format!(
                "token rejected by Telegram ({e}) → paste the token from @BotFather again"
            )),
        }
    };
    ok(format!("Connected to @{bot_name}"));
    Ok((token, bot_name))
}

/// Wizard steps 2-8 (also `profile create`).
pub async fn create_profile(name: Option<String>) -> Result<Profile> {
    step(2, "Bot token");
    let (token, bot_name) = ask_token().await?;

    step(3, "Profile name");
    let name = match name {
        Some(n) => {
            profile::validate_name(&n)?;
            if profile::list().contains(&n) {
                bail!("✗ profile {n:?} already exists → run: cc-telegram profile delete {n}");
            }
            println!("  {n}");
            n
        }
        None => loop {
            let n: String = Input::new()
                .with_prompt("Profile name")
                .default("default".into())
                .interact_text()?;
            match profile::validate_name(&n) {
                Err(e) => bad(e),
                Ok(()) if profile::list().contains(&n) => {
                    bad(format!("{n:?} already exists, pick another name"))
                }
                Ok(()) => break n,
            }
        },
    };
    let cfg = Config::new(token);
    let p = Profile::create(&name, &cfg)?;
    finish_profile(&p, cfg, &bot_name).await?;
    Ok(p)
}

/// Wizard steps 4-8 for a created profile (pairing is skipped if someone is allowlisted).
async fn finish_profile(p: &Profile, mut cfg: Config, bot_name: &str) -> Result<()> {
    let name = &p.name;
    step(4, "Pair yourself");
    if cfg.allowed_user_ids.is_empty() {
        println!(
            "  Open {} and press Start (waiting 2 minutes)…",
            style(format!("https://t.me/{bot_name}")).underlined()
        );
        if !pair_window(&Bot::new(&cfg.bot_token), &mut cfg, p).await? {
            bad("no /start confirmed in time; pair later with the code below");
            print_pair_code(p)?;
        }
    } else {
        ok(format!(
            "{} user(s) already allowed",
            cfg.allowed_user_ids.len()
        ));
    }

    step(5, "Model");
    let i = Select::new()
        .with_prompt("Default model")
        .items(MODELS)
        .default(
            MODELS
                .iter()
                .position(|m| *m == cfg.default_model)
                .unwrap_or(0),
        )
        .interact()?;
    cfg.default_model = MODELS[i].into();
    p.save_config(&cfg)?;

    step(6, "Voice notes (optional)");
    voice_step(&mut cfg).await?;
    p.save_config(&cfg)?;

    step(7, "Run in background");
    let service = Confirm::new()
        .with_prompt("Install as a background service (starts on login)?")
        .default(true)
        .interact()?;
    if service {
        match service::install(p) {
            Ok(()) => ok("service installed and running"),
            Err(e) => bad(format!("{e:#}")),
        }
    } else {
        println!("  Start it with: cc-telegram run {name}");
    }

    println!("\n{}", style("🎉 All set!").bold().green());
    println!("  Bot:     https://t.me/{bot_name}");
    println!("  Profile: {}", p.dir.display());
    println!("  Useful commands:");
    println!("    cc-telegram pair {name}             add a friend");
    println!("    cc-telegram service status {name}   is it running?");
    println!("    cc-telegram profile edit {name}     model, limits, allowlist");
    println!("    cc-telegram doctor                  check everything");
    Ok(())
}

/// Accept the first /start within 2 minutes, after the user confirms it in the terminal.
async fn pair_window(bot: &Bot, cfg: &mut Config, p: &Profile) -> Result<bool> {
    // Skip anything sent before the window opened.
    let old = bot.get_updates().offset(-1).timeout(0).await?;
    let mut offset = old.last().map_or(0, |u| u.id.0 as i32 + 1);
    let deadline = Instant::now() + Duration::from_secs(120);
    let spinner = ProgressBar::new_spinner().with_message("waiting for /start");
    spinner.enable_steady_tick(Duration::from_millis(120));
    let mut paired = false;
    'outer: while Instant::now() < deadline {
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .clamp(1, 10);
        let updates = match bot.get_updates().offset(offset).timeout(wait as u32).await {
            Ok(u) => u,
            Err(teloxide::RequestError::Api(teloxide::ApiError::TerminatedByOtherGetUpdates)) => {
                spinner.suspend(|| println!("{}", crate::bot::conflict_hint(&p.name)));
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => {
                spinner.suspend(|| bad(format!("Telegram error: {e}")));
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        for u in updates {
            offset = u.id.0 as i32 + 1;
            let UpdateKind::Message(m) = u.kind else {
                continue;
            };
            let Some(user) = m.from.as_ref() else {
                continue;
            };
            if !m.text().is_some_and(|t| t.starts_with("/start")) {
                continue;
            }
            let who = format!(
                "@{} ({}, id {})",
                user.username.as_deref().unwrap_or("no-username"),
                user.full_name(),
                user.id.0
            );
            let yes = spinner.suspend(|| {
                Confirm::new()
                    .with_prompt(format!("Is this you? {who}"))
                    .default(false) // explicit y: Enter-through must not allowlist a stranger
                    .interact()
            })?;
            if yes {
                cfg.allowed_user_ids.push(user.id.0);
                p.save_config(cfg)?;
                p.claim_imported(user.id.0)?;
                bot.send_message(m.chat.id, "Paired ✅").await?;
                paired = true;
                break 'outer;
            }
        }
    }
    spinner.finish_and_clear();
    let _ = bot.get_updates().offset(offset).timeout(0).await; // mark consumed
    if paired {
        ok("paired");
    }
    Ok(paired)
}

async fn voice_step(cfg: &mut Config) -> Result<()> {
    let (ff, wh, model) = voice_deps(cfg);
    if ff && wh && model {
        cfg.voice.enabled = true;
        ok("ffmpeg + whisper-cli found, voice notes enabled");
        return Ok(());
    }
    let want = Confirm::new()
        .with_prompt("Enable voice notes? Runs `brew install ffmpeg whisper-cpp` + downloads base model (~150MB)")
        .default(false)
        .interact()?;
    if !want {
        cfg.voice.enabled = false;
        return Ok(());
    }
    if !(ff && wh) {
        if cfg!(target_os = "macos") && which("brew").is_some() {
            Command::new("brew")
                .args(["install", "ffmpeg", "whisper-cpp"])
                .status()?;
        } else {
            bad(
                "install ffmpeg and whisper.cpp (whisper-cli) with your package manager, then run: cc-telegram doctor",
            );
        }
    }
    if !model {
        download_model(&expand_tilde(&cfg.voice.whisper_model)).await?;
    }
    let (ff, wh, model) = voice_deps(cfg);
    cfg.voice.enabled = ff && wh && model;
    if cfg.voice.enabled {
        ok("voice notes enabled");
    } else {
        bad("voice notes still missing deps; disabled → run: cc-telegram doctor");
    }
    Ok(())
}

async fn download_model(dest: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dest.parent().context("bad model path")?)?;
    let mut resp = reqwest::get(WHISPER_MODEL_URL).await?.error_for_status()?;
    let pb = ProgressBar::new(resp.content_length().unwrap_or(0)).with_style(
        ProgressStyle::with_template("  {bar:40.cyan/blue} {bytes}/{total_bytes} ({eta})")?,
    );
    let part = dest.with_extension("part");
    let mut f = std::fs::File::create(&part)?;
    while let Some(chunk) = resp.chunk().await? {
        f.write_all(&chunk)?;
        pb.inc(chunk.len() as u64);
    }
    pb.finish_and_clear();
    std::fs::rename(&part, dest)?;
    ok(format!("model saved to {}", dest.display()));
    Ok(())
}

async fn non_interactive(a: SetupArgs) -> Result<()> {
    match claude_version("claude") {
        None => bad("claude not found → run: npm i -g @anthropic-ai/claude-code"),
        Some(v) => ok(format!("Claude Code {v}")),
    }
    if claude_logged_in("claude") == Some(false) {
        bad("claude not logged in → run: claude");
    }
    if a.import_hermes {
        let Some(home) = hermes::detect() else {
            bail!("✗ no Hermes Agent found (looked at $HERMES_HOME and ~/.hermes)");
        };
        let imported =
            import_hermes(&home, a.profile.clone(), false, false, a.token.clone()).await?;
        if imported.is_empty() {
            bail!("✗ nothing imported → run: cc-telegram setup");
        }
        for (p, bot_name) in imported {
            let mut cfg = p.load_config()?;
            if a.no_voice {
                cfg.voice.enabled = false;
                p.save_config(&cfg)?;
            }
            if a.service {
                service::install(&p)?;
                ok(format!("service installed and running for {}", p.name));
            } else {
                println!("Start it with: cc-telegram run {}", p.name);
            }
            if cfg.allowed_user_ids.is_empty() {
                print_pair_code(&p)?;
            }
            println!("Bot: https://t.me/{bot_name}");
        }
        return Ok(());
    }
    let token = a.token.unwrap_or_default();
    let bot_name = get_me(&token).await.map_err(|e| {
        anyhow::anyhow!("✗ token rejected by Telegram ({e}) → get a valid token from @BotFather")
    })?;
    ok(format!("Connected to @{bot_name}"));
    let name = a.profile.unwrap_or_else(|| "default".into());
    profile::validate_name(&name)?;
    if profile::list().contains(&name) {
        bail!("✗ profile {name:?} already exists → run: cc-telegram profile delete {name}");
    }
    let mut cfg = Config::new(token);
    if let Some(m) = a.model {
        cfg.default_model = m;
    }
    let (ff, wh, model) = voice_deps(&cfg);
    cfg.voice.enabled = !a.no_voice && ff && wh && model;
    let p = Profile::create(&name, &cfg)?;
    ok(format!("profile {name} created at {}", p.dir.display()));
    if a.service {
        service::install(&p)?;
        ok("service installed and running");
    } else {
        println!("Start it with: cc-telegram run {name}");
    }
    print_pair_code(&p)?;
    println!("Bot: https://t.me/{bot_name}");
    Ok(())
}

// ---------- Hermes import ----------

/// Import Hermes sources into cc-telegram profiles. Returns (profile, bot username) for each
/// profile created or updated. `interactive` = prompts; `yes` = accept all + overwrite.
pub async fn import_hermes(
    home: &std::path::Path,
    root_name: Option<String>,
    interactive: bool,
    yes: bool,
    token_override: Option<String>,
) -> Result<Vec<(Profile, String)>> {
    let srcs = hermes::sources(home);
    if srcs.is_empty() {
        bad(format!("nothing importable in {}", home.display()));
        return Ok(vec![]);
    }
    let found: Vec<_> = srcs.iter().map(|s| hermes::scan(&s.dir)).collect();
    println!("\n{}", style("Hermes Agent").bold());
    for (s, f) in srcs.iter().zip(&found) {
        println!(
            "  {}: {}",
            s.name.as_deref().unwrap_or("(root)"),
            f.summary()
        );
    }
    let ask = interactive && !yes;
    let mut chosen: Vec<usize> = (0..srcs.len()).collect();
    if ask {
        let go = Confirm::new()
            .with_prompt(format!(
                "Hermes Agent found at {}. Import it?",
                home.display()
            ))
            .default(true)
            .interact()?;
        if !go {
            return Ok(vec![]);
        }
        if srcs.len() > 1 {
            let labels: Vec<_> = srcs
                .iter()
                .map(|s| s.name.clone().unwrap_or_else(|| "(root)".into()))
                .collect();
            chosen = dialoguer::MultiSelect::new()
                .with_prompt("Which Hermes profiles? (space toggles, Enter confirms)")
                .items(&labels)
                .defaults(&vec![true; labels.len()])
                .interact()?;
        }
    }

    let mut out = vec![];
    for i in chosen {
        let (src, f) = (&srcs[i], &found[i]);
        let name = match &src.name {
            Some(n) => n.clone(),
            None if ask => Input::new()
                .with_prompt("cc-telegram profile name for the Hermes root profile")
                .default(root_name.clone().unwrap_or_else(|| "default".into()))
                .interact_text()?,
            None => root_name.clone().unwrap_or_else(|| "default".into()),
        };
        println!("\n{}", style(format!("→ profile {name}")).bold());
        if let Err(e) = profile::validate_name(&name) {
            bad(format!("skipped: {e}"));
            continue;
        }
        let existing = profile::list().contains(&name);
        let mut rows: Vec<hermes::Row> = vec![];

        // Token: override (root) > Hermes .env > existing profile > ask.
        let candidate = match (&src.name, &token_override) {
            (None, Some(t)) => Some(t.clone()),
            _ => f.token.clone(),
        };
        let from_hermes = candidate.is_some() && candidate == f.token;
        let mut token = None;
        if let Some(t) = candidate {
            match get_me(&t).await {
                Ok(u) => token = Some((t, u)),
                Err(e) => bad(format!("imported bot token rejected by Telegram ({e})")),
            }
        }
        let p = if existing {
            Some(Profile::open(&name)?)
        } else {
            None
        };
        let current = p.as_ref().map(|p| p.load_config()).transpose()?;
        if token.is_none()
            && let Some(c) = &current
            && let Ok(u) = get_me(&c.bot_token).await
        {
            token = Some((c.bot_token.clone(), u));
        }
        if token.is_none() {
            if ask {
                token = Some(ask_token().await?);
            } else {
                bad(format!(
                    "skipped {name}: no valid bot token → run: cc-telegram setup --token <TOKEN> --import-hermes"
                ));
                continue;
            }
        }
        let (token, bot_name) = token.unwrap();
        if from_hermes {
            println!(
                "{} this bot token is also Hermes's. If its gateway is running, both poll Telegram → 409 Conflict.\n   Stop it → run: hermes gateway stop{}  (or create a new bot with @BotFather)",
                style("⚠").yellow().bold(),
                src.name
                    .as_ref()
                    .map(|n| format!("  (profile: hermes -p {n} gateway stop)"))
                    .unwrap_or_default()
            );
        }

        // config.toml
        let mut cfg = current
            .clone()
            .unwrap_or_else(|| Config::new(token.clone()));
        if cfg.bot_token != token {
            let replace = !existing
                || yes
                || (ask
                    && Confirm::new()
                        .with_prompt(format!(
                            "Replace profile {name}'s bot token with the Hermes one?"
                        ))
                        .default(false)
                        .interact()?);
            if replace {
                cfg.bot_token = token.clone();
                rows.push(("bot token".into(), "imported".into()));
            } else {
                rows.push(("bot token".into(), "kept existing".into()));
            }
        } else {
            rows.push(("bot token".into(), "set".into()));
        }
        let before = cfg.allowed_user_ids.len();
        for id in &f.allowed {
            if !cfg.is_allowed(*id) {
                cfg.allowed_user_ids.push(*id);
            }
        }
        rows.push((
            "allowed users".into(),
            format!("{} added", cfg.allowed_user_ids.len() - before),
        ));
        rows.push((
            "model".into(),
            match f.model.as_deref().map(|m| (m, hermes::map_model(m))) {
                Some((_, Some(c))) if !existing => {
                    cfg.default_model = c.into();
                    c.to_string()
                }
                Some((_, Some(_))) => "kept existing".into(),
                Some((m, None)) => {
                    format!("kept {} ({m} is not a Claude model)", cfg.default_model)
                }
                None => "none in Hermes".into(),
            },
        ));
        let p = match p {
            Some(p) => {
                let _lock = p.lock()?;
                p.save_config(&cfg)?;
                p
            }
            None => Profile::create(&name, &cfg)?,
        };

        // Hermes memory has one owner.
        let owner = match f.allowed.as_slice() {
            [] => None,
            [one] => Some(*one),
            many if ask => {
                let labels: Vec<_> = many.iter().map(u64::to_string).collect();
                let i = Select::new()
                    .with_prompt("Hermes memory belongs to one user. Which Telegram id is it?")
                    .items(&labels)
                    .default(0)
                    .interact()?;
                Some(many[i])
            }
            many => {
                println!(
                    "  note: several allowed users; Hermes memory assigned to the first ({})",
                    many[0]
                );
                Some(many[0])
            }
        };
        let mut confirm = |dst: &std::path::Path| {
            yes || (ask
                && Confirm::new()
                    .with_prompt(format!("Overwrite {}?", dst.display()))
                    .default(false)
                    .interact()
                    .unwrap_or(false))
        };
        rows.extend(hermes::import_files(f, &p, owner, &mut confirm));
        if owner.is_none() && (f.memory.is_some() || f.user.is_some()) {
            rows.push((
                "memory owner".into(),
                "assigned on first successful pair".into(),
            ));
        }
        for (item, result) in &rows {
            println!("  {item:<24} {result}");
        }
        out.push((p, bot_name));
    }
    Ok(out)
}

// ---------- pair / profile helpers ----------

fn print_pair_code(p: &Profile) -> Result<()> {
    let lock = p.lock()?;
    let mut store = PairStore::load(&p.pair_path());
    let code = store.issue(chrono::Utc::now().timestamp())?;
    store.save(&p.pair_path())?;
    drop(lock);
    println!(
        "Send this to your bot within 10 min:  {}",
        style(format!("/pair {code}")).bold()
    );
    println!(
        "(the bot must be running: cc-telegram run {0}  or  cc-telegram service install {0})",
        p.name
    );
    Ok(())
}

pub fn pair(p: &Profile) -> Result<()> {
    print_pair_code(p)
}

pub fn edit(p: &Profile) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let bin = parts.next().unwrap_or("vi");
    Command::new(bin)
        .args(parts)
        .arg(p.config_path())
        .status()
        .with_context(|| format!("✗ could not start editor {bin:?} → set $EDITOR"))?;
    std::fs::set_permissions(p.config_path(), std::fs::Permissions::from_mode(0o600))?;
    match p.load_config() {
        Ok(_) => ok("config saved (restart the bot to apply token/model changes)"),
        Err(e) => bad(format!("{e:#}")),
    }
    Ok(())
}

pub fn delete(p: &Profile) -> Result<()> {
    let sure = Confirm::new()
        .with_prompt(format!(
            "Delete profile {} and everything in {} (memory, sessions, workspace)?",
            p.name,
            p.dir.display()
        ))
        .default(false)
        .interact()?;
    if sure {
        service::uninstall(&p.name)?;
        std::fs::remove_dir_all(&p.dir)?;
        ok(format!("deleted {}", p.name));
    }
    Ok(())
}

pub fn update() -> Result<()> {
    // ponytail: re-runs the installer instead of axoupdater; same result, no extra dependency.
    println!("Running: curl -fsSL {INSTALL_SH} | sh");
    let st = Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsSL {INSTALL_SH} | sh"))
        .status()?;
    if !st.success() {
        bail!("✗ update failed → run: curl -fsSL {INSTALL_SH} | sh");
    }
    for n in profile::list()
        .into_iter()
        .filter(|n| service::is_installed(n))
    {
        println!("Restart {n} to use the new version: cc-telegram service install {n}");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    for n in profile::list() {
        service::uninstall(&n)?;
        ok(format!("service removed: {n}"));
    }
    let home = profile::home();
    if home.exists()
        && Confirm::new()
            .with_prompt(format!(
                "Also delete {} (all profiles, memory, sessions)?",
                home.display()
            ))
            .default(false)
            .interact()?
    {
        std::fs::remove_dir_all(&home)?;
        ok(format!("deleted {}", home.display()));
    }
    let exe = std::env::current_exe()?;
    if exe.to_string_lossy().contains("/Cellar/") {
        println!("Installed with Homebrew → run: brew uninstall cc-telegram");
    } else {
        std::fs::remove_file(&exe)?;
        ok(format!("removed {}", exe.display()));
    }
    Ok(())
}

// ---------- doctor ----------

pub async fn doctor() -> Result<()> {
    let mut failures = 0;
    let mut check = |pass: bool, what: String, fix: &str| {
        if pass {
            ok(what);
        } else {
            failures += 1;
            bad(format!("{what} → {fix}"));
        }
    };
    let ver = claude_version("claude");
    check(
        ver.is_some(),
        format!("claude installed {}", ver.unwrap_or_default()),
        "run: npm i -g @anthropic-ai/claude-code",
    );
    check(
        claude_logged_in("claude") != Some(false),
        "claude logged in".into(),
        "run: claude",
    );
    let home = profile::home();
    let writable = std::fs::create_dir_all(&home).is_ok() && tempfile_probe(&home);
    check(
        writable,
        format!("{} writable", home.display()),
        "check the directory's owner/permissions",
    );

    let profiles = profile::list();
    check(
        !profiles.is_empty(),
        "at least one profile".into(),
        "run: cc-telegram setup",
    );
    let mut voice_wanted = false;
    for name in &profiles {
        println!("\n{}", style(format!("profile {name}")).bold());
        let p = Profile::open(name)?;
        let cfg = match p.load_config() {
            Ok(c) => c,
            Err(e) => {
                check(
                    false,
                    format!("{e:#}"),
                    &format!("run: cc-telegram profile edit {name}"),
                );
                continue;
            }
        };
        let mode = std::fs::metadata(p.config_path())?.permissions().mode() & 0o777;
        check(
            mode == 0o600,
            format!("config.toml mode {mode:o}"),
            &format!("run: chmod 600 {}", p.config_path().display()),
        );
        let me = get_me(&cfg.bot_token).await;
        check(
            me.is_ok(),
            match &me {
                Ok(u) => format!("token valid (@{u})"),
                Err(e) => format!("token rejected by Telegram ({e})"),
            },
            &format!("run: cc-telegram profile edit {name}"),
        );
        check(
            !cfg.allowed_user_ids.is_empty(),
            format!("allowlist has {} user(s)", cfg.allowed_user_ids.len()),
            &format!("run: cc-telegram pair {name}"),
        );
        if cfg.claude_bin != "claude" {
            check(
                claude_version(&cfg.claude_bin).is_some(),
                format!("claude_bin {}", cfg.claude_bin),
                &format!("fix claude_bin: cc-telegram profile edit {name}"),
            );
        }
        check(
            service::is_running(name),
            "service running".into(),
            &format!("run: cc-telegram service install {name}"),
        );
        voice_wanted |= cfg.voice.enabled;
        if cfg.voice.enabled {
            let (ff, wh, model) = voice_deps(&cfg);
            check(ff, "ffmpeg".into(), "run: brew install ffmpeg");
            check(
                wh,
                format!("{} (whisper.cpp)", cfg.voice.whisper_bin),
                "run: brew install whisper-cpp",
            );
            check(
                model,
                format!("whisper model {}", cfg.voice.whisper_model),
                &format!("download {WHISPER_MODEL_URL}"),
            );
        }
    }
    if !voice_wanted {
        let ff = which("ffmpeg").is_some();
        let wh = which("whisper-cli").is_some();
        println!(
            "\nvoice (optional): ffmpeg {} · whisper-cli {} → enable with: brew install ffmpeg whisper-cpp, then set [voice] enabled = true",
            if ff { "✅" } else { "✗" },
            if wh { "✅" } else { "✗" }
        );
    }
    if failures > 0 {
        bail!("{failures} check(s) failed");
    }
    Ok(())
}

fn tempfile_probe(dir: &std::path::Path) -> bool {
    let probe = dir.join(".doctor-probe");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}
