"""Fails if a program imports the Visual C++ runtime (VCRUNTIME140.dll,
MSVCP140.dll, ucrtbase.dll or the api-ms-win-crt-* forwarders). The C
runtime is linked statically (.cargo/config.toml) so the programs run on a
clean Windows without the Visual C++ Redistributable.

    python tools/check-imports.py target/release/quickseek.exe ...
    python tools/check-imports.py          (the four release programs)

Reads the PE import and delay-load tables itself (no dumpbin needed)."""
import os
import re
import struct
import sys

CRT = re.compile(r"^(vcruntime\d+.*|msvcp\d+.*|ucrtbase.*|api-ms-win-crt-.*|concrt\d+.*)\.dll$", re.I)
ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
DEFAULT = [os.path.join(ROOT, "target", "release", n) for n in
           ("quickseek.exe", "quickseek-gui.exe", "quickseek-mcp.exe", "quickseek-setup.exe")]


def imports(path):
    """The DLL names a PE file imports (normal and delay-loaded)."""
    data = open(path, "rb").read()
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError(f"{path}: not a PE file")
    nsec, opt_size = struct.unpack_from("<H", data, pe + 6)[0], struct.unpack_from("<H", data, pe + 20)[0]
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    dirs = opt + (112 if magic == 0x20B else 96)
    sections = []
    for i in range(nsec):
        s = opt + opt_size + 40 * i
        vsize, va, rsize, raw = struct.unpack_from("<IIII", data, s + 8)
        sections.append((va, max(vsize, rsize), raw))

    def off(rva):
        for va, size, raw in sections:
            if va <= rva < va + size:
                return rva - va + raw
        raise ValueError(f"{path}: RVA {rva:#x} outside the sections")

    def name_at(rva):
        o = off(rva)
        return data[o:data.index(b"\0", o)].decode("ascii", "replace")

    names = []
    for index, size in ((1, 20), (13, 32)):  # imports, delay-load imports
        rva = struct.unpack_from("<I", data, dirs + 8 * index)[0]
        if not rva:
            continue
        o = off(rva)
        while True:
            if index == 1:
                name_rva = struct.unpack_from("<I", data, o + 12)[0]
                empty = data[o:o + size] == b"\0" * size
            else:
                name_rva = struct.unpack_from("<I", data, o + 4)[0]
                empty = name_rva == 0
            if empty:
                break
            names.append(name_at(name_rva))
            o += size
    return names


def main():
    paths = sys.argv[1:] or DEFAULT
    bad = 0
    for p in paths:
        found = imports(p)
        crt = [n for n in found if CRT.match(n)]
        if crt:
            bad += 1
            print(f"{os.path.basename(p)}: imports the C runtime: {', '.join(crt)}")
        else:
            print(f"{os.path.basename(p)}: no C runtime imports ({len(found)} DLLs: {', '.join(sorted(set(n.lower() for n in found)))})")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
