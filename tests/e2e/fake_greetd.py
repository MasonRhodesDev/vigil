#!/usr/bin/env python3
"""Minimal fake greetd: greetd-ipc wire protocol (native u32 length + JSON)."""
import json, os, socket, struct, sys

path, password = sys.argv[1], sys.argv[2]
try:
    os.unlink(path)
except FileNotFoundError:
    pass
srv = socket.socket(socket.AF_UNIX)
srv.bind(path)
srv.listen(1)
print("fake-greetd: listening", flush=True)

def recv(c):
    hdr = c.recv(4, socket.MSG_WAITALL)
    if len(hdr) < 4:
        return None
    (n,) = struct.unpack("=I", hdr)
    return json.loads(c.recv(n, socket.MSG_WAITALL))

def send(c, obj):
    b = json.dumps(obj).encode()
    c.sendall(struct.pack("=I", len(b)) + b)

c, _ = srv.accept()
print("fake-greetd: client connected", flush=True)
while True:
    req = recv(c)
    if req is None:
        print("fake-greetd: client gone", flush=True)
        break
    t = req["type"]
    print("fake-greetd: <-", t, flush=True)
    if t == "create_session":
        send(c, {"type": "auth_message", "auth_message_type": "secret", "auth_message": "Password:"})
    elif t == "post_auth_message_response":
        if req.get("response") == password:
            send(c, {"type": "success"})
        else:
            send(c, {"type": "error", "error_type": "auth_error", "description": "Wrong password"})
    elif t == "start_session":
        print("fake-greetd: START_SESSION cmd=%s" % (req.get("cmd"),), flush=True)
        send(c, {"type": "success"})
    elif t == "cancel_session":
        send(c, {"type": "success"})
