# Command Line Interface for Ark Enclaves

`ark list` shows attached hardware Arks and emulators registered by local launchers.
Select an Ark with `ark status --device emulator:18181` or the hardware locator
printed by `list`. Unique serials, names and image basenames also work;
ambiguous labels are rejected. Discovery warnings do not hide devices found
through another discovery source.
The `hardware:` and `emulator:` selector prefixes are reserved for locators.
Both select discovered Arks; connect handles the transport used to reach them.

```sh
cargo run -- list
cargo run -- status --device emulator:18181
cargo run -- genuine --device emulator:18181
cargo run -- unlock --device emulator:18181 --timeout 60
cargo run -- slots --device SERIAL
cargo run -- upload calls.vcf.gz --device SERIAL
cargo run -- upload --reference-genome --device SERIAL
cargo run -- upload --gene-annotations --device SERIAL
cargo run -- upload --variant-catalog --device SERIAL
cargo run -- delete snp-indel-calls --device SERIAL
cargo run -- repair 2 --device SERIAL
cargo run -- execute app.wasm --device SERIAL > result.json
cargo run -- cancel 42 --device SERIAL
cargo run -- update --device SERIAL --check
cargo run -- update --device SERIAL
cargo run --features internal,develop -- onboard --device emulator:18181 --cwt device.cwt
```

The default build trusts release Arks. Add `--features develop` or
`--features staging` to enable those environments, or `--all-features` for all
environments and internal commands. Use `--no-default-features --features develop`
for a develop-only build. Cargo's `--release` flag selects optimization, while
the `release` feature selects the production trust roots.

An attestation from an excluded environment reports the feature needed to
connect. For a develop Ark, use `cargo run --features develop -- status`.

`--env release|staging|develop` explicitly selects the cloud environment. Its
matching build feature must be enabled, for example:

```sh
cargo run --features develop -- slots --env develop
```

Attested Arks select their cloud automatically unless overridden. Self-signed
and recovery connections require `--env` for cloud-dependent commands; discovery
selects their hardware or emulator registry. An attested realm takes precedence
over discovery. Selecting an environment does not change the handshake's trust
result. The Ark verifies the cloud certificates, and protected cloud endpoints
verify device proofs against the cloud's registry, independently of the
certificate presented during the handshake.

`ark slots` fetches the Ark's dataset slot inventory and metadata. It displays
each slot's name, ID, description, origin, filled or damaged state, dependencies,
and available build information. Reference slots may also advertise a download
URL, byte count and SHA-256; these are displayed as metadata. The command transfers
no dataset files and makes no changes to slots.

Connect synchronizes the cloud before requesting the inventory. `--timeout`
bounds setup and the request in seconds (30 by default, after connecting).
The Ark requires unlocking to read its slots; a refusal is returned unchanged.
Use `ark unlock --device SERIAL` to request unlocking before retrying.

`ark delete SLOT` removes a healthy, filled slot's contents. `ark repair SLOT`
resets a slot to empty, removing damaged or incomplete data too; it does not
restore data. Select an ID from `ark slots` or a protocol name:
`reference-genome`, `gene-annotations`, `snp-indel-calls` or `variant-catalog`.
Both commands synchronize the cloud and attach the companion relay automatically.
Approve the operation in the companion app. The Ark enforces slot state and
dependencies, and its refusal is returned unchanged. `--timeout` covers setup,
approval and deletion in seconds (60 by default, after connecting).

`ark upload FILE` sends a local dataset to the Ark. The Ark identifies the file
from its first chunk, selects the slot and requests companion approval when
needed. Files are streamed as they are, including compressed inputs; format
validation belongs to the Ark. Uploading waits for validation and indexing to
finish before reporting success. Transfer and processing progress retain the
CLI's colored output on the terminal and go to stderr.
Transfers show recent throughput and estimated time remaining. Processing reports
name the current step and its position, for example `Processing [2/3] Index: 40%`,
with an ETA for that step only. Estimates appear after enough progress is observed
and restart for each new processing step. Slow steps refresh every five seconds
as reports arrive, even when the displayed percentage has not changed.

`ark upload --reference-genome`, `--gene-annotations` or `--variant-catalog`
fetches the slot inventory and streams the advertised reference download directly
to the Ark. Choose one reference flag or a local file per command. The flags select
the protocol's slot kind, independently of the name reported by the Ark.
The download must match the advertised byte count and SHA-256
before processing starts. Reference URLs and redirects require HTTPS and receive
no cloud or package credentials. The Ark also validates reference content against
its catalog. Missing download metadata is reported instead of guessing a build.

