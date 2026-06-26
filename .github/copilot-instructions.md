# Copilot instructions for this repository

## Big picture (read this first)
- The active product is a **Windows-native shell**: Rust + Slint.
- Entry point is `src/main.rs` (`run_native_shell()`), not the old React path.
- `README.md` still describes legacy React/Express flows; prefer `package.json` scripts and the Rust code under `src/` as source of truth.
- Core runtime flow:
  1. Slint UI in `ui/native_shell.slint` defines callbacks and state properties.
  2. `run_native_shell()` in `src/lib.rs` wires those callbacks.
  3. Shared playback singleton comes from `src/core/bridge.rs` (`playback_core()`).
  4. Playback logic lives in `src/core/playback.rs` (queue, history, likes, worker thread).

## Service boundaries and data flow
- Keep UI logic in Slint + callback handlers; keep playback/queue state inside `PlaybackCore`.
- `refresh_native_shell_ui()` is the primary hydration path from backend state to Slint models.
- There is no webview or Tauri command layer; the app is a pure Slint shell wired up in `src/lib.rs` (`run_native_shell()`).
- Native audio extraction uses `core/stream_player.rs`: a yt-dlp based streaming source + HTTP/range decode path used by the playback worker. `yt-dlp` is a user-installed add-on, not bundled.

## Build/run workflows
- Build: `npm run build` (or directly: `cargo build --bin native_shell`)
- Run: `npm run run` (or directly: `cargo run --bin native_shell`)
- Release build: `npm run release` (adds `--release` flag)
- There is no webview, React, Vite, or legacy frontend path. The only UI is the native Slint shell.
- There are currently no established automated tests in this repo; rely on smoke checks for playback, queue, search, and tray behavior.

## Project-specific coding conventions
- Do not bypass `core::bridge::playback_core()` with ad-hoc playback instances.
- Preserve non-blocking UI behavior: long operations run on threads; UI updates return through `slint::invoke_from_event_loop`.
- Keep the 500ms background polling loop semantics in `run_native_shell()` consistent with playback flags (`take_advance_pending`, `take_audio_just_started`).
- Follow existing error telemetry style for native audio failures (`[native-audio][stage=...][code=...]`).
- Cache behavior is LRU and persisted by `core/cache.rs` (`cache_index.json`, default 500 MB). Keep cache writes compatible with existing index structure.

## Integration points to be aware of
- External crates/services: `ytmapi-rs` (search/library), user-installed `yt-dlp` (stream extraction), `rodio`/`symphonia` (decode/playback), `tray-icon`/`image` (OS tray icon).
- `yt-dlp` and `ffmpeg` are optional third-party add-ons that are NOT bundled or redistributed. They are installed at the user's discretion (via the in-app "Install essential add-ons" step or manually) into a per-user app-data dir, and resolved from there or from `PATH` by stream/cache code.
- Tray/minimize behavior is controlled in Rust (tray setup in `src/lib.rs`); keep window minimize/restore behavior consistent.

## Agent coordination protocol
- **Act as coordinator**: decompose every non-trivial task into subtasks A/B/C before writing code.
- **One worker per subtask**: spawn a dedicated subagent for each subtask. Each worker must read this file first.
- **Inter-worker communication**: all workers share context — each must declare what files/symbols it touches and what contracts (function signatures, struct fields, callback names) it depends on. If Worker B depends on a type Worker A is creating, Worker A's output is resolved first.
- **Conflict prevention**: no two workers may edit the same function body simultaneously. Coordinate at function/callback granularity.
- **Merge verification**: after all subtask workers complete, the coordinator verifies the combined result compiles (`cargo build --bin native_shell`) and performs a smoke-check summary.

## When adding features
- Implement one vertical slice end-to-end (Slint callback -> Rust handler -> `PlaybackCore`/module -> UI refresh).
- Prefer extending existing models (`SongItem`, `NowPlaying`, bridge structs) over introducing parallel state shapes.
- Validate manually on Windows for: startup, play/pause/next/seek/volume, queue interactions, and minimize-to-tray restore.