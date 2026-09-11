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
cargo run --features internal -- onboard --device emulator:18181 --cwt device.cwt
```

`ark genuine` checks an attested Ark against the cloud device registry. It
automatically synchronizes the Ark's cloud keys and clock before requesting
the genuinity proof. Cloud synchronization and proof verification share a
30-second budget. The verified environment selects the cloud; the verified
realm selects the hardware or emulator registry. The result reports the serial,
enrollment time and any disabled, expired or superseded state. An inactive
registration or a failed check exits with status 3.

Connect owns cloud setup, which is lazy and tied to the connection. Requests
declare whether they need it through `Request::CLOUD_SYNC`. Client clones share
one setup attempt and reuse a completed sync; failures allow a later retry.
`status` and `onboard` do not contact the cloud. Self-signed and recovery
connections skip cloud setup; a genuinity check
requires an attested identity. Relay setup will be added with operations that
need it.

Onboarding reads the certificate before connecting and reconnects to the same
endpoint for status. Failure of that follow-up check is a warning after
successful onboarding. Exit codes are 0 for success, 1 for local input or
selection failures, 2 for argument parsing, and 3 for connection or request
failures.

Status reports what the handshake established: a trusted certificate, a
self-signed identity, or a key pinned with `--pubkey`. Recovery does not check
attestation; self-signing does not establish provisioning history. The CLI
trusts the release, staging and develop roots. Discovery metadata and endpoint
locators are not authenticated identities, and locators can change or be
reused after a device reconnects.

The `connect` workspace crate is the reusable library. It owns discovery,
trust policy, transport adapters and cloud operations; `darkbio-wire` owns the
encrypted protocol, multiplexing, worker threads and session lifecycle. `list()` discovers both
kinds of Ark and retains independent discovery failures. `hardware::list()`
and `emulator::list()` each return `Result<Vec<Device>, Error>` for one kind.
Devices from either list connect and authenticate through `Device::connect`.
Custom verifiers return `Identity` to establish cloud routing along with trust.
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
setup waiters; an HTTP request already in progress retains its deadline.

Operations requiring companion authorization need the application to receive
and service requests through `Ark::recv` concurrently with outgoing calls.
Create the client and closer before moving the owner to the receive loop.
The returned wire `Responder` preserves reply completion:
`responder.reply(response, deadline)?.wait()?` checks that the adapter accepted
the output. It does not confirm delivery or processing by the peer.
The application owns handler dispatch, shutdown and any cloud relay integration;
the CLI exposes status, onboarding and cloud genuinity checks.

Wire 0.7 automatically replies `UNKNOWN` to requests outside its schema.
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
in place of per-operation convenience methods, an explicit receive loop in
place of request/disconnect callbacks, and `Discovery::select` for endpoint
selection. `Device::kind()` reports the discovery classification; the
authenticated realm is available on `Identity`.

```sh
cargo test --workspace --all-features
cargo test -p darkbio-connect --no-default-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Tests use in-memory wire peers, loopback WebSocket/HTTP servers and simulated
USB transfer queues. They do not require a physical Ark.
