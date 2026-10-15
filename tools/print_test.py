#!/usr/bin/python3
# Prints one calibration label on an Epson LW-C410 over Bluetooth serial,
# using the protocol decoded from Epson's Label Editor app.
import re, socket, struct, sys, time

MAC = "44:B7:D0:64:FC:0E"
CHANNEL = 6
SOL_BLUETOOTH, BT_SECURITY, BT_SECURITY_MEDIUM = 274, 4, 2

# Status "TW" code -> (tape name, pixels across the tape at 180 dpi) for the LW-C410
TAPES = {0x0B: ("4 mm", 22), 0x01: ("6 mm", 36), 0x02: ("9 mm", 54),
         0x03: ("12 mm", 72), 0x04: ("18 mm", 96)}

LABEL_LINES = 360  # 360 lines at 180 dpi = 2 inches = about 51 mm


def frame(cmd, params=b""):
    """ESC { <len> <cmd> <params> <checksum> }"""
    body = bytes([cmd]) + params
    return b"\x1b\x7b" + bytes([len(body) + 2]) + body + bytes([sum(body) & 0xFF]) + b"\x7d"


class Printer:
    def __init__(self):
        self.s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
        self.s.setsockopt(SOL_BLUETOOTH, BT_SECURITY, struct.pack("BB", BT_SECURITY_MEDIUM, 0))
        self.s.settimeout(10)
        self.s.connect((MAC, CHANNEL))
        self.buf = b""

    def send(self, data):
        self.s.sendall(data)

    def statuses(self, wait):
        """Collect status blocks ('@' ... 0xFF, 64 bytes) arriving within `wait` seconds."""
        self.s.settimeout(0.2)
        end, found = time.time() + wait, []
        while time.time() < end:
            try:
                chunk = self.s.recv(1024)
                if not chunk:
                    break
                self.buf += chunk
            except socket.timeout:
                pass
            while True:
                i = self.buf.find(b"@")
                if i < 0 or len(self.buf) - i < 64:
                    break
                block = self.buf[i:i + 64]
                self.buf = self.buf[i + 64:]
                if block[63] == 0xFF:
                    found.append({k.decode(): int(v, 16) for k, v in
                                  re.findall(rb"([A-Z]{2}):([0-9A-Fa-f]{2});", block)})
            if found and wait < 1:
                break
        return found

    def close(self):
        self.s.close()


def test_bitmap(width, lines):
    """Calibration pattern: border, big block at the start, small block at the end."""
    def black(x, y):
        if x < 3 or x >= width - 3 or y < 3 or y >= lines - 3:
            return True                                   # border
        if y < 40 and x < width // 2:
            return True                                   # BIG block: first lines, first pixels
        if y >= lines - 25 and x >= width - 15:
            return True                                   # small block: last lines, last pixels
        if 80 <= y < lines - 60 and (y // 12) % 2 == 0 and 8 <= x < width - 8:
            return True                                   # stripes in the middle
        return False

    rows = []
    for y in range(lines):
        row = bytearray((width + 7) // 8)
        for x in range(width):
            if black(x, y):
                row[x // 8] |= 0x80 >> (x % 8)            # leftmost pixel = highest bit
        rows.append(bytes(row))
    return rows


def main():
    p = Printer()
    try:
        p.send(frame(0x51, b"\x00\x00"))                  # request status
        st = p.statuses(3)
        if not st:
            sys.exit("No status reply from the printer.")
        st = st[-1]
        print("Status:", st)
        if st.get("ST") != 0 or st.get("ER") != 0:
            sys.exit("Printer is busy or reports an error; not printing.")
        if st.get("TW") not in TAPES:
            sys.exit(f"Unknown or missing tape (TW={st.get('TW')}).")
        tape, width = TAPES[st["TW"]]
        print(f"Detected {tape} tape -> {width} pixels per line, {LABEL_LINES} lines.")
        if input("Print a ~51 mm calibration label? [y/N] ").strip().lower() != "y":
            return

        rows = test_bitmap(width, LABEL_LINES)
        p.send(frame(0x51, b"\x05\x00"))                  # automatic status during print
        # job setup (bytes match Epson's library: cut after label, default density)
        p.send(frame(0x40) + frame(0x7B, b"\x00\x00ST") + frame(0x43, b"\x01\x01\x01\x01")
               + frame(0x44, bytes([5])) + frame(0x47))
        # page setup: label length in lines, margin 0
        p.send(frame(0x4C, struct.pack("<I", LABEL_LINES)) + frame(0x54, struct.pack("<H", 0)))
        line_cmd = b"\x1b\x2e\x00\x00\x00\x01" + struct.pack("<H", width)
        for start in range(0, LABEL_LINES, 1024):         # Epson sends in bands of up to 1024 lines
            p.send(b"".join(line_cmd + r for r in rows[start:start + 1024]))
            time.sleep(0.5)                               # Epson pauses 0.5 s per band for the C410
        p.send(b"\x0c")                                   # end of page
        p.send(frame(0x40))                               # end of job

        print("Sent. Watching printer status...")
        seen_busy, end = False, time.time() + 40
        while time.time() < end:
            for s in p.statuses(0.5):
                print("  ", s)
                if s.get("ER"):
                    sys.exit(f"Printer reported error ER={s['ER']:02X}.")
                if s.get("ST"):
                    seen_busy = True
                elif seen_busy:
                    print("Done.")
                    return
        print("Stopped watching (timeout). Check the label anyway.")
    finally:
        p.close()


if __name__ == "__main__":
    main()
