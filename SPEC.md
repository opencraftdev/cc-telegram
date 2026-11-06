# cc-gateway — Spec (v1)

A local, Hermes-style agent gateway in front of **`claude -p`** (Claude Code headless) on your own machine. A profile runs one or more **gateways**: a **Telegram** bot and/or an **OpenAI-compatible HTTP API**. One Rust binary (`cc-gateway`), which is both the CLI and the daemon. Anyone can run it with their own Claude Code login. The backend is always the user's own Claude subscription through `claude -p`; there is **no Anthropic API key** anywhere.

Repository: `github.com/opencraftdev/cc-gateways` (renamed from `cc-telegram`).

## Decisions (from grilling session, 2026-11-06)

| # | Topic | Decision |
|---|-------|----------|
| 1 | Access | **Allowlist** of Telegram user IDs per profile. Everyone else is ignored silently. |
| 2 | Conversation | **Per-chat session**: store `session_id`, pass `--resume <id>`. `/new` resets it. |
| 3 | Personality / memory | **Shared `SOUL.md`** + **profile-wide `MEMORY.md` + `USER.md`** (Hermes semantics: shared by everyone using the bot) + **per-person notes** `users/<id>/USER.md` + `MEMORY.md`. |
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
| 15 | Name | **`cc-gateway`** (repo `cc-gateways`). Home `~/.cc-gateway`; an existing `~/.cc-telegram` is moved there once, with a notice; old paths in configs are rewritten and old `cc-telegram` services removed. |
| 16 | Gateways | Telegram and API are gateways inside one profile; `cc-gateway run <profile>` starts whichever are enabled. Telegram is on when `bot_token` is set; API when `[api] enabled = true`. API-only profiles need no token. |
| 17 | API | **axum**, OpenAI-compatible `/v1/chat/completions` + `/v1/models`, plus a native `/v1/runs` API. Loopback bind by default; non-loopback prints a loud warning. |
| 18 | API auth | Bearer keys `ccg_…` created by `cc-gateway key create`; only sha256 + name + created_at are stored. Each key has its own memory namespace `users/key-<name>/`. |
| 19 | API concurrency | Same per-conversation queues as Telegram; API runs are queued by session id (`session:<id>`), so one session never runs twice at once. Global `max_concurrent` cap → 429. Sessions belong to their creator (`sessions.json`): only the API key that created a session can resume it; Telegram sessions can't be resumed over the API. |
| 20 | Hermes server import | Allowlist derived like Hermes' authz (`gateway/authz_mixin.py`): `TELEGRAM_ALLOWED_USERS` ∪ `GATEWAY_ALLOWED_USERS` ∪ approved pairings ∪ the home-channel owner. Hermes allow-all is **not** imported (owner only, with a warning). Discord-only profiles are skipped. |
| 21 | Cron import | Hermes `cron/jobs.json` → our schedules: agent jobs → prompt jobs, `no_agent` script jobs → **command jobs** (argv, no shell, no claude). `enabled`, interval schedules and silent (`deliver = false`) jobs are supported. |
| 22 | RAM | Process-wide cap `max_concurrent_runs` (default 2) on concurrent claude processes, shared by Telegram, API and cron; waiting jobs queue (Telegram shows `⏳ queued (n ahead)`). |
| 24 | Prompt parity with Hermes | SOUL.md is the identity slot and explicitly overrides Claude Code's default persona (kept appended, not replaced: verified with claude 2.1.x). Memory rendered like Hermes (`§`-delimited entries, `[pct — n/limit chars]` headers, limits 2200/1375), Telegram platform hint with the sender's name, first `.hermes.md`/`HERMES.md`/`AGENTS.md` in the working dir, current date + UTC offset. |
| 25 | History import | Hermes `state.db` (read-only, immutable) → last 30 user/assistant messages per allowlisted Telegram chat in `users/<chat>/HISTORY.md`, injected only until that chat's first session is stored. |
| 26 | Working dir | Optional `workdir` (imported from Hermes `terminal.cwd` / `TERMINAL_CWD` when absolute): claude's cwd; the workspace is then passed with `--add-dir`, which keeps its skills loaded. |
| 27 | Flaky networks | getMe errors are classified: only HTTP 401/404 = "token rejected"; anything else = "can't reach Telegram (network)", retried (CLI: 8×, 1s→16s; bot startup: forever, cap 5 min). api.telegram.org also resolves to Telegram's seed IPs after the DNS answer. A typing indicator runs from message acceptance to the last file sent (4s, 3s-capped requests). |
| 23 | Multi-profile | `cc-gateway run --all` runs every profile in one process; `service install --all` installs one unit for it and refuses if per-profile units exist (double polling → 409). |

