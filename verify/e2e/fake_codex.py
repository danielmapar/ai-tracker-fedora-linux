"""A stand-in for the Codex app-server, for the collector's end-to-end tests.

Serves the JSON-RPC calls the collector makes, either as the background service's control socket
(`serve_socket`: WebSocket at ws://localhost/rpc over a unix socket) or over stdio
(`python3 fake_codex.py app-server`, like the private `codex app-server` fallback).

Behaviour comes from a JSON file named by $FAKE_CODEX_CONFIG (re-read on every call):
  {"fail": {"account/usage/read": "simulated outage"}, "slow": {"account/usage/read": 9}}
Every call is appended to $FAKE_CODEX_LOG. All data is made up.
"""
import base64, hashlib, json, os, socket, struct, sys, threading, time


def _today(offset_days=0):
    return time.strftime("%Y-%m-%d", time.localtime(time.time() + offset_days * 86400))


def answer(method):
    now = int(time.time())
    cfg = {}
    try:
        with open(os.environ.get("FAKE_CODEX_CONFIG", "")) as f:
            cfg = json.load(f)
    except (OSError, ValueError):
        pass
    try:
        with open(os.environ["FAKE_CODEX_LOG"], "a") as f:
            f.write(method + "\n")
    except (KeyError, OSError):
        pass
    time.sleep(cfg.get("slow", {}).get(method, 0))
    if method in cfg.get("fail", {}):
        return None, cfg["fail"][method]
    if method == "initialize":
        return {"userAgent": "fake-codex/0.0"}, None
    if method == "account/read":
        return {"account": {"email": "codex-user@example.com", "planType": "plus", "type": "chatgpt"}}, None
    if method == "account/rateLimits/read":
        return {"rateLimits": {
            "primary": {"usedPercent": 7, "windowDurationMins": 300, "resetsAt": now + 4 * 3600},
            "secondary": {"usedPercent": 11, "windowDurationMins": 10080, "resetsAt": now + 3 * 86400},
            "credits": {"hasCredits": True, "unlimited": False, "balance": "500"},
            "rateLimitReachedType": None}}, None
    if method == "account/usage/read":
        days = [{"startDate": _today(-i), "tokens": 1_000_000 * (i + 1)} for i in range(9, -1, -1)]
        return {"dailyUsageBuckets": days, "summary": {"lifetimeTokens": 1_840_000_000}}, None
    if method == "remoteControl/status/read":
        return {"status": cfg.get("relay", "connected"), "serverName": "my-laptop",
                "installationId": "fake-installation", "environmentId": "fake-environment"}, None
    return None, f"unknown method {method}"


def reply(msg):
    """The response line for one JSON-RPC message, or None for a notification."""
    if "id" not in msg:
        return None
    result, err = answer(msg.get("method", ""))
    if err is not None:
        return {"id": msg["id"], "error": {"code": -32000, "message": err}}
    return {"id": msg["id"], "result": result}


# ---------- WebSocket over a unix socket (server side, text frames only) ----------

def _recv_exact(conn, n, buf):
    while len(buf) < n:
        chunk = conn.recv(65536)
        if not chunk:
            raise ConnectionError("closed")
        buf += chunk
    return buf[:n], buf[n:]


def _ws_conn(conn):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = conn.recv(4096)
        if not chunk:
            return
        buf += chunk
    head, buf = buf.split(b"\r\n\r\n", 1)
    key = next((l.split(b":", 1)[1].strip() for l in head.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key:")), b"")
    accept = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
    conn.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                 b"Sec-WebSocket-Accept: " + accept + b"\r\n\r\n")
    while True:
        hdr, buf = _recv_exact(conn, 2, buf)
        op, ln = hdr[0] & 0x0F, hdr[1] & 0x7F
        if ln == 126:
            ext, buf = _recv_exact(conn, 2, buf); ln = struct.unpack(">H", ext)[0]
        elif ln == 127:
            ext, buf = _recv_exact(conn, 8, buf); ln = struct.unpack(">Q", ext)[0]
        mask = b""
        if hdr[1] & 0x80:
            mask, buf = _recv_exact(conn, 4, buf)
        payload, buf = _recv_exact(conn, ln, buf)
        if mask:
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        if op == 8:  # close
            return
        if op != 1:
            continue
        out = reply(json.loads(payload))
        if out is None:
            continue
        data = json.dumps(out).encode()
        n = len(data)
        head = bytes([0x81]) + (bytes([n]) if n < 126 else bytes([126]) + struct.pack(">H", n) if n < 65536
                                else bytes([127]) + struct.pack(">Q", n))
        conn.sendall(head + data)


def serve_socket(path):
    """Serve the control socket at `path` in background threads; returns the listening socket."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if os.path.exists(path):
        os.unlink(path)
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(path)
    srv.listen(16)

    def loop():
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            def handle(c=conn):
                try:
                    _ws_conn(c)
                except (OSError, ConnectionError, ValueError):
                    pass
                finally:
                    c.close()
            threading.Thread(target=handle, daemon=True).start()

    threading.Thread(target=loop, daemon=True).start()
    return srv


if __name__ == "__main__":
    # `codex app-server` over stdio (the collector's private fallback). Anything else: a spy that
    # records the call and fails, so a test notices if the collector ever runs another codex command.
    if sys.argv[1:] == ["app-server"]:
        for line in sys.stdin:
            if line.strip():
                out = reply(json.loads(line))
                if out is not None:
                    sys.stdout.write(json.dumps(out) + "\n")
                    sys.stdout.flush()
    else:
        with open(os.environ.get("FAKE_CODEX_LOG", "/dev/null"), "a") as f:
            f.write("UNEXPECTED codex " + " ".join(sys.argv[1:]) + "\n")
        sys.exit(1)
