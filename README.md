# Snippy

A tiny floating screen **snipping** and **recording** tool for Windows, written in Rust.

![bar](https://img.shields.io/badge/platform-Windows%2010%2F11-blue)

## Download

Grab `snippy.exe` from the [Releases](../../releases) page. It's a single, portable
executable — no installer, no runtime dependencies.

## Features

**Screenshots**
- Region (drag), Window (click to pick, Snipping-Tool style) or Full screen
- Optional 3 / 5 / 10 s delay
- Auto-copied to the clipboard
- Annotate with pen, highlighter, arrow and box; undo / redo
- Save as PNG or JPEG

**Screen recording**
- Record a region, a window, or the full screen to **MP4 (H.264)**
- Countdown, pause / resume, and a red outline around the recorded area
- The Snippy bar itself is hidden from recordings (Windows 10 2004+)
- Mouse pointer is drawn into the video
- Saved to `Videos\Snippy\Recording <date>.mp4`

Snippy always captures the monitor its floating bar is on.

## Using it

The floating bar, top to bottom:

| Control | What it does |
|---|---|
| ⋮⋮ | Drag to move the bar |
| ✂ / ⏺ | Take a snip / start a recording |
| 📷 🎥 | Switch between screenshot and recording |
| ⬚ 🗔 🖵 | Region / Window / Full screen |
| ⏱ | Delay (snips) or countdown (recordings). Right-click resets |
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

The binary ends up in `target/release/snippy.exe`. Builds on Windows (MSVC) need nothing
beyond the Rust toolchain and the Visual Studio C++ build tools.

Cross-compiling from Linux also works:

```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Every push to `main` is built and tested by GitHub Actions; pushing a tag like `v0.3.0`
publishes a release with `snippy.exe` attached.

## Notes

- No audio is recorded.
- Recording uses Cisco's OpenH264 encoder, compiled from source into the executable.
