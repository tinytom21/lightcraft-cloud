#!/usr/bin/env python3
"""Parity harness: the Python tool (legacy/lightcraft_cloud.py, `lc-cloud`) and the Rust app (lightcraft-cloud-desktop)
publish the same generated photo folder to two local-folder remotes, then apply the same synthetic phone edits, and the
results are compared: remote file lists, manifests (field by field), thumbnails and smart previews (format and dimensions;
bytes are only reported), the sidecars after the inbox ran (byte for byte), local history, the sidecar version store, the
state rows, and the inbox lines of the log.

Everything happens under one work folder (default /tmp/lc-parity): two copies of a generated fixture, two throwaway
configs, two state folders, two remotes. The live configuration is never touched: the Python tool gets `--config` and
LC_CLOUD_CONFIG is removed from its environment (the tool reads ~/.config/lightcraft-cloud/config.toml only when it is
given no config at all), RCLONE_CONFIG points at an empty file, and the remotes are plain folders. Photos are generated
by `cargo run --example fixture` (procedural scenes from LightCraft's lightcraft-scenes); no real photo is used.

    tools/parity/parity.py [--work /tmp/lc-parity] [--keep]

Needs: rclone, the LightCraft Flatpak (ai.storyteller.lightcraft), cargo. Exit status 1 when an unexpected difference was
found. Differences that are known and intended are listed in EXPECTED and reported as "expected".
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "legacy"))
sys.path.insert(0, str(REPO / "legacy" / "tests"))
from test_lc import sidecar, with_settings  # the Python tests' LightCraft-style sidecar generator  # noqa: E402

# Differences that are intended; everything else is a failure.
EXPECTED = {
    "manifest.generated": "a timestamp",
    "manifest.publisher": "'lightcraft-cloud 0.1.0' (Python) vs 'lightcraft-cloud-desktop <version>' (Rust)",
    "state.note": "free-text notes",
    "state.last_counts.sc_changed": "the Rust app publishes a phone edit in the sync that applied it; the Python tool, whose scan sees the "
                                    "fresh sidecar as still being written, in the next one (so its round 4 still has sidecars to publish)",
}

problems = []   # (area, text) unexpected
expected = []   # (area, text)


def report(area, ok, text="", key=None):
    if ok:
        print(f"  PASS  {area}")
        return
    if key in EXPECTED:
        expected.append((area, text))
        print(f"  EXPECTED  {area}: {text}")
    else:
        problems.append((area, text))
        print(f"  DIFF  {area}: {text}")


def sh(cmd, env=None, cwd=None, check=True):
    r = subprocess.run(cmd, env=env, cwd=cwd, capture_output=True, text=True)
    if check and r.returncode != 0:
        sys.exit(f"command failed ({r.returncode}): {' '.join(map(str, cmd))}\n{r.stdout[-2000:]}\n{r.stderr[-2000:]}")
    return r


class Side:
    def __init__(self, work, name):
        self.name = name
        self.dir = work / name
        self.src = self.dir / "src"
        self.remote = self.dir / "remote"
        self.state = self.dir / "state"
        self.cfg = self.dir / "config.toml"
        self.log = []

    def write_config(self, extra=""):
        self.cfg.write_text(f'source = "{self.src}"\nremote = "{self.remote}"\nstate_dir = "{self.state}"\n'
                            f'exclude = ["exported"]\nnotify = false\n{extra}')


def run_python(side, work):
    env = {k: v for k, v in os.environ.items() if k != "LC_CLOUD_CONFIG"}
    env["RCLONE_CONFIG"] = str(work / "rclone.conf")
    r = sh([sys.executable, str(REPO / "legacy/lightcraft_cloud.py"), "--config", str(side.cfg), "sync"], env=env, check=False)
    side.log = [l.split(" ", 2)[2] for l in r.stdout.splitlines() if l.count(" ") >= 2]
    if r.returncode != 0:
        sys.exit(f"python sync failed:\n{r.stdout[-3000:]}\n{r.stderr[-2000:]}")
    return side.log


def run_rust(side, work, binary):
    env = {k: v for k, v in os.environ.items() if k not in ("LC_CLOUD_CONFIG",)}
    env["RCLONE_CONFIG"] = str(work / "rclone.conf")
    r = sh([str(binary), "sync", "--once", "--config", str(side.cfg)], env=env, check=False)
    side.log = [l.split(" ", 1)[1] for l in r.stdout.splitlines() if " " in l]
    if r.returncode != 0:
        sys.exit(f"rust sync failed:\n{r.stdout[-3000:]}\n{r.stderr[-2000:]}")
    return side.log


def files(root):
    root = Path(root)
    return sorted(str(p.relative_to(root)) for p in root.rglob("*") if p.is_file() and p.name != "lock")


def norm_path(p):
    p = re.sub(r"^(history|inbox-done)/\d{4}-\d{2}-\d{2}/", r"\1/<date>/", p)
    p = re.sub(r"\.\d{6}(?=(\.[^./]*)?$)", ".<HHMMSS>", p) if p.startswith("history/") else p
    return p


def norm_hist(p):
    return re.sub(r"pc-\d{8}T\d{6}Z", "pc-<mtime>", p)


def xmp_files(root):
    """The stored sidecars (the Rust app also keeps a `.stamp` beside the ones an edit made)."""
    return {p for p in files(root) if str(p).endswith(".xmp")}


def compare_sets(area, a, b, norm=lambda x: x):
    A, B = sorted(map(norm, a)), sorted(map(norm, b))
    if A == B:
        report(area, True)
    else:
        only_a = [x for x in A if x not in B]
        only_b = [x for x in B if x not in A]
        report(area, False, f"only Python: {only_a[:8]} / only Rust: {only_b[:8]}")


def jpeg_size(data):
    i = 2
    while i < len(data) - 9:
        if data[i] != 0xFF:
            i += 1
            continue
        m = data[i + 1]
        if m in (0xC0, 0xC1, 0xC2):
            return int.from_bytes(data[i + 7:i + 9], "big"), int.from_bytes(data[i + 5:i + 7], "big")
        i += 2 + int.from_bytes(data[i + 2:i + 4], "big")
    return None


def lcsp_header(data):
    magic, header, _ = data.split(b"\n", 2)
    return magic.decode(), json.loads(header)


def manifest_of(side):
    return json.loads((side.remote / "mobile/manifest.json").read_text())


def flat(photo):
    """A manifest photo with its alternates flattened, for field-by-field comparison."""
    out = {k: v for k, v in photo.items() if k != "alternates"}
    return out, photo.get("alternates", [])


def compare_manifest(py, rs):
    a, b = manifest_of(py), manifest_of(rs)
    report("manifest.format", a["format"] == b["format"] == 1, f"{a['format']} vs {b['format']}")
    report("manifest.generated", False, f"{a['generated']} vs {b['generated']}", key="manifest.generated")
    report("manifest.publisher", a["publisher"] == b["publisher"], f"{a['publisher']!r} vs {b['publisher']!r}", key="manifest.publisher")
    report("manifest.keys", set(a) == set(b), f"{sorted(a)} vs {sorted(b)}")
    pa, pb = a["photos"], b["photos"]
    report("manifest.photo order", [p["path"] for p in pa] == [p["path"] for p in pb], f"{[p['path'] for p in pa]} vs {[p['path'] for p in pb]}")
    by_b = {p["path"]: p for p in pb}
    diffs = []
    for p in pa:
        q = by_b.get(p["path"])
        if q is None:
            diffs.append(f"{p['path']}: only in Python")
            continue
        fa, alta = flat(p)
        fb, altb = flat(q)
        for k in sorted(set(fa) | set(fb)):
            if fa.get(k, "<absent>") != fb.get(k, "<absent>"):
                diffs.append(f"{p['path']}.{k}: {fa.get(k, '<absent>')!r} vs {fb.get(k, '<absent>')!r}")
        if len(alta) != len(altb):
            diffs.append(f"{p['path']}.alternates: {len(alta)} vs {len(altb)}")
        for x, y in zip(alta, altb):
            for k in sorted(set(x) | set(y)):
                if x.get(k, "<absent>") != y.get(k, "<absent>"):
                    diffs.append(f"{p['path']}.alternates[].{k}: {x.get(k, '<absent>')!r} vs {y.get(k, '<absent>')!r}")
    diffs += [f"{p}: only in Rust" for p in by_b if p not in {x['path'] for x in pa}]
    report("manifest.photos (every field, ids, paths, sidecars, alternates, previewKey/Bytes)", not diffs, "; ".join(diffs[:12]))
    # key order of the file itself
    ka = list(json.loads((py.remote / "mobile/manifest.json").read_text()).keys())
    kb = list(json.loads((rs.remote / "mobile/manifest.json").read_text()).keys())
    report("manifest.top-level key order", ka == kb, f"{ka} vs {kb}")
    pk_a = list(pa[0].keys()) if pa else []
    pk_b = list(pb[0].keys()) if pb else []
    report("manifest.photo key order", pk_a == pk_b, f"{pk_a} vs {pk_b}")


def compare_media(py, rs):
    ta = sorted(p for p in files(py.remote) if p.startswith("mobile/thumbs/"))
    tb = sorted(p for p in files(rs.remote) if p.startswith("mobile/thumbs/"))
    bad, same_bytes = [], 0
    for p in ta:
        if p not in tb:
            continue
        da, db = (py.remote / p).read_bytes(), (rs.remote / p).read_bytes()
        if da == db:
            same_bytes += 1
        sa, sb = jpeg_size(da), jpeg_size(db)
        if not (da[:2] == db[:2] == b"\xff\xd8") or sa != sb:
            bad.append(f"{p}: {sa} vs {sb}")
    report("thumbnails: JPEG, same dimensions", not bad and ta == tb, "; ".join(bad[:6]) or f"{ta} vs {tb}")
    print(f"        ({same_bytes} of {len(ta)} thumbnails are byte-identical)")
    pa = sorted(p for p in files(py.remote) if p.startswith("mobile/previews/"))
    pb = sorted(p for p in files(rs.remote) if p.startswith("mobile/previews/"))
    bad, same_bytes = [], 0
    for p in pa:
        if p not in pb:
            continue
        da, db = (py.remote / p).read_bytes(), (rs.remote / p).read_bytes()
        if da == db:
            same_bytes += 1
        try:
            if lcsp_header(da) != lcsp_header(db):
                bad.append(f"{p}: {lcsp_header(da)} vs {lcsp_header(db)}")
        except Exception as e:  # noqa
            bad.append(f"{p}: unreadable ({e})")
    report("smart previews: .lcsp, same header (size, encoding, look)", not bad and pa == pb, "; ".join(bad[:4]) or f"{pa} vs {pb}")
    print(f"        ({same_bytes} of {len(pa)} smart previews are byte-identical)")


def state_rows_python(side):
    db = sqlite3.connect(f"file:{side.state / 'state.sqlite'}?mode=ro", uri=True)
    db.row_factory = sqlite3.Row
    out = {}
    for r in db.execute("SELECT * FROM files"):
        meta = json.loads(r["meta"] or "{}")
        out[r["path"]] = dict(size=r["size"], mtime_ns=r["mtime_ns"], sidecar=r["sidecar"], sidecar_hash=r["sidecar_hash"], id=r["id"],
                              status=r["status"], meta=meta)
    kv = {r["k"]: r["v"] for r in db.execute("SELECT * FROM kv")}
    return out, kv


def state_rows_rust(side):
    st = json.loads((side.state / "state.json").read_text())
    out = {}
    for p, r in st["files"].items():
        out[p] = dict(size=r["size"], mtime_ns=r["mtime_ns"], sidecar=r["sidecar"], sidecar_hash=r["sidecar_hash"], id=r["id"],
                      status=r["status"], meta=r["meta"])
    return out, st["kv"]


def compare_state(py, rs):
    a, ka = state_rows_python(py)
    b, kb = state_rows_rust(rs)
    diffs = []
    for p in sorted(set(a) | set(b)):
        x, y = a.get(p), b.get(p)
        if x is None or y is None:
            diffs.append(f"{p}: only in {'Python' if y is None else 'Rust'}")
            continue
        for k in x:
            if k == "meta":
                for mk in sorted(set(x[k]) | set(y[k])):
                    if x[k].get(mk) != y[k].get(mk):
                        diffs.append(f"{p}.meta.{mk}: {x[k].get(mk)!r} vs {y[k].get(mk)!r}")
            elif x[k] != y[k]:
                diffs.append(f"{p}.{k}: {x[k]!r} vs {y[k]!r}")
    report("state rows (size, mtime, sidecar, hash, id, status, meta incl. previewKey)", not diffs, "; ".join(diffs[:10]))
    ca, cb = json.loads(ka.get("last_counts")), json.loads(kb.get("last_counts"))
    late = {k: v for k, v in ca.items() if k != "sc_changed"} == {k: v for k, v in cb.items() if k != "sc_changed"} and ca["sc_changed"] > cb["sc_changed"] == 0
    report("state.last_result / last_counts", ka.get("last_result") == kb.get("last_result") and ca == cb,
           f"{ka.get('last_result')}, {ka.get('last_counts')} vs {kb.get('last_result')}, {kb.get('last_counts')}",
           key="state.last_counts.sc_changed" if late else None)
    report("state.manifest_uploaded", ka.get("manifest_uploaded") == str(py.remote) and kb.get("manifest_uploaded") == str(rs.remote),
           f"{ka.get('manifest_uploaded')} vs {kb.get('manifest_uploaded')}")


def compare_round(py, rs, label):
    print(f"\n== {label}")
    compare_sets("remote file list", files(py.remote), files(rs.remote), norm_path)
    compare_manifest(py, rs)
    compare_media(py, rs)
    compare_state(py, rs)
    compare_sets("sidecar version store (names)", xmp_files(py.state / "sidecar-versions"), xmp_files(rs.state / "sidecar-versions"))
    # originals: the same bytes
    diffs = []
    for p in files(py.remote):
        if p.startswith("originals/") and (rs.remote / p).exists() and (py.remote / p).read_bytes() != (rs.remote / p).read_bytes():
            diffs.append(p)
    report("originals on the remote: same bytes", not diffs, str(diffs[:5]))
    diffs = []
    for p in files(py.src):
        if (py.src / p).read_bytes() != (rs.src / p).read_bytes():
            diffs.append(p)
    report("source folders: same bytes", not diffs and files(py.src) == files(rs.src), str(diffs[:5]))


STAMP = {}


def write(side, rel, data, age=1000):
    p = side.src / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    t = STAMP.setdefault((rel, age), time.time() - age)   # the same mtime on both sides
    os.utime(p, (t, t))


def put_inbox(side, name, data):
    p = side.remote / "inbox" / name
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)


def sha16(b):
    return hashlib.sha256(b).hexdigest()[:16]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", default="/tmp/lc-parity")
    ap.add_argument("--keep", action="store_true", help="do not delete the work folder first")
    ap.add_argument("--release", action="store_true")
    a = ap.parse_args()
    work = Path(a.work).resolve()
    if not str(work).startswith("/tmp/"):
        sys.exit("the work folder must be under /tmp")
    if not a.keep and work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True, exist_ok=True)
    (work / "rclone.conf").write_text("")
    env = dict(os.environ, CARGO_BUILD_JOBS="4")
    prof = ["--release"] if a.release else []
    print("building the Rust app and the fixture generator ...")
    sh(["cargo", "build", "-q", "-p", "lightcraft-cloud-desktop", "--bins", "--example", "fixture", *prof], cwd=REPO, env=env)
    target = REPO / "target" / ("release" if a.release else "debug")
    binary = target / "lightcraft-cloud-desktop"
    sh([str(target / "examples" / "fixture"), str(work / "fixture")])
    py, rs = Side(work, "python"), Side(work, "rust")
    for s in (py, rs):
        s.dir.mkdir(parents=True, exist_ok=True)
        shutil.copytree(work / "fixture", s.src, symlinks=True, copy_function=shutil.copy2)
        s.write_config()
    assert str(py.state).startswith("/tmp/") and str(py.remote).startswith("/tmp/")

    # the inbox scenarios' starting points: LightCraft-style sidecars next to the small Edits/ photos
    v0, v1 = sidecar(), sidecar(with_settings(("color.vibrance", 10.0)))
    for s in (py, rs):
        for i, data in {0: v0, 1: v1, 2: v1, 3: v0, 4: v0}.items():
            write(s, f"Edits/E{i}.xmp", data)

    print("\n#### round 1: first publish")
    run_python(py, work)
    run_rust(rs, work, binary)
    compare_round(py, rs, "round 1 (fresh publish)")
    for s in (py, rs):
        print(f"  [{s.name}] log: {[l for l in s.log if l.startswith(('scan:', 'inbox:'))]}")

    print("\n#### round 2: the source changes (new photo, changed photo, removed photo, edited sidecars)")
    sh([str(target / "examples" / "fixture"), str(work / "fixture"), "round2"])
    pc1 = sidecar(with_settings(("color.vibrance", 30.0)), rating=2)
    crs_old = (py.src / "2024/Trip/IMG_0002.xmp").read_text()
    crs_new = crs_old.replace('xmp:Rating="3"', 'xmp:Rating="4"')
    for s in (py, rs):
        write(s, "Late/NEW_0001.jpg", (work / "fixture/Late/NEW_0001.jpg").read_bytes())
        write(s, "Other/IMG_0003.png", (work / "fixture/Other/IMG_0003.png").read_bytes())   # changed in place
        os.remove(s.src / "2024/Trip/RAW_0002.dng")
        os.remove(s.src / "2024/Trip/RAW_0002.xmp")
        write(s, "2024/Trip/IMG_0002.xmp", crs_new.encode())                              # the PC edits a crs sidecar
        write(s, "Edits/E0.xmp", pc1)                                                     # the PC edits E0 and E3
        write(s, "Edits/E3.xmp", pc1)
    run_python(py, work)
    run_rust(rs, work, binary)
    compare_round(py, rs, "round 2 (new / changed / removed / sidecar changed)")
    for s in (py, rs):
        print(f"  [{s.name}] log: {[l for l in s.log if l.startswith(('scan:', 'inbox:'))]}")

    print("\n#### round 3: phone edits in the inbox")
    m = manifest_of(py)
    ids = {}
    for p in m["photos"]:
        ids[p["path"]] = p["id"]
        for alt in p.get("alternates", []):
            ids[alt["path"]] = alt["id"]
    future, past = "20990101T000000Z", "20000101T000000Z"
    e_merge = sidecar(with_settings(("light.exposure", 0.5), ("wb.temp", 5000.0)), flag="pick")
    e_conflict = sidecar(with_settings(("light.exposure", 0.1), ("light.contrast", 7.0)), rating=5)
    e2 = sidecar(with_settings(("light.exposure", 0.0), ("light.contrast", 20.0)))
    e1 = sidecar(with_settings(("light.exposure", 0.5), ("light.contrast", 20.0)))
    crs_phone = crs_old.replace('crs:Exposure2012="+0.80"', 'crs:Exposure2012="+1.50"')
    junk = b"<junk/>"
    edits = [
        (f"phone/{ids['Edits/E0.jpg']}-{future}-{sha16(v0)}.xmp", e_merge),                    # merge (PC changed since v0)
        (f"phone/{ids['Edits/E1.jpg']}-{future}-0123456789abcdef.xmp", e_conflict),            # unknown base, phone newer: apply
        (f"phone/{ids['Edits/E2.jpg']}-{past}-0123456789abcdef.xmp", e_conflict),              # unknown base, PC newer: keep
        (f"phone/{ids['Edits/E3.jpg']}-20990101T000000Z-{sha16(v0)}.xmp", e1),                 # chain, first edit: merge
        (f"phone/{ids['Edits/E3.jpg']}-20990101T000100Z-{sha16(e1)}.xmp", e2),                 # chain, second edit names the first as base
        (f"phone/{ids['Edits/E4.jpg']}-{future}-{sha16(v0)}.xmp", v0),                         # identical to the PC sidecar: noop
        (f"phone/{ids['Edits/E5.jpg']}-{future}-none.xmp", e_merge),                           # no sidecar yet: apply
        (f"phone/{ids['2024/Trip/IMG_0002.jpg']}-{future}-{sha16(crs_old.encode())}.xmp", crs_phone.encode()),  # crs attribute sidecar: merge
        (f"phone/{ids['2024/Trip/PAIR_0001.DNG']}-{future}-none.xmp", e_merge),                # the raw (alternate): PAIR_0001.xmp
        (f"phone/{ids['2024/Trip/PAIR_0001.JPG']}-{future}-none.xmp", e_conflict),             # the JPEG (primary): PAIR_0001.JPG.xmp
        (f"phone/{'7' * 32}-{future}-none.xmp", e_merge),                                      # unknown id: rejected
        (f"phone/{ids['Edits/E4.jpg']}-20990101T000200Z-none.xmp", junk),                      # not develop data: rejected
        ("phone/not-an-inbox-name.xmp", e_merge),                                              # bad name: rejected
        (f"phone2/{ids['Late/NEW_0001.jpg']}-{future}-none.xmp", e_merge),                    # a second device
    ]
    for s in (py, rs):
        for name, data in edits:
            put_inbox(s, name, data)
    run_python(py, work)
    run_rust(rs, work, binary)
    print("\n== round 3 (inbox)")
    pl = [l for l in py.log if l.startswith("inbox:")]
    rl = [l for l in rs.log if l.startswith("inbox:")]
    report("inbox log lines (every decision, reason and conflict list)", sorted(pl) == sorted(rl),
           f"only Python: {[l for l in pl if l not in rl][:4]} / only Rust: {[l for l in rl if l not in pl][:4]}")
    for l in pl:
        print("        " + l)
    compare_sets("source folders after the inbox: file list", files(py.src), files(rs.src))
    diffs = [p for p in files(py.src) if (rs.src / p).exists() and (py.src / p).read_bytes() != (rs.src / p).read_bytes()]
    report("sidecars after the inbox: byte-identical (apply and merge)", not diffs, str(diffs))
    compare_sets("local history (names)", files(py.state / "history"), files(rs.state / "history"), lambda p: norm_hist(re.sub(r"^\d{4}-\d{2}-\d{2}/", "", p)))
    diffs = [p for p in files(py.state / "history") if (rs.state / "history" / p).exists() and (py.state / "history" / p).read_bytes() != (rs.state / "history" / p).read_bytes()]
    report("local history: same bytes", not diffs, str(diffs))
    compare_sets("sidecar version store (names)", xmp_files(py.state / "sidecar-versions"), xmp_files(rs.state / "sidecar-versions"))
    compare_sets("remote: inbox-done / inbox leftovers", [p for p in files(py.remote) if p.startswith(("inbox", "inbox-done"))],
                 [p for p in files(rs.remote) if p.startswith(("inbox", "inbox-done"))], norm_path)
    # the reject notes say why
    notes = {}
    for s in (py, rs):
        for p in files(s.remote):
            if p.endswith(".rejected"):
                notes.setdefault(norm_path(p), []).append(re.sub(r"^rejected \S+: ", "", (s.remote / p).read_text()).strip())
    report(".rejected notes carry the same reason", all(len(set(v)) == 1 for v in notes.values()), str(notes))

    print("\n#### round 4: publish what the inbox wrote")
    for s in (py, rs):
        for p in files(s.src):
            if p.endswith(".xmp"):
                t = time.time() - 1000
                os.utime(s.src / p, (t, t))
    run_python(py, work)
    run_rust(rs, work, binary)
    compare_round(py, rs, "round 4 (new sidecars published, thumbnails re-rendered)")
    for s in (py, rs):
        print(f"  [{s.name}] log: {[l for l in s.log if l.startswith(('scan:', 'inbox:'))]}")

    print("\n#### round 5: nothing changed")
    run_python(py, work)
    run_rust(rs, work, binary)
    report("second run is quiet (Python)", any(l.startswith("scan:") and "new=0 changed=0 sidecar-changed=0 removed=0" in l for l in py.log) and "manifest unchanged; not uploading" in "\n".join(py.log))
    report("second run is quiet (Rust)", any(l.startswith("scan:") and "new=0 changed=0 sidecar-changed=0 removed=0" in l for l in rs.log) and "manifest unchanged; not uploading" in "\n".join(rs.log))

    print("\n#### adoption: the Rust app with a fresh state folder takes over the library the Python tool published")
    adopt = Side(work, "adopt")
    adopt.dir.mkdir(parents=True, exist_ok=True)
    shutil.copytree(py.remote, adopt.remote, symlinks=True)
    adopt.src = py.src
    adopt.write_config()
    before_files = {p: hashlib.sha256((adopt.remote / p).read_bytes()).hexdigest() for p in files(adopt.remote)}
    run_rust(adopt, work, binary)
    after_files = {p: hashlib.sha256((adopt.remote / p).read_bytes()).hexdigest() for p in files(adopt.remote)}
    changed = sorted(p for p in after_files if p in before_files and before_files[p] != after_files[p])
    added = sorted(p for p in after_files if p not in before_files)
    report("adoption: no original, thumbnail or preview is uploaded again or moved to history", not added and changed in ([], ["mobile/manifest.json"]),
           f"added {added[:6]}, changed {changed[:6]}")
    print(f"        (the manifest is the only file written: {changed})")

    print("\n#### dry run on both (round 2-style changes pending)")
    for s in (py, rs):
        write(s, "Late/NEW_0002.jpg", (work / "fixture/Late/NEW_0001.jpg").read_bytes() + b"\0")
    before = {s.name: (hashtree(s.src), hashtree(s.remote), hashtree(s.state)) for s in (py, rs)}
    env2 = {k: v for k, v in os.environ.items() if k != "LC_CLOUD_CONFIG"}
    env2["RCLONE_CONFIG"] = str(work / "rclone.conf")
    sh([sys.executable, str(REPO / "legacy/lightcraft_cloud.py"), "--config", str(py.cfg), "sync", "--dry-run"], env=env2)
    sh([str(binary), "sync", "--once", "--dry-run", "--config", str(rs.cfg)], env=env2)
    after = {s.name: (hashtree(s.src), hashtree(s.remote), hashtree(s.state)) for s in (py, rs)}
    report("--dry-run changes nothing (Python)", before["python"] == after["python"])
    report("--dry-run changes nothing (Rust)", before["rust"] == after["rust"])

    print("\n==== summary")
    print(f"  unexpected differences: {len(problems)}")
    for ar, t in problems:
        print(f"    - {ar}: {t}")
    print(f"  expected (intended) differences: {len(expected)}")
    for ar, t in expected:
        print(f"    - {ar}: {t}  [{EXPECTED.get(ar, '')}]")
    sys.exit(1 if problems else 0)


def hashtree(root):
    h = hashlib.sha256()
    for p in files(root):
        h.update(p.encode())
        h.update((Path(root) / p).read_bytes())
    return h.hexdigest()


if __name__ == "__main__":
    main()
