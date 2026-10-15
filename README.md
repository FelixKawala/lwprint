# lwprint

Print labels on an **Epson LabelWorks LW-C410** from Linux over Bluetooth: text, QR codes,
barcodes and images. The protocol was reverse-engineered from Epson's Label Editor app
and verified on real hardware.

```
crates/lw-core       label rendering + printer protocol (no I/O, also builds for WebAssembly)
crates/lwprint       command-line tool and print service (Bluetooth via BlueZ)
docs/                protocol reference, reverse-engineering notes, developer guide,
                     JSON Schema of the label format
packaging/           example config and OpenRC init script
tools/               Python diagnostic scripts and the emulator used for reverse engineering
```

Documentation:

* [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md): architecture, design decisions, testing,
  deployment notes, known limitations and roadmap
* [`docs/PROTOCOL.md`](docs/PROTOCOL.md): the printer protocol, status codes, tape codes
  and Epson's table of other models
* [`docs/REVERSE_ENGINEERING.md`](docs/REVERSE_ENGINEERING.md): how the protocol was
  obtained, to repeat it for other models
* [`docs/label.schema.json`](docs/label.schema.json): the label JSON format

## Build

Needs Rust 1.85 or newer (`rustup`, or e.g. `apt install cargo-1.89` on Ubuntu 24.04).
No D-Bus or other system libraries are required.

```bash
cargo build --release
sudo cp target/release/lwprint /usr/local/bin/
```

## Setup

Pair the printer once (it must be switched on):

```bash
bluetoothctl
  power on
  scan on            # wait for LW-C410@...
  pair 44:B7:D0:64:FC:0E
  trust 44:B7:D0:64:FC:0E
```

Set the address so you don't need `--device` every time:

```bash
export LWPRINT_DEVICE=44:B7:D0:64:FC:0E
```

The serial port channel is looked up automatically (6 on the tested printer).
If the lookup fails, pass `--channel 6` or set `LWPRINT_CHANNEL=6`.

## Usage

```bash
lwprint status
lwprint print --text "Screws M4"
lwprint print --text "Screws M4" --text "Box 12" --qr "https://example.com/box/12"
lwprint print --barcode "BOX-0042" --text "Box 42"
lwprint print --barcode 4006381333931 --barcode-type ean13
lwprint print --icon screwdriver --text "Screws M4"
lwprint print --icon oil --icon flower --text "Olive oil" --border rounded
lwprint print --text "Fridge ❄️"
lwprint icons light bulb          # search icon names (add --emoji for emoji)
lwprint print --image logo.png --text "Felix"
lwprint print --image photo.jpg --dither
lwprint print --text "Kabel HDMI" --copies 3 --density 2
```

The tape width is detected automatically. Elements are placed left to right in this
order: image, icon, QR code, text, barcode. For any other order, use a JSON spec.

Icons come from [Material Design Icons](https://pictogrammers.com/library/mdi/) (the
same `mdi:` names Home Assistant uses, about 7,200 icons). Emoji in text are printed with
the monochrome Noto Emoji font (about 1,400 emoji). Both are built into the binary;
their licenses are in `crates/lwprint/assets/`.

**Preview without a printer** (saves a PNG at real resolution, 1 pixel = 0.14 mm):

```bash
lwprint print --text "Test" --qr "hello" --tape 12 --preview label.png
```

### Options

| Option | Meaning |
|---|---|
| `--font NAME\|FILE` | Font family or .ttf/.otf file (default: DejaVu Sans, then any sans-serif) |
| `--text-size MM` | Line height in mm (default: lines share the tape height) |
| `--density -5..5` | Darker or lighter print |
| `--no-cut` | Don't cut after the label |
| `--copies N` | Print N labels |
| `--margin MM` | Blank tape at both ends (default 2) |
| `--gap MM` | Space between elements (default 1.5) |
| `--min-length MM` | Minimum label length; content is centred |
| `--rotate` | Turn the label 180° on the tape |
| `--border STYLE` | Frame: `line`, `double`, `rounded`, `dashed` |
| `--padding MM` | Extra blank space at the top and bottom edge |

## JSON labels (for automation)

```json
{
  "elements": [
    { "type": "icon", "name": "mdi:toolbox" },
    { "type": "qr", "data": "https://inventory.lan/box/42", "size_mm": 8 },
    { "type": "text", "text": "Box 42\nTools", "align": "left" },
    { "type": "icon", "name": "flower", "size_mm": 5, "valign": "top" },
    { "type": "barcode", "data": "BOX-42", "symbology": "code128", "module_px": 2 },
    { "type": "image", "path": "logo.png", "dither": false, "height_mm": 6 },
    { "type": "space", "mm": 5 }
  ],
  "margin_mm": 2,
  "padding_mm": 0.5,
  "gap_mm": 1.5,
  "min_length_mm": 40,
  "border": "rounded"
}
```

```bash
lwprint print --spec label.json
echo '{"elements":[{"type":"text","text":"Hello"}]}' | lwprint print --spec -
```

Image paths are relative to the JSON file. `symbology` is `code128`, `ean13` or `code39`.

## Print service (`lwprint serve`)

