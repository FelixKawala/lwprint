#!/usr/bin/python3
# Asks an Epson LW-C410 for its status over Bluetooth serial. Prints nothing on the tape.
import socket, struct, sys, time

MAC = "44:B7:D0:64:FC:0E"
STATUS_REQUEST = bytes.fromhex("1b 7b 05 51 00 00 51 7d")
SOL_BLUETOOTH, BT_SECURITY, BT_SECURITY_MEDIUM = 274, 4, 2  # medium = encrypted link

def try_channel(ch):
    s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
    s.setsockopt(SOL_BLUETOOTH, BT_SECURITY, struct.pack("BB", BT_SECURITY_MEDIUM, 0))
    s.settimeout(5)
    data = b""
    try:
        s.connect((MAC, ch))
        time.sleep(0.3)  # give the printer a moment after connecting
        s.sendall(STATUS_REQUEST)
        end = time.time() + 3
        while time.time() < end:
            try:
                chunk = s.recv(1024)
            except socket.timeout:
                break
            if not chunk:
                break
            data += chunk
        result = "ok"
    except OSError as e:
        result = f"{type(e).__name__}: {e}"
    finally:
        s.close()
    print(f"channel {ch}: {result}, {len(data)} bytes")
    if data:
        print("  hex:  ", data.hex(" "))
        print("  ascii:", repr(data))
    return bool(data)

channels = [int(a) for a in sys.argv[1:]] or [1, 2, 3]
for ch in channels:
    if try_channel(ch):
        break
    time.sleep(1)
