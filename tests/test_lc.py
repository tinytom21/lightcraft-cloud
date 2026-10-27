from unittest import mock
import hashlib, json, os, stat, subprocess, sys, tempfile, time, unittest
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import lightcraft_cloud as lc

ID = "a" * 32


def tree_hash(root):
    h = hashlib.sha256()
    for p in sorted(Path(root).rglob("*")):
        if p.is_file() and p.name != "lock":
            h.update(str(p.relative_to(root)).encode() + p.read_bytes())
        elif p.is_dir():
            h.update(b"D" + str(p.relative_to(root)).encode())
    return h.hexdigest()


class Tmp(unittest.TestCase):
    def setUp(self):
        self._t = tempfile.TemporaryDirectory()
        self.t = Path(self._t.name)
        self.addCleanup(self._t.cleanup)

    def touch(self, rel, data=b"x", age=100):
        p = self.t / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        ts = time.time() - age
        os.utime(p, (ts, ts))
        return p


class ScanRules(Tmp):
    def test_extensions_and_skips(self):
        for n in ["a.ARW", "b.jpg", "c.HEIC", "d.txt", ".hidden.jpg", ".lightcraft-ab.tmp", "e.xmp", "f.DNG", "g.png"]:
            self.touch(n)
        self.touch(".hiddendir/x.jpg")
        self.touch("sub/h.tif")
        entries, unstable = lc.scan_source(self.t)
        self.assertEqual(sorted(entries), ["a.ARW", "b.jpg", "c.HEIC", "f.DNG", "g.png", "sub/h.tif"])
        self.assertFalse(unstable)

    def test_unstable_recent_files(self):
        self.touch("a.jpg", age=2)
        self.touch("b.jpg", age=60)
        self.touch("b.xmp", age=1)
        entries, unstable = lc.scan_source(self.t)
        self.assertEqual(unstable, {"a.jpg", "b.xmp"})
        plan = lc.diff_scan(entries, unstable, {})
        self.assertEqual(plan["new"], [])  # both skipped this run

    def test_unstable_not_counted_removed(self):
        self.touch("a.jpg", age=2)
        entries, unstable = lc.scan_source(self.t)
        known = {"a.jpg": dict(size=entries["a.jpg"]["size"], mtime_ns=0, sidecar_hash=None, status="ok")}
        plan = lc.diff_scan(entries, unstable, known)
        self.assertEqual(plan["removed"], [])
        self.assertEqual(plan["changed"], [])

    def test_sidecar_naming_rules(self):
        self.touch("x/P1.ARW"); self.touch("x/P1.jpg"); self.touch("x/P1.xmp"); self.touch("x/P1.jpg.xmp")
        self.touch("x/Q.ARW"); self.touch("x/Q.ARW.xmp"); self.touch("x/R.jpg"); self.touch("x/R.xmp")
        e, _ = lc.scan_source(self.t)
        self.assertEqual(e["x/P1.ARW"]["sidecar"], "x/P1.xmp")        # raw owns the stem sidecar
        self.assertEqual(e["x/P1.jpg"]["sidecar"], "x/P1.jpg.xmp")    # JPG uses <file>.xmp
        self.assertEqual(e["x/Q.ARW"]["sidecar"], "x/Q.ARW.xmp")
        self.assertEqual(e["x/R.jpg"]["sidecar"], "x/R.xmp")

    def test_diff_classification(self):
        self.touch("a.jpg", b"1"); self.touch("b.jpg", b"22"); self.touch("c.jpg", b"3"); self.touch("c.xmp", b"<x/>")
        e, u = lc.scan_source(self.t)
        known = {p: dict(size=v["size"], mtime_ns=v["mtime_ns"], sidecar_hash=v["sidecar_hash"], status="ok") for p, v in e.items()}
        known["b.jpg"]["size"] = 99                       # changed
        known["c.jpg"]["sidecar_hash"] = "0" * 16         # sidecar changed
        known["gone.jpg"] = dict(size=1, mtime_ns=1, sidecar_hash=None, status="ok")
        del known["a.jpg"]                                # new
        plan = lc.diff_scan(e, u, known)
        self.assertEqual((plan["new"], plan["changed"], plan["sc_changed"], plan["removed"]),
                         (["a.jpg"], ["b.jpg"], ["c.jpg"], ["gone.jpg"]))


