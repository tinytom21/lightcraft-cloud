from unittest import mock
import contextlib, hashlib, io, json, os, stat, subprocess, sys, tempfile, time, unittest
import xml.etree.ElementTree as ET
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import lightcraft_cloud as lc

ID = "a" * 32
NS_RDF = "http://www.w3.org/1999/02/22-rdf-syntax-ns#"


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
        self.assertEqual(lc.notify_message(2, ["X"], "Photos", merged=1),
                         "2 phone edits applied (1 merged with PC changes) in X — in LightCraft run Synchronize Folder on this folder")
        self.assertEqual(lc.notify_message(1, [], "Photos", merged=1),
                         "1 phone edit applied (1 merged with PC changes) — in LightCraft run Synchronize Folder")

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

    def test_merge_verdict_only_on_conflict(self):
        d = lc.decide
        self.assertEqual(d("abc", "zzz", 100, 500, can_merge=True)[0], "merge")   # conflict, either side newer
        self.assertEqual(d("abc", "zzz", 900, 500, can_merge=True)[0], "merge")
        self.assertEqual(d("abc", "abc", 100, 500, can_merge=True)[0], "apply")   # no conflict: plain apply
        self.assertEqual(d("abc", "zzz", 100, 500, inbox_hash="zzz", can_merge=True)[0], "noop")
        self.assertEqual(d("abc", None, 100, None, can_merge=True)[0], "apply")   # PC sidecar gone
        self.assertEqual(d("none", None, 100, None, can_merge=True)[0], "apply")
        self.assertTrue(lc.phone_is_newer(2, 1) and lc.phone_is_newer(1, None))
        self.assertFalse(lc.phone_is_newer(1, 1) or lc.phone_is_newer(1, 2))      # a tie goes to the PC


# A LightCraft-style sidecar: properties as child elements, lc:settings as JSON, an xpacket wrapper.
SETTINGS = json.loads(r"""
{
    "version":1,
    "process":2,
    "profile":{"id":"lc.color","amount":100.0},
    "treatment":"color",
    "wb":{"mode":"asShot","temp":6500.0,"tint":0.0},
    "light":{"exposure":0.0,"contrast":0.0,"highlights":0.0,"shadows":0.0,"whites":0.0,"blacks":0.0},
    "curve":{"highlights":0.0,"lights":0.0,"darks":0.0,"shadows":0.0,"split_shadows":25.0,"split_mid":50.0,"split_highlights":75.0,"master":[],"red":[],"green":[],"blue":[],"refine_saturation":100.0},
    "color":{"vibrance":0.0,"saturation":0.0},
    "mixer":{"red":{"hue":0.0,"sat":0.0,"lum":0.0},"orange":{"hue":0.0,"sat":0.0,"lum":0.0},"yellow":{"hue":0.0,"sat":0.0,"lum":0.0},"green":{"hue":0.0,"sat":0.0,"lum":0.0},"aqua":{"hue":0.0,"sat":0.0,"lum":0.0},"blue":{"hue":0.0,"sat":0.0,"lum":0.0},"purple":{"hue":0.0,"sat":0.0,"lum":0.0},"magenta":{"hue":0.0,"sat":0.0,"lum":0.0}},
    "point_colors":[],
    "bw_mix":{"red":0.0,"orange":0.0,"yellow":0.0,"green":0.0,"aqua":0.0,"blue":0.0,"purple":0.0,"magenta":0.0},
    "grading":{"shadows":{"hue":0.0,"sat":0.0,"lum":0.0},"midtones":{"hue":0.0,"sat":0.0,"lum":0.0},"highlights":{"hue":0.0,"sat":0.0,"lum":0.0},"global":{"hue":0.0,"sat":0.0,"lum":0.0},"blending":50.0,"balance":0.0},
    "effects":{"texture":0.0,"clarity":0.0,"dehaze":0.0},
    "vignette":{"amount":0.0,"midpoint":50.0,"roundness":0.0,"feather":50.0,"highlights":0.0,"style":"highlightPriority"},
    "grain":{"amount":0.0,"size":25.0,"roughness":50.0,"seed":0},
    "detail":{"sharpen_amount":0.0,"sharpen_radius":1.0,"sharpen_detail":25.0,"sharpen_masking":0.0,"nr_luminance":0.0,"nr_detail":50.0,"nr_contrast":0.0,"nr_color":0.0,"nr_color_detail":50.0,"nr_color_smoothness":50.0},
    "optics":{"remove_ca":false,"lens_profile":false,"profile_distortion":100.0,"profile_vignetting":100.0,"defringe_purple_amount":0.0,"defringe_purple_hue_lo":30.0,"defringe_purple_hue_hi":70.0,"defringe_green_amount":0.0,"defringe_green_hue_lo":40.0,"defringe_green_hue_hi":60.0,"distortion":0.0,"vignetting":0.0,"vignetting_midpoint":50.0,"ca_red":0.0,"ca_blue":0.0},
    "geometry":{"upright":"off","guides":[],"upright_transform":null,"vertical":0.0,"horizontal":0.0,"rotate":0.0,"aspect":0.0,"scale":100.0,"offset_x":0.0,"offset_y":0.0,"constrain_crop":false},
    "crop":{"geometry":{"rect":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"angle":0.0},"aspect":null,"flip_h":false,"flip_v":false},
    "orientation":"Normal",
    "masks":[],
    "spots":[],
    "red_eye":[],
    "lens_blur":{"enabled":false,"amount":0.0,"focal_distance":0.0,"focal_range":0.0},
    "enhance":{"denoise":0.0,"raw_details":false,"super_resolution":false},
    "calibration":{"shadows_tint":0.0,"red_hue":0.0,"red_sat":0.0,"green_hue":0.0,"green_sat":0.0,"blue_hue":0.0,"blue_sat":0.0},
    "disabled_sections":[]}
""")
NS_XMP, NS_LC = "http://ns.adobe.com/xap/1.0/", "http://ns.lightcraft.app/lc/1.0/"


def with_settings(*edits, base=SETTINGS):
    """A copy of the default settings with ("light.exposure", 0.5) style edits applied."""
    s = json.loads(json.dumps(base))
    for path, v in edits:
        *parents, leaf = path.split(".")
        d = s
        for k in parents:
            d = d[k]
        d[leaf] = v
    return s


