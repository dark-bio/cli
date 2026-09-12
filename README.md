# Ark CLI

`ark` talks to Dark Bio Arks attached to this computer and locally running
emulators. It keeps results on stdout, diagnostics on stderr, and owner approval
in Ark Companion.

```sh
cargo run -- devices
cargo run -- status
cargo run -- pair
cargo run -- data list --unlock
cargo run -- data upload calls.vcf.gz --dry-run
cargo run -- data fetch --all --unlock
cargo run -- app run app.wasm --unlock > report.md
cargo run -- firmware update --dry-run
```

Build and install locally with Rust 1.98 or later:

```sh
cargo install --path . --locked
ark help agents
```

The Cargo package is `darkbio-ark`; its executable is `ark`. GitHub releases
provide plain executables and a shell installer. The macOS Homebrew formula is
`dark-bio/tap/ark-cli`. Executables use `ark-<version>-<os>-<arch>`, with `arm64`
or `amd64` and `.exe` on Windows. A version tag runs the
[release workflow](.github/workflows/publish.yml).

## Commands

| Command | Purpose |
| --- | --- |
| `devices` | List hardware and emulators, without connecting |
| `status` | Read identity, trust, firmware, pairing and lock state, without cloud sync |
| `genuine` | Check the device proof against the cloud registry |
| `pair` | Pair with Ark Companion through the cloud rendezvous |
| `unlock` | Unlock, approved on the phone; an unlocked Ark is a no-op |
| `enroll` | Open the Hub enrollment path, or install an attestation with `--cwt FILE` |
| `data list`, `data show SLOT` | Read dataset inventory and metadata |
| `data paths` | Print the Ark's README of paths available to apps |
| `data upload FILE` | Identify, upload and process a local file |
| `data fetch SLOT`, `data fetch --all` | Download and install advertised reference data |
| `data delete SLOT`, `data repair SLOT` | Delete or reset a slot, approved on the phone |
| `app run FILE`, `app cancel TASK` | Run a WASI app or cancel its task |
| `firmware list`, `firmware update` | Inspect candidates or install and verify an update |
| `doctor` | Diagnose discovery, connection, cloud, relay and cache access |
| `help`, `completions SHELL` | Read command contracts or generate completions |

Use `ark help COMMAND` or `COMMAND --help` for prerequisites, approval, timing,
result fields and examples. `-h` is the short form; `ark help --all` prints the
manual. The six topics are `agents`, `states`, `output`, `devices`, `datasets`
and `apps`.

Select by exact locator, serial, name or emulator image basename with
`--device`. The bare words `hardware` and `emulator` select the only Ark of that
kind. Ambiguity lists locators. Another CLI process or a browser tab can hold
a hardware device; close it before retrying `device-busy`.

## Scripts and terminals

`--format json` writes one bare JSON result on stdout and newline-delimited
JSON events on stderr. JSON never prompts. A failed command may still return
partial results: an allocated app task, completed reference slots, or an
acknowledged firmware installation. Check the exit code as well as the document.

```sh
ark data fetch --all --unlock --format json > result.json 2> events.jsonl &
wait $!
ark firmware update --yes --no-input --format json
```

The default `auto` format follows each stream's TTY independently. `human` keeps
colours and progress bars; `text` uses plain key blocks and tables. `NO_COLOR`
disables colour. App reports and app stderr are exact bytes in human and text
mode; JSON uses strings or base64 fields when bytes are not UTF-8.

`--timeout` defaults to 60 seconds per expected machine response. It does not
limit an entire transfer or app run. Phone, button and pairing waits use the
protocol's own window with a reply margin. Rust callers can also pass absolute
deadlines; scripts can impose a ceiling with their shell's `timeout` command.
Ctrl-C and SIGTERM attempt cancellation, preserve partial results and exit 130
or 143. Closing a connection alone does not cancel work on the Ark.

`--unlock` permits the extra unlocking step. `--dry-run` never unlocks, starts
an upload or requests approval, and conflicts with `--unlock`. The CLI asks
for confirmation only before firmware installation: `--yes` confirms the
install and reboot, while device approval still belongs to the owner.

