# Snippy

A tiny floating screen **snipping** and **recording** tool for Windows, written in Rust.

![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-blue)

## Download

Grab `snippy.exe` from the [Releases](../../releases) page. It's a single, portable
executable: no installer, no runtime dependencies.

## Features

**Screenshots**
- Region (drag), Window (click to pick) or Full screen, with an optional 3 / 5 / 10 s delay
- Works across **all monitors**: the selection overlay covers every screen (with several
  monitors, Full screen asks which one); mixed display scaling is handled
- Copied to the clipboard and auto-saved to `Pictures\Screenshots`
- A flyout in the corner offers **Edit · Copy · Save as · Open folder**
- Editor: **select** (click an annotation to select it, drag to move, Delete to remove,
  double-click text to edit it) and **crop** (drag the part to keep, Enter applies; undo
  brings the rest back)
- Editor: pen, highlighter, arrow, box, **text** and numbered **steps**; undo / redo; save
  as PNG or JPEG. Text: click and type (Shift+Enter for a new line, Enter to finish).
  Steps: click to place 1, 2, 3…, or drag to add an arrow to what the step points at
- **Redact:** drag over anything private to pixelate, blur or black it out (black is the
  safest: nothing can be recovered). **Find personal info** hides email addresses, phone
  numbers and long numbers (card, account, order) automatically

**Text, colours, pins & history**
- **Scrolling capture:** select the scrollable part of a window (e.g. a web page) and
  Snippy scrolls it and stitches one tall image; sticky headers/footers appear once.
  Esc or Stop ends it early
- **Copy text from screen:** select an area and its text goes to the clipboard, using the
  OCR engine built into Windows (offline, nothing to install)
- **Colour picker:** a magnifier follows the pointer; click to copy the colour as hex, with
  one-click copy as HEX or RGB
- **Pin to screen:** keep a snip floating on top of everything. Drag to move, mouse wheel to
  zoom, hover for Copy / Close (Esc or double-click closes)
- **History:** your recent snips and recordings with Copy, Edit, Pin, Open, Show in folder

**Screen recording**
- Record a region, a window or a full screen, on any monitor, to **MP4 (H.264 + AAC)**
- **System audio** and/or **microphone**, mixed and kept in sync with the video
- Countdown, pause / resume, red outline around the recorded area, mouse pointer in the video
- **GIF** button (video mode): record a silent, looping animated GIF instead, ideal for
  chats and bug reports (up to 15 fps, scaled to at most 960 px; only what changes between
  frames is stored, so still screens make tiny files)
- Saved to `Videos\Snippy`; the flyout offers **Play · Copy · Save as · Delete · Open
  folder**. Copy puts the file on the clipboard, ready to paste into a chat, email or folder
- Uses Windows' built-in Media Foundation encoders (GPU-accelerated where available). On
  systems without Media Foundation (Windows "N" editions) it falls back to a software
  encoder, video only.

**Look & feel**
- Glass bar with rounded corners: one-click **Clear**, **Frosted**, **Tinted** or **Solid**
  looks, plus glass style, tint colour, tint strength, frosting and darkness in Settings
- Snippy's own windows never appear in your screenshots or recordings

**Shortcuts & tray**
- **Print Screen** and **Windows + Shift + S** bring up the Snippy bar (or, if you prefer, start a
  snip right away) instead of the Windows Snipping
  Tool (each can be turned off in Settings)
- The bar jumps to the monitor under your mouse when a shortcut brings it up
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
| Page with arrow | Scrolling capture of a long page |
| Text scan | Copy text from an area of the screen |
| Pipette | Pick a colour from the screen |
| Camera | Switch between screenshot mode and video mode (red) |
| GIF | *(video mode)* record an animated GIF instead of an MP4 video |
| Speaker / Mic | *(video mode, MP4)* record system audio / microphone |
| Timer | Delay (snips) or countdown (recordings): 0 → 3 → 5 → 10 s. Right-click resets |
| Clock | Recent captures (history) |
| Gear | Settings |
| Tray arrow | Hide to the system tray (click the tray icon to bring it back). Right-click exits |

In the selection overlay, **Esc** or **right-click** cancels.

### Editor shortcuts

| Keys | Action |
|---|---|
| Ctrl+N | New snip |
| Ctrl+Z / Ctrl+Y (or Ctrl+Shift+Z) | Undo / Redo |
| Ctrl+C | Copy (with annotations) |
| Ctrl+S | Save as… |
| V / C | Select / Crop |
| P / H / A / R / T / N / X | Pen / Highlighter / Arrow / Box / Text / Step / Redact |
| Delete · Enter · Esc | Delete selection · apply crop · cancel |

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
