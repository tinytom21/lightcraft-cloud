#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""lc-cloud: publish a photo folder to an rclone remote for the LightCraft phone app.

The PC is the master copy. See FORMAT.md for the remote layout (format 1).
Usage: lc-cloud [--config FILE] {sync [--dry-run] | status | purge-history [--days N] | inbox-check}
"""
import argparse
import datetime as dt
import fcntl
import fnmatch
import hashlib
import json
import os
import re
import secrets
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tomllib
import xml.etree.ElementTree as ET
from pathlib import Path

VERSION = "0.1.0"
FORMAT = 1
PHOTO_EXTS = {
    ".arw", ".dng", ".cr2", ".cr3", ".nef", ".nrw", ".raf", ".orf", ".rw2", ".pef", ".srw",
    ".jpg", ".jpeg", ".heic", ".heif", ".tif", ".tiff", ".png",
}
JPEG_EXTS = {".jpg", ".jpeg"}
RAW_EXTS = {".arw", ".dng", ".cr2", ".cr3", ".nef", ".nrw", ".raf", ".orf", ".rw2", ".pef", ".srw"}
STABLE_SECS = 10
MAX_XMP_BYTES = 2 * 1024 * 1024
FLATPAK_APP = "ai.storyteller.lightcraft"
DEFAULT_RCLONE = shutil.which("rclone") or str(Path.home() / ".local/bin/rclone")  # override with `rclone` in the config
DEVICE_RE = re.compile(r"^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$")
INBOX_RE = re.compile(r"^([0-9a-f]{32})-(\d{8}T\d{6}Z)-([0-9a-f]{16}|none)\.xmp$")

DRY = False


class Refuse(Exception):
    """A safety check failed; nothing was changed."""


def log(msg):
    line = f"{dt.datetime.now():%F %T} {msg}"
    print(line, flush=True)
    logfile = globals().get("_LOGFILE")
    if logfile and not DRY:
        try:
            with open(logfile, "a") as f:
                f.write(line + "\n")
        except OSError:
            pass


def utcnow():
    return dt.datetime.now(dt.timezone.utc)


def today_utc():
    return utcnow().strftime("%Y-%m-%d")


# ---------------------------------------------------------------- config

DEFAULT_CONFIG = Path.home() / ".config/lightcraft-cloud/config.toml"


class Cfg:
    def __init__(self, path=None):
        path = Path(os.environ.get("LC_CLOUD_CONFIG") or path or DEFAULT_CONFIG)
        d = tomllib.loads(path.read_text()) if path.exists() else {}
        self.path = path
        self.source = Path(os.path.expanduser(d.get("source", "~/Pictures/Photos")))
        self.remote = d.get("remote", "gdrive:LightCraft Cloud").rstrip("/")
        self.device_history_days = int(d.get("device_history_days", 30))
        self.max_delete = int(d.get("max_delete", 50))
        self.thumb_px = int(d.get("thumb_px", 512))
        self.state = Path(os.path.expanduser(d.get("state_dir", "~/.local/state/lightcraft-cloud")))
        self.rclone = d.get("rclone", DEFAULT_RCLONE)
        self.photo_backup = d.get("photo_backup", "restic")
        self.cli = d.get("lightcraft_cli", None)  # None: flatpak
        self.notify = d.get("notify", True)
        self.exclude = [str(x) for x in d.get("exclude", [])]  # folder-name patterns (fnmatch, case-sensitive)
        self.pair_primary = str(d.get("pair_primary", "jpeg")).lower()
        if self.pair_primary not in ("jpeg", "raw"):
            raise Refuse(f"pair_primary must be \"jpeg\" or \"raw\", not {self.pair_primary!r}")
        # HARD SAFETY: the publisher's scratch library must never be under ~/Pictures.
        st = self.state.resolve()
        if str(st).startswith(str((Path.home() / "Pictures").resolve())):
            raise Refuse(f"state_dir {st} is under ~/Pictures; refusing (keep it outside backups)")

    @property
    def lib(self):
        return self.state / "lib"

    @property
    def mobile(self):
        return self.state / "mobile"


# ---------------------------------------------------------------- state

SCHEMA = """
CREATE TABLE IF NOT EXISTS files(
  path TEXT PRIMARY KEY, size INT, mtime_ns INT,
  sidecar TEXT, sidecar_hash TEXT, id TEXT, lib_id INT, meta TEXT, status TEXT, note TEXT);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT);
