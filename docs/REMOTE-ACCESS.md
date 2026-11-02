# Remote access: Claude Code and Codex from your phone (Linux)

This page explains how to make Claude Code and Codex sessions on a Linux computer visible and controllable
from the Claude app, the ChatGPT/Codex app on your phone, and the web. It also covers keeping that
working through crashes and reboots. It describes a setup that was built and tested on Fedora 44 with
GNOME 50 (Claude Code 2.1, Codex CLI 0.166), including the problems hit on the way.

Placeholders: `<you>` is your Linux user name, `my-machine` your computer's name, and
`~/projects/my-project` the folder sessions should start in.

> **Security first.** Remote access means that anyone signed in to your Claude or ChatGPT account can
> run Claude Code or Codex on this computer, with your user's permissions, from anywhere. Use strong
> account security (2FA), and keep the SSH server below bound to localhost.

## What you end up with

| | Claude Code | Codex |
| --- | --- | --- |
| Terminal sessions show up on the phone and web | yes, automatically | yes (same threads) |
| Start a new session on this computer from the phone or web | yes (server mode) | yes |
| Desktop app sees the same sessions live | yes | yes, over a local SSH connection |
| Push notifications when a session needs you | yes | per the ChatGPT app |
| Survives crashes and reboots | systemd service + linger | systemd timer + watchdog + linger |

## Prerequisites

- **The computer must stay awake.** Sessions live on this machine. Check that automatic suspend is off,
  for example `gsettings get org.gnome.settings-daemon.plugins.power sleep-inactive-ac-type` should
  print `'nothing'`. If it never sleeps, you don't need tmux: a session lives as long as its
  terminal or service.
- **User services keep running without a login:** `loginctl enable-linger <you>`. Without this,
  systemd user services only start when you log in and stop when you log out.
- **`~/.local/bin` must be on PATH for non-interactive shells too.** The Codex desktop app's SSH
  connection runs commands without a login shell. For zsh, put this in `~/.zshenv` (for bash,
  `~/.bashrc`):
  ```sh
  case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac
  ```

## Claude Code

### 1. Every interactive session joins Remote Control

Add these to `~/.claude/settings.json`:

```json
{
  "remoteControlAtStartup": true,
  "inputNeededNotifEnabled": true,
  "agentPushNotifEnabled": true
}
```

- `remoteControlAtStartup`: every new interactive `claude` session connects to Remote Control on its
  own. Each one appears in the Claude app and on claude.ai/code under this computer.
- The two notification settings send a push to your phone when a session is waiting for input or an
  agent finishes.
- A session that was already running when you changed the setting joins with `/remote-control`
  (alias `/rc`).

### 2. Start new sessions on this computer from the phone or web (server mode)

A connected terminal session lets you continue that session remotely. Starting a *new* session on
this computer from the phone, the web or the desktop app needs a running Remote Control server:

```sh
claude remote-control --no-create-session-in-dir --remote-control-session-name-prefix my-machine
```

Run it as a systemd user service so it is always there. Use
[`remote-workers/claude-remote-control.service`](../remote-workers/claude-remote-control.service):
`Restart=always`, plus linger, means it comes back after crashes and starts at boot. Pick the folder
new sessions open in with a drop-in, which survives reinstalls:

```sh
systemctl --user edit claude-remote-control.service
# [Service]
# WorkingDirectory=%h/projects/my-project
systemctl --user enable --now claude-remote-control.service
```

Things to know:

- **Error "<name> · <host> is offline. You can turn remote control back on there or choose another
  place to run."** in the app or on the web means the server isn't running. Start the service.
- **A new environment for each server start.** Each start registers a new environment, which is the
  `environment=` part of the claude.ai/code link the server prints. Old links stop pointing at the
  live server.
- **Capacity.** One server takes many sessions (it reports something like `2/32`).
- **Restarting the server ends the sessions running inside it**, including ones you are using from
  the phone. Restart it only when it is really stuck.
- **Short drops are normal.** On a network hiccup the server logs `Reconnecting · retrying in 1.8s`
  and then `[hh:mm:ss] Reconnected after 14s`. It does not repeat its `Connected` status line after
  reconnecting, so a tool that reads the log must treat "Reconnected after" as connected.
- **The log is chatty.** The server writes its live status screen to the journal (tens of thousands
  of lines a day). Read it with `journalctl --user -u claude-remote-control -a`, because `-a` shows
  lines that contain terminal escape codes. A script that parses it should strip ANSI and OSC
  hyperlink escapes, and read incrementally with `--show-cursor` / `--after-cursor`.
- **How tools can tell a terminal session is connected.** Each running session has a record in
  `~/.claude/sessions/<pid>.json`. The record has a `bridgeSessionId` when it is connected, and
  `entrypoint: "cli"` for terminal sessions.

### 3. Desktop app: sessions appear, folder grouping doesn't

Terminal and server sessions show up live in the Claude desktop app and on the web. The desktop
app's "group by folder" doesn't apply to Remote Control sessions: they land under **Other**. You can
make a custom group in the desktop app and move sessions into it by hand. Custom groups are
desktop-only and don't appear on the phone, and there is no setting that sends new sessions into a
group automatically.

## Codex

### 1. The background service with remote control

Codex's remote access is a background app-server (the "daemon") connected to OpenAI's relay. Your
phone sees the computer under its host name.

