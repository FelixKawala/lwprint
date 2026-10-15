# Developing lwprint

## Architecture

```
crates/lw-core        no I/O; also meant to build for WebAssembly
  protocol.rs         command framing, job/page setup, raster encoding, status parsing,
                      ST/ER code names
  model.rs            printer models and tapes (only the LW-C410 so far)
  render.rs           label layout: elements left to right, sizes, vertical position,
                      padding, borders; text (with fallback fonts), icons, QR, barcodes,
                      images
  bitmap.rs           1-bit bitmap, threshold/dithering

crates/lwprint        the binary (CLI + service)
  main.rs             CLI (clap): status, print, icons, serve
  printer.rs          RFCOMM connection, sending a job, waiting for the kernel queue,
                      status polling, end-of-job detection
  sdp.rs              minimal SDP client to find the Serial Port channel
  fonts.rs            font loading (fontdb), data: URL decoding
  icons.rs            bundled Material Design Icons + Noto Emoji, search
  service.rs          print service: job queue, single worker, retries, stop/retry,
                      cached printer status, events
  http.rs             axum HTTP API + editor page
  mqtt.rs             MQTT client, Home Assistant discovery
  config.rs           /etc/lwprint.toml
  web/editor.html     the editor (plain HTML/CSS/JS, embedded in the binary)
  assets/             icon/emoji fonts and indexes, with licenses

docs/                 PROTOCOL.md, REVERSE_ENGINEERING.md, label.schema.json, this file
packaging/            example config, OpenRC init script
tools/                Python diagnostic scripts; tools/re: emulator for Epson's library
```

Data flow when printing through the service:

```
HTTP / MQTT ─► Service::submit ─► queue ─► worker ─► wait_for_printer (connect, status)
                                                  ─► render for the inserted tape
                                                  ─► Printer::print per copy
                                                  ─► events ─► MQTT state/result
```

## Design decisions (and why)

* **The service owns the printer; everything else prints through it.** Only one
  Bluetooth connection is possible at a time, so one queue avoids conflicts.
* **Connect per job, disconnect afterwards.** Lets other devices (the laptop CLI, the
  phone app) use the printer in between. Status checks while idle are infrequent
  (default 5 minutes) for the same reason.
* **Render at print time for the inserted tape**, not at submit time. The tape can
  change while a job waits.
* **Retry automatically only before any image data was sent** (`PrintFailure::started`).
  After that, a retry could print a duplicate, so it is left to the user (`retry`).
* **Copies (and pages) go into one printer job**, like Epson's app does: one job setup,
  a page per label, one job end. The printer's firmware then cuts between labels itself
  instead of feeding the blank lead for every label. Pages are sent one at a time and
  the kernel queue is drained after each, so a stop request ends the job after the
  labels already sent; they still print and the job ends normally (never mid-label).
* **Never abandon a print mid-job.** Errors reported while `ST` says printing are
  remembered and reported after the printer finishes or stops on its own.
* **The service only accepts embedded images (data: URLs)**, so requests can't read files
  on the server. The CLI also accepts file paths.
* **Retained MQTT print/cancel messages are ignored**, so a broker restart never reprints.
* **No D-Bus dependency.** `bluer` is used only for its socket types (features `rfcomm`,
  `l2cap`); pairing is left to BlueZ/`bluetoothctl`. This keeps a static musl build easy.
* **Fonts are bundled** (icons, emoji) so the binary works on a bare Alpine install; the
  text font comes from the system (DejaVu Sans preferred).
* **QR codes**: the error-correction level giving the larger squares wins; squares must
  be at least 2 px; a note warns at 2 px (0.28 mm). The printer clips lines to the
  printable width (72 px on 12 mm, tested with `tools/edge_test.py`) and centres it on
  the tape, so the only way to a larger code is `fill`: squares of uneven whole-pixel
  size filling the full height, with the tape edge as the white border. In a simulation
  (blur, ink spread, camera resolution, dark surroundings; ZBar decoder) fill decoded as
  well or better than equal squares; confirm with real phones before making it default.
* **Editor without build tools**: one HTML file, no framework, no CDN (works offline on
  the LAN). The design is remembered in the browser's localStorage.

## Building and testing

```sh
cargo build                          # debug
cargo test                           # unit tests (protocol bytes, rendering, SDP parsing, config, icons)
cargo build --release --target x86_64-unknown-linux-musl   # static binary for Alpine
```

