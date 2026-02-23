// ark: command line interface for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.

use crate::wire_protocol::{ArkToHost, HostToArk};

use prost::Message;
use std::io;
use std::io::{Read, Write};
use std::ops::Range;
use thiserror::Error;

/// Maximum frame size (2 MiB). Frames exceeding this are silently discarded.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

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
    FrameEncodingFailed(cobs::EncodeError),

    #[error("wire frame decode failed: {0}")]
    FrameDecodingFailed(cobs::DecodeError),

    #[error("wire send failed: {0}")]
    SendFailed(io::Error),

    #[error("wire receive failed: {0}")]
    RecvFailed(io::Error),

    #[error("wire terminated")]
    Terminated,
}

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
        }
    }

    /// Sends a session reset signal (two zero bytes) to the enclave. The first
    /// zero terminates any previously interrupted message, the second signals
    /// a fresh session start.
    pub fn reset_session(&mut self) -> Result<(), WireError> {
        self.writer
            .write_all(&[0x00, 0x00])
            .map_err(WireError::SendFailed)?;
        self.writer.flush().map_err(WireError::SendFailed)
    }

    /// Reads the next packet and decodes an ark-to-host response with protobuf.
    pub fn next_message(&mut self) -> Result<ArkToHost, WireError> {
        let size = self.next_packet()?;
        ArkToHost::decode(&self.decobs_buffer[..size]).map_err(WireError::PacketDecodingFailed)
    }

    /// Encodes a host-to-ark request with protobuf and injects it into the transport.
    pub fn send_message(&mut self, req: HostToArk) -> Result<(), WireError> {
        let len = req.encoded_len();
        if len > MAX_FRAME_SIZE {
            return Err(WireError::PacketTooLarge(len));
        }
        self.encode_buffer.clear();
        req.encode(&mut self.encode_buffer)
            .map_err(WireError::PacketEncodingFailed)?;

        self.send_packet(len)
    }

    /// next_packet reads the next frame and decodes the packet with the COBS
    /// algorithm to recover any zero bytes removed due to frame delimitation.
    #[inline]
    fn next_packet(&mut self) -> Result<usize, WireError> {
        let frame = self.next_frame()?;

        cobs::decode(
            &self.reader_buffer[frame.start..frame.end],
            &mut self.decobs_buffer,
        )
        .map_err(WireError::FrameDecodingFailed)
    }

    /// send_packet encodes the next packet with COBS to remove any 0 bytes in the
    /// data and sends it as a frame into the stream.
    #[inline]
    fn send_packet(&mut self, size: usize) -> Result<(), WireError> {
        let len = cobs::encode_buffer(size);
        if len > MAX_FRAME_SIZE {
            return Err(WireError::FrameTooLarge(len));
        }
        let size = cobs::encode(&self.encode_buffer[..size], &mut self.encobs_buffer)
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
