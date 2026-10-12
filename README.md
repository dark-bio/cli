# Ark command line interface

[![](https://img.shields.io/crates/v/darkbio-ark.svg)](https://crates.io/crates/darkbio-ark)
[![](https://github.com/dark-bio/cli/workflows/tests/badge.svg)](https://github.com/dark-bio/cli/actions/workflows/ci.yml)

**Built for people. Ready for agents.**

`ark` is the command line interface to [Ark](https://dark.bio) enclaves. An
Ark holds one person's genomic data and runs apps on it. It is plugged into
your computer over USB, or emulated on it by
[Ark Emulator](https://github.com/dark-bio/emulator). Its owner approves
access to their data on their phone in Ark Companion, available on the
[App Store](https://apps.apple.com/app/id6751324700) and
[Google Play](https://play.google.com/store/apps/details?id=bio.dark.companion).
`ark` talks to the Ark and the Dark Bio cloud, and it cannot approve anything
for the owner.

## Installation

Homebrew on macOS:

```sh
brew install dark-bio/tap/ark-cli      # stable releases
brew install dark-bio/tap/ark-cli-dev  # develop releases
```

The shell installer on Linux:

```sh
curl -fsSL https://github.com/dark-bio/cli/releases/latest/download/ark-installer.sh | sh
```

With Rust, you can build it yourself:

```sh
cargo install darkbio-ark --locked
```

Plain executables are attached to every
[GitHub release](https://github.com/dark-bio/cli/releases). On Linux, your
user needs access to the Ark's USB device once:

```sh
echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="2e8a", ATTR{idProduct}=="10f1", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-darkbio-ark.rules
sudo udevadm control --reload-rules
```

## First session

Plug in your Ark, or [start an emulated one](#emulated-arks), and run:

```sh
ark devices                       # find the Ark
ark status                        # trust, firmware, pairing and lock
ark pair                          # once, with your phone and the Ark's button
ark unlock                        # after each power loss, approved on your phone
ark data list                     # what the Ark holds
ark data upload calls.vcf.gz      # your variant calls, approved on your phone
ark data fetch --all              # the public references for them
ark app run my.wasm > report.md   # approve on your phone, then release the report
```

Pairing happens once, and the Ark stays unlocked until it loses power. The
Ark's button sits behind a pinhole under its bottom right LED, to the right
of the USB cable, and is pressed with the pin that came with the Ark.

## Emulated Arks

[Ark Emulator](https://github.com/dark-bio/emulator) boots the real Ark
firmware on your computer, for development and demos, and `ark` talks to it
as to hardware. It keeps its data in a plain file, so real data belongs on
hardware. A fresh emulator is enrolled once, in a browser at
[Ark Hub](https://hub.dark.bio), before it pairs:

```sh
ark-emulator start   # boot one; prints its locator once it is ready
ark enroll           # prints where to enroll it at Ark Hub
```

The enrollment lasts 30 days. `ark-emulator stop`, `ark-emulator wipe` and
`ark-emulator start` then give a fresh device to enroll again.

## Commands

| Command | What it does | Who approves |
| --- | --- | --- |
| `ark devices` | Lists hardware Arks and running emulators | nobody |
| `ark status` | Shows serial, firmware, pairing and lock, offline | nobody |
| `ark genuine` | Checks the Ark against Dark Bio's device registry | nobody |
| `ark enroll` | Gives an emulator an attested identity at Ark Hub | nobody; the person enrolls it in a browser |
| `ark pair` | Pairs the Ark with Ark Companion, once | the owner, on the phone and the Ark's button |
| `ark unlock` | Unlocks the Ark after a power loss | the owner, on the phone |
| `ark data list`, `show` | Shows the dataset slots and their state | nobody |
| `ark data paths` | Maps the data an app can read | nobody |
| `ark data upload` | Uploads the owner's variant calls | the owner, on the phone |
| `ark data fetch` | Downloads public reference data onto the Ark | nobody |
| `ark data delete`, `repair` | Empties or erases a slot | the owner, on the phone |
| `ark app run` | Runs an app and prints its report | the owner, on the phone, for the run and its report |
| `ark firmware list`, `update` | Lists and installs firmware | by the Ark's state: nobody, the Ark's button or the phone |
| `ark doctor` | Checks this computer, the Ark and the cloud | nobody |

Data and app commands need a paired, unlocked Ark. `ark <command> --help`
prints a command's full contract: what it requires, who approves, how long it
takes, what it prints and how it exits.

## AI agents

AI agents read [`ark help agents`](src/help/agents.md) first. It covers
running commands in the background, passing on the owner's approvals,
reading results and getting an Ark ready. Nothing prompts without a terminal
or under `--json`. `--json` prints complete, exact values, and the exit code
says what happened. [`ark help output`](src/help/output.md) defines the
streams, the JSON documents and every error code.

The [examples](https://github.com/dark-bio/examples) repository has worked
apps in Rust, Go, C and Python, with fixtures that run them on your computer.

## Help

`ark -h` is the overview, and `ark help --all` prints the whole manual. The
topics cover the rest, and each one's source in `src/help` reads
the same on GitHub:

| Topic | Contents |
| --- | --- |
| [`agents`](src/help/agents.md) | Running ark from an AI agent, the approvals it waits on, and common tasks |
| [`devices`](src/help/devices.md) | Finding an Ark, trust, pairing and unlocking, emulators and cloud environments |
| [`datasets`](src/help/datasets.md) | Slots, uploads, reference downloads and the cache |
| [`apps`](src/help/apps.md) | Running an app, the manifest, grants, questions, the data map and the sandbox |
| [`firmware`](src/help/firmware.md) | Compatibility, published builds and updates |
| [`output`](src/help/output.md) | Reading output, JSON, events and error codes |

`ark completions <shell>` prints completions for your shell.

## Stability

The Ark, its protocols and this tool still change quickly, and firmware,
cloud and tool versions move together. `ark --version` prints this tool's
version. Against firmware older than it supports, commands fail with
`firmware-outdated`, which names the minimum version. The JSON documents and
error codes follow `ark help output`, while the reading layouts may change.

The connection library in `connect/` is internal to this package. Its Rust
API is unstable and is not a supported integration interface.

## License

Licensed under the [BSD 3-Clause License](https://github.com/dark-bio/cli/blob/main/LICENSE).
