# Profiles and the background service

## Profiles

A profile is a complete, independent instance. It has its own config (Telegram token, allowlist, API keys, model, limits), `SOUL.md`, memory, sessions and workspace, all under `~/.cc-gateway/profiles/<profile>/`. Profiles don't share any state, so two bots with two tokens can run side by side.

```bash
cc-gateway profile create <name>     # wizard: gateways, token, pairing, model, voice, service
cc-gateway profile list              # name, running/stopped, path
cc-gateway profile edit <name>       # open config.toml in $EDITOR
cc-gateway profile delete <name>     # asks first; removes its service and directory
```

Names may contain letters, digits, `-` and `_`.

### Gateways in a profile

| Gateway | Enabled when |
|---------|--------------|
| Telegram | `bot_token` is set |
| API | `[api] enabled = true` |

`cc-gateway run <profile>` starts every enabled gateway in one process. They share the claude job queues. If neither is enabled, it exits with the fix.

### Allowlist and keys

```bash
cc-gateway pair <profile>                 # one-time /pair code for Telegram
cc-gateway allow <profile> <user_id>
cc-gateway deny  <profile> <user_id>
cc-gateway key create <profile> <name>    # API key, printed once
cc-gateway key list <profile>
cc-gateway key revoke <profile> <name>
```

## Running

Foreground, with logs on the terminal and in `logs/bot.log`:

```bash
cc-gateway run <profile>
```

Background, restarted automatically and started at login:

```bash
cc-gateway service install <profile>      # install (or reinstall/restart) and start
cc-gateway service status <profile>
cc-gateway service uninstall <profile>
```

| OS | What gets installed |
|----|---------------------|
| macOS | launchd agent `~/Library/LaunchAgents/dev.opencraft.cc-gateway.<profile>.plist` (`RunAtLoad`, `KeepAlive`). stdout/stderr go to `logs/service.log`. |
| Linux | systemd user template `~/.config/systemd/user/cc-gateway@.service`, one instance `cc-gateway@<profile>.service` per profile (`Restart=always`). |

The service gets the `PATH` you had when you ran `service install`, so it can find `claude`, `ffmpeg` and `whisper-cli`. If you install claude somewhere new later, run `service install` again.

On Linux, user services stop at logout unless lingering is on: `loginctl enable-linger $USER`.

When the service stops (SIGTERM from launchd or systemd, or Ctrl-C in the foreground), cc-gateway kills every running claude process group before exiting, so no orphaned agents keep running.

## Update and uninstall

```bash
cc-gateway update       # re-runs the install script for the latest release
cc-gateway uninstall    # removes all services and the binary; asks before deleting ~/.cc-gateway
```

After `update`, run `cc-gateway service install <profile>` so running services pick up the new binary. If you installed with Homebrew, use `brew upgrade` / `brew uninstall cc-gateway`.

## Migrating from cc-telegram

Every `cc-gateway` command checks for cc-telegram leftovers and prints a line for each fix:

1. `~/.cc-telegram` is moved to `~/.cc-gateway`, once, and only if `~/.cc-gateway` doesn't exist yet.
2. Old paths in profile configs, both `~/.cc-telegram/…` and the absolute `/Users/you/.cc-telegram/…` form, are rewritten to `.cc-gateway`. This is retried on every run while any remain.
3. Old services are stopped and removed: the launchd agent `dev.opencraft.cc-telegram.<profile>` (`~/Library/LaunchAgents/dev.opencraft.cc-telegram.<profile>.plist`) or the systemd unit `cc-telegram-<profile>.service`.

Then reinstall: `cc-gateway service install <profile>`. If step 3 fails, remove them by hand:

```bash
launchctl bootout gui/$(id -u)/dev.opencraft.cc-telegram.<profile>; rm ~/Library/LaunchAgents/dev.opencraft.cc-telegram.<profile>.plist
systemctl --user disable --now cc-telegram-<profile>.service; rm ~/.config/systemd/user/cc-telegram-<profile>.service
```

Telegram chat sessions survive the move: Claude Code 2.1.x resumes a session by id from any directory (verified). If an older Claude Code can't find one, the bot replies "started a new session" once; memory files are unaffected.
