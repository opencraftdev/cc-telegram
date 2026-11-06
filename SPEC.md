# cc-telegram — Spec (v1)

A local, Hermes-style agent: a Telegram bot that forwards messages to **`claude -p`** (Claude Code headless) on your own machine and sends the answers back. One Rust binary, which is both the CLI and the bot daemon. Anyone can clone it and run it with their own bot token and their own Claude Code login.

## Decisions (from grilling session, 2026-11-06)

| # | Topic | Decision |
|---|-------|----------|
| 1 | Access | **Allowlist** of Telegram user IDs per profile. Everyone else is ignored silently. |
| 2 | Conversation | **Per-chat session**: store `session_id`, pass `--resume <id>`. `/new` resets it. |
| 3 | Personality / memory | **Shared `SOUL.md`** per profile + **per-user `MEMORY.md` + `USER.md`**. |
| 4 | Memory writes | **Claude edits its own memory files** (instructed via system prompt, ~2k char cap each). |
| 5 | Tools | **Full agent** with `--dangerously-skip-permissions`, cwd = profile workspace. |
| 6 | Stack | **Rust**: `tokio` + `teloxide` + `serde_json` + `clap`. |
| 7 | Reply delivery | **Live progress**: `stream-json`, one status message edited with tool steps, then the final answer. |
| 8 | Concurrency | **Queue per chat** (FIFO) + `/stop`. Different chats run in parallel. |
| 9 | Inputs | Photos, documents, voice notes (in); files from `outbox/` (out). |
| 10 | Voice STT | **Local whisper.cpp** (`ffmpeg` → wav → `whisper-cli`). Voice is disabled if either is missing. |
| 11 | Scheduling | **`/schedule <cron> <prompt>`**, in-process cron. |
| 12 | Profiles | **Profile = full bot instance** (own token, allowlist, soul, memory, sessions, workspace), created via the CLI. |
| 13 | Daemon | **`service install`**: launchd (macOS) / systemd user unit (Linux). |
| 14 | Model | Profile default model + `/model` per chat, `--max-turns` + timeout, cost shown in `/status`. |

> Why Rust: a single-binary distribution for others to use. It does **not** improve agent memory (that lives in markdown + Claude sessions) or meaningfully reduce RAM (each `claude` process is ~200–400MB of Node no matter what).

## Security model

- `claude -p` runs with **full tool access** (Bash, file edits, web). Every allowlisted user can run arbitrary commands as your OS user. **Only allowlist people you would give your laptop to.**
- Messages from non-allowlisted users are dropped before any processing (no download, no spawn). Each drop is logged with user id and username.
- The bot token sits in `config.toml` with file mode `0600`. It never appears in logs.
- No secrets are passed in prompts. Claude inherits the env of the daemon, so keep that env lean.
- Per-user memory isolation protects against **accidental** leakage only. Claude runs with full file access, so any allowlisted user can ask it to read `config.toml` (the bot token), other users' `MEMORY.md`/`USER.md`, or anything else your OS user can read. It is not a boundary against a hostile allowlisted user.

## Installation (must be dead simple)

Goal: **two commands from zero to chatting**, and nobody needs to know their Telegram user id.

```bash
curl -fsSL https://raw.githubusercontent.com/opencraftdev/cc-telegram/main/install.sh | sh
cc-telegram setup
```

### Install channels

| Channel | Command | Notes |
|---------|---------|-------|
| Install script (primary) | `curl -fsSL .../install.sh \| sh` | Detects OS/arch, downloads a prebuilt binary from GitHub Releases, verifies its sha256, puts it in `~/.local/bin` (no sudo), and prints a PATH hint if needed |
| Homebrew | `brew install opencraftdev/tap/cc-telegram` | Tap formula auto-updated on release |
| Cargo | `cargo install cc-telegram` | For Rust users |

- Release binaries come from **cargo-dist** (GitHub Actions): macOS arm64/x64, Linux x64/arm64. cargo-dist also generates `install.sh` and the Homebrew formula, so we don't hand-maintain either.
- `cc-telegram update`: self-update to the latest release (via `axoupdater`, which ships with cargo-dist).
- `cc-telegram uninstall`: removes services and the binary. It asks before deleting `~/.cc-telegram`.

### `cc-telegram setup` — the guided wizard

Running `cc-telegram` with no args and no profiles also starts it. Each step checks first and fixes or explains:

