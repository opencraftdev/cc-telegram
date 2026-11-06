# Importing from Hermes Agent

If you already use [Hermes Agent](https://github.com/NousResearch/hermes-agent), cc-gateway can take over its Telegram bot, allowlist, persona, memory and skills. The import only reads from the Hermes home; nothing there is written.

## Where it looks

`$HERMES_HOME` if set, otherwise `~/.hermes`. Hermes profiles in `~/.hermes/profiles/<name>/` are offered too. A folder only counts as a profile if it contains `config.yaml`, `.env`, `SOUL.md`, `profile.yaml`, `auth.json` or `state.db`.

## Running it

Every bot you import stops working in Hermes as soon as cc-gateway polls its token, so profiles are only imported when you pick them.

- **In the wizard:** after the Claude Code check, `cc-gateway setup` lists what it found (counts and yes/no only, never secret values) and asks `Hermes Agent found at ~/.hermes. Import it?`. You then tick the Hermes profiles to import; **none is selected by default**. The root profile becomes `default`, or a name you pick; other profiles keep their names.
- **Non-interactive:** `cc-gateway setup --import-hermes --only <name>` (repeatable). `--token <TOKEN>` overrides the root profile's token. `--no-import-hermes` skips the offer.
- **Standalone:**

```bash
cc-gateway import hermes [--only <name>]... [--profile <name>] [--hermes-home <path>] [--yes]
```

`--only` picks a Hermes profile by name (repeatable; `root` is the top-level `~/.hermes`). Without a terminal, or with `--yes`, **nothing is imported unless `--only` is given**: the profiles are listed and the command prints the `--only` hint. `--hermes-home ~/.hermes/profiles/<name>` points straight at one profile; that counts as picking it, and the cc-gateway profile gets its name. `--profile` names the cc-gateway profile for the Hermes root profile (default `default`). `--yes` skips every prompt and overwrites existing files.

## What maps where

| Hermes | cc-gateway |
|--------|------------|
| `.env` `TELEGRAM_BOT_TOKEN` | `bot_token`, checked live with Telegram (network errors are retried for about a minute; if Telegram stays unreachable the token is imported anyway and marked "not verified — network"). If Telegram rejects it or it's missing, the wizard asks for one; without prompts, the existing profile's token is kept, or that profile is skipped ("no Telegram bot token in Hermes"). |
| `.env` `DISCORD_BOT_TOKEN` without a Telegram token | The profile is skipped: "Discord-only, skipped (Discord isn't supported by cc-gateway)". |
| Who may use the bot (see below) | Merged into `allowed_user_ids` |
| The owner (see below) | Owner of the imported cron jobs |
| `SOUL.md` | `SOUL.md` |
| `memories/MEMORY.md`, `memories/USER.md` | `<profile>/MEMORY.md` and `<profile>/USER.md`: **shared** memory, injected for every user of the bot, exactly like Hermes (where these files are profile-wide). |
| `state.db` (conversation history) | `users/<chat_id>/HISTORY.md` for each allowlisted Telegram chat: the last 30 user/assistant text messages, oldest first, long ones trimmed. It's injected only until that chat's first cc-gateway session is stored, so the first conversation continues from Hermes. The database is opened read-only and immutable; it's never written. |
| `terminal.cwd` (config.yaml) / `TERMINAL_CWD` (.env), if it's an absolute path that exists | `workdir`: claude's working dir. The workspace is added with `--add-dir`, so the imported skills still load. Context files Hermes reads from that dir (`.hermes.md`, `HERMES.md`, `AGENTS.md`) are injected the same way; `CLAUDE.md` is loaded by claude itself. |
| `config.yaml` `model.default` (or `model: <id>`) | `default_model`, for new profiles only, and only if it's clearly a Claude model (→ `opus`, `sonnet` or `haiku`). Anything else, e.g. `gpt-5.6-sol`, keeps the cc-gateway default, and the summary says "(not a Claude model: keeps the cc-gateway default)". |
| `skills/**/<skill>/SKILL.md` | Copied **flat** to `workspace/.claude/skills/<skill>/`. Claude Code only discovers `.claude/skills/<name>/SKILL.md`, not nested category folders, so `skills/apple/notes/` becomes `.claude/skills/notes/`. Symlinks are followed (contents are copied); folders without `SKILL.md` and `.hub/` are skipped. If two categories hold a skill with the same name, the second one is copied as `<category>-<name>` and reported as renamed. |
| `cron/jobs.json` | The owner's `schedules.json`, posted to the home chat (see [Cron jobs](#cron-jobs)) |
| `scripts/` | `workspace/scripts/`, when script jobs are imported |

`.env` parsing handles `export KEY=…`, single or double quotes, and `#` comments. Non-numeric allowlist entries (e.g. usernames) can't be used and are reported as a count.

### Allowlist and owner

The allowlist is derived the way Hermes authorizes Telegram users (`gateway/authz_mixin.py`):

- user ids in `TELEGRAM_ALLOWED_USERS` and `GATEWAY_ALLOWED_USERS`,
- **approved pairings**: `pairing/telegram-approved.json` if that legacy folder has content, else `platforms/pairing/telegram-approved.json`,
- the **owner**: `config.yaml` `platforms.telegram.home_channel.user_id`, else `TELEGRAM_HOME_CHANNEL` if it's a private chat (positive id, which equals the user id), else the single allowed user.

Profiles that Hermes ran with only pairings and a home channel (no `TELEGRAM_ALLOWED_USERS`) therefore keep the same users.

If Hermes was configured to let **everyone** in (`TELEGRAM_ALLOW_ALL_USERS` or `GATEWAY_ALLOW_ALL_USERS` true, or `*` in a list), cc-gateway does **not** copy that. Only the owner is imported, with a warning; add others with `cc-gateway pair <profile>` or `allow`. If there is no owner either, the allowlist stays empty and the warning says to pair.

The home chat for cron output is `home_channel.chat_id`, else `TELEGRAM_HOME_CHANNEL`, else the owner's DM.

### Cron jobs

| Hermes job | cc-gateway schedule |
|------------|---------------------|
| `schedule.kind: cron` (5-field) | `cron` |
| `schedule.kind: interval`, `minutes: N` | `every_minutes: N` (due when the minute since the Unix epoch is a multiple of N, rather than N minutes after the last run) |
| `schedule.kind: once`, or non-standard cron | skipped, with the reason |
| agent job (`prompt`) | prompt job: a fresh claude session, output posted with `⏰` |
| `no_agent: true` + `script` | **command job**: `bash` for `.sh`/`.bash`, otherwise `python3`, on the absolute path `workspace/scripts/<script>`. It runs from the job's `workdir` if that folder exists, otherwise from the script's folder. No shell, no claude. |
| `enabled: false`, `paused_at` set, or `repeat` limit reached | imported with `enabled: false` |
| `deliver: telegram` | posted to the home chat |
| `deliver: origin` | posted to the Telegram chat the job was created in (its `origin`); without a recorded origin, to the owner's DM; a non-Telegram origin runs silently |
| `deliver: telegram:<chat_id>` | posted to that chat |
| `deliver: local` (or another platform) | `deliver: false`: runs silently |

Command jobs handle output like Hermes (stdout and stderr are capped at 1 MB each; the rest is discarded): stdout is posted; empty stdout or a last line `{"wakeAgent": false}` stays silent; a failure posts `⚠ <name> failed` with the output tail. They time out after `run_timeout_secs` (Hermes' default is 3600 s; raise it if a script needs longer).

Not carried over, but reported as notes: per-job pinned skills (every workspace skill is available), pre-run scripts of agent jobs, `repeat` counts that haven't been reached yet.

Python scripts run with the system `python3`, not Hermes' virtualenv. Install the packages they import, or edit `command` in `users/<owner>/schedules.json`, for example `["/home/me/.hermes/venv/bin/python", "summary.py"]`.

Hermes evaluates cron in `HERMES_TIMEZONE` / `config.yaml` `timezone`, else the machine's local time. cc-gateway always uses the process's local time; if Hermes had a timezone set, the import prints a note. Set `TZ` for the service in that case.

Re-running the import doesn't duplicate jobs: a job with the same name and body is skipped as "already imported".

Partial installs are fine: whatever is missing shows up as "skipped". Files that already have content in cc-gateway are never overwritten without confirmation (`--yes` overrides). An untouched default `SOUL.md` or an empty memory file counts as empty. Each profile ends with an imported/skipped table.

## Stop the Hermes gateway first

Telegram lets only one process poll a bot token. If Hermes keeps running with the same token, both get `409 Conflict`. The importer warns whenever the token came from Hermes and prints the right stop command: on Linux when the unit `~/.config/systemd/user/hermes-gateway-<name>.service` exists,

```bash
systemctl --user stop hermes-gateway-<name>.service && systemctl --user disable hermes-gateway-<name>.service
```

otherwise `hermes -p <name> gateway stop` (or `hermes gateway stop` for the root profile).

or create a new bot with @BotFather and give its token to cc-gateway (`cc-gateway profile edit <profile>`).

If cc-gateway is already running and gets a 409, it logs `✗ [<profile>] Telegram 409 Conflict … hermes-gateway-<profile>.service …` and keeps retrying with backoff; other profiles keep working.

## Migrating a whole Hermes server

```bash
cc-gateway import hermes --only <name> --only <other> --yes   # the profiles you're moving
cc-gateway doctor
systemctl --user disable --now hermes-gateway-<name>.service   # for each migrated profile
cc-gateway service install --all       # one process for all profiles
```

`run --all` shares one cap on concurrent claude processes (`max_concurrent_runs`, default 2) across all profiles; see [profiles-and-service.md](profiles-and-service.md).
