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
  imports/<device>/<id>/<file name>           a photo another PC uploads to be added to the PC's photo folder
  imports/<device>/<id>/<sidecar name>        its XMP sidecar, optional
  imports/<device>/<id>.json                  the commit note, written LAST; a file with no note is an upload in progress
  imports-done/<UTC date>/<device>/...        processed imports (moved, not deleted), with <id>.result.json
  history/<UTC date>/...           rclone --backup-dir for anything replaced/deleted under originals/ or mobile/
```
Backup-dir files get a `.HHMMSS` suffix before the extension (`photo.153012.jpg`).

**Encrypted remotes.** `<remote>` may be an rclone `crypt` remote (`filename_encryption = standard`,
`directory_name_encryption = true`). The layout, names and files in this document are what rclone shows *through* the crypt
remote: a client must open the crypt remote (e.g. `lccrypt:`), never the underlying folder, which holds only encrypted names
and contents. Nothing in the format changes. Names are longer underneath (a 12-byte name is 26 characters, a 143-byte one is
231); see "Encrypting the remote" in the README for name and path limits.

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
     value, and so are the sections `crop`, `geometry`, `curve`, `profile` and `wb`: if both sides changed anything inside one
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

## Imports (other PC to home PC)
A second PC can send photos to the home PC, which adds them to its photo folder (and publishes them with the next sync).
The format stays 1: these are new files in new folders, which a client that does not know about them ignores.

* The uploader puts the files first: `imports/<device>/<id>/<file name>` (the photo's bytes) and, optionally,
  `imports/<device>/<id>/<sidecar name>`. `device` follows the inbox's device-name rule; `id` is the LightCraft content hash
  (32 lower-case hex) of the photo's bytes. **Last** it writes the commit note `imports/<device>/<id>.json`. A file with no
  note is an upload still in progress and is ignored; the home PC never reads, moves or deletes it.
* The note: `{"format":1,"id":"<32 hex>","folder":"261101 - Lisbon","name":"DSC0001.ARW","size":N,"mtime":"<UTC ISO>",
  "sidecar":"DSC0001.ARW.xmp" or null}`. `folder` is the photo's folder relative to the uploader's source root, POSIX
  separators, `""` for the root itself. `name` is the file name. `size` is in bytes; `mtime` is UTC `YYYY-MM-DDTHH:MM:SSZ`
  (fractional seconds allowed). Unknown fields are ignored.
* The home PC processes notes in the same sync that applies the inbox, after the inbox and before it scans the folder.
  It **rejects** a note (status `rejected`, with the reason) when: `format` is not 1; `id` is not 32 lower-case hex or is not
  the id in the note's file name; `folder`, `name` or `sidecar` is absolute, or has a `..` or `.` component, a backslash, a
  NUL or other control character, an empty component (`a//b`, a trailing `/`), a component over 255 bytes, a component that
  ends in a dot or a space, a Windows reserved name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with or
  without an extension) or one of `< > : " | ? *`, or a component that starts with `.` (hidden files are never published);
  `name` is not a photo file type (the extensions the publisher scans) or `sidecar` is not `<stem>.xmp` or `<name>.xmp`; the
  folder matches the home PC's `exclude` patterns; the sidecar is not an acceptable XMP (the inbox's rule); the folder, once
  symbolic links in the home PC's photo folder are resolved, would be outside it (or is hidden or excluded); or the
  downloaded bytes are not `size` bytes with content hash `id`. Nothing is written for a rejected note.
* A note whose files are not (all) there yet is left for the next sync. If they are still missing 48 hours after the home PC
  first saw the note it is rejected. A network or disk problem (including too little free disk space: the file's size plus
  a safety margin, 256 MiB by default, must be free) never rejects; the import waits and the problem is shown.
* A photo whose `id` the home PC already has (in its published state), or whose bytes already sit at the target, is a
  `duplicate`; nothing is written. A target name that is taken by a file with other bytes is never overwritten: the photo
  is written as `<stem>-2<ext>`, `-3`, ... (case-insensitive), and its sidecar is named to match (`<stem>-2.xmp` or
  `<stem>-2.ext.xmp`). An existing sidecar is never replaced; if one is in the way of a photo with its original name the
  photo is still added and the result says the sidecar was kept.
* The home PC writes a sidecar before its photo (so a scan never sees the photo without it) and each file as a temporary
  file in the same folder, `fsync`, then a link that fails if the name exists, so a race cannot overwrite anything. The
  photo's mtime is set from the note (never into the future). Only after the photo is on disk does it file the import away,
  so a crash at any point is repaired by the next sync (identical bytes at the target are a duplicate, not a second copy).
