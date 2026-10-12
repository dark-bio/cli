# Apps

An app is one WebAssembly module for WASI preview 1, and the Ark runs it
twice. In the manifest pass it runs without arguments and prints its
manifest, which names the app and lists what it reads and asks. In the
report pass it gets the data directory as its first argument, reads its
grants as files and prints a report. Apps run on an Ark, hardware or
emulated, since ark has no app runtime of its own. Worked apps in Rust, Go,
C and Python, with fixtures that run them on this computer, are at
https://github.com/dark-bio/examples.

## Running an app

`ark app run FILE` uploads the app, then follows its task to the end:

1. The Ark checks the module, runs the manifest pass, and checks the
   manifest and every grant. It refuses the app at the first check that
   fails, naming the rule, before the owner is asked.
2. The owner approves the run on their phone within 5 minutes. They see the
   app's name and version, whether it is a develop build, and every grant of
   their data in the Ark's own words. They switch on the optional grants
   they choose and answer the app's questions.
3. The report pass runs, with the granted data and the answers mounted
   read-only. It has no time limit.
4. The owner reviews the report on their phone within 10 minutes, then
   releases it to this command or declines, which drops it from the Ark.

Under -v, ark shows the task id once the Ark assigns it. At steps 2 and 4 it
passes on an approval. Only the connection that started the task receives
its report. Ending that connection, by stopping ark or losing the Ark,
cancels a task still running and drops its report.

Without --json the report goes to stdout exactly as the app printed it, so
`ark app run app.wasm > report.md` keeps it. With --json the document holds:

- task, the task id
- app, with name, version and develop from the manifest
- success, whether the app exited 0 with its output within the limits and
  the report rule, as the sandbox section describes
- paths, the grants of the owner's data the Ark mounted, in the order of the
  approval. Optional grants appear only when the owner switched them on, and
  grants of public data never do.
- media, the report's media type, text/markdown
- stdout and stderr, or stdout_base64 and stderr_base64 when a stream is not
  UTF-8
- duration_seconds, how long the app ran, measured by this computer

A released run of a successful app exits 0. A failed app exits 8 with
app-failed once released, and its review carries only the failure unless
the app is a develop build. A report the owner declines fails with
approval-denied, and one left unanswered with approval-timeout, both exit 6.
Either way the Ark drops the report, so no later command can fetch it.
A refused app fails with error[ark], whose message names the reason.

## The manifest

The manifest is a TOML 1.1 document:

    manifest = 1

    [app]
    name = "Cilantro Taste Test"
    version = "0.4.0"

    [reads]
    paths = ["v1/genome/rsids/rs72921001"]
    optional = ["v1/genome/rsids/rs713598"]

    [inputs.detail]
    type = "choice"
    prompt = "How much detail should the report give?"
    choices = ["Short", "Full"]

The Ark reads every table but `[listing]` strictly, and refuses a table, a
key or a manifest number it does not know.

- `manifest = 1` is the format's version.
- `[app]` is required. name is what the owner sees, display text of at most
  64 characters. version is a Semantic Versioning 2.0.0 version of at most
  32 characters, such as 0.4.0 or 1.0.0-beta.1, without a leading v or
  build metadata.
- `develop = true` under `[app]` adds the app's stderr to the owner's review,
  and its stdout after a failure. The owner sees a warning when approving a
  develop build, so leave it out of apps you ship.
- `[reads]` lists the grants in paths and optional, as described under
  Grants. An app that reads nothing leaves it out.
- `[inputs.NAME]` asks the owner one question, as described under
  Questions.
- `[output]` holds report, the report's media type. text/markdown is the
  default and the only type the Ark accepts.
- `[listing]` holds what Ark Hub shows for the app, and the Ark skips it.

Display text, such as a name, a prompt or a choice, holds at least one
character, and no control characters, line or paragraph separators or text
direction controls. Emoji are fine. The whole manifest has to fit in the
64 KiB the manifest pass may print.

## Grants

A path in paths or optional grants the app that directory and everything
beneath it, read-only. Spell it as `ark data paths` shows it, with v1/ kept
and every placeholder filled in, such as v1/genome/genes/TAS2R38. Paths are
relative, without empty, . or .. segments or a trailing /. Files, changes
directories, the data root and v1/ itself cannot be granted. A manifest
grants at most 1,024 paths across both lists, each once, and none inside
another's directory.

The Ark checks every grant before the owner is asked, and refuses a
misspelled or ungrantable path. It also refuses a path in paths whose data
is missing, such as an empty slot, an unknown gene or rsID, or a position
past the end of its chromosome. The refusal names the missing data.

A path in optional is one the owner may decline. Its switch starts off on
the phone, and when its data is missing the phone shows it as unavailable.
A declined grant and a missing one are both left unmounted, so the app reads
either as absent, and cannot distinguish the two. Public data, which is the
same on every Ark, cannot be optional. The Ark mounts public grants without
showing them to the owner.

A gene or region grant includes its changes, the owner's variants inside
it, even when the app reads only the reference sequence.

## Questions

Each `[inputs.NAME]` table asks the owner one question, which they answer on
the phone while approving the run. ark never sees the answers.

- type is choice or text.
- prompt is display text of at most 80 characters.
- choices lists 1 to 32 distinct answers for a choice, each display text of
  at most 64 characters. `multiple = true` lets the owner pick any number of
  them.
- max is the longest text answer accepted, from 1 to 256 characters. A text
  answer is display text, so it holds at least one character.
- default is what the phone fills in first. It is one of the choices, a list
  of them in the order of choices for a multiple choice, or display text
  within max. Without it, a choice starts unpicked and a text empty.

NAME is 1 to 32 lowercase ASCII letters, digits, hyphens and underscores,
starting with a letter, and a manifest asks at most 16 questions. The Ark
checks every answer against its question. The answers reach the app as files
under inputs/ in its data directory, one per question and named after it, in
UTF-8 without a trailing newline. A choice's file holds the pick, a multiple
choice's file the picks one per line in the order of choices, and a text's
file the text.

## The data an app reads

`ark data paths` maps every path an app can read on this Ark, as a tree. A
trailing / marks a directory, + a path a manifest may grant, and ! data the
Ark lacks, for that entry and everything beneath it. A placeholder such as
<gene> stands for a value the app fills in, and the column on the right
shows examples. With --json each entry, in the same order, carries:

- path, complete and spelled as a manifest names it
- directory and grantable
- available, whether the slots the path needs are filled, which does not
  promise an answer for every gene or position
- description, what the path holds, when it is absent and when reading it
  fails
- format, a file's exact contents or what a directory lists
- examples, sample values, the most typical first
- public, whether a grant of it reads only public data
- wording, how the phone words a grant of it to the owner, with its
  placeholders unfilled

The map never carries a value from the owner's data.

## The sandbox

- The manifest pass gets 32 MiB of memory, 250ms and 64 KiB of output, and
  its stderr is discarded. It must exit 0.
- The report pass gets 128 MiB of memory and 1 MiB per output stream, with
  no time limit. A stream that passes 1 MiB is cut there, and the run fails.
- A report is UTF-8 text without control characters other than line feed
  and tab, line or paragraph separators, or text direction controls. A
  stream that breaks this rule is dropped, and the run fails. A develop
  build's stderr is held to the same rule.
- Neither pass has a network or writable storage. Stdin is closed, random
  bytes are zero and both clocks count reads, so an app cannot depend on
  time or randomness.
- A module is at most 256 MiB. The Ark refuses a component, a module with a
  start section, more than one memory or table, imports beyond WASI preview
  1, or no `_start` function without parameters or results.
