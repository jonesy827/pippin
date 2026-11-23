# Contributing to pippin

Bug reports and pull requests are welcome.

* **Bug Reports:** Include your Mac model and macOS version, your Linux distro, X11 or Wayland, your GPU, and a `-vv` log of the problem. The log's `video decoder:` line shows which decoder `pippin` picked.
* **Pull Requests:** Run `cargo test` and `cargo clippy --all-targets` before opening one. CI builds on stable Rust, Rust 1.95 and Nix. Changes to the wire format need a matching vector in `src/interop_tests.rs`.
* **Testing on a Mac:** Unit tests and the display tests can't reach a real Mac. Say in your PR which changes you ran against one, on which macOS version, and which you didn't.
* **AI Assistance:** AI-assisted code, reviews and bug reports are welcome. State in the PR or issue which tool and model you used (e.g. Claude Code with Claude Opus 5.5) and what it did. You are responsible for every change you submit and should be able to explain it in review. Agents should read [AGENTS.md](AGENTS.md) first.
* **Licensing:** Contributions are licensed under the AGPL-3.0-or-later, and you must have the right to submit them. Do not submit code or disassembly taken from Apple's software.
