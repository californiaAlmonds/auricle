# Auricle

Auricle is a native desktop music player for Windows, built with **Rust** and the
**[Slint](https://slint.dev/)** UI toolkit. It provides fast search, a persistent
queue, likes/history, OS media-key integration, and a local audio cache — all in a
lightweight native shell with no embedded web view.

## Status

This is an actively evolving prototype. The only supported UI is the native Slint
shell (`src-tauri/ui/native_shell.slint`); there is no React/Vite/Express frontend.

## Essential add-ons (yt-dlp & ffmpeg)

Auricle does **not** bundle or redistribute third-party media tools. Audio
extraction relies on [`yt-dlp`](https://github.com/yt-dlp/yt-dlp), and some cache
operations use [`ffmpeg`](https://ffmpeg.org/). On first run, Auricle offers an
optional **"Install essential add-ons"** step that can download these tools into a
per-user application directory.

Installing them is entirely at your own discretion and choice. You may also install
them yourself and make them available on your `PATH`. Auricle will detect existing
installations and skip them.

These tools are licensed by their respective authors; Auricle is not affiliated with
them and does not distribute them.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (edition 2021, Rust ≥ 1.77.2)
- Windows with the MSVC C++ Build Tools
- (Optional) `yt-dlp` and `ffmpeg`, installed via the in-app add-on step or manually

## Build & Run

The project is driven by Cargo through convenience npm scripts:

```bash
# Debug build
npm run build

# Run the native shell
npm run run

# Release build
npm run release
```

Equivalent direct Cargo commands:

```bash
cargo run --bin native_shell \
  --manifest-path src-tauri/Cargo.toml \
  --target-dir src-tauri/target-native-slint
```

## Architecture

- `src-tauri/ui/native_shell.slint` — Slint UI: layout, callbacks, and state.
- `src-tauri/src/lib.rs` — `run_native_shell()` wires UI callbacks to backend logic.
- `src-tauri/src/core/bridge.rs` — shared `PlaybackCore` singleton.
- `src-tauri/src/core/playback.rs` — queue, history, likes, and the playback worker.
- `src-tauri/src/core/stream_player.rs` — HTTP/range streaming audio source.
- `src-tauri/src/core/cache.rs` — LRU on-disk audio cache.

## Technologies

- **UI**: Slint
- **Language / runtime**: Rust, Tokio
- **Audio**: rodio + symphonia (decode/playback)
- **Music API**: `ytmapi-rs`
- **OS media controls**: souvlaki
- **Local storage**: rusqlite

## Contributing

Contributions follow a `feature → release/x.y.z → main` branch model with
CI-enforced versioning and protected branches. Before opening a pull request,
read [CONTRIBUTING.md](CONTRIBUTING.md) for the branch model, release process,
and rules. Developers using GitHub Copilot can rely on
[.github/copilot-instructions.md](.github/copilot-instructions.md) for project
conventions.

## License

Auricle is free software, licensed under the **GNU General Public License v3.0**.
See [LICENSE](LICENSE) for the full text.

Auricle uses the Slint UI toolkit under its GPLv3 licensing option. Third-party
tools such as `yt-dlp` and `ffmpeg` are not distributed with Auricle and remain
under their own respective licenses.
