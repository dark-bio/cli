// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use crate::attests::DeviceAttestation;
use crate::pubkeys::{self, Release};
use crate::wire_protocol::{ArkToHost, HostToArk};

use darkbio_crypto::cbor::Cbor;
use darkbio_crypto::{cbor, cose, cwt, xdsa, xhpke};
use prost::Message;
use std::io;
use std::io::{Read, Write};
use std::ops::Range;
use thiserror::Error;

/// Maximum frame size (2 MiB). Frames exceeding this are silently discarded.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Cryptographic domain for the wire transport handshake.
const CRYPTO_DOMAIN_WIRE: &[u8] = b"wire-v1";

/// Cryptographic domain for device attestation CWTs.
const CRYPTO_DOMAIN_DEVICE_ATTESTATION: &[u8] = b"device-attestation-v1";

/// WireError describes things that can go wrong in the wire transport.
#[derive(Error, Debug)]
pub enum WireError {
    #[error("wire packet too large: {0} bytes, max {MAX_FRAME_SIZE} bytes")]
    PacketTooLarge(usize),

    #[error("wire packet encode failed: {0}")]
    PacketEncodingFailed(prost::EncodeError),

    #[error("wire packet decode failed: {0}")]
    PacketDecodingFailed(prost::DecodeError),

    #[error("wire frame too large: {0} bytes, max {MAX_FRAME_SIZE} bytes")]
    FrameTooLarge(usize),

    #[error("wire frame encode failed: {0}")]
    FrameEncodingFailed(darkbio_cobs::EncodeError),

    #[error("wire frame decode failed: {0}")]
    FrameDecodingFailed(darkbio_cobs::DecodeError),

    #[error("wire send failed: {0}")]
    SendFailed(io::Error),

    #[error("wire receive failed: {0}")]
    RecvFailed(io::Error),

    #[error("wire terminated")]
    Terminated,

    #[error("wire handshake failed: {0}")]
    HandshakeFailed(String),

    #[error("wire encryption failed: {0}")]
    EncryptionFailed(String),
}

// ---------- Handshake message types (must match firmware wire.rs exactly) ----------

/// Session initiation message from the host, containing its ephemeral keys.
#[derive(Cbor)]
#[cbor(array)]
struct HandshakeHostHello {
    host_signer: xdsa::PublicKey,  // Host's ephemeral xDSA signer key
    host_crypto: xhpke::PublicKey, // Host's ephemeral xHPKE encryption key
}

/// Session initiation acknowledgement from the Ark, containing the ephemeral
/// encryption key of the Ark, the encryption context for ark-to-host messaging
/// and the device's genuinity attestation.
#[derive(Cbor)]
#[cbor(array)]
struct HandshakeArkHello {
    ark_attest: Vec<u8>, // Ark's genuinity attestation (embeds the xDSA signer key)
    ark_crypto: xhpke::PublicKey, // Ark's ephemeral xHPKE encryption key
    a2h_encap: Vec<u8>,  // Ark-to-Host HPKE encryption context
}

/// Authentication data for the HandshakeArkHello message, binding the Ark's keys
/// to the request to ensure a MitM attacker is detected.
#[derive(Cbor)]
#[cbor(array)]
struct HandshakeArkHelloAuth {
    host_signer: xdsa::PublicKey,  // Host's ephemeral xDSA signer key
    host_crypto: xhpke::PublicKey, // Host's ephemeral xHPKE encryption key
}

/// Session acknowledgement from the host, containing the encryption context for
/// host-to-ark messaging.
#[derive(Cbor)]
#[cbor(array)]
struct HandshakeHostAck {
    h2a_encap: Vec<u8>, // Host-to-Ark HPKE encryption context
}

/// Authentication data for the HandshakeHostAck message, binding the Host's
/// HPKE encap key to the Ark's response to ensure a MitM attacker is detected.
#[derive(Cbor)]
#[cbor(array)]
struct HandshakeHostAckAuth {
    ark_signer: xdsa::PublicKey,  // Ark's permanent xDSA signer key
    ark_crypto: xhpke::PublicKey, // Ark's ephemeral xHPKE encryption key
}

// ---------- Session and trust types ----------

/// An active encrypted session between host and ark.
struct Session {
    sender: xhpke::Sender,     // Host->Ark encryption context
    receiver: xhpke::Receiver, // Ark->Host decryption context
}

