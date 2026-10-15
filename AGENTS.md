# Notes for contributors and AI assistants

Start with `README.md` (usage), then `docs/DEVELOPMENT.md` (architecture, decisions,
testing, limitations, roadmap) and `docs/PROTOCOL.md` (everything known about the printer).

Ground rules that came out of hardware testing; don't undo them without testing on a
real printer:

* Encrypted RFCOMM (`BT_SECURITY` medium), channel looked up via SDP.
* Wait for the kernel send queue to drain before closing (`Printer::wait_until_sent`).
* The first status after `51 05 00` is stale; completion is `ST 05`, or idle after `ST 02`.
* 500 ms pause after each raster band of 1024 lines.
* Never retry automatically once image data was sent; stop between labels, not mid-label.
* Keep `lw-core` free of I/O (it is meant to build for WebAssembly).
* Keep the byte-level tests in `crates/lw-core/src/protocol.rs` passing.

Checks before handing over changes: `cargo build`, `cargo test`, and for rendering
changes a `--preview` PNG. Hardware-dependent changes need a test print (`lwprint -v`).
