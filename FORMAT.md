# LightCraft cloud layout, format 1

A PC runs `lc-cloud sync`, which publishes a photo folder to any rclone remote. A phone app browses
thumbnails, downloads small smart previews to edit, and sends edits back. **The PC is the master copy.**

```
<remote>/
  originals/<relative path>        photos + sidecars, mirrored one-way from the PC source root
  mobile/manifest.json             written LAST, atomically (manifest.json.tmp-<rand>, then moveto)
  mobile/thumbs/<id>.jpg           ~512 px long edge, JPEG q~80, rendered WITH the current edits
  mobile/previews/<id>.lcsp        LightCraft smart preview (2560 px long edge), flat folder
  inbox/<device>/<id>-<UTCstamp>-<base>.xmp   phone edits; never overwritten
  inbox-done/<UTC date>/<device>/...          processed inbox files (moved, not deleted)
  history/<UTC date>/...           rclone --backup-dir for anything replaced/deleted under originals/ or mobile/
```
Backup-dir files get a `.HHMMSS` suffix before the extension (`photo.153012.jpg`).

## manifest.json
```json
{"format": 1, "generated": "2026-10-27T02:25:25Z", "publisher": "lightcraft-cloud 0.1.0",
 "photos": [{"id": "<32 hex>", "path": "A/IMG_0001.ARW", "size": 24000000, "mtime": "2024-05-01T12:00:00Z",
   "sidecar": "A/IMG_0001.xmp", "sidecarVersion": "d4c93bdc22b6b5f8", "captured": "2024-05-01T09:30:00+02:00",
   "camera": "ExampleCam X1", "lens": "...", "width": 6000, "height": 4000, "rating": 0, "pick": false,
   "flag": "none", "label": null, "thumb": "thumbs/<id>.jpg", "preview": "previews/<id>.lcsp",
   "previewKey": "<32 hex>", "previewBytes": 2629027,
   "alternates": [{"id": "<32 hex>", "path": "A/IMG_0001.JPG", "sidecar": "A/IMG_0001.JPG.xmp",
     "sidecarVersion": null, "preview": "previews/<id>.lcsp", "previewKey": "<32 hex>", "previewBytes": 812345,
     "thumb": "thumbs/<id>.jpg", "width": 6000, "height": 4000}]}]}
```
* `id` = the LightCraft `content_hash` (32 hex) of the original file's bytes; identical on every device.
* `path` is relative to `originals/`, POSIX separators. `sidecar` is the XMP path relative to `originals/` or null.
* `sidecarVersion` = first 16 hex of SHA-256 of the sidecar's bytes, or null when there is no sidecar.
  It changes whenever the sidecar or the edits change; use it to decide whether to refetch the thumbnail.
* `previewKey`: the engine names a smart preview `<previewKey>.lcsp` (a hash of the content hash), in
  `Smart Previews/<first 2 hex>/` of a LightCraft library. The remote file is named by `id` so clients need not know this;
  a client that drops the file into a library must rename it to `<previewKey>.lcsp`.
* Unavailable fields are null. Duplicate files (identical bytes) are listed once.
* `alternates` (optional, additive; absent when the photo has no partner): a raw file and a JPEG (`.jpg`/`.jpeg`)
  with the same stem in the same folder are one shot. Only the primary is a top-level photo (the JPEG by default;
  the raw when the publisher is configured with `pair_primary = "raw"`); the other file is listed in its `alternates`
  with its own id, thumb and preview. Pairing needs exactly one raw and one JPEG for the stem. Both files are
  uploaded to `originals/`. A client that ignores `alternates` simply shows the primary. A client must not show an
  alternate as a photo of its own. Inbox edits may be addressed to the id of either file; each id edits its own
  file's sidecar.
* Folders matching the publisher's `exclude` patterns are not published at all (no originals, thumbs or
  manifest entries).
* The manifest is only replaced when the photo list changes, and only after thumbs/previews are uploaded.

## Inbox (phone to PC)
* A client only ever **creates new files** in `inbox/<device>/`, named `<id>-<UTCstamp>-<base>.xmp`:
  `id` 32 hex from the manifest; `UTCstamp` `YYYYMMDDTHHMMSSZ`; `base` = the `sidecarVersion` the edit started from
  (from the manifest), or `none` if the photo had no sidecar. Never overwrite or delete inbox files.
* Content: a complete XMP sidecar, well-formed XML, < 2 MB, containing `lc:settings` or `crs:` data.
* The PC processes oldest stamp first. Unknown ids, bad names and invalid files are moved to `inbox-done` with a
  `<name>.rejected` note.

### Conflict policy
1. `base` equals the current PC `sidecarVersion` (or `none` and the PC has no sidecar): **apply**.
2. Content identical to the current sidecar: nothing to do.
3. Otherwise the PC sidecar changed since the phone's version. If the PC still has the sidecar version `base` names, it
   **merges** the two edits three ways, setting by setting, so a phone edit and a PC edit of different things both
   survive. The PC keeps every sidecar version it has published and every inbox file it has received, so `base` may
   also be the hash of the phone's own previous edit. The merge works on the `rdf:Description` properties of the three
   documents (base, PC, phone), each compared by namespace and name, written as attribute or child element alike:
   * the same on both sides, or changed only by the phone: the phone's value; changed only on the PC: the PC's value
   * changed differently on both sides: a conflict, and the **newer side wins** that one property (inbox UTC stamp vs.
     PC sidecar mtime; ties go to the PC). Conflicts are logged. A property that is absent counts as a value, so a
     deletion on one side is kept unless the other side changed the property too.
   * `xmp:MetadataDate`, `xmp:ModifyDate` and `xmp:CreatorTool` come from the newer side and are never conflicts.
   * `lc:settings` (JSON) is merged the same way leaf by leaf through nested objects. Lists (masks, spots, ...) are one
     value, and so are the sections `crop`, `geometry`, `curve` and `profile`: if both sides changed anything inside one
     of them, the newer side's whole section wins.
   The merged file is the PC's document with the changed properties spliced in: foreign properties (darktable,
   Lightroom `crs:`, ...), namespace prefixes, the `xpacket` wrapper and the layout stay as they were. The replaced PC
   sidecar and the phone's edit both go to the PC's local history.
4. When there is nothing to merge against or the merge is not safe, **newest wins** for the whole file, comparing the
   inbox UTC stamp with the PC sidecar's mtime; ties go to the PC. The loser is never lost: a replaced PC sidecar is
   copied to the PC's local history first; a losing phone edit is saved there as `<path>.<device>.<stamp>.xmp`. This
   applies when: `base` is `none` while the PC has a sidecar, or is a version the PC does not have (never published,
   or older than its history limit); the engine `version` or `process` in `lc:settings` differs between PC and phone;
   a document or its JSON does not parse (or is not UTF-8, has a DTD, or lists a property twice); or `base` has no
   `lc:settings` while a side has one.
5. Applying (or merging) writes the sidecar atomically (temp file in the same folder + rename). Photo files are never
   modified. The user then runs Synchronize Folder in LightCraft to pick up the new sidecar.

## Rules
* PC is master. `originals/` and `mobile/` are written only by the publisher.
* Clients must ignore unknown fields (and unknown files). A breaking change bumps `format`; clients must refuse
  a `format` higher than they know.
