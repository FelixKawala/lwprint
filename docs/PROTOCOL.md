# Epson LabelWorks protocol notes

Everything here was learned from Epson's Label Editor Mobile app (Android, v1.8.0) and
verified on an **LW-C410** unless marked otherwise. See `REVERSE_ENGINEERING.md` for how.

Legend: **verified** = tested on the LW-C410 · **from app** = read from Epson's code, not
tested on hardware.

## Transport (verified)

* **Bluetooth Classic, Serial Port Profile (RFCOMM).** The LW-C410 advertises three SDP
  records:

  | Service | RFCOMM channel | Notes |
  |---|---|---|
  | `SerialPort` (UUID 0x1101) | **6** | the one to use |
  | `Wireless iAP` (UUID `00000000-deca-fade-deca-deafdecacaff`) | 3 | Apple MFi accessory protocol; answers in iAP frames (`FF 55 …`) |
  | PnP Information (0x1200) | – | |

  Channels 1 and 2 accept a connection and reset it immediately. Always look up the
  channel via SDP (`crates/lwprint/src/sdp.rs`); Epson's app connects by service UUID.
* **Encryption is required.** Set `BT_SECURITY` to *medium* before connecting.
  Unencrypted connections are reset by the printer; *high* fails with `EACCES` when the
  pairing used "Just Works" (no PIN), which is how the C410 pairs.
* **Keep the SDP connection open until the serial connection is up.** Closing the last
  connection to the printer makes the kernel start tearing down the Bluetooth link; an
  RFCOMM connection opened right then "succeeds" and the first write fails with
  `ENOTCONN` ("Transport endpoint is not connected"). `sdp::Lookup` holds the SDP socket,
  and `Printer::open` verifies the connection with a status request and retries once.
* **Only one connection at a time.** Connect per job and disconnect afterwards so other
  devices can print.
* **Linux queues writes in the kernel.** `write()` returns long before the data is on
  the air (a 4.3 KB label took ~1.6 s to transmit). Closing the socket early silently
  drops the rest; the printer then prints only the first lines. Wait until the send
  queue is empty: the `TIOCOUTQ` ioctl on a Bluetooth socket reports *free* space, so
  record its value right after connecting and wait until it returns to that value
  (`Printer::wait_until_sent`).
* The printer switches itself off after a while; connecting then times out.
* *From app:* some other models use Bluetooth Low Energy instead (service
  `0783b03e-8535-b5a0-7140-a304d2495cb7`), and Wi-Fi models use TCP port 9100.

## Framing (verified)

Control commands:

```
1B 7B <len> <cmd> <params…> <sum> 7D
```

* `len` = number of bytes from `cmd` through the closing `7D` (= params + 3)
* `sum` = low byte of (`cmd` + all params)

Example: status request `1B 7B 05 51 00 00 51 7D`.

## Commands

| Bytes (`cmd` + params) | Meaning | |
|---|---|---|
| `51 00 00` | request one status report | verified |
| `51 05 00` | report status automatically during the job | verified (see below) |
| `40` | reset / end of job | verified |
| `7B 00 00 'S' 'T'` | start of job | verified |
| `43 a b c d` | cut mode: `01 01 01 01` cut after each label, `01 00 01 01` cut after the job, `00 00 00 00` no cut | verified (each label); others from app |
| `44 n` | density, `n` = 0…10 (= −5…+5 + 5); default `05` | default verified, other values from app |
| `47` | end of job setup | verified |
| `4C <u32 LE>` | page length in raster lines | verified |
| `54 <u16 LE>` | page margin; only `0` tested, meaning not fully understood | verified with 0 |
| `2B 00` / `2B 01` | feed tape / feed and cut | from app, untested |

The printer clips each line to the printable width from the table below (tested on
12 mm tape: 76/80/84-pixel lines print no wider than 72) and centres that window on the
tape.

