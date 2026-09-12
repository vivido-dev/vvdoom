"""Exercise the Windows input reader in an isolated ConPTY (Python stdlib only).

Run from vvdoom: python tests/windows_input.py
The probe verifies actual VT decoding and console restoration. The driver checks
that a press arrives within 500 ms and no synthetic release appears while held.
"""
import ctypes as c
import argparse
from ctypes import wintypes as w
import json
from pathlib import Path
import queue
import subprocess
import threading
import time


class Coord(c.Structure):
    _fields_ = [("X", c.c_short), ("Y", c.c_short)]


class StartupInfo(c.Structure):
    _fields_ = [("cb", w.DWORD), ("reserved", w.LPWSTR), ("desktop", w.LPWSTR),
                ("title", w.LPWSTR), ("x", w.DWORD), ("y", w.DWORD),
                ("xsize", w.DWORD), ("ysize", w.DWORD), ("xchars", w.DWORD),
                ("ychars", w.DWORD), ("fill", w.DWORD), ("flags", w.DWORD),
                ("show", w.WORD), ("reserved_size", w.WORD), ("reserved2", c.c_void_p),
                ("stdin", w.HANDLE), ("stdout", w.HANDLE), ("stderr", w.HANDLE)]


class StartupInfoEx(c.Structure):
    _fields_ = [("startup", StartupInfo), ("attributes", c.c_void_p)]


class ProcessInfo(c.Structure):
    _fields_ = [("process", w.HANDLE), ("thread", w.HANDLE),
                ("pid", w.DWORD), ("tid", w.DWORD)]


k = c.WinDLL("kernel32", use_last_error=True)
signatures = {
    "CreatePipe": ([c.POINTER(w.HANDLE), c.POINTER(w.HANDLE), c.c_void_p, w.DWORD], w.BOOL),
    "CreatePseudoConsole": ([Coord, w.HANDLE, w.HANDLE, w.DWORD, c.POINTER(w.HANDLE)], c.c_long),
    "InitializeProcThreadAttributeList": ([c.c_void_p, w.DWORD, w.DWORD, c.POINTER(c.c_size_t)], w.BOOL),
    "UpdateProcThreadAttribute": ([c.c_void_p, w.DWORD, c.c_size_t, c.c_void_p, c.c_size_t, c.c_void_p, c.c_void_p], w.BOOL),
    "CreateProcessW": ([w.LPCWSTR, w.LPWSTR, c.c_void_p, c.c_void_p, w.BOOL, w.DWORD,
                        c.c_void_p, w.LPCWSTR, c.POINTER(StartupInfoEx), c.POINTER(ProcessInfo)], w.BOOL),
    "ReadFile": ([w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p], w.BOOL),
    "WriteFile": ([w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p], w.BOOL),
    "WaitForSingleObject": ([w.HANDLE, w.DWORD], w.DWORD),
    "GetExitCodeProcess": ([w.HANDLE, c.POINTER(w.DWORD)], w.BOOL),
    "TerminateProcess": ([w.HANDLE, w.UINT], w.BOOL),
    "CloseHandle": ([w.HANDLE], w.BOOL),
    "ClosePseudoConsole": ([w.HANDLE], None),
    "DeleteProcThreadAttributeList": ([c.c_void_p], None),
}
for name, (args, result) in signatures.items():
    getattr(k, name).argtypes = args
    getattr(k, name).restype = result


def check(ok):
    if not ok:
        raise c.WinError(c.get_last_error())


