// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Emulator discovery through the local launcher registry.
//!
//! The launcher holding the registry port serves entries from all launchers.
//! A refused connection means no registry is running. HTTP framing and the
//! overall request deadline belong to the client; redirects and proxies are
//! disabled for this loopback service.

use crate::Error;
use serde::Deserialize;
use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

/// Loopback address of the registry, one port below the first emulator endpoint.
const ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18180);

/// Overall timeout for fetching a listing from the local registry.
///
/// Windows retries refused loopback connections before reporting the error.
const TIMEOUT: Duration = Duration::from_secs(if cfg!(windows) { 5 } else { 1 });

/// Largest accepted listing body, before JSON decoding.
const MAX_LISTING: u64 = 1024 * 1024;

/// Emulator endpoint and metadata published by its launcher.
///
/// Optional reports remain absent until the launcher supplies them.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Instance {
    /// Host port forwarded to the guest's WebSocket endpoint.
    pub port: u16,
    /// File name of the disk image, which several emulators may share.
    #[serde(default)]
    pub disk: String,
    /// Readiness to accept clients, as the firmware reports it.
    #[serde(default)]
    pub ready: Option<bool>,
    /// Cloud environment the device reports.
    #[serde(default)]
    pub env: Option<String>,
    /// Device name, if reported.
    #[serde(default)]
    pub name: Option<String>,
    /// Device serial, if reported.
    #[serde(default)]
    pub serial: Option<String>,
}

impl Instance {
    /// Builds the loopback WebSocket URL from the published host port.
    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/usb", self.port)
    }
}

/// Listing version this build understands.
///
/// A breaking change to the registry bumps it, so a newer registry is refused
/// rather than misread.
const VERSION: u64 = 1;

/// Registry response with entries retained for individual decoding.
#[derive(Deserialize)]
struct Listing {
    /// Schema version of the listing, absent when the body carries none.
    #[serde(default)]
    version: Option<u64>,
    /// Raw entries, decoded one by one so an unreadable entry drops alone.
    #[serde(default)]
    instances: Vec<serde_json::Value>,
}

/// Lists emulators from the local registry.
///
/// An absent registry returns an empty list.
pub(super) fn list() -> Result<Vec<Instance>, Error> {
    list_at(ADDRESS.into())
}

/// Fetches a registry at the supplied address.
///
/// Only connection refusal is treated as an empty listing; transport, HTTP and
/// decoding failures remain errors.
fn list_at(addr: SocketAddr) -> Result<Vec<Instance>, Error> {
    // Nobody listening means no registry is running
    let body = match fetch(addr) {
        Ok(body) => body,
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => return Ok(Vec::new()),
        Err(err) => return Err(Error::Registry(err)),
    };

    // Decode the listing, refusing a version this build does not know
    let listing: Listing = serde_json::from_slice(&body)
        .map_err(|err| Error::Registry(io::Error::new(io::ErrorKind::InvalidData, err)))?;
    if listing.version != Some(VERSION) {
        return Err(Error::Registry(io::Error::new(
            io::ErrorKind::InvalidData,
            match listing.version {
                Some(version) => format!("emulator registry version {version} is not supported"),
                None => "emulator registry listing carries no version".to_string(),
            },
        )));
    }

    // Skip entries this build cannot decode without losing compatible entries
    // from the same registry response
    Ok(listing
        .instances
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect())
}

/// Fetches a successful HTTP response under the registry's deadline and size limit.
fn fetch(addr: SocketAddr) -> io::Result<Vec<u8>> {
    // Ask the loopback registry directly, never through a proxy or redirect
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let mut response = agent
        .get(format!("http://{addr}/v1/instances"))
        .call()
        .map_err(|err| match err {
            ureq::Error::Io(err) => err,
            err => io::Error::other(err),
        })?;

    // Only a successful status carries a listing
    if !response.status().is_success() {
        return Err(io::Error::other(format!(
            "listing refused with status {}",
            response.status()
        )));
    }

    // One extra byte distinguishes a complete body from a truncated oversized one
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_LISTING + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_LISTING {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "emulator listing too large",
        ));
    }
    Ok(body)
}