def sidecar(settings=SETTINGS, rating=0, flag="none", extra="", indent=None, packet=True, decl=False, tool="LightCraft",
            compact=True):
    j = json.dumps(settings, separators=(",", ":") if compact else None, indent=indent)
    j = j.replace("&", "&amp;").replace("<", "&lt;")
    return ((b'<?xml version="1.0" encoding="UTF-8"?>\n' if decl else b"") +
            (b'<?xpacket begin="\xef\xbb\xbf" id="W5M0MpCehiHzreSzNTczkc9d"?>\n' if packet else b"") +
            f"""<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="LightCraft">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="{NS_XMP}"
    xmlns:exif="http://ns.adobe.com/exif/1.0/"
    xmlns:lc="{NS_LC}">
   <xmp:CreatorTool>{tool}</xmp:CreatorTool>
   <xmp:Rating>{rating}</xmp:Rating>
   <exif:DateTimeOriginal>2026-10-27T23:51:26</exif:DateTimeOriginal>
   <lc:settings>{j}</lc:settings>
   <lc:flag>{flag}</lc:flag>{extra}
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
""".encode() + (b'<?xpacket end="w"?>' if packet else b""))


def settings_of(data):
    return json.loads(ET.fromstring(data).find(f".//{{{NS_LC}}}settings").text)


def text_of(data, ns, name):
    e = ET.fromstring(data).find(f".//{{{ns}}}{name}")
    return None if e is None else e.text