"""


class State:
    def __init__(self, cfg, readonly=False):
        self.cfg = cfg
        db = cfg.state / "state.sqlite"
        self.empty = False
        if readonly:
            if db.exists():
                self.db = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            else:
                self.db = sqlite3.connect(":memory:")
                self.db.executescript(SCHEMA)
        else:
            cfg.state.mkdir(parents=True, exist_ok=True)
            (cfg.state / "logs").mkdir(exist_ok=True)
            self.db = sqlite3.connect(db)
            self.db.executescript(SCHEMA)
        self.db.row_factory = sqlite3.Row

    def files(self):
        return {r["path"]: dict(r) for r in self.db.execute("SELECT * FROM files")}

    def put(self, row):
        cols = ["path", "size", "mtime_ns", "sidecar", "sidecar_hash", "id", "lib_id", "meta", "status", "note"]
        self.db.execute(
            f"INSERT OR REPLACE INTO files({','.join(cols)}) VALUES({','.join('?' * len(cols))})",
            [row.get(c) for c in cols])

    def delete(self, path):
        self.db.execute("DELETE FROM files WHERE path=?", (path,))

    def get(self, k, default=None):
        r = self.db.execute("SELECT v FROM kv WHERE k=?", (k,)).fetchone()
        return r["v"] if r else default

    def set(self, k, v):
        self.db.execute("INSERT OR REPLACE INTO kv VALUES(?,?)", (k, v))

    def commit(self):
        self.db.commit()


# ---------------------------------------------------------------- scan

def sha16(data):
    return hashlib.sha256(data).hexdigest()[:16]


def is_skipped_name(name):
    return name.startswith(".") or (name.startswith(".lightcraft-") and name.endswith(".tmp"))


def is_excluded_dir(name, exclude):
    """True if a single directory name matches any exclude pattern (fnmatch, case-sensitive)."""
    return any(fnmatch.fnmatchcase(name, pat) for pat in exclude)


def is_excluded(relpath, exclude):
    """True if any directory component of a root-relative path matches an exclude pattern.
    The file name itself is never matched (a file merely named like the pattern is not excluded)."""
    return any(is_excluded_dir(part, exclude) for part in relpath.split("/")[:-1])


def rclone_excludes(exclude):
    """rclone --exclude filters matching the same folders (at the root and nested) as is_excluded."""
    out = []
    for pat in exclude:
        out += [f"/{pat}/**", f"**/{pat}/**"]
    return out


def walk_source(root, exclude=()):
    """Yield (relpath, abs, stat) for all non-hidden files, skipping hidden and excluded directories."""
    stack = [Path(root)]
    while stack:
        d = stack.pop()
        try:
            it = list(os.scandir(d))
        except OSError:
            continue
        for e in it:
            if is_skipped_name(e.name):
                continue
            if e.is_dir(follow_symlinks=False):
                if not is_excluded_dir(e.name, exclude):
                    stack.append(Path(e.path))
            elif e.is_file(follow_symlinks=False):
                yield Path(e.path).relative_to(root).as_posix(), e.path, e.stat()


def find_sidecars(photos, files):
    """photos: list of relpaths. files: set of all relpaths. Mirrors the engine's StemOwners rule:
    when several photos in a folder share a stem, the raw (else first by name) owns <stem>.xmp and the
    others use <file>.xmp. Returns {photo: existing sidecar relpath or None} and {photo: sidecar path
    to create}."""
    lower = {f.lower(): f for f in files}
    groups = {}
    for p in photos:
        pp = Path(p)
        groups.setdefault((pp.parent.as_posix(), pp.stem.lower()), []).append(p)
    existing, target = {}, {}
    for members in groups.values():
        owner = None
        if len(members) > 1:
            owner = sorted(members, key=lambda m: (Path(m).suffix.lower() not in RAW_EXTS, m.lower()))[0]
        for p in members:
            pp = Path(p)
            stem_x = pp.with_suffix(".xmp").as_posix()
            full_x = p + ".xmp"
            uses_stem = owner is None or owner == p
            target[p] = stem_x if uses_stem else full_x
            cands = [stem_x, full_x] if uses_stem else [full_x]
            found = None
            for c in cands:
                if c.lower() in lower:
                    found = lower[c.lower()]
                    break
            existing[p] = found
    return existing, target


def scan_source(root, now=None, exclude=()):
    """Returns (entries, unstable). entries: {rel: dict(size, mtime_ns, sidecar, sidecar_hash, sidecar_mtime_ns,
    sidecar_target)}, unstable: set of relpaths (photo or sidecar) modified < STABLE_SECS ago."""
    now = now if now is not None else utcnow().timestamp()
    allf, photos = {}, []
    for rel, ab, st in walk_source(root, exclude):
        allf[rel] = (ab, st)
        if Path(rel).suffix.lower() in PHOTO_EXTS:
            photos.append(rel)
    existing, target = find_sidecars(photos, set(allf))
    entries, unstable = {}, set()
    for p in photos:
        ab, st = allf[p]
        e = {"size": st.st_size, "mtime_ns": st.st_mtime_ns, "sidecar": None, "sidecar_hash": None,
             "sidecar_mtime_ns": None, "sidecar_target": target[p]}
        if now - st.st_mtime < STABLE_SECS:
            unstable.add(p)
        sc = existing[p]
        if sc:
            sab, sst = allf[sc]
            e["sidecar"] = sc
            e["sidecar_mtime_ns"] = sst.st_mtime_ns
            if now - sst.st_mtime < STABLE_SECS:
                unstable.add(sc)
            try:
                with open(sab, "rb") as f:
                    e["sidecar_hash"] = sha16(f.read())
            except OSError:
                e["sidecar"] = None
        entries[p] = e
    return entries, unstable


def diff_scan(entries, unstable, known):
    """known: state files dict. Returns dict(new, changed, sc_changed, removed, retry). Unstable photos are
    skipped (treated as unchanged/not present for this run) but never counted as removed."""
    new, changed, sc_changed, retry = [], [], [], []
    for p, e in sorted(entries.items()):
        k = known.get(p)
        if p in unstable or (e["sidecar"] in unstable if e["sidecar"] else False):
            continue
        if k is None:
            new.append(p)
        elif k["size"] != e["size"] or k["mtime_ns"] != e["mtime_ns"]:
            changed.append(p)
        elif (k["sidecar_hash"] or None) != e["sidecar_hash"]:
            sc_changed.append(p)
        elif k["status"] == "retry":
            retry.append(p)
    removed = sorted(p for p in known if p not in entries)
    return {"new": new, "changed": changed, "sc_changed": sc_changed, "removed": removed, "retry": retry}


def check_source(root):
    root = Path(root)
    if not root.exists():
        raise Refuse(f"source {root} does not exist (drive not mounted?)")
    if not root.is_dir():
        raise Refuse(f"source {root} is not a directory")
    try:
        empty = not any(root.iterdir())
    except OSError as e:
        raise Refuse(f"source {root} unreadable: {e}")
    if empty:
        raise Refuse(f"source {root} is empty (drive not mounted?)")


def published_removals(removed, known):
    """Removed paths that were actually published (status ok); never-published rows don't count."""
    return [p for p in removed if (known.get(p) or {}).get("status", "ok") == "ok"]


def check_disappeared(removed, max_delete):
    if len(removed) > max_delete:
        raise Refuse(f"{len(removed)} source photos disappeared since the last run (max_delete={max_delete}); "
                     f"refusing. Check the drive, or raise max_delete in the config.")