/// Registry listing, framing, size limit and refusal regressions against local
/// listeners.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Binds a registry listener on an available loopback port.
    fn bind() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// Serves one HTTP request with the supplied response, on its own thread.
    ///
    /// It reads the complete request first so the client does not write into a
    /// closed socket.
    fn serve(listener: TcpListener, response: impl Into<String>) {
        let response = response.into();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = stream.write_all(response.as_bytes());
        });
    }

    /// A listing is read leniently, taking the claims present, leaving absent
    /// ones out and dropping entries this build cannot read.
    #[test]
    fn test_listing() {
        let (listener, addr) = bind();
        serve(
            listener,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
             {\"version\":1,\"instances\":[\
             {\"port\":18182,\"disk\":\"ark-b.img\",\"disk_id\":\"00902e5b\",\"ready\":true,\
             \"env\":\"develop\",\"name\":\"test ark\",\"serial\":\"abc123\",\"expiry\":1},\
             {\"port\":18181,\"disk\":\"ark-a.img\"},\
             {\"disk\":\"lost.img\"},7]}",
        );
        let instances = list_at(addr).unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].port, 18182);
        assert_eq!(instances[0].ready, Some(true));
        assert_eq!(instances[0].env.as_deref(), Some("develop"));
        assert_eq!(instances[0].name.as_deref(), Some("test ark"));
        assert_eq!(instances[0].serial.as_deref(), Some("abc123"));
        assert_eq!(instances[0].url(), "ws://127.0.0.1:18182/v1/usb");
        assert_eq!(instances[1].disk, "ark-a.img");
        assert_eq!(instances[1].ready, None);
        assert_eq!(instances[1].env, None);
        assert_eq!(instances[1].name, None);
    }

    /// Nobody serving the listing yields an empty one rather than a failure.
    #[test]
    fn test_nobody_serving() {
        let (listener, addr) = bind();
        drop(listener);
        assert!(list_at(addr).unwrap().is_empty());
    }

    /// HTTP chunk framing is removed before the registry body is decoded.
    #[test]
    fn test_chunked_listing() {
        let (listener, addr) = bind();
        let parts = ["{\"version\":1,\"instances\":[", "{\"port\":18181}]}"];
        let mut response = String::from("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        for part in parts {
            response.push_str(&format!("{:x}\r\n{part}\r\n", part.len()));
        }
        response.push_str("0\r\n\r\n");
        serve(listener, response);
        assert_eq!(list_at(addr).unwrap()[0].port, 18181);
    }

    /// A body exceeding the registry size limit is refused before JSON decoding.
    #[test]
    fn test_listing_limit() {
        let (listener, addr) = bind();
        let body = " ".repeat(MAX_LISTING as usize + 1);
        serve(
            listener,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(
            matches!(list_at(addr), Err(Error::Registry(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
    }

    /// A listing of an unknown version, or of none at all, is refused rather
    /// than read.
    #[test]
    fn test_unknown_version_is_refused() {
        for body in [
            "{\"version\":2,\"instances\":[{\"port\":18181}]}",
            "{\"instances\":[{\"port\":18181}]}",
        ] {
            let (listener, addr) = bind();
            serve(listener, format!("HTTP/1.1 200 OK\r\n\r\n{body}"));
            assert!(
                matches!(list_at(addr), Err(Error::Registry(error)) if error.kind() == io::ErrorKind::InvalidData),
                "{body}"
            );
        }
    }

    /// A refusing status, a body of another shape or no answer at all fails
    /// the lookup.
    #[test]
    fn test_refusals() {
        // A refusing status fails the lookup
        let (listener, addr) = bind();
        serve(listener, "HTTP/1.1 404 Not Found\r\n\r\nno such route");
        assert!(matches!(list_at(addr), Err(Error::Registry(_))));

        // A body of another shape fails decoding
        let (listener, addr) = bind();
        serve(listener, "HTTP/1.1 200 OK\r\n\r\nnot a listing");
        assert!(matches!(list_at(addr), Err(Error::Registry(_))));

        // A service that never answers runs out the registry's timeout
        let (listener, addr) = bind();
        let (release, held) = mpsc::channel::<()>();
        let stalled = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = held.recv();
            drop(stream);
        });
        assert!(matches!(list_at(addr), Err(Error::Registry(_))));
        release.send(()).unwrap();
        stalled.join().unwrap();
    }
}