```sh
codex remote-control start                         # start the daemon with remote control on
codex remote-control pair                          # short-lived pairing code, if the phone asks
codex app-server daemon bootstrap --remote-control # install durable local management (SSH use)
codex app-server daemon restart                    # restart it (interrupts running work)
codex app-server daemon disable-remote-control     # turn remote access off
```

- The setting is kept in `~/.codex/app-server-daemon/settings.json` (`"remoteControlEnabled": true`).
- The daemon listens on a local control socket,
  `~/.codex/app-server-control/app-server-control.sock`: WebSocket at `ws://localhost/rpc`,
  JSON-RPC. Read-only status calls that tools can use:
  - `account/read`, with `{"refreshToken": false}` so nothing gets refreshed;
  - `account/rateLimits/read`;
  - `account/usage/read`;
  - `remoteControl/status/read`.

### 2. One live view in the terminal, the desktop app and the phone

The Codex desktop app has its own built-in app-server. Two problems follow from that:

- **409 "Remote app server already online".** It shares the `~/.codex` identity with the daemon, so
  turning on the desktop app's own remote control while the daemon is online fails with this error.
- **No live updates.** Left on its own, the desktop app shows threads started elsewhere only after
  it restarts.

The fix is to connect the desktop app to the daemon over SSH to this same machine. The app then
runs `codex app-server proxy` over SSH and shows the daemon's threads live: the same threads you
see in the terminal and on the phone.

1. **Run sshd on localhost only**, with keys and no passwords. As root, create
   `/etc/ssh/sshd_config.d/10-localhost-only.conf`:
   ```
   ListenAddress 127.0.0.1
   ListenAddress ::1
   PasswordAuthentication no
   ```
   then `systemctl enable --now sshd`. Check with `ss -ltn` that port 22 listens only on
   `127.0.0.1` and `[::1]`.
2. **Let your own key log in:** add `~/.ssh/id_ed25519.pub` to `~/.ssh/authorized_keys` (create a
   key with `ssh-keygen -t ed25519` if you have none).
3. **Add a host alias** to `~/.ssh/config`:
   ```
   Host my-machine-local
       HostName 127.0.0.1
       User <you>
       IdentityFile ~/.ssh/id_ed25519
       IdentitiesOnly yes
   ```
   Test it: `ssh my-machine-local codex --version` must work without a password, which also proves
   the PATH setup above.
4. In the desktop app, open **Connections → SSH**, add `my-machine-local`, then use **Add remote
   project** to map your folder (for example `~/projects/my-project`).

Notes:

- **Use the app's remote projects, not its local ones.** Its local project list is separate from
  the remote ones. Editing the app's state file to add projects does not stick: the app rewrites it.
- **"Control this PC"** in the app's Connections page is the reverse direction (the app driving
  another machine). It isn't needed here.

### 3. Keep it running: the watchdog

The daemon doesn't restart itself after a crash. Use
[`remote-workers/`](../remote-workers/): a timer runs `codex-remote-control-watchdog` every minute.
Each run does one of three things:

- **The daemon isn't running:** start it.
- **A stray server is in the way:** when the daemon dies while the desktop app's SSH connection is
  open, the app's `codex app-server proxy` can start its own *unmanaged* `app-server --listen
  unix://…` without remote control. That process blocks the daemon from coming back. The watchdog
  stops it (only your own `codex` processes) and runs `bootstrap --remote-control`.
- **The daemon runs but is disconnected from the relay for 3 checks in a row:** restart it.

```sh
systemctl --user enable --now codex-remote-control.timer
```

Lessons from building it:

- **A `Type=oneshot` service's processes die when it finishes.** systemd kills everything the
  service started when it exits, so a daemon launched from the watchdog died with it. Launch it in
  its own scope: `systemd-run --user --scope --quiet --collect -- codex ...`.
- **Avoid `RemainAfterExit=yes` on a unit a timer re-runs.** A unit left "active (exited)" is
  skipped by the timer. Use a plain oneshot with `OnCalendar=minutely`.
- **Give the oneshot a `TimeoutStartSec`,** so a hung check can't block the timer forever.
- **`pgrep -f` matches more than you want,** including your own grep, scripts and editors that
  mention the pattern. Filter by user (`-u`) and check `/proc/<pid>/exe`.
- In a crash test (killing the daemon), the timer brought it back within about 22 s.

## After a reboot: checklist

| Piece | Why it comes back |
| --- | --- |
| Linger | `loginctl enable-linger <you>`: user services start at boot, no login needed |
| Claude server | `claude-remote-control.service` enabled, `Restart=always` |
| Claude terminal sessions | `remoteControlAtStartup` in `~/.claude/settings.json` (new sessions join on their own) |
| Codex daemon | the watchdog timer (enabled, `OnStartupSec=20s`) starts it with remote control on |
| SSH for the Codex app | `sshd` enabled, localhost only |

Running sessions do not survive a reboot. The services do, so you can start new sessions from the
phone again within about a minute of boot.

## Watching it

The [AI Tracker widget](../README.md) in this repo shows all of the above in the GNOME top bar:

- the state of each piece;
- today's drops and watchdog restarts;
- fix buttons when something is down.

Without the widget:

```sh
systemctl --user status claude-remote-control.service codex-remote-control.timer
journalctl --user -u claude-remote-control -a -n 20
journalctl --user -u codex-remote-control -n 20
codex remote-control start --json   # prints the daemon status (connected / ...); a no-op when it runs
```
