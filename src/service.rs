//! Background service: launchd agent (macOS) or systemd user unit (Linux), either per
//! profile (`run <profile>`) or one for every profile (`run --all`).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a service runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Profile(String),
    All,
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::Profile(n) => format!("dev.opencraft.cc-gateway.{n}"),
            Target::All => "dev.opencraft.cc-gateway.all".into(),
        }
    }
    fn run_args(&self) -> Vec<String> {
        match self {
            Target::Profile(n) => vec!["run".into(), n.clone()],
            Target::All => vec!["run".into(), "--all".into()],
        }
    }
    /// systemd unit name.
    fn unit(&self) -> String {
        match self {
            Target::Profile(n) => format!("cc-gateway@{n}.service"),
            Target::All => "cc-gateway-all.service".into(),
        }
    }
    /// Where launchd writes stdout/stderr.
    fn logs_dir(&self) -> PathBuf {
        match self {
            Target::Profile(n) => crate::profile::profiles_dir().join(n).join("logs"),
            Target::All => crate::profile::home().join("logs"),
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Target::Profile(n) => n.clone(),
            Target::All => "--all".into(),
        }
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn plist(t: &Target, exe: &Path, logs: &Path, path_env: &str, home: &Path) -> String {
    let s = |p: &Path| xml(&p.to_string_lossy());
    let args: String = t
        .run_args()
        .iter()
        .map(|a| format!("<string>{}</string>", xml(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string>{args}</array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{out}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>{path}</string>
    <key>HOME</key><string>{home}</string>
  </dict>
</dict>
</plist>
"#,
        label = t.label(),
        exe = s(exe),
        out = s(&logs.join("service.log")),
        path = xml(path_env),
        home = s(home),
    )
}

fn systemd_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

/// Profile: template `cc-gateway@.service` (`%i` = profile). All: `cc-gateway-all.service`.
pub fn unit(t: &Target, exe: &Path, path_env: &str) -> String {
    let (desc, args) = match t {
        Target::Profile(_) => ("cc-gateway (%i)", "run %i"),
        Target::All => ("cc-gateway (all profiles)", "run --all"),
    };
    format!(
        "[Unit]\nDescription={desc}\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} {args}\nRestart=always\nRestartSec=5\nEnvironment={}\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(&format!("PATH={path_env}")),
    )
}

fn plist_path(t: &Target) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", t.label()))
}

fn unit_path(t: &Target) -> PathBuf {
    let file = match t {
        Target::Profile(_) => "cc-gateway@.service".to_string(),
        Target::All => t.unit(),
    };
    dirs::config_dir()
        .unwrap_or_default()
        .join("systemd/user")
        .join(file)
}

fn sh(cmd: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd}"))
}

fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Dirs the service must see besides the installing shell's PATH: `~/.local/bin` (where the
/// install script puts things) and the folder of each served profile's resolved `claude_bin`.
fn extra_path_dirs(t: &Target) -> Vec<PathBuf> {
    let mut dirs = vec![dirs::home_dir().unwrap_or_default().join(".local/bin")];
    let names = match t {
        Target::Profile(n) => vec![n.clone()],
        Target::All => crate::profile::list(),
    };
    for n in names {
        if let Ok(cfg) = crate::profile::Profile::open(&n).and_then(|p| p.load_config())
            && let Some(dir) =
                crate::media::which(&cfg.claude_bin).and_then(|b| b.parent().map(Path::to_path_buf))
        {
            dirs.push(dir);
        }
    }
    dirs
}

/// `current` PATH plus `extra` dirs not already in it.
pub fn service_path(current: &str, extra: &[PathBuf]) -> String {
    let mut parts: Vec<String> = current
        .split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    for d in extra {
        let d = d.to_string_lossy().into_owned();
        if !d.is_empty() && !parts.contains(&d) {
            parts.push(d);
        }
    }
    parts.join(":")
}

/// Per-profile services and the --all service poll the same tokens: never both (409).
fn check_conflicts(t: &Target) -> Result<()> {
    match t {
        Target::All => {
            let per: Vec<String> = crate::profile::list()
                .into_iter()
                .filter(|n| is_installed(&Target::Profile(n.clone())))
                .collect();
            if !per.is_empty() {
                bail!(
                    "✗ per-profile services are installed for {} (they would poll the same bots → 409) → run: {}",
                    per.join(", "),
                    per.iter()
                        .map(|n| format!("cc-gateway service uninstall {n}"))
                        .collect::<Vec<_>>()
                        .join(" && ")
                );
            }
        }
        Target::Profile(n) if is_installed(&Target::All) => bail!(
            "✗ the all-profiles service is installed and already runs {n} → use it, or run: cc-gateway service uninstall --all"
        ),
        Target::Profile(_) => {}
    }
    Ok(())
}

pub fn install(t: &Target) -> Result<()> {
    check_conflicts(t)?;
    let exe = std::env::current_exe()?;
    let path_env = service_path(
        &std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        &extra_path_dirs(t),
    );
    if cfg!(target_os = "macos") {
        let p = plist_path(t);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::create_dir_all(t.logs_dir())?;
        let home = dirs::home_dir().unwrap_or_default();
        std::fs::write(&p, plist(t, &exe, &t.logs_dir(), &path_env, &home))?;
        let target = format!("{}/{}", gui_domain(), t.label());
        let _ = sh("launchctl", &["bootout", &target]);
        // bootout is asynchronous: wait (bounded) until the old job is gone, then bootstrap,
        // retrying once since launchd can still answer "Input/output error" right after.
        for _ in 0..20 {
            if !sh("launchctl", &["print", &target]).is_ok_and(|o| o.status.success()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let bootstrap = || {
            sh(
                "launchctl",
                &["bootstrap", &gui_domain(), &p.to_string_lossy()],
            )
        };
        let mut out = bootstrap()?;
        if !out.status.success() {
            std::thread::sleep(std::time::Duration::from_secs(1));
            out = bootstrap()?;
        }
        if !out.status.success() {
            bail!(
                "✗ launchctl bootstrap failed: {} → run: launchctl bootstrap {} {}",
                String::from_utf8_lossy(&out.stderr).trim(),
                gui_domain(),
                p.display()
            );
        }
    } else if cfg!(target_os = "linux") {
        let p = unit_path(t);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, unit(t, &exe, &path_env))?;
        sh("systemctl", &["--user", "daemon-reload"])?;
        let svc = t.unit();
        // enable + restart (not `enable --now`) so a reinstall picks up a new binary/config.
        let mut out = sh("systemctl", &["--user", "enable", &svc])?;
        if out.status.success() {
            out = sh("systemctl", &["--user", "restart", &svc])?;
        }
        if !out.status.success() {
            bail!(
                "✗ systemctl failed: {} → run: systemctl --user enable --now {svc}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("Tip: to keep it running after logout/reboot → run: loginctl enable-linger $USER");
    } else {
        bail!(
            "services are only supported on macOS and Linux → run: cc-gateway run {}",
            t.describe()
        );
    }
    Ok(())
}

pub fn uninstall(t: &Target) -> Result<()> {
    if cfg!(target_os = "macos") {
        let _ = sh(
            "launchctl",
            &["bootout", &format!("{}/{}", gui_domain(), t.label())],
        );
        let _ = std::fs::remove_file(plist_path(t));
    } else {
        let _ = sh("systemctl", &["--user", "disable", "--now", &t.unit()]);
        // The profile template is shared: remove it only when no other profile still uses it.
        let shared_in_use = match t {
            Target::Profile(name) => crate::profile::list()
                .iter()
                .any(|n| n != name && is_installed(&Target::Profile(n.clone()))),
            Target::All => false,
        };
        if !shared_in_use {
            let _ = std::fs::remove_file(unit_path(t));
            let _ = sh("systemctl", &["--user", "daemon-reload"]);
        }
    }
    Ok(())
}

/// Remove a pre-rename (cc-telegram) service for `name`, if present. Returns what was done.
pub fn remove_legacy(name: &str) -> Vec<String> {
    let mut done = vec![];
    let home = dirs::home_dir().unwrap_or_default();
    let label = format!("dev.opencraft.cc-telegram.{name}");
    let plist = home.join(format!("Library/LaunchAgents/{label}.plist"));
    if plist.exists() {
        let _ = sh(
            "launchctl",
            &["bootout", &format!("{}/{label}", gui_domain())],
        );
        if std::fs::remove_file(&plist).is_ok() {
            done.push(format!("removed old launchd agent {label}"));
        }
    }
    let unit = format!("cc-telegram-{name}.service");
    let unit_file = dirs::config_dir()
        .unwrap_or_default()
        .join("systemd/user")
        .join(&unit);
    if unit_file.exists() {
        let _ = sh("systemctl", &["--user", "disable", "--now", &unit]);
        if std::fs::remove_file(&unit_file).is_ok() {
            let _ = sh("systemctl", &["--user", "daemon-reload"]);
            done.push(format!("removed old systemd unit {unit}"));
        }
    }
    done
}

pub fn is_installed(t: &Target) -> bool {
    if cfg!(target_os = "macos") {
        plist_path(t).exists()
    } else {
        unit_path(t).exists()
            && sh("systemctl", &["--user", "is-enabled", "--quiet", &t.unit()])
                .is_ok_and(|o| o.status.success())
    }
}

pub fn is_running(t: &Target) -> bool {
    if cfg!(target_os = "macos") {
        sh(
            "launchctl",
            &["print", &format!("{}/{}", gui_domain(), t.label())],
        )
        .map(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")
        })
        .unwrap_or(false)
    } else {
        sh("systemctl", &["--user", "is-active", "--quiet", &t.unit()])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_and_keeps_path() {
        let p = plist(
            &Target::Profile("work".into()),
            Path::new("/Users/a&b/bin/cc-gateway"),
            Path::new("/Users/a&b/.cc-gateway/profiles/work/logs"),
            "/opt/homebrew/bin:/usr/bin",
            Path::new("/Users/a&b"),
        );
        assert!(p.contains("<string>/Users/a&amp;b/bin/cc-gateway</string><string>run</string><string>work</string>"));
        assert!(p.contains("<key>Label</key><string>dev.opencraft.cc-gateway.work</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(p.contains("profiles/work/logs/service.log"));
        let all = plist(
            &Target::All,
            Path::new("/x"),
            Path::new("/l"),
            "/p",
            Path::new("/h"),
        );
        assert!(all.contains("<string>/x</string><string>run</string><string>--all</string>"));
        assert!(all.contains("dev.opencraft.cc-gateway.all"));
    }

    #[test]
    fn unit_quotes_exec() {
        let t = Target::Profile("work".into());
        let u = unit(&t, Path::new("/home/a b/100%/cc-gateway"), "/usr/bin:/bin");
        assert!(u.contains("ExecStart=\"/home/a b/100%%/cc-gateway\" run %i"));
        assert_eq!(t.unit(), "cc-gateway@work.service");
        assert!(u.contains("Environment=\"PATH=/usr/bin:/bin\""));
        assert!(u.contains("Restart=always"));
        let a = unit(&Target::All, Path::new("/usr/bin/cc-gateway"), "/usr/bin");
        assert!(a.contains("ExecStart=\"/usr/bin/cc-gateway\" run --all"));
        assert_eq!(Target::All.unit(), "cc-gateway-all.service");
    }

    #[test]
    fn service_path_adds_missing_dirs_once() {
        let p = service_path(
            "/usr/bin:/bin",
            &[
                PathBuf::from("/home/u/.local/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/home/u/.local/bin"),
            ],
        );
        assert_eq!(p, "/usr/bin:/bin:/home/u/.local/bin");
    }
}