> Why Rust: a single-binary distribution for others to use. It does **not** improve agent memory (that lives in markdown + Claude sessions) or meaningfully reduce RAM (each `claude` process is ~200–400MB of Node no matter what).

## Security model

- `claude -p` runs with **full tool access** (Bash, file edits, web). Every allowlisted user can run arbitrary commands as your OS user. **Only allowlist people you would give your laptop to.**
- Messages from non-allowlisted users are dropped before any processing (no download, no spawn). Each drop is logged with user id and username.
- The bot token sits in `config.toml` with file mode `0600`. It never appears in logs.
- No secrets are passed in prompts. Claude inherits the env of the daemon, so keep that env lean.
- **API keys are equivalent to a shell on this machine.** The API binds to `127.0.0.1` by default; binding elsewhere logs and prints a warning at startup. Remote use should go through an authenticated tunnel. Keys are stored only as sha256 hashes; compare is constant-time; revocation applies on the next request.
- `image_url` parts must be `data:` URLs; remote URLs are rejected (no server-side fetching, no SSRF).
- Per-user memory isolation protects against **accidental** leakage only. Claude runs with full file access, so any allowlisted user can ask it to read `config.toml` (the bot token), other users' `MEMORY.md`/`USER.md`, or anything else your OS user can read. It is not a boundary against a hostile allowlisted user.

## Installation (must be dead simple)

Goal: **two commands from zero to chatting**, and nobody needs to know their Telegram user id.

```bash
curl -fsSL https://raw.githubusercontent.com/opencraftdev/cc-gateways/main/install.sh | sh
cc-gateway setup
```

### Install channels

| Channel | Command | Notes |
|---------|---------|-------|
| Install script (primary) | `curl -fsSL .../install.sh \| sh` | Detects OS/arch, downloads a prebuilt binary from GitHub Releases, verifies its sha256, puts it in `~/.local/bin` (no sudo), and prints a PATH hint if needed |
| Homebrew | `brew install opencraftdev/tap/cc-gateway` | Tap formula auto-updated on release |
| Cargo | `cargo install cc-gateway` | For Rust users |

- Release binaries come from **cargo-dist** (GitHub Actions): macOS arm64/x64, Linux x64/arm64. cargo-dist also generates `install.sh` and the Homebrew formula, so we don't hand-maintain either.
- `cc-gateway update`: re-runs the install script for the latest release (instead of `axoupdater`; same result, no extra dependency).
- `cc-gateway uninstall`: removes services and the binary. It asks before deleting `~/.cc-gateway`.

### `cc-gateway setup` — the guided wizard

Running `cc-gateway` with no args and no profiles also starts it. Each step checks first and fixes or explains:

1. **Check Claude Code.** `claude --version`. If missing: print `npm i -g @anthropic-ai/claude-code` (offer to run it). If not logged in: tell the user to run `claude` once to log in, then continue.
2. **Gateways.** `Enable Telegram? [Y/n]` and `Enable API server (OpenAI-compatible, on 127.0.0.1:8787)? [y/N]`; at least one. If Telegram: **bot token.** Show 3-line BotFather instructions (`/newbot` in @BotFather, copy the token). The user pastes the token, it's validated live via `getMe`, and the wizard shows `✅ Connected to @your_bot`. Bad tokens are re-asked, not crashed on.
3. **Profile name.** Default `default`. Enter to accept.
4. **Pair yourself (no user-id hunting)** (Telegram only; if the API is on, a key named `default` is created and printed once here). The wizard says *"Open t.me/your_bot and press Start."* The bot temporarily accepts the **first** `/start` (2-minute window) and shows `Is this you? @username (Muhammad, id 123456789) [y/N]` (default No, so pressing Enter never allowlists a stranger). On an explicit yes, that id goes on the allowlist and the bot replies "Paired ✅".
5. **Model.** Pick `sonnet` / `opus` / `haiku` with arrow keys (default sonnet).
6. **Voice (optional).** If `ffmpeg`/`whisper-cli` are missing: "Enable voice notes? Runs `brew install ffmpeg whisper-cpp` + downloads base model (~150MB) [y/N]". Skippable; the wizard shows a progress bar for the model download.
7. **Run in background?** `[Y/n]`. Yes installs the launchd/systemd service and starts it. No prints `cc-gateway run default`.
8. **Done screen:** bot link, profile path, and the 4 most useful commands.

