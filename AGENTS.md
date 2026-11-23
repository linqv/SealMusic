# AGENTS

Instructions for AI coding agents working in this repository.

## Build Policy (Linux Only)
- **Target Linux only — do not package or build for other platforms (只构建 Linux，不打包和构建其他平台).**
- Only build, test, and verify on Linux (`cargo run`, `cargo build`, `cargo build --release`, `cargo test`, `cargo clippy`).
- **Do not package:** never run `./packager/*` scripts, `cargo packager`, `.deb`, `.AppImage`, NSIS, MSI, or DMG builds unless the user explicitly overrides this rule.
- **Do not build or cross-check other platforms:** never run Windows/macOS builds or `--target` cross-compilation checks. Existing `#[cfg(...)]` boundaries stay intact in source code, but CI/agent verification stays strictly Linux-only.

## Quick Start
- Dev run: `cargo run`
- Release build: `cargo build --release`
- Runtime smoke check: `./target/release/sealmusic --check-runtime`
- Unit tests: `cargo test`
- MPRIS live D-Bus test: `dbus-run-session -- cargo test mpris_dbus_roundtrip -- --ignored`
- Library benchmark: `SEALMUSIC_BENCH_DIR=<dir> cargo test --release measure_library -- --ignored --nocapture`
- Format (requires nightly): `cargo +nightly fmt`
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Pre-commit check: `cargo +nightly fmt --check && cargo clippy --all-targets --all-features -- -D warnings`

## Project Map
- `src/main.rs`: `APP_ID = "sealmusic"`, `--check-runtime` fast exit, `single-instance` lock, logger init.
- `src/app.rs`: Slint UI setup (`set_xdg_app_id`), callbacks, tray actions, theme/progress timers, state projection.
- `src/player/`: `command.rs` (`PlayerCommand`, `Controls`), `state.rs` (`PlaybackState`, `PlaybackEvent`, `Queue`), `local.rs` (`decode`), `mod.rs` (`PlayerCore` + Rodio worker).
- `src/library/`: `mod.rs` (`Track`, `TrackId`, `TrackOrigin`), `scan.rs` (`scan`, `sort`, `search`), `metadata.rs` (`read_meta_info`, `read_details`), `lyrics.rs` (`read_lyrics`, `parse_lyrics`).
- `src/integrations/mpris.rs`: `#[cfg(target_os = "linux")]` MPRIS2 D-Bus server (`org.mpris.MediaPlayer2.sealmusic`), cover cache (`cache_cover`).
- `src/config.rs` & `src/logger.rs`: TOML persistence (with legacy migration) and file/stdout logging.
- `ui/` & `src/slint_types.rs`: Slint UI components (`ui/app.slint`, `ui/theme.slint`, etc.) and generated bindings.

## Architecture Rules
- **Single source of truth:** `PlayerCore` owns Rodio, queue, history, and `PlaybackState`. UI, tray, and MPRIS only send `PlayerCommand` through `Controls` and project `PlaybackEvent` / `SharedState`.
- **Distinct playback commands:** `Play`, `Pause`, `TogglePlayPause`, and `Stop` have separate semantics (never alias `Pause` to toggle). `LoopMode` (`None`, `Track`, `Playlist`) and `shuffle` (`bool`) are independent.
- **Stable track identity:** Use `TrackId` (`/dev/sealmusic/track/local_<hex>`) for track selection and `SeekTo` / MPRIS `SetPosition` validation — never list indices.
- **GUI thread safety:** Update Slint properties only via `slint::invoke_from_event_loop`. Keep queue/history in Rust.
- **MPRIS isolation:** Keep Linux-only dependencies (`mpris-server`, `futures-channel`, `futures-lite`, `url`, `tempfile`) under `[target.'cfg(target_os = "linux")'.dependencies]` and isolate code behind `#[cfg(target_os = "linux")]`. Startup must still succeed when no D-Bus session bus is available (MPRIS disables itself and sets `can_run_hidden = false`).
- **Scope guardrails:** Metadata and library scanning are strictly read-only — no tag editing, no file deletion, no spectrum/FFT, and no pinyin sorting (plain deterministic string order).
- **Navigation pages:** `0 = Gallery`, `1 = Search`, `2 = Lyrics`, `3 = Settings` (About info is integrated into Settings).

