#!/usr/bin/env python3
"""A relay, in memory, for testing the halves that talk through one.

Speaks the same envelope protocol as the Worker in `relay/`, so `ah` and the
phone example can be driven end to end without a Cloudflare account and
without a wasm toolchain. It stores ciphertext it cannot read, exactly as the
real one does, and it is deliberately the only part of this that is a toy: no
persistence, no pruning, no quota.

    python3 relay/mock.py [port]

Then, against it:

    ah remote pair --url http://127.0.0.1:8787
"""

import base64
import hashlib
import hmac
import json
import os
import socket
import struct
import sys
import threading
import time

SKEW_MS = 5 * 60 * 1000
PROTO = 1
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def b64u(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def unb64u(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


class Hub:
    """One pairing: a key it cannot decrypt with, a log, and its sockets."""

    def __init__(self, relay_key: bytes):
        self.relay_key = relay_key
        self.log: list[dict] = []
        self.peers: list["Peer"] = []
        self.lock = threading.Lock()

    def append(self, link: str, seq: int, ct: str) -> int:
        with self.lock:
            n = len(self.log) + 1
            self.log.append({"n": n, "link": link, "seq": seq, "ct": ct})
            return n

    def desk(self):
        return next((p for p in self.peers if p.role == "desk"), None)

    def phones(self):
        return [p for p in self.peers if p.role == "phone"]


HUBS: dict[str, Hub] = {}
HUBS_LOCK = threading.Lock()


class Peer:
    """One socket, and the frame codec it needs."""

    def __init__(self, sock: socket.socket, role: str, hub: Hub):
        self.sock = sock
        self.role = role
        self.hub = hub
        self.sub = False
        self.since = 0
        self.lock = threading.Lock()

    def send(self, payload: str) -> None:
        raw = payload.encode()
        head = bytearray([0x81])
        if len(raw) < 126:
            head.append(len(raw))
        elif len(raw) < 65536:
            head.append(126)
            head += struct.pack(">H", len(raw))
        else:
            head.append(127)
            head += struct.pack(">Q", len(raw))
        with self.lock:
            try:
                self.sock.sendall(bytes(head) + raw)
            except OSError:
                pass

    def read(self) -> str | None:
        """One text frame, reassembled. None when the socket is done."""
        chunks = bytearray()
        while True:
            head = self.recv_exactly(2)
            if head is None:
                return None
            fin, opcode = head[0] & 0x80, head[0] & 0x0F
            masked, length = head[1] & 0x80, head[1] & 0x7F
            if length == 126:
                more = self.recv_exactly(2)
                if more is None:
                    return None
                length = struct.unpack(">H", more)[0]
            elif length == 127:
                more = self.recv_exactly(8)
                if more is None:
                    return None
                length = struct.unpack(">Q", more)[0]
            key = self.recv_exactly(4) if masked else b"\0\0\0\0"
            if key is None:
                return None
            body = self.recv_exactly(length) if length else b""
            if body is None:
                return None
            body = bytes(b ^ key[i % 4] for i, b in enumerate(body))
            if opcode == 0x8:
                return None
            if opcode == 0x9:
                self.pong(body)
                continue
            if opcode == 0xA:
                continue
            chunks += body
            if fin:
                return chunks.decode("utf-8", "replace")

    def pong(self, body: bytes) -> None:
        with self.lock:
            try:
                self.sock.sendall(bytes([0x8A, len(body)]) + body)
            except OSError:
                pass

    def recv_exactly(self, n: int) -> bytes | None:
        buf = bytearray()
        while len(buf) < n:
            try:
                chunk = self.sock.recv(n - len(buf))
            except OSError:
                return None
            if not chunk:
                return None
            buf += chunk
        return bytes(buf)


def http_error(sock: socket.socket, status: int, body: str = "") -> None:
    sock.sendall(
        f"HTTP/1.1 {status} x\r\nContent-Length: {len(body)}\r\n"
        f"Content-Type: application/json\r\nConnection: close\r\n\r\n{body}".encode()
    )
    sock.close()


def serve(sock: socket.socket) -> None:
    head = bytearray()
    while b"\r\n\r\n" not in head:
        chunk = sock.recv(4096)
        if not chunk:
            return sock.close()
        head += chunk
    text, _, rest = head.partition(b"\r\n\r\n")
    lines = text.decode("latin1").split("\r\n")
    method, target, _ = lines[0].split(" ")
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    path, _, query = target.partition("?")
    q = dict(
        (kv.split("=", 1) + [""])[:2] for kv in query.split("&") if kv
    ) if query else {}
    q = {k: unquote(v) for k, v in q.items()}

    if path == "/health":
        return http_error(sock, 200, "ok")

    parts = [p for p in path.split("/") if p]
    if len(parts) < 2 or parts[0] != "hub":
        return http_error(sock, 404)
    hub_id = parts[1]
    if len(hub_id) != 32 or any(c not in "0123456789abcdef" for c in hub_id):
        return http_error(sock, 400)

    if method == "POST" and parts[-1] == "provision":
        length = int(headers.get("content-length", "0"))
        body = bytes(rest)
        while len(body) < length:
            body += sock.recv(4096)
        key = json.loads(body or b"{}").get("relay_key", "")
        with HUBS_LOCK:
            if hub_id in HUBS:
                return http_error(sock, 409)
            HUBS[hub_id] = Hub(unb64u(key))
        return http_error(sock, 200, "paired")

    hub = HUBS.get(hub_id)
    if hub is None:
        return http_error(sock, 404)

    role, ts, nonce, sig = q.get("r"), q.get("ts"), q.get("n"), q.get("h")
    if role not in ("desk", "phone") or not ts or not nonce or not sig:
        return http_error(sock, 401)
    now = int(time.time() * 1000)
    if abs(now - int(ts)) > SKEW_MS:
        return http_error(sock, 401, json.dumps({"e": "skew", "server_ms": now}))
    message = f"ah/v1 connect|{hub_id}|{role}|{ts}|{nonce}".encode()
    want = b64u(hmac.new(hub.relay_key, message, hashlib.sha256).digest())
    if not hmac.compare_digest(want, sig):
        return http_error(sock, 401)
    if role == "desk" and hub.desk() is not None:
        return http_error(sock, 409, "a desktop is already connected")

    accept = base64.b64encode(
        hashlib.sha1((headers.get("sec-websocket-key", "") + GUID).encode()).digest()
    ).decode()
    sock.sendall(
        ("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
         f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").encode()
    )

    peer = Peer(sock, role, hub)
    hub.peers.append(peer)
    try:
        pump(peer)
    finally:
        if peer in hub.peers:
            hub.peers.remove(peer)
        sock.close()


def pump(peer: Peer) -> None:
    hub = peer.hub
    while True:
        text = peer.read()
        if text is None:
            return
        try:
            frame = json.loads(text)
        except ValueError:
            continue
        if frame.get("v") != PROTO:
            return
        kind = frame.get("t")

        if kind == "pub" and peer.role == "desk":
            n = hub.append(frame["link"], frame["seq"], frame["ct"])
            out = json.dumps({
                "t": "evt", "v": PROTO, "link": frame["link"],
                "seq": frame["seq"], "ct": frame["ct"], "n": n,
            })
            for phone in hub.phones():
                if phone.sub:
                    phone.send(out)

        elif kind == "cmd" and peer.role == "phone":
            desk = hub.desk()
            if desk is None:
                peer.send(json.dumps({"t": "ctl", "v": PROTO, "e": "offline"}))
            else:
                desk.send(text)

        elif kind == "sub" and peer.role == "phone":
            since = int(frame.get("since", 0))
            limit = max(1, min(int(frame.get("max", 200)), 500))
            with hub.lock:
                kept = [row for row in hub.log if row["n"] > since][:limit]
                oldest = hub.log[0]["n"] if hub.log else 0
            if since > 0 and oldest > since + 1:
                peer.send(json.dumps({"t": "gap", "v": PROTO, "from": oldest}))
            for row in kept:
                peer.send(json.dumps({
                    "t": "evt", "v": PROTO, "link": row["link"],
                    "seq": row["seq"], "ct": row["ct"], "n": row["n"],
                }))
            peer.sub = True
            peer.since = kept[-1]["n"] if kept else since


def unquote(s: str) -> str:
    out, i = [], 0
    while i < len(s):
        if s[i] == "%" and i + 2 < len(s) + 1:
            try:
                out.append(chr(int(s[i + 1:i + 3], 16)))
                i += 3
                continue
            except ValueError:
                pass
        out.append(s[i])
        i += 1
    return "".join(out)


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8787
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(16)
    print(f"mock relay on http://127.0.0.1:{port} (pid {os.getpid()})", flush=True)
    while True:
        conn, _ = server.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