class Manifest(unittest.TestCase):
    def row(self, path, i, **kw):
        r = dict(path=path, size=5, mtime_ns=1_700_000_000 * 10**9, sidecar=None, sidecar_hash=None, id=i, lib_id=1,
                 status="ok", meta=json.dumps({"captured": "2024-05-01T09:30:00+02:00", "camera": "C", "lens": None,
                                                "width": 6000, "height": 4000, "rating": 3, "flag": "pick", "label": None,
                                                "previewKey": "k"}))
        r.update(kw)
        return r

    def test_build(self):
        rows = [self.row("b/2.jpg", "2" * 32, sidecar="b/2.xmp", sidecar_hash="ab" * 8), self.row("a/1.jpg", "1" * 32),
                self.row("a/dup.jpg", "1" * 32), self.row("a/bad.jpg", None, status="failed")]
        ph = lc.build_manifest_photos(rows, {"1" * 32: 1234})
        self.assertEqual([p["path"] for p in ph], ["a/1.jpg", "b/2.jpg"])
        p = ph[0]
        self.assertEqual(p["thumb"], "thumbs/" + "1" * 32 + ".jpg")
        self.assertEqual((p["preview"], p["previewBytes"], p["pick"], p["rating"]), ("previews/" + "1" * 32 + ".lcsp", 1234, True, 3))
        self.assertIsNone(ph[1]["preview"])  # no staged preview
        self.assertEqual(ph[1]["sidecarVersion"], "ab" * 8)
        doc = lc.manifest_doc(ph)
        self.assertEqual(doc["format"], 1)
        for k in ("id", "path", "size", "mtime", "sidecar", "sidecarVersion", "captured", "camera", "lens", "width",
                  "height", "rating", "label", "thumb", "preview", "previewBytes"):
            self.assertIn(k, p)


class Inbox(unittest.TestCase):
    def test_notify_message(self):
        self.assertEqual(lc.notify_message(1, ["2024/Trip"], "Photos"),
                         "1 phone edit applied in 2024/Trip — in LightCraft run Synchronize Folder on this folder")
        self.assertEqual(lc.notify_message(2, [".", "B"], "Photos"),
                         "2 phone edits applied in Photos, B — in LightCraft run Synchronize Folder on these folders")
        self.assertIn("A, B, C and 1 more", lc.notify_message(4, ["A", "B", "C", "D"], "Photos"))
        self.assertEqual(lc.notify_message(3, [], "Photos"), "3 phone edits applied — in LightCraft run Synchronize Folder")

    def test_parse(self):
        p = lc.parse_inbox_name(f"phone/{ID}-20261027T022310Z-{'b' * 16}.xmp")
        self.assertEqual((p["device"], p["id"], p["stamp"], p["base"]), ("phone", ID, "20261027T022310Z", "b" * 16))
        self.assertEqual(lc.parse_inbox_name(f"p/{ID}-20261027T022310Z-none.xmp")["base"], "none")
        for bad in ["x.xmp", f"p/{ID}-20261027T022310Z.xmp", f"p/q/{ID}-20261027T022310Z-none.xmp",
                    f"p/{ID[:-1]}-20261027T022310Z-none.xmp", f"p/{ID}-20261327T022310Z-none.xmp",
                    f"p/{ID}-20261027T022310Z-XYZ.xmp", f"p/{ID}-20261027T022310Z-none.jpg"]:
            self.assertIsNone(lc.parse_inbox_name(bad), bad)

    def test_validate(self):
        ok = b'<x:xmpmeta xmlns:x="a" xmlns:lc="b"><lc:settings>{}</lc:settings></x:xmpmeta>'
        self.assertIsNone(lc.validate_xmp(ok))
        self.assertIsNone(lc.validate_xmp(b'<r xmlns:crs="c" crs:Exposure2012="1"/>'))
        self.assertIn("well-formed", lc.validate_xmp(b"<a><lc:settings></a>"))
        self.assertIn("develop", lc.validate_xmp(b"<a/>"))
        self.assertIn("large", lc.validate_xmp(ok + b" " * (2 * 1024 * 1024)))
        self.assertIn("DTD", lc.validate_xmp(b'<!DOCTYPE a [<!ENTITY e "x">]><a>lc:settings</a>'))