1. **Check Claude Code.** `claude --version`. If missing: print `npm i -g @anthropic-ai/claude-code` (offer to run it). If not logged in: tell the user to run `claude` once to log in, then continue.
2. **Bot token.** Show 3-line BotFather instructions (`/newbot` in @BotFather, copy the token). The user pastes the token, it's validated live via `getMe`, and the wizard shows `✅ Connected to @your_bot`. Bad tokens are re-asked, not crashed on.
3. **Profile name.** Default `default`. Enter to accept.
4. **Pair yourself (no user-id hunting).** The wizard says *"Open t.me/your_bot and press Start."* The bot temporarily accepts the **first** `/start` (2-minute window) and shows `Is this you? @username (Muhammad, id 123456789) [y/N]` (default No, so pressing Enter never allowlists a stranger). On an explicit yes, that id goes on the allowlist and the bot replies "Paired ✅".
5. **Model.** Pick `sonnet` / `opus` / `haiku` with arrow keys (default sonnet).
6. **Voice (optional).** If `ffmpeg`/`whisper-cli` are missing: "Enable voice notes? Runs `brew install ffmpeg whisper-cpp` + downloads base model (~150MB) [y/N]". Skippable; the wizard shows a progress bar for the model download.
7. **Run in background?** `[Y/n]`. Yes installs the launchd/systemd service and starts it. No prints `cc-telegram run default`.
8. **Done screen:** bot link, profile path, and the 4 most useful commands.

Every prompt has a sane default, so pressing Enter all the way through works (except the pairing confirmation, which needs an explicit `y`). There's also a non-interactive mode for scripts:
`cc-telegram setup --token $TOKEN --profile work --model sonnet --service --no-voice` (prints a pair code instead of the 2-min window).

### Hermes import

