# Configuration

Each profile has one `config.toml`, at `~/.cc-gateway/profiles/<profile>/config.toml`, with file mode 0600. Edit it with `cc-gateway profile edit <profile>`, which opens `$VISUAL` or `$EDITOR` (default `vi`), restores mode 0600 afterwards and validates the file.

Every field has a default, and partial tables are fine (e.g. `[voice]` with only `enabled = true`), so a minimal file is just `bot_token = "…"` (Telegram), or `[api]` with `enabled = true` (API). Only `[[api.keys]]` entries need all three fields; `cc-gateway key create` writes them.

```toml
# ----- Telegram gateway -----
bot_token = "123456:ABC..."        # on when set; "" or missing = Telegram off
allowed_user_ids = [123456789]     # Telegram user ids allowed to use the bot

# ----- Claude -----
default_model = "sonnet"           # passed as --model; opus | sonnet | haiku | full id
max_turns = 30                     # --max-turns
run_timeout_secs = 900             # kill the claude process group after this
claude_bin = "claude"              # path to the claude CLI (~ is expanded)
extra_claude_args = []             # appended verbatim, e.g. ["--add-dir", "/Users/me/code"]
workdir = "/home/me/project"       # optional: claude's working dir (default: the profile workspace);
                                   # the workspace is then added with --add-dir so its skills load
max_concurrent_runs = 2            # cap on concurrent claude processes in this process
                                   # (Telegram + API + cron; with run --all the smallest value wins)

# ----- Voice notes (Telegram) -----
[voice]
enabled = false                    # set by setup when ffmpeg + whisper-cli + model exist
whisper_bin = "whisper-cli"
whisper_model = "~/.cc-gateway/models/ggml-base.bin"
language = "auto"                  # passed to whisper-cli -l

# ----- API gateway -----
[api]
enabled = false
bind = "127.0.0.1:8787"            # non-loopback → loud startup warning
max_concurrent = 4                 # queued + running API requests; more → 429

[[api.keys]]                       # managed by `cc-gateway key create/revoke`
name = "laptop"
sha256 = "<hex sha256 of the key>"
created_at = "2026-11-06T10:00:00+07:00"
```

## When changes apply

| Field | Applies |
|-------|---------|
| `allowed_user_ids`, `api.keys` | Right away. They are re-read on every message or request. |
| `default_model`, `max_turns`, `run_timeout_secs`, `claude_bin`, `extra_claude_args`, `voice.*` | On the next run. |
| `bot_token`, `api.enabled`, `api.bind`, `api.max_concurrent`, `max_concurrent_runs` | After a restart: `cc-gateway service install <profile>` or restart `cc-gateway run`. |

## Other files in a profile

| Path | Contents |
|------|----------|
| `SOUL.md` | Personality / standing instructions, shared by every user and key of the profile |
| `state.json` | Per Telegram chat: session id, model override, last cost, `/new` counter |
| `pair.json` | Active pair codes and failed-attempt counters |
| `sessions.json` | Claude session id → owner (`tg:<chat>` or `key:<name>`); only the owner can resume it |
| `MEMORY.md`, `USER.md` | Shared memory for everyone using the bot ([telegram.md](telegram.md#memory)) |
| `users/<telegram_id>/` | This person's `USER.md`, `MEMORY.md`; `schedules.json` (fields: [telegram.md](telegram.md#schedules)); `HISTORY.md` (imported chat history) |
| `users/key-<name>/` | `USER.md`, `MEMORY.md` for an API key |
| `imported/` | Memory left by imports from older versions; moved to the first user who pairs |
| `logs/bot.log` | Gateway log (`logs/service.log` holds launchd's stdout/stderr) |
| `workspace/` | Working directory for every claude run, unless `workdir` is set |
| `workspace/inbox/`, `workspace/outbox/` | Files received and files to send back |
| `workspace/.claude/skills/` | Claude Code skills available to every run (flat: `<name>/SKILL.md`) |
| `workspace/scripts/` | Scripts for command jobs (e.g. imported from Hermes) |

Log verbosity: set `RUST_LOG`, for example `RUST_LOG=debug cc-gateway run <profile>`. The default is `info`.