class Conflict(unittest.TestCase):
    def test_all_branches(self):
        d = lc.decide
        self.assertEqual(d("none", None, 100, None)[0], "apply")            # no sidecar either side
        self.assertEqual(d("abc", "abc", 100, 500)[0], "apply")             # base matches (even if mtime newer)
        self.assertEqual(d("abc", "abc", 100, 500, inbox_hash="abc")[0], "noop")
        self.assertEqual(d("abc", "zzz", 200, 100)[0], "apply")             # conflict, phone newer
        self.assertEqual(d("abc", "zzz", 100, 200)[0], "keep")              # conflict, PC newer
        self.assertEqual(d("none", "zzz", 200, 100)[0], "apply")            # phone had none, PC has one, phone newer
        self.assertEqual(d("none", "zzz", 100, 200)[0], "keep")
        self.assertEqual(d("abc", None, 100, None)[0], "apply")             # PC sidecar vanished
        self.assertEqual(d("abc", "zzz", 100, 100)[0], "keep")              # tie: PC wins


class Safety(Tmp):
    def test_source_checks(self):
        with self.assertRaises(lc.Refuse):
            lc.check_source(self.t / "missing")
        with self.assertRaises(lc.Refuse):
            lc.check_source(self.touch("file"))
        (self.t / "empty").mkdir()
        with self.assertRaises(lc.Refuse):
            lc.check_source(self.t / "empty")
        lc.check_source(self.t)

    def test_too_many_deletions(self):
        lc.check_disappeared(["a"] * 50, 50)
        with self.assertRaises(lc.Refuse):
            lc.check_disappeared(["a"] * 51, 50)

    def test_state_inside_pictures_refused(self):
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'state_dir = "{Path.home()}/Pictures/_x_state"\n')
        with self.assertRaises(lc.Refuse):
            lc.Cfg(cfgf)

    def test_lock(self):
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'state_dir = "{self.t}/state"\n')
        cfg = lc.Cfg(cfgf)
        with lc.Lock(cfg, False):
            with self.assertRaises(lc.Refuse):
                with lc.Lock(cfg, False):
                    pass


