# Apps

An app is one WASI preview 1 WebAssembly file. With no arguments it prints a
TOML manifest; when the Ark runs it with a data directory it reads its declared
paths and prints a report. The Ark owns manifest validation, sandboxing and
permission enforcement. Use a hardware Ark or emulator; the CLI has no local
execution sandbox.

`ark app run FILE` uploads, asks for owner approval and waits for the result.
The task ID is printed as soon as allocated. Ctrl-C or SIGTERM attempts to
cancel it; `ark app cancel TASK` can clean up a run whose CLI process died.
Results arrive whole. Redirect stdout to preserve the exact report bytes.

The manifest contains four fields, all under `[package]`:

```toml
[package]
name = "cilantro"
version = "0.1.0"
datasets = ["v1/genome/rsids/rs72921001"]
develop = false
```

Name, version and datasets are required; the dataset list may be empty and
`develop` defaults to false. Dataset paths are relative to the data root; the
Ark mounts only those paths, read-only. Absolute paths, traversal with `..`
and the entire root are refused.

The manifest pass has about 16 MiB of memory, a 250 ms limit and 1 KiB for
each output stream. The run pass has 100 MiB of memory and 1 MiB per output
stream; its duration is unbounded but cancellable. Neither pass has a network
or writable storage. Stdin is closed, random bytes are zero and clocks are
counters, so apps must not depend on wall-clock time or randomness.

By default only a successful run's stdout is returned. `develop = true`
returns stdout on failure and stderr too; leave it out of shipped apps.

See https://github.com/dark-bio/examples for working apps and the data tree.

A finished result is retained by the Ark for 60 seconds. There is no detached
mode or later result retrieval after the CLI consumed it. Background the whole
command to keep its connection and companion relay alive.