If a [Hermes Agent](https://github.com/NousResearch/hermes-agent) install is found (`$HERMES_HOME`, else `~/.hermes`), `setup` offers to import it right after the Claude Code check: it lists what it found (counts and yes/no flags only, never secret values) and asks `Hermes Agent found at ~/.hermes. Import it? [Y/n]`. Hermes profiles (`~/.hermes/profiles/<name>/`, a dir with `config.yaml`, `.env`, `SOUL.md`, `profile.yaml`, `auth.json` or `state.db`) are offered too (multi-select); the root becomes `default` (or a name you pick), others keep their name. Non-interactive: `setup --import-hermes` / `--no-import-hermes` (`--token` overrides the root's token). Standalone: `cc-telegram import hermes [--profile <name>] [--hermes-home <path>] [--yes]`. Nothing in the Hermes home is written.

| Hermes | cc-telegram |
|--------|-------------|
| `.env` `TELEGRAM_BOT_TOKEN` | `bot_token` (validated with `getMe`; if invalid/missing: ask, or skip non-interactively) |
| `.env` `TELEGRAM_ALLOWED_USERS` (comma-separated ids) | merged into `allowed_user_ids` |
| `SOUL.md` | `SOUL.md` |
| `memories/MEMORY.md`, `memories/USER.md` (single owner) | `users/<owner>/`: the one allowed id; if several, you pick (non-interactive: first, with a note); if none, `imported/` and moved to the first user who pairs |
| `config.yaml` `model.default` (or `model: <id>`) | `default_model` only if clearly Claude (→ `opus`/`sonnet`/`haiku`), else our default is kept and the summary says so |
| `skills/**/<skill>/SKILL.md` (category folders, symlinks followed, `.hub` skipped) | copied to `workspace/.claude/skills/<skill>/` so `claude -p` picks them up |

- Partial installs are fine: missing items are reported as skipped. Existing cc-telegram files with content are never overwritten without confirmation (`--yes` overrides). A summary table of imported/skipped items is printed per profile.
- **Token conflict:** two pollers on one token make Telegram answer `409 Conflict`. When the token came from Hermes the importer warns and suggests `hermes gateway stop` (`hermes -p <name> gateway stop` for a Hermes profile) or a new bot. At runtime a 409 from `getUpdates` is logged with the same hint.

### Adding other users later: pair codes

```
cc-telegram pair <profile>        # prints: Send this to your bot within 10 min:  /pair 482913
```
The friend sends `/pair 482913` and is added to the allowlist. Codes are single-use, expire after 10 min, and 5 wrong attempts per user lock that user out. `allow <profile> <user_id>` still works for people who know their id.

### Friendly errors

- Every error names **what failed + exact fix command**, e.g. `✗ claude not logged in → run: claude` or `✗ token rejected by Telegram → run: cc-telegram profile edit default`.
- `cc-telegram doctor` re-runs all setup checks (claude, login, token, service running, ffmpeg, whisper, disk perms) as a ✅/✗ checklist.
- Crates for the wizard: `dialoguer` (prompts/select), `indicatif` (progress), `console` (colors). The single binary stays ~10MB.

## Directory layout

```
~/.cc-telegram/
└── profiles/
    └── <profile>/
        ├── config.toml          # token, allowlist, model, limits (chmod 600)
        ├── SOUL.md              # shared personality for this bot
        ├── state.json           # chat_id -> { session_id, model }
        ├── logs/bot.log
        ├── users/
        │   └── <telegram_user_id>/
        │       ├── MEMORY.md    # facts Claude learned (Claude-edited)
        │       ├── USER.md      # who this user is (Claude-edited)
        │       └── schedules.json
        └── workspace/           # cwd for every claude run in this profile
            ├── inbox/<user_id>/   # downloaded photos/docs/voice transcripts
            └── outbox/<chat_id>/  # files Claude wants sent back
```

One cwd per profile matters because Claude Code keys sessions by project dir. `--resume` only works when the cwd stays the same.

## CLI

```
cc-telegram setup                      # guided wizard (see Installation); also runs on first launch
cc-telegram pair <name>                # one-time code to add another user
cc-telegram update | uninstall
cc-telegram profile create <name>      # same as setup steps 2-7 for an extra profile
cc-telegram profile list
cc-telegram profile edit <name>        # opens config.toml in $EDITOR
cc-telegram profile delete <name>      # confirms; deletes the dir
cc-telegram allow <name> <user_id>     # add to allowlist
cc-telegram deny  <name> <user_id>
cc-telegram run <name>                 # foreground
cc-telegram service install <name>     # launchd/systemd, auto-restart
cc-telegram service uninstall <name>
cc-telegram service status <name>
cc-telegram doctor                     # checks claude, ffmpeg, whisper-cli, token validity
cc-telegram import hermes [--profile <name>] [--hermes-home <path>] [--yes]   # import Hermes Agent
```

`profile create` checks the token with `getMe`, writes a default `SOUL.md`, and prints the bot's @username. Tip shown to the user: send `/start` to the bot and copy your user id from the "not allowed" log line, or use @userinfobot.

### config.toml

```toml
bot_token = "123456:ABC..."
allowed_user_ids = [123456789]
default_model = "sonnet"          # passed as --model
max_turns = 30                    # --max-turns
run_timeout_secs = 900            # kill claude after this
claude_bin = "claude"             # path override
extra_claude_args = []            # appended verbatim

[voice]
enabled = true                    # auto-false if deps missing
whisper_bin = "whisper-cli"
whisper_model = "~/.cc-telegram/models/ggml-base.bin"
language = "auto"
```

## Core flow (one message)

1. **Receive** an update via long polling (`teloxide`). Drop it if `from.id` is not in the allowlist.
2. **Normalize the input** into a prompt string:
   - text: as-is
   - photo: download the largest size to `inbox/<uid>/<ts>.jpg`, prompt = caption + `\n[Attached image: <abs path>]`
   - document: download to `inbox/<uid>/<filename>`, prompt = caption + `[Attached file: <abs path>]`
   - voice: download `.oga`, `ffmpeg -i in.oga -ar 16000 -ac 1 out.wav`, `whisper-cli -m <model> -f out.wav -otxt`, prompt = `[Voice transcript]: <text>`. Reply with the transcript first so the user can see what was heard.
3. **Enqueue** on the chat's queue (`HashMap<ChatId, mpsc::Sender<Job>>`, one worker task per chat).
4. **Build the system prompt** fresh on every run (so memory edits apply next turn):
   ```
   <SOUL.md>

   ## About this user (USER.md — path: <abs>)
   <USER.md>

   ## Your memory of this user (MEMORY.md — path: <abs>)
   <MEMORY.md>

   ## Memory rules
   - When you learn something durable about the user or their preferences, Edit USER.md / MEMORY.md at the paths above.
   - Keep each file under 2000 characters; prune or merge old entries instead of appending forever.
   - Never store secrets (passwords, tokens, keys).
   - To send a file back, write it into <abs outbox/chat_id>/.
   - You are replying in Telegram: be concise, use plain Markdown.
   ```
5. **Spawn**:
   ```
   claude -p "<prompt>" \
     --output-format stream-json --verbose \
     --append-system-prompt "<system prompt>" \
     --dangerously-skip-permissions \
     --model <chat model or default> \
     --max-turns <n> \
     [--resume <session_id>]
   ```
   cwd = `workspace/`. Pass the prompt via an arg (not a shell string, so nothing gets shell-interpreted). Wrap the run in `tokio::time::timeout`.
6. **Stream** stdout line by line as JSON:
   - `system/init`: capture `session_id`
   - `assistant` with `tool_use` blocks: append `🔧 <tool>: <short input>` to the status message. Edit at most once every 1.5s (Telegram rate limit) and keep the last ~10 lines.
   - `result`: final `result` text, `session_id`, `total_cost_usd`, `is_error`
   - Send a `typing` chat action every 4s while running.
7. **Reply**: send the final text as a new message (split at 4096 chars, try MarkdownV2 and fall back to plain text on a parse error). Delete the status message or collapse it to `✅ done · $0.04 · 12s`.
8. **Outbox**: send every file in `outbox/<chat_id>/` (photos as photo, others as document), then delete them.
9. **Persist** `session_id` → `state.json` (atomic write: temp file + rename).

### Errors

- If `--resume` fails because the session is missing or corrupt, clear the session id, retry once without `--resume`, and tell the user "started a new session".
- On timeout, kill the process group and reply `⏱️ timed out after Ns`. The session is kept.
- If `claude` is not found or not logged in, reply with the stderr summary. `doctor` explains the fix.

## Chat commands

| Command | Action |
|---------|--------|
| `/start` | Greeting + your user id (non-allowlisted users get silence; their id goes to the log only) |
| `/new` | Drop the chat's session id (memory files stay) |
| `/stop` | Kill the running claude process for this chat and clear the queue |
| `/status` | Model, session id (short), queue length, running time, last run cost |
| `/model <opus\|sonnet\|haiku\|full-id>` | Set the model for this chat |
| `/memory` | Show this user's MEMORY.md + USER.md |
| `/forget` | Wipe this user's MEMORY.md + USER.md (asks for confirmation via inline button) |
| `/schedule <m h dom mon dow> <prompt>` | Add a cron job |
| `/schedules` | List jobs with ids |
| `/unschedule <id>` | Remove a job |

## Scheduling

- `users/<uid>/schedules.json`: `[{ id, cron, prompt, chat_id, created_at }]`. 5-field cron, local timezone.
- Use the `cron` crate. Every minute a single tokio task checks all jobs across users.
- A due job goes onto **that chat's queue** like a normal message, but runs **without `--resume`** (a fresh session, so cron runs don't pollute the chat context). Memory + soul still get injected. The output is posted to `chat_id`, prefixed `⏰ <prompt snippet>`.
- Missed runs while the bot was down are skipped, not backfilled.

