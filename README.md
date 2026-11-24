# claude-chill

[![CI](https://github.com/geiszla/claude-chill/actions/workflows/ci.yml/badge.svg)](https://github.com/geiszla/claude-chill/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Version](https://img.shields.io/badge/version-0.1.5-blue)
![Linux](https://img.shields.io/badge/Linux-supported-green)
![macOS](https://img.shields.io/badge/macOS-supported-green)
![Windows](https://img.shields.io/badge/Windows-unsupported-red)
![Rust](https://img.shields.io/badge/rust-2024-orange)

A PTY proxy that tames Claude Code's massive terminal updates using VT-based rendering.

This is a fork of [davidbeesley/claude-chill](https://github.com/davidbeesley/claude-chill), adapted to Claude Code's current inline renderer. It differs from upstream in these ways:

- Rows that scroll off the screen reach your terminal's native scrollback as they leave, so you can scroll up and read while Claude works.
- Lookback writes the emulator's history, each row once as last rendered, instead of replaying raw output, which repeated every partial repaint.
- Resizes reach Claude once they settle, and the scrollback is then rewritten clean at the new size. After a width change Claude is asked to print its transcript again, so the rows above the screen are wrapped for the new width too.
- The screen is never cleared with `ESC[2J`, which VS Code's terminal scrolls into the scrollback instead of erasing.
- A lone Esc is not held back when the lookback key starts with Esc.

## The Problem

Claude Code uses synchronized output to update the terminal atomically. It wraps output in sync markers (`\x1b[?2026h` ... `\x1b[?2026l`) so the terminal renders everything at once without flicker.

The problem: Claude Code sends *entire* screen redraws in these sync blocks - often thousands of lines. Your terminal receives a 5000-line atomic update when only 20 lines are visible. This causes lag, flicker, or jitters in the terminal, making for a poor user experience.

## The Solution

claude-chill sits between your terminal and Claude Code:

1. **Intercepts sync blocks** - Catches those massive atomic updates
2. **VT-based rendering** - Uses a VT100 emulator to track screen state and renders only the differences; rows that scroll off the emulator's screen scroll off your terminal's screen the same way, so your native scrollback grows with the conversation and what you are reading above the live rows stays put
3. **Preserves history** - The emulator keeps every row that scrolled off its screen
4. **Enables lookback** - Press a key to pause Claude and write that history into your terminal's scrollback
5. **Settles resizes** - A drag is forwarded to Claude once it stops, then the scrollback is rewritten clean

## Installation

```bash
cargo install --git https://github.com/geiszla/claude-chill
```

Or, if you've cloned the repository locally:

```bash
cargo install --path crates/claude-chill
```

## Usage

```bash
claude-chill claude
claude-chill -- claude --verbose   # Use -- for command flags
```

### Command Line Help

```
$ claude-chill --help
A PTY proxy that tames Claude Code's massive terminal updates

Usage: claude-chill [OPTIONS] <COMMAND> [ARGS]...

Arguments:
  <COMMAND>  Command to run (e.g., "claude")
  [ARGS]...  Arguments to pass to the command

Options:
  -H, --history <HISTORY_LINES>
          Max lines stored for lookback (default: 100000)
  -k, --lookback-key <LOOKBACK_KEY>
          Key to toggle lookback mode, quote to prevent glob expansion (default: "[ctrl][6]")
  -a, --auto-lookback-timeout <AUTO_LOOKBACK_TIMEOUT>
          Auto-lookback timeout in ms, 0 to disable (default: 15000)
  -r, --resize-refresh <RESIZE_REFRESH>
          Rewrite the terminal scrollback from the emulator this many ms after a resize settles, 0 to disable (default: 500)
  -s, --startup-refresh <STARTUP_REFRESH>
          Once after startup, rewrite the terminal scrollback from the emulator when its history has grown and then stayed unchanged this many ms, 0 to disable (default: 0, the live rendering already scrolls history into the terminal)
  -h, --help
          Print help
  -V, --version
          Print version
```

### Examples

```bash
# Basic usage
claude-chill claude

# Pass arguments to claude
claude-chill -- claude --verbose

# Custom history size
claude-chill -H 50000 claude

# Custom lookback key
claude-chill -k "[f12]" claude

# Disable auto-lookback (see below)
claude-chill -a 0 claude

# Combine options with claude arguments
claude-chill -H 50000 -a 0 -- claude --verbose
```

## Lookback Mode

Press `Ctrl+6` (or your configured key) to enter lookback mode:

> **macOS Note:** Some Mac terminals don't send control characters for `Ctrl+number` keys.`Ctrl+Shift+6` (i.e., `Ctrl+^`) will work by default, or the lookback key can be customized.

1. **Claude pauses** - Output from Claude is cached, input is blocked
2. **History dumps** - Your terminal's scrollback is wiped and replaced with the emulator's history: every row that scrolled off the screen plus the screen itself, each row once, as last rendered
3. **Scroll freely** - Use your terminal's scrollback and search to review everything
4. **Exit** - Press the lookback key again or `Ctrl+C` to resume

When you exit lookback mode, any cached output is processed and the live screen is repainted over the end of the dump, so the dump stays in your scrollback until the next one.

Claude Code repaints by moving the cursor home and rewriting the visible rows, so the emulator's history holds no partial repaints. A row that Claude changes after it has scrolled off the screen (for example tool output that it collapses later) stays in the history as it was when it left.

## Auto-Lookback

After `auto_lookback_timeout_ms` (default 15 seconds) of idle (no user input), the history is automatically dumped to your terminal so you can scroll back without pressing any keys. This continues to re-dump every `auto_lookback_timeout_ms` while idle, whenever there was new output. Each dump wipes the scrollback and rewrites it, which moves what you were reading, so turn it off with `-a 0` if you scroll while Claude works.

## Scrollback Refresh

The live rendering scrolls rows into your terminal's own scrollback as they leave the emulator's screen, in the state they left in, and repaints in place only what changed on the screen. So the native scrollback normally holds the conversation as it happened. It is rewritten wholesale, as one clean copy, on these occasions:

- **Lookback** (the key above), on demand.
- **After a resize**: window resizes are forwarded to Claude only once they settle (150 ms without a change), so a drag causes one repaint. `resize_refresh_ms` (default 500) after that, once Claude's output has gone quiet or at the latest 1.5 s later, the scrollback is rewritten at the new size. Set it to 0 to disable.

  After a resize Claude repaints only the visible rows, with the last rows of its transcript. What the rewrite uses depends on what changed:

  - **Height only**: the emulator moves rows between its history and its screen, so that the screen again holds the last rows. The history stays complete.
  - **Width**: the rows above the screen would keep their old wrapping, and only Claude can wrap them again. The proxy sends Claude `SIGCONT`, which makes it print its whole transcript at the new size, and that print replaces the history. It can be shorter than what had scrolled by: a resumed long session, for example, prints only its recent part.

- **Once after startup**, optionally: `startup_refresh_ms` (default 0, off) rewrites the scrollback when the emulator's history has grown and then stayed unchanged that long. The live rendering already scrolls a resumed session's transcript in, so this is only for terminals where that does not work out.

With `resize_refresh_ms = 0` nothing is printed again and the emulator keeps old rows at the width they were rendered: when the terminal is narrower than those rows, your terminal wraps them. A rewrite replaces the whole scrollback, so it is not done while you may be reading it, which is why there is no periodic refresh. Once the emulator's scrollback is full (`history_lines`), the number of rows that scrolled is unknown and the screen is repainted in place until the next rewrite.

## Configuration

Config file location:
- **Linux**: `~/.config/claude-chill.toml`
- **macOS**: `~/Library/Application Support/claude-chill.toml`

```toml
history_lines = 100000           # Max rows the emulator keeps off-screen (each row costs about 32 bytes per column)
lookback_key = "[ctrl][6]"       # Key to toggle lookback mode
refresh_rate = 20                # Rendering FPS
auto_lookback_timeout_ms = 15000 # Auto-lookback after 15s idle (0 to disable)
resize_refresh_ms = 500          # Rewrite the scrollback this long after a resize settles (0 to disable)
startup_refresh_ms = 0           # Rewrite it once after startup, this long after the history stops growing (0 = off)
```

### Kitty Keyboard Protocol

Modern terminals like Kitty, Ghostty, and WezTerm support the [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) which encodes keys differently than legacy terminals.

claude-chill automatically tracks Kitty protocol state by monitoring the escape sequences passing through the proxy. When Claude Code enables Kitty mode, claude-chill switches to expecting Kitty-encoded key sequences for the lookback key. When Claude Code disables it, claude-chill switches back to legacy mode. This happens transparently with no configuration needed.

### Key Format

`[modifier][key]` - Examples: `[f12]`, `[ctrl][g]`, `[ctrl][shift][j]`

Modifiers: `[ctrl]`, `[shift]`, `[alt]`

Keys: `[a]`-`[z]`, `[f1]`-`[f12]`, `[pageup]`, `[pagedown]`, `[home]`, `[end]`, `[enter]`, `[tab]`, `[space]`, `[esc]`

**Note:** Quote the key value on the command line to prevent shell glob expansion: `-k "[ctrl][7]"`

A key that starts with Esc (`[f12]`, `[esc]`) is matched byte by byte, so a lone Esc keypress is held for 50 ms before it is forwarded; Esc followed at once by another key (Alt+key) and escape sequences such as arrow keys pass through unchanged.

### Why Ctrl+6?

`Ctrl+6` sends 0x1E (ASCII RS), a control character not frequently used by terminals, signals, or shells. Avoid `Ctrl+letter` hotkeys - terminals can't distinguish `Ctrl+J` from `Ctrl+Shift+J`.

**macOS caveat:** Mac terminals don't send control characters for `Ctrl+number` combinations. On macOS, press `Ctrl+Shift+6` (equivalent to `Ctrl+^`) which produces the same 0x1E byte. A more portable default key may be chosen in a future release.

## How It Works

claude-chill creates a pseudo-terminal (PTY) and spawns Claude Code as a child process. It then acts as a transparent proxy between your terminal and Claude:

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│   Terminal   │◄───►│ claude-chill │◄───►│  Claude Code │
│   (stdin/    │     │   (proxy)    │     │   (child)    │
│    stdout)   │     │              │     │              │
└──────────────┘     └──────────────┘     └──────────────┘
```

1. **Input handling**: Keystrokes pass through to Claude, except for the lookback key which toggles lookback mode
2. **Output processing**: Scans output for sync block markers. Non-sync output passes through directly
3. **VT emulation**: Feeds output through a VT100 emulator to track the virtual screen state
4. **Differential rendering**: Compares current screen to previous and emits only the changes
5. **History tracking**: The emulator's scrollback is the history; lookback and the resize refresh write it out row by row
6. **Signal forwarding**: Window resize (SIGWINCH, debounced), interrupt (SIGINT), and terminate (SIGTERM) signals are forwarded to Claude

## Installation with Nix

### Any System (Linux / MacOS)

```bash
# Install directly from GitHub
nix profile install github:geiszla/claude-chill

# Or run without installing
nix run github:geiszla/claude-chill -- --help
```

### NixOS

Add the following to your `flake.nix`:
```nix
inputs.claude-chill.url = "github:geiszla/claude-chill";
```

And then the following package to your environment.systemPackages or home.packages:
```nix
inputs.claude-chill.packages.${system}.default
```

## Disclaimer

This tool was developed for personal convenience. It works for me on Linux and macOS, but it hasn't been extensively tested across different terminals or edge cases. Don't use it to send anyone to space, perform surgery, or run critical infrastructure. If it breaks, you get to keep both pieces.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
