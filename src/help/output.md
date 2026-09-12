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
version. Scripts should pin the tool version.

App reports are exact bytes on stdout in human and text modes; app stderr is
announced and written verbatim. JSON contains stdout and stderr strings, or
stdout_base64 and stderr_base64 when the bytes are not UTF-8. An app failure
still delivers its report. A command that did partial work retains that result
and reports failure on stderr with a nonzero exit code.

-q drops optional diagnostics, but retains errors, hints, owner approval
instructions and app output. -v shows steps; -vv connect debug, -vvv wire trace.
Credentials from package login never appear in output or logs.
