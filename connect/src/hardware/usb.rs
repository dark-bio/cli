// connect-rs: connections to Ark enclaves from host processes
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Arks over USB, the wire's byte stream on the bulk endpoints of the vendor
//! interface a plugged in Ark enumerates with. The host claims the interface
//! exclusively, so an Ark held by another program, a browser tab included,
//! cannot be opened until that lets go.
//!
//! Each direction is a ring of transfers queued with the system, the shape
//! the firmware drives its own end of the bus with: transfers of 64 KiB,
//! sixteen in flight, a zero length packet closing a frame that ended on a
//! packet boundary, and flushes that take back the transfers already finished
//! rather than drain the ring, so consecutive frames overlap on the bus.
//! Every wait is bounded by the deadline the wire installed and ends early
//! once the connection is closed.

use crate::ark::Ark;
use crate::{Error, wire};
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, Completion, Direction, EndpointDirection, In, Out, TransferError,
};
use nusb::{ErrorKind, MaybeFuture};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;
use wire::transport::{self, Verifier};

/// Class, subclass and protocol of the vendor interface carrying the wire. It
/// tells the interface from the mass storage a development Ark exposes too,
/// which is bulk in both directions as well.
const VENDOR_INTERFACE: (u8, u8, u8) = (0xff, 1, 2);

/// Separator between the parts of the product string an Ark enumerates
/// with, its carrier, its revision and the name it was given, the last there
/// only when a name was given.
const PRODUCT_SEPARATOR: &str = " \u{00b7} ";

/// Bytes per transfer in either direction, the chunk the firmware moves over
/// the bus and the size the bench found saturating it. A multiple of the
/// packet size of any bulk endpoint.
const TRANSFER_SIZE: usize = 64 * 1024;

/// Transfers kept in flight per direction, the queue the firmware keeps on
/// its end of the bus and the depth the bench found saturating it.
const TRANSFERS: usize = 16;

/// Name the Ark was given, carried in its product string after the carrier
/// and the revision.
pub(crate) fn name(product: &str) -> Option<&str> {
    product
        .splitn(3, PRODUCT_SEPARATOR)
        .nth(2)
        .filter(|name| !name.is_empty())
}

/// Opens the Ark and runs the wire handshake over it, the verifier deciding
/// whether to trust the attestation it presents.
pub(crate) fn connect<V: Verifier<Info = crate::Identity>>(
    info: &nusb::DeviceInfo,
    verifier: &V,
    cloud: Option<(crate::trust::Environment, crate::trust::Realm)>,
) -> Result<(Ark, V::Info), Error> {
    let device = info.open().wait().map_err(Error::Usb)?;
    let config = device
        .active_configuration()
        .map_err(|err| Error::Usb(err.into()))?;

    // Find the vendor interface with bulk endpoints in both directions
    let mut found = None;
    'search: for group in config.interfaces() {
        for alt in group.alt_settings() {
            if (alt.class(), alt.subclass(), alt.protocol()) != VENDOR_INTERFACE {
                continue;
            }
            let mut ep_in = None;
            let mut ep_out = None;
            for ep in alt.endpoints() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    Direction::In => ep_in = ep_in.or(Some(ep.address())),
                    Direction::Out => ep_out = ep_out.or(Some(ep.address())),
                }
            }
            if let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) {
                found = Some((
                    group.interface_number(),
                    alt.alternate_setting(),
                    ep_in,
                    ep_out,
                ));
                break 'search;
            }
        }
    }
    let (number, alternate, ep_in, ep_out) = found.ok_or(Error::Unsupported)?;

    // Claim the interface and open the endpoints, a claim refused for the
    // device being held meaning another program has it. The endpoints keep
    // the interface claimed and the device open for as long as either lives.
    let iface = device
        .claim_interface(number)
        .wait()
        .map_err(|err| match err.kind() {
            ErrorKind::Busy => Error::Busy(err),
            _ => Error::Usb(err),
        })?;
    if iface.get_alt_setting() != alternate {
        iface
            .set_alt_setting(alternate)
            .wait()
            .map_err(Error::Usb)?;
    }
    let ep_in = iface.endpoint::<Bulk, In>(ep_in).map_err(Error::Usb)?;
    let ep_out = iface.endpoint::<Bulk, Out>(ep_out).map_err(Error::Usb)?;

    // Wrap the endpoints into the wire's reader and writer, each woken by
    // its own transfers finishing and by the close
    let closed = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(Notifier::default());
    let writes = Arc::new(Notifier::default());
    let reader = Reader::new(ep_in, reads.clone(), closed.clone());
    let writer = Writer::new(ep_out, writes.clone(), closed.clone());

    // Shutdown ends the waits of both directions, a read then reporting the
    // end of the stream and a write refusing. The wire's closer waits for
    // these calls before reporting the stream closed.
    let stream = transport::Stream::new(reader, writer, move || {
        closed.store(true, Ordering::Release);
        reads.notify();
        writes.notify();
    });
    Ark::attach(stream, verifier, cloud)
}

