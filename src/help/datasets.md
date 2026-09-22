# Datasets

`ark data list` is a compact inventory of slots and their state, and
`ark data show SLOT` adds the detail it leaves out. `ark data paths` maps the
data an app can read. None of them transfers dataset bytes. Numeric slot IDs
remain usable when a new kind has no name in this CLI build.

Check `ark status` before a data command. It needs a paired, unlocked Ark;
use --unlock only if status reports locked, and the owner will approve on
the phone. A dry run never unlocks and conflicts with --unlock.

## Slot fields

JSON list entries contain slot, id, name, description, format, state, origin,
damage, requires, size_bytes, build, version and download. show adds required_by
and cached. The reading list keeps to short columns, while show also prints the
full description and format. Both show dependency state alongside each name.

- description explains the slot's data to its owner. format tells whoever fills
  the slot which file it accepts, the shape that file needs, what the Ark
  refuses and whether the owner approves the upload.
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
  and sha256; the CLI never builds a URL itself. It says nothing about what a
  slot already holds, which is what state reports.
- cached refers only to this computer's download cache, never to the Ark. yes
  means a file named by the advertised SHA-256 is already here, which fetch
  still verifies as it replays. Personal slots always print no, since personal
  uploads are never cached.

## Data paths

`ark data paths` prints every path pattern an app can read as an indented
tree, with each entry under its parent. A trailing / marks a directory, + one a
manifest may grant, and ! data this Ark lacks, which covers everything beneath
the marked entry. Placeholders such as <gene> stand for values an app fills in.
The column to the right lists examples, sample values for a placeholder and
sample contents for a file.

--json returns the same entries in order, each with path, directory, grantable,
available, description, format and examples. path is complete, v1/ included,
and is what a manifest names. description says what the path holds, when it is
absent and when reading it fails. format gives a file's exact contents or what a
directory lists, and examples lists sample values, most typical first.

available means the slots a pattern needs are filled, not that every gene,
position or genotype has an answer. The map never carries a value from the
owner's data. When anything is unavailable, a hint points at `ark data list`.
`ark help apps` covers manifest grants and the Ark's checks before an app runs.

## Transfers and changes

`ark data upload FILE` asks the Ark to identify the first bytes, then uploads
and waits for validation and indexing. Compressed files stay compressed.
--slot asserts the expected identification. --dry-run stops after identification
and reads the slot state without changing it.

`ark data fetch SLOT` installs the reference download advertised by the Ark.
--all fills empty reference slots in dependency order; filled slots are skipped.
Each item reports done, skipped, failed, or not-attempted. A dry run reports
planned or skipped. Download URLs are never guessed. On a fresh Ark the
reference slots can require snp-indel-calls first: the owner must upload
personal calls before --all can fill those references. --all does not supply
personal data.

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
