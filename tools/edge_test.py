#!/usr/bin/python3
# Measures where the printable area actually sits on the tape, and whether the printer
# prints lines wider than Epson's table allows (72 pixels on 12 mm tape).
#
# Usage: /usr/bin/python3 edge_test.py 72     (then 76, 80, 84)
#
# Each label (about 30 mm) has:
#   * a solid black bar across all pixels of the line (first 8 mm)
#   * a gap, then thin lines on the first two and last two pixel rows
#   * a 1-pixel line on the middle row
# Measure the white gap above and below the black bar with a ruler or a photo.
# Never use more than 85 pixels on 12 mm tape (that is the full tape width).
import re, socket, struct, sys, time

MAC = "44:B7:D0:64:FC:0E"
CHANNEL = 6
SOL_BLUETOOTH, BT_SECURITY, BT_SECURITY_MEDIUM = 274, 4, 2
LINES = 216  # 30 mm at 180 dpi


def frame(cmd, params=b""):
    body = bytes([cmd]) + params
    return b"\x1b\x7b" + bytes([len(body) + 2]) + body + bytes([sum(body) & 0xFF]) + b"\x7d"


def statuses(s, wait):
    s.settimeout(0.2)
    buf, out, end = b"", [], time.time() + wait
    while time.time() < end:
        try:
            chunk = s.recv(1024)
            if not chunk:
                break
            buf += chunk
        except socket.timeout:
            pass
        while (i := buf.find(b"@")) >= 0 and len(buf) - i >= 64:
            block, buf = buf[i:i + 64], buf[i + 64:]
            if block[63] == 0xFF:
                out.append({k.decode(): int(v, 16) for k, v in re.findall(rb"([A-Z]{2}):([0-9A-Fa-f]{2});", block)})
        if out:
            break
    return out


def pattern(width):
    rows = []
    for y in range(LINES):            # y = position along the tape
        row = bytearray((width + 7) // 8)
        for x in range(width):        # x = pixel across the tape
            black = (
                y < 57                                      # solid bar, 8 mm
                or (y >= 80 and (x < 2 or x >= width - 2))  # edge lines
                or (y >= 80 and x == width // 2)            # middle line
            )
            if black:
                row[x // 8] |= 0x80 >> (x % 8)
        rows.append(bytes(row))
    return rows


def main():
    width = int(sys.argv[1]) if len(sys.argv) > 1 else 72
    if not 8 <= width <= 85:
        sys.exit("width must be between 8 and 85 pixels (85 = full 12 mm tape)")
    s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
    s.setsockopt(SOL_BLUETOOTH, BT_SECURITY, struct.pack("BB", BT_SECURITY_MEDIUM, 0))
    s.settimeout(10)
    s.connect((MAC, CHANNEL))
    s.sendall(frame(0x51, b"\x00\x00"))
    st = statuses(s, 3)
    print("Status:", st[-1] if st else None)
    if not st or st[-1].get("ER") or st[-1].get("ST") not in (0, 5) or st[-1].get("TW") != 3:
        sys.exit("Printer not ready, or not 12 mm tape. This test is meant for 12 mm tape.")
    if input(f"Print a {width}-pixel test label? [y/N] ").strip().lower() != "y":
        return
    line_cmd = b"\x1b\x2e\x00\x00\x00\x01" + struct.pack("<H", width)
    s.sendall(frame(0x51, b"\x05\x00"))
    s.sendall(frame(0x40) + frame(0x7B, b"\x00\x00ST") + frame(0x43, b"\x01\x01\x01\x01")
              + frame(0x44, bytes([5])) + frame(0x47))
    s.sendall(frame(0x4C, struct.pack("<I", LINES)) + frame(0x54, struct.pack("<H", 0)))
    s.sendall(b"".join(line_cmd + r for r in pattern(width)))
    time.sleep(0.5)
    s.sendall(b"\x0c" + frame(0x40))
    # Poll until the printer reports "print finished" (05) or idle after printing.
    time.sleep(2)
    seen_printing, end = False, time.time() + 60
    while time.time() < end:
        s.sendall(frame(0x51, b"\x00\x00"))
        for st in statuses(s, 3):
            print("  ", st)
            if st.get("ST") == 2:
                seen_printing = True
            if st.get("ER") and st.get("ER") != 0x22:
                sys.exit(f"Printer error {st['ER']:02X}")
            if st.get("ST") == 5 or (st.get("ST") == 0 and seen_printing):
                print("Done.")
                s.close()
                return
        time.sleep(0.7)
    print("Timed out; check the label.")
    s.close()


if __name__ == "__main__":
    main()
