# Errors

Each code is stable and belongs to one exit class. Running a command again
does its work again, and one gated on an approval asks the owner again, so
ask the person before such a run. Each class below says when another run
can help.

## Exit 1

Exit 1 is a problem with a local input or a missing confirmation. Another
run helps once the input is fixed or the confirmation given.

- `file-not-found`, `file-unreadable` and `file-empty` mean the named file
  cannot be used. Check its path and permissions, and that it is not empty.
- `file-rejected` means the Ark or ark refused the file's content, and the
  message says why. Fix the file, or use another one.
- `invalid-slot` means the Ark identified another slot than --slot names, or
  has no such slot. `ark data list` shows the slots.
- `invalid-key` means --pubkey is not a valid public key.
- `invalid-version` means the build given to `ark firmware update` is not
  published for this environment. `ark firmware list` shows what is.
- `confirmation-required` means installing firmware needs confirmation. Add
  --yes to `ark firmware update`.
- `enrollment-required` means the Ark is enrolled in a browser at Ark Hub,
  at the address the hint gives. Enroll it there.
- `io` means a local read or write failed. The message names what failed.

## Exit 2

Exit 2 is `usage`, for invalid arguments or an unknown help topic. Another
run helps once the arguments are fixed, as the hint or the command's help
shows. A slot of 0 or below is a usage error, and a positive one the Ark
lacks is `invalid-slot`.

## Exit 3

Exit 3 is a problem reaching the Ark. Another run can succeed once the Ark
is attached, selected and free, unless `handshake-failed` names a failed
verification.

- `no-device` means no Ark is attached, or none matches --device. Run
  `ark devices`.
- `ambiguous-device` means several Arks match. Pick one with --device from
  the selectors the hint lists.
- `device-busy` means another ark command or an Ark Hub browser tab holds
  the Ark. Wait for the command to finish, or close the tab.
- `device-unreachable` and `disconnected` mean the connection failed or
  dropped. Check that the Ark is plugged in or the emulator is running. On
  Linux, a permission error's hint gives the udev rule.
- `handshake-failed` means the handshake did not complete, and the message
  says why. A connection that dropped during it can pass on another run. A
  failed verification, such as an attestation signed by an unknown key,
  needs the identity fixed, and `ark help devices` covers enrollment and
  recovering an identity.

## Exit 4

Exit 4 is a problem with the cloud, which usually passes. Run the command
again after a short wait, once any login or approval hint is followed.
Another run cannot fix `environment-unknown` or `registry-inactive`.

- `cloud-unreachable` means a request to the cloud, the package host or the
  relay failed. Check the network.
- `approval-undelivered` means the approval never reached the phone. The
  warning before it names the cause, and `ark doctor` checks the relay.
- `environment-unknown` means no cloud environment is known. Select one with
  --env.
- `login-required` means a develop or staging host wants a browser login.
  The hint gives the cloudflared command to run.
- `proof-rejected` means the cloud refused the Ark's proof. Run
  `ark doctor`. A firmware update refreshes the keys first, so run it again.
- `pairing-failed` means the pairing exchange failed in the cloud or on the
  phone. Run `ark pair` again.
- `registry-inactive` means the Ark's registration is disabled, expired or
  superseded. A disabled Ark needs Dark Bio. An expired or superseded
  emulator becomes a fresh device through `ark-emulator stop`, `wipe` and
  `start`, and is then enrolled again.

## Exit 5

Exit 5 is the Ark's state or its refusal. Another run helps once the state
or the request is fixed.

- `not-paired` means no phone is paired with the Ark yet. Run `ark pair`.
- `locked` means the Ark needs unlocking. Run `ark unlock`, or add --unlock.
  A dry run needs `ark unlock` first.
- `already-paired` and `already-enrolled` mean the Ark is in that state
  already. `ark status` hints at the next step.
- `firmware-outdated` means the firmware is older than this tool supports.
  Run `ark firmware update`, or install a newer Ark Emulator for an
  emulator.
- `update-unverified` means the Ark came back from a firmware update running
  another build than the one installed. Check `ark status` and
  `ark firmware list` before choosing a build again.
- `dependency-missing` means a reference slot requires a slot that is empty,
  such as the owner's calls. Fill that one first.
- `no-download` means the Ark offers no download for the slot
  `ark data fetch` names. `ark data show` shows the slot's requirements.
- `ark` means the Ark refused the request. Act on its message, never on its
  number, which can change between firmware builds.
- `unsupported` means the Ark never serves the request in its build or role,
  such as a firmware update on an emulator.
- `unavailable` means the Ark serves the request, but not in its current
  state, such as before a cloud sync. `ark doctor` finds what is missing.
- `unknown` means the Ark does not know the request, so the firmware and ark
  are out of step. Update both.
- `unanswered` means the Ark dropped the request without an answer. Run
  `ark doctor` before running the command again.

## Exit 6

Exit 6 is an approval that did not come. Ask the person before running the
command again, since another run asks the owner again.

- `approval-denied` means the owner declined the request or an app's
  report, or Ark Companion sent an answer the Ark could not accept. Ask the
  person whether they want another attempt.
- `approval-timeout` means nobody answered within the window. Ask the person
  to have the phone or the Ark at hand for another attempt. For pairing,
  run `ark pair` again.

## Exit 7

Exit 7 is `timeout`, for a wait on a machine that ran past --timeout or past
a fixed limit. Such limits are the 5s handshake that opens a connection and
the 120s an Ark gets to return from a firmware update. A reference download
also fails after 30s without an acknowledgment. Another run may succeed.
Check the connection and `ark status` first, and raise --timeout for a slow
machine.

## Exit 8

Exit 8 is `app-failed`, for an app that reported failure when the owner
released its result. The document still holds what the Ark returned. Read
the report, and fix the app or its inputs before running it again.

## Exit 130 and 143

Exit 130 is `interrupted` and exit 143 `terminated`, for Ctrl-C and SIGTERM.
ark asks the Ark to cancel the active task or upload before it exits, and
under --json the document holds the last partial result. An upload the Ark
is already processing keeps going. Another run can finish the work. Check
the partial result and `ark status` first, and ask the person before another
approval.
