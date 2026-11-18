# pippin

`pippin` brings macOS High-Performance Screen Sharing to Linux. 

Normally reserved for Mac-to-Mac connections, Apple's high-performance mode delivers low-latency, color-accurate remote desktop sessions. `pippin` is a native Rust client that connects directly to your Mac's built-in screen sharing server—no third-party software required on macOS.

### Features
* **Crisp Video & Audio:** Streams sharp HEVC 4:4:4 video with system audio.
* **Dynamic Display:** Automatically resizes the Mac's resolution to fit your Linux window.
* **Zero-Lag Cursor:** Renders the cursor locally so mouse movement always feels instant.
* **GPU-Accelerated:** Uses hardware decoding via CUDA or VAAPI whenever supported.
* **Two-Way Clipboard:** Copy and paste text and images seamlessly between Linux and macOS.

## Requirements

* **Host (Mac):** Apple Silicon running macOS Sonoma (14.0) or later.
* **Client (Linux):** X11 or Wayland with a `wgpu`-supported GPU (Vulkan or OpenGL).
* **Network:** IPv4 with UDP ports `5900` and `5901` open.
* **Audio (Optional):** Install `libfdk-aac` (`pacman -S libfdk-aac`, `apt install libfdk-aac2`, or `dnf install fdk-aac`). Without it, `pippin` runs silently.

## Quick Start

1. On your Mac, enable **System Settings → General → Sharing → Screen Sharing**.
2. On Linux, install `pippin` (see below) and connect:

```sh
pippin my-mac.local -u alice
```

## Installation

### Arch Linux
Official AUR packages (`pippin` and `pippin-git`) are ready and will be published as soon as AUR account registration reopens. In the meantime, you can build and install the `pacman` package directly from the repository (which includes the desktop entry and icon):

```sh
git clone https://github.com/jonesy827/pippin
cd pippin/packaging/aur/pippin-git
makepkg -si
```

### Nix
A flake is provided for Nix users. Run it directly, or enter a development shell:

```sh
# Run without installing
nix run github:jonesy827/pippin -- my-mac.local -u alice

# Build and install into your profile
nix profile install github:jonesy827/pippin

# Development shell (Rust toolchain, clang, FFmpeg headers, etc.)
nix develop
```