/// Information about the Ark device extracted from the verified CWT during
/// session establishment.
pub struct SessionInfo {
    /// The Ark's verified identity public key.
    pub ark_identity: xdsa::PublicKey,
    /// Device serial number (from the CWT subject claim).
    pub serial: String,
    /// Hardware model identifier bytes (from the CWT hw_model claim).
    pub hw_model: Vec<u8>,
    /// Hardware version string (from the CWT hw_version claim).
    pub hw_version: String,
    /// Which root key tier signed the CWT, if any. None for self-signed.
    pub release: Option<Release>,
}

/// Controls which device attestation CWTs are accepted during the handshake.
pub enum TrustMode {
    /// Accept CWTs signed by a known root key or self-signed by the embedded key.
    /// Used for onboarding and development.
    RootOrSelf,

    /// Require the CWT to be signed by a known device root key.
    /// Used for production commands.
    RootOnly,

    /// Skip CWT verification entirely and use the provided public key as the
    /// Ark's identity. Used for recovering devices with corrupted/empty CWTs.
    Recover(xdsa::PublicKey),
}

// ---------- Wire transport ----------

/// Wire is a data stream, wrapped into all the necessary protocol features to be a
/// reliable transport between Ark enclaves and a client code:
///
///  - The data stream uses COBS framing, meaning 0 bytes are used as packet limit
///    markers and any actual 0-byte data is encoded away.
///     - https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing
///  - Empty packets are disallowed in normal operation and are used to signal the
///    start of a new session. This is needed to detect new clients, as the data
///    stream is not session based and is unaware of lifecycle events.
///    - Clients will send 2 zero packets. The first one may be needed to terminate
///      a previously interrupted message before signalling the session reset.
pub struct Wire<R: Read, W: Write> {
    reader: R, // Input stream for ingress data
    writer: W, // Output stream for egress data

    reader_buffer: Vec<u8>, // Buffer of received but not consumed data (fragments, multi-frames)
    reader_filled: usize,   // Number of meaningful bytes in the buffer
    reader_offset: usize,   // Offset of the not yet unconsumed data (multi-frames)
    reader_search: usize,   // Offset of unconsumed data already scanned for zeroes (fragments)

    decobs_buffer: Vec<u8>, // Buffer of COBS decoded bytes, but not protobuf consumed
    encobs_buffer: Vec<u8>, // Buffer of COBS encoded bytes to send to the framer
    encode_buffer: Vec<u8>, // Buffer of protobuf encoded bytes to send to the COBs encoder

    session: Option<Session>, // Active encrypted session (if handshake completed)
}