/// Queue of transfers on one endpoint, what a direction of the adapter
/// drives. The tests drive the rings through it without a device.
trait Transfers {
    /// Packet size of the endpoint, deciding when a frame needs a zero length
    /// packet behind it.
    fn packet_size(&self) -> usize;

    /// Transfers queued and not yet taken back.
    fn in_flight(&self) -> usize;

    /// Queues a transfer behind the ones in flight.
    fn queue(&mut self, buffer: Buffer);

    /// Takes the next finished transfer, or arranges for the context's waker
    /// to be woken once one finishes.
    fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Completion>;
}

impl<D: EndpointDirection> Transfers for nusb::Endpoint<Bulk, D> {
    fn packet_size(&self) -> usize {
        self.max_packet_size()
    }

    fn in_flight(&self) -> usize {
        self.pending()
    }

    fn queue(&mut self, buffer: Buffer) {
        self.submit(buffer);
    }

    fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Completion> {
        self.poll_next_complete(cx)
    }
}

/// Wakes a direction waiting on its endpoint, a transfer finishing or the
/// connection closing being what there is to wake for.
#[derive(Default)]
struct Notifier {
    woken: Mutex<bool>, // Whether a wake arrived since the wait last looked
    wake: Condvar,      // Signalled on every wake
}

impl Notifier {
    /// Wakes the waiting direction, or its next wait if none is on.
    fn notify(&self) {
        *self.woken.lock().expect("USB wake state not poisoned") = true;
        self.wake.notify_all();
    }
}

impl Wake for Notifier {
    fn wake(self: Arc<Self>) {
        self.notify();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
    }
}

/// Returns whether an optional deadline has expired.
fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Waits for the next transfer of the queue to finish, giving up without one
/// once the deadline passes or the connection is closed. Without a deadline
/// only a finished transfer or the close end the wait. A deadline already
/// passed makes the wait a look at what has finished.
fn finished<T: Transfers>(
    queue: &mut T,
    notifier: &Arc<Notifier>,
    closed: &AtomicBool,
    deadline: Option<Instant>,
) -> Option<Completion> {
    let waker = Waker::from(notifier.clone());
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(completion) = queue.poll_finished(&mut cx) {
            return Some(completion);
        }
        let mut woken = notifier.woken.lock().expect("USB wake state not poisoned");
        while !*woken {
            if closed.load(Ordering::Acquire) || expired(deadline) {
                return None;
            }
            woken = match deadline {
                None => notifier
                    .wake
                    .wait(woken)
                    .expect("USB wake state not poisoned"),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    notifier
                        .wake
                        .wait_timeout(woken, left)
                        .expect("USB wake state not poisoned")
                        .0
                }
            };
        }
        *woken = false;
    }
}

/// Maps a failed transfer to the error the wire reports, the device going
/// away being the connection lost.
fn transfer_error(err: TransferError) -> io::Error {
    match err {
        TransferError::Disconnected => io::Error::new(io::ErrorKind::NotConnected, err),
        err => io::Error::other(err),
    }
}

/// The error of output refused once the connection was closed.
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "device closed")
}

/// Reader over the bulk IN endpoint, every transfer of the ring queued ahead
/// so the device never waits for a buffer, each served to the wire once it
/// finishes and queued again once served out. An empty transfer is a zero
/// length packet the device closed a frame with, not the end of the stream.
/// A wait ends at the deadline the wire installed, or with the stream once
/// the connection is closed.
struct Reader<T: Transfers> {
    queue: T,                  // Transfers in flight on the endpoint
    notifier: Arc<Notifier>,   // Wakes the wait on a finished transfer or the close
    closed: Arc<AtomicBool>,   // Close signal
    served: Option<Buffer>,    // Finished transfer being served to the wire
    offset: usize,             // Bytes of it served so far
    deadline: Option<Instant>, // Deadline the wire installed for its reads
}

