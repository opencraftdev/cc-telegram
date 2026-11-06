//! Telegram downloads + voice transcription (ffmpeg -> wav -> whisper-cli).

use crate::profile::{Config, Profile, expand_tilde};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::FileId;

/// Find an executable on PATH (or check an explicit path).
pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = expand_tilde(bin);
        return p.is_file().then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

pub fn voice_ready(cfg: &Config) -> bool {
    cfg.voice.enabled
        && which("ffmpeg").is_some()
        && which(&cfg.voice.whisper_bin).is_some()
        && expand_tilde(&cfg.voice.whisper_model).is_file()
}

/// Keep only the final path component so a crafted file name can't escape the inbox.
pub fn safe_file_name(name: Option<&str>, fallback: &str) -> String {
    name.and_then(|n| Path::new(n).file_name())
        .and_then(|n| n.to_str())
        .filter(|n| !n.starts_with('.'))
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

async fn download(bot: &Bot, id: &FileId, dest: &Path) -> Result<()> {
    let file = bot.get_file(id.clone()).await?;
    let mut out = tokio::fs::File::create(dest).await?;
    bot.download_file(&file.path, &mut out).await?;
    Ok(())
}

/// Turn a message into a prompt string. `Ok(None)` = nothing to run.
pub async fn to_prompt(
    bot: &Bot,
    profile: &Profile,
    cfg: &Config,
    msg: &Message,
    uid: u64,
) -> Result<Option<String>> {
    if let Some(t) = msg.text() {
        return Ok(Some(t.to_string()));
    }
    let caption = msg.caption().unwrap_or("").to_string();
    let inbox = profile.inbox(uid);
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S%3f").to_string();
    let with_caption = |tag: String| {
        if caption.is_empty() {
            tag
        } else {
            format!("{caption}\n{tag}")
        }
    };

    if let Some(photos) = msg.photo() {
        let Some(p) = photos.iter().max_by_key(|p| p.width * p.height) else {
            return Ok(None);
        };
        std::fs::create_dir_all(&inbox)?;
        let dest = inbox.join(format!("{ts}.jpg"));
        download(bot, &p.file.id, &dest).await?;
        return Ok(Some(with_caption(format!(
            "[Attached image: {}]",
            dest.display()
        ))));
    }
    if let Some(doc) = msg.document() {
        std::fs::create_dir_all(&inbox)?;
        let dest = inbox.join(safe_file_name(
            doc.file_name.as_deref(),
            &format!("{ts}.bin"),
        ));
        download(bot, &doc.file.id, &dest).await?;
        return Ok(Some(with_caption(format!(
            "[Attached file: {}]",
            dest.display()
        ))));
    }
    if let Some(voice) = msg.voice() {
        if !voice_ready(cfg) {
            bot.send_message(
                msg.chat.id,
                "🎤 Voice notes are disabled (needs ffmpeg + whisper-cli + model) → run: cc-gateway doctor",
            )
            .await?;
            return Ok(None);
        }
        std::fs::create_dir_all(&inbox)?;
        let oga = inbox.join(format!("{ts}.oga"));
        download(bot, &voice.file.id, &oga).await?;
        let text = transcribe(cfg, &oga).await?;
        bot.send_message(msg.chat.id, format!("🎤 {text}")).await?;
        return Ok(Some(format!("[Voice transcript]: {text}")));
    }
    Ok(None)
}

async fn transcribe(cfg: &Config, oga: &Path) -> Result<String> {
    let wav = oga.with_extension("wav");
    let base = oga.with_extension(""); // whisper-cli writes <base>.txt
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(oga)
        .args(["-ar", "16000", "-ac", "1"])
        .arg(&wav)
        .output()
        .await
        .context("running ffmpeg")?;
    if !out.status.success() {
        bail!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let out = tokio::process::Command::new(expand_tilde(&cfg.voice.whisper_bin))
        .arg("-m")
        .arg(expand_tilde(&cfg.voice.whisper_model))
        .arg("-f")
        .arg(&wav)
        .args(["-l", &cfg.voice.language, "-np", "-otxt", "-of"])
        .arg(&base)
        .output()
        .await
        .context("running whisper-cli")?;
    let _ = std::fs::remove_file(&wav);
    if !out.status.success() {
        bail!(
            "whisper-cli failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = std::fs::read_to_string(base.with_extension("txt"))?;
    Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_cannot_escape() {
        assert_eq!(safe_file_name(Some("../../etc/passwd"), "f"), "passwd");
        assert_eq!(safe_file_name(Some("/abs/x.pdf"), "f"), "x.pdf");
        assert_eq!(safe_file_name(Some(".."), "f"), "f");
        assert_eq!(safe_file_name(Some(".bashrc"), "f"), "f");
        assert_eq!(safe_file_name(None, "f"), "f");
        assert_eq!(safe_file_name(Some("report.pdf"), "f"), "report.pdf");
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("/bin/sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }
}