### Cargo (All Other Distros)
Building from source requires **Rust 1.95 or newer** (use [rustup](https://rustup.rs) if your distro's package is older), along with `clang`, `pkg-config`, FFmpeg development headers, and ALSA:

```sh
# Debian 13 / Ubuntu 24.04
sudo apt install build-essential pkg-config clang libclang-dev libavcodec-dev libavutil-dev libasound2-dev

# Fedora
sudo dnf install gcc pkgconf-pkg-config clang clang-devel libavcodec-free-devel libavutil-free-devel alsa-lib-devel
```

Then install the binary via Cargo:

```sh
cargo install --locked --git https://github.com/jonesy827/pippin
```

*Optional:* `cargo install` only installs the binary. To launch `pippin` from your desktop application menu, install the `.desktop` entry and icon from a local checkout of this repository:

```sh
install -Dm644 assets/pippin.desktop ~/.local/share/applications/pippin.desktop
install -Dm644 assets/pippin.svg ~/.local/share/icons/hicolor/scalable/apps/pippin.svg
```

## Authentication

Unless `--dialog` is passed, `pippin` checks for your password in the following order:
1. `--password <PASS>` or the `PIPPIN_PASSWORD` environment variable
2. Standard input via `--password-stdin`
3. Your desktop keyring
4. An interactive terminal prompt (or the GUI connect dialog if launched from an app menu)

```sh
# Example using pass
pass show mac | pippin my-mac.local -u alice --password-stdin
```

When prompted interactively, `pippin` offers to save your password to the desktop keyring. You can also pass `--save-password` to save it automatically, or `--forget-password` to remove an existing entry.

## Options

| Flag | Description |
|---|---|
| `--dialog` | Always show the GUI connect dialog, even when run from a terminal or with a saved password. |
| `--session share\|alt` | Share the active user's screen (default), or log into a separate background session. |
| `--scale N` | Set display scale. Defaults to `2` on HiDPI screens, `1` otherwise. |
| `--no-auto-resize` | Keep the Mac's display resolution fixed instead of resizing with the window. |
| `--render-scale 0.5` | Stream at half resolution on a 2× display. Softens text slightly in exchange for a 75% bandwidth reduction. |
| `--decoder <TYPE>` | Choose video decoder: `auto` (default), `cuda`, `vaapi`, or `software`. Use `--vaapi-device` to specify a render node. |
| `--after-loss idr\|conceal` | On packet loss, freeze until the next keyframe (`idr`, default) or continue with visual artifacts (`conceal`). |
| `--no-audio` | Mute audio and instruct the Mac not to stream sound. |
| `--swap-ctrl-cmd` | Map `Ctrl` to `⌘`. By default, `Super` is `⌘` and `Alt` is `⌥`. |
| `-v`, `-vv` | Enable debug or trace logging (passwords and clipboard contents are never logged). |

## Performance & Troubleshooting

### Hardware Decoding
macOS streams exclusively in HEVC 4:4:4, which requires specific hardware support:
* **NVIDIA:** Supported on Turing (GTX 16-series / RTX 20-series) or newer via CUDA.
* **Intel:** Supported on Ice Lake (10th Gen) or newer via VAAPI (verify by checking `vainfo` for `VAProfileHEVCMain444`).
* **AMD:** Hardware decoding is not supported for HEVC 4:4:4. `pippin` automatically falls back to CPU decoding, which works well at the cost of higher CPU usage.

By default, `pippin` tries `cuda` → `vaapi` → `software`.

### Slow or Lossy Networks
Apple's protocol does not retransmit dropped video packets—any lost packet stalls the video until a full keyframe arrives. Even 1% packet loss can freeze the display half the time. To smooth out unstable connections:
* Pass `--render-scale 0.5` to shrink keyframe sizes and cut bandwidth usage.
* Pass `--after-loss conceal` to keep the video moving through packet drops (the image will smear briefly until the next keyframe, rather than freezing).

### 4K Streaming
Switching to 4K can briefly backlog incoming video in the kernel (~670 KB) while the decoder initializes. If you experience dropped frames at 4K, raise Linux's maximum UDP receive buffer:

```sh
sudo sysctl -w net.core.rmem_max=8388608
```

## Limitations

* **Clipboard:** Supports text and images, but not file transfers.
* **Single Instance / IPv4:** Media ports are fixed, limiting `pippin` to IPv4 and one active session per client machine.

## Development

Run the test suite with:

```sh
cargo test
```

* **Wire Format Tests:** `src/interop_tests.rs` verifies the protocol byte-for-byte against reference test vectors.
* **Decoder Tests:** Marked `--ignored` by default as they require sample HEVC streams in `PIPPIN_TEST_HEVC_DIR`. You can generate these using FFmpeg (`testsrc2` + `libx265`); see `src/video/decoder.rs` for expected filenames.
* **Network Telemetry:** Pass `--stats-json FILE` to log per-second metrics (schema in `src/session/metrics.rs`).
* **Live Control:** Set `PIPPIN_CONTROL_FIFO=/tmp/pippin.ctl` to adjust parameters on the fly (e.g., `echo 'render_scale 0.5' > /tmp/pippin.ctl`). Supported commands (`render_scale`, `after_loss`, `hold`, `recovery`, `ltr`, `fps`, `keyframe`, `restart`) are documented in `src/session/operating.rs`.
* **AUR Packaging:** AUR package definitions live in `packaging/aur/`. After tagging a release, bump `pkgver` in `packaging/aur/pippin/PKGBUILD`, then run `updpkgsums` and `makepkg --printsrcinfo > .SRCINFO` before pushing to the AUR.

## Credits & License

`pippin` is an independent Rust implementation built on the protocol reverse-engineering work of [iShareScreen](https://github.com/renegadelink/iShareScreen). 

Licensed under **AGPL-3.0**. See [LICENSE](LICENSE) for details.
