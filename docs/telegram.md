# Telegram gateway

The Telegram gateway is on when the profile's `bot_token` is set. It uses long polling, so you don't need a public URL or webhook.

## Who can talk to it

Only Telegram user ids in `allowed_user_ids` can use the bot. The bot drops messages from everyone else before doing anything with them: no download, no claude run, no reply. It logs each drop to `logs/bot.log` with the user's id and username.

Ways to get on the allowlist:

| Way | How |
|-----|-----|
| Setup pairing | During `cc-gateway setup` the bot accepts the first `/start` within 2 minutes. You confirm it in the terminal with `y`. |
| Pair code | `cc-gateway pair <profile>` prints `/pair 123456`. Send it to the bot within 10 minutes. Each code works once. 5 wrong attempts lock that user out. |
| By id | `cc-gateway allow <profile> <user_id>`. Remove with `cc-gateway deny <profile> <user_id>`. |

Allowlist changes apply right away. The bot re-reads `config.toml` for every message.

## Chat commands

| Command | Action |
|---------|--------|
| `/start` | Greeting and your Telegram user id |
| `/pair <code>` | Join the allowlist with a pair code |
| `/new` | Start a new Claude session for this chat (memory files are kept) |
| `/stop` | Kill the running job and clear this chat's queue |
| `/status` | Model, short session id, queued jobs, running time, last run cost |
| `/model [opus\|sonnet\|haiku\|<full-id>]` | Show or set this chat's model |
| `/memory` | Show your MEMORY.md and USER.md |
| `/forget` | Wipe your MEMORY.md and USER.md (asks for confirmation with a button) |
| `/schedule <m h dom mon dow> <prompt>` | Add a cron job |
| `/schedules` | List your jobs with their ids |
| `/unschedule <id>` | Remove a job |

Any other text, including unknown `/commands`, goes to Claude as a prompt.

## How a message runs

1. Each chat has its own FIFO queue, so messages in one chat run one at a time and different chats run in parallel.
2. A status message shows `⏳ working…` and the latest tool steps (`🔧 Bash: ls`). It updates at most every 1.5 s. The bot sends "typing…" every 4 s while Claude works.
3. The answer arrives as a new message, split at Telegram's 4096-character limit. Markdown is converted to MarkdownV2; if Telegram rejects the formatting, the bot sends plain text. The status message ends up as `✅ done · $0.04 · 12s`.
4. The session id is saved per chat, so the next message continues the conversation (`--resume`). If the saved session is gone, the bot starts a new one and tells you.
5. A run that exceeds `run_timeout_secs` is killed and you get `⏱️ timed out after Ns`.

## Memory

All users of a profile share its `SOUL.md`. Each user has their own `users/<id>/USER.md` and `MEMORY.md`, which Claude edits itself; each file is capped at about 2000 characters. Both are injected into the system prompt on every run, so a preference like "reply in Indonesian" survives `/new`.

## Media

| You send | Claude gets |
|----------|-------------|
| Photo | The largest size saved to `workspace/inbox/<uid>/<timestamp>.jpg`. The prompt is your caption plus `[Attached image: <path>]`. |
| Document | Saved to `workspace/inbox/<uid>/<filename>`. The prompt is your caption plus `[Attached file: <path>]`. |
| Voice note | Converted with `ffmpeg` and transcribed by `whisper-cli`. The bot first replies `🎤 <transcript>`; the prompt is `[Voice transcript]: …`. If voice is disabled or a dependency is missing, the bot says so and points to `cc-gateway doctor`. |

**Files back:** anything Claude writes to `workspace/outbox/<chat_id>/` is sent to the chat after the run and then deleted. `.jpg`, `.jpeg`, `.png` and `.webp` go as photos; everything else goes as a document.

## Schedules

```
/schedule */5 * * * * say hi
/schedule 0 9 * * 1-5 summarize my unread GitHub notifications
```

- Standard 5-field cron (minute, hour, day of month, month, day of week) in local time. Weekdays are `0-7` (0 and 7 are Sunday) or names like `mon`. If both day-of-month and day-of-week are restricted, the job runs when either matches, as in standard cron.
- Jobs are stored in `users/<uid>/schedules.json`. Due jobs go onto that chat's queue.
- Scheduled runs use a **fresh session**, not the chat's, so they don't clutter your conversation. Memory and SOUL.md still apply. Output starts with `⏰ <prompt snippet>`.
- Runs missed while the bot was down are skipped, not caught up.

## 409 Conflict

Telegram allows only one process to poll a bot token. If something else, such as a Hermes gateway, uses the same token, the log shows a `409 Conflict` line with the fix. See [troubleshooting.md](troubleshooting.md#telegram-409-conflict).
