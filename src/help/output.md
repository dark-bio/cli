# Output contract

stdout carries the result. stderr carries progress, notes, warnings, approval
instructions, hints and errors. auto chooses human on a terminal and text in a
pipe, independently for each stream. human keeps tables, status marks, local
timestamps and scaled units; colour and live progress require a terminal.
NO_COLOR disables colour. Text is for agents reading the values; JSON is for
programmatic parsing.

Text is a lossless projection of the JSON result, using the same field names and
values. Nested fields use dotted paths; object arrays add zero-based indices,
for example slots.0.size_bytes. Scalar arrays use JSON brackets and quotes;
empty arrays and objects are [] and {}. Null is -, booleans are yes/no, and
_bytes and _seconds values are raw integers. Multiline strings continue on lines
indented by two spaces, preserving their line breaks. Text never truncates
fields, scales numbers or converts timestamps. Text and JSON field names share
the same promise: additions are allowed, renames require a major version.

Human tables may omit fields and add status marks, such as "ok filled" or a
check mark before filled; the mark is decoration, not part of the state value.
Byte columns use one shared unit so sizes can be compared down the column.

JSON prints one bare document on stdout and JSON lines with an event field on
stderr. Keys are snake_case, absent values null, enums strings, times ISO 8601
UTC, byte counts suffixed _bytes and durations _seconds. Task IDs are decimal
strings so every u64 is exact. Scripts should pin the tool version. help and
completions print text in every format.

App reports and the dataset README are payloads rather than fields. Human and
text write the exact bytes to stdout, with app stderr announced and written
verbatim, so plain redirection keeps a raw report. JSON carries both streams as
stdout and stderr strings, or stdout_base64 and stderr_base64 when the bytes are
not UTF-8. Failed apps return output only
with `develop = true`; the CLI preserves whatever the Ark returns.

A command that did partial work keeps that result, reports the failure on stderr
and exits nonzero, and never replaces the result with an error document. With no
result at all, JSON prints {"error":{...}} on stdout beside the error event on
stderr, and text leaves stdout empty.

-q drops optional diagnostics, but retains errors, hints, owner approval
instructions and app output. -v shows steps; -vv connect debug, -vvv wire trace.
Credentials from package login never appear in output or logs.

Human progress refreshes once a second, showing new steps and completion at
once; text and JSON report at ten-percent boundaries or every five seconds. App
elapsed time refreshes every second in human mode and every five otherwise.
JSON progress events have the same envelope as notes and approvals:

    {"event":"progress","message":"uploading: 1048576/2097152 bytes (50%)"}

The event names are progress, note, warning, approve, hint, step, log and error.
Non-error events carry message; error events carry an error object. Progress
messages describe transfers, processing phases or elapsed time; their wording
is for reading, not a structured progress API. -v enables step events.

## Error codes

The code in error[code]: is stable and its exit code is the class below. hint:
lines name the next step where the tool knows one. In JSON the error event
carries code, message and, for the Ark's own verdicts, remote code and message.

Exit 1, local input or confirmation:
- `file-not-found`, `file-unreadable`, `file-empty`: the named path
- `file-rejected`: the Ark or the tool refused the file's content
- `invalid-slot`: --slot differs from what the Ark identified, or the Ark has
  no such slot
- `invalid-key`, `invalid-version`: a malformed --pubkey, or a --version not
  published for the environment
- `confirmation-required`: firmware installation needs confirmation; use
  --yes when running noninteractively
- `enrollment-required`: online enrollment happens at the Ark Hub
- `io`: a local read or write failed

Exit 2, `usage`: invalid arguments or an unknown help topic. A zero, negative or
malformed slot is a usage error; a positive ID the Ark does not have fails later
as invalid-slot at exit 1.

Exit 3, device access:
- `no-device`: no Ark found; run `ark devices`
- `ambiguous-device`: several match; the hint lists locators for --device
- `device-busy`: another ark process or an Ark Hub browser tab holds the USB
  session; wait for your other command to finish, or close the browser tab
- `device-unreachable`, `disconnected`: the connection failed or dropped.
  Linux USB permission errors include a udev-rule hint.
- `handshake-failed`: the attestation or the identity did not verify

Exit 4, cloud:
- `cloud-unreachable`: an HTTP, package host or relay request failed
- `environment-unknown`: no cloud environment; select one with --env
- `login-required`: the cloud or package host wants a browser login; the hint
  has the cloudflared command
- `proof-rejected`: the cloud refused the device proof; run `ark doctor`
- `pairing-failed`: the rendezvous or the companion side failed
- `registry-inactive`: the registration is disabled, expired or superseded

Exit 5, Ark state or refusal:
- `not-paired`: run `ark pair`
- `locked`: run `ark unlock`, or add --unlock; a dry run needs it separately
- `already-paired`, `already-enrolled`: the Ark is in that state already
- `firmware-outdated`: this tool needs newer firmware; update it with
  `ark firmware update`, or update the emulator app
- `update-unverified`: the Ark returned running a different build
- `dependency-missing`, `no-download`: a reference slot lacks a dependency or
  advertises no download
- `ark`: the Ark's own verdict with its number; read the message, never match
  it
- `unsupported`, `unknown`, `unavailable`, `unanswered`: reserved refusals;
  `unknown` means the firmware and this tool disagree, update both

Exit 6, approval:
- `approval-denied`: the owner declined on the phone or the button
- `approval-timeout`: nobody answered in time; a pairing hints `ark pair`

Exit 7, `timeout`: a machine wait exceeded --timeout, or the Ark did not
return from a reboot within 120 seconds.

Exit 8, `app-failed`: the app reported failure. Any output returned by the
Ark is still printed.

Exit 130 `interrupted` and 143 `terminated`: Ctrl-C or SIGTERM, after a
best-effort cancel of the active task or upload.
