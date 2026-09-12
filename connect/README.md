# darkbio-connect

Authenticated connections to Dark Bio Arks from Rust. Discovery returns hardware
and local emulators through one `Device` API. `Ark` owns a session; clonable
`Client` handles issue typed requests without keeping it open.

Cloud synchronization and companion relay attachment happen when a request needs
them. Pairing, dataset upload, firmware installation and app execution are
protocol workflows with progress callbacks. Each operation accepts an absolute
deadline, an inactivity allowance, or both through `Timing`; no timeout is stored
on the client.

Device-info responses supply the sync marker and clock for lazy setup. A cloud
proof rejected with HTTP 403 triggers one forced sync and authentication retry
with a new proof. Established pairing exchanges, approvals and transfers are
never replayed by this retry.
Firmware proof rejection refreshes keys and returns an error; the caller starts
the next update attempt explicitly because preparation can require approval.

Callers supply dataset and firmware readers. Downloading, caching, firmware
selection, prompts and signal handlers belong to the application.

The crate defaults to no built-in trust roots. Enable `release`, `staging` or
`develop` for the environments the application trusts. Cloud routing is explicit
or established by attestation; connect does not infer it from launcher metadata.

See the crate documentation for API examples and ownership semantics.
