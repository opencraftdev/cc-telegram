//! Background service: launchd agent (macOS) or systemd user unit (Linux).

use crate::profile::Profile;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

fn label(name: &str) -> String {
    format!("dev.opencraft.cc-telegram.{name}")
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn plist(name: &str, exe: &Path, logs: &Path, path_env: &str, home: &Path) -> String {
    let s = |p: &Path| xml(&p.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>run</string><string>{name}</string></array>
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
        label = label(name),
        exe = s(exe),
        name = xml(name),
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

pub fn unit(name: &str, exe: &Path, path_env: &str) -> String {
    format!(
        "[Unit]\nDescription=cc-telegram bot ({name})\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} run {name}\nRestart=always\nRestartSec=5\nEnvironment={}\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(&format!("PATH={path_env}")),
    )
}

fn plist_path(name: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(name)))
}

fn unit_path(name: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("systemd/user")
        .join(format!("cc-telegram-{name}.service"))
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

pub fn install(profile: &Profile) -> Result<()> {
    let exe = std::env::current_exe()?;
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
    let name = &profile.name;
    if cfg!(target_os = "macos") {
        let p = plist_path(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        let home = dirs::home_dir().unwrap_or_default();
        std::fs::write(&p, plist(name, &exe, &profile.logs_dir(), &path_env, &home))?;
        let _ = sh(
            "launchctl",
            &["bootout", &format!("{}/{}", gui_domain(), label(name))],
        );
        // bootout is asynchronous: wait (bounded) until the old job is gone, then bootstrap,
        // retrying once since launchd can still answer "Input/output error" right after.
        let target = format!("{}/{}", gui_domain(), label(name));
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
        let p = unit_path(name);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, unit(name, &exe, &path_env))?;
        sh("systemctl", &["--user", "daemon-reload"])?;
        let svc = format!("cc-telegram-{name}.service");
        let out = sh("systemctl", &["--user", "enable", "--now", &svc])?;
        if !out.status.success() {
            bail!(
                "✗ systemctl failed: {} → run: systemctl --user enable --now {svc}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("Tip: to keep it running after logout/reboot → run: loginctl enable-linger $USER");
    } else {
        bail!("services are only supported on macOS and Linux → run: cc-telegram run {name}");
    }
    Ok(())
}

pub fn uninstall(name: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        let _ = sh(
            "launchctl",
            &["bootout", &format!("{}/{}", gui_domain(), label(name))],
        );
        let _ = std::fs::remove_file(plist_path(name));
    } else {
        let svc = format!("cc-telegram-{name}.service");
        let _ = sh("systemctl", &["--user", "disable", "--now", &svc]);
        let _ = std::fs::remove_file(unit_path(name));
        let _ = sh("systemctl", &["--user", "daemon-reload"]);
    }
    Ok(())
}

pub fn is_installed(name: &str) -> bool {
    if cfg!(target_os = "macos") {
        plist_path(name).exists()
    } else {
        unit_path(name).exists()
    }
}

pub fn is_running(name: &str) -> bool {
    if cfg!(target_os = "macos") {
        sh(
            "launchctl",
            &["print", &format!("{}/{}", gui_domain(), label(name))],
        )
        .map(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")
        })
        .unwrap_or(false)
    } else {
        sh(
            "systemctl",
            &[
                "--user",
                "is-active",
                "--quiet",
                &format!("cc-telegram-{name}.service"),
            ],
        )
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
            "work",
            Path::new("/Users/a&b/bin/cc-telegram"),
            Path::new("/Users/a&b/.cc-telegram/profiles/work/logs"),
            "/opt/homebrew/bin:/usr/bin",
            Path::new("/Users/a&b"),
        );
        assert!(p.contains("<string>/Users/a&amp;b/bin/cc-telegram</string><string>run</string><string>work</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(p.contains("profiles/work/logs/service.log"));
    }

    #[test]
    fn unit_quotes_exec() {
        let u = unit("work", Path::new("/home/a b/cc-telegram"), "/usr/bin:/bin");
        assert!(u.contains("ExecStart=\"/home/a b/cc-telegram\" run work"));
        assert!(u.contains("Environment=\"PATH=/usr/bin:/bin\""));
        assert!(u.contains("Restart=always"));
    }
}