impl<R: Read, W: Write> Wire<R, W> {
    /// Creates a new wire stream around a low level reader and writer.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            reader_buffer: vec![0u8; MAX_FRAME_SIZE + 1],
            reader_offset: 0,
            reader_filled: 0,
            reader_search: 0,
            decobs_buffer: vec![0u8; MAX_FRAME_SIZE],
            encobs_buffer: vec![0u8; MAX_FRAME_SIZE],
            encode_buffer: vec![0u8; MAX_FRAME_SIZE],
            session: None,
        }
    }

    /// Sends a session reset and drives the 1.5 RTT encrypted handshake with
    /// the Ark. Returns session info including the verified device identity.
    ///
    ///   1. Host -> Ark:  HandshakeHostHello  { host_xdsa, host_xhpke }          (plain CBOR)
    ///   2. Ark  -> Host: HandshakeArkHello   { ark_cwt, ark_xhpke, a2h_encap }  (cose::seal)
    ///   3. Host -> Ark:  HandshakeHostAck    { h2a_encap }                       (cose::seal)
    pub fn establish_session(&mut self, trust: TrustMode) -> Result<SessionInfo, WireError> {
        self.session = None;

        // Send two zero bytes: first terminates any interrupted message, second
        // signals a fresh session.
        self.writer
            .write_all(&[0x00, 0x00])
            .map_err(WireError::SendFailed)?;
        self.writer.flush().map_err(WireError::SendFailed)?;

        // Generate ephemeral host keys for this session
        let host_xdsa_sk = xdsa::SecretKey::generate();
        let host_xdsa_pk = host_xdsa_sk.public_key();
        let host_xhpke_sk = xhpke::SecretKey::generate();
        let host_xhpke_pk = host_xhpke_sk.public_key();

        // Message 1: Send HostHello (plain CBOR, COBS-framed)
        let hello = cbor::encode(&HandshakeHostHello {
            host_signer: host_xdsa_pk.clone(),
            host_crypto: host_xhpke_pk.clone(),
        })
        .map_err(|err| {
            WireError::HandshakeFailed(format!("failed to encode host hello: {}", err))
        })?;

        self.encode_buffer.clear();
        self.encode_buffer.extend_from_slice(&hello);
        self.send_packet(hello.len())?;

        // Message 2: Read ArkHello (COSE seal'd, COBS-framed)
        let size = self.next_packet()?;

        let auth = HandshakeArkHelloAuth {
            host_signer: host_xdsa_pk.clone(),
            host_crypto: host_xhpke_pk.clone(),
        };

        // Step 2a: Decrypt the outer COSE_Encrypt0 layer
        let sign1 = cose::decrypt(
            &self.decobs_buffer[..size],
            &auth,
            &host_xhpke_sk,
            CRYPTO_DOMAIN_WIRE,
        )
        .map_err(|err| {
            WireError::HandshakeFailed(format!("failed to decrypt ark hello: {}", err))
        })?;

        // Step 2b: Peek at the unverified payload to discover the Ark's identity
        let unverified: HandshakeArkHello = cose::peek(&sign1).map_err(|err| {
            WireError::HandshakeFailed(format!("invalid ark hello payload: {}", err))
        })?;

        // Steps 2c+2d: Verify the Ark's identity and COSE_Sign1 signature.
        //
        // In normal modes we extract the identity from the CWT attestation and
        // verify the signature against it. In recovery mode the caller supplies
        // the Ark's public key directly (the CWT may be corrupted / empty).
        let (session_info, ark_hello) = match &trust {
            TrustMode::Recover(ark_identity) => {
                // Recovery: caller-provided key, verify signature but skip CWT.
                let hello: HandshakeArkHello =
                    cose::verify(&sign1, &auth, ark_identity, CRYPTO_DOMAIN_WIRE, None).map_err(
                        |err| {
                            WireError::HandshakeFailed(format!(
                                "ark hello signature invalid: {}",
                                err
                            ))
                        },
                    )?;
                let info = SessionInfo {
                    ark_identity: ark_identity.clone(),
                    serial: String::new(),
                    hw_model: Vec::new(),
                    hw_version: String::new(),
                    release: None,
                };
                (info, hello)
            }
            _ => {
                // Normal: verify CWT, then verify COSE_Sign1 with discovered key.
                let info = verify_cwt(&unverified.ark_attest, &trust)?;
                let hello: HandshakeArkHello =
                    cose::verify(&sign1, &auth, &info.ark_identity, CRYPTO_DOMAIN_WIRE, None)
                        .map_err(|err| {
                            WireError::HandshakeFailed(format!(
                                "ark hello signature invalid: {}",
                                err
                            ))
                        })?;
                (info, hello)
            }
        };

        // Set up the Ark->Host receiver context
        let enc_a2h: [u8; xhpke::ENCAP_KEY_SIZE] = ark_hello
            .a2h_encap
            .try_into()
            .map_err(|_| WireError::HandshakeFailed("invalid a2h_encap size".into()))?;

        let receiver = host_xhpke_sk
            .new_receiver(&enc_a2h, b"wire-v1:ark-to-host")
            .map_err(|err| {
                WireError::HandshakeFailed(format!("host receiver setup failed: {}", err))
            })?;

        // Set up the Host->Ark sender context
        let ark_xhpke_pk = ark_hello.ark_crypto;
        let (sender, enc_h2a) = ark_xhpke_pk
            .new_sender(b"wire-v1:host-to-ark")
            .map_err(|err| {
                WireError::HandshakeFailed(format!("host sender setup failed: {}", err))
            })?;

        // Message 3: Send HostAck (COSE seal'd, COBS-framed)
        let ack = cose::seal(
            &HandshakeHostAck {
                h2a_encap: enc_h2a.to_vec(),
            },
            &HandshakeHostAckAuth {
                ark_signer: session_info.ark_identity.clone(),
                ark_crypto: ark_xhpke_pk.clone(),
            },
            &host_xdsa_sk,
            &ark_xhpke_pk,
            CRYPTO_DOMAIN_WIRE,
        )
        .map_err(|err| WireError::HandshakeFailed(format!("failed to seal host ack: {}", err)))?;

        self.encode_buffer.clear();
        self.encode_buffer.extend_from_slice(&ack);
        self.send_packet(ack.len())?;

        // Session established
        self.session = Some(Session { sender, receiver });
        Ok(session_info)
    }

    /// Reads the next packet and decodes an ark-to-host response with protobuf.
    pub fn next_message(&mut self) -> Result<ArkToHost, WireError> {
        let size = self.next_packet()?;

        // Decrypt the message
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| WireError::EncryptionFailed("no active session".into()))?;

        let blob = session
            .receiver
            .open(&self.decobs_buffer[..size], &[])
            .map_err(|err| WireError::EncryptionFailed(format!("decryption failed: {}", err)))?;

        ArkToHost::decode(&blob[..]).map_err(WireError::PacketDecodingFailed)
    }

    /// Encodes a host-to-ark request with protobuf and injects it into the transport.
    pub fn send_message(&mut self, req: HostToArk) -> Result<(), WireError> {
        // Encode with protobuf
        let len = req.encoded_len();
        if len > MAX_FRAME_SIZE {
            return Err(WireError::PacketTooLarge(len));
        }
        self.encode_buffer.clear();
        req.encode(&mut self.encode_buffer)
            .map_err(WireError::PacketEncodingFailed)?;

        // Encrypt the encoded message
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| WireError::EncryptionFailed("no active session".into()))?;

        let blob = session
            .sender
            .seal(&self.encode_buffer, &[])
            .map_err(|err| WireError::EncryptionFailed(err.to_string()))?;

        let len = blob.len();
        if len > MAX_FRAME_SIZE {
            return Err(WireError::PacketTooLarge(len));
        }
        self.encode_buffer.clear();
        self.encode_buffer.extend_from_slice(&blob);

        self.send_packet(len)
    }

    // ----- Transport layer (unchanged from the plaintext version) -----

    /// next_packet reads the next frame and decodes the packet with the COBS
    /// algorithm to recover any zero bytes removed due to frame delimitation.
    #[inline]
    fn next_packet(&mut self) -> Result<usize, WireError> {
        let frame = self.next_frame()?;

        darkbio_cobs::decode(
            &self.reader_buffer[frame.start..frame.end],
            &mut self.decobs_buffer,
        )
        .map_err(WireError::FrameDecodingFailed)
    }

    /// send_packet encodes the next packet with COBS to remove any 0 bytes in the
    /// data and sends it as a frame into the stream.
    #[inline]
    fn send_packet(&mut self, size: usize) -> Result<(), WireError> {
        let len = darkbio_cobs::encode_buffer(size);
        if len > MAX_FRAME_SIZE {
            return Err(WireError::FrameTooLarge(len));
        }
        let size = darkbio_cobs::encode(&self.encode_buffer[..size], &mut self.encobs_buffer)
            .map_err(WireError::FrameEncodingFailed)?;

        self.send_frame(size)
    }

    /// Reads the next 0-bounded frame from the transport. If the frame is longer
    /// than allowed, data is silently discarded until a valid frame is found
    /// again. The method returns the indices into the `reader_buffer` slice to
    /// allow subsequent parsing with zero-copy.
    ///
    /// # Safety
    /// Uses unchecked indexing. Safety guaranteed by fixed-size reader_buffer
    /// (MAX_FRAME_SIZE + 1) and bounds tracking via reader_offset/reader_filled.
    #[inline]
    fn next_frame(&mut self) -> Result<Range<usize>, WireError> {
        let mut discard = 0usize;

        'outer: loop {
            // Search for the frame delimiter, starting from where we left off
            while self.reader_search < self.reader_filled {
                if unsafe { *self.reader_buffer.get_unchecked(self.reader_search) } == 0 {
                    let start = self.reader_offset;
                    let end = self.reader_search;

                    self.reader_offset = end + 1;
                    self.reader_search = end + 1;

                    // If we were in discard mode, report, throw away and start over
                    if discard > 0 {
                        eprintln!(
                            "warning: discarded oversized frame of {} bytes",
                            discard + end - start
                        );
                        discard = 0;
                        continue 'outer;
                    }
                    return Ok(Range { start, end });
                }
                self.reader_search += 1;
            }
            // Frame delimiter not found, we only have fragments
            if discard == 0 {
                if self.reader_offset > 0 {
                    let used = self.reader_filled - self.reader_offset;
                    unsafe {
                        std::ptr::copy(
                            self.reader_buffer.as_ptr().add(self.reader_offset),
                            self.reader_buffer.as_mut_ptr(),
                            used,
                        );
                    }
                    self.reader_filled = used;
                    self.reader_offset = 0;
                    self.reader_search = used;
                }
            } else {
                discard += self.reader_filled;
                self.reader_filled = 0;
                self.reader_offset = 0;
                self.reader_search = 0
            }
            // Buffer full without a delimiter means oversized frame, discard all.
            if self.reader_filled == MAX_FRAME_SIZE + 1 {
                discard += MAX_FRAME_SIZE + 1;
                self.reader_filled = 0;
                self.reader_offset = 0;
                self.reader_search = 0
            }
            // Read more data to try and find the next frame marker
            match self
                .reader
                .read(unsafe { self.reader_buffer.get_unchecked_mut(self.reader_filled..) })
            {
                Err(err) => return Err(WireError::RecvFailed(err)),
                Ok(0) => return Err(WireError::Terminated),
                Ok(n) => self.reader_filled += n,
            }
        }
    }

    /// Writes the next 0-bounded frame into the transport. The zero character
    /// will be injected automatically after the data. The input data is taken
    /// from the encobs_buffer.
    #[inline]
    fn send_frame(&mut self, size: usize) -> Result<(), WireError> {
        (|| -> io::Result<()> {
            self.writer.write_all(&self.encobs_buffer[..size])?;
            self.writer.write_all(&[0u8])?;
            self.writer.flush()?;
            Ok(())
        })()
        .map_err(WireError::SendFailed)
    }
}