Both forms synchronize cloud keys and time automatically and attach the relay
when the Ark requests authorization. They let the Ark enforce locking,
dependencies and slot availability. `--timeout` covers setup, approval, download,
transfer and processing in seconds (3600 by default, after connecting).
Failures are not retried. Once a session is known, failures attempt cancellation
within the remaining deadline, waiting at most one second for cleanup. After a
timeout or disconnect, the Ark may retain an incomplete session until it expires.

`ark execute FILE.wasm` streams an app to the Ark, requests companion approval,
and waits for its result. The Ark validates the app and its dataset requirements.
Uploads show throughput and ETA; running apps show elapsed time, since the
protocol provides no execution percentage. All CLI progress and completion
messages go to stderr. The app's stdout and stderr are written as bytes to their
respective streams, including when the app fails. This allows redirecting stdout
to a result file without CLI messages entering it.

`--timeout` covers cloud setup, upload, approval and execution in seconds (3600
by default, after connecting). Execution prints its task ID as soon as the Ark
allocates it. Ctrl-C requests cancellation and waits up to five seconds for an
acknowledgement before exiting with status 130. `ark cancel TASK` can also cancel
an upload or execution after reconnecting, with a default timeout of 30 seconds.
Another process must release the device before this command can connect to it.

Failed execution operations attempt cancellation within the remaining deadline,
waiting at most one second. Timeout, disconnect or interruption before receiving
a task ID can leave work on the Ark; closing the connection alone does not cancel
it. Operations are never retried automatically. Retrieving a completed execution
consumes its result, so only one caller should poll a given task.

`ark genuine` checks an Ark against the cloud device registry. It
automatically synchronizes the Ark's cloud keys and clock before requesting
the genuinity proof. Cloud synchronization and proof verification share a
30-second budget. The result reports the serial, enrollment time and any
disabled, expired or superseded state. An inactive
registration or a failed check exits with status 3.

`ark unlock` requests approval from the paired companion app. Connect synchronizes
the Ark and attaches its cloud relay before sending the unlock request, then
forwards the encrypted authorization exchange internally. `--timeout` bounds
that whole operation in seconds (60 by default). The firmware also enforces its
own approval window. Success is printed only after the Ark confirms unlocking;
denial, an unavailable relay or an expired deadline exits with status 3.

`ark update` installs the newest published firmware for the Ark's
selected environment. `--check` only displays the available update;
`--version 0.12.0-1234567` selects an exact published build and lets the Ark decide
whether to accept it. Automatic selection looks for a higher semantic version
or a replacement for a develop build at the same version.
`--timeout` covers cloud setup, approval, download, upload,
verification and installation in seconds (600 by default, after connecting).

An unpaired Ark needs no approval. A paired, locked Ark requests a button press;
an unlocked Ark requests companion approval through the relay. The CLI streams
the encrypted archive, checks its advertised length and SHA-256, then asks the
Ark to verify and install it. Progress, transfer throughput and ETA are printed
to stderr. Successful installation reboots the Ark; the command acknowledges
installation but does not wait to verify
the subsequent boot. Errors stop the sequence, and chunks or installation are
never retried automatically. If the connection is lost during installation, check
the Ark's status after reconnecting before attempting another update.

Self-signed and recovery connections can update with an explicit environment.
The Ark decides whether it can perform an update, and its rejection is returned
unchanged.

Develop and staging builds include login support for package hosts protected by
Cloudflare Access. Install [cloudflared](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/)
on your PATH, then run:

```sh
cargo run --features develop -- update --device SERIAL --check
```

