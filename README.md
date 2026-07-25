# ccdesk

A Claude Code session manager TUI, modeled after **Claude Desktop** and the built-in
**Agent View** — see, switch, and drive all your sessions in one terminal.

ccdesk embeds the official `claude` CLI in a PTY pane and keeps an Agent View–style
session list in a persistent sidebar (like Claude Desktop's session list). Sessions are
dispatched and owned by the official Claude Code supervisor (`claude --bg` /
`claude attach`), so they survive ccdesk restarts and stay consistent with the official
Agent View.

![ccdesk](docs/screenshot.png)

## Features

- **Persistent sidebar** with every Claude Code session across all projects,
  grouped by state (Ready for review / Needs input / Working / Completed) or by directory
- **Official lifecycle** — new sessions dispatch via `claude --bg`, windows are
  `claude attach` clients; stop/delete use `claude stop` / `claude rm`
- **Live status** from `claude agents --json` (rename, state changes reflect in ~2 s)
- **Account line** in the sidebar footer, showing the signed-in Claude account, the same value
  `ccdesk doctor` prints. A login or logout that rewrites `~/.claude/.credentials.json` shows
  up in ~1 s; where credentials live outside that file, only the ~60 s refresh applies
- **Mouse-first**: click to switch/focus, ☰ menu for stop/delete, drag the border to resize
- **Keyboard**: `Ctrl+X` stop→delete, `Ctrl+S` toggle grouping, `Alt+←/→` pane focus,
  `Ctrl+Q` quit — everything else passes through to claude untouched. On the new-session
  screen, `Tab` cycles fields and `Enter` runs the selected folder-list row
- **New-session screen** with a folder browser, editable path field (paste / drag & drop),
  and a first-prompt input. The list starts with a `+ start in <folder>` row, so you can
  launch a session in the folder you are browsing without typing a prompt first

## Requirements

- Windows 10/11 (ConPTY). Linux/macOS are untested.
- [Claude Code](https://claude.com/claude-code) CLI on `PATH`
- Rust toolchain (for installation from source)

## Install

### With cargo

```sh
cargo install --git https://github.com/HRYooba/ccdesk
```

### From Releases (no Rust required)

1. Download the latest `ccdesk-vX.Y.Z-x86_64-pc-windows-msvc.zip` from the
   [Releases](https://github.com/HRYooba/ccdesk/releases) page.
2. Unzip it and place `ccdesk.exe` somewhere on your `PATH`.
3. Run `ccdesk`.

Each release also ships a `.sha256` file so you can verify the download.

(Developers working from a clone: `cargo install --path .`)

## Commands

```sh
ccdesk            # launch the TUI
ccdesk doctor     # diagnose the environment (claude CLI, account, config dir, terminal)
ccdesk logs       # print the path and tail of the error log
ccdesk update     # check for a new release and show how to update
ccdesk --version  # print version
ccdesk --help     # show usage
```

Settings (grouping, opt-ins) live in `~/.ccdesk/config.json`; window state
(sidebar width, last screen, last folder) in `~/.ccdesk/state.json`.
Errors and panics are appended to `~/.ccdesk/error.log`.

## Usage display (opt-in)

Add `"usage_display": "on"` to `~/.ccdesk/config.json` to show your Claude
rate-limit usage (5h / 7d windows, with time until reset) at the bottom right.

When enabled, ccdesk passes the official `--settings` flag to its embedded
`claude attach` processes to install a status-line hook. The hook saves the
official rate-limit JSON that Claude Code provides to status lines, and then
chains to your own status line if you have one configured — your display keeps
working unchanged. No files outside `~/.ccdesk/` are modified, and sessions
opened outside ccdesk are unaffected.

## License

MIT
