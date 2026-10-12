# Output

stdout carries only the result. stderr carries everything read along the
way: progress, notes, warnings, approvals, hints, steps, logs and errors.
Keep the two apart when parsing.

## Reading output

The default output is formatted for reading, the same on a terminal and in a
pipe. One item prints as a block of labels and values, several as a table.
Each size carries its own unit in the value. Absent values print as -, empty
lists as none and booleans as yes and no. Timestamps are in local time, a
date alone in a table and the full time in a block. A mark before a state is
decoration, and the word beside it is the value. The view may shorten, round
or leave out long fields, while --json always has the whole value. Colors
and live progress need a terminal, and NO_COLOR or CLICOLOR=0 turns colors
off.

`ark doctor` prints its checks in two groups. The first is this computer,
and the second takes the Ark's name, or Ark when no connection could be
made. A check's name is its JSON name, and its mark shows ok, warn, fail or
skip. Skips with one cause share a row, and a summary counts failures as
problems and warns as warnings. Under --json the document holds version and
checks, every check on its own with name, result, detail and hint. A failed
doctor ends with its error and hints, even when a check already showed them.

## JSON

--json prints one result document on stdout, indented by two spaces, and
one JSON event per line on stderr. Keys are snake_case. Absent values are
null, enums are strings, times are ISO 8601 in UTC, byte counts end in
_bytes and durations in _seconds. Task ids and the Ark's error numbers are
decimal strings, so they stay exact. New fields may appear,
and a renamed field means a new major version, so scripts pin the version
`ark --version --json` prints. Help and completions print text even with
--json.

Every stderr line under --json is one event, however long its message:

    {"event":"approve","message":"Approve the unlock on your phone, in Ark Companion","action":"unlock","where":"phone"}

The kinds are progress, note, warning, approve, hint, step, log and error.
A progress event carries stage, phase, step, steps, percent, done_bytes,
total_bytes and remaining_seconds, each present and null when unknown. Its
stage names ark's own work, such as uploading, while phase, step and steps
carry the Ark's processing, the phase spelled as the Ark names it. An
approve event carries action, such as unlock or upload, and where, phone or
button, or null when the Ark did not say. A log event carries level, target
and fields, and an error event the whole error object. A caller that merged
the streams drops the lines starting with `{"event":` and keeps the document.

## Events and verbosity

A stage in progress shows behind ›, and ends as a line behind ✓ with its
time or a fact, or behind ✗ when it fails. An approval shows its instruction
behind !, starting with Approve or Review. On a terminal its timer counts up
on that line, and a pipe prints the instruction once, then its outcome. Only
a deadline ark sets itself counts down. Durations read 14s, 1m 02s or
1h 05m. Diagnostics keep their labels, error[code]:, warning:, hint:, note:
and log:, and the steps -v adds indent two spaces.

On a terminal, progress redraws in place once a second. Redirected, it
prints a line at every ten percent or every 5s. -q hides progress, notes,
warnings and steps, and keeps errors, hints, approvals, logs and app output.
-v adds steps. --log debug adds the diagnostics of ark's release lookup and
its connection library, and --log trace adds the wire protocol too. Logs
never carry credentials or authorization headers.

## App output

Without --json, `ark run` writes the released report to stdout exactly
as the app printed it. The app's stderr, which only a develop build
releases, follows on stderr after a note giving its size. With --json both
streams are fields of the document, stdout and stderr when they are UTF-8,
stdout_base64 and stderr_base64 otherwise. Either way, app output arrives
only once the owner releases it.

## Failures

A failure prints error[code]: message on stderr, then hint: lines naming the
next step where ark knows one. Under --json the error event carries the
error object. It holds code, message and hints, and for a refusal by the Ark
also remote, with the Ark's own code and message. Under --json an error's
hints live only in that object, and a hint that belongs to no error stays an
event of its own.

A command that did part of its work keeps that result on stdout, reports
the failure on stderr and exits non-zero. A command with no result prints
nothing on stdout, or under --json a document holding only the error:

    {"error": {"code": "no-device", "message": "no Ark enclave found", "hints": []}}

`ark help errors` lists every code with its next step, and says when
running a command again can help.

## New releases

At most once an hour, ark asks GitHub for its newest release. A release
build reads the redirect at https://github.com/dark-bio/cli/releases/latest,
and a development build reads the release list from api.github.com. The
request carries nothing about this computer, its Arks or the running
version. It runs in a detached copy of ark that exits within 30s, so no
command waits for it. The answer is kept in update.json in ark's cache
directory, and a nonempty CI turns the lookup off. A local build, marked
with -local in its version, skips this lookup and its note.

While the kept answer names a newer version, every command except help,
completions, --version and doctor starts with a note naming both versions
and how to upgrade. Under --json it is an ordinary note event. The note
never changes the result or the exit code, and -q hides it. doctor looks up
afresh and reports its version and the answer together in the ark check.
The check is ok on the newest release, warn with the upgrade command when a
newer one is out, and skip when the lookup fails, under CI or in a local
build.