class SidecarMerge(unittest.TestCase):
    def merge(self, base, pc, phone, newer=True):
        r = lc.merge_sidecars(base, pc, phone, newer)
        self.assertIsNotNone(r)
        self.assertIsNone(lc.validate_xmp(r[0]))
        return r

    def test_disjoint_changes_in_one_section_both_kept(self):
        b = sidecar()
        pc = sidecar(with_settings(("light.exposure", 0.7)))
        ph = sidecar(with_settings(("light.contrast", 25.0), ("color.vibrance", 30.0)))
        for newer in (True, False):
            out, conflicts = self.merge(b, pc, ph, newer)
            self.assertEqual(conflicts, [])
            self.assertEqual(settings_of(out), with_settings(("light.exposure", 0.7), ("light.contrast", 25.0), ("color.vibrance", 30.0)))

    def test_same_leaf_newer_side_wins_and_is_reported(self):
        b = sidecar()
        pc = sidecar(with_settings(("light.exposure", 0.7), ("wb.temp", 5000.0)))
        ph = sidecar(with_settings(("light.exposure", -0.3), ("color.vibrance", 9.0)))
        out, conflicts = self.merge(b, pc, ph, True)
        self.assertEqual(conflicts, ["lc:settings/light/exposure"])
        self.assertEqual(settings_of(out), with_settings(("light.exposure", -0.3), ("wb.temp", 5000.0), ("color.vibrance", 9.0)))
        out, conflicts = self.merge(b, pc, ph, False)
        self.assertEqual(conflicts, ["lc:settings/light/exposure"])
        self.assertEqual(settings_of(out)["light"]["exposure"], 0.7)
        self.assertEqual(settings_of(out)["color"]["vibrance"], 9.0)

    def test_equal_change_on_both_sides_is_no_conflict(self):
        b = sidecar()
        both = with_settings(("light.exposure", 0.5))
        out, conflicts = self.merge(b, sidecar(both, rating=2), sidecar(both), True)
        self.assertEqual(conflicts, [])
        self.assertEqual((settings_of(out), text_of(out, NS_XMP, "Rating")), (both, "2"))

    def test_atomic_sections_are_taken_whole(self):
        edits = {"crop": [("crop.geometry.angle", 5.0), ("crop.aspect", "3:2")],
                 "geometry": [("geometry.rotate", 2.0), ("geometry.scale", 90.0)],
                 "curve": [("curve.lights", 10.0), ("curve.darks", -10.0)],
                 "profile": [("profile.amount", 50.0), ("profile.id", "lc.mono")],
                 "wb": [("wb.temp", 5200), ("wb.tint", 12)]}
        for section, (a, b_) in edits.items():
            base = sidecar()
            pc, ph = sidecar(with_settings(a)), sidecar(with_settings(b_))
            out, conflicts = self.merge(base, pc, ph, True)
            self.assertEqual(conflicts, ["lc:settings/" + section], section)      # not a leaf-by-leaf mix
            self.assertEqual(settings_of(out)[section], with_settings(b_)[section], section)
            out, conflicts = self.merge(base, pc, ph, False)
            self.assertEqual(settings_of(out)[section], with_settings(a)[section], section)

    def test_atomic_section_changed_by_one_side_only(self):
        pc = sidecar(with_settings(("crop.geometry.angle", 5.0)))
        ph = sidecar(with_settings(("light.exposure", 1.0)))
        out, conflicts = self.merge(sidecar(), pc, ph)
        self.assertEqual(conflicts, [])
        self.assertEqual(settings_of(out), with_settings(("crop.geometry.angle", 5.0), ("light.exposure", 1.0)))

    def test_lists_are_atomic(self):
        mask = lambda n: [{"id": n, "kind": "radial"}]
        b = sidecar()
        pc = sidecar(with_settings(("masks", mask(1))))
        ph = sidecar(with_settings(("masks", mask(1) + mask(2))))
        out, conflicts = self.merge(b, pc, ph, False)
        self.assertEqual(conflicts, ["lc:settings/masks"])
        self.assertEqual(settings_of(out)["masks"], mask(1))                      # no element-wise mixing
        out, _ = self.merge(b, pc, ph, True)
        self.assertEqual(settings_of(out)["masks"], mask(1) + mask(2))
        out, conflicts = self.merge(b, b, ph)                                     # changed on one side only
        self.assertEqual((settings_of(out)["masks"], conflicts), (mask(1) + mask(2), []))

    def test_setting_deleted_on_one_side(self):
        gone = json.loads(json.dumps(SETTINGS))
        del gone["light"]["blacks"]
        out, conflicts = self.merge(sidecar(), sidecar(with_settings(("light.exposure", 1.0))), sidecar(gone))
        self.assertEqual(conflicts, [])
        self.assertNotIn("blacks", settings_of(out)["light"])
        self.assertEqual(settings_of(out)["light"]["exposure"], 1.0)
        out, conflicts = self.merge(sidecar(), sidecar(gone), sidecar(with_settings(("color.vibrance", 3.0))))  # PC deleted it
        self.assertEqual(conflicts, [])
        self.assertNotIn("blacks", settings_of(out)["light"])
        self.assertEqual(settings_of(out)["color"]["vibrance"], 3.0)

    def test_property_deleted_on_one_side(self):
        label = '\n   <xmp:Label>Red</xmp:Label>'
        b = sidecar(extra=label)
        out, conflicts = self.merge(b, sidecar(extra=label, rating=4), sidecar())   # phone deleted the label
        self.assertEqual(conflicts, [])
        self.assertIsNone(text_of(out, NS_XMP, "Label"))
        self.assertEqual(text_of(out, NS_XMP, "Rating"), "4")
        self.assertNotIn(b"Label", out)
        self.assertNotIn(b"\n\n", out)                                              # the whole line went
        out, conflicts = self.merge(b, sidecar(), sidecar(extra=label, rating=5))  # PC deleted it, phone kept it
        self.assertEqual((conflicts, text_of(out, NS_XMP, "Label"), text_of(out, NS_XMP, "Rating")), ([], None, "5"))
        out, conflicts = self.merge(b, sidecar(), sidecar(extra=label.replace("Red", "Blue")), True)  # deleted vs changed
        self.assertEqual((conflicts, text_of(out, NS_XMP, "Label")), (["xmp:Label"], "Blue"))
        out, conflicts = self.merge(b, sidecar(), sidecar(extra=label.replace("Red", "Blue")), False)
        self.assertEqual((conflicts, text_of(out, NS_XMP, "Label")), (["xmp:Label"], None))

    def test_property_added_on_one_side(self):
        out, conflicts = self.merge(sidecar(), sidecar(rating=2), sidecar(extra="\n   <xmp:Label>Green</xmp:Label>"))
        self.assertEqual((conflicts, text_of(out, NS_XMP, "Label"), text_of(out, NS_XMP, "Rating")), ([], "Green", "2"))
        self.assertIn(b"\n   <xmp:Label>Green</xmp:Label>", out)                    # indented like its siblings

    def test_rating_and_flag_on_different_sides(self):
        out, conflicts = self.merge(sidecar(), sidecar(rating=4), sidecar(flag="pick"))
        self.assertEqual(conflicts, [])
        self.assertEqual((text_of(out, NS_XMP, "Rating"), text_of(out, NS_LC, "flag")), ("4", "pick"))
        out, conflicts = self.merge(sidecar(), sidecar(rating=4), sidecar(rating=1), False)
        self.assertEqual((conflicts, text_of(out, NS_XMP, "Rating")), (["xmp:Rating"], "4"))

    def test_housekeeping_comes_from_the_newer_side(self):
        extra = '\n   <xmp:ModifyDate>{}</xmp:ModifyDate>'
        b = sidecar(extra=extra.format("2026-01-01"), tool="LightCraft 1")
        pc = sidecar(extra=extra.format("2026-02-01"), tool="LightCraft 2")
        ph = sidecar(extra=extra.format("2026-03-01"), tool="LightCraft Android")
        for newer, want in ((True, ("2026-03-01", "LightCraft Android")), (False, ("2026-02-01", "LightCraft 2"))):
            out, conflicts = self.merge(b, pc, ph, newer)
            self.assertEqual(conflicts, [])
            self.assertEqual((text_of(out, NS_XMP, "ModifyDate"), text_of(out, NS_XMP, "CreatorTool")), want)

    def test_attribute_form_properties(self):
        def dt(rating, exposure, contrast, extra=""):
            return f"""<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="{NS_XMP}"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmp:Rating="{rating}"
    crs:Exposure2012="{exposure}"
    crs:Contrast2012='{contrast}'
    darktable:xmp_version="5"{extra}>
   <darktable:history>
    <rdf:Seq><rdf:li darktable:num="0" darktable:operation="exposure"/></rdf:Seq>
   </darktable:history>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
""".encode()
        out, conflicts = self.merge(dt(1, "+0.00", "+0"), dt(3, "+0.50", "+0"), dt(1, "+0.00", "+25"))
        self.assertEqual(conflicts, [])
        root = ET.fromstring(out).find(f".//{{{NS_RDF}}}Description")
        crs = "{http://ns.adobe.com/camera-raw-settings/1.0/}"
        self.assertEqual((root.get(f"{{{NS_XMP}}}Rating"), root.get(crs + "Exposure2012"), root.get(crs + "Contrast2012")),
                         ("3", "+0.50", "+25"))
        self.assertIn(b"darktable:xmp_version=\"5\"", out)
        self.assertIn(b"<rdf:li darktable:num=\"0\" darktable:operation=\"exposure\"/>", out)   # foreign data untouched
        self.assertIn(b"crs:Contrast2012='+25'", out)                                         # quote style kept
        out, conflicts = self.merge(dt(1, "+0.00", "+0"), dt(3, "+0.50", "+0"), dt(1, "+0.90", "+0"), True)
        self.assertEqual(conflicts, ["crs:Exposure2012"])

    def test_attribute_and_element_forms_are_the_same_property(self):
        attr = lambda r: f"""<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="{NS_RDF}">
 <rdf:Description rdf:about="" xmlns:xmp="{NS_XMP}" xmlns:crs="c" xmp:Rating="{r}" crs:Exposure2012="1"/></rdf:RDF></x:xmpmeta>""".encode()
        elem = lambda r: f"""<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="{NS_RDF}">
 <rdf:Description rdf:about="" xmlns:xmp="{NS_XMP}" xmlns:crs="c"><xmp:Rating>{r}</xmp:Rating><crs:Exposure2012>1</crs:Exposure2012></rdf:Description></rdf:RDF></x:xmpmeta>""".encode()
        out, conflicts = self.merge(attr(1), attr(1), elem(5))             # PC writes attributes, phone elements
        self.assertEqual(conflicts, [])
        self.assertIn(b'xmp:Rating="5"', out)                                # rewritten in the PC's form
        out, conflicts = self.merge(elem(1), elem(1), attr(5))
        self.assertEqual(conflicts, [])
        self.assertIn(b"<xmp:Rating>5</xmp:Rating>", out)
        out, conflicts = self.merge(attr(1), attr(2), elem(2))             # same value in different forms: no change
        self.assertEqual((conflicts, out), ([], attr(2)))

    def test_foreign_properties_comments_and_layout_preserved(self):
        foreign = """
   <!-- kept -->
   <darktable:history xmlns:darktable="http://darktable.sf.net/"><rdf:Seq><rdf:li>exposure</rdf:li></rdf:Seq></darktable:history>"""
        pc = sidecar(with_settings(("light.exposure", 0.7)), extra=foreign, decl=True)
        out, _ = self.merge(sidecar(), pc, sidecar(with_settings(("color.vibrance", 9.0)), rating=2))
        self.assertIn(foreign.encode(), out)
        cut = lambda d: d[:d.index(b"<lc:settings>")]
        self.assertEqual(cut(out), cut(pc).replace(b"<xmp:Rating>0<", b"<xmp:Rating>2<"))   # everything before: byte for byte

    def test_prefixes_xpacket_and_declaration_preserved(self):
        pc = sidecar(with_settings(("light.exposure", 0.7)), decl=True)
        out, _ = self.merge(sidecar(), pc, sidecar(with_settings(("color.vibrance", 9.0)), flag="pick"))
        self.assertTrue(out.startswith(b'<?xml version="1.0" encoding="UTF-8"?>\n<?xpacket begin="\xef\xbb\xbf" id="W5M0MpCehiHzreSzNTczkc9d"?>\n<x:xmpmeta'))
        self.assertTrue(out.endswith(b'</x:xmpmeta>\n<?xpacket end="w"?>'))
        self.assertNotIn(b"ns0", out)
        for needle in (b'xmlns:xmp="', b'xmlns:lc="', b"<lc:flag>pick</lc:flag>", b"<x:xmpmeta xmlns:x=", b"rdf:about"):
            self.assertIn(needle, out)
        # only the two changed values differ from the PC file
        before, after = pc.decode(), out.decode()
        self.assertEqual(before.replace("<lc:flag>none<", "<lc:flag>pick<").replace('"vibrance":0.0', '"vibrance":9.0'), after)

    def test_json_layout_follows_the_pc_file(self):
        pc_ind = sidecar(with_settings(("light.exposure", 0.7)), indent=2, compact=False)
        ph = sidecar(with_settings(("color.vibrance", 9.0)))
        out, _ = self.merge(sidecar(), pc_ind, ph)
        text = ET.fromstring(out).find(f".//{{{NS_LC}}}settings").text
        self.assertEqual(text, json.dumps(with_settings(("light.exposure", 0.7), ("color.vibrance", 9.0)), indent=2))
        pc_sp = sidecar(with_settings(("light.exposure", 0.7)), compact=False)          # json.dumps default spacing
        out, _ = self.merge(sidecar(), pc_sp, ph)
        self.assertIn(b'"color": {"vibrance": 9.0,', out)
        pc_tab = sidecar(with_settings(("light.exposure", 0.7)), indent="\t", compact=False)
        out, _ = self.merge(sidecar(), pc_tab, ph)
        self.assertIn(b'\n\t"version": 1,', out)
        out, _ = self.merge(sidecar(), sidecar(with_settings(("light.exposure", 0.7))), ph)
        self.assertIn(b'"vibrance":9.0', out)                                          # compact stays compact

    def test_text_is_escaped(self):
        tricky = with_settings(("profile.id", 'a<b>&"c"'))
        out, _ = self.merge(sidecar(), sidecar(tricky), sidecar(with_settings(("light.exposure", 1.0))))
        self.assertEqual(settings_of(out)["profile"]["id"], 'a<b>&"c"')

    def test_complex_properties_and_namespace_declarations(self):
        dc = "http://purl.org/dc/elements/1.1/"
        phone = sidecar(extra=f"""
   <d:subject xmlns:d="{dc}"><rdf:Bag><rdf:li>trip</rdf:li><rdf:li>sea &amp; sun</rdf:li></rdf:Bag></d:subject>""", rating=3)
        # the PC does not declare dc at all; it binds the phone's prefix d to something else
        pc = sidecar().replace(b'xmlns:lc="', b'xmlns:d="urn:other" xmlns:lc="')
        out, conflicts = self.merge(sidecar(), pc, phone)
        self.assertEqual(conflicts, [])
        subj = ET.fromstring(out).find(f".//{{{dc}}}subject")
        self.assertEqual([li.text for li in subj.iter(f"{{{NS_RDF}}}li")], ["trip", "sea & sun"])
        self.assertEqual(text_of(out, NS_XMP, "Rating"), "3")
        # replacing a complex value in place
        phone2 = sidecar(extra=f"""
   <dc:subject xmlns:dc="{dc}"><rdf:Bag><rdf:li>trip</rdf:li><rdf:li>new</rdf:li></rdf:Bag></dc:subject>""")
        pc2 = sidecar(extra=f"""
   <dc:subject xmlns:dc="{dc}"><rdf:Bag><rdf:li>trip</rdf:li></rdf:Bag></dc:subject>""", flag="pick")
        base2 = sidecar(extra=f"""
   <dc:subject xmlns:dc="{dc}"><rdf:Bag><rdf:li>trip</rdf:li></rdf:Bag></dc:subject>""")
        out, conflicts = self.merge(base2, pc2, phone2)
        self.assertEqual(conflicts, [])
        subj = ET.fromstring(out).find(f".//{{{dc}}}subject")
        self.assertEqual([li.text for li in subj.iter(f"{{{NS_RDF}}}li")], ["trip", "new"])
        self.assertEqual(text_of(out, NS_LC, "flag"), "pick")

    def test_attribute_added_with_an_undeclared_prefix(self):
        crs = "http://ns.adobe.com/camera-raw-settings/1.0/"
        phone = sidecar().replace(b'xmlns:lc="', f'xmlns:crs="{crs}" crs:Exposure2012="+1.00" xmlns:lc="'.encode())
        out, conflicts = self.merge(sidecar(), sidecar(rating=1), phone)
        self.assertEqual(conflicts, [])
        self.assertEqual(ET.fromstring(out).find(f".//{{{NS_RDF}}}Description").get(f"{{{crs}}}Exposure2012"), "+1.00")
        self.assertEqual(out.count(b"xmlns:crs="), 1)

    def test_self_closing_description(self):
        crs = "http://ns.adobe.com/camera-raw-settings/1.0/"
        doc = lambda body, attrs="": f"""<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="{NS_RDF}">
  <rdf:Description rdf:about="" xmlns:xmp="{NS_XMP}" xmlns:crs="{crs}" xmlns:dc="http://purl.org/dc/elements/1.1/" crs:Exposure2012="0"{attrs}{body}
 </rdf:RDF>
</x:xmpmeta>
""".encode()
        base = doc("/>")
        pc = doc("/>", ' xmp:Rating="2"')
        phone = doc('><dc:title><rdf:Alt><rdf:li xml:lang="x-default">Sunset</rdf:li></rdf:Alt></dc:title></rdf:Description>')
        out, conflicts = self.merge(base, pc, phone)
        self.assertEqual(conflicts, [])
        d = ET.fromstring(out).find(f".//{{{NS_RDF}}}Description")
        self.assertEqual(d.get(f"{{{NS_XMP}}}Rating"), "2")
        self.assertEqual(d.find(".//{http://purl.org/dc/elements/1.1/}title//{%s}li" % NS_RDF).text, "Sunset")
        self.assertEqual(out.count(b"xmp:Rating"), 1)

    def test_nothing_to_merge_returns_the_pc_bytes(self):
        pc = sidecar(with_settings(("light.exposure", 0.7)))
        self.assertEqual(self.merge(sidecar(), pc, sidecar())[0], pc)           # phone changed nothing
        self.assertEqual(self.merge(sidecar(), pc, pc)[0], pc)                   # both made the same edit

    def test_gives_up_when_it_cannot_merge(self):
        b, ok = sidecar(), sidecar(with_settings(("light.exposure", 0.7)))
        merge = lambda *a: lc.merge_sidecars(*a, True)
        self.assertIsNotNone(merge(b, ok, sidecar(with_settings(("color.vibrance", 5.0)))))
        for key in ("version", "process"):                                     # engine versions differ
            self.assertIsNone(merge(b, ok, sidecar(with_settings((key, 99)))), key)
            self.assertIsNone(merge(b, sidecar(with_settings((key, 99))), ok), key)
        bad_json = sidecar().replace(b'"version":1,', b'"version":1,,')
        for args in ((bad_json, ok, b), (b, bad_json, ok), (b, ok, bad_json)):
            self.assertIsNone(merge(*args))
        bad_xml = ok[:-40]
        for args in ((bad_xml, ok, b), (b, bad_xml, ok), (b, ok, bad_xml), (b, ok, b"")):
            self.assertIsNone(merge(*args))
        crs_only = b'<x:xmpmeta xmlns:x="a"><rdf:RDF xmlns:rdf="%s"><rdf:Description rdf:about="" xmlns:crs="c" crs:Exposure2012="1"/></rdf:RDF></x:xmpmeta>' % NS_RDF.encode()
        self.assertIsNone(merge(crs_only, ok, b))                              # the base has no lc:settings
        self.assertIsNone(merge(crs_only, crs_only, b))
        self.assertIsNotNone(merge(crs_only, crs_only, crs_only))              # nobody has any
        self.assertIsNone(merge(b, ok, b'<a xmlns:lc="x">lc:settings</a>'))    # no rdf:Description
        self.assertIsNone(merge(b, ok, b"<!DOCTYPE a [<!ENTITY e 'x'>]>" + b))
        self.assertIsNone(merge(b, sidecar(decl=True).replace(b'encoding="UTF-8"', b'encoding="ISO-8859-1"'), b))
        self.assertIsNotNone(merge(b, sidecar(decl=True), b))
        self.assertIsNone(merge(b, ok, b.replace(b"<lc:flag>none</lc:flag>", b"<lc:flag>a</lc:flag><lc:flag>b</lc:flag>")))  # twice
        self.assertIsNone(merge(b, ok, b.replace(b'{"version"', b'["version"')))   # not a JSON object

    def test_phone_chain_revert_wins(self):
        """The phone's next edit names its own previous edit E1 as base, which the PC never had as a version."""
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)), rating=2)              # the PC edits the photo
        e1 = sidecar(with_settings(("light.exposure", 0.5), ("light.contrast", 20.0)))  # the phone edits V0
        m1, conflicts = self.merge(v0, pc1, e1)
        self.assertEqual(conflicts, [])
        e2 = sidecar(with_settings(("light.exposure", 0.0), ("light.contrast", 20.0)))  # the phone reverts the exposure of E1
        m2, conflicts = self.merge(e1, m1, e2, False)                                  # even though the PC is "newer"
        self.assertEqual(conflicts, [])
        self.assertEqual(settings_of(m2), with_settings(("light.exposure", 0.0), ("light.contrast", 20.0), ("color.vibrance", 30.0)))
        self.assertEqual(text_of(m2, NS_XMP, "Rating"), "2")                          # the PC's own change survives
        m2b, _ = self.merge(v0, m1, e2, False)                                        # (against V0 the revert looks like no change)
        self.assertEqual(settings_of(m2b)["light"]["exposure"], 0.5)


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


