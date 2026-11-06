# Getting started

## 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/opencraftdev/cc-gateways/main/install.sh | sh
```

The script picks the release binary for your OS and architecture, checks its sha256, and installs it to `~/.local/bin` without sudo. It prints a PATH hint if that folder isn't on your PATH yet. You can also use `brew install opencraftdev/tap/cc-gateway` or `cargo install cc-gateway`.

You also need Claude Code, logged in:

```bash
npm i -g @anthropic-ai/claude-code
claude            # log in once, then exit
```

cc-gateway runs `claude -p` with your own login, so usage comes out of your Claude subscription. No Anthropic API key is used or needed.

## 2. Run the wizard

```bash
cc-gateway setup
```

Running `cc-gateway` with no arguments also starts the wizard when no profile exists yet. Pressing Enter accepts the default at every prompt.

| Step | What happens |
|------|--------------|
| 1. Claude Code | Checks `claude --version` and the login. If claude is missing, it offers to run `npm i -g @anthropic-ai/claude-code`. If a Hermes Agent install is found, it offers to import it ([hermes-import.md](hermes-import.md)). |
| 2. Gateways | `Enable Telegram?` (default yes) and `Enable API server (OpenAI-compatible, on 127.0.0.1:8787)?` (default no). If Telegram is on, paste a bot token from @BotFather. The token is checked live. |
| 3. Profile name | Default `default`. |
| 4. Pair yourself | Telegram only. Open your bot and press **Start**. The wizard shows `Is this you? @name (Full Name, id …)`; type `y` to confirm (the default is No). If the API is on, a key named `default` is created here and printed once. |
| 5. Model | `sonnet`, `opus` or `haiku`. |
| 6. Voice notes | Optional. Needs `ffmpeg`, `whisper-cli` and a whisper model. The wizard can install them with Homebrew and downloads the ~150 MB model with a progress bar. |
| 7. Run in background | Installs a launchd agent (macOS) or a systemd user unit (Linux). If you say no, start it with `cc-gateway run <profile>`. |

## Non-interactive setup

```bash
# Telegram + API, as a background service, no voice
cc-gateway setup --token "$BOT_TOKEN" --profile work --model sonnet --api --service --no-voice

# API only (no Telegram token)
cc-gateway setup --api --profile api

# Import a Hermes Agent install
cc-gateway setup --import-hermes
```

| Flag | Meaning |
|------|---------|
| `--token <TOKEN>` | Telegram bot token. Turns on non-interactive mode. |
| `--profile <NAME>` | Profile name (default `default`). |
| `--model <MODEL>` | `sonnet`, `opus`, `haiku` or a full model id. |
| `--api` | Enable the API gateway and create a key named `default`, printed once. Turns on non-interactive mode. |
| `--service` | Install and start the background service. |
| `--no-voice` | Don't enable voice notes. |
| `--import-hermes` | Import a detected Hermes Agent install. Turns on non-interactive mode. |
| `--no-import-hermes` | Don't offer the Hermes import in the wizard. |

`--model`, `--service` and `--no-voice` only apply in non-interactive mode, that is together with `--token`, `--api` or `--import-hermes`; the interactive wizard asks instead. In non-interactive mode a Telegram profile gets a `/pair <code>` to send to the bot instead of the 2-minute Start window.

## 3. Check everything

```bash
cc-gateway doctor
```

This prints a ✅/✗ checklist covering claude, login, each profile's config permissions, the Telegram token and allowlist, API bind and keys, the service, and voice dependencies. Every ✗ comes with the command that fixes it.

Next: [telegram.md](telegram.md) or [api.md](api.md).
