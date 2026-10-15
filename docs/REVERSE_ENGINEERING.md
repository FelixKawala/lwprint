# How the protocol was reverse-engineered

Notes for repeating the process, for example to support another LabelWorks model.
Reverse engineering for interoperability with hardware you own is generally permitted
(explicitly in the EU); keep Epson's code and binaries out of this repository.

## 1. Get the app

Epson Label Editor Mobile (`com.epson.labeleditormobile`), version 1.8.0 was used. It
ships as a split APK bundle (`.apks`): `base.apk`, `split_config.arm64_v8a.apk` (native
library) and a resource split. From a phone with the app installed:

```sh
adb shell pm path com.epson.labeleditormobile   # lists all split APKs
adb pull <each path>
```

(Wireless debugging works for this: Developer options → Wireless debugging,
`adb pair <ip>:<port>`, then `adb connect <ip>:<port>`.)

## 2. Decompile the Java/Kotlin side

[jadx](https://github.com/skylot/jadx) 1.5.1:

```sh
jadx -j 4 --no-res -d out base.apk
```

The printer SDK lives in `com.epson.lwprint.sdk`. Useful classes:

| Class | What it contains |
|---|---|
| `android.androidcore.PrintController` | the print job sequence: status checks, job/page setup, raster loop, band pauses (`Thread.sleep(500)` for model 1300), end-of-job handling |
| `android.androidcore.AccessManagerBluetooth` | status request bytes and parsing of the 64-byte status report |
| `android.androidcore.AccessManagerBluetooth2` | the alternative status path for other models |
| `android.androidcore.DeviceManagerBluetooth` | RFCOMM connection (secure socket by Serial Port UUID, fallback to channel 1 via reflection) |
| `android.androidcore.AccessManagerBLE` / `DeviceConnectionBLE` | BLE variant and its UUIDs |
| `LWPrintProductInformation` | model table and printable pixels per tape (see `PROTOCOL.md`) |
| `core.command.CommandFilter` | JNI wrapper around the native command builder |
| `core.command.PrintCoreStatus`, `PrintCoreDeviceStatus` | names of `ST` and `ER` codes |

Some byte arrays are written with constants from Apache POI that jadx inlined by name,
e.g. `{27, 123, 3, Ptg.CLASS_ARRAY, Ptg.CLASS_ARRAY, 125}`. Resolve them from the POI
sources in the same output: `Ptg.CLASS_ARRAY` = 0x40, `AreaErrPtg.sid` = 0x2B,
`RefNPtg.sid` = 0x2C. Where jadx fails to decompile a method, re-run it for that one
class with `--single-class <name> -m restructure --show-bad-code`.

## 3. The native command builder

`lib/arm64-v8a/libLWPrint.so` (from the arm64 split APK) builds the job and page setup
commands and converts bitmaps to raster data. Its C++ symbols are not stripped, e.g.:

* `CommandFilter::makeJobEnvironmentCommand(char*)`
* `CommandFilter::makePageEnvironmentCommand(char*, int, int, int, int)` (height, width,
  margin, command level)
* `CommandFilter::makeCommand(...)` (per-command switch, reads the print parameters)
* `CommandFilter::bitmapToRaster(int, int, unsigned char const*, unsigned char*)`
* `CommandFilter::tapeWidthFromLWStatusTWCode(int)`

Rather than reading all the assembly, the functions were **run in an emulator** to get
exact output bytes: `tools/re/emu.py` loads the library into
[Unicorn](https://www.unicorn-engine.org/), applies its relocations, stubs the few libc
functions it needs (`malloc`, `memcpy`, …) and calls methods directly. The print
parameters live in a `std::map<std::string, int>` inside the `CommandFilter` object; the
helpers in `emu.py` walk that map so values can be changed between calls.

```sh
pip install unicorn capstone pyelftools
LWPRINT_SO=path/to/libLWPrint.so python3 tools/re/example_page_env.py
```

`tools/re/lwdis.py` disassembles a function by address (`python3 lwdis.py <hex addr> <count>`).

## 4. Confirm on hardware

The Python scripts in `tools/` were used step by step and are handy for diagnosing a new
printer or setup:

| Script | Purpose |
|---|---|
| `sdp_browse.py` | list the printer's Bluetooth services and their RFCOMM channels |
| `status_diag.py` | try channels and security levels, report where a connection fails |
| `status.py` | request one status report on a given channel (prints nothing) |
| `print_test.py` | print a calibration label (border, blocks, stripes) to check orientation |
| `send_dump.py` | send a job saved with `lwprint print … --dump job.bin` over a plain Python socket |

All need the system Python with Bluetooth support (`/usr/bin/python3` on Debian/Ubuntu;
Python builds from pyenv/conda/uv often lack `socket.AF_BLUETOOTH`). Edit the MAC address
at the top of each script.

`lwprint -v …` logs every byte sent and every status received, with timestamps.

## Lessons learned

* Look up the RFCOMM channel via SDP; don't guess. Channel 3 on the C410 is Apple's iAP
  and answers with `FF 55 …` frames, which looks like garbage at first.
* An immediate connection reset usually means "no service on this channel" or
  "encryption required".
* Compare bytes, not behaviour: dumping the Rust job and diffing it against the working
  Python script isolated the kernel-queue bug in minutes.
* A status reply right after sending a job is not a completion signal.