When the package host redirects to Access, the CLI invokes
`cloudflared access login --app https://pkg.darkbio.dev` (or the staging host).
The helper opens the browser when login is needed and manages its stored token.
The CLI captures the token and reuses it for package requests during the command.
Login and the single HTTP retry share the update deadline. Release-only builds
omit this helper entirely; mixed builds invoke it only for enabled develop or
staging package hosts. The browser flow follows Cloudflare's
[CLI authentication procedure](https://developers.cloudflare.com/cloudflare-one/tutorials/cli/).

Connect exposes `Client::with_package_auth` for callers supplying package
credentials. Its callback receives the package origin, an optional login redirect
and the deadline, and returns an optional HTTP header. With no redirect it may
return cached credentials; a redirect allows it to authenticate. The returned
client and its clones retain the hook. Credentials are sent only to the original
package origin and never added to cloud API or relay requests. Connect runs no
login processes and contains no Cloudflare-specific authentication logic.

Connect owns cloud setup, which is lazy and tied to the connection. Requests
declare `Request::SETUP` as `Setup::None`, `Setup::Cloud` or `Setup::Relay`.
Relay setup includes cloud synchronization. Client clones share
one setup attempt and reuse a completed sync; failures allow a later retry.
`status` and `onboard` do not contact the cloud. Requests requiring `Setup::Relay` attach
the relay lazily and reuse it across client clones.

Onboarding reads the certificate before connecting and reconnects to the same
endpoint for status. Failure of that follow-up check is a warning after
successful onboarding. Exit codes are 0 for success, 1 for local input or
selection failures, 2 for argument parsing, and 3 for connection or request
failures. An app that finishes unsuccessfully exits with status 4; an interrupted
execution exits with status 130.

Status reports what the handshake established: a trusted certificate, a
self-signed identity, or a key pinned with `--pubkey`. Recovery does not check
attestation; self-signing does not establish provisioning history. The CLI
trusts the roots enabled by its build features. Discovery metadata and endpoint
locators are not authenticated identities, and locators can change or be
reused after a device reconnects.

The `connect` workspace crate is the reusable library. It owns discovery,
trust policy, transport adapters and cloud operations; `darkbio-wire` owns the
encrypted protocol, multiplexing, worker threads and session lifecycle. `list()` discovers both
kinds of Ark and retains independent discovery failures. `hardware::list()`
and `emulator::list()` each return `Result<Vec<Device>, Error>` for one kind.
Devices from either list connect and authenticate through `Device::connect`.
Custom verifiers return `Identity` to establish cloud routing along with trust.
`Device::connect_with_env(&verifier, env)` supplies an explicit environment while
preserving the verifier's identity. The setting is fixed for the connection and
shared by every client handle. Without an attested or explicit environment,
cloud-dependent operations return `Error::MissingEnvironment`.
Hardware owns its USB adapter; emulator owns its WebSocket adapter and launcher
registry discovery. Those implementation modules are private.

```rust
use darkbio_connect::{schema::DeviceInfoRequest, TrustMode};
use std::time::Duration;

let found = darkbio_connect::list();
for error in &found.errors {
    eprintln!("discovery warning: {error}");
}
let (ark, identity) = found.select(None)?.connect(&TrustMode::RootOrSelf)?;
let client = ark.client();
let info = client.call_timeout(DeviceInfoRequest {}, Duration::from_secs(2))?;
println!("Firmware: {}", info.firmware_version);
```

`Ark` is the session owner. Its clonable `Client` handles issue requests without
keeping the connection alive. Dropping or closing the owner ends pending calls.
Clients carry no default timeout. `call(request, deadline)` and
`send(request, deadline)` take an `Instant`; reuse one deadline to share a budget
across multiple requests. `call_timeout` and `send_timeout` accept a `Duration`
for a budget starting at that call. Request deadlines cover cloud prerequisites,
queueing, sending and accepting the response. Discovery, connection setup and
response decoding are outside that budget. Timing out or dropping a pending result does not
cancel work already received by the Ark.

```rust
use darkbio_connect::schema::{DeviceInfoRequest, PairingStatusRequest};
use std::time::{Duration, Instant};

let deadline = Instant::now() + Duration::from_secs(2);
let info = client.call(DeviceInfoRequest {}, deadline)?;
let pairing = client.call(PairingStatusRequest {}, deadline)?;
```

`client.genuine(deadline)` returns a `Registration` after syncing if needed,
obtaining a proof and checking the cloud registry. `registration.active()`
reports whether the verified device is permitted to use the cloud; the
individual flags explain an inactive registration.

When calls join an ongoing sync, the first caller's deadline bounds the attempt.
Other callers retain their own deadlines while waiting. Closing the Ark releases
setup waiters; setup I/O already in progress retains its deadline. Relay setup
follows the same rules. A later call can replace a failed relay; failed operations
are not automatically replayed.
Unfinished DNS lookups are shared across attachment retries; a caller timing out
does not start a replacement lookup while the previous one is still running.

Unlock is a normal typed request:

```rust
use darkbio_connect::schema::UnlockRequest;

client.call_timeout(UnlockRequest {}, Duration::from_secs(60))?;
```

Firmware discovery and the complete update sequence are also available through
connect. `Client::firmwares(deadline)` returns published `Firmware` entries newest
first. `Firmware::is_update_for(installed)` follows the device's version rules.
`Client::update_firmware(&firmware, deadline, progress)` reports `UpdateProgress`
stages on the caller's thread and returns after the installation acknowledgement.
Client clones cannot start overlapping updates through this helper. Callers using
raw firmware requests must coordinate those separately.

`Client::upload_dataset(name, size, &mut reader, deadline, progress)` identifies,
authorizes, streams and processes a dataset from a `Read` source. The source must
provide exactly `size` bytes from its current position. `Client::upload_reference`
accepts a `schema::SlotStatus` containing the advertised reference download and
runs the same transfer and processing sequence. Both report `UploadProgress` on
the calling thread. Reads and progress callbacks must return promptly; a blocking
caller-supplied reader cannot be interrupted by the operation deadline. Only two
chunks are outstanding at a time, allowing device writes and USB transfers to
overlap without buffering the whole file. The Ark coordinates concurrent upload
sessions and rejects conflicting operations.

`Client::execute(size, &mut reader, deadline, progress)` streams an app, obtains
companion approval and polls until the Ark returns its `ExecutionResultResponse`.
It reports `ExecutionProgress` on the caller's thread. `Started { taskid }` exposes
the ID for `ExecutionCancelRequest` through another client clone, including while
approval or a status request is pending. Connect installs no signal handlers.
The source must contain exactly `size` bytes; reads and callbacks must return
promptly. A failed app returns its output with `success: false`, while transport,
approval and invalid status errors return `Err`.

Slot deletion and repair remain single typed calls using `SlotDeleteRequest` and
`SlotRepairRequest`; both select the target through their `slot` field.

Execution scheduling and slot repair/deletion also establish the relay before
sending. Firmware preparation and uploads attach when the Ark first requests
authorization, so unpaired updates and catalog uploads need no companion relay.
Transport ping/pong checks detect a dead connection independently of companion
authorization. Wire refusals and timeouts of individual forwarded requests leave
other exchanges running; only the Ark can seal an error response for the companion.

Connect handles relay traffic for a selected cloud without an application receive
loop. `Ark::recv` remains available for other incoming requests and explicit
handlers. Create the client and closer before moving the owner to that loop.
The returned wire `Responder` preserves reply completion:
`responder.reply(response, deadline)?.wait()?` checks that the adapter accepted
the output. It does not confirm delivery or processing by the peer.
The relay adapter is private. It maps wire's wrappers to the outer cloud
envelope and preserves encrypted bodies and request IDs. Presence and notification
envelopes have no wire input and are ignored. Connect does not interpret
companion authorization or expose a relay protocol API.

Wire automatically replies `UNKNOWN` to requests outside its schema.
Handlers receive known requests and can reply `UNSUPPORTED` for operations they
never serve, or `UNAVAILABLE` when the current state prevents serving them.
Use `schema::Error::reserved` with `schema::ReservedErrors` for those codes.
Application errors can implement the re-exported `CodedError` trait (codes
starting at 0x100) and pass directly to `Responder::fail`. Such refusals surface
as `Error::Remote` without ending the session. Dropping a responder still replies
`UNANSWERED`.

`Client::send` may wait for cloud prerequisites before using wire's unbounded
output queue. It then returns without waiting for output or a response; callers
manage the number of outstanding requests. `Pending::notify` lets one channel
observe many completions without one waiting thread per request.

USB retains the bounded transfer rings that overlap traffic on the bus.
WebSocket I/O has one protocol owner for binary data, Ping/Pong and Close,
driven by socket readiness with bounded buffers. Registry discovery uses an
HTTP client with a size limit and overall deadline; it disables proxies and
redirects for the local service.

This cleanup changes the library API: use `ark.client().call(Request { ... }, deadline)`
in place of per-operation convenience methods, an optional receive loop for
application handlers, and `Discovery::select` for endpoint
selection. `Device::kind()` reports the discovery classification; the
authenticated realm is available on `Identity`.

```sh
cargo test --workspace --all-features
cargo test -p darkbio-connect --no-default-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Tests use in-memory wire peers, loopback WebSocket/HTTP servers and simulated
USB transfer queues. They do not require a physical Ark.
