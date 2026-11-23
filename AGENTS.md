# Agent Guide

Notes for AI coding agents working on `pippin`. Read [README.md](README.md) first: it covers what `pippin` does, its options and the session toolbar. This file covers what the README leaves out.

## Ground Rules

* **Disclose yourself:** Pull request and issue descriptions state the tool and model used (e.g. Claude Code with Claude Opus 5.5) and what it did. See [CONTRIBUTING.md](CONTRIBUTING.md).
* **You can't reach a Mac:** Nothing you run here talks to a real host. Never claim a change works against macOS unless the person you work with tested it. Say which parts were only covered by unit or display tests.
* **Never type into a real desktop:** Run the viewer only inside an isolated X server (see below). Don't drive the user's own session with `xdotool`, `ydotool` or similar unless they ask you to.

## Build & Test

Build dependencies are listed under **Installation** in the README. The minimum Rust version is 1.95 (`rust-version` in `Cargo.toml`).

```sh
cargo build
cargo test
cargo clippy --all-targets
```

CI runs all three on stable and 1.95, plus `nix build`. Clippy currently warns in `src/proto/handshake.rs` and `src/ui/dialog.rs`; don't add new warnings.

Tests marked `--ignored` need hardware or sample data:

| Test | Needs |
|---|---|
| `viewer_controls_render_and_capture_input` | An X server and a GPU or software renderer. Run with `xvfb-run -a cargo test viewer_controls -- --ignored`. Set `PIPPIN_SMOKE_HOVER=1` to keep the toolbar expanded for a screenshot. |
| `decodes_sample_streams`, `hw_rejects_unsupported_streams` | Sample HEVC streams in `PIPPIN_TEST_HEVC_DIR` (see `src/video/decoder.rs`). |
| `bench_threads` | A recorded stream in `PIPPIN_BENCH_HEVC` (record one with `PIPPIN_DUMP_HEVC`). |
| `plays_on_default_device` | A working audio output. |

## Running the Viewer Without a Mac

The viewer opens its window before it connects, so the UI can be exercised against a fake host. Keep it off the real desktop:

* **Isolate the display:** Start `Xvfb`, set `DISPLAY` to it and run with `env -u WAYLAND_DISPLAY`. Without unsetting `WAYLAND_DISPLAY`, winit opens the window on the user's Wayland session.
* **Isolate the network:** Media ports are fixed (UDP `5900` and `5901`), so a running `pippin` or other service blocks a second one. Run inside `unshare -rn` (bring up `lo` first) with a TCP listener on `127.0.0.1:5900`. The viewer stays in its "connecting" state until the handshake times out.
* **Check redraws:** The viewer must not redraw while nothing changes. Compare CPU time from `/proc/<pid>/stat` while idle and while moving the pointer with `xdotool` inside the X server.

## Rules the Code Keeps

* **The keyboard belongs to the Mac:** Every key, including Esc, goes to the host. The only local shortcuts are in `shortcut()` in `src/ui/controls.rs`, and they use the physical Ctrl and Alt keys even with `--swap-ctrl-cmd`. egui never receives key or IME events.
* **The pointer is local only over the toolbar:** The toolbar takes the pointer only while it is over the toolbar or dragging it. A button press that starts on the Mac stays with the Mac until it is released.
* **No idle redraws:** Don't pass `RedrawRequested` to egui; it answers every redraw with another. Pointer movement away from the toolbar must not redraw the window.
* **Session events are generation-tagged:** Each session gets a new generation, and events from an older one are dropped (`user_event` in `src/ui/mod.rs`). Keep this when adding events.
* **Don't log secrets:** Passwords and clipboard contents are never logged, at any verbosity.

## Where Things Live

* **Protocol:** `src/proto/` holds the RFB control protocol: authentication, the encrypted record layer, messages and media offers. `src/rtp/` holds SRTP, RTCP feedback and HEVC depayloading.
* **Session:** `src/session/` runs the control and media threads. `operating.rs` holds the quality levers and the commands for `PIPPIN_CONTROL_FIFO`.
* **Viewer:** `src/ui/` holds the window, input forwarding, rendering, the toolbar (`controls.rs`), the connect dialog and reconnect policy.
* **Wire format:** `src/interop_tests.rs` pins messages and ciphers byte for byte to vectors from iShareScreen. A wire format change needs a matching vector.
* **Upstream:** `reference/` is git-ignored. Clone [iShareScreen](https://github.com/renegadelink/iShareScreen) into `reference/iShareScreen` to read the Python reference implementation. It is also AGPL-3.0, so its code can be ported with credit.

## Commits & Pull Requests

* **Commit messages:** A short imperative subject in sentence case with no prefix (e.g. "Fix the toolbar's redraw loop"), and a body that explains why.
* **Branches:** Work on a branch and open a pull request against `main`. `main` requires an approving review.
* **Docs:** Update the README in the same change when behavior or options change. Match its level of detail and style.
* **Releases:** The AUR steps are under **Development** in the README.
