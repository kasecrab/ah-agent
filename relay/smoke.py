#!/usr/bin/env python3
"""Drive the relay over HTTP and check it turns away everyone it should.

Start a relay first — either the real one or the mock beside this file — then
run this. The signing key and hub name come from the harness's own key ladder
rather than from a second copy of it, so what is tested is the relay against
the thing that will really be talking to it.

    npx wrangler dev &            # or: python3 relay/mock.py 8799
    python3 relay/smoke.py        # AH_RELAY_PORT=8799 for the mock
"""

import base64
import hashlib
import hmac
import json
import os
import socket
import struct
import subprocess
import sys
import time

BASE_HOST = os.environ.get("AH_RELAY_HOST", "127.0.0.1")
BASE_PORT = int(os.environ.get("AH_RELAY_PORT", "8787"))
PROTO_VERSION = 1

failures = []


def b64u(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def unb64u(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def hkdf(salt: bytes, ikm: bytes, info: bytes, length: int) -> bytes:
    """HKDF-SHA256, the ladder the harness derives everything from."""
    prk = hmac.new(salt, ikm, hashlib.sha256).digest()
    out, block, counter = b"", b"", 1
    while len(out) < length:
        block = hmac.new(prk, block + info + bytes([counter]), hashlib.sha256).digest()
        out += block
        counter += 1
    return out[:length]


def pairing(code: bytes) -> tuple[str, bytes]:
    """`(hub id, relay key)` for a pairing code."""
    salt = b"ah-remote v1"
    return hkdf(salt, code, b"hub", 16).hex(), hkdf(salt, code, b"relay", 32)


def vectors() -> dict:
    """The key ladder, straight out of the Rust that the harness will use."""
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out = subprocess.run(
        ["cargo", "test", "-p", "ah-remote", "--lib", "vectors",
         "--", "--ignored", "--nocapture"],
        capture_output=True, text=True, check=True, cwd=root,
    ).stdout
    got = {}
    for line in out.splitlines():
        parts = line.split(" ", 1)
        if len(parts) == 2 and parts[0] in (
            "hub", "relay_key", "ts", "nonce", "sig_desk", "sig_phone",
            "stale_ts", "sig_stale",
        ):
            got[parts[0]] = parts[1].strip()
    return got


def request(method: str, path: str, body: str | None = None,
            upgrade: bool = False) -> tuple[int, str]:
    """One HTTP/1.1 exchange. Raw, because a 101 is one of the answers."""
    head = [f"{method} {path} HTTP/1.1", f"Host: {BASE_HOST}:{BASE_PORT}",
            "Connection: close"]
    if upgrade:
        head = head[:-1] + [
            "Connection: Upgrade", "Upgrade: websocket",
            "Sec-WebSocket-Version: 13",
            "Sec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==",
        ]
    if body is not None:
        head += ["Content-Type: application/json",
                 f"Content-Length: {len(body)}"]
    raw = ("\r\n".join(head) + "\r\n\r\n" + (body or "")).encode()

    with socket.create_connection((BASE_HOST, BASE_PORT), timeout=10) as sock:
        sock.sendall(raw)
        sock.settimeout(5)
        buf = b""
        try:
            while b"\r\n\r\n" not in buf and len(buf) < 65536:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                buf += chunk
            # An upgrade that succeeded holds the socket open and has no body
            # to read; one that was refused is an ordinary response, and its
            # body is the part worth reading.
            refused = not buf.startswith(b"HTTP/1.1 101")
            while refused and len(buf) < 65536:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                buf += chunk
        except socket.timeout:
            pass
    text = buf.decode("utf-8", "replace")
    status = int(text.split(" ", 2)[1]) if text.startswith("HTTP/") else 0
    payload = text.split("\r\n\r\n", 1)[1] if "\r\n\r\n" in text else ""
    return status, payload


class Socket:
    """Just enough WebSocket to say things and hear them back."""

    def __init__(self, path: str):
        self.sock = socket.create_connection((BASE_HOST, BASE_PORT), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((
            f"GET {path} HTTP/1.1\r\nHost: {BASE_HOST}:{BASE_PORT}\r\n"
            "Connection: Upgrade\r\nUpgrade: websocket\r\n"
            f"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n"
        ).encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                break
            head += chunk
        self.status = int(head.split(b" ")[1]) if head.startswith(b"HTTP/") else 0
        self.rest = head.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in head else b""

    def send(self, obj: dict) -> None:
        raw = json.dumps(obj).encode()
        mask = os.urandom(4)
        head = bytearray([0x81])
        if len(raw) < 126:
            head.append(0x80 | len(raw))
        else:
            head.append(0x80 | 126)
            head += struct.pack(">H", len(raw))
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(raw))
        self.sock.sendall(bytes(head) + mask + body)

    def frames(self, want: int, seconds: float = 3.0) -> list[dict]:
        """Up to `want` frames, or whatever arrived before time ran out."""
        out: list[dict] = []
        end = time.time() + seconds
        buf = bytearray(self.rest)
        self.rest = b""
        while len(out) < want and time.time() < end:
            self.sock.settimeout(max(0.05, end - time.time()))
            try:
                chunk = self.sock.recv(8192)
                if not chunk:
                    break
                buf += chunk
            except (socket.timeout, OSError):
                break
            while True:
                frame, buf = unframe(buf)
                if frame is None:
                    break
                if frame:
                    out.append(json.loads(frame))
        return out

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def unframe(buf: bytearray) -> tuple[str | None, bytearray]:
    """One unmasked server frame off the front of `buf`."""
    if len(buf) < 2:
        return None, buf
    length, at = buf[1] & 0x7F, 2
    if length == 126:
        if len(buf) < 4:
            return None, buf
        length, at = struct.unpack(">H", buf[2:4])[0], 4
    elif length == 127:
        if len(buf) < 10:
            return None, buf
        length, at = struct.unpack(">Q", buf[2:10])[0], 10
    if len(buf) < at + length:
        return None, buf
    body = bytes(buf[at:at + length])
    rest = buf[at + length:]
    if buf[0] & 0x0F != 0x1:
        return "", rest
    return body.decode("utf-8", "replace"), rest


def check(name: str, got, want) -> None:
    if got == want:
        print(f"  ok    {name}")
    else:
        print(f"  FAIL  {name}: got {got!r}, wanted {want!r}")
        failures.append(name)


def main() -> int:
    v = vectors()

    # Before trusting anything this script derives or signs, check it against
    # what the harness produced for the same code. If these disagree, every
    # result below is meaningless.
    print("the ladder")
    fixed = bytes([42]) * 20
    hub_fixed, key_fixed = pairing(fixed)
    check("python derives the hub rust derives", hub_fixed, v["hub"])
    check("python derives the key rust derives", b64u(key_fixed), v["relay_key"])

    def signer(hub: str, key: bytes):
        def sign(role: str, ts: int, nonce: str) -> str:
            message = f"ah/v1 connect|{hub}|{role}|{ts}|{nonce}".encode()
            return b64u(hmac.new(key, message, hashlib.sha256).digest())
        return sign

    check("python signs what rust signs",
          signer(hub_fixed, key_fixed)("desk", int(v["ts"]), v["nonce"]),
          v["sig_desk"])
    if failures:
        return 1

    # A pairing of its own, so a second run starts from an unprovisioned hub
    # rather than from whatever the last one left behind.
    hub, key = pairing(os.urandom(20))
    sign = signer(hub, key)

    now = int(time.time() * 1000)
    nonce = v["nonce"]
    unknown = "0" * 32

    print("relay")
    check("it is alive", request("GET", "/health")[0], 200)
    check("a hub nobody paired is not there",
          request("GET", f"/hub/{unknown}?r=desk&ts={now}&n={nonce}"
                         f"&h={sign('desk', now, nonce)}", upgrade=True)[0], 404)
    check("a name that is not a hub is refused",
          request("GET", "/hub/nonsense", upgrade=True)[0], 400)

    body = json.dumps({"relay_key": b64u(key)})
    check("pairing is accepted",
          request("POST", f"/hub/{hub}/provision", body)[0], 200)
    check("pairing twice is not",
          request("POST", f"/hub/{hub}/provision", body)[0], 409)

    check("an unsigned socket is refused",
          request("GET", f"/hub/{hub}?r=desk", upgrade=True)[0], 401)
    check("a forged signature is refused",
          request("GET", f"/hub/{hub}?r=desk&ts={now}&n={nonce}&h=bm90YXNpZw",
                  upgrade=True)[0], 401)
    check("a signature for the other role is refused",
          request("GET", f"/hub/{hub}?r=desk&ts={now}&n={nonce}"
                         f"&h={sign('phone', now, nonce)}", upgrade=True)[0], 401)

    stale = now - 3_600_000
    status, payload = request(
        "GET", f"/hub/{hub}?r=desk&ts={stale}&n={nonce}&h={sign('desk', stale, nonce)}",
        upgrade=True)
    check("a clock far out is refused", status, 401)
    check("and is told so", "skew" in payload, True)

    check("a signed desktop gets in",
          request("GET", f"/hub/{hub}?r=desk&ts={now}&n={nonce}"
                         f"&h={sign('desk', now, nonce)}", upgrade=True)[0], 101)
    check("a signed phone gets in",
          request("GET", f"/hub/{hub}?r=phone&ts={now}&n=BBBBBBBBBBBBBBBB"
                         f"&h={sign('phone', now, 'BBBBBBBBBBBBBBBB')}",
                  upgrade=True)[0], 101)

    # The relay cannot read a payload, so this does not give it one: what is
    # being checked is the numbering, the fan-out and the replay.
    print("frames")
    desk = Socket(f"/hub/{hub}?r=desk&ts={now}&n={nonce}&h={sign('desk', now, nonce)}")
    check("the desktop is connected", desk.status, 101)
    phone = Socket(f"/hub/{hub}?r=phone&ts={now}&n=PPPPPPPPPPPPPPPP"
                   f"&h={sign('phone', now, 'PPPPPPPPPPPPPPPP')}")
    check("the phone is connected", phone.status, 101)

    link = "ab" * 16
    phone.send({"t": "sub", "v": PROTO_VERSION, "since": 0, "max": 100})
    for i in range(1, 4):
        desk.send({"t": "pub", "v": PROTO_VERSION, "link": link, "seq": i,
                   "ct": f"sealed-{i}"})
    got = phone.frames(3)
    check("three frames arrive", [f.get("ct") for f in got],
          ["sealed-1", "sealed-2", "sealed-3"])
    check("and are numbered in order", [f.get("n") for f in got], [1, 2, 3])

    # A phone that went away and came back asks for what it missed.
    phone.close()
    later = Socket(f"/hub/{hub}?r=phone&ts={now}&n=QQQQQQQQQQQQQQQQ"
                   f"&h={sign('phone', now, 'QQQQQQQQQQQQQQQQ')}")
    later.send({"t": "sub", "v": PROTO_VERSION, "since": 2, "max": 100})
    got = later.frames(1)
    check("only what was missed is replayed", [f.get("ct") for f in got], ["sealed-3"])

    # And what it says reaches the desktop as it was sent.
    later.send({"t": "cmd", "v": PROTO_VERSION, "link": link, "plink": "cd" * 16,
                "seq": 1, "ct": "from-the-phone"})
    got = desk.frames(1)
    check("a command reaches the desktop", [f.get("ct") for f in got],
          ["from-the-phone"])

    desk.close()
    orphan = Socket(f"/hub/{hub}?r=phone&ts={now}&n=RRRRRRRRRRRRRRRR"
                    f"&h={sign('phone', now, 'RRRRRRRRRRRRRRRR')}")
    orphan.send({"t": "cmd", "v": PROTO_VERSION, "link": link, "plink": "cd" * 16,
                 "seq": 1, "ct": "nobody-is-home"})
    got = orphan.frames(1)
    check("with no desktop, a command is refused rather than kept",
          [f.get("e") for f in got], ["offline"])
    later.close()
    orphan.close()

    print()
    if failures:
        print(f"{len(failures)} failed: {', '.join(failures)}")
        return 1
    print("all good")
    return 0


if __name__ == "__main__":
    sys.exit(main())
