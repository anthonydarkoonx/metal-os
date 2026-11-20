#!/usr/bin/env python3
"""USB input test: boot the kernel under QEMU with an xHCI controller, a USB keyboard and a USB tablet, press keys and move the pointer through QMP, and check the
driver reported each one. Usage: usb-test.py '<qemu command line>' flat|hub|many (the command line must not already have a -qmp option; `hub`: the devices sit behind a hub,
so the hub path is the only way to them; `flat`: they are on root ports, and the test also plugs a second keyboard in and out and holds a key down)."""
import json, os, shlex, socket, subprocess, sys, time

SOCK = "build-usb.sock"
LOG = "build-usb.log"


def main():
    cmd = shlex.split(sys.argv[1]) + ["-qmp", f"unix:{SOCK},server,nowait"]
    if os.path.exists(SOCK):
        os.remove(SOCK)
    log = open(LOG, "w")
    qemu = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    try:
        sys.exit(run(qemu))
    finally:
        qemu.kill()
        qemu.wait()


def wait_for(text, timeout):
    end = time.time() + timeout
    while time.time() < end:
        with open(LOG, errors="replace") as f:
            data = f.read()
        if text in data:
            return data
        time.sleep(0.3)
    return None


def qmp(sock, f, obj):
    sock.sendall((json.dumps(obj) + "\n").encode())
    while True:
        line = f.readline()
        if not line:
            raise RuntimeError("QMP closed")
        msg = json.loads(line)
        if "return" in msg or "error" in msg:
            return msg


def run(qemu):
    mode = sys.argv[2] if len(sys.argv) > 2 else "flat"
    if wait_for("[kbd] usb ready", 90) is None:
        print("== USB: FAILED (the input driver never reported a USB controller) ==")
        print(open(LOG, errors="replace").read()[-1500:])
        return 1
    for _ in range(50):
        if os.path.exists(SOCK):
            break
        time.sleep(0.1)
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK)
    f = s.makefile("r")
    f.readline()  # greeting
    qmp(s, f, {"execute": "qmp_capabilities"})

    def key(q, down):
        return {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": q}}}

    def send(device, events):
        # (no device named: with only these two input devices, keys go to the USB keyboard and pointer events to the tablet)
        r = qmp(s, f, {"execute": "input-send-event", "arguments": {"events": events}})
        if "error" in r:
            raise RuntimeError(r)
        time.sleep(0.4)

    time.sleep(1.0)
    send("kbd0", [key("a", True)])
    send("kbd0", [key("a", False)])
    send("kbd0", [key("shift", True)])
    send("kbd0", [key("b", True)])
    send("kbd0", [key("b", False)])
    send("kbd0", [key("shift", False)])
    send("kbd0", [key("ret", True)])
    send("kbd0", [key("ret", False)])
    send("tab0", [{"type": "abs", "data": {"axis": "x", "value": 16384}}, {"type": "abs", "data": {"axis": "y", "value": 8192}}])
    send("tab0", [{"type": "btn", "data": {"down": True, "button": "left"}}])
    send("tab0", [{"type": "btn", "data": {"down": False, "button": "left"}}])
    time.sleep(1.0)
    extra = []
    if mode == "flat":
        # a held key repeats
        send("kbd0", [key("x", True)])
        time.sleep(1.5)
        send("kbd0", [key("x", False)])
        # a keyboard plugged in while the system runs is found, and gone when unplugged
        r = qmp(s, f, {"execute": "device_add", "arguments": {"driver": "usb-kbd", "id": "kbd9", "bus": "xhci.0"}})
        time.sleep(2.0)
        mid = open(LOG, errors="replace").read()
        r2 = qmp(s, f, {"execute": "device_del", "arguments": {"id": "kbd9"}})
        time.sleep(2.0)
        end = open(LOG, errors="replace").read()
        extra = [
            ("a held key repeats", end.count("[usb] key 45 repeat") >= 2),
            ("a keyboard plugged in later is found", mid.count("[usb] device attached") >= 3 and "error" not in r),
            ("a keyboard unplugged is forgotten", "device removed" in end[len(mid):] and "error" not in r2),
        ]
    elif mode == "many":
        o = open(LOG, errors="replace").read()
        extra = [("the hub is found", "[usb] hub attached" in o), ("all ten devices are found (more than the old limit of six)", o.count("[usb] device attached") >= 9 and "too many devices" not in o)]
    else:
        extra = [("the hub is found", "[usb] hub attached" in open(LOG, errors="replace").read()), ("the devices behind it are found", open(LOG, errors="replace").read().count("[usb] device attached") >= 2)]
    out = open(LOG, errors="replace").read()
    ok = True
    expect = [
        ("a pressed", "[usb] key 30 down"), ("a released", "[usb] key 30 up"),
        ("shift pressed", "[usb] key 42 down"), ("b pressed", "[usb] key 48 down"), ("shift released", "[usb] key 42 up"),
        ("enter pressed", "[usb] key 28 down"), ("pointer position", "[usb] pointer at 16384 8192"),
        ("button down", "[usb] button 0 down"), ("button up", "[usb] button 0 up"),
    ]
    for what, text in expect:
        found = text in out or (what == "pointer position" and "[usb] pointer at 16383 8191" in out)
        print(f"[usb-test] TEST {what} reaches the driver: {'PASS' if found else 'FAIL'}")
        ok &= found
    for what, found in extra:
        print(f"[usb-test] TEST {what}: {'PASS' if found else 'FAIL'}")
        ok &= found
    if "PANIC" in out:
        print("[usb-test] the kernel panicked")
        ok = False
    print("== USB: ALL PASS ==" if ok else "== USB: FAILED ==")
    if not ok:
        print(out[-2000:])
    return 0 if ok else 1


main()