PID = "5" * 32


class InboxMerge(Tmp):
    """process_inbox with a fake rclone: conflicts between a phone edit and a PC edit are merged."""
    def setUp(self):
        super().setUp()
        self.today = lc.today_utc()

    def setup(self, pc, age=1000, stored=()):
        self.touch("src/A/x.jpg")
        self.touch("src/A/x.xmp", pc, age=age)
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nremote = "fake:R"\nstate_dir = "{self.t}/state"\n')
        self.cfg = lc.Cfg(cfgf)
        self.st = lc.State(self.cfg)
        self.addCleanup(self.st.db.close)
        self.st.put(dict(path="A/x.jpg", size=1, mtime_ns=1, status="ok", id=PID, sidecar="A/x.xmp"))
        self.st.commit()
        for data in stored:
            lc.store_version(lc.versions_dir(self.cfg), data)

    def name(self, data_base, stamp="20990101T000000Z", base=None):
        return f"phone/{PID}-{stamp}-{base or lc.sha16(data_base)}.xmp"

    def run_inbox(self, files, dry=False):
        calls, lines = [], []

        def fake_rclone(c, args, check=True, input_bytes=None, capture=True):
            calls.append(args)
            if args[0] == "cat":
                return mock.Mock(stdout=files[args[1].split("/inbox/", 1)[1]], returncode=0)
            return mock.Mock(stdout=b"", returncode=0)
        with mock.patch.object(lc, "rclone", fake_rclone), \
             mock.patch.object(lc, "rclone_lsjson", lambda *a, **k: [{"Path": n} for n in files]), \
             mock.patch.object(lc, "log", lambda m: lines.append(m)):
            s = lc.process_inbox(self.cfg, self.st, dry)
        self.calls, self.lines = calls, lines
        return s

    def history(self):
        d = self.cfg.state / "history" / self.today / "A"
        return {f.name: f.read_bytes() for f in d.glob("*")} if d.is_dir() else {}

    def pc(self):
        return (self.t / "src/A/x.xmp").read_bytes()

    def test_conflict_is_merged(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)), rating=2)
        e1 = sidecar(with_settings(("light.exposure", 0.5), ("wb.temp", 5000.0)), flag="pick")
        self.setup(pc1, stored=[v0])
        name = self.name(v0)
        s = self.run_inbox({name: e1})
        self.assertEqual((s["applied"], s["merged"], s["kept"], s["noop"], s["rejected"]), (0, 1, 0, 0, 0), s)
        self.assertEqual(s["folders"], ["A"])
        merged = self.pc()
        self.assertEqual(settings_of(merged), with_settings(("color.vibrance", 30.0), ("light.exposure", 0.5), ("wb.temp", 5000.0)))
        self.assertEqual((text_of(merged, NS_XMP, "Rating"), text_of(merged, NS_LC, "flag")), ("2", "pick"))
        h = self.history()
        self.assertEqual(len(h), 2, sorted(h))
        self.assertEqual(sorted(h.values(), key=len), sorted([pc1, e1], key=len))
        self.assertIn("x.xmp.phone.20990101T000000Z.xmp", h)                       # same tags as apply / keep
        self.assertTrue(any(n.startswith("x.xmp.pc-") for n in h))
        self.assertEqual(h["x.xmp.phone.20990101T000000Z.xmp"], e1)
        self.assertIn(f"inbox: {name} -> merge (0 conflicts)", self.lines)
        self.assertEqual(sum(1 for c in self.calls if c[0] == "moveto"), 1)
        self.assertEqual(s["items"], [f"merge {name}: 0 conflicts"])
        vd = lc.versions_dir(self.cfg)
        self.assertTrue((vd / f"{lc.sha16(e1)}.xmp").exists() and (vd / f"{lc.sha16(merged)}.xmp").exists())

    def test_conflicts_are_logged(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("light.exposure", 0.9), ("light.contrast", 5.0)), rating=2)
        e1 = sidecar(with_settings(("light.exposure", 0.1), ("light.contrast", 7.0)), rating=5)
        self.setup(pc1, stored=[v0])
        name = self.name(v0)
        s = self.run_inbox({name: e1})
        self.assertEqual(s["merged"], 1)
        self.assertIn(f"inbox: {name} -> merge (3 conflicts: xmp:Rating, lc:settings/light/exposure, lc:settings/light/contrast)", self.lines)
        self.assertEqual(settings_of(self.pc())["light"], {**SETTINGS["light"], "exposure": 0.1, "contrast": 7.0})   # phone is newer

    def test_pc_newer_wins_the_conflicting_leaf(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("light.exposure", 0.9)))
        e1 = sidecar(with_settings(("light.exposure", 0.1), ("color.vibrance", 4.0)))
        self.setup(pc1, age=0, stored=[v0])                                          # the PC file was just written
        s = self.run_inbox({self.name(v0, "20000101T000000Z"): e1})
        self.assertEqual((s["merged"], s["kept"]), (1, 0))
        self.assertEqual(settings_of(self.pc()), with_settings(("light.exposure", 0.9), ("color.vibrance", 4.0)))

    def test_base_not_stored_falls_back_to_newest_wins(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        self.setup(pc1)                                                              # v0 was never stored
        s = self.run_inbox({self.name(v0): e1})
        self.assertEqual((s["applied"], s["merged"], s["kept"]), (1, 0, 0))
        self.assertEqual(self.pc(), e1)                                              # whole-file newest-wins, as before
        self.assertEqual(sorted(self.history()), [n for n in sorted(self.history()) if n.startswith("x.xmp.pc-")])
        self.assertEqual(list(self.history().values()), [pc1])
        self.assertIn(f"inbox: {self.name(v0)} -> apply (conflict: phone edit is newer; PC version kept in history)", self.lines)

    def test_base_not_stored_pc_newer_keeps_pc(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        self.setup(pc1, age=0)
        s = self.run_inbox({self.name(v0, "20000101T000000Z"): e1})
        self.assertEqual((s["applied"], s["merged"], s["kept"]), (0, 0, 1))
        self.assertEqual(self.pc(), pc1)
        self.assertEqual(list(self.history().values()), [e1])

    def test_unmergeable_falls_back_to_newest_wins(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("process", 3), ("light.exposure", 0.5)))        # another engine process
        self.setup(pc1, stored=[v0])
        s = self.run_inbox({self.name(v0): e1})
        self.assertEqual((s["applied"], s["merged"], s["kept"]), (1, 0, 0))
        self.assertEqual(self.pc(), e1)
        self.assertIn("-> apply (conflict: phone edit is newer", self.lines[0])

    def test_base_none_with_existing_sidecar_is_not_merged(self):
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        self.setup(pc1, stored=[sidecar()])
        s = self.run_inbox({f"phone/{PID}-20990101T000000Z-none.xmp": e1})
        self.assertEqual((s["applied"], s["merged"]), (1, 0))

    def test_plain_apply_does_not_merge(self):
        v0 = sidecar()
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        self.setup(v0, stored=[v0])
        s = self.run_inbox({self.name(v0): e1})
        self.assertEqual((s["applied"], s["merged"]), (1, 0))
        self.assertEqual(self.pc(), e1)

    def test_merge_that_changes_nothing_is_a_noop(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(), indent=2, compact=False)                       # same settings, other layout
        self.setup(pc1, stored=[v0])
        s = self.run_inbox({self.name(v0): e1})
        self.assertEqual((s["applied"], s["merged"], s["noop"]), (0, 0, 1))
        self.assertEqual((self.pc(), self.history()), (pc1, {}))

    def test_phone_chain_in_one_run(self):
        """E2 names E1 as its base. E1 was merged, so the PC never had E1 as a version; the inbox file is the store."""
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("light.exposure", 0.5), ("light.contrast", 20.0)))
        e2 = sidecar(with_settings(("light.exposure", 0.0), ("light.contrast", 20.0)))   # reverts E1's exposure
        self.setup(pc1, stored=[v0])
        s = self.run_inbox({self.name(v0, "20990101T000000Z"): e1, self.name(e1, "20990101T000100Z"): e2})
        self.assertEqual((s["merged"], s["applied"], s["kept"]), (2, 0, 0), s["items"])
        self.assertEqual(settings_of(self.pc()), with_settings(("light.exposure", 0.0), ("light.contrast", 20.0), ("color.vibrance", 30.0)))

    def test_phone_chain_across_runs(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        e2 = sidecar(with_settings(("light.exposure", 0.0)))
        self.setup(pc1, stored=[v0])
        self.run_inbox({self.name(v0): e1})
        m1 = self.pc()
        self.assertEqual(settings_of(m1), with_settings(("color.vibrance", 30.0), ("light.exposure", 0.5)))
        os.utime(self.t / "src/A/x.xmp", (time.time() - 500,) * 2)
        s = self.run_inbox({self.name(e1, "20990101T000100Z"): e2})
        self.assertEqual(s["merged"], 1)
        self.assertEqual(settings_of(self.pc()), with_settings(("color.vibrance", 30.0)))

    def test_dry_run_merges_in_memory_only(self):
        v0 = sidecar()
        pc1 = sidecar(with_settings(("color.vibrance", 30.0)))
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        self.setup(pc1, stored=[v0])
        before = (tree_hash(self.t / "src"), tree_hash(self.cfg.state))
        s = self.run_inbox({self.name(v0): e1}, dry=True)
        self.assertEqual((s["merged"], s["applied"], s["kept"]), (1, 0, 0))
        self.assertEqual((tree_hash(self.t / "src"), tree_hash(self.cfg.state)), before)
        self.assertEqual([c[0] for c in self.calls], ["cat"])                        # nothing moved on the remote either

    def test_dry_run_stores_no_version(self):
        self.setup(sidecar())
        self.run_inbox({self.name(sidecar()): sidecar(with_settings(("light.exposure", 0.5)))}, dry=True)
        self.assertFalse(lc.versions_dir(self.cfg).exists())

    def test_inbox_files_are_stored_on_receipt(self):
        v0 = sidecar()
        e1 = sidecar(with_settings(("light.exposure", 0.5)))
        self.setup(v0)
        self.run_inbox({self.name(v0): e1, f"phone/{'6' * 32}-20990101T000000Z-none.xmp": e1})   # (second: unknown id, rejected)
        self.assertEqual((lc.versions_dir(self.cfg) / f"{lc.sha16(e1)}.xmp").read_bytes(), e1)
        self.assertFalse((lc.versions_dir(self.cfg) / f"{lc.sha16(b'<junk/>')}.xmp").exists())
        self.run_inbox({self.name(v0, "20990101T000100Z"): b"<junk/>"})                    # invalid: not stored
        self.assertFalse((lc.versions_dir(self.cfg) / f"{lc.sha16(b'<junk/>')}.xmp").exists())


class SidecarVersions(Tmp):
    def cfg(self):
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nremote = "fake:R"\nstate_dir = "{self.t}/state"\n')
        return lc.Cfg(cfgf)

    def test_scan_saves_stable_sidecars_once(self):
        self.touch("src/a.jpg"); self.touch("src/a.xmp", b"<a>one</a>", age=100)
        self.touch("src/b.jpg"); self.touch("src/b.xmp", b"<b>being written</b>", age=2)
        v = self.t / "v"
        entries, unstable = lc.scan_source(self.t / "src", versions=v)
        self.assertEqual(unstable, {"b.xmp"})
        self.assertEqual({f.name for f in v.glob("*")}, {lc.sha16(b"<a>one</a>") + ".xmp"})     # not the unstable one, no temp files
        self.assertEqual(entries["a.jpg"]["sidecar_hash"], lc.sha16(b"<a>one</a>"))
        self.assertEqual((v / (lc.sha16(b"<a>one</a>") + ".xmp")).read_bytes(), b"<a>one</a>")
        f = v / (lc.sha16(b"<a>one</a>") + ".xmp")
        os.utime(f, (1, 1))
        lc.scan_source(self.t / "src", versions=v)
        self.assertEqual(f.stat().st_mtime, 1)                                                 # not rewritten
        self.touch("src/a.xmp", b"<a>two</a>", age=100)                                         # every version is kept
        lc.scan_source(self.t / "src", versions=v)
        self.assertEqual(len(list(v.glob("*.xmp"))), 2)

    def test_scan_without_store_writes_nothing(self):
        self.touch("src/a.jpg"); self.touch("src/a.xmp", b"<a/>")
        lc.scan_source(self.t / "src")
        self.assertEqual(sorted(p.name for p in self.t.rglob("*")), ["a.jpg", "a.xmp", "src"])

    def test_store_and_load_check_the_hash(self):
        cfg = self.cfg()
        lc.store_version(lc.versions_dir(cfg), b"<one/>")
        h = lc.sha16(b"<one/>")
        self.assertEqual(lc.load_version(cfg, h), b"<one/>")
        self.assertIsNone(lc.load_version(cfg, "0" * 16))
        (lc.versions_dir(cfg) / f"{h}.xmp").write_bytes(b"<tor")                                # a torn or damaged file
        self.assertIsNone(lc.load_version(cfg, h))

    def test_purge_keeps_current_versions_and_drops_old_ones(self):
        cfg = self.cfg()
        d = lc.versions_dir(cfg)
        datas = {n: f"<v>{n}</v>".encode() for n in ("old_current", "old_unused", "new_unused")}
        for data in datas.values():
            lc.store_version(d, data)
        long_ago = time.time() - 90 * 86400
        for n in ("old_current", "old_unused"):
            os.utime(d / f"{lc.sha16(datas[n])}.xmp", (long_ago, long_ago))
        (d / ".lightcraft-ab12cd34.tmp").write_bytes(b"<half")                                  # interrupted write
        os.utime(d / ".lightcraft-ab12cd34.tmp", (long_ago, long_ago))
        st = lc.State(cfg)
        st.put(dict(path="a.jpg", size=1, mtime_ns=1, status="ok", sidecar="a.xmp", sidecar_hash=lc.sha16(datas["old_current"])))
        st.commit(); st.db.close()
        with mock.patch.object(lc, "latest_backup_age_hours", return_value=1.0), \
             mock.patch.object(lc, "rclone", return_value=mock.Mock(returncode=3, stdout=b"", stderr=b"")), \
             mock.patch.object(lc, "log"):
            lc.cmd_purge(cfg, 30)
        self.assertEqual({f.name for f in d.iterdir()}, {lc.sha16(datas["old_current"]) + ".xmp", lc.sha16(datas["new_unused"]) + ".xmp"})

    def test_purge_refused_without_recent_backup_keeps_versions(self):
        cfg = self.cfg()
        lc.store_version(lc.versions_dir(cfg), b"<v/>")
        f = next(lc.versions_dir(cfg).glob("*.xmp"))
        os.utime(f, (1, 1))
        with mock.patch.object(lc, "latest_backup_age_hours", return_value=100.0):
            with self.assertRaises(lc.Refuse):
                lc.cmd_purge(cfg, 30)
        self.assertTrue(f.exists())

    def test_notification_counts_merged_edits(self):
        msgs = []
        cfg = self.cfg()
        with mock.patch.object(lc.subprocess, "run", lambda argv, **k: msgs.append(argv)):
            lc.notify(cfg, 3, ["A"], 2)
            lc.notify(cfg, 0, ["A"], 0)
        self.assertEqual(msgs, [["notify-send", "LightCraft cloud",
                                 "3 phone edits applied (2 merged with PC changes) in A — in LightCraft run Synchronize Folder on this folder"]])


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

    def test_sync_log_reports_merged_edits(self):
        self.touch("src/a.jpg", b"1")
        fake = self.t / "rclone"
        fake.write_text(FAKE_RCLONE); fake.chmod(0o755)
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nremote = "fake:R"\nstate_dir = "{self.t}/state"\nrclone = "{fake}"\n')
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(lc.main(["--config", str(cfgf), "sync", "--dry-run"]), 0)
        self.assertIn("inbox: applied=0 merged=0 kept-pc=0 rejected=0 identical=0", out.getvalue())
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


class ManifestPublish(Tmp):
    """Step 6: the manifest goes to the remote in use whenever that remote does not have this photo list."""
    PHOTOS = [{"id": ID, "path": "A/one.jpg"}]

    def cfg(self, remote):
        cfgf = self.t / "c.toml"
        cfgf.write_text(f'source = "{self.t}/src"\nremote = "{remote}"\nstate_dir = "{self.t}/state"\n')
        return lc.Cfg(cfgf)

    def publish(self, remote, photos=None, fail=False, calls=None):
        cfg = self.cfg(remote)
        st = lc.State(cfg)

        def fake(c, args, check=True, input_bytes=None, capture=True):
            if calls is not None:
                calls.append(args)
            if fail and args[0] == "moveto":
                raise RuntimeError("boom")
            return mock.Mock(returncode=0)
        try:
            with mock.patch.object(lc, "rclone", fake), contextlib.redirect_stdout(io.StringIO()):
                return lc.publish_manifest(cfg, st, self.PHOTOS if photos is None else photos)
        finally:
            st.commit()
            st.db.close()

    def test_unchanged_list_is_uploaded_once(self):
        calls = []
        self.assertTrue(self.publish("r:A", calls=calls))
        self.assertFalse(self.publish("r:A", calls=calls))
        self.assertEqual([c[0] for c in calls], ["copyto", "moveto"])
        self.assertEqual(calls[1][-1], "r:A/mobile/manifest.json")

    def test_changed_list_is_uploaded(self):
        self.assertTrue(self.publish("r:A"))
        self.assertTrue(self.publish("r:A", photos=[]))

    def test_a_new_remote_gets_the_manifest_although_the_list_is_unchanged(self):
        self.assertTrue(self.publish("r:A"))
        calls = []
        self.assertTrue(self.publish("r:B", calls=calls))
        self.assertEqual(calls[1][-1], "r:B/mobile/manifest.json")
        self.assertFalse(self.publish("r:B"))

    def test_a_failed_upload_is_retried(self):
        self.assertTrue(self.publish("r:A"))
        with self.assertRaises(RuntimeError):
            self.publish("r:A", photos=[], fail=True)
        self.assertTrue(self.publish("r:A", photos=[]))
        self.assertFalse(self.publish("r:A", photos=[]))

    def test_state_from_before_the_remote_was_recorded(self):
        cfg = self.cfg("r:A")
        (cfg.mobile).mkdir(parents=True)
        (cfg.mobile / "manifest.json").write_text(json.dumps(lc.manifest_doc(self.PHOTOS)))
        st = lc.State(cfg)
        st.set("manifest_uploaded", "1")
        st.commit()
        st.db.close()
        self.assertFalse(self.publish("r:A"))   # it went to the remote in use then
        self.assertTrue(self.publish("r:B"))    # and a later switch is noticed


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
