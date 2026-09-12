# Output contract

stdout carries the result. stderr carries progress, notes, warnings, approval
instructions, hints and errors. auto chooses human or text independently for
each stream. human keeps colour and terminal progress; text uses stable field
names but is for reading. NO_COLOR disables colour. JSON is for parsing.

Human upload and processing progress refreshes once a second as updates arrive,
with new steps and completion shown immediately. Text and JSON report at
ten-percent boundaries or every five seconds. App elapsed time refreshes every
second in human mode and every five seconds otherwise.

JSON prints one bare document on stdout and JSON lines with an event field on
stderr. Keys are snake_case, absent values null, enums strings, times ISO 8601
UTC, byte counts suffixed _bytes and durations _seconds. Task IDs are decimal
strings so every u64 is exact. Fields may be added; renames require a major
version. Scripts should pin the tool version. help and completions print text
in every format.

App reports are exact bytes on stdout in human and text modes; app stderr is
announced and written verbatim. JSON contains stdout and stderr strings, or
stdout_base64 and stderr_base64 when the bytes are not UTF-8. Failed apps
return output only with `develop = true`; the CLI preserves whatever the Ark
returns. A command that did partial work retains that result
and reports failure on stderr with a nonzero exit code.

-q drops optional diagnostics, but retains errors, hints, owner approval
instructions and app output. -v shows steps; -vv connect debug, -vvv wire trace.
Credentials from package login never appear in output or logs.

## Error codes

The code in error[code]: is stable and its exit code is the class below. hint:
lines name the next step where the tool knows one. In JSON the error event
carries code, message and, for the Ark's own verdicts, remote code and message.

Exit 1, local input or confirmation:
- `file-not-found`, `file-unreadable`, `file-empty`: the named path
- `file-rejected`: the Ark or the tool refused the file's content
- `invalid-slot`: --slot differs from what the Ark identified, or no such slot
- `invalid-key`, `invalid-version`: a malformed --pubkey, or a --version not
  published for the environment
- `confirmation-required`: firmware installation needs confirmation; use
  --yes when running noninteractively
- `enrollment-required`: online enrollment happens at the Ark Hub
- `io`: a local read or write failed

Exit 2, `usage`: invalid arguments or an unknown help topic.

Exit 3, device access:
- `no-device`: no Ark found; run `ark devices`
- `ambiguous-device`: several match; the hint lists locators for --device
- `device-busy`: another ark process or an Ark Hub browser tab holds the USB
  session; close it
- `device-unreachable`, `disconnected`: the connection failed or dropped.
  Linux USB permission errors include a udev-rule hint.
- `handshake-failed`: the attestation or the identity did not verify

Exit 4, cloud:
- `cloud-unreachable`: an HTTP, package host or relay request failed
- `environment-unknown`: no cloud environment; select one with --env
- `login-required`: the package host wants a browser login; the hint has the
  cloudflared command
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
