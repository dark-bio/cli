# Datasets

`ark data list` is an inventory of slots, with short names and state.
`ark data show SLOT` adds the long description, required_by and cached.
Neither transfers dataset bytes. `ark data paths` prints the Ark's README
verbatim, including absent paths and the slots that would supply them.
With --json it returns readme. Numeric slot IDs remain usable when a new kind
has no name in this CLI build.

Check `ark status` before a data command. It needs a paired, unlocked Ark;
use --unlock only if status reports locked, and the owner will approve on
the phone. A dry run never unlocks and conflicts with --unlock.

## Slot fields

JSON list entries contain slot, id, name, state, origin, damage, requires,
size_bytes, build, version and download. show adds description, required_by
and cached. The reading view scales
size_bytes under Size and includes dependency state alongside each name.

- size_bytes is the bytes on the Ark's disk for this slot, zero when empty.
  It is not the original upload or download size; processing may change it.
- build is the reference assembly, for example GRCh38.p14. The Ark matches
  datasets by assembly family rather than exact patch, so GRCh38.p13 beside
  GRCh38.p14 is normal, and it refuses data that does not fit. version is the
  dataset's own release, for example dbSNP 157 for variant-catalog; it is not
  a firmware version.
- requires names dependency slots, whether already filled or still missing.
  The reading view marks each as filled or not filled. required_by names the
  slots that depend on this one. These relationships differ from a help page's
  Requires preconditions.
- download is what the Ark offers to fill an empty slot, with url, size_bytes
  and sha256; the CLI never builds a URL itself. Filled slots normally offer
  none, shown as - in the reading view or null in JSON, so read state to learn
  whether a slot holds data.
- cached refers only to this computer's download cache, never to the Ark. yes
  means a file named by the advertised SHA-256 is already here, which fetch
  still verifies as it replays. Personal slots always print no, since personal
  uploads are never cached.

## Transfers and changes

`ark data upload FILE` asks the Ark to identify the first bytes, then uploads
and waits for validation and indexing. Compressed files stay compressed.
--slot asserts the expected identification. --dry-run stops after identification
and reads the slot state without changing it.

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
