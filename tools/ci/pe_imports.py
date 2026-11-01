#!/usr/bin/env python3
"""List the DLLs a Windows program (PE file) imports, and check that it needs nothing but the system's own DLLs.

    pe_imports.py <file.exe> [--system-dir C:\\Windows\\System32] [--allow NAME ...]

Reads the import table and the delay-load table with the standard library only. Prints every imported DLL. Fails (exit 1) when

  * a DLL of the C/C++ runtime is imported (vcruntime*, msvcp*, msvcr*, concrt*, vcomp*, ucrtbase, api-ms-win-crt-*): the program should
    carry the runtime inside it (`-C target-feature=+crt-static`), so a clean PC needs no Visual C++ Redistributable;
  * a DLL that is not a file of the system folder is imported (the `api-ms-win-*` and `ext-ms-win-*` API sets, which Windows resolves
    itself, are system by definition). Pass --allow for a DLL that is meant to sit next to the program.

Delay-loaded DLLs are listed too (a program may load one only when it needs it) and held to the same rule.
"""
import argparse
import os
import re
import struct
import sys

RUNTIME = re.compile(r"^(vcruntime|msvcp|msvcr|concrt|vcomp|vccorlib|ucrtbase|api-ms-win-crt-)", re.I)
APISET = re.compile(r"^(api|ext)-ms-win-", re.I)


def imports(path):
    data = open(path, "rb").read()
    if data[:2] != b"MZ":
        raise SystemExit(f"{path}: not a PE file")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise SystemExit(f"{path}: no PE header")
    nsec, = struct.unpack_from("<H", data, pe + 6)
    opt_size, = struct.unpack_from("<H", data, pe + 20)
    opt = pe + 24
    magic, = struct.unpack_from("<H", data, opt)
    dd = opt + (96 if magic == 0x10B else 112)  # data directories
    sections = []
    sec = opt + opt_size
    for i in range(nsec):
        name, vsize, va, rsize, raw = struct.unpack_from("<8sIIII", data, sec + 40 * i)
        sections.append((va, max(vsize, rsize), raw))

    def rva(r):
        for va, size, raw in sections:
            if va <= r < va + size:
                return r - va + raw
        raise SystemExit(f"{path}: bad RVA {r:#x}")

    def cstr(off):
        end = data.index(b"\0", off)
        return data[off:end].decode("ascii", "replace")

    found = {}
    # import directory (index 1): 20-byte descriptors ending in an all-zero one; the name RVA is the fourth field
    r, size = struct.unpack_from("<II", data, dd + 8 * 1)
    if r:
        off = rva(r)
        while True:
            fields = struct.unpack_from("<IIIII", data, off)
            if not any(fields):
                break
            found[cstr(rva(fields[3])).lower()] = "import"
            off += 20
    # delay-load directory (index 13): 32-byte descriptors; the name RVA is the second field
    r, size = struct.unpack_from("<II", data, dd + 8 * 13)
    if r:
        off = rva(r)
        while True:
            fields = struct.unpack_from("<8I", data, off)
            if not any(fields):
                break
            found.setdefault(cstr(rva(fields[1])).lower(), "delay-load")
            off += 32
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--system-dir", default=os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32"))
    ap.add_argument("--allow", nargs="*", default=[])
    ap.add_argument("--no-system-check", action="store_true", help="only look for the C runtime (for a machine without Windows)")
    a = ap.parse_args()
    found = imports(a.file)
    allow = {x.lower() for x in a.allow}
    bad = []
    print(f"{a.file} imports {len(found)} DLLs:")
    for name in sorted(found):
        notes = []
        if RUNTIME.match(name):
            notes.append("C/C++ RUNTIME")
            bad.append(f"{name}: the C/C++ runtime is not inside the program")
        elif name in allow:
            notes.append("allowed")
        elif APISET.match(name):
            notes.append("API set")
        elif not a.no_system_check and not os.path.isfile(os.path.join(a.system_dir, name)):
            notes.append("NOT A SYSTEM DLL")
            bad.append(f"{name}: not a file of {a.system_dir}")
        print(f"  {name:36} {found[name]:11} {' '.join(notes)}")
    if bad:
        print("FAILED:")
        for b in bad:
            print("  " + b)
        sys.exit(1)
    print("ok: only system DLLs, and no C/C++ runtime DLL")


if __name__ == "__main__":
    main()
