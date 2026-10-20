"""Logging forward proxy. Logs the first request line + Host of every connection.
CONNECT host:port -> tunnels to the real host (so HTTPS keeps working) unless BLOCK_CONNECT=1.
Absolute-form http://host:port/... -> forwarded; hosts ending in .test are sent to 127.0.0.1:<MOCKPORT>."""
import socket, sys, threading, json, time, os

port = int(sys.argv[1]); logp = sys.argv[2]; mockport = int(os.environ.get("MOCKPORT", "18080"))
block = os.environ.get("BLOCK_CONNECT") == "1"
lk = threading.Lock()

def log(o):
    with lk, open(logp, "a") as f:
        f.write(json.dumps(o) + "\n")

def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    except Exception:
        pass
    finally:
        for s in (a, b):
            try: s.shutdown(socket.SHUT_RDWR)
            except Exception: pass

def handle(c):
    buf = b""
    while b"\r\n\r\n" not in buf:
        d = c.recv(65536)
        if not d: return
        buf += d
    head, rest = buf.split(b"\r\n\r\n", 1)
    lines = head.decode(errors="replace").split("\r\n")
    reqline = lines[0]
    host = next((l.split(":", 1)[1].strip() for l in lines[1:] if l.lower().startswith("host:")), None)
    hdrs = [l for l in lines[1:] if not l.lower().startswith(("authorization", "proxy-authorization"))]
    log({"t": time.time(), "requestline": reqline, "host": host, "headers": hdrs})
    method, target, ver = reqline.split(" ", 2)
    if method == "CONNECT":
        if block:
            c.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n"); c.close(); return
        h, p = target.rsplit(":", 1)
        try:
            u = socket.create_connection((h, int(p)), timeout=10)
        except Exception as e:
            c.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n"); c.close(); return
        c.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        threading.Thread(target=pipe, args=(u, c), daemon=True).start(); pipe(c, u); return
    if target.startswith("http://"):
        hp = target[7:].split("/", 1)[0]; path = "/" + (target[7:].split("/", 1)[1] if "/" in target[7:] else "")
        h, p = (hp.rsplit(":", 1) + ["80"])[:2] if ":" in hp else (hp, "80")
        if h.endswith(".test"): h, p = "127.0.0.1", str(mockport)
        try:
            u = socket.create_connection((h, int(p)), timeout=10)
        except Exception:
            c.sendall(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n"); c.close(); return
        newhead = (f"{method} {path} {ver}\r\n" + "\r\n".join(lines[1:]) + "\r\n\r\n").encode()
        u.sendall(newhead + rest)
        threading.Thread(target=pipe, args=(u, c), daemon=True).start(); pipe(c, u); return
    c.sendall(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n"); c.close()

s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("127.0.0.1", port)); s.listen(64)
while True:
    c, _ = s.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