* Filing away: first `imports-done/<UTC date>/<device>/<id>.result.json`, then the files move to
  `imports-done/<UTC date>/<device>/<id>/`, and the note to `imports-done/<UTC date>/<device>/<id>.json` last. Files are
  moved, not deleted. The date is the day of processing.
* The result: `{"id":"<32 hex>","status":"added"|"duplicate"|"rejected","path":"<path relative to originals/>" or null,
  "reason": null or text}`. `path` is the photo's final path on the home PC, which differs from `folder/name` when it was
  renamed (or when a link inside the photo folder was followed); for `duplicate` it is where the photo already is. The
  uploader reads it to learn the final path.
* The weekly purge removes `imports-done/` day folders like `inbox-done/` (same age limit, same backup check). `imports/`
  itself is never purged.

## A client that is a PC (another PC)
A second PC keeps a full copy of some folders of the library in a photo folder of its own, with its own LightCraft library, and sends
its edits back. The home PC stays the master. This adds no files or fields to the format (it stays 1): the other PC reads what the
phone reads and writes what the phone writes, plus the imports above. **It never writes `originals/` or `mobile/`**, and it never
reads another device's inbox.

* **What it keeps.** The user picks folders from the manifest (the folders `path` values sit in). A chosen folder means the photos
  directly in it, not its subfolders. For each photo of a chosen folder (alternates included, each by its own `id`) the PC downloads
  `originals/<path>` and its sidecar to `<its photo folder>/<path>`. A download is written next to its target as a temporary file,
  accepted only when its content hash equals `id` (and its size `size`), then linked into place by a call that cannot replace an
  existing file, with `mtime` set from the manifest (never into the future). The sidecar goes in before the photo. A different file
  already at the target is never replaced. A manifest path that is absolute, has a `..` or `.` component, an empty component or a
  backslash is not written. A pass that is cut off is finished by the next; what arrived is kept.
* **Edits go out.** A local sidecar whose bytes differ from the ones last downloaded or sent, and whose `sidecarVersion` is not the
  manifest's, is an edit. The PC sends it to `inbox/<device>/<id>-<UTCstamp>-<base>.xmp` like a phone (never overwriting, stamps
  strictly increasing per photo), with `base` the version it started from: the manifest's `sidecarVersion` when it last downloaded
  or saw the home PC's sidecar, else the version of its own previous edit (the home PC keeps every inbox file it received), else
  `none`. An edit is sent once. `device` follows the inbox's name rule, and must not be a phone's name.
* **The home PC's version comes back.** When the manifest's `sidecarVersion` changes and the local sidecar has no unsent edit, the PC
  replaces the local sidecar with the home PC's (temporary file, then rename), keeping the old one in its own history. When both
  changed, the PC sends its edit first and **leaves its local sidecar alone**: the home PC merges (conflict policy above), the
  merged result becomes the manifest's version, and a later pass takes it. Until the manifest names a version other than the one it
  had when the edit was sent, the PC treats its edit as waiting and does not replace anything. If the home PC kept its own version
  instead of applying the edit, the PC keeps its local edit until the home PC's sidecar changes again. A local edit is never
  discarded: a replaced sidecar is always kept in the PC's history first.
* **Deletions.** Only the home PC's deletions spread: a photo whose `id` is no longer in the manifest, in a chosen folder, is moved
  (with its sidecar) to the PC's own `removed/<date>/` folder, never deleted, and only if its bytes still hash to `id`. More
  removals in one pass than the PC's `max_delete` are refused as a whole (an emptied or damaged manifest looks like that).
  Deleting a photo on the PC affects only that PC: the PC remembers the photo as left out and does not download it again, however the
  home PC changes it. It comes back when the user un-chooses the photo's folder and chooses it again (un-choosing forgets what the PC
  remembers about the folder's photos and leaves the files where they are; choosing again downloads what is missing). When the home
  PC deletes the photo too, the memory goes with it.
* **Disk space.** Before a download the PC checks that the file's size plus a safety margin (256 MiB by default) is free, and stops
  with a clear message when it is not.

## Rules
* PC is master. `originals/` and `mobile/` are written only by the publisher (the home PC).
* Clients must ignore unknown fields (and unknown files). A breaking change bumps `format`; clients must refuse
  a `format` higher than they know.