Every prompt has a sane default, so pressing Enter all the way through works (except the pairing confirmation, which needs an explicit `y`). There's also a non-interactive mode for scripts:
`cc-gateway setup --token $TOKEN --profile work --model sonnet --service --no-voice` (prints a pair code instead of the 2-min window). Add `--api` to also enable the API (creates key `default`); `--api` without `--token` makes an API-only profile.

### Hermes import

If a [Hermes Agent](https://github.com/NousResearch/hermes-agent) install is found (`$HERMES_HOME`, else `~/.hermes`), `setup` offers to import it right after the Claude Code check: it lists what it found (counts and yes/no flags only, never secret values) and asks `Hermes Agent found at ~/.hermes. Import it? [Y/n]`. Hermes profiles (`~/.hermes/profiles/<name>/`, a dir with `config.yaml`, `.env`, `SOUL.md`, `profile.yaml`, `auth.json` or `state.db`) are offered too (multi-select, **none selected by default**); the root becomes `default` (or a name you pick), others keep their name. Non-interactive: `setup --import-hermes --only <name>…` / `--no-import-hermes` (`--token` overrides the root's token). Standalone: `cc-gateway import hermes [--only <name>]... [--profile <name>] [--hermes-home <path>] [--yes]`. Without a terminal or with `--yes`, nothing is imported unless `--only` picks profiles (`root` = top level), since every imported bot starts being polled by cc-gateway; a `--hermes-home` pointing at `profiles/<name>` counts as picking that profile and keeps its name. Nothing in the Hermes home is written.

| Hermes | cc-gateway |
|--------|-------------|
| `.env` `TELEGRAM_BOT_TOKEN` | `bot_token` (validated with `getMe`; if invalid/missing: ask, or skip non-interactively) |
| `.env` `TELEGRAM_ALLOWED_USERS`, `GATEWAY_ALLOWED_USERS` (comma-separated ids), approved pairings (`pairing/telegram-approved.json` if that legacy dir has content, else `platforms/pairing/telegram-approved.json`), plus the owner | merged into `allowed_user_ids`. If Hermes allows everyone (`TELEGRAM_ALLOW_ALL_USERS` / `GATEWAY_ALLOW_ALL_USERS` true, or `*` in a list) only the owner is imported, with a warning |
| owner: `config.yaml` `platforms.telegram.home_channel.user_id`, else a private (positive) `TELEGRAM_HOME_CHANNEL`, else the single allowed user | owner of imported cron jobs (memory is shared, see below) |
| home chat: `home_channel.chat_id`, else `TELEGRAM_HOME_CHANNEL`, else the owner's DM | `chat_id` of imported cron jobs |
| `cron/jobs.json` | owner's `schedules.json` (see Scheduling); `scripts/` → `workspace/scripts/` when script jobs are imported |
| `SOUL.md` | `SOUL.md` |
| `memories/MEMORY.md`, `memories/USER.md` (profile-wide in Hermes) | `<profile>/MEMORY.md`, `<profile>/USER.md` (shared memory, injected for every user) |
| `state.db` (sessions + messages, opened read-only/immutable) | `users/<chat_id>/HISTORY.md`: last 30 user/assistant text messages of each allowlisted Telegram chat (DMs of allowed users, the home chat), long ones trimmed |
| `terminal.cwd` / `TERMINAL_CWD` (absolute, existing) | `workdir` |
| `config.yaml` `model.default` (or `model: <id>`) | `default_model` only if clearly Claude (→ `opus`/`sonnet`/`haiku`), else our default is kept and the summary says so |
| `skills/**/<skill>/SKILL.md` (category folders, symlinks followed, `.hub` skipped) | copied **flat** to `workspace/.claude/skills/<skill>/` (Claude Code only discovers `.claude/skills/<name>/SKILL.md`, not nested category folders; verified with claude 2.1.x) |

- Profiles with only a Discord bot (`DISCORD_BOT_TOKEN`, no `TELEGRAM_BOT_TOKEN`) are reported "Discord-only, skipped (Discord isn't supported)". Profiles with no bot token at all are asked for one (wizard) or skipped (non-interactive).
- Partial installs are fine: missing items are reported as skipped. Existing cc-gateway files with content are never overwritten without confirmation (`--yes` overrides). A summary table of imported/skipped items is printed per profile.
- **Token conflict:** two pollers on one token make Telegram answer `409 Conflict`. When the token came from Hermes the importer warns and suggests `systemctl --user disable --now hermes-gateway-<name>.service` (or `hermes -p <name> gateway stop`; `hermes gateway stop` for the root) or a new bot. At runtime a 409 from `getUpdates` is logged per profile, naming the likely Hermes unit; polling retries with backoff and other profiles are unaffected.

### Adding other users later: pair codes

```
cc-gateway pair <profile>        # prints: Send this to your bot within 10 min:  /pair 482913
```
The friend sends `/pair 482913` and is added to the allowlist. Codes are single-use, expire after 10 min, and 5 wrong attempts per user lock that user out. `allow <profile> <user_id>` still works for people who know their id.

### Friendly errors

- Every error names **what failed + exact fix command**, e.g. `✗ claude not logged in → run: claude` or `✗ token rejected by Telegram → run: cc-gateway profile edit default`.
- `cc-gateway doctor` re-runs all setup checks (claude, login, token, service running, ffmpeg, whisper, disk perms) as a ✅/✗ checklist.
- Crates for the wizard: `dialoguer` (prompts/select), `indicatif` (progress), `console` (colors). The single binary stays ~10MB.

## Directory layout

```
~/.cc-gateway/
└── profiles/
    └── <profile>/
        ├── config.toml          # token, allowlist, model, limits (chmod 600)
        ├── SOUL.md              # shared personality for this bot
        ├── MEMORY.md            # shared memory: work, owner, environment (Claude-edited, 2200 chars)
        ├── USER.md              # shared profile of the owner/user (Claude-edited, 1375 chars)
        ├── state.json           # chat_id -> { session_id, model, … }
        ├── sessions.json        # session_id -> owner (tg:<chat> | key:<name>)
        ├── logs/bot.log         # gateway log (service.log: launchd stdout/stderr)
        ├── users/
        │   ├── <telegram_user_id>/
        │   │   ├── MEMORY.md    # notes about this person (Claude-edited)
        │   │   ├── USER.md      # this person's preferences (Claude-edited)
        │   │   ├── HISTORY.md   # imported chat history (chat id = user id for DMs)
        │   │   └── schedules.json
        │   └── key-<api_key_name>/   # API key memory namespace (MEMORY.md, USER.md)
        └── workspace/           # cwd for every claude run (unless `workdir` is set)
            ├── inbox/<user_id>/      # downloaded photos/docs/voice transcripts
            ├── inbox/key-<name>/     # images sent to the API as data: URLs
            ├── outbox/<chat_id>/     # files Claude wants sent back (Telegram)
            └── .claude/skills/       # skills (e.g. imported from Hermes)
```

One cwd per profile matters because Claude Code keys sessions by project dir. `--resume` only works when the cwd stays the same.

## CLI

```
cc-gateway setup                      # guided wizard (see Installation); also runs on first launch
cc-gateway pair <name>                # one-time code to add another user
cc-gateway update | uninstall
cc-gateway profile create <name>      # same as setup steps 2-7 for an extra profile
cc-gateway profile list
cc-gateway profile edit <name>        # opens config.toml in $EDITOR
cc-gateway profile delete <name>      # confirms; deletes the dir
cc-gateway allow <name> <user_id>     # add to allowlist
cc-gateway deny  <name> <user_id>
cc-gateway run <name>                 # foreground; starts the enabled gateways
cc-gateway run --all                  # every profile in one process (shared run cap)
cc-gateway key create <profile> <name>  # API key, printed once
cc-gateway key list <profile>
cc-gateway key revoke <profile> <name>
cc-gateway service install <name>|--all   # launchd/systemd, auto-restart
cc-gateway service uninstall <name>|--all
cc-gateway service status <name>|--all
cc-gateway doctor                     # checks claude, ffmpeg, whisper-cli, token validity
cc-gateway import hermes [--only <name>]... [--profile <name>] [--hermes-home <path>] [--yes]
```

Service units: launchd label `dev.opencraft.cc-gateway.<profile>` (`~/Library/LaunchAgents`), systemd template `cc-gateway@.service` with one instance `cc-gateway@<profile>` per profile. `--all`: launchd `dev.opencraft.cc-gateway.all` / systemd `cc-gateway-all.service`, logs in `~/.cc-gateway/logs/`. Installing `--all` is refused while per-profile units exist (and vice versa), since both would poll the same bots. The profile name `all` is reserved.

`run --all` starts every profile that has a gateway enabled; each keeps its own state, queues, locks and session owners. A profile that fails (bad token, port in use) is logged and the others keep running; a profile with the same Telegram token as an earlier one is skipped. The process-wide claude cap is the smallest `max_concurrent_runs` among them. Logs go to one file, `~/.cc-gateway/logs/bot.log`; lines carry the profile name where it is known.

`profile create` checks the token with `getMe`, writes a default `SOUL.md`, and prints the bot's @username. Tip shown to the user: send `/start` to the bot and copy your user id from the "not allowed" log line, or use @userinfobot.

### config.toml

```toml
bot_token = "123456:ABC..."       # Telegram gateway on when set ("" = off)
allowed_user_ids = [123456789]
default_model = "sonnet"          # passed as --model
max_turns = 30                    # --max-turns
run_timeout_secs = 900            # kill claude after this
claude_bin = "claude"             # path override
extra_claude_args = []            # appended verbatim
max_concurrent_runs = 2           # process-wide cap on concurrent claude runs (run --all: smallest wins)

[voice]
enabled = false                   # setup turns it on when ffmpeg + whisper-cli + model exist
whisper_bin = "whisper-cli"
whisper_model = "~/.cc-gateway/models/ggml-base.bin"
language = "auto"

[api]
enabled = false
bind = "127.0.0.1:8787"           # non-loopback → startup warning
max_concurrent = 4                # queued + running API requests; more → 429

[[api.keys]]                      # written by `cc-gateway key create`
name = "laptop"
sha256 = "<hex sha256 of the key>"
created_at = "2026-11-06T10:00:00+07:00"
```

## API gateway

Served by axum on `[api] bind`. Auth runs as middleware before any request body is read. Request bodies up to 20 MB. All errors (including 404 unknown endpoint, 405, 413) use the OpenAI shape `{"error": {"message", "type", "code": null}}`. The `Bearer` scheme is case-insensitive.

| Endpoint | Auth | Purpose |
|----------|------|---------|
| `GET /health` | no | `{"ok": true, "version": "…"}` |
| `GET /v1/models` | yes | `opus`, `sonnet`, `haiku` (+ the profile's `default_model` if it's a full id); any id `claude --model` accepts works |
| `POST /v1/chat/completions` | yes | OpenAI chat completions, `stream` true or false |
| `POST /v1/runs` | yes | Native: `{prompt, session_id?, model?, stream?}` |
| `DELETE /v1/runs/{id}` | yes | Stop a run started by the same key (process-group kill) |

- **Auth:** `Authorization: Bearer ccg_…` (43 base62 chars ≈ 256 bits). Missing/unknown/revoked → 401 `authentication_error`. Config is re-read per request, so `key create`/`revoke` apply without a restart.
- **Chat mapping (stateless, OpenAI semantics):** `system`/`developer` messages are appended to the system prompt (after SOUL + the key's USER/MEMORY). One user message is sent as-is; a longer conversation is rendered as a transcript ending with "Reply to the last User message." Content parts: `text`, and `image_url` with a base64 `data:` URL (saved to `workspace/inbox/key-<name>/`, referenced by path); http(s) URLs → 400. Unknown parameters (temperature, …) are ignored.
- **Sessions (optional):** request header `X-CC-Session: <id>` → `--resume <id>` and only the last user message is sent (only its images are saved). Responses of runs that started carry `X-CC-Session` (streaming included) and `X-CC-Run`. A session id not created by this key (unknown, another key's, or a Telegram chat's) → 404 `session_not_found`. Saved request images are deleted when the run ends.
- **Responses:** non-stream: `chat.completion` with `usage {prompt_tokens, completion_tokens, total_tokens, total_cost_usd}` (tokens as reported by claude, else 0). Stream: SSE `chat.completion.chunk`s (first delta has `role`, text deltas as claude emits text blocks, last chunk has `finish_reason: "stop"` + `usage`), then `data: [DONE]`. A failure mid-stream is sent as `data: {"error": …}` before `[DONE]`.
- **Native runs:** non-stream → `{run_id, text, session_id, cost_usd, duration_ms, is_error}`. Stream → SSE events `init {run_id, session_id}`, `tool {name, input}`, `text {text}`, `result {…same as JSON}`, `error {type, message}`.
- **Status codes:** 400 invalid request, 401 auth, 404 unknown session/run, 409 stopped, 413 body too large, 429 over `max_concurrent`, 500 claude failed to run, 502 claude returned an error, 504 timeout (`run_timeout_secs`).
- **Concurrency:** queue key `session:<id>` (same session serialized); requests without a session get their own queue (parallel). Global semaphore `max_concurrent` covers queued + running requests and is taken before anything is written to disk. A client that disconnects stops its run.

## Core flow (one message)

1. **Receive** an update via long polling (`teloxide`). Drop it if `from.id` is not in the allowlist.
2. **Normalize the input** into a prompt string:
   - text: as-is
   - photo: download the largest size to `inbox/<uid>/<ts>.jpg`, prompt = caption + `\n[Attached image: <abs path>]`
   - document: download to `inbox/<uid>/<filename>`, prompt = caption + `[Attached file: <abs path>]`
   - voice: download `.oga`, `ffmpeg -i in.oga -ar 16000 -ac 1 out.wav`, `whisper-cli -m <model> -f out.wav -otxt`, prompt = `[Voice transcript]: <text>`. Reply with the transcript first so the user can see what was heard.
3. **Enqueue** on the chat's queue (`HashMap<ChatId, mpsc::Sender<Job>>`, one worker task per chat).
4. **Build the system prompt** fresh on every run (so memory edits apply next turn), appended to Claude Code's own prompt (`--append-system-prompt`), in Hermes' order:
   ```
   # Identity            — "SOUL.md defines who you are … overrides any default assistant identity"
   <SOUL.md>
   # Platform            — Telegram hint + "talking with <Full Name> (@username)" + outbox path
   # Project context     — first of .hermes.md / HERMES.md / AGENTS.md in the working dir (20k cap)
   # Memory              — Hermes' memory guidance: shared vs per-person, declarative facts,
                           `§` entry format, prune to the limits, no secrets
   ══ MEMORY (shared notes for this bot) [pct — n/2200 chars] — file: <profile>/MEMORY.md
   ══ USER PROFILE (who the user/owner is) [pct — n/1375 chars] — file: <profile>/USER.md
   ══ THIS PERSON (who is writing now) [… /1375] — file: users/<id>/USER.md
   ══ NOTES ABOUT THIS PERSON [… /1375] — file: users/<id>/MEMORY.md
   # Earlier conversation — users/<chat>/HISTORY.md, only until the chat's first session exists
   Current date: Friday, November 06, 2026 14:03 (timezone UTC+07:00)
   ```
   Verified: with a SOUL like "You are Budi, reply only in Indonesian, sign every message with — Budi", `who are you?` is answered as Budi, in Indonesian, signed.
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
   - A `typing` chat action every 4s from message acceptance through queueing, the run and the reply (upload actions while sending outbox files).
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
| `/memory` | Show the shared MEMORY.md + USER.md and this user's own notes |
| `/forget` | Wipe this user's own MEMORY.md + USER.md (shared memory stays; asks for confirmation via inline button) |
| `/schedule <m h dom mon dow> <prompt>` | Add a cron job |
| `/schedules` | List jobs with ids |
| `/unschedule <id>` | Remove a job |

## Scheduling

- `users/<uid>/schedules.json`: `[{ id, cron, prompt, chat_id, created_at, every_minutes?, command?, cwd?, name?, enabled?, deliver? }]`. 5-field cron, local timezone (the process `TZ`). `every_minutes: N` is due when the minute since the Unix epoch is a multiple of N. `enabled: false` jobs are kept but never run.
- **Command jobs** (`command: [argv…]`, imported from Hermes `no_agent` script jobs): run directly, no shell and no claude, in `workspace/<cwd>` in their own process group, killed after `run_timeout_secs`; stdout/stderr capped at 1 MB each; a job still running when it comes due again is skipped (logged). Output like Hermes: stdout posted to `chat_id`; empty stdout or a last line `{"wakeAgent": false}` → silent; failure → `⚠ <name> failed` + output tail. `deliver: false` runs silently (failures logged). Command jobs don't take a claude run slot.
- Use the `cron` crate. Every minute a single tokio task checks all jobs across users.
- A due prompt job goes onto **that chat's queue** like a normal message, but runs **without `--resume`** (a fresh session, so cron runs don't pollute the chat context). Memory + soul still get injected. The output is posted to `chat_id`, prefixed `⏰ <prompt snippet>`.
- Missed runs while the bot was down are skipped, not backfilled.

## Rust crates

`tokio` (full), `teloxide` (macros), `serde`/`serde_json`, `toml`, `clap` (derive), `cron`, `chrono`, `tracing` + `tracing-appender`, `anyhow`, `dirs`; for the API: `axum`, `sha2`, `tokio-stream` (SSE body); for the Hermes history import: `rusqlite` (`bundled`, cross-compiles with cargo-zigbuild). Nothing else unless a need shows up.

Module sketch:
```
src/main.rs        # clap CLI dispatch
src/profile.rs     # create/list/edit/delete, config load/save
src/bot.rs         # Telegram gateway: teloxide dispatcher, allowlist filter, commands
src/api.rs         # API gateway: axum routes, keys, OpenAI mapping, SSE
src/hermes.rs      # Hermes Agent import
src/setup.rs       # wizard, doctor, keys, pair, update/uninstall
src/media.rs       # downloads, ffmpeg + whisper
src/runner.rs      # spawn claude, parse stream-json, timeout, kill
src/queue.rs       # per-conversation workers (tg:<chat>, session:<id>), stop handles
src/prompt.rs      # system prompt assembly from SOUL/USER/MEMORY
src/schedule.rs    # cron loop
src/service.rs     # launchd plist / systemd unit
src/state.rs       # state.json atomic persistence
```

## Acceptance criteria (v1)

- [ ] On a fresh Mac with Claude Code logged in: `install.sh` + `cc-gateway setup` pressing only Enter + Telegram Start leads to a working bot in under 3 minutes
- [ ] Setup never asks the user to type their Telegram user id
- [ ] A friend is added via `cc-gateway pair` + `/pair <code>` with no config editing
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
- [ ] An existing `~/.cc-telegram` is moved to `~/.cc-gateway` on first run, with a one-line notice; old paths in configs are rewritten and old cc-telegram launchd/systemd services are removed
- [ ] An API-only profile (`setup --api`, no token) runs `cc-gateway run <profile>` without Telegram
- [ ] The OpenAI Python/JS SDKs work against `http://127.0.0.1:8787/v1` with a `ccg_` key, streaming and non-streaming
- [ ] Missing/bad/revoked keys get 401 in OpenAI error shape; over `max_concurrent` gets 429
- [ ] `X-CC-Session` continues a conversation; every response returns the session id
- [ ] `DELETE /v1/runs/{id}` stops a running native run; binding to a non-loopback address prints a warning
- [ ] Importing a real Hermes server profile allowlists approved pairings + the home-channel owner without a `TELEGRAM_ALLOWED_USERS`; allow-all imports the owner only; Discord-only profiles are skipped with a clear line
- [ ] Hermes cron jobs arrive as schedules (script jobs as command jobs with their scripts copied; paused ones disabled); re-importing doesn't duplicate them
- [ ] With `max_concurrent_runs = 2`, a third message waits and shows `⏳ queued (n ahead)`
- [ ] An imported Hermes profile answers as its SOUL, knows the shared Hermes memory for every user, and the first chat after the move continues from the imported history
- [ ] `run --all` serves every profile in one process; one profile's 409 or bad token doesn't stop the others; `service install --all` refuses while per-profile units exist

## Out of scope (v1)

- Group chats / @mention mode
- Spend budgets (just display the cost)
- Remote/cloud hosting, webhooks (long polling only)
- Multiple workspaces per profile (`/cd`). Possible v2: `cc-gateway workspace add`
- Windows service support