impl<T: Transfers> Reader<T> {
    /// Queues every transfer of the ring on the endpoint.
    fn new(mut queue: T, notifier: Arc<Notifier>, closed: Arc<AtomicBool>) -> Self {
        let packet = queue.packet_size();
        let size = TRANSFER_SIZE.div_ceil(packet) * packet;
        for _ in 0..TRANSFERS {
            queue.queue(Buffer::new(size));
        }
        Self {
            queue,
            notifier,
            closed,
            served: None,
            offset: 0,
            deadline: None,
        }
    }
}

impl<T: Transfers> Read for Reader<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // Serve the finished transfer, queueing it again once served out
            if let Some(served) = self.served.as_ref() {
                let n = buf.len().min(served.len() - self.offset);
                if n > 0 {
                    buf[..n].copy_from_slice(&served[self.offset..self.offset + n]);
                    self.offset += n;
                    return Ok(n);
                }
            }
            if let Some(mut buffer) = self.served.take() {
                buffer.clear();
                self.queue.queue(buffer);
                self.offset = 0;
            }
            if self.closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            if expired(self.deadline) {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            let Some(completion) =
                finished(&mut self.queue, &self.notifier, &self.closed, self.deadline)
            else {
                if self.closed.load(Ordering::Acquire) {
                    return Ok(0);
                }
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            };
            match completion.status {
                Ok(()) => {
                    self.served = Some(completion.buffer);
                    self.offset = 0;
                }
                Err(TransferError::Cancelled) if self.closed.load(Ordering::Acquire) => {
                    return Ok(0);
                }
                Err(err) => return Err(transfer_error(err)),
            }
        }
    }
}

impl<T: Transfers> transport::Read for Reader<T> {
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.deadline = deadline;
        Ok(())
    }
}

/// Writer over the bulk OUT endpoint, each write queued as transfers of the
/// chunk size behind the ones in flight, waiting for room only once the ring
/// is full. A flush closes the frame with a zero length packet if the last
/// write ended on a packet boundary, as the device's read would otherwise
/// wait for the next frame to complete it, then takes back the transfers
/// already finished for their outcome without draining the ring. The
/// deadline the wire installed bounds every wait, the connection closing
/// refuses further output.
struct Writer<T: Transfers> {
    queue: T,                  // Transfers in flight on the endpoint
    notifier: Arc<Notifier>,   // Wakes the wait on a finished transfer or the close
    closed: Arc<AtomicBool>,   // Close signal
    spare: Vec<Buffer>,        // Buffers of finished transfers, reused by the next
    wrote: usize,              // Length of the last write, deciding the zero length packet at flush
    deadline: Option<Instant>, // Deadline the wire installed for its writes
}

impl<T: Transfers> Writer<T> {
    /// Wraps the endpoint, the ring empty until the wire writes.
    fn new(queue: T, notifier: Arc<Notifier>, closed: Arc<AtomicBool>) -> Self {
        Self {
            queue,
            notifier,
            closed,
            spare: Vec::new(),
            wrote: 0,
            deadline: None,
        }
    }

    /// Keeps the buffer of a finished transfer for the next, its failure
    /// surfacing. The zero length packets travel in buffers with no room
    /// for anything, not worth keeping.
    fn finished(&mut self, completion: Completion) -> io::Result<()> {
        if completion.buffer.capacity() >= TRANSFER_SIZE {
            self.spare.push(completion.buffer);
        }
        completion.status.map_err(transfer_error)
    }

    /// Makes room on the ring for one more transfer, waiting for one in
    /// flight to finish within the deadline.
    fn room(&mut self) -> io::Result<()> {
        while self.queue.in_flight() >= TRANSFERS {
            match finished(&mut self.queue, &self.notifier, &self.closed, self.deadline) {
                Some(completion) => self.finished(completion)?,
                None if self.closed.load(Ordering::Acquire) => return Err(closed()),
                None => return Err(io::Error::from(io::ErrorKind::TimedOut)),
            }
        }
        Ok(())
    }

    /// Queues one transfer of the bytes behind the ones in flight.
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.room()?;
        let mut buffer = self
            .spare
            .pop()
            .unwrap_or_else(|| Buffer::new(TRANSFER_SIZE));
        buffer.clear();
        buffer.extend_from_slice(bytes);
        self.queue.queue(buffer);
        Ok(())
    }

    /// Takes back the transfers already finished, their failures surfacing,
    /// without waiting for the rest.
    fn reap(&mut self) -> io::Result<()> {
        while self.queue.in_flight() > 0 {
            let now = Some(Instant::now());
            let Some(completion) = finished(&mut self.queue, &self.notifier, &self.closed, now)
            else {
                return Ok(());
            };
            self.finished(completion)?;
        }
        Ok(())
    }
}