def main():
    options = argparse.ArgumentParser()
    options.add_argument("--conpty", help="Test the same conpty.dll used by Vivido")
    args = options.parse_args()
    console_api = c.WinDLL(args.conpty, use_last_error=True) if args.conpty else k
    for name in ("CreatePseudoConsole", "ClosePseudoConsole"):
        getattr(console_api, name).argtypes, getattr(console_api, name).restype = signatures[name]
    root = Path(__file__).resolve().parent.parent
    build = subprocess.run(["cargo", "test", "--no-run", "--message-format=json"],
                           cwd=root, check=True, capture_output=True, text=True)
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith("{")]
    exe = next(a["executable"] for a in artifacts if a.get("executable") and a.get("profile", {}).get("test"))
    read_in, write_in, read_out, write_out, console = [w.HANDLE() for _ in range(5)]
    check(k.CreatePipe(c.byref(read_in), c.byref(write_in), None, 0))
    check(k.CreatePipe(c.byref(read_out), c.byref(write_out), None, 0))
    result = console_api.CreatePseudoConsole(Coord(80, 24), read_in, write_out, 0, c.byref(console))
    if result != 0:
        raise RuntimeError(f"CreatePseudoConsole: {result:#x}")
    k.CloseHandle(read_in)
    k.CloseHandle(write_out)
    size = c.c_size_t()
    k.InitializeProcThreadAttributeList(None, 1, 0, c.byref(size))
    attributes = c.create_string_buffer(size.value)
    check(k.InitializeProcThreadAttributeList(attributes, 1, 0, c.byref(size)))
    check(k.UpdateProcThreadAttribute(attributes, 0, 0x20016, console, c.sizeof(console), None, None))
    startup = StartupInfoEx()
    startup.startup.cb = c.sizeof(startup)
    # Null explicit handles let ConPTY supply console handles rather than inheriting
    # the test runner's redirected standard streams.
    startup.startup.flags = 0x100
    startup.attributes = c.cast(attributes, c.c_void_p)
    process = ProcessInfo()
    command = c.create_unicode_buffer(subprocess.list2cmdline([
        exe, "--exact", "windows_input::tests::conpty_input_probe", "--ignored", "--nocapture"]))
    check(k.CreateProcessW(None, command, None, None, False, 0x80000, None, str(root),
                           c.byref(startup), c.byref(process)))
    chunks = queue.Queue()
    output = bytearray()

    def reader():
        buffer, count = c.create_string_buffer(4096), w.DWORD()
        while k.ReadFile(read_out, buffer, len(buffer), c.byref(count), None) and count.value:
            chunks.put(buffer.raw[:count.value])

    threading.Thread(target=reader, daemon=True).start()

    def collect_until(marker, timeout):
        deadline = time.monotonic() + timeout
        while marker not in output:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"Timed out waiting for {marker!r}: {bytes(output)!r}")
            try:
                output.extend(chunks.get(timeout=remaining))
            except queue.Empty:
                continue

    def send(data):
        count = w.DWORD()
        check(k.WriteFile(write_in, data, len(data), c.byref(count), None))
        assert count.value == len(data)

    try:
        collect_until(b"INPUT_READY", 5)
        assert b"\x1b[>11u" in output, "enhanced keyboard request did not reach the terminal"
        directions = [(b"1", b"D"), (b"1", b"C")] * 2
        directions += [(b"97", b"u"), (b"100", b"u")] * 2
        directions += [(b"119", b"u"), (b"115", b"u")] * 2
        for index, (number, end) in enumerate(directions):
            start = time.monotonic()
            send(b"\x1b[" + (b"" if end in (b"C", b"D") else number) + end)
            collect_until(f"INPUT_EVENT {index * 2 + 1}".encode(), 0.5)
            print(f"Key {index + 1}: press delivered in {(time.monotonic() - start) * 1000:.1f} ms")
            time.sleep(0.1)
            while not chunks.empty():
                output.extend(chunks.get_nowait())
            assert f"INPUT_EVENT {index * 2 + 2}".encode() not in output, "synthetic key release"
            send(b"\x1b[" + number + b";1:3" + end)
            collect_until(f"INPUT_EVENT {index * 2 + 2}".encode(), 0.5)
        collect_until(b"INPUT_VERIFIED", 2)
        assert k.WaitForSingleObject(process.process, 2000) == 0
        exit_code = w.DWORD()
        check(k.GetExitCodeProcess(process.process, c.byref(exit_code)))
        assert exit_code.value == 0
        print("ConPTY input, real releases, shutdown, and exact console mode restoration passed.")
    finally:
        if k.WaitForSingleObject(process.process, 0) != 0:
            k.TerminateProcess(process.process, 1)
        k.CloseHandle(process.thread)
        k.CloseHandle(process.process)
        k.CloseHandle(write_in)
        console_api.ClosePseudoConsole(console)
        k.CloseHandle(read_out)
        k.DeleteProcThreadAttributeList(attributes)


if __name__ == "__main__":
    main()