Raster line (not framed): `1B 2E 00 00 00 01 <n u16 LE>` followed by `ceil(n/8)` bytes,
`n` = pixels across the tape, first pixel in the highest bit. Epson's converter prints a
pixel when its grey value is below 140.

### A complete job (verified)

1. `51 00 00` → wait for the status report; check `ST` idle/finished, `ER` 0, `TW` tape.
2. `51 05 00`
3. Job setup: `40`, `7B 00 00 'S' 'T'`, `43 …`, `44 …`, `47`
4. Page setup: `4C <lines>`, `54 0000`
5. Raster lines, in bands of up to 1024 lines; Epson pauses **500 ms after each band** on
   the C410 (model number 1300).
6. `0C` end of page. For more labels (copies or several pages), repeat steps 4 to 6 for
   each label within the same job; the cut mode from step 3 decides where the printer
   cuts (from app: this is how Epson's app prints copies; verified: copies come out
   without the blank lead between them). Different labels can share a job the same way.
7. `40` end of job.
8. Wait until the kernel has transmitted everything (see Transport).
9. Poll `51 00 00` about every 0.7 s: `ST` is `02` while printing and `05` when finished.

Pitfall: right after `51 05 00` the printer sends **one** status report immediately,
still showing idle. Treating that as "finished" closes the connection too early. Only
accept idle after `ST 02` was seen (or after a generous timeout); `ST 05` is a reliable
"finished".

### Orientation (verified with a calibration print)

The first raster line ends up at the far end of the label, and the first pixel of each
line at the top edge. A bitmap with x along the tape and y across it is therefore sent
with line `i` = column `width-1-i`, pixel `j` = row `j` (`protocol::raster_bands`,
`Orientation::Normal`). `Orientation::Rotated` turns it 180°.

## Status report (verified)

64 bytes of text:

```
@BDC ST\r\nST:00;ER:00;WR:00;RV:00;TW:03;TR:00;CT:00;OP:00;EI:00;\xFF
```

Fields are `KEY:hh;` with two hex digits.

**`ST` – state** (names from the app): `00` idle, `01` feeding, `02` printing, `03`
receiving data, `04` feed finished, `05` print finished, `13` firmware update. A fresh
connection after a job may still report `05`; treat it like idle.

**`ER` – error** (names from the app; `22` seen on hardware): `01` cutter error, `06` no
tape cartridge, `15` head overheated, `20` cancelled on the printer, `21` cover open,
`22` low voltage, `23` cancelled by power off, `24` tape eject error, `30` tape feed
error, `40` ink ribbon slack, `41` ink ribbon short, `42` tape end, `43` cut label error,
`44` temperature error, `5C` stopped by auto power off, `5D` stopped by power supply
change. Observed: `ER:22` together with `EI:08` and `ST:05` at the end of a successful
job running on weak batteries.

