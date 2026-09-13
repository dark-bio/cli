# Ark command line interface

[![](https://img.shields.io/crates/v/darkbio-ark.svg)](https://crates.io/crates/darkbio-ark)
[![](https://github.com/dark-bio/cli/workflows/tests/badge.svg)](https://github.com/dark-bio/cli/actions/workflows/ci.yml)

`ark` is the command line interface to [Ark](https://dark.bio) enclaves. An Ark holds one person's data. It is plugged into your computer over USB, or emulated on it. Its owner approves access from their phone in Ark Companion, available on the [App Store](https://apps.apple.com/app/id6751324700) and [Google Play](https://play.google.com/store/apps/details?id=bio.dark.companion). This CLI tool talks to the Ark, the Dark Bio cloud and the phone on the owner's behalf, but it can never approve anything itself.

What this tool does:

- **Discovery**: find hardware Arks over USB and locally running emulators.
- **Pairing and unlocking**: pair once with the phone, unlock after every reboot.
- **Datasets**: upload your own files, install public reference data, inspect slots.
- **Apps**: run WebAssembly apps on the Ark and collect their reports.
- **Firmware**: list published builds, install and verify an update.
- **Diagnostics**: check your computer, the Ark and the cloud, and suggest fixes.

## Installation

Homebrew on macOS:

```sh
# Pick one or the other
brew install dark-bio/tap/ark-cli      # Stable releases
brew install dark-bio/tap/ark-cli-dev  # Develop releases
```

Shell installer on Linux:

```sh
curl -fsSL https://github.com/dark-bio/cli/releases/latest/download/ark-installer.sh | sh
```

With Rust, you can build it yourself:

```sh
cargo install darkbio-ark --locked
```

Plain executables are attached to every [GitHub release](https://github.com/dark-bio/cli/releases).

---

*Note, on Linux you need to grant your user access to the USB device once:*

```sh
echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="2e8a", ATTR{idProduct}=="10f1", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-darkbio-ark.rules
sudo udevadm control --reload-rules
```

## First session

```sh
ark devices                       # find the Ark
ark status                        # trust, firmware, paired, unlocked
ark pair                          # scan and confirm in Ark Companion
ark unlock                        # approve on the phone
ark data list                     # what is loaded
ark app run my.wasm > report.md   # approve on the phone; report on stdout
```

Pairing happens once. Unlocking lasts until the Ark loses power, and every data and app command needs an unlocked Ark; pass `--unlock` to let a command unlock first, which the phone still approves. Nothing bypasses the phone. Deleting the pairing in Ark Companion discards the unlock key, and the reset button on the Ark erases all data.

## Datasets

The Ark keeps data in named slots. `ark data list` shows them, `ark data show <slot>` describes one, and `ark data paths` prints the file paths an app can read right now.

```sh
ark data upload calls.vcf.gz --dry-run   # what the Ark makes of the file, nothing changes
ark data upload calls.vcf.gz             # identify, approve on the phone, upload, process
ark data fetch --all                     # install every public reference the Ark advertises
```

Uploads of your own data are approved on the phone; public reference downloads are not. The Ark identifies a file itself, `--slot` only asserts what you expect. Reference downloads are cached on your computer and resume after interruptions; `ark doctor` shows where the cache lives. `ark data delete` and `ark data repair` empty or reset a slot, both with a `--dry-run`.

## Apps

An app is one WebAssembly file that reads the paths it declares and prints a report. `ark app run my.wasm` uploads it, waits for the owner's approval, runs it on the Ark and prints the report on stdout, so redirect stdout to keep the exact bytes. `--format json` wraps the report with the run's metadata instead. Ctrl-C cancels a run; `ark app cancel <task>` cleans up one whose terminal went away. The [examples](https://github.com/dark-bio/examples) repository has working apps and the data tree; `ark help apps` has the manifest and the sandbox limits.

## Firmware

`ark firmware list` shows the installed build and the published candidates; `ark firmware update` installs the newest one and waits for the Ark to come back running it. Who approves depends on the Ark's state: nobody while unpaired, the device button while paired and locked, the phone while unlocked. The installation itself is confirmed on this computer, with `--yes` when there is no one to ask.

This release needs Ark firmware 0.11.5 or later; `ark --version` prints the minimum. An older Ark is still served by `status`, `doctor`, the `firmware` commands and `enroll --cwt`, enough to bring it up to date.

## Devices

One Ark is picked automatically. With several, select one with `-d` by locator, serial, name or emulator image, or with the bare words `hardware` and `emulator`. Only one process can hold an Ark at a time; `device-busy` means another `ark` command or an Ark Hub browser tab has it.

`ark status` shows the Ark's attested identity and works offline; `ark genuine` checks that identity against Dark Bio's device registry; `ark enroll` gives a fresh emulator its identity. When something is off, `ark doctor` checks your computer, the Ark and the cloud, and suggests fixes.

## Scripts and agents

`ark help agents` is written for scripts and AI agents and should be read first. Text carries every result field with exact values, so an agent can read it as it is. `--format json` is for programmatic parsing, with one JSON document on stdout and JSON events on stderr. Nothing prompts without a terminal, and the exit code says what happened: 0 done, 1 local, 2 usage, 3 device, 4 cloud, 5 Ark, 6 approval, 7 timeout, 8 app.

## Help

`ark -h` is the scan. For one command, `ark <command> --help` or `ark help <command>` adds its contract: what it requires, who approves, how long it takes, what it prints and how it exits. Six topics cover the rest, and `ark help --all` prints the whole manual as one document:

| Topic | Contents |
| --- | --- |
| `agents` | Driving the tool from a script or an AI agent |
| `states` | Pairing, locking, trust and firmware compatibility |
| `output` | Streams, formats, the JSON contract and every error code |
| `devices` | Locators, selection and cloud environments |
| `datasets` | Slots, uploads, reference downloads and the cache |
| `apps` | The manifest, the sandbox and its limits |

`ark completions <shell>` generates completions for your shell.

## Disclaimer

The Ark, its protocols and this tool are still evolving quickly. Command names and output fields are meant to stay stable, and `ark help output` has the exact promise. Every release may change behaviour, and firmware, cloud and tool versions are expected to move together.

The connection library in `connect/` is internal to this CLI package. Its Rust API is unstable and is not a supported integration interface.

## License

Licensed under the [BSD 3-Clause License](https://github.com/dark-bio/cli/blob/main/LICENSE).
