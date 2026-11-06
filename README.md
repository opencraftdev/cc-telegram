# cc-gateway

**Use Claude Code on your own machine from Telegram, or from any OpenAI-compatible client.**

`cc-gateway` is one Rust binary that sits in front of `claude -p` (Claude Code in headless mode). Each profile can run two gateways:

- **Telegram**: a private bot with live tool progress, per-chat sessions, per-user memory, photos/files/voice notes, and cron schedules.
- **API**: an OpenAI-compatible HTTP server (`/v1/chat/completions`, `/v1/models`) plus a native runs API with streaming tool events.

It runs on **your Claude subscription** through your local Claude Code login. You don't need an Anthropic API key.

> ⚠️ Claude runs with full tool access (`--dangerously-skip-permissions`). Anyone on the Telegram allowlist and anyone holding an API key can run commands as your OS user. Read [docs/security.md](docs/security.md).

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/opencraftdev/cc-gateways/main/install.sh | sh
cc-gateway setup
```

Other channels: `brew install opencraftdev/tap/cc-gateway` or `cargo install cc-gateway`.

Requirements: [Claude Code](https://docs.anthropic.com/en/docs/claude-code), installed and logged in (`npm i -g @anthropic-ai/claude-code`, then run `claude` once). macOS or Linux.

## Quickstart: Telegram

1. Run `cc-gateway setup` and answer **Yes** to "Enable Telegram?".
2. Paste a bot token from @BotFather.
3. Open your bot and press **Start**. Confirm "Is this you?" with `y`.
4. Choose a model. Say yes to running it in the background.

Then chat with your bot. To add a friend, run `cc-gateway pair <profile>`; they send the printed `/pair 123456` to the bot. See [docs/telegram.md](docs/telegram.md).

## Quickstart: API

```bash
cc-gateway setup --api --profile api      # API-only profile; prints a ccg_ key once
cc-gateway run api                         # or: cc-gateway service install api
```

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CCG_KEY" -H "Content-Type: application/json" \
  -d '{"model":"sonnet","messages":[{"role":"user","content":"Hello"}]}'
```

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="ccg_...")
print(client.chat.completions.create(model="sonnet", messages=[{"role": "user", "content": "Hello"}]).choices[0].message.content)
```

See [docs/api.md](docs/api.md).

## Documentation

- [Getting started](docs/getting-started.md)
- [Telegram gateway](docs/telegram.md): commands, pairing, media, schedules
- [API gateway](docs/api.md): auth, endpoints, OpenAI SDK examples, streaming, sessions, native runs
- [Configuration](docs/configuration.md): full `config.toml` reference
- [Profiles and background service](docs/profiles-and-service.md)
- [Importing from Hermes Agent](docs/hermes-import.md)
- [Security](docs/security.md)
- [Architecture](docs/architecture.md)
- [Troubleshooting](docs/troubleshooting.md)

Formerly `cc-telegram`. An existing `~/.cc-telegram` is moved to `~/.cc-gateway` automatically the first time you run it.
