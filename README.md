# KeyMemorizer

A Windows keyboard input logger with full multi-language layout support. Captures all typed characters — including non-Latin scripts like Cyrillic — by querying the active keyboard layout via the Windows API.

## Features

- **Multi-language support** — correctly logs characters from any installed keyboard layout Cyrillic, not just English
- **Layout-switch detection** — detects `Alt+Shift`, `Ctrl+Shift`, and `Win+Space` hotkeys and adjusts logging accordingly
- **Race-condition fix** — when switching layouts while staying in the same window, forces the correct HKL until Windows confirms the change
- **Backspace handling** — removes the last logged UTF-8 character on Backspace
- **Smart filtering** — ignores navigation keys (arrows, Home, End, etc.), function keys, and modifier-only events
- **Configurable output path** — accepts a custom folder as a command-line argument
- **Lightweight** — runs in the background with minimal resource usage

## Requirements

- Windows (uses WinAPI for keyboard layout handling)
- [Rust](https://www.rust-lang.org/) toolchain (MSVC target recommended)

## Build

```powershell
# Debug build
cargo build

# Optimized release build
cargo build --release
```

## Usage

```powershell
# Run with default save path (D:\Dev\KeyMemorizer\)
cargo run

# Run with a custom save folder
cargo run -- "C:\Users\You\KeyLogs"

# Run the release binary directly
.\target\release\key_memorizer.exe "E:\MyLogs"
```

All keystrokes are appended to `<folder>\ai_history.log`.

## Debug Mode

Enable debug logging to stderr for diagnostics:

```powershell
# Via environment variable
$env:KEYMEM_DEBUG = "1"
cargo run
```

Or create an empty file `<folder>\debug.enabled`.

For per-key debug spam (verbose), additionally create `<folder>\debug.verbose`.

## How It Works

1. Hooks into the global keyboard event stream via [`rdev`](https://crates.io/crates/rdev)
2. On each keypress, queries the foreground window's keyboard layout (HKL) via `GetKeyboardLayout` / `GetForegroundWindow`
3. Translates the virtual-key code through `ToUnicodeEx` with the correct HKL to produce the actual character
4. Handles layout-switch hotkeys (`Alt+Shift`, `Ctrl+Shift`) by toggling between English and the last-used Cyrillic layout
5. Detects a common Windows race condition: after switching layouts, the foreground window may still report the old HKL for a few milliseconds — KeyMemorizer forces the correct HKL until the OS catches up
6. Removes the last character from the log file on Backspace by detecting UTF-8 byte boundaries

## Supported Layouts

Any keyboard layout installed on the system works. Cyrillic layouts are auto-detected by LANGID 0x04xx range

## License

MIT
