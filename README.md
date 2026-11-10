# ytgrab

Click **Download** under a YouTube video (or the ytgrab icon in the toolbar) and the ytgrab app downloads it at the best quality, always as VP9 video + Opus audio in MKV, into `~/Downloads`.

## Layout (Windows)

```
ytgrab\
  ytgrab.exe
  bin\
    yt-dlp.exe     https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe
    deno.exe       https://github.com/denoland/deno/releases (deno-x86_64-pc-windows-msvc.zip)
    ffmpeg.exe     https://github.com/yt-dlp/FFmpeg-Builds/releases (ffmpeg-master-latest-win64-gpl.zip)
```

yt-dlp updates itself (`-U`) every time the app starts. It finds `deno.exe` on its own because it sits in the same folder. On macOS/Linux the tools are taken from `PATH` if `bin/` is missing.

## Build

```
cd app
cargo build --release
```

Windows needs the Rust MSVC toolchain (Visual Studio Build Tools + Windows SDK). `build.rs` embeds `assets/icon.ico` into the exe there.

## Release

Push a tag (`git tag v0.2.0 && git push origin v0.2.0`). `.github/workflows/release.yml` builds on GitHub-hosted runners and publishes a GitHub Release with:

- `ytgrab-windows-x64.zip`: `ytgrab.exe` plus `bin/` with yt-dlp, deno, ffmpeg and ffprobe (latest at build time).
- `ytgrab-macos-arm64.tar.gz`: the bare, unsigned binary. Install the tools with `brew install yt-dlp deno ffmpeg`.
- `ytgrab-extension.zip`: unzip it and use Load unpacked (see below).

The Windows and macOS binaries are rebuilt only if `app/` or the workflow changed since the previous tag. Otherwise they are copied from that tag's release, so don't delete old releases.

## Extension

1. `chrome://extensions` → Developer mode → Load unpacked → `extension/`.
2. On a YouTube video, click **Download** next to Like/Share, or the toolbar icon. Result: `Queued`/`OK`, `App closed`/`OFF`, `Error`/`ERR`, `Not a video`/`YT?`.

The `"key"` in `manifest.json` pins the extension ID to `nmiolemafidjgnibgjgjooceilfpdoaf` wherever it is loaded from. The app accepts requests only with that `Origin`. If you change the key, update `EXTENSION_ORIGIN` in `app/src/main.rs`.

## Notes

- The app listens only on `127.0.0.1:47321` and accepts only the pinned extension Origin. That is enough while the server can only queue YouTube URLs into Downloads; add real auth if it ever does more.
- The download folder is chosen with **Change...** in the app and saved to `%APPDATA%\ytgrab\settings.json` (`~/.config/ytgrab/settings.json` elsewhere). Default: `~/Downloads`.
- Thumbnails come from `i.ytimg.com` via the system `curl` and are cached in the temp folder until the next start.
- Cookies are written to a temp file per job and deleted afterwards.
- Videos without a VP9 stream fail on purpose so every file uses the same codecs.
- Downloading a lot with a logged-in account's cookies can get that account rate-limited or flagged by YouTube.

## Icon

`assets/icon.png` (generated with Codex) is the source. `extension/icons/*.png` and `assets/icon.ico` are resized from it.

## Tests

```
cd app && cargo test
bun extension/cookies.test.js
```
