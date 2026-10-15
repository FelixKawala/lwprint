#!/usr/bin/python3
# Diagnoses the LW-C410 Bluetooth serial connection. Prints nothing on the tape.
# For each channel and security level, reports at which step the connection fails.
import socket, struct, time

MAC = "44:B7:D0:64:FC:0E"
STATUS_REQUEST = bytes.fromhex("1b 7b 05 51 00 00 51 7d")
SOL_BLUETOOTH, BT_SECURITY = 274, 4
LEVELS = {"low": 1, "medium": 2, "high": 3}

def test(ch, level, send=True):
    stage, data = "socket", b""
    s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
    try:
        s.setsockopt(SOL_BLUETOOTH, BT_SECURITY, struct.pack("BB", LEVELS[level], 0))
        s.settimeout(8)
        stage = "connect"
        s.connect((MAC, ch))
        t0 = time.time()
        stage = "wait-before-send"
        s.settimeout(1)
        try:
            data += s.recv(1024)  # does the printer say anything on its own?
        except socket.timeout:
            pass
        if send:
            stage = "send"
            s.sendall(STATUS_REQUEST)
        stage = "receive"
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
        result = f"FAILED at {stage}: {type(e).__name__}: {e}"
    finally:
        s.close()
    label = f"ch {ch} security={level:<6} {'send' if send else 'no-send'}"
    print(f"{label}: {result}, {len(data)} bytes")
    if data:
        print("    hex:  ", data.hex(" "))
        print("    ascii:", repr(data))
    time.sleep(2)  # let the printer settle between attempts

for ch in (1, 2):
    test(ch, "low", send=False)
    for level in ("low", "medium", "high"):
        test(ch, level)
