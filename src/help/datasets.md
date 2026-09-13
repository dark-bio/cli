# Datasets

`ark data list` reports slots by protocol name and state. `ark data show SLOT`
adds required_by and cached. Both include the Ark's descriptions, dependencies
and public download when advertised. Neither command transfers dataset bytes.
`ark data paths` prints the Ark's README verbatim, including absent paths and
the slots that would supply them. JSON returns it as readme.
Numeric slot IDs remain usable when a new kind has no name in this CLI build.

Slot fields describe the Ark's inventory:

- size_bytes is the bytes on the Ark's disk for this slot, zero when empty.
  It is not the original upload or download size; processing may change it.
- build is the reference assembly, for example GRCh38.p14. version is the
  dataset's own release, for example dbSNP 157 for variant-catalog; it is not
  a firmware version. Absent values are - in text.
- requires names slots that must be filled before this slot is actionable.
  required_by is the reverse direction: slots that depend on this one.
  These dataset dependencies differ from a help page's Requires preconditions.
- download is an optional offer with url, size_bytes and sha256. Filled slots
  normally advertise none, shown as - in text. Read state to learn whether a
  slot holds data.
- cached refers only to this computer's download cache, never to the Ark. yes
  means a file named by the advertised SHA-256 is already here, which fetch
  still verifies as it replays. Personal slots always print no, since personal
  uploads are never cached.

`ark data upload FILE` asks the Ark to identify the first bytes, then uploads
and waits for validation and indexing. Compressed files stay compressed.
--slot is an assertion about identification, not an override. --dry-run stops
after identification and reads the slot state without changing it.

`ark data fetch SLOT` installs the reference download advertised by the Ark.
--all fills empty reference slots in dependency order; filled slots are skipped.
Each item reports done, skipped, failed, or not-attempted. A dry run reports
planned or skipped. Download URLs are never guessed. On a fresh Ark the
reference slots can require snp-indel-calls first: the owner must upload personal
calls before --all can fill those references. --all does not supply personal data.

The CLI streams and caches public reference bytes at the same time. Files are
addressed by SHA-256; incomplete entries retain resumable prefixes. Transport
failures retry at most three attempts, opening a new Ark session and replaying
the prefix. A changed server response restarts from zero. Cache corruption falls
back to a fresh download, and cache write failure does not stop a transfer.
--cache selects a directory, --no-cache opts out, doctor reports its location.
Personal uploads and app reports are never copied into this cache.

Delete empties a filled slot; repair resets a damaged or half-written slot.
Both ask the owner when the Ark requires it and offer --dry-run. The dry run
shows dependent slots; the Ark decides whether the real operation is allowed.