## Hotkeys (`ui/app.slint` `FocusScope`)
| Key | Action |
|---|---|
| `Space` | Play/pause toggle |
| `←` | Seek backward 15s; on Lyrics page (`2`), snap to previous timed lyric line |
| `→` | Seek forward 15s; on Lyrics page (`2`), snap to next timed lyric line |
| `↑` | Previous song |
| `↓` | Next song |
| `F1`–`F4` | Switch pages (`0` Gallery, `1` Search, `2` Lyrics, `3` Settings) |
| `Ctrl+Q` | Quit and save state |

## Theme & Localization
- Custom `Theme` global in [ui/theme.slint](ui/theme.slint); light/dark controlled by `Theme.is-dark` via `set_light_theme()` alongside Slint `Palette.color-scheme`.
- `dark-light` polls the system theme every 1s when `follow_system_theme` is enabled.
- Only English (`""`) and Simplified Chinese (`"zh_CN"`) are bundled (`lang/sealmusic.pot`, `lang/zh_CN/LC_MESSAGES/sealmusic.po`); unsupported codes in config fall back to `""`.

## Conventions
- **Logging:** `env_logger` at `Info` (`zbus` filtered to `Warn`), writing to stdout + `~/.sealmusic.log` (auto-removed when > 10 MiB on startup).
- **Config:** `$XDG_CONFIG_HOME/sealmusic/config.toml` (fallback `~/.config/sealmusic/config.toml`), saved atomically via temp file + rename.
- **Cover cache:** MPRIS exports embedded cover art as PNG under `$XDG_CACHE_HOME/sealmusic/art` (fallback `~/.cache/sealmusic/art`) via `tempfile::NamedTempFile`, cleaned up on track change/exit.
- **Audio & tags:** Supported extensions `mp3`, `flac`, `wav`, `ogg`; `lofty` reads tags/covers; cover falls back to `ui/cover.svg`; Linux font defaults to `Noto Sans CJK SC`.

## Pitfalls
- Slint is pinned to `=1.19.0` in both `[dependencies]` and `[build-dependencies]` — keep versions aligned.
- `mpris-server` is pinned to `"0.9"` (Linux-only); check `mpris_dbus_roundtrip` after any MPRIS or `TrackId` changes.
- `build.rs` uses `slint_build::CompilerConfiguration` with style `fluent` and `with_bundled_translations("lang")`. Run `i18n-update` whenever Slint `@tr(...)` text changes.
- `single-instance` (`APP_ID = "sealmusic"`) prevents duplicate instances — do not remove.
- Rust edition is `2024` with `.rustfmt.toml` nightly rules (`use_small_heuristics = "Off"`, `imports_granularity = "Crate"`, `group_imports = "StdExternalCrate"`) — always run `cargo +nightly fmt`.
- Release profile uses `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`.
- Slint bindings are generated in `src/slint_types.rs` via `slint::include_modules!()`.

## Change Playbooks
- **Add a player command:** Add variant to `PlayerCommand` in `src/player/command.rs`, handle it in `PlayerCore::handle` in `src/player/mod.rs`, then wire UI (`src/app.rs`), tray, or MPRIS (`src/integrations/mpris.rs`).
- **Add audio format:** Update extension filter in `src/library/scan.rs` and verify both `rodio` decoding and `lofty` tag reading.
- **Add/edit UI text:** Update `.slint` files under `ui/`, then follow [.agents/skills/i18n-update/SKILL.md](.agents/skills/i18n-update/SKILL.md).
- **Add Slint component:** Create file in `ui/`, import in `ui/app.slint`, check `src/slint_types.rs` exports and `src/app.rs` wiring.

## Existing Docs
- [tools/lx-probe/README.md](tools/lx-probe/README.md) — LX custom source P0 probe and verification record
- [README.md](README.md) — English overview
- [README-zh.md](README-zh.md) — Chinese overview
- [.agents/skills/i18n-update/SKILL.md](.agents/skills/i18n-update/SKILL.md) — i18n workflow
