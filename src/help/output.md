# Output

stdout carries the result. stderr carries progress, notes, warnings, approval
instructions, hints and errors. Keep the streams separate when parsing output.

## Reading and parsing

Default output is formatted for reading, including when redirected. It may use
tables, scale units, localize timestamps and add status marks. Labels describe
what is shown, so Size carries its unit in the value. Byte columns share one unit
so sizes can be compared down the column. A mark before a state value is decoration,
not part of the value. Absent values appear as -, empty lists as none and
booleans as yes/no.

The view may shorten or omit a long field; --json always has the complete value.
Lists are inventories, and data show adds the detail they leave out. See
`ark help datasets` and `ark help states` for field meanings. Each command's
Prints line follows the reading order and names the JSON fields where the two
differ.

--json prints one complete, exact result document on stdout, indented by two
spaces. Keys are snake_case, absent values null, enums strings, times ISO 8601
UTC, byte counts suffixed _bytes and durations _seconds. Task IDs and the Ark's
error numbers are decimal strings so every 64-bit value is exact. JSON field
additions are allowed; renames require a major version. Scripts should pin the
tool version. Reading layouts and labels may change. Help and completions always
print text.

Color and live progress require a terminal. NO_COLOR or CLICOLOR=0 disables
color; neither can force it on in a pipe.

## Payloads and failures

App reports pass through to stdout as exact bytes by default. App stderr is
announced and written verbatim to stderr. With --json, app streams become stdout
and stderr strings, or stdout_base64 and stderr_base64 when their bytes are not
UTF-8. Failed apps return output only with `develop = true`; the CLI preserves
whatever the Ark returns.

A command that did partial work keeps that result, reports failure on stderr
and exits nonzero. It never replaces an emitted result with an error document.
With no result at all, --json prints an error object on stdout beside the error
event on stderr; default output leaves stdout empty.

## Diagnostics

-q drops progress, notes, warnings and steps. Errors, hints, owner approval
instructions, diagnostic logs and app output remain. -v enables step narration.
--log debug enables connect diagnostics; --log trace enables connect and wire
traces. They are independent of -v. HTTP and subprocess log targets are excluded
so authorization headers and package login credentials cannot enter the stream.

Terminal progress refreshes once a second, showing new steps and completion at
once. Redirected progress and JSON report at ten-percent boundaries or every
five seconds. App elapsed time refreshes every second on a terminal and every
five otherwise.

With --json, stderr is JSON Lines. Each event occupies exactly one line, even
when its message contains line breaks. For example:

    {"event":"progress","message":"uploading: 1048576/2097152 bytes (50%)"}

Events are progress, note, warning, approve, hint, step, log and error. Reading
events carry message; diagnostic log events carry level, target and fields;
error events carry an error object. Progress messages describe transfers,
processing phases or elapsed time. Their wording is for reading, not a
structured progress API.

## Error codes

The code in error[code]: is stable and its exit code is the class below. This
prefix also applies to argument errors. hint: lines name a next step where the
tool knows one. JSON errors carry code, message and, for the Ark's own verdicts,
remote code and message, the code as a decimal string.

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
