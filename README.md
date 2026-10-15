# Tide

A minimal macOS video downloader.

## Tech stack

- Tauri 2 / Rust
- librqbit 9.0.1
- plain HTML/CSS/JS (no bundler)
- WKWebView
- VLC for unsupported formats
- Cinemeta for metadata

## Build

```
cargo install tauri-cli --version "^2"
cd src-tauri && cargo tauri build
```