class Exclude(Tmp):
    def test_matching(self):
        ex = ["darktable_exported", "tmp*"]
        self.assertTrue(lc.is_excluded("darktable_exported/a.jpg", ex))          # root level
        self.assertTrue(lc.is_excluded("2022/x/darktable_exported/a.jpg", ex))   # nested
        self.assertTrue(lc.is_excluded("2022/darktable_exported/deep/a.jpg", ex))
        self.assertTrue(lc.is_excluded("tmp2/a.jpg", ex))                        # glob
        self.assertFalse(lc.is_excluded("2022/darktable_exported", ex))          # a FILE named like the folder
        self.assertFalse(lc.is_excluded("darktable_exported", ex))
        self.assertFalse(lc.is_excluded("Darktable_Exported/a.jpg", ex))         # case-sensitive
        self.assertFalse(lc.is_excluded("2022/a.jpg", ex))
        self.assertEqual(lc.rclone_excludes(["x"]), ["/x/**", "**/x/**"])

    def test_scan_skips_excluded(self):
        self.touch("keep/a.jpg"); self.touch("darktable_exported/b.jpg"); self.touch("keep/darktable_exported/c.jpg")
        self.touch("keep/darktable_exported/deep/d.jpg"); self.touch("darktable_exported.jpg")
        e, _ = lc.scan_source(self.t, exclude=["darktable_exported"])
        self.assertEqual(sorted(e), ["darktable_exported.jpg", "keep/a.jpg"])
        e, _ = lc.scan_source(self.t)
        self.assertEqual(len(e), 5)

    def test_max_delete_accounting(self):
        self.touch("keep/a.jpg")
        for i in range(60):
            self.touch(f"darktable_exported/{i}.jpg")
        e, u = lc.scan_source(self.t, exclude=["darktable_exported"])
        # never published (no state rows): nothing removed at all
        plan = lc.diff_scan(e, u, {"keep/a.jpg": dict(size=e["keep/a.jpg"]["size"], mtime_ns=e["keep/a.jpg"]["mtime_ns"],
                                                      sidecar_hash=None, status="ok")})
        self.assertEqual(plan["removed"], [])
        # tracked but never published (failed/dup rows) do not count toward max_delete
        known = {f"darktable_exported/{i}.jpg": dict(size=1, mtime_ns=1, sidecar_hash=None, status="failed") for i in range(60)}
        plan = lc.diff_scan(e, u, known)
        self.assertEqual(len(plan["removed"]), 60)                       # still cleaned out of state
        lc.check_disappeared(lc.published_removals(plan["removed"], known), 50)
        # previously published photos that become excluded are removals, subject to max_delete
        known = {f"darktable_exported/{i}.jpg": dict(size=1, mtime_ns=1, sidecar_hash=None, status="ok") for i in range(60)}
        plan = lc.diff_scan(e, u, known)
        with self.assertRaises(lc.Refuse):
            lc.check_disappeared(lc.published_removals(plan["removed"], known), 50)
        lc.check_disappeared(lc.published_removals(plan["removed"][:10], known), 50)

    def test_config(self):
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'state_dir = "{self.t}/state"\nexclude = ["a", "b*"]\npair_primary = "RAW"\n')
        cfg = lc.Cfg(cfgf)
        self.assertEqual((cfg.exclude, cfg.pair_primary), (["a", "b*"], "raw"))
        cfgf.write_text(f'state_dir = "{self.t}/state"\npair_primary = "tiff"\n')
        with self.assertRaises(lc.Refuse):
            lc.Cfg(cfgf)


class Pairing(unittest.TestCase):
    def rows(self, *paths):
        return [dict(path=p, size=5, mtime_ns=10**18, sidecar=None, sidecar_hash=None, id=f"{i:032x}", lib_id=i,
                     status="ok", meta=json.dumps({"width": 6000, "height": 4000, "previewKey": f"k{i}"}))
                for i, p in enumerate(paths, 1)]

    def paths(self, photos):
        return [(p["path"], [a["path"] for a in p.get("alternates", [])]) for p in photos]

    def test_pair_default_jpeg_primary(self):
        ph = lc.build_manifest_photos(self.rows("A/x.ARW", "A/x.JPG", "A/lone.ARW", "B/lone2.jpg"), {})
        self.assertEqual(self.paths(ph), [("A/lone.ARW", []), ("A/x.JPG", ["A/x.ARW"]), ("B/lone2.jpg", [])])
        self.assertNotIn("alternates", ph[0])
        alt = ph[1]["alternates"][0]
        self.assertEqual(set(alt), {"id", "path", "sidecar", "sidecarVersion", "preview", "previewKey", "previewBytes",
                                    "thumb", "width", "height"})
        self.assertEqual(alt["thumb"], f"thumbs/{alt['id']}.jpg")
        self.assertEqual(lc.manifest_ids(ph), {r["id"] for r in self.rows("A/x.ARW", "A/x.JPG", "A/lone.ARW", "B/lone2.jpg")})

    def test_pair_primary_raw(self):
        ph = lc.build_manifest_photos(self.rows("A/x.jpeg", "A/x.nef"), {}, "raw")
        self.assertEqual(self.paths(ph), [("A/x.nef", ["A/x.jpeg"])])

    def test_not_pairs(self):
        # different folders, two JPEGs differing only in extension case, other formats
        ph = lc.build_manifest_photos(self.rows("A/x.jpg", "A/x.JPG", "B/x.ARW", "A/y.png", "A/y.ARW"), {})
        self.assertEqual(len(ph), 5)
        self.assertTrue(all("alternates" not in p for p in ph))