Rust 1.85+ (edition 2024). Install with rustup; distribution packages are often older.

Testing without a printer:

* `lwprint print … --preview out.png` renders exactly what would be printed.
* `lwprint print … --dump job.bin` saves the exact bytes of a job.
* The service runs without a printer; jobs wait and then fail after `retry_minutes`.
  Set `retry_minutes = 0` for quick failures and `status_interval_secs = 0` to stop polling.
* MQTT: run a local broker (`mosquitto`) and watch with `mosquitto_sub -t '#' -v`.
* The protocol tests in `protocol.rs` assert byte-for-byte equality with output captured
  from Epson's library; keep them green when changing commands.

Testing on hardware: `lwprint -v status`, then `lwprint -v print …`. The `-v` log shows
every send, the kernel queue drain time and every status report.

## Common changes

* **New element type**: add a variant to `Element` in `render.rs` (serde tag = JSON
  `type`), render it in `render_element`, add it to `docs/label.schema.json`, the CLI
  (`build_spec` in `main.rs`) if useful, the editor (`KINDS`, `newEl`, `toElement`,
  `body` in `editor.html`) and `PrintRequest::summary` in `service.rs`.
* **New printer model**: add a `Model` in `model.rs` from the table in `PROTOCOL.md`;
  check page setup for its command level and its status path (see `PROTOCOL.md`, "Other
  models"); currently the code assumes `LW_C410` in several places (`main.rs`,
  `service.rs`), which would need a model setting in the config.
* **New config option**: `config.rs` (with `deny_unknown_fields`), the example in
  `packaging/lwprint.toml.example` (a test parses it), and the README.

## Deployment target (the setup this was built for)

* Printer: LW-C410, Bluetooth address `44:B7:D0:64:FC:0E`, Serial Port on channel 6.
* Server: Proxmox host without built-in Bluetooth (Wi-Fi card removed). Plan: a small
  **Alpine VM** with a USB Bluetooth dongle passed through (`qm set <vmid> -usb0
  host=<vendor:product>`), via a USB hub because all ports are in use; printer next to
  the server.
* Alpine notes: replace `linux-virt` with `linux-lts` for the Bluetooth driver (`btusb`),
  install dongle firmware (`linux-firmware-rtl_bt` / `-intel`), run `bluez` + `dbus`
  services, `font-dejavu`; OpenRC script in `packaging/`.
* **Containers don't work for Bluetooth here**: LXC containers (including Docker inside
  the existing Alpine Docker LXC) can't use Bluetooth sockets, which only work in the
  host's own network namespace. Docker inside a VM with host networking would work.
* Home Assistant uses MQTT; the service appears via discovery.

## Known limitations

* Only the LW-C410 is supported and tested.
* Job history is kept in memory (last 100 jobs) and lost on restart.
* No text shaping: combined emoji (flags, skin tones, family sequences) and complex
  scripts don't render correctly. The emoji picker only lists single-glyph emoji.
* QR codes on 12 mm tape are limited to the 72 printable pixels (about 10 mm); with
  equal squares, content over ~17 characters gets 0.28 mm squares. `fill` helps.
* The `54` page margin command is always sent as 0; its meaning isn't fully understood.
* `requested_by` is free text, not authentication. The optional HTTP token is the only
  access control.

## Not yet confirmed on paper

Density other than 0, cut modes `end` and `none`, `--rotate`, multiple copies in one
job (and how much tape it saves), images,
barcodes, icons and emoji, 4/6/9/18 mm tape, borders, the service end to end on the target VM, Home
Assistant discovery against a real Home Assistant.

## Ideas / roadmap

* Batch printing of *different* labels: merge queued jobs with the same density and cut
  mode into one printer job, so a series of labels (e.g. from an automation) shares one
  blank lead. Depends on how much the one-job copies save in practice.
* Free layout mode: absolutely positioned and scalable elements with a canvas editor,
  next to the list editor.
* Public browser version: compile `lw-core` to WebAssembly and print via Web Serial
  (desktop Chrome/Edge); test whether Web Serial works with the C410's RFCOMM first.
* Persist job history (e.g. a small JSON file) and saved label templates on the server.
* More models (see `PROTOCOL.md`), Wi-Fi models over TCP 9100, BLE models.
* An MCP server as a thin adapter over the HTTP API (preview PNG + status), only if
  needed; agents can already use the CLI.
* Decode the remaining status fields (`WR`, `RV`, `TR`, `CT`, `OP`, `EI`).
