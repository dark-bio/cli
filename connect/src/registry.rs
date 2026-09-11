// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.

//! The emulators running on the host, as the listing their launchers keep
//! serves them. Whichever launcher holds the port serves it and the others
//! publish themselves into it, so nobody serving it means no emulator runs.
//! The listing is read with one request spoken directly over TCP, a fixed
//! route against a loopback server being short of what an HTTP client is for.

use crate::Error;
use darkbio_trust::Environment;
use serde::Deserialize;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

/// Address the launchers serve the listing on, one port below the first an
/// emulator takes.
const ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18180);

/// How long the lookup may take, a loopback service answering from memory
/// or not at all.
const TIMEOUT: Duration = Duration::from_secs(1);

/// Largest listing read, a few hundred bytes per emulator. Anything beyond
/// it is not one.
const MAX_LISTING: u64 = 1024 * 1024;

/// A running emulator as the listing describes it. A claim the device has
/// not made yet is absent rather than empty.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Instance {
    pub port: u16, // Host port the emulator forwards into its guest, and what to connect to
    #[serde(default)]
    pub disk: String, // File name of the disk image, never its path
    #[serde(default)]
    pub ready: bool, // Whether the firmware booted far enough to accept a client
    #[serde(default)]
    pub env: Option<String>, // Environment the device is bound to, once it has said
    #[serde(default)]
    pub name: Option<String>, // Name the device was given, if any
    #[serde(default)]
    pub serial: Option<String>, // Serial the device reports, once onboarded
}

impl Instance {
    /// Wire endpoint of the emulator, the listing publishing a port rather
    /// than a URL.
    pub fn url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/usb", self.port)
    }

    /// Environment the device is bound to, as the listing names it, if the
    /// build knows of that environment at all.
    pub fn environment(&self) -> Option<Environment> {
        match self.env.as_deref()? {
            #[cfg(feature = "develop")]
            "develop" => Some(Environment::Develop),
            #[cfg(feature = "staging")]
            "staging" => Some(Environment::Staging),
            #[cfg(feature = "release")]
            "release" => Some(Environment::Release),
            _ => None,
        }
    }
}

/// The listing as served, its entries left for a lenient read one by one.
#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    instances: Vec<serde_json::Value>, // Entries as served, read one by one
}

/// Lists the emulators running on the host. Nobody serving the listing means
/// none is running.
pub(crate) fn list() -> Result<Vec<Instance>, Error> {
    list_at(ADDRESS.into())
}

/// Lists the emulators the service at the address knows of. A refused
/// connection is an empty listing, anything else failing is an error.
fn list_at(addr: SocketAddr) -> Result<Vec<Instance>, Error> {
    let body = match fetch(addr) {
        Ok(body) => body,
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => return Ok(Vec::new()),
        Err(err) => return Err(Error::Registry(err)),
    };
    let listing: Listing = serde_json::from_slice(&body)
        .map_err(|err| Error::Registry(io::Error::new(io::ErrorKind::InvalidData, err)))?;

    // The entries are read one by one, one this build cannot make sense of
    // dropped rather than failing a listing served by another build
    Ok(listing
        .instances
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect())
}

/// Requests the listing and returns its body, any status but success being
/// an error.
fn fetch(addr: SocketAddr) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let request =
        format!("GET /v1/instances HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    // The connection closes after the response, so it ends at EOF without
    // any framing to interpret
    let mut raw = Vec::new();
    stream.take(MAX_LISTING).read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "listing without headers"))?;
    let status = std::str::from_utf8(&raw[..split])
        .ok()
        .and_then(|head| head.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "listing without a status"))?;
    if !(200..300).contains(&status) {
        return Err(io::Error::other(format!(
            "listing refused with status {status}"
        )));
    }
    Ok(raw[split + 4..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    // Binds a listener on a free loopback port for a test's service.
    fn bind() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    // Serves one request on the listener with the canned response, on a
    // thread of its own, reading the whole request before answering it so
    // the client never writes into a closed socket.
    fn serve(listener: TcpListener, response: &'static str) {
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

    // Tests that a listing is read leniently, the claims present taken, the
    // absent ones left out, and the entries this build cannot make sense of
    // dropped rather than failing the listing.
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
        assert!(instances[0].ready);
        #[cfg(feature = "develop")]
        assert_eq!(instances[0].environment(), Some(Environment::Develop));
        #[cfg(not(feature = "develop"))]
        assert_eq!(instances[0].environment(), None);
        assert_eq!(instances[0].name.as_deref(), Some("test ark"));
        assert_eq!(instances[0].serial.as_deref(), Some("abc123"));
        assert_eq!(instances[0].url(), "ws://127.0.0.1:18182/v1/usb");
        assert_eq!(instances[1].disk, "ark-a.img");
        assert!(!instances[1].ready);
        assert_eq!(instances[1].environment(), None);
        assert_eq!(instances[1].name, None);
    }

    // Tests that nobody serving the listing is an empty one rather than a
    // failure.
    #[test]
    fn test_nobody_serving() {
        let (listener, addr) = bind();
        drop(listener);
        assert!(list_at(addr).unwrap().is_empty());
    }

    // Tests that a service answering with anything but a listing fails the
    // lookup, a refusal, a body of another shape or no answer at all.
    #[test]
    fn test_refusals() {
        let (listener, addr) = bind();
        serve(listener, "HTTP/1.1 404 Not Found\r\n\r\nno such route");
        assert!(matches!(list_at(addr), Err(Error::Registry(_))));

        let (listener, addr) = bind();
        serve(listener, "HTTP/1.1 200 OK\r\n\r\nnot a listing");
        assert!(matches!(list_at(addr), Err(Error::Registry(_))));

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
