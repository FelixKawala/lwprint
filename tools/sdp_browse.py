#!/usr/bin/python3
# Lists the Bluetooth services the printer advertises (like `sdptool browse`),
# including which RFCOMM channel each one uses. Prints nothing on the tape.
import socket, struct, sys

MAC = sys.argv[1] if len(sys.argv) > 1 else "44:B7:D0:64:FC:0E"
SDP_PSM = 1

KNOWN = {0x1101: "Serial Port", 0x1200: "PnP Information", 0x0003: "RFCOMM",
         0x0100: "L2CAP", 0x1002: "Public Browse Group"}


def parse(buf, i=0):
    """Parse one SDP data element; returns (value, next_index)."""
    hdr = buf[i]; i += 1
    typ, sz = hdr >> 3, hdr & 7
    if typ == 0:
        return None, i
    if sz < 5:
        n = [1, 2, 4, 8, 16][sz]
    else:
        lb = [1, 2, 4][sz - 5]
        n = int.from_bytes(buf[i:i + lb], "big"); i += lb
    raw = buf[i:i + n]; end = i + n
    if typ in (1, 2, 5):
        return int.from_bytes(raw, "big", signed=(typ == 2)), end
    if typ == 3:
        if n in (2, 4):
            return ("uuid", int.from_bytes(raw, "big")), end
        h = raw.hex()
        return ("uuid", f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"), end
    if typ in (4, 8):
        return raw.decode("utf-8", "replace").rstrip("\0"), end
    if typ in (6, 7):
        items, j = [], i
        while j < end:
            v, j = parse(buf, j)
            items.append(v)
        return items, end
    return raw, end


def uuid_name(u):
    return f"{KNOWN.get(u, '')} (0x{u:04x})".strip() if isinstance(u, int) else u


def rfcomm_channel(pdl):
    for proto in pdl or []:
        if isinstance(proto, list) and proto and proto[0] == ("uuid", 0x0003) and len(proto) > 1:
            return proto[1]
    return None


def query(sock):
    # ServiceSearchAttributeRequest: all services containing L2CAP (= everything), all attributes
    pattern = bytes([0x35, 0x03, 0x19, 0x01, 0x00])
    attrs = bytes([0x35, 0x05, 0x0A, 0x00, 0x00, 0xFF, 0xFF])
    cont, data, tid = b"\x00", b"", 1
    while True:
        params = pattern + struct.pack(">H", 0xFFFF) + attrs + cont
        sock.send(struct.pack(">BHH", 0x06, tid, len(params)) + params)
        resp = sock.recv(4096)
        if resp[0] != 0x07:
            raise RuntimeError(f"unexpected SDP response: {resp.hex(' ')}")
        count = struct.unpack(">H", resp[5:7])[0]
        data += resp[7:7 + count]
        cont = resp[7 + count:]
        tid += 1
        if cont[0] == 0:
            return data


def main():
    s = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_SEQPACKET, socket.BTPROTO_L2CAP)
    s.settimeout(10)
    s.connect((MAC, SDP_PSM))
    records, _ = parse(query(s))
    s.close()
    for rec in records:
        a = dict(zip(rec[0::2], rec[1::2]))
        classes = [uuid_name(u[1]) for u in a.get(0x0001, []) if isinstance(u, tuple)]
        name = a.get(0x0100, "")
        ch = rfcomm_channel(a.get(0x0004))
        print(f"- {name or '(no name)'}")
        print(f"    classes: {', '.join(classes) or '?'}")
        print(f"    RFCOMM channel: {ch if ch is not None else '-'}")


if __name__ == "__main__":
    main()