/// Verifies a device attestation CWT according to the requested trust mode.
///
/// Tries each known device root key first. If no root key's fingerprint matches
/// the CWT signer, falls back to self-signed verification (if permitted by the
/// trust mode). Returns session info with the verified identity and device metadata.
fn verify_cwt(cwt_bytes: &[u8], trust: &TrustMode) -> Result<SessionInfo, WireError> {
    // Extract the signer fingerprint from the CWT without verifying
    let signer_fp = cwt::signer(cwt_bytes)
        .map_err(|err| WireError::HandshakeFailed(format!("failed to read CWT signer: {}", err)))?;

    // Try each known root key until we find one whose fingerprint matches
    for release in [Release::Release, Release::Staging, Release::Develop] {
        for root_key in pubkeys::device_root_keys(release) {
            if root_key.fingerprint() != signer_fp {
                continue;
            }
            // Fingerprint matches, verify the CWT with this root key
            let attestation: DeviceAttestation =
                cwt::verify(cwt_bytes, &root_key, CRYPTO_DOMAIN_DEVICE_ATTESTATION, None).map_err(
                    |err| WireError::HandshakeFailed(format!("CWT verification failed: {}", err)),
                )?;

            return Ok(SessionInfo {
                ark_identity: attestation.cnf.key().clone(),
                serial: attestation.sub.sub,
                hw_model: attestation.hwm.hw_model,
                hw_version: attestation.hwv.version().to_string(),
                release: Some(release),
            });
        }
    }
    // No root key matched. Check for self-signed if the trust mode allows it.
    // Recover mode never reaches here (handled by establish_session directly).
    match trust {
        TrustMode::RootOnly | TrustMode::Recover(_) => Err(WireError::HandshakeFailed(
            "device CWT not signed by a known root key".into(),
        )),
        TrustMode::RootOrSelf => {
            // Peek at the attestation to get the embedded key
            let attestation: DeviceAttestation = cwt::peek(cwt_bytes).map_err(|err| {
                WireError::HandshakeFailed(format!("failed to peek CWT: {}", err))
            })?;

            let embedded_key = attestation.cnf.key().clone();

            // Verify the signer fingerprint matches the embedded key
            if embedded_key.fingerprint() != signer_fp {
                return Err(WireError::HandshakeFailed(
                    "CWT signer does not match embedded key (not root-signed or self-signed)"
                        .into(),
                ));
            }
            // Verify the CWT is actually signed by the embedded key
            let _: DeviceAttestation = cwt::verify(
                cwt_bytes,
                &embedded_key,
                CRYPTO_DOMAIN_DEVICE_ATTESTATION,
                None,
            )
            .map_err(|err| {
                WireError::HandshakeFailed(format!("self-signed CWT verification failed: {}", err))
            })?;

            Ok(SessionInfo {
                ark_identity: embedded_key,
                serial: attestation.sub.sub,
                hw_model: attestation.hwm.hw_model,
                hw_version: attestation.hwv.version().to_string(),
                release: None,
            })
        }
    }
}