**`TW` – tape width** (from the app's `tapeWidthFromLWStatusTWCode`; `03` verified):

| `TW` | Tape | Tape kind (app enum) |
|---|---|---|
| `00` | none | 0 |
| `0B` | 4 mm | 1 |
| `01` | 6 mm | 2 |
| `02` | 9 mm | 3 |
| `03` | 12 mm | 4 |
| `04` | 18 mm | 5 |
| `05` | 24 mm | 6 |
| `06` | 36 mm | 7 |
| `11` | cable 24 mm | 8 |
| `12` | cable 36 mm | 9 |
| `21` | 50 mm | 10 |
| `23` | 100 mm | 11 |
| `07` | new 50 mm | 12 |

Codes `51`–`5B` mean the same as `01`–`0B`.

`WR`, `RV`, `TR`, `CT`, `OP`, `EI` are not decoded yet (`EI` looks like extra error info).

Label length: Epson's app never sends labels shorter than **10 mm**
(`ObjectData.getMinTapeLength`, 1 cm) or longer than 1 m. The LW-C410 itself pads every
**page** shorter than about **20 mm** (verified roughly: 6, 15 and 20 mm labels all came
out about 2 cm long, centred), whether or not it cuts between pages. Epson's spec sheets
list a minimum print margin of 2 mm and a minimum lead waste of 12 mm (the head-to-cutter
distance, paid by the first label of each job). Copies printed as one job come out without the blank lead between labels
(verified); the first label of every job still gets the lead, i.e. the distance between
print head and cutter.

Timing observed on the LW-C410: about 1.6 s Bluetooth transfer for a 35 mm label and
roughly 3–4 mm/s printing, so ~13 s per short label including feed and cut.

## Other models (from app, untested)

Epson's model table (`LWPrintProductInformation`):

| Model | Number | dpi | Status length | Command level | Half cut | Tape kinds |
|---|---|---|---|---|---|---|
| LW-1000P | 1000 | 360 | 20 | 3 | yes | 2–7 |
| LW-600P | 1001 | 180 | 8 | 1 | no | 2–6 |
| OK1000P | 1100 | 360 | 20 | 3 | yes | 1–7 |
| OK600P | 1101 | 180 | 8 | 1 | no | 1–6 |
| LW-PX800 | 1200 | 360 | 20 | 3 | yes | 1–7 |
| LW-PX400 | 1201 | 180 | 8 | 1 | no | 1–6 |
| **LW-C410** | 1300 | 180 | 8 | 1 | no | 1–5 |
| LW-C610 | 1301 | 360 | 78 | 5 | no | 1–6 |
| LW-C650 | 1302 | 360 | 22 | 5 | no | 1–6 |
| LW-Z710 | 1400 | 180 | 8 | 1 | no | 1–6 |
| LW-Z5000 | 1500 | 300 | 78 | 4 | yes | 1–7, 12 |
| LW-Z5010 | 1501 | 300 | 78 | 4 | yes | 1–7, 12 |
| LW-C630 | 1600 | 180 | 22 | 5 | no | 2–6 |
| LW-Z730 | 1601 | 180 | 22 | 5 | no | 1–6 |
| MP100 | 10000 | 180 | 8 | 1 | no | 2–6 |
| MP500 | 10001 | 300 | 78 | 4 | yes | 2–7, 12 |
| MP550 | 10002 | 300 | 78 | 4 | yes | 2–7, 12 |

Printable pixels across the tape, by tape kind and resolution:

| Tape kind | 180 dpi | 270 dpi | 360 dpi | 300 dpi |
|---|---|---|---|---|
| 1 (4 mm) | 22 | 33 | 44 | 36 |
| 2 (6 mm) | 36 | 54 | 72 | 60 |
| 3 (9 mm) | 54 | 81 | 108 | 90 |
| 4 (12 mm) | 72 | 108 | 144 | 120 |
| 5 (18 mm) | 108 | 162 | 216 | 180 |
| 6 (24 mm) | 128 | 192 | 288 | 240 |
| 7 (36 mm) | 192 | 288 | 384 | 361 |
| 10 (50 mm) | 297 | 446 | 595 | 496 |
| 11 (100 mm) | 652 | 979 | 1305 | 1088 |
| 12 (new 50 mm) | 297 | 446 | 595 | 566 |

Exceptions: LW-C410 on 18 mm tape → **96** pixels (special case in the app, used in
`model.rs`; not yet printed on 18 mm); LW-C610/C650 on 24 mm tape → 256.

What differs for other models, as far as the app shows:

* **Page setup** depends on the *command level*; level 1 (C410) sends only `4C` and `54`.
  Higher levels add fields. Get exact bytes by emulating
  `CommandFilter::makePageEnvironmentCommand` with another level
  (`tools/re/example_page_env.py`).
* **Status**: models with a status length other than 8 use a second Bluetooth access path
  in the app (`AccessManagerBluetooth2`) with different status handling. Not analysed.
* **Resolution** 360/300 dpi changes the pixel sizes above; the protocol framing is the same.