class InboxAlternate(Tmp):
    def test_edit_for_alternate_id_applied(self):
        src = self.t / "src"
        self.touch("src/A/x.ARW"); self.touch("src/A/x.JPG")
        self.touch("src/A/x.xmp", b"<old/>", age=1000); self.touch("src/A/x.JPG.xmp", b"<oldj/>", age=1000)
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{src}"\nremote = "fake:R"\nstate_dir = "{self.t}/state"\n')
        cfg = lc.Cfg(cfgf)
        st = lc.State(cfg)
        raw_id, jpg_id = "1" * 32, "2" * 32
        st.put(dict(path="A/x.ARW", size=1, mtime_ns=1, status="ok", id=raw_id, sidecar="A/x.xmp"))
        st.put(dict(path="A/x.JPG", size=1, mtime_ns=1, status="ok", id=jpg_id, sidecar="A/x.JPG.xmp"))
        st.commit()
        xmp = b'<x:xmpmeta xmlns:x="a" xmlns:lc="b"><lc:settings>{}</lc:settings></x:xmpmeta>'
        name = f"phone/{raw_id}-20990101T000000Z-none.xmp"      # raw is the alternate (JPEG is primary)
        calls = []
        def fake_rclone(c, args, check=True, input_bytes=None, capture=True):
            calls.append(args)
            return mock.Mock(stdout=xmp, returncode=0)
        with mock.patch.object(lc, "rclone", fake_rclone), \
             mock.patch.object(lc, "rclone_lsjson", lambda *a, **k: [{"Path": name}]):
            s = lc.process_inbox(cfg, st, False)
        self.assertEqual((s["applied"], s["rejected"]), (1, 0), s)
        self.assertEqual(s["folders"], ["A"])
        self.assertEqual((src / "A/x.xmp").read_bytes(), xmp)          # the raw's own sidecar
        self.assertEqual((src / "A/x.JPG.xmp").read_bytes(), b"<oldj/>")
        st.db.close()


FAKE_RCLONE = """#!/usr/bin/env python3
import sys
a = sys.argv[1:]
if a[0] == "lsjson":
    print("[]"); sys.exit(0)
if a[0] == "sync" and "--dry-run" in a:
    sys.stderr.write("INFO  : x: Skipped copy as --dry-run\\n"); sys.exit(0)
sys.stderr.write("UNEXPECTED mutating call: %s\\n" % a); sys.exit(9)
"""


