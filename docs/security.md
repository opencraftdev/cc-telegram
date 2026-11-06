# Security

## Threat model in one sentence

**Anyone who can talk to cc-gateway can run any command as your OS user.** Claude Code runs with `--dangerously-skip-permissions` (shell, file edits, web), with the profile workspace as its working directory, but nothing confines it to that folder.

So treat these as equivalent to a shell on your machine:

- every Telegram user id on a profile's allowlist,
- every API key,
- anyone who can read `config.toml` (it holds the bot token).

Only give access to people you would hand your laptop to.

## Telegram

- Messages from users not on the allowlist are dropped **before** anything happens: no file download, no claude run, no reply. Each drop is logged with the user id and username.
- Pair codes are 6 digits, single-use, and expire after 10 minutes. 5 wrong attempts lock that Telegram user out of pairing.
- The setup pairing window accepts only the first `/start` within 2 minutes, and only after you confirm the shown username, name and id in the terminal. The default answer is **No**.
- The bot token lives in `config.toml` (mode 0600) and is never logged. teloxide strips it from network errors.

## API

- Keys are `ccg_` plus 43 base62 characters (~256 bits) from `/dev/urandom`. Only their sha256 is stored, and the comparison is constant-time. A key is shown once, at creation.
- Revoking (`cc-gateway key revoke`) applies on the next request.
- **Bind to loopback.** The default is `127.0.0.1:8787`. Any other address prints a warning at startup, because a leaked key then means remote code execution from anywhere that can reach the port.
- `image_url` must be a base64 `data:` URL. The server never fetches remote URLs (no SSRF).
- Request bodies are capped at 20 MB, and `max_concurrent` caps queued + running requests.
- API keys don't grant Telegram access, and Telegram users can't use the API. Every Claude session is recorded with its owner (`sessions.json`): a key can only resume sessions it created itself, never another key's or a Telegram chat's.
- The key is checked in middleware before the request body is read, so unauthenticated clients can't make the server buffer large bodies.

### Remote access

If you need the API from another machine, keep `bind = "127.0.0.1:8787"` and put an **authenticated** tunnel in front of it, for example Tailscale (only your tailnet can reach it), an SSH tunnel (`ssh -L 8787:127.0.0.1:8787 you@host`), or Cloudflare Tunnel with Cloudflare Access in front.

⚠️ A public tunnel without its own authentication layer leaves your machine one leaked `ccg_` key away from being taken over. Don't publish the port directly.

## Memory isolation

Each Telegram user (`users/<id>/`) and each API key (`users/key-<name>/`) gets its own USER.md and MEMORY.md. This prevents **accidental** mixing: Claude is only told about the current user's files. It is **not** a security boundary. Because Claude has full file access, any allowlisted user or key holder can ask it to read `config.toml` (the bot token and key hashes), other users' memory files, or anything else your OS user can read.

## Environment

Claude inherits the environment of the cc-gateway process. Keep secrets out of it. The launchd/systemd units only set `PATH` (and `HOME` on macOS) on top of the system defaults. Nothing secret is put into prompts.

## Processes

Each claude run is started with an argument list (no shell) in its own process group. `/stop`, `DELETE /v1/runs/{id}`, timeouts, client disconnects and service shutdown kill the whole group, so child processes go down with it.
