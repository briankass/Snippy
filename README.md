# Snippy

A tiny floating screen **snipping** and **recording** tool for Windows, written in Rust.

![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-blue)

## Download

Grab `snippy.exe` from the [Releases](../../releases) page. It's a single, portable
executable: no installer, no runtime dependencies.

## Features

**Screenshots**
- Region (drag), Window (click to pick) or Full screen, with an optional 3 / 5 / 10 s delay
- Copied to the clipboard and auto-saved to `Pictures\Screenshots`
- A flyout in the corner offers **Edit · Copy · Save as · Open folder**
- Editor: pen, highlighter, arrow and box; undo / redo; save as PNG or JPEG

**Screen recording**
- Record a region, a window or the full screen to **MP4 (H.264 + AAC)**
- **System audio** and/or **microphone**, mixed and kept in sync with the video
- Countdown, pause / resume, red outline around the recorded area, mouse pointer in the video
- Saved to `Videos\Snippy`; the flyout offers **Play · Save as · Delete · Open folder**
- Uses Windows' built-in Media Foundation encoders (GPU-accelerated where available). On
  systems without Media Foundation (Windows "N" editions) it falls back to a software
  encoder, video only.

**Look & feel**
- Glass bar with rounded corners: one-click **Clear**, **Frosted**, **Tinted** or **Solid**
  looks, plus glass style, tint colour, tint strength, frosting and darkness in Settings
- Snippy's own windows never appear in your screenshots or recordings

**Shortcuts & tray**
- **Print Screen** and **Windows + Shift + S** open Snippy instead of the Windows Snipping
  Tool (each can be turned off in Settings)
- System tray icon with quick snip / record actions; optional tray-only mode that hides the bar

**Settings** (gear icon on the bar)
- Screenshot and recording folders, auto-save on/off
- Start with Windows, tray icon, tray-only mode, keyboard shortcuts
- Check for updates: automatically on startup and on demand; one click downloads the new
  version from GitHub Releases, swaps it in and restarts
- Recording frame rate (15/30/60), mouse pointer on/off, default audio sources

## Using it

The floating bar, top to bottom:

| Icon | What it does |
|---|---|
| ⋮⋮ | Drag anywhere on the bar to move it |
| Region / Window / Full screen | Start a snip (or a recording, in video mode) right away |
| Camera | Switch between screenshot mode and video mode (red) |
| Speaker / Mic | *(video mode)* record system audio / microphone |
| Timer | Delay (snips) or countdown (recordings): 0 → 3 → 5 → 10 s. Right-click resets |
| Gear | Settings |
| ✕ | Exit |

In the selection overlay, **Esc** or **right-click** cancels.

### Editor shortcuts

| Keys | Action |
|---|---|
| Ctrl+N | New snip |
| Ctrl+Z / Ctrl+Y (or Ctrl+Shift+Z) | Undo / Redo |
| Ctrl+C | Copy (with annotations) |
| Ctrl+S | Save as… |
| P / H / A / R | Pen / Highlighter / Arrow / Box |

## Building

```bash
cargo build --release
```

The binary ends up in `target/release/snippy.exe`. Building on Windows (MSVC) needs the Rust
toolchain and the Visual Studio C++ build tools.

Cross-compiling from Linux also works:

```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## Releases

Every push is built and tested on Windows by GitHub Actions. **To publish a release, bump
`version` in `Cargo.toml` and push to `main`.** The workflow creates the `vX.Y.Z` tag and a
GitHub Release with `snippy.exe` attached. Pushing a `v*` tag manually also works.

## Notes

- **Shortcuts** use a low-level keyboard hook (Windows reserves Win+Shift+S, so a normal
  hotkey can't claim it). Windows doesn't deliver keys to it while an app running as
  administrator has focus.

- **Glass:** "Frosted blur" (the default) uses a long-standing Windows composition API that
  keeps working when the bar isn't focused. "Windows 11 backdrop" uses the official API, but
  Windows only draws it while the window is focused, so a floating bar usually looks solid.

- Audio is captured with WASAPI; "system audio" records the default playback device.
- The fallback video encoder is Cisco's OpenH264, compiled from source into the executable.