impl<T: Transfers> Write for Writer<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if expired(self.deadline) {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        // Chunks already queued stay queued in order, so a wait for room
        // failing reports what was taken, the rest failing on the next call
        let mut accepted = 0;
        for chunk in buf.chunks(TRANSFER_SIZE) {
            match self.send(chunk) {
                Ok(()) => accepted += chunk.len(),
                Err(err) if accepted == 0 => return Err(err),
                Err(_) => break,
            }
        }
        self.wrote = accepted;
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if expired(self.deadline) {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        // A frame ending on a packet boundary leaves the device's read open,
        // only a short packet completes it, so close the frame with an empty
        // one. Whether one is owed depends on the last write alone, a short
        // one having completed the read whatever came before it.
        if self.wrote > 0 && self.wrote.is_multiple_of(self.queue.packet_size()) {
            self.room()?;
            self.queue.queue(Buffer::new(0));
        }
        self.wrote = 0;
        self.reap()
    }
}

impl<T: Transfers> transport::Write for Writer<T> {
    fn set_write_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        self.deadline = Some(deadline);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::thread;
    use std::time::Duration;

    /// Packet size of the fake endpoint, that of a high speed bulk one.
    const PACKET: usize = 512;

    /// Fake endpoint the tests finish transfers on as they please, from any
    /// thread, waking the waiting direction the way the system does.
    #[derive(Default)]
    struct Ring {
        queued: VecDeque<Buffer>,       // Transfers queued, oldest first
        finished: VecDeque<Completion>, // Transfers finished and not yet taken back
        waker: Option<Waker>,           // Waiter to wake on the next finish
    }

    type Fake = Arc<Mutex<Ring>>;

    impl Transfers for Fake {
        fn packet_size(&self) -> usize {
            PACKET
        }

        fn in_flight(&self) -> usize {
            let ring = self.lock().unwrap();
            ring.queued.len() + ring.finished.len()
        }

        fn queue(&mut self, buffer: Buffer) {
            self.lock().unwrap().queued.push_back(buffer);
        }

        fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Completion> {
            let mut ring = self.lock().unwrap();
            match ring.finished.pop_front() {
                Some(completion) => Poll::Ready(completion),
                None => {
                    ring.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }

    // Finishes the oldest transfer queued with the status, an inbound one
    // filled with the bytes, waking the waiting direction.
    fn finish(fake: &Fake, bytes: usize, status: Result<(), TransferError>) {
        let waker = {
            let mut ring = fake.lock().unwrap();
            let mut buffer = ring.queued.pop_front().expect("no transfer queued");
            if buffer.is_empty() {
                buffer.extend_fill(bytes, 0xab);
            }
            let actual_len = buffer.len();
            ring.finished.push_back(Completion {
                buffer,
                actual_len,
                status,
            });
            ring.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    // Lengths of the transfers queued, oldest first.
    fn queued(fake: &Fake) -> Vec<usize> {
        fake.lock()
            .unwrap()
            .queued
            .iter()
            .map(|buffer| buffer.len())
            .collect()
    }

    fn reader() -> (Reader<Fake>, Fake, Arc<AtomicBool>) {
        let fake = Fake::default();
        let closed = Arc::new(AtomicBool::new(false));
        let reader = Reader::new(fake.clone(), Arc::new(Notifier::default()), closed.clone());
        (reader, fake, closed)
    }

    fn writer() -> (Writer<Fake>, Fake, Arc<AtomicBool>) {
        let fake = Fake::default();
        let closed = Arc::new(AtomicBool::new(false));
        let writer = Writer::new(fake.clone(), Arc::new(Notifier::default()), closed.clone());
        (writer, fake, closed)
    }

    // Tests that the ring is queued ahead, that finished transfers are
    // served as they arrive and queued again once served out, and that an
    // empty one is skipped rather than ending the stream.
    #[test]
    fn test_read_serves_transfers() {
        let (mut reader, fake, _closed) = reader();
        assert_eq!(fake.in_flight(), TRANSFERS);

        finish(&fake, 3, Ok(()));
        let mut buf = [0u8; 2];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(buf, [0xab, 0xab]);
        assert_eq!(reader.read(&mut buf).unwrap(), 1);

        finish(&fake, 0, Ok(()));
        finish(&fake, 4, Ok(()));
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 4);
        assert_eq!(fake.in_flight(), TRANSFERS - 1);
    }

    // Tests that a wait ends with the deadline the wire installed, that a
    // transfer finishing meanwhile ends it with data, and that the close
    // ends it with the stream.
    #[test]
    fn test_read_waits() {
        let (mut reader, fake, closed) = reader();
        let mut buf = [0u8; 8];

        let started = Instant::now();
        transport::Read::set_read_deadline(&mut reader, Some(started + Duration::from_millis(50)))
            .unwrap();
        assert_eq!(
            reader.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        transport::Read::set_read_deadline(&mut reader, None).unwrap();

        let arriving = {
            let fake = fake.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                finish(&fake, 5, Ok(()));
            })
        };
        assert_eq!(reader.read(&mut buf).unwrap(), 5);
        arriving.join().unwrap();

        let closing = {
            let closed = closed.clone();
            let notifier = reader.notifier.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                closed.store(true, Ordering::Release);
                notifier.notify();
            })
        };
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
        closing.join().unwrap();
    }