## Rust crates

`tokio` (full), `teloxide` (macros), `serde`/`serde_json`, `toml`, `clap` (derive), `cron`, `chrono`, `tracing` + `tracing-appender`, `anyhow`, `dirs`. Nothing else unless a need shows up.

Module sketch:
```
src/main.rs        # clap CLI dispatch
src/profile.rs     # create/list/edit/delete, config load/save
src/bot.rs         # teloxide dispatcher, allowlist filter, commands
src/media.rs       # downloads, ffmpeg + whisper
src/runner.rs      # spawn claude, parse stream-json, timeout, kill
src/queue.rs       # per-chat workers, /stop handles
src/prompt.rs      # system prompt assembly from SOUL/USER/MEMORY
src/schedule.rs    # cron loop
src/service.rs     # launchd plist / systemd unit
src/state.rs       # state.json atomic persistence
```

## Acceptance criteria (v1)

- [ ] On a fresh Mac with Claude Code logged in: `install.sh` + `cc-telegram setup` pressing only Enter + Telegram Start leads to a working bot in under 3 minutes
- [ ] Setup never asks the user to type their Telegram user id
- [ ] A friend is added via `cc-telegram pair` + `/pair <code>` with no config editing
- [ ] Every setup failure prints the exact fix command

- [ ] `profile create` → `run` → a message from an allowlisted user gets a Claude reply in Telegram
- [ ] A non-allowlisted user gets no reply, and the attempt is logged
- [ ] A follow-up message remembers the previous turn; `/new` forgets it
- [ ] Telling the bot "I prefer Indonesian replies" updates USER.md, and the preference survives `/new`
- [ ] Two allowlisted users never see each other's memory
- [ ] The status message shows tool steps live during a multi-step task
- [ ] Two quick messages in one chat run in order; `/stop` kills the running one
- [ ] Photo, PDF and voice note inputs work; a file Claude writes to outbox arrives in chat
- [ ] `/schedule */5 * * * * say hi` posts every 5 minutes; `/unschedule` stops it
- [ ] `service install` survives a reboot; `doctor` reports missing deps clearly
- [ ] Two profiles (two bot tokens) run side by side without sharing state
- [ ] With `~/.hermes` present, `setup` offers the import; token, allowlist, SOUL.md, memories, Claude model and skills land in the right places; no secret is printed; `~/.hermes` is untouched
- [ ] A partial Hermes install (e.g. only `config.yaml` + `skills/`) imports what exists and reports the rest as skipped
- [ ] Re-running the import never overwrites edited files without confirmation (or `--yes`)
- [ ] When the Hermes gateway still polls the same token, the bot logs the 409 hint instead of failing silently

## Out of scope (v1)

- Group chats / @mention mode
- Spend budgets (just display the cost)
- Remote/cloud hosting, webhooks (long polling only)
- Multiple workspaces per profile (`/cd`). Possible v2: `cc-telegram workspace add`
- Windows service support
