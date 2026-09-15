# Apps

An app is one WASI preview 1 WebAssembly file. With no arguments it prints a
TOML manifest; when the Ark runs it with a data directory it reads its declared
paths and prints a report. The Ark owns manifest validation, sandboxing and
permission enforcement. Use a hardware Ark or emulator; the CLI has no local
execution sandbox.

## Running and collecting results

`ark app run FILE` uploads, asks for owner approval and waits for the result.
The task ID is printed as soon as allocated. Ctrl-C or SIGTERM attempts to
cancel it; `ark app cancel TASK` can clean up a run whose CLI process died.
Results arrive whole. `ark app run FILE > report.md` preserves the exact report
bytes. --json includes task, app, success, stdout, stderr and duration_seconds;
non-UTF-8 streams use base64 fields as described in `ark help output`.

A finished result is retained by the Ark for 60 seconds. There is no detached
mode or later result retrieval after the CLI consumed it. Background the whole
command to keep its connection and companion relay alive.

## Manifest

The manifest contains four fields, all under `[package]`:

```toml
[package]
name = "cilantro"
version = "0.1.0"
datasets = ["v1/genome/rsids/rs72921001"]
develop = false
```

Name, version and datasets are required; the dataset list may be empty and
`develop` defaults to false. The name holds at most 64 characters and the
version at most 32, and neither may be empty or contain control characters,
line separators or text direction controls. The module must not have a
WebAssembly start section, since a run begins at `_start`.

By default only a successful run's stdout is returned. `develop = true`
returns stdout on failure and stderr too; leave it out of shipped apps.

## Data grants

Each entry in datasets grants one directory and everything beneath it.
`ark data paths` shows which directories a manifest may grant and which data
this Ark lacks. Its --json entries describe every path, the values each
placeholder accepts, each file's exact contents and examples.

Spell a grant as the paths map does, with v1/ kept and every placeholder
replaced, such as v1/genome/genes/BRCA1. Paths are relative, and empty, . and
.. segments or a trailing / are refused. Files, changes directories, the data
root and v1/ itself cannot be granted.

The Ark checks every grant before asking the owner. It refuses a misspelled or
ungrantable path, and one whose data is missing, such as an empty slot, an
unknown gene or rsID, or a position past the end of its chromosome. After
approval the Ark mounts the grants read-only and passes the data directory as
the app's first argument, with the manifest paths beneath it.

## Sandbox

The manifest pass has about 16 MiB of memory, a 250 ms limit and 1 KiB for
each output stream. The run pass has 100 MiB of memory and 1 MiB per output
stream; its duration is unbounded but cancellable. Neither pass has a network
or writable storage. Stdin is closed, random bytes are zero and clocks are
counters, so apps must not depend on wall-clock time or randomness.