    // Tests that a failed transfer fails the read, the device going away
    // reported as the connection lost.
    #[test]
    fn test_read_failure() {
        let (mut reader, fake, _closed) = reader();
        finish(&fake, 0, Err(TransferError::Disconnected));
        assert_eq!(
            reader.read(&mut [0u8; 8]).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    // Tests that a write is queued as transfers of the chunk size, that a
    // full ring waits for a transfer to finish within the deadline, that what
    // was taken before the wait ran out is reported as written, and that the
    // close refuses output.
    #[test]
    fn test_write_chunks() {
        let (mut writer, fake, closed) = writer();
        let data = vec![7u8; 100_000];
        assert_eq!(writer.write(&data).unwrap(), 100_000);
        assert_eq!(queued(&fake), [TRANSFER_SIZE, 100_000 - TRANSFER_SIZE]);

        let chunk = vec![7u8; TRANSFER_SIZE];
        for _ in 2..TRANSFERS {
            assert_eq!(writer.write(&chunk).unwrap(), TRANSFER_SIZE);
        }
        assert_eq!(fake.in_flight(), TRANSFERS);

        let started = Instant::now();
        transport::Write::set_write_deadline(&mut writer, started + Duration::from_millis(50))
            .unwrap();
        assert_eq!(
            writer.write(&chunk).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(50));

        let sending = {
            let fake = fake.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                finish(&fake, 0, Ok(()));
            })
        };
        transport::Write::set_write_deadline(
            &mut writer,
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap();
        let two = vec![7u8; 2 * TRANSFER_SIZE];
        assert_eq!(writer.write(&two).unwrap(), TRANSFER_SIZE);
        sending.join().unwrap();

        closed.store(true, Ordering::Release);
        assert_eq!(
            writer.write(&chunk).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        assert_eq!(
            writer.flush().unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    // Tests that a flush closes a frame ending on a packet boundary with a
    // zero length packet and leaves a short one alone, that nothing is owed
    // for nothing written, and that a failed transfer surfaces at the next
    // flush without the flush draining the ring.
    #[test]
    fn test_flush() {
        let (mut writer, fake, _closed) = writer();

        assert_eq!(writer.write(&[1u8; 2 * PACKET]).unwrap(), 2 * PACKET);
        writer.flush().unwrap();
        assert_eq!(queued(&fake), [2 * PACKET, 0]);
        writer.flush().unwrap();
        assert_eq!(queued(&fake), [2 * PACKET, 0]);

        assert_eq!(writer.write(&[1u8; PACKET + 1]).unwrap(), PACKET + 1);
        writer.flush().unwrap();
        assert_eq!(queued(&fake), [2 * PACKET, 0, PACKET + 1]);

        finish(&fake, 0, Ok(()));
        finish(&fake, 0, Err(TransferError::Fault));
        assert!(writer.flush().is_err());
        assert_eq!(fake.in_flight(), 1);
        assert_eq!(writer.spare.len(), 1);
    }

    // Tests that the name is taken from the product string only when a name
    // was given, the carrier and the revision never mistaken for one.
    #[test]
    fn test_name() {
        assert_eq!(name("Ark I \u{00b7} v1.2"), None);
        assert_eq!(name("Ark I \u{00b7} v1.2 \u{00b7} "), None);
        assert_eq!(name("Ark I \u{00b7} v1.2 \u{00b7} lab"), Some("lab"));
        assert_eq!(
            name("Ark I \u{00b7} v1.2 \u{00b7} lab \u{00b7} two"),
            Some("lab \u{00b7} two")
        );
        assert_eq!(name("Ark"), None);
    }
}
