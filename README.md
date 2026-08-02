# ccdesk

A session manager TUI for Claude Code — see, switch, and drive all your sessions in
one terminal.

Each pane **is** a real foreground `claude` session. The list outlives them: closing
ccdesk ends the processes but keeps the rows, and reopening one resumes the
conversation. OpenAI's `codex` CLI works the same way — [opt in](#codex-opt-in).

![ccdesk](screenshots/screenshot.png)

## Features

- **One list for every session**, across all projects — group it by state, by
  directory, or by agent
- **Start, stop, and resume from the sidebar** — `stop` ends the process and keeps the
  row, `close` drops the row. Your transcripts are never touched
- **Folders stay** in the directory grouping after their last session is gone, so the
  way back in is never lost
- **Rows are named by the agent**, not by ccdesk — rename with `/rename` in the pane
- **Status you can trust** — each row shows what its process is actually doing right
  now (Waiting / Working / Idle / Stopped)
- **Unread marks and pinning** — `●` for rows that spoke while you were elsewhere;
  pinned rows sit at the top
- **In-place updates** for ccdesk and each agent, from rows at the top of the sidebar
- **Mouse-first, keyboard-light** — click to switch, the `=` at the end of a row for
  everything else. ccdesk reserves two keys — `Alt+←/→` and `Ctrl+Q` — and **every
  other key goes to the agent untouched**
- **ccdesk never modifies your agent's config or files.** It installs its status hooks
  per session only, and sessions you open outside ccdesk are unaffected

## Requirements

- Windows 10/11. Linux/macOS are untested.
- [Claude Code](https://claude.com/claude-code) CLI on `PATH`
- [Codex](https://developers.openai.com/codex/cli) CLI on `PATH` — only with codex on
- Rust toolchain (to install from source)

## Install

```sh
cargo install --git https://github.com/HRYooba/ccdesk
```

Or grab `ccdesk-x86_64-pc-windows-msvc.exe` from
[Releases](https://github.com/HRYooba/ccdesk/releases) and put it on your `PATH`.
Each release ships a matching `.sha256`.

## Commands

```sh
ccdesk            # launch the TUI
ccdesk doctor     # diagnose the environment
ccdesk logs       # print the path and tail of the error log
ccdesk update     # update to the latest release
ccdesk --version
ccdesk --help
```

Settings are in `~/.ccdesk/config.json`, errors in `~/.ccdesk/error.log`. Both
optional features are one line each:

```json
{ "codex": "on", "usage_display": "on" }
```

## Codex (opt-in)

Turn it on and codex sessions join the same list — same rows, same menu, same states —
and `agent` becomes a third way to group. A folder's menu asks which agent to start,
so it never launches something you did not pick.

It is off by default so that people without codex installed never pay for it. Turning
it off later hides your codex rows without deleting them.

Two things come from codex itself, not ccdesk:

- A codex row has no name until you send your first prompt.
- Codex prints a trust warning on every launch. ccdesk takes the warning rather than
  write hook settings into your `~/.codex/config.toml`.

## Usage display (opt-in)

Shows your rate-limit usage at the bottom right, one line per agent: the 5-hour
window, the 7-day window, and each per-model weekly window, with time until reset.
It stays current on its own, and you can click it to refresh now.

It is opt-in because it is the only thing ccdesk does that reaches the provider's
servers while you are away — everything else it watches is local. It consumes no
tokens and no rate-limit quota. `ccdesk doctor` shows what it would report on your
machine, so you can look before turning it on.

## License

MIT
