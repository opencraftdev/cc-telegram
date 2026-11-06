# cc-telegram

A Telegram bot that forwards your messages to **Claude Code** (`claude -p`) on your own machine and sends the answers back, with live tool progress, per-chat sessions, per-user memory, files, voice notes and cron schedules. One Rust binary: both the CLI and the bot daemon.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/opencraftdev/cc-telegram/main/install.sh | sh
cc-telegram setup
```

Also: `brew install opencraftdev/tap/cc-telegram` or `cargo install cc-telegram`.

Needs [Claude Code](https://docs.anthropic.com/en/docs/claude-code) installed and logged in (`npm i -g @anthropic-ai/claude-code`, then run `claude` once). Voice notes optionally need `ffmpeg` and `whisper-cli` (`brew install ffmpeg whisper-cpp`).

## Setup

`cc-telegram setup` walks you through it: it checks Claude Code, asks for a bot token from @BotFather, pairs you by having you press **Start** in your bot (no user-id hunting), then asks for a model and voice notes and installs a background service. Pressing Enter all the way through works, except the "Is this you?" pairing check, which needs an explicit `y`.

Non-interactive:

```bash
cc-telegram setup --token "$TOKEN" --profile work --model sonnet --service --no-voice
# prints a /pair code to send to the bot
```

Coming from [Hermes Agent](https://github.com/NousResearch/hermes-agent)? `setup` detects `~/.hermes` (or `$HERMES_HOME`) and offers to import the bot token, allowlist, `SOUL.md`, memories, model (Claude models only) and skills. Standalone: `cc-telegram import hermes [--profile <name>] [--hermes-home <path>] [--yes]`. Stop the Hermes gateway first (`hermes gateway stop`) or use a new bot: two processes polling one token get `409 Conflict`.

Add a friend: `cc-telegram pair <profile>` prints `/pair 123456`. They send it to the bot within 10 minutes. Codes are single-use, and 5 wrong attempts lock that user out.

## CLI

```
cc-telegram setup                      # guided wizard (also runs on first launch)
cc-telegram pair <name>                # one-time code to add another user
cc-telegram update | uninstall
cc-telegram profile create|list|edit|delete <name>
cc-telegram allow|deny <name> <user_id>
cc-telegram run <name>                 # foreground
cc-telegram service install|uninstall|status <name>
cc-telegram doctor                     # checks claude, login, tokens, service, ffmpeg, whisper
cc-telegram import hermes [--profile <name>] [--hermes-home <path>] [--yes]
```

Data lives in `~/.cc-telegram/profiles/<name>/`: `config.toml` (0600), `SOUL.md`, `state.json`, `logs/`, `users/<id>/{MEMORY.md,USER.md,schedules.json}`, and `workspace/` (cwd for every run, with `inbox/` and `outbox/`).

## Chat commands

| Command | Action |
|---|---|
| `/start` | Greeting + your user id |
| `/new` | Start a new session (memory files stay) |
| `/stop` | Kill the running job and clear the queue |
| `/status` | Model, session, queue, running time, last cost |
| `/model <opus\|sonnet\|haiku\|id>` | Model for this chat |
| `/memory` / `/forget` | Show / wipe your MEMORY.md + USER.md |
| `/schedule <m h dom mon dow> <prompt>` | Add a cron job (local time) |
| `/schedules` / `/unschedule <id>` | List / remove jobs |

Photos, documents and voice notes are passed to Claude as files in the workspace. Files Claude writes to `outbox/<chat_id>/` are sent back.

## ⚠️ Security

Claude runs with `--dangerously-skip-permissions`: **every allowlisted user can run arbitrary commands as your OS user** (shell, file edits, web). Only allowlist people you would hand your laptop to. Messages from anyone else are dropped before any download or spawn, and logged. The bot token is stored in `config.toml` with mode 0600 and is never logged. Claude inherits the daemon's environment, so keep it free of secrets.

Per-user memory separation only prevents *accidental* leakage. Because Claude has full file access, any allowlisted user can ask it to read `config.toml` (your bot token), other users' `MEMORY.md`/`USER.md`, or any file your OS user can read.
