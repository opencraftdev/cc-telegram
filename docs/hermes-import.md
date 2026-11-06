# Importing from Hermes Agent

If you already use [Hermes Agent](https://github.com/NousResearch/hermes-agent), cc-gateway can take over its Telegram bot, allowlist, persona, memory and skills. The import only reads from the Hermes home; nothing there is written.

## Where it looks

`$HERMES_HOME` if set, otherwise `~/.hermes`. Hermes profiles in `~/.hermes/profiles/<name>/` are offered too. A folder only counts as a profile if it contains `config.yaml`, `.env`, `SOUL.md`, `profile.yaml`, `auth.json` or `state.db`.

## Running it

- **In the wizard:** after the Claude Code check, `cc-gateway setup` lists what it found (counts and yes/no only, never secret values) and asks `Hermes Agent found at ~/.hermes. Import it?`. You then choose which Hermes profiles to import. The root profile becomes `default`, or a name you pick; other profiles keep their names.
- **Non-interactive:** `cc-gateway setup --import-hermes` imports everything. `--token <TOKEN>` overrides the root profile's token. `--no-import-hermes` skips the offer.
- **Standalone:**

```bash
cc-gateway import hermes [--profile <name>] [--hermes-home <path>] [--yes]
```

`--profile` names the cc-gateway profile for the Hermes root profile (default `default`). `--yes` skips every prompt and overwrites existing files.

## What maps where

| Hermes | cc-gateway |
|--------|------------|
| `.env` `TELEGRAM_BOT_TOKEN` | `bot_token`, checked live with Telegram. If invalid or missing, the wizard asks for one; without prompts, the existing profile's token is kept, or that profile is skipped. |
| `.env` `TELEGRAM_ALLOWED_USERS` (comma-separated ids) | Merged into `allowed_user_ids` |
| `SOUL.md` | `SOUL.md` |
| `memories/MEMORY.md`, `memories/USER.md` | `users/<owner>/`. Hermes memory belongs to a single owner: with one allowed id, that id. With several, you pick (without prompts: the first, with a note). With none, the files wait in `imported/` and move to the first user who pairs. |
| `config.yaml` `model.default` (or `model: <id>`) | `default_model`, for new profiles only, and only if it's clearly a Claude model (→ `opus`, `sonnet` or `haiku`). Anything else keeps the cc-gateway default, and the summary says so. |
| `skills/**/<skill>/SKILL.md` | Copied to `workspace/.claude/skills/<skill>/`, so `claude -p` picks them up. Category folders are searched, symlinks are followed (contents are copied), folders without `SKILL.md` and `.hub/` are skipped. |

`.env` parsing handles `export KEY=…`, single or double quotes, and `#` comments.

Partial installs are fine: whatever is missing shows up as "skipped". Files that already have content in cc-gateway are never overwritten without confirmation (`--yes` overrides). An untouched default `SOUL.md` or an empty memory file counts as empty. Each profile ends with an imported/skipped table.

## Stop the Hermes gateway first

Telegram lets only one process poll a bot token. If Hermes keeps running with the same token, both get `409 Conflict`. The importer warns whenever the token came from Hermes. Either stop Hermes:

```bash
hermes gateway stop              # root profile
hermes -p <name> gateway stop    # a Hermes profile
```

or create a new bot with @BotFather and give its token to cc-gateway (`cc-gateway profile edit <profile>`).