class DryRun(Tmp):
    def test_dry_run_changes_nothing(self):
        src = self.t / "src"
        self.touch("src/A/one.jpg", b"1" * 10); self.touch("src/A/one.xmp", b"<x/>"); self.touch("src/B/two.arw", b"2" * 10)
        state = self.t / "state"
        (state / "mobile" / "thumbs").mkdir(parents=True)
        (state / "mobile" / "thumbs" / "t.jpg").write_bytes(b"jpg")
        fake = self.t / "rclone"
        fake.write_text(FAKE_RCLONE); fake.chmod(0o755)
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{src}"\nremote = "fake:Remote"\nstate_dir = "{state}"\nrclone = "{fake}"\n')
        st = lc.State(lc.Cfg(cfgf))
        st.put(dict(path="gone.jpg", size=1, mtime_ns=1, status="ok", id=ID)); st.commit(); st.db.close()
        before = (tree_hash(src), tree_hash(state))
        rc = lc.main(["--config", str(cfgf), "sync", "--dry-run"])
        self.assertEqual(rc, 0)
        self.assertEqual((tree_hash(src), tree_hash(state)), before)
        lc.DRY = False

    def test_dry_run_without_state_dir_creates_nothing(self):
        self.touch("src/a.jpg", b"1")
        fake = self.t / "rclone"
        fake.write_text(FAKE_RCLONE); fake.chmod(0o755)
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nremote = "fake:R"\nstate_dir = "{self.t}/state"\nrclone = "{fake}"\n')
        self.assertEqual(lc.main(["--config", str(cfgf), "sync", "--dry-run"]), 0)
        self.assertFalse((self.t / "state").exists())
        lc.DRY = False

    def test_refuses_empty_source(self):
        (self.t / "src").mkdir()
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nstate_dir = "{self.t}/state"\n')
        self.assertEqual(lc.main(["--config", str(cfgf), "sync"]), 2)
        self.assertFalse((self.t / "state").exists())


class InboxCheck(Tmp):
    def run_check(self, stdout, returncode=0):
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nremote = "fake:R"\nstate_dir = "{self.t}/state"\n')
        calls = []

        def fake_rclone(c, args, check=True, input_bytes=None, capture=True):
            calls.append(args)
            return mock.Mock(stdout=stdout, stderr=b"boom", returncode=returncode)
        with mock.patch.object(lc, "rclone", fake_rclone), \
             mock.patch.object(lc.subprocess, "run", return_value=mock.Mock(returncode=0)) as run, \
             mock.patch.dict(os.environ, {"LC_INBOX_SYNC_CMD": "echo WOULD-START"}):
            rc = lc.main(["inbox-check", "--config", str(cfgf)])
        self.assertEqual(calls[0][:5], ["lsf", "-R", "--files-only", "--max-depth", "2"])
        self.assertEqual(calls[0][-1], "fake:R/inbox")
        self.assertFalse((self.t / "state" / "state.db").exists())
        return rc, run

    def test_names_filtered_by_parser(self):
        good = f"phone1/{ID}-20260102T030405Z-{'b' * 16}.xmp"
        out = "\n".join([good, "stray.txt", f"{ID}-20260102T030405Z-none.xmp", "bad dev/x.xmp", "phone1/notes.xmp", ""]).encode()
        rc, run = self.run_check(out)
        self.assertEqual(rc, 0)
        run.assert_called_once_with(["echo", "WOULD-START"])

    def test_only_stray_files_no_start(self):
        rc, run = self.run_check(b"stray.txt\nphone1/junk.xmp\n")
        self.assertEqual(rc, 0)
        run.assert_not_called()

    def test_empty_inbox_no_start(self):
        rc, run = self.run_check(b"")
        self.assertEqual(rc, 0)
        run.assert_not_called()

    def test_missing_inbox_no_start(self):
        rc, run = self.run_check(b"", 3)
        self.assertEqual(rc, 0)
        run.assert_not_called()

    def test_network_error_exit0_no_start(self):
        rc, run = self.run_check(b"", 1)
        self.assertEqual(rc, 0)
        run.assert_not_called()

    def test_start_called_once_for_many(self):
        out = "\n".join(f"p/{ID}-20260102T03040{i}Z-none.xmp" for i in range(5)).encode()
        rc, run = self.run_check(out)
        self.assertEqual(run.call_count, 1)

    def test_config_error_nonzero(self):
        cfgf = self.t / "c.toml"
        cfgf.write_text('pair_primary = "bogus"\n')
        self.assertNotEqual(lc.main(["inbox-check", "--config", str(cfgf)]), 0)


if __name__ == "__main__":
    unittest.main()