# ---------------------------------------------------------------- manifest

def iso_utc(ts):
    return dt.datetime.fromtimestamp(ts, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _pair_key(path):
    pp = Path(path)
    return pp.parent.as_posix(), pp.stem.lower()


def _pair_up(items, primary):
    """items: [(row, photo dict)] sorted by path. Within one folder a raw and a JPEG with the same stem are one
    shot, but only when it is unambiguous: exactly one raw and exactly one JPEG for that stem. Returns
    {primary_id: alternate_id} for the pairs found."""
    groups = {}
    for r, ph in items:
        groups.setdefault(_pair_key(r["path"]), []).append((r, ph))
    pairs = {}
    for members in groups.values():
        raws = [m for m in members if Path(m[0]["path"]).suffix.lower() in RAW_EXTS]
        jpgs = [m for m in members if Path(m[0]["path"]).suffix.lower() in JPEG_EXTS]
        if len(raws) == 1 and len(jpgs) == 1:
            pri, alt = (jpgs[0], raws[0]) if primary == "jpeg" else (raws[0], jpgs[0])
            pairs[pri[1]["id"]] = alt[1]["id"]
    return pairs


ALT_KEYS = ("id", "path", "sidecar", "sidecarVersion", "preview", "previewKey", "previewBytes", "thumb",
            "width", "height")


def build_manifest_photos(rows, preview_sizes, pair_primary="jpeg"):
    """rows: state files dicts. preview_sizes: {id: bytes}. Photos sharing an id (identical content) are
    listed once (first path). A raw+JPEG pair (same folder and stem) is listed once, as the primary, with the
    other file under "alternates"."""
    items, seen = [], set()
    for r in sorted(rows, key=lambda r: r["path"]):
        if r["status"] != "ok" or not r["id"] or r["id"] in seen:
            continue
        seen.add(r["id"])
        m = json.loads(r["meta"] or "{}")
        pv = preview_sizes.get(r["id"])
        items.append((r, {
            "id": r["id"], "path": r["path"], "size": r["size"], "mtime": iso_utc(r["mtime_ns"] / 1e9),
            "sidecar": r["sidecar"], "sidecarVersion": r["sidecar_hash"],
            "captured": m.get("captured"), "camera": m.get("camera"), "lens": m.get("lens"),
            "width": m.get("width"), "height": m.get("height"), "rating": m.get("rating"),
            "pick": (m.get("flag") == "pick") if m.get("flag") is not None else None,
            "flag": m.get("flag"), "label": m.get("label"),
            "thumb": f"thumbs/{r['id']}.jpg", "preview": f"previews/{r['id']}.lcsp" if pv else None,
            "previewKey": m.get("previewKey") if pv else None,
            "previewBytes": pv,
        }))
    pairs = _pair_up(items, pair_primary)
    alt_ids = set(pairs.values())
    by_id = {ph["id"]: ph for _, ph in items}
    out = []
    for _, ph in items:
        if ph["id"] in alt_ids:
            continue
        if ph["id"] in pairs:
            alt = by_id[pairs[ph["id"]]]
            ph["alternates"] = [{k: alt[k] for k in ALT_KEYS}]
        out.append(ph)
    return out


def manifest_ids(photos):
    """All ids a manifest refers to, including alternates (their thumbs/previews must be kept)."""
    ids = set()
    for p in photos:
        ids.add(p["id"])
        ids.update(a["id"] for a in p.get("alternates", []))
    return ids


def manifest_doc(photos, generated=None):
    return {"format": FORMAT, "generated": generated or iso_utc(utcnow().timestamp()),
            "publisher": f"lightcraft-cloud {VERSION}", "photos": photos}


# ---------------------------------------------------------------- inbox

def parse_inbox_name(relpath):
    """relpath like 'phone1/<id>-<stamp>-<base>.xmp'. Returns dict(device,id,stamp,base,epoch) or None."""
    parts = relpath.split("/")
    # the device name ends up in local history file names: plain characters only
    if len(parts) != 2 or not DEVICE_RE.match(parts[0]):
        return None
    m = INBOX_RE.match(parts[1])
    if not m:
        return None
    try:
        t = dt.datetime.strptime(m.group(2), "%Y%m%dT%H%M%SZ").replace(tzinfo=dt.timezone.utc)
    except ValueError:
        return None
    return {"device": parts[0], "id": m.group(1), "stamp": m.group(2), "base": m.group(3), "epoch": t.timestamp()}


def validate_xmp(data):
    """Returns None if OK, else an error string."""
    if len(data) >= MAX_XMP_BYTES:
        return "too large"
    low = data.lower()
    if b"<!doctype" in low or b"<!entity" in low:
        return "DTD/entities not allowed"
    try:
        ET.fromstring(data)
    except ET.ParseError as e:
        return f"not well-formed XML ({e})"
    if b"lc:settings" not in data and b"crs:" not in data:
        return "no lc:settings or crs: develop data"
    return None


def decide(base, cur_version, inbox_epoch, local_epoch, inbox_hash=None):
    """Conflict policy. base: sidecar version the phone started from or 'none'. cur_version: sha16 of the
    current PC sidecar or None. local_epoch: when the PC sidecar was last written (mtime) or None.
    Returns (action, reason): action in 'apply' | 'keep' | 'noop'."""
    if inbox_hash is not None and inbox_hash == cur_version:
        return "noop", "identical to current sidecar"
    if cur_version is None and base == "none":
        return "apply", "no sidecar on either side"
    if cur_version is not None and base == cur_version:
        return "apply", "base matches current sidecar"
    if cur_version is None:
        return "apply", "PC sidecar is gone; phone edit restores it"
    # conflict: PC sidecar changed (or phone started from nothing while the PC has one)
    if local_epoch is None or inbox_epoch > local_epoch:
        return "apply", "conflict: phone edit is newer; PC version kept in history"
    return "keep", "conflict: PC sidecar is newer; phone edit kept in history"


# ---------------------------------------------------------------- rclone

def rclone(cfg, args, check=True, input_bytes=None, capture=True):
    cmd = [cfg.rclone, *args]
    r = subprocess.run(cmd, capture_output=capture, input=input_bytes)
    if check and r.returncode != 0:
        raise RuntimeError(f"rclone {' '.join(args[:2])} failed ({r.returncode}): {(r.stderr or b'').decode(errors='replace')[-400:]}")
    return r


def rclone_lsjson(cfg, path, extra=()):
    r = rclone(cfg, ["lsjson", "-R", "--files-only", *extra, path], check=False)
    if r.returncode == 3:  # directory not found
        return []
    if r.returncode != 0:
        raise RuntimeError(f"rclone lsjson {path} failed: {r.stderr.decode(errors='replace')[-300:]}")
    return json.loads(r.stdout or b"[]")


def rclone_sync(cfg, src, dst, backup_sub, dry=False, excludes=(), exclude_from=None):
    """rclone sync with backup-dir + max-delete. Returns summary dict."""
    date = today_utc()
    args = ["sync", str(src), dst,
            "--backup-dir", f"{cfg.remote}/history/{date}/{backup_sub}",
            "--suffix", utcnow().strftime(".%H%M%S"), "--suffix-keep-extension",
            "--max-delete", str(cfg.max_delete), "-v", "--stats-one-line", "--stats", "1h"]
    for x in excludes:
        args += ["--exclude", x]
    if exclude_from:
        args += ["--exclude-from", str(exclude_from)]
    if dry:
        args.append("--dry-run")
    r = rclone(cfg, args, check=False)
    err = r.stderr.decode(errors="replace")
    if r.returncode != 0:
        raise RuntimeError(f"rclone sync {src} -> {dst} failed ({r.returncode}): {err[-500:]}")
    new = len(re.findall(r"Copied \(new\)", err))
    moved = len(re.findall(r"Moved \(server-side\) to:", err))
    deleted = len(re.findall(r"Moved into backup dir|: Deleted", err))
    if dry:
        dnew = len(re.findall(r": Skipped copy as --dry-run", err))
        dmov = len(re.findall(r": Skipped (?:move|delete) as --dry-run", err))
        s = {"would_copy": dnew, "would_remove_or_backup": dmov}
    else:
        s = {"copied_new": new, "replaced_remote_backed_up": moved, "removed_to_history": deleted}
    stat = [l for l in err.splitlines() if re.search(r"INFO\s+:\s+[\d.]+ ?\S* / [\d.]+ ?\S*, \d+%", l)]
    s["stats"] = stat[-1].split(":", 3)[-1].strip() if stat else "nothing transferred"
    return s


# ---------------------------------------------------------------- lightcraft CLI

class CliError(Exception):
    pass


def cli(cfg, cmds):
    """Run commands against the scratch library (never any other). cmds: [(id, params|None)].
    Returns list of results (None for failed commands, with errors logged)."""
    lib = cfg.lib.resolve() if cfg.lib.exists() else cfg.lib.absolute()
    if "LightCraft Library" in str(lib) or str(lib).startswith(str((Path.home() / "Pictures").resolve())):
        raise Refuse(f"library path {lib} is not allowed")
    cfg.lib.mkdir(parents=True, exist_ok=True)
    if cfg.cli:
        base = [cfg.cli]
    else:
        base = ["flatpak", "run", f"--filesystem={cfg.source.resolve()}:ro", f"--filesystem={cfg.state.resolve()}",
                "--command=lightcraft-cli", FLATPAK_APP]
    argv = base + ["run", "--keep-going", "--library", str(lib), "library.xmpPreferences", '{"autoWrite":false}']
    for cid, params in cmds:
        argv.append(cid)
        if params is not None:
            argv.append(json.dumps(params, separators=(",", ":")))
    r = subprocess.run(argv, capture_output=True, text=True)
    lines = [json.loads(l) for l in r.stdout.splitlines() if l.startswith("{")]
    if len(lines) != len(cmds) + 1:
        raise CliError(f"lightcraft-cli returned {len(lines)} results for {len(cmds) + 1} commands: {r.stderr[-300:]}")
    out = []
    for (cid, _), l in zip(cmds, lines[1:]):
        if l.get("ok"):
            out.append(l["result"])
        else:
            log(f"WARNING: {cid} failed: {str(l.get('error'))[:200]}")
            out.append(None)
    return out


def chunks(seq, n):
    for i in range(0, len(seq), n):
        yield seq[i:i + n]


def refresh_library(cfg, st, plan, entries):
    """Import/refresh/render for plan['new'|'changed'|'sc_changed'|'retry']; updates state. Returns failed paths."""
    known = st.files()
    src = cfg.source
    todo_import = plan["new"] + plan["changed"] + plan["retry"]
    # drop library entries for changed/removed photos
    drop = [known[p]["lib_id"] for p in plan["changed"] + plan["removed"] + plan["retry"]
            if p in known and known[p]["lib_id"] is not None]
    if drop:
        cli(cfg, [("photo.deletePermanently", {"ids": drop})])
    for p in plan["removed"]:
        st.delete(p)
    for p in plan["changed"] + plan["retry"]:
        if p in known:
            known[p]["lib_id"] = None
    # import
    path_to_lib = {}  # abs path -> lib id
    dup_of_lib = {}
    for batch in chunks(todo_import, 50):
        res = cli(cfg, [("library.import", {"paths": [str(src / p) for p in batch], "mode": "add"})])[0]
        if res is None:
            continue
        for d in res.get("duplicates", []):
            dup_of_lib[d["path"]] = d.get("existing")
        for f in res.get("failed", []):
            log(f"WARNING: import failed: {f}")
    # find new library ids and inspect them (mapping id -> source path)
    keep_ids = {r["lib_id"] for r in st.files().values() if r["lib_id"] is not None}
    q = cli(cfg, [("catalog.query", {"limit": 1000000})])[0] or {"photos": []}
    new_ids = [p["id"] for p in q["photos"] if p["id"] not in keep_ids]
    insp = {}
    for batch in chunks(new_ids, 100):
        for pid, r in zip(batch, cli(cfg, [("photo.inspect", {"id": i}) for i in batch])):
            if r:
                insp[pid] = r
    for pid, r in insp.items():
        path_to_lib[r["source"]["path"]] = pid
    # sidecar-changed: re-read sidecar into the scratch library
    sc_ids = [known[p]["lib_id"] for p in plan["sc_changed"] if known[p]["lib_id"] is not None]
    reread_ids = []
    for batch in chunks(sc_ids, 100):
        cli(cfg, [("photo.readMetadataFromFile", {"ids": batch})])
        reread_ids += batch
    for pid, r in zip(sc_ids, cli(cfg, [("photo.inspect", {"id": i}) for i in sc_ids]) if sc_ids else []):
        if r:
            insp[pid] = r
    # smart previews for newly imported photos. The engine names a preview by a hash of the photo's
    # content_hash string (not the content_hash itself), so find the file by diffing the folder.
    sp_dir = cfg.lib / "Smart Previews"
    listing = lambda: {f.name for f in sp_dir.glob("*/*.lcsp")} if sp_dir.is_dir() else set()
    for lid in path_to_lib.values():
        before = listing()
        cli(cfg, [("library.smartPreviews", {"ids": [lid]})])
        gained = listing() - before
        if not gained:  # already existed (same content seen before): discard and rebuild to learn its name
            cli(cfg, [("library.smartPreviews", {"ids": [lid], "discard": True})])
            before = listing()
            cli(cfg, [("library.smartPreviews", {"ids": [lid]})])
            gained = listing() - before
        if len(gained) == 1:
            insp[lid]["_previewKey"] = next(iter(gained))[:-5]
        else:
            log(f"WARNING: could not identify smart preview for library photo {lid}")
    # thumbnails: new + sidecar-changed, rendered with current edits
    th = cfg.mobile / "thumbs"
    th.mkdir(parents=True, exist_ok=True)
    (cfg.mobile / "previews").mkdir(parents=True, exist_ok=True)
    thumb_jobs = []  # (lib_id, content_hash)
    for lid, r in insp.items():
        thumb_jobs.append((lid, r["content_hash"]))
    tmp_dir = cfg.state / "tmp"
    shutil.rmtree(tmp_dir, ignore_errors=True)
    tmp_dir.mkdir(parents=True, exist_ok=True)
    for batch in chunks(thumb_jobs, 50):
        cmds = [("app.export", {"ids": [lid], "path": str(tmp_dir / f"{h}.jpg"), "longEdge": cfg.thumb_px,
                                "quality": 80, "colorSpace": "srgb", "conflict": "overwrite"}) for lid, h in batch]
        for (lid, h), res in zip(batch, cli(cfg, cmds)):
            f = tmp_dir / f"{h}.jpg"
            if res is not None and f.exists():
                os.replace(f, th / f"{h}.jpg")
    shutil.rmtree(tmp_dir, ignore_errors=True)
    # copy smart previews into the staged mobile mirror (flat, named by the photo id)
    for lid, r in insp.items():
        key = r.get("_previewKey")
        if not key:
            continue
        h = r["content_hash"]
        s = cfg.lib / "Smart Previews" / key[:2] / f"{key}.lcsp"
        d = cfg.mobile / "previews" / f"{h}.lcsp"
        if s.exists() and (not d.exists() or d.stat().st_size != s.stat().st_size):
            t = d.with_name(f".{d.name}.tmp")
            shutil.copyfile(s, t)
            os.replace(t, d)
    # update state rows
    by_lib = {pid: r for pid, r in insp.items()}
    failed = []
    for p in todo_import + plan["sc_changed"]:
        e = entries[p]
        row = {"path": p, "size": e["size"], "mtime_ns": e["mtime_ns"], "sidecar": e["sidecar"],
               "sidecar_hash": e["sidecar_hash"], "status": "ok", "note": None}
        lid = path_to_lib.get(str(src / p))
        if lid is None and p in plan["sc_changed"]:
            lid = known[p]["lib_id"]
        if lid is None:
            dup = dup_of_lib.get(str(src / p))
            row.update(status="dup" if dup is not None else "failed",
                       note=f"duplicate of library photo {dup}" if dup is not None else "import failed")
            if dup is None:
                failed.append(p)
            st.put(row)
            continue
        r = by_lib.get(lid)
        if r is not None and not r.get("_previewKey") and p in known and known[p]["meta"]:
            r["_previewKey"] = json.loads(known[p]["meta"]).get("previewKey")
        if r is None:
            row.update(status="failed", note="inspect failed")
            failed.append(p)
            st.put(row)
            continue
        row.update(lib_id=lid, id=r["content_hash"], meta=json.dumps({
            "captured": r.get("captured"), "camera": (r.get("meta") or {}).get("camera"),
            "lens": (r.get("meta") or {}).get("lens"), "width": r.get("width"), "height": r.get("height"),
            "rating": r.get("rating"), "flag": r.get("flag"), "label": r.get("label"),
            "previewKey": r.get("_previewKey")}))
        st.put(row)
    st.commit()
    return failed


# ---------------------------------------------------------------- inbox processing

def local_history_dir(cfg):
    d = cfg.state / "history" / today_utc()
    return d


def save_history(cfg, rel, data, tag):
    d = local_history_dir(cfg) / (rel + f".{tag}.xmp")
    d.parent.mkdir(parents=True, exist_ok=True)
    d.write_bytes(data)
    return d


def write_atomic(path, data):
    path = Path(path)
    tmp = path.with_name(f".lightcraft-{secrets.token_hex(4)}.tmp")
    with open(tmp, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def process_inbox(cfg, st, dry):
    """Returns dict(applied, kept, rejected, noop, items[list of str], folders[folders with applied edits])."""
    summary = {"applied": 0, "kept": 0, "rejected": 0, "noop": 0, "items": [], "folders": []}
    items = rclone_lsjson(cfg, f"{cfg.remote}/inbox", ["--include", "*.xmp"])
    if not items:
        return summary
    parsed = []
    for it in items:
        p = parse_inbox_name(it["Path"])
        parsed.append((p, it))
    # oldest stamp first; unparseable ones last (they are rejected)
    parsed.sort(key=lambda x: (x[0]["epoch"] if x[0] else 1e18, x[1]["Path"]))
    known = st.files()
    by_id = {}
    for p, r in sorted(known.items()):
        # every ok row, so an edit addressed to a pair's alternate id (raw or JPEG) finds its own file
        if r["status"] == "ok" and r["id"] and r["id"] not in by_id:
            by_id[r["id"]] = r
    cur = {}  # path -> (hash, epoch) overlay of sidecars written during this run
    date = today_utc()
    scan_cache = {}

    def reject(it, why):
        summary["rejected"] += 1
        summary["items"].append(f"reject {it['Path']}: {why}")
        log(f"inbox: REJECT {it['Path']}: {why}")
        if not dry:
            dest = f"{cfg.remote}/inbox-done/{date}/{it['Path']}"
            rclone(cfg, ["moveto", f"{cfg.remote}/inbox/{it['Path']}", dest])
            rclone(cfg, ["rcat", dest + ".rejected"], input_bytes=f"rejected {utcnow().isoformat()}: {why}\n".encode())

    def done(it):
        if not dry:
            rclone(cfg, ["moveto", f"{cfg.remote}/inbox/{it['Path']}", f"{cfg.remote}/inbox-done/{date}/{it['Path']}"])

    for p, it in parsed:
        if p is None:
            reject(it, "file name is not <device>/<id>-<UTCstamp>-<base>.xmp")
            continue
        data = rclone(cfg, ["cat", f"{cfg.remote}/inbox/{it['Path']}"]).stdout
        err = validate_xmp(data)
        if err:
            reject(it, err)
            continue
        row = by_id.get(p["id"])
        if row is None:
            reject(it, f"unknown photo id {p['id']}")
            continue
        rel = row["path"]
        sc_rel = row["sidecar"]
        if scan_cache.get(rel) is None:
            scan_cache[rel] = scan_sidecar_now(cfg, rel)
        sc_rel, sc_hash, sc_mtime = scan_cache[rel]
        if rel in cur:
            sc_hash, sc_mtime = cur[rel]
        inbox_hash = sha16(data)
        action, reason = decide(p["base"], sc_hash, p["epoch"], sc_mtime, inbox_hash)
        log(f"inbox: {it['Path']} -> {action} ({reason})")
        summary["items"].append(f"{action} {it['Path']}: {reason}")
        target = sc_rel or find_target_sidecar(cfg, rel)
        tag = f"{p['device']}.{p['stamp']}"
        if action == "noop":
            summary["noop"] += 1
            done(it)
            continue
        if dry:
            summary["applied" if action == "apply" else "kept"] += 1
            continue
        abs_t = cfg.source / target
        if action == "apply":
            if abs_t.exists():
                prev = abs_t.read_bytes()
                save_history(cfg, target, prev, f"pc-{dt.datetime.fromtimestamp(abs_t.stat().st_mtime, dt.timezone.utc):%Y%m%dT%H%M%SZ}")
            write_atomic(abs_t, data)
            cur[rel] = (inbox_hash, p["epoch"])
            scan_cache[rel] = (target, inbox_hash, p["epoch"])
            summary["applied"] += 1
            folder = str(Path(target).parent)
            if folder not in summary["folders"]:
                summary["folders"].append(folder)
        else:
            save_history(cfg, target, data, tag)
            summary["kept"] += 1
        done(it)
    return summary


def scan_sidecar_now(cfg, rel):
    """Current sidecar (relpath, hash, mtime epoch) for photo rel, by rescanning its folder."""
    root = cfg.source
    folder = (root / rel).parent
    files = set()
    photos = []
    for e in os.scandir(folder):
        if e.is_file() and not is_skipped_name(e.name):
            r = Path(e.path).relative_to(root).as_posix()
            files.add(r)
            if Path(r).suffix.lower() in PHOTO_EXTS:
                photos.append(r)
    existing, _ = find_sidecars(photos, files)
    sc = existing.get(rel)
    if not sc:
        return None, None, None
    data = (root / sc).read_bytes()
    return sc, sha16(data), (root / sc).stat().st_mtime


def find_target_sidecar(cfg, rel):
    folder = (cfg.source / rel).parent
    files, photos = set(), []
    for e in os.scandir(folder):
        if e.is_file() and not is_skipped_name(e.name):
            r = Path(e.path).relative_to(cfg.source).as_posix()
            files.add(r)
            if Path(r).suffix.lower() in PHOTO_EXTS:
                photos.append(r)
    _, target = find_sidecars(photos, files)
    return target[rel]


def notify_message(n, folders, source_name):
    edits = "1 phone edit" if n == 1 else f"{n} phone edits"
    names = [source_name if f in ("", ".") else f for f in folders]
    if not names:
        return f"{edits} applied — in LightCraft run Synchronize Folder"
    shown = ", ".join(names[:3]) + (f" and {len(names) - 3} more" if len(names) > 3 else "")
    which = "this folder" if len(names) == 1 else "these folders"
    return f"{edits} applied in {shown} — in LightCraft run Synchronize Folder on {which}"


def notify(cfg, n, folders=()):
    if not cfg.notify or n <= 0:
        return
    msg = notify_message(n, list(folders), cfg.source.name)
    env = dict(os.environ)
    env.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path=/run/user/{os.getuid()}/bus")
    try:
        subprocess.run(["notify-send", "LightCraft cloud", msg], env=env, timeout=10, check=False)
    except (OSError, subprocess.SubprocessError) as e:
        log(f"notify-send unavailable: {e}")


# ---------------------------------------------------------------- sync

class Lock:
    def __init__(self, cfg, dry):
        self.path = cfg.state / "lock"
        self.dry = dry
        self.fd = None

    def __enter__(self):
        if self.dry:
            if not self.path.exists():
                return self
            self.fd = os.open(self.path, os.O_RDONLY)
        else:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self.fd = os.open(self.path, os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Refuse("another lc-cloud run holds the lock")
        return self

    def __exit__(self, *a):
        if self.fd is not None:
            os.close(self.fd)


def exclude_file_for(unstable, cfg):
    f = cfg.state / "rclone-exclude.txt"
    esc = lambda s: re.sub(r"([\\*?\[\]{}])", r"\\\1", s)
    f.write_text("".join("/" + esc(u) + "\n" for u in sorted(unstable)))
    return f


def stage_preview_sizes(cfg):
    d = cfg.mobile / "previews"
    return {f.stem: f.stat().st_size for f in d.glob("*.lcsp")} if d.is_dir() else {}


def sync(cfg, dry):
    global DRY
    DRY = dry
    t0 = utcnow()
    check_source(cfg.source)
    if dry:
        log("DRY RUN: nothing will be changed")
    with Lock(cfg, dry):
        st = State(cfg, readonly=dry)
        known = st.files()
        # (1) inbox
        log("step 1/6: inbox")
        inbox = process_inbox(cfg, st, dry)
        log(f"inbox: applied={inbox['applied']} kept-pc={inbox['kept']} rejected={inbox['rejected']} identical={inbox['noop']}")
        # (2) scan
        log("step 2/6: scan source")
        entries, unstable = scan_source(cfg.source, exclude=cfg.exclude)
        if not entries:
            raise Refuse(f"no photos found under {cfg.source}; refusing (unmounted?)")
        plan = diff_scan(entries, unstable, known)
        check_disappeared(published_removals(plan["removed"], known), cfg.max_delete)
        log("scan: %d photos, new=%d changed=%d sidecar-changed=%d removed=%d retry=%d unstable-skipped=%d" % (
            len(entries), len(plan["new"]), len(plan["changed"]), len(plan["sc_changed"]),
            len(plan["removed"]), len(plan["retry"]), len(unstable)))
        if dry:
            for k in ("new", "changed", "sc_changed", "removed"):
                for p in plan[k]:
                    log(f"  plan {k}: {p}")
            log("step 4/6 (dry-run): rclone sync originals")
            r = rclone_sync(cfg, cfg.source, f"{cfg.remote}/originals", "originals", dry=True,
                            excludes=[".*", ".*/**", *rclone_excludes(cfg.exclude)])
            log(f"  originals: {r}")
            for sub in ("thumbs", "previews"):
                if (cfg.mobile / sub).is_dir():
                    r = rclone_sync(cfg, cfg.mobile / sub, f"{cfg.remote}/mobile/{sub}", f"mobile/{sub}", dry=True)
                    log(f"  mobile/{sub}: {r}")
            nb = len(plan["new"]) + len(plan["changed"]) + len(plan["sc_changed"])
            log(f"  would render {nb} thumbnails / build {len(plan['new']) + len(plan['changed'])} smart previews; "
                f"manifest would be rewritten if anything changed")
            log("DRY RUN done")
            return {"plan": plan, "inbox": inbox}
        # (3) library
        log("step 3/6: scratch library (import, smart previews, thumbnails)")
        failed = []
        if any(plan[k] for k in ("new", "changed", "sc_changed", "removed", "retry")):
            failed = refresh_library(cfg, st, plan, entries)
            if failed:
                log(f"WARNING: {len(failed)} photos failed to import: {failed[:5]}")
        # refresh unchanged rows' bookkeeping is unnecessary; build manifest
        rows = list(st.files().values())
        previews = stage_preview_sizes(cfg)
        photos = build_manifest_photos(rows, previews, cfg.pair_primary)
        live_ids = manifest_ids(photos)
        for sub, ext in (("thumbs", ".jpg"), ("previews", ".lcsp")):
            d = cfg.mobile / sub
            if d.is_dir():
                for f in d.iterdir():
                    if f.stem not in live_ids or f.name.startswith("."):
                        f.unlink()
        # (4) originals
        log("step 4/6: rclone sync originals")
        exf = exclude_file_for(unstable, cfg)
        r = rclone_sync(cfg, cfg.source, f"{cfg.remote}/originals", "originals", excludes=[".*", ".*/**", *rclone_excludes(cfg.exclude)],
                        exclude_from=exf)
        log(f"  originals: {r}")
        # (5) mobile files
        log("step 5/6: rclone sync mobile thumbs + previews")
        for sub in ("thumbs", "previews"):
            (cfg.mobile / sub).mkdir(parents=True, exist_ok=True)
            r = rclone_sync(cfg, cfg.mobile / sub, f"{cfg.remote}/mobile/{sub}", f"mobile/{sub}", excludes=[".*"])
            log(f"  mobile/{sub}: {r}")
        # (6) manifest last
        log("step 6/6: manifest")
        mpath = cfg.mobile / "manifest.json"
        old = json.loads(mpath.read_text()) if mpath.exists() else None
        if old and old.get("photos") == photos and old.get("format") == FORMAT and st.get("manifest_uploaded") == "1":
            log("  manifest unchanged; not uploading")
        else:
            doc = manifest_doc(photos)
            write_atomic(mpath, json.dumps(doc, indent=1).encode())
            tmp = f"{cfg.remote}/mobile/manifest.json.tmp-{secrets.token_hex(4)}"
            rclone(cfg, ["copyto", str(mpath), tmp])
            rclone(cfg, ["moveto", tmp, f"{cfg.remote}/mobile/manifest.json"])
            st.set("manifest_uploaded", "1")
            log(f"  manifest uploaded ({len(photos)} photos)")
        st.set("last_run", iso_utc(utcnow().timestamp()))
        st.set("last_result", "ok" if not failed else f"ok, {len(failed)} photos failed")
        st.set("last_counts", json.dumps({k: len(v) for k, v in plan.items()} | {"photos": len(photos)}))
        st.commit()
        notify(cfg, inbox["applied"], inbox.get("folders", ()))
        log("sync done in %.1fs" % (utcnow() - t0).total_seconds())
        return {"plan": plan, "inbox": inbox}


# ---------------------------------------------------------------- status / purge

def human(n):
    for u in ("B", "KiB", "MiB", "GiB"):
        if n < 1024 or u == "GiB":
            return f"{n:.1f} {u}" if u != "B" else f"{n} B"
        n /= 1024


def cmd_status(cfg):
    st = State(cfg, readonly=True)
    print(f"last run:   {st.get('last_run', 'never')} ({st.get('last_result', '-')})")
    print(f"last plan:  {st.get('last_counts', '-')}")
    rows = st.files()
    print(f"tracked:    {len(rows)} photos ({sum(1 for r in rows.values() if r['status'] != 'ok')} not published)")
    try:
        r = rclone(cfg, ["size", "--json", cfg.remote], check=False)
        if r.returncode == 0:
            j = json.loads(r.stdout)
            print(f"remote:     {j['count']} files, {human(j['bytes'])} at {cfg.remote}")
        else:
            print("remote:     (not reachable or empty)")
        inbox = [i for i in rclone_lsjson(cfg, f"{cfg.remote}/inbox", ["--include", "*.xmp"])]
        print(f"inbox:      {len(inbox)} pending")
    except Exception as e:
        print(f"remote:     error: {e}")


def latest_backup_age_hours(cfg):
    r = subprocess.run([cfg.photo_backup, "snapshots", "--latest", "1", "--json"], capture_output=True, text=True)
    if r.returncode != 0:
        raise Refuse("`photo_backup snapshots` failed; cannot confirm a recent backup")
    try:
        data = json.loads(r.stdout)
    except ValueError:
        raise Refuse("`photo_backup snapshots` gave unparseable output")
    snaps = []
    for g in data:
        snaps += g["snapshots"] if isinstance(g, dict) and "snapshots" in g else [g]
    times = []
    for s in snaps:
        t = re.sub(r"(\.\d{6})\d+", r"\1", s["time"])
        times.append(dt.datetime.fromisoformat(t))
    if not times:
        raise Refuse("no restic snapshots found")
    return (utcnow() - max(times)).total_seconds() / 3600


def cmd_purge(cfg, days):
    age = latest_backup_age_hours(cfg)
    if age > 48:
        raise Refuse(f"newest restic snapshot is {age:.0f} h old (> 48 h); refusing to purge history")
    cutoff = (utcnow() - dt.timedelta(days=days)).strftime("%Y-%m-%d")
    n = 0
    for top in ("history", "inbox-done"):
        r = rclone(cfg, ["lsf", "--dirs-only", f"{cfg.remote}/{top}"], check=False)
        if r.returncode != 0:
            continue
        for d in r.stdout.decode().split():
            d = d.rstrip("/")
            if re.fullmatch(r"\d{4}-\d{2}-\d{2}", d) and d < cutoff:
                log(f"purge {top}/{d}")
                rclone(cfg, ["purge", f"{cfg.remote}/{top}/{d}"])
                n += 1
    log(f"purged {n} folders older than {days} days (cutoff {cutoff}); newest backup {age:.1f} h old")


def cmd_inbox_check(cfg):
    """Cheap poll: one listing of <remote>/inbox; start a full sync only if a valid entry is pending.
    No lock, no state, no source access. Network errors are logged and swallowed (exit 0)."""
    try:
        r = rclone(cfg, ["lsf", "-R", "--files-only", "--max-depth", "2", f"{cfg.remote}/inbox"], check=False)
    except OSError as e:
        log(f"inbox: check failed (cannot run rclone: {e})")
        return 0
    if r.returncode == 3:  # inbox dir does not exist: empty
        return 0
    if r.returncode != 0:
        log(f"inbox: check failed (rclone exit {r.returncode}): {r.stderr.decode(errors='replace').strip()[-200:]}")
        return 0
    n = sum(1 for line in r.stdout.decode(errors="replace").splitlines() if parse_inbox_name(line.strip()))
    if not n:
        return 0
    log(f"inbox: {n} pending, starting sync")
    cmd = os.environ.get("LC_INBOX_SYNC_CMD") or "systemctl --user start --no-block lightcraft-cloud.service"
    rc = subprocess.run(shlex.split(cmd)).returncode
    if rc != 0:
        log(f"inbox: start command exited {rc}")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(prog="lc-cloud", description=__doc__)
    ap.add_argument("--config")
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sync")
    s.add_argument("--dry-run", action="store_true")
    sub.add_parser("status")
    ic = sub.add_parser("inbox-check")
    ic.add_argument("--config", default=argparse.SUPPRESS)
    p = sub.add_parser("purge-history")
    p.add_argument("--days", type=int)
    a = ap.parse_args(argv)
    try:
        cfg = Cfg(a.config)
        globals()["_LOGFILE"] = str(cfg.state / "logs" / "lc-cloud.log")
        if a.cmd == "sync":
            sync(cfg, a.dry_run)
        elif a.cmd == "inbox-check":
            return cmd_inbox_check(cfg)
        elif a.cmd == "status":
            cmd_status(cfg)
        else:
            cmd_purge(cfg, a.days if a.days is not None else cfg.device_history_days)
    except Refuse as e:
        log(f"REFUSED: {e}")
        return 2
    except Exception as e:  # noqa
        log(f"ERROR: {type(e).__name__}: {e}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