Runs on the machine with the printer and provides a web editor, an HTTP API and
(optionally) MQTT with Home Assistant discovery. All printing goes through one queue;
jobs wait while the printer is off or has no tape and are retried for `retry_minutes`.

```sh
cp packaging/lwprint.toml.example /etc/lwprint.toml   # then edit
lwprint serve --config /etc/lwprint.toml
```

Open `http://<host>:8080/` for the editor.

### Alpine (OpenRC)

```sh
apk add bluez dbus font-dejavu
rc-update add dbus && rc-update add bluetooth
cp lwprint /usr/local/bin/
cp packaging/lwprint.openrc /etc/init.d/lwprint && chmod +x /etc/init.d/lwprint
cp packaging/lwprint.toml.example /etc/lwprint.toml      # then edit
rc-update add lwprint && rc-service lwprint start
tail -f /var/log/lwprint.log
```

Build a static binary that runs on Alpine with
`cargo build --release --target x86_64-unknown-linux-musl`
(after `rustup target add x86_64-unknown-linux-musl`).

### HTTP API

| Request | Body | Response |
|---|---|---|
| `GET /api/status` | | printer status JSON |
| `POST /api/status/refresh` | | checks the printer in the background |
| `POST /api/preview` | label JSON | PNG (headers `x-label-length-mm`, `x-tape-mm`) |
| `POST /api/print` | label JSON | job JSON (`202`) |
| `GET /api/jobs` | | recent jobs |
| `GET /api/jobs/{id}` | | one job: `queued`, `waiting`, `printing`, `done`, `failed` or `cancelled` |
| `POST /api/jobs/{id}/cancel` | | stop a job; a printing job stops after the current label |
| `POST /api/jobs/{id}/retry` | | print the missing copies of a failed/stopped job (all copies of a finished one) |
| `POST /api/jobs/cancel` | | stop all jobs |
| `GET /api/icons?q=…` | | icon search results (`name`, `codepoint`) |
| `GET /api/emoji?q=…` | | emoji search results (`emoji`, `name`, `group`) |

The label JSON is the same as for `--spec`, plus optional `copies`, `density`, `cut`,
`rotate`, `tape_mm`, `request_id` and `requested_by` (see `docs/label.schema.json`).
Each job records `requested_by` (a free-text name, not verified) and its `source`
(`web <ip>` or `mqtt`), so the shared history shows who printed what.

Jobs retry by themselves while the printer is off or busy, and also when a print fails
before any image data was sent. Once data was sent they never retry automatically,
because that could print a label twice; use `retry` instead. Images must be
embedded as `data:` URLs; the service never reads files named in a request.

```sh
curl -X POST http://labelprinter.lan:8080/api/print \
  -d '{"elements":[{"type":"text","text":"Hello"}],"copies":2}'
```

If `token` is set in `[http]`, send `Authorization: Bearer <token>` (or `?token=`).

### MQTT and Home Assistant

With an `[mqtt]` section, the service connects to your broker and Home Assistant
discovers a "Label printer" device with these entities: printer reachable, tape width,
state, problem, labels waiting, last print, and "Check printer" and "Cancel printing" buttons.

| Topic | Direction | Content |
|---|---|---|
| `lwprint/print` | to service | label JSON (publish **without** retain) |
| `lwprint/result` | from service | job result: `done`, `failed` or `rejected`, with `request_id` |
| `lwprint/state` | from service | printer status (retained) |
| `lwprint/availability` | from service | `online` / `offline` |
| `lwprint/refresh` | to service | anything: check the printer now |
| `lwprint/cancel` | to service | `all`, a job id, or a `request_id`: stop printing |

Print from an automation:

```yaml
action: mqtt.publish
data:
  topic: lwprint/print
  payload: >-
    {"elements": [
      {"type": "qr", "data": "https://inventory.lan/box/{{ box_id }}"},
      {"type": "text", "text": "Box {{ box_id }}\n{{ box_name }}"}
    ], "request_id": "box-{{ box_id }}"}
```

React to failures, e.g. notify when a label could not be printed:

```yaml
trigger:
  - trigger: mqtt
    topic: lwprint/result
condition:
  - condition: template
    value_template: "{{ trigger.payload_json.state in ['failed', 'rejected'] }}"
action:
  - action: notify.mobile_app_phone
    data:
      message: "Label not printed: {{ trigger.payload_json.message }}"
```

The service checks the printer every `status_interval_secs` while idle (and after every
job). Each check connects briefly, so another device can't print at exactly that moment;
keep the interval at a few minutes.

## Status

Verified on an LW-C410: connection, automatic channel lookup, status and tape detection,
text + QR labels, cutting, end-of-job detection, stop/retry logic (without
printer). Not yet confirmed on paper: see "Not yet confirmed on paper" in
`docs/DEVELOPMENT.md`. Protocol details: `docs/PROTOCOL.md`.

## Licenses

The bundled assets keep their own licenses (in `crates/lwprint/assets/`): Material Design
Icons (Apache 2.0), Noto Emoji (SIL Open Font License 1.1), unicode-emoji-json (MIT).
No license has been chosen for lwprint's own code yet; add a `LICENSE` file before
publishing it.
