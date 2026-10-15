#!/usr/bin/python3
# Sends a print job saved with `lwprint print ... --dump job.bin` using the same
# Python connection as the working print_test.py. Usage: python3 send_dump.py job.bin
import re, socket, struct, sys, time

MAC = "44:B7:D0:64:FC:0E"
CHANNEL = 6
SOL_BLUETOOTH, BT_SECURITY, BT_SECURITY_MEDIUM = 274, 4, 2
END = bytes.fromhex("0c 1b 7b 03 40 40 7d")  # end of page + end of job

job = open(sys.argv[1], "rb").read()
if not job.endswith(END):
    sys.exit("This does not look like an lwprint job dump.")

s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
s.setsockopt(SOL_BLUETOOTH, BT_SECURITY, struct.pack("BB", BT_SECURITY_MEDIUM, 0))
s.settimeout(10)
s.connect((MAC, CHANNEL))


def read_status(wait):
    s.settimeout(0.2)
    data, end = b"", time.time() + wait
    while time.time() < end:
        try:
            chunk = s.recv(1024)
            if not chunk:
                break
            data += chunk
        except socket.timeout:
            pass
        if b"\xff" in data:
            break
    return data


s.sendall(bytes.fromhex("1b 7b 05 51 00 00 51 7d"))
print("Status:", read_status(3))
input("Press Enter to send the job...")
s.sendall(job[: -len(END)])
time.sleep(0.5)
s.sendall(END)
print("Sent", len(job), "bytes. Waiting for the printer...")
print("Status:", read_status(40))
s.close()
