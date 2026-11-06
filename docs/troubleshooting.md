# Troubleshooting

Start with:

```bash
cc-gateway doctor
```

It checks claude (installed, logged in), `~/.cc-gateway` permissions, and for every profile: config readability and mode 0600, that a gateway is enabled, the Telegram token and allowlist, the API bind and keys, the service, and voice dependencies. Every ✗ comes with the command that fixes it.

Logs: `~/.cc-gateway/profiles/<profile>/logs/bot.log`. On macOS, `logs/service.log` also has the service's stdout/stderr. Use `RUST_LOG=debug cc-gateway run <profile>` for more detail.

## Telegram 409 Conflict

```
✗ [<profile>] Telegram 409 Conflict: another process is polling this bot token (retrying with backoff). If it's Hermes Agent, its unit is likely hermes-gateway-<profile>.service …
```

Two processes are polling the same bot token. cc-gateway keeps retrying with backoff (up to about a minute between tries), and other profiles keep working. Common causes:

- A **Hermes Agent** gateway still runs with the same token: `systemctl --user disable --now hermes-gateway-<name>.service`, or `hermes -p <name> gateway stop` (`hermes gateway stop` for the root profile).
- Both `cc-gateway service install --all` and a per-profile service: cc-gateway refuses to install one while the other exists; remove the extra one with `cc-gateway service uninstall <profile>` or `--all`.
- `cc-gateway run <profile>` is running in a terminal while the service runs too: stop one (`cc-gateway service uninstall <profile>` or Ctrl-C).
- Two cc-gateway profiles share one token: give each its own bot (`cc-gateway profile edit <profile>`).

## claude not found / not logged in

```
✗ claude not found → run: npm i -g @anthropic-ai/claude-code
✗ claude not logged in → run: claude
```

Install Claude Code, run `claude` once to log in, then retry. If claude works in your terminal but the **service** can't find it, the service has an old `PATH`: run `cc-gateway service install <profile>` again from a shell where `which claude` works, or set `claude_bin` to the full path (`cc-gateway profile edit <profile>`).

Telegram answers `✗ claude failed: … → run: cc-gateway doctor`; the API returns 500 `server_error` with the same text.

## Can't reach Telegram (network)

```
✗ can't reach Telegram (network): …
```

The machine couldn't connect to api.telegram.org; this is different from a bad token. Setup, import and doctor retry 8 times over about a minute; the running bot retries forever (backing off to 5 minutes) instead of exiting, and long polling rides out network errors on its own. cc-gateway also tries Telegram's fallback addresses (149.154.166.110, 149.154.167.220) after the DNS answer, like Hermes. The import keeps a Hermes token it couldn't verify ("not verified — network").

## Token rejected

```
✗ token rejected by Telegram (…) → run: cc-gateway profile edit <profile>
```

Paste the current token from @BotFather into `bot_token`. If you revoked it in BotFather, you need the new one.

## The bot ignores me

You're not on the allowlist. Check `logs/bot.log` for `not allowed: dropped message` with your user id, then run `cc-gateway allow <profile> <user_id>`, or `cc-gateway pair <profile>` and send the code. After 5 wrong `/pair` codes you're locked out of pairing; use `allow`.

## API: 401 / 429 / 404 session_not_found

- **401 `authentication_error`**: send `Authorization: Bearer ccg_…`. Check that the key still exists: `cc-gateway key list <profile>`.
- **429 `rate_limit_error`**: more than `[api] max_concurrent` requests are queued or running. Retry later or raise the limit (applies on restart).
- **404 `session_not_found`**: the `X-CC-Session` / `session_id` wasn't created by this API key (sessions belong to the key that started them; Telegram sessions can't be resumed over the API), or Claude Code no longer has it. Drop the header to start a new session.
- **Connection refused**: is the API enabled (`[api] enabled = true`) and the profile running? Check `cc-gateway service status <profile>`. If the port is taken, the log says `could not bind API to …`.

## "⏳ queued (n ahead)"

All `max_concurrent_runs` claude slots (default 2, shared by every profile in the process) are busy. The job starts when one frees up. Raise the value in `config.toml` if the machine has the RAM: each claude process uses roughly 200-400 MB.

## Command job failed

`⚠ <name> failed` followed by output: an imported Hermes script exited non-zero, timed out (`run_timeout_secs`) or couldn't start. Python scripts run with the system `python3`; install the packages they need, or point `command` in `users/<owner>/schedules.json` at another interpreter.

## "started a new session"

The saved session for a chat no longer exists in Claude Code, so the bot started a fresh one. Memory files are unaffected.

## Timed out

`⏱️ timed out after 900s` (or API 504): the run took longer than `run_timeout_secs`. Raise it in `config.toml`. The chat session is kept.

## Voice notes disabled

`🎤 Voice notes are disabled…`: install `ffmpeg` and whisper.cpp (`brew install ffmpeg whisper-cpp`), download the model to `voice.whisper_model`, and set `[voice] enabled = true`. `cc-gateway doctor` shows which piece is missing.

## Moved from cc-telegram

`ℹ moved ~/.cc-telegram → ~/.cc-gateway …`: this is a one-time migration. Old cc-telegram services are removed automatically (`ℹ removed old launchd agent …`); reinstall with `cc-gateway service install <profile>`. See [profiles-and-service.md](profiles-and-service.md#migrating-from-cc-telegram).
