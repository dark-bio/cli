# Datasets

`ark data list` reports slots by protocol name and state. `ark data show SLOT`
includes the Ark's descriptions, dependencies and advertised public download.
`ark data paths` prints the Ark's README verbatim, including absent paths and
the slots that would supply them. JSON returns the text as `readme`.
Numeric slot IDs remain usable when a new kind has no name in this CLI build.

`ark data upload FILE` asks the Ark to identify the first bytes, then uploads
and waits for validation and indexing. Compressed files stay compressed.
--slot is an assertion about identification, not an override. --dry-run stops
after identification and reads the slot state without changing it.

`ark data fetch SLOT` installs the reference download advertised by the Ark.
--all fills empty reference slots in dependency order; filled slots are skipped.
Each item reports done, skipped, failed, or not-attempted. A dry run reports
planned or skipped. Download URLs are never guessed.

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
