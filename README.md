# SealMusic

A local music player built with Rust, Rodio and Slint. Choose a music directory in Settings to scan its MP3, FLAC, WAV and OGG files. Double-click a song in the library or search results to play it. Metadata is read-only. The lyrics page supports embedded timed lyrics and UTF-8 sidecar files with the same basename and the extension `.lrc`.

SealMusic is a fork of [Zeedle](https://github.com/Jordan-Haidee/zeedle), licensed under GPL-3.0.

[中文介绍](README-zh.md)

## Playback

- Click a column heading to sort by title, artist or duration. Text uses ordinary string order. Sorting preserves the current track and playback history.
- Repeat cycles through playlist, track and off. Shuffle can be enabled independently.
- Space toggles playback. Up and Down select the previous and next track. Left and Right seek 15 seconds, or jump between timed lines on the lyrics page. F1–F4 open the library, search, lyrics and settings.
- Closing the window keeps playback available through the tray or Linux desktop controls when available. Choose Exit from the tray or press Ctrl+Q to quit.
- Settings includes English and Simplified Chinese, automatic or manual theme selection, the version, license and upstream project.

On Linux, MPRIS clients can control playback and query metadata. For example:

```sh
playerctl -p sealmusic play-pause
playerctl -p sealmusic metadata
playerctl -p sealmusic position 10+
```

## Configuration

The fork stores its configuration in `$XDG_CONFIG_HOME/sealmusic/config.toml`, defaulting to `~/.config/sealmusic/config.toml`. To import an upstream configuration, copy its TOML file there while the player is closed. InOrder becomes playlist repeat, Recursive becomes track repeat and Random enables shuffle with playlist repeat. The former About page opens Settings. Unsupported languages fall back to English.

![Settings](assets/settings-en.png)

## Build and package

Run `cargo run` for development, or `cargo build --locked --release` for a release binary. The packaging scripts use cargo-packager and write their output to `target/release`.

| Platform | Commands |
| --- | --- |
| Linux | `./packager/pack-deb.sh`, `./packager/pack-appimage.sh` |
| Windows | `./packager/pack-nsis.ps1`, `./packager/pack-msi.ps1` |
| macOS | `./packager/pack-dmg.sh` |

The Linux packages install the desktop entry and executable as `sealmusic`. [P0 implementation and measurements](P0_PLAN.md)
