# Apps

An app is one WASI preview 1 WebAssembly file. With no arguments it prints a
TOML manifest; when the Ark runs it with a data directory it reads its granted
paths and prints a report. The Ark owns manifest validation, sandboxing and
permission enforcement, and the CLI has no local execution sandbox. Apps run on
a hardware Ark or on one emulated by Ark Emulator, from
https://github.com/dark-bio/emulator.

Worked apps in Rust, Go, C and Python, with fixtures that run them on this
computer, are at https://github.com/dark-bio/examples.

## Running and collecting results

`ark app run FILE` uploads the app, asks the owner to approve the task and
waits for the app to end. The owner then reviews the report in Ark Companion,
and releases it to this command or keeps it. An app that fails goes to review
as well, so even a failure arrives only with a release. The task ID is printed
as soon as allocated. Ctrl-C or SIGTERM attempts to cancel the task.

Results arrive whole, once released. `ark app run FILE > report.md` preserves
the exact report bytes. --json includes task, app with name, version and
develop, success, paths, media, stdout, stderr and duration_seconds; non-UTF-8
streams use base64 fields as described in `ark help output`. duration_seconds
counts how long the app ran, not the owner's review. A report the owner keeps,
or leaves unanswered, fails the command with an approval error.

Only the connection that started the task receives its report. Ending that
connection cancels a task still running and drops its report. There is no
detached mode or later retrieval, so background the whole command to keep its
connection and companion relay alive.

## Manifest

The manifest opens with the schema number, then names the app and lists what
it reads:

```toml
manifest = 1

[app]
name = "Cilantro Taste Test"
version = "0.4.0"

[reads]
paths = ["v1/genome/rsids/rs72921001"]
```

`manifest` is always 1, and `[app]` is required. The name holds at most 64
characters, without control characters, line separators or text direction
controls. The version is a Semantic Versioning 2.0.0 version of at most 32
characters, such as 0.4.0 or 1.0.0-beta.1, without build metadata or a leading
v. `[reads]` may be left out of an app that reads nothing. A `[listing]` table
holds what Ark Hub shows, and the Ark skips it. The Ark refuses any other table
or key, and a module with a WebAssembly start section, since an app begins at
`_start`.

By default the owner reviews only stdout, and only when the app succeeds.
`develop = true` under `[app]` adds stderr, and stdout after a failure, to the
review; leave it out of shipped apps.

## Data grants

Each entry in `paths` grants one directory and everything beneath it.
`ark data paths` shows which directories a manifest may grant and which data
this Ark lacks. Its --json entries describe every path, the values each
placeholder accepts, each file's exact contents and examples, whether a grant
reads only public data, and how the owner sees a grant.

Spell a grant as the paths map does, with v1/ kept and every placeholder
replaced, such as v1/genome/genes/BRCA1. Paths are relative, and empty, . and
.. segments or a trailing / are refused. Files, changes directories, the data
root and v1/ itself cannot be granted. A manifest grants at most 1,024 paths,
each once, and none inside another's directory.

The Ark checks every grant before asking the owner. It refuses a misspelled or
ungrantable path, and one whose data is missing, such as an empty slot, an
unknown gene or rsID, or a position past the end of its chromosome. The owner
sees every grant of their own data; public data, which is the same on every
Ark, is mounted without asking. After approval the Ark mounts the grants
read-only and passes the data directory as the app's first argument, with the
manifest paths beneath it.

## Sandbox

The manifest pass has 32 MiB of memory, a 250 ms limit and 64 KiB for its
output, and its stderr is discarded. The report pass has 128 MiB of memory and
1 MiB per output stream; its duration is unbounded but cancellable. A report is
UTF-8 text without control characters other than line feed and tab, and an app
whose output breaks that fails. Neither pass has a network or writable storage.
Stdin is closed, random bytes are zero and clocks are counters, so apps must
not depend on wall-clock time or randomness.