`-q` hides optional diagnostics and retains errors, hints and approvals. `-v`
shows steps, `-vv` connect diagnostics, and `-vvv` wire trace events. Package
credentials and subprocess output never enter these logs.

## Reference data

`data fetch` streams HTTPS downloads through to the Ark and retains public
reference bytes in the platform cache directory under `ark`. Local uploads and
app reports are never cached. `--cache DIR` changes the location; `--no-cache`
streams without retention. `doctor` reports its path and size.

One writer owns each partial entry. Durable prefixes resume with a validated
range response and ETag or Last-Modified. Complete entries are rehashed while
replaying; corruption causes a fresh download. Network failures get at most
three attempts; device refusals and processing failures are not retried. Disk
write failures disable retention while the upload continues. Removing the cache
directory resets it.

## Environments

The CLI trusts release, staging and develop roots in every build:

```sh
cargo run -- status
cargo run -- data list --env develop
```

The CLI chooses cloud routing from `--env`, then a trusted attestation, then the
emulator launcher's report, then release. An explicit override that contradicts
attestation warns. Routing never changes handshake trust. A develop or staging
route prints one note per command, hidden by `--quiet`. The offline attested
label identifies the signer; `genuine` checks current cloud registration.
`--version` shows dependency versions and firmware compatibility minimums.

Develop and staging package hosts can use `cloudflared` for protected access.
They reuse cached Access tokens; an interactive login opens the browser only
when the package host challenges. JSON, `--no-input` and redirected stdin
instead return `login-required` with the command to run. The release package
host does not use the Access helper.

## Library boundary

[`darkbio-connect`](connect) owns discovery, authenticated sessions, request
pairings, operation timing, lazy cloud sync and relay attachment, and protocol
workflows for pairing, transfers and execution. The CLI owns selection defaults,
state checks, prompts, package selection, HTTPS downloads, cache recovery,
signals, help and rendering.

```rust
use darkbio_connect::{schema, Timing, TrustMode};
use std::time::{Duration, Instant};

fn main() -> Result<(), darkbio_connect::Error> {
let found = darkbio_connect::list();
let (ark, identity) = found.select(None)?.connect(&TrustMode::RootOrSelf)?;
let client = ark.client();
let info = client.call(schema::DeviceInfoRequest {}, Timing::inactivity(Duration::from_secs(2)))?;

// An absolute deadline can bound several calls together.
let deadline = Instant::now() + Duration::from_secs(5);
let state = client.call(schema::DeviceInfoRequest {}, deadline)?;
Ok(())
}
```

`Ark` owns the session. Clonable `Client` handles do not prolong it and have no
global timeout. Requests establish their prerequisites lazily. `sync` refreshes
cloud keys and time explicitly; `attach_relay` reuses a healthy attachment.
Readers and progress callbacks run on the caller's thread. Supplied readers
must enforce their own I/O timeout because an arbitrary blocking `Read` cannot
be interrupted by connect.

Dataset and firmware helpers accept readers and size/hash descriptors. Connect
never downloads archives or references, selects a firmware version, opens a
browser or chooses a cache directory. Its API is documented by `cargo doc -p
darkbio-connect --open`.

## Protocol integration and checks

The CLI requires firmware 0.11.5 or later. Status reports sync freshness without contacting the cloud;
connect skips redundant sync and refreshes explicitly for `genuine` and `doctor`.
`data paths` returns the firmware README, and reserved approval outcomes use exit class 6.

Below the minimum, only discovery, diagnostics, firmware commands and
`enroll --cwt` are served. Status prints its document, then `firmware-outdated`.
Develop firmware images must also meet the fixed timestamp cutoff printed by
`ark --version`. Older images report "develop build is outdated; rebuild or
update the firmware". Tagged builds use the version minimum alone.
Online enrollment directs callers to the Hub.

```sh
cargo build --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Tests use simulated wire peers, USB queues and loopback HTTP/WebSocket servers;
no device writes or live cloud approval are involved.
