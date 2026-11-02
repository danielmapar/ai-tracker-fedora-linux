# ai-tracker-fedora-linux

A GNOME top-bar widget for Claude Code and Codex. It shows:

- plan limits;
- token usage;
- whether this computer's remote-access services, the ones that let you work from your phone, are
  healthy.

It fetches only when you open it, never in the background.

<p align="center">
  <img src="docs/demo.gif" width="408" alt="Demo: open the widget, saved numbers update live, switch to Codex, refresh into a problem, one click on Fix now, healthy again">
</p>

> Unofficial: not affiliated with Anthropic or OpenAI. Live Claude limits are
> [off by default](#live-claude-limits-optional).

## Features

- **Plan limits**: a ring per limit window with a reset countdown and a pace marker. When you are using
  it up too fast, it also projects when it will run out ("full in ~19m").
- **Tokens**:
  - Claude: today, on this computer;
  - Codex: today, the last 7 days and lifetime.
- **Remote access**: health of the Claude Remote Control server and the Codex background service, with
  today's up-time and drops on a timeline.
- **One-click fixes**: start or restart a service, or replace a stray Codex server. Anything that would end
  running sessions asks twice.
- **Fast**: opens instantly with the last numbers, then streams in fresh ones. The first frame takes under
  10 ms, and the live parts arrive within about half a second.

## Install

You need:

- GNOME Shell 50 on Wayland (tested on Fedora 44);
- Rust;
- Claude Code (`claude`) and/or Codex (`codex`) in `~/.local/bin`, logged in.

```sh
git clone https://github.com/danielmapar/ai-tracker-fedora-linux.git
cd ai-tracker-fedora-linux
./install.sh
```

Log out and back in once. After that, `./install.sh` updates the widget in place.

### Live Claude limits (optional)

Claude's limit rings come from an undocumented endpoint that needs your Claude Code login token.
Anthropic's [terms](https://code.claude.com/docs/en/legal-and-compliance) reserve subscription logins
for "ordinary use of Claude Code and other native Anthropic applications". So the widget leaves this off,
and the Claude tab shows everything else. To turn it on at your own risk:

```sh
mkdir -p ~/.config/ai-tracker
echo '{"claude_live_limits": true}' > ~/.config/ai-tracker/config.json
```

## Remote access from your phone

[docs/REMOTE-ACCESS.md](docs/REMOTE-ACCESS.md) explains how to run Claude Code and Codex on Linux so you
can start and continue sessions from your phone or the web. It also covers keeping them up through
crashes and reboots. `./install.sh --workers` installs the services it describes, without switching
them on.

> While they run, anyone signed in to your Claude or ChatGPT account can start work on this computer.

## How it works

A small Rust program (`collector/`) gathers everything and prints JSON. The GNOME extension
(`extension/`) runs it when you open the menu and draws each result as it arrives.
[docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md) covers:

- the data sources;
- what is read and written;
- the streaming format;
- privacy.

## Development

```sh
agent-usage --pretty    # one snapshot (contains your account emails: don't share it)
sh verify/run.sh        # UI tests in a sandboxed headless GNOME Shell, with made-up data
sh verify/demo.sh       # re-record docs/demo.gif (needs ffmpeg and Pillow)
```

## License

[MIT](LICENSE)
