# Ark command line interface

[![](https://img.shields.io/crates/v/darkbio-ark.svg)](https://crates.io/crates/darkbio-ark)
[![](https://github.com/dark-bio/cli/workflows/tests/badge.svg)](https://github.com/dark-bio/cli/actions/workflows/ci.yml)

`ark` is the command line interface to [Ark](https://dark.bio) enclaves. An Ark holds one person's data. It is plugged into your computer over USB, or emulated on it by [Ark Emulator](https://github.com/dark-bio/emulator). Its owner approves access from their phone in Ark Companion, available on the [App Store](https://apps.apple.com/app/id6751324700) and [Google Play](https://play.google.com/store/apps/details?id=bio.dark.companion). This tool talks to the Ark, the Dark Bio cloud and the phone on the owner's behalf, but it can never approve anything itself.

What this tool does:

- **Discovery**: find hardware Arks over USB and locally running emulators.
- **Unlocking**: pair once with the phone, unlock after every reboot.
- **Datasets**: inspect slots, upload your own files and install public reference data.
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

Plug in your Ark, or [start an emulated one](#emulated-arks), and run:

```sh
ark devices                       # find the Ark
ark status                        # trust, firmware, paired, unlocked
ark pair                          # if unpaired; scan in Ark Companion
ark unlock                        # if locked; approve on the phone
ark data list                     # what is loaded
ark app run my.wasm > report.md   # approve on the phone; report on stdout
```

Pairing happens once. Unlocking lasts until the Ark loses power. Check `ark status` before a data command or app run; these need an unlocked Ark. If it reports locked, `--unlock` lets the command unlock first, with phone approval. Nothing bypasses the phone. Deleting the pairing in Ark Companion discards the unlock key, and the reset button on the Ark erases all data.

## Emulated Arks

[Ark Emulator](https://github.com/dark-bio/emulator) boots the real Ark firmware on your computer, for development and demos, and `ark` talks to it as it does to hardware. It keeps its data in a plain file, so keep real data on hardware. A fresh emulator is enrolled once, in a browser at [Ark Hub](https://hub.dark.bio), before it pairs:

```sh
ark-emulator start   # boot one; prints its locator once it is ready
ark enroll           # prints where to enroll it at Ark Hub
```

After that, the first session above applies. The enrollment lasts 30 days; then `ark-emulator stop`, `ark-emulator wipe` and `ark-emulator start` give a fresh device to enroll again. `ark-emulator --help` covers the rest.

## Datasets

The Ark keeps data in named slots. `ark data list` shows the inventory, `ark data show <slot>` adds a slot's description and upload format, and `ark data paths` maps the data apps can read, with `--json` giving each path's description, exact format and examples. These commands do not transfer datasets.

```sh
ark data upload calls.vcf.gz --dry-run   # identify and plan without changes
ark data upload calls.vcf.gz             # identify, approve, upload, process
ark data fetch --all                     # fill empty public reference slots
```

Uploads of your own data are approved on the phone; public reference downloads are not. The Ark identifies a file itself, `--slot` only asserts what you expect. Reference downloads are cached on your computer and resume after interruptions; `ark doctor` shows where the cache lives. `ark data delete` empties a filled slot and `ark data repair` resets a damaged slot; both offer `--dry-run`. See `ark help datasets` for dependencies, build metadata and cache details.

## Apps

An app is one WebAssembly file that reads the paths it declares and prints a report. `ark app run my.wasm` uploads it, waits for the owner's approval, runs it on the Ark and writes the report's exact bytes to stdout. Redirect stdout to save it, or use `--json` to include the run's metadata. Ctrl-C cancels a run; `ark app cancel <task>` cleans up one whose terminal went away. `ark data paths` describes the data tree; `ark help apps` has the manifest, grant rules and sandbox limits. The [examples](https://github.com/dark-bio/examples) repository has worked apps in Rust, Go, C and Python, with fixtures that run them on your computer.

## Firmware

`ark firmware list` shows the installed build and published candidates; `ark firmware update` installs the newest candidate and waits for the Ark to return running it. Who approves depends on the Ark's state: nobody while unpaired, the device button while paired and locked, the phone while unlocked. The installation itself is confirmed on this computer, with `--yes` when there is no one to ask.

This release needs Ark firmware 0.11.5 or later; `ark --version` prints the minimum. An older Ark is still served by `status`, `doctor`, the `firmware` commands and `enroll --cwt`, enough to bring it up to date. An emulator runs the firmware bundled with Ark Emulator, so a newer emulator release is its update.

## Devices and diagnostics

One Ark is picked automatically. With several, select one with `-d` by locator, serial, name or emulator image, or with the bare words `hardware` and `emulator`. Run commands one at a time, including reads. `device-busy` means another `ark` command or an Ark Hub browser tab holds the USB session.

`ark status` shows the Ark's attested identity and works offline, and `ark genuine` checks that identity against Dark Bio's device registry. When something is off, `ark doctor` checks your computer, the Ark and the cloud, and suggests fixes without applying them. `-v` adds step narration; `--log debug` or `--log trace` enables diagnostic logs.

## Output and automation

Default output is formatted for reading. Use `--json` for complete, exact values, with an indented result document on stdout and one JSON event per line on stderr. App reports pass through raw by default. Color requires a terminal; `NO_COLOR` or `CLICOLOR=0` disables it.

Scripts and AI agents should read `ark help agents` first. Nothing prompts without a terminal or with `--json`, and the exit code says what happened. `ark help output` defines the streams, JSON fields and error codes.

## Help

`ark -h` is the scan. For one command, `ark <command> --help` or `ark help <command>` adds its contract: what it requires, who approves, how long it takes, what it prints and how it exits. Global flags are available on every command. Six topics cover the rest, and `ark help --all` prints the whole manual:

| Topic | Contents |
| --- | --- |
| `agents` | Driving the tool from a script or an AI agent |
| `states` | Pairing, locking, trust and firmware compatibility |
| `output` | Reading output, JSON, streams and error codes |
| `devices` | Locators, selection, emulators and cloud environments |
| `datasets` | Slots, uploads, reference downloads and the cache |
| `apps` | The manifest, the sandbox and its limits |

`ark completions <shell>` generates completions for your shell.

## Disclaimer

The Ark, its protocols and this tool are still evolving quickly. `ark help output` describes the JSON contract; reading layouts may change. Firmware, cloud and tool versions are expected to move together.

The connection library in `connect/` is internal to this CLI package. Its Rust API is unstable and is not a supported integration interface.

## License

Licensed under the [BSD 3-Clause License](https://github.com/dark-bio/cli/blob/main/LICENSE).
