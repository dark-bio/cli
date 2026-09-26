// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Arks over USB, the wire's byte stream on the bulk endpoints of the vendor
//! interface a plugged-in Ark enumerates with.
//!
//! The host claims the interface exclusively, so an Ark held by another
//! program, a browser tab included, cannot be opened until that program lets
//! go.
//!
//! Each direction queues a ring of host transfers to keep the bus occupied.
//! A zero length packet closes a frame that ended on a packet boundary. Flushes
//! reap finished transfers without draining the ring, so consecutive frames
//! can overlap on the bus. Every wait ends at the deadline the wire installed,
//! if any, measured on the connection's clock, or early once the connection is
//! closed.

use crate::ark::Ark;
use crate::{Error, wire};
use darkbio_clock::{Clock, sync};
use nusb::descriptors::TransferType;
use nusb::transfer::{
    Buffer, Bulk, Completion, Direction, EndpointDirection, In, Out, TransferError,
};
use nusb::{ErrorKind, MaybeFuture};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;
use wire::transport::{self, Verifier};

/// Class, subclass and protocol of the vendor interface carrying the wire.
///
/// It distinguishes the wire's interface from any other one with bulk
/// endpoints in both directions.
const VENDOR_INTERFACE: (u8, u8, u8) = (0xff, 1, 2);

/// Separator between the parts of the product string an Ark enumerates with.
///
/// The parts are its model, its revision and the name it was given, the last
/// only when a name was given.
const PRODUCT_SEPARATOR: &str = " \u{00b7} ";

/// Bytes per host transfer in either direction.
///
/// It is a multiple of every supported bulk endpoint's packet size, keeping
/// partial packets at frame boundaries.
const TRANSFER_SIZE: usize = 64 * 1024;

/// Host transfers kept in flight per direction to overlap USB and protocol work.
const TRANSFERS: usize = 16;

/// Returns the name the Ark was given, taken from its product string after the
/// model and the revision.
pub(crate) fn name(product: &str) -> Option<&str> {
    product
        .splitn(3, PRODUCT_SEPARATOR)
        .nth(2)
        .filter(|name| !name.is_empty())
}

/// Opens the Ark and runs the wire handshake over it, the verifier deciding
/// whether to trust the attestation it presents.
///
/// The connection measures its deadlines on the clock.
pub(crate) fn connect<V: Verifier<Info = crate::Identity>>(
    info: &nusb::DeviceInfo,
    verifier: &V,
    cloud: impl FnOnce(&crate::Identity) -> Option<(crate::trust::Environment, crate::trust::Realm)>,
    clock: &Clock,
) -> Result<(Ark, V::Info), Error> {
    // Open the device and read its active configuration
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

    // Claim the interface and open the endpoints. A claim refused as busy
    // means another program holds the device. The endpoints keep the
    // interface claimed and the device open for as long as either lives.
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
    let reads = Arc::new(Notifier::new(clock));
    let writes = Arc::new(Notifier::new(clock));
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

/// Queue of transfers on one endpoint, which a direction of the adapter drives.
///
/// The tests drive the rings through it without a device.
trait Transfers {
    /// Returns the endpoint's packet size, which decides when a frame needs a
    /// zero length packet behind it.
    fn packet_size(&self) -> usize;

    /// Counts the transfers queued and not yet taken back.
    fn in_flight(&self) -> usize;

    /// Queues a transfer behind the ones in flight.
    fn queue(&mut self, buffer: Buffer);

    /// Takes the next finished transfer, or arranges for the context's waker
    /// to be woken once one finishes.
    fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Completion>;
}

impl<D: EndpointDirection> Transfers for nusb::Endpoint<Bulk, D> {
    /// Reads the active endpoint descriptor's maximum packet size.
    fn packet_size(&self) -> usize {
        self.max_packet_size()
    }

    /// Counts the transfers the system USB queue still holds for the endpoint.
    fn in_flight(&self) -> usize {
        self.pending()
    }

    /// Transfers ownership of the buffer to the system USB queue.
    fn queue(&mut self, buffer: Buffer) {
        self.submit(buffer);
    }

    /// Polls the system USB queue for the endpoint's next finished transfer.
    fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Completion> {
        self.poll_next_complete(cx)
    }
}

/// Wake signal for a direction waiting on its endpoint, raised by a finishing
/// transfer or the closing connection.
struct Notifier {
    /// Clock that the wire's deadlines are measured on.
    clock: Clock,
    /// Flag set by a wake that arrived since the wait last looked.
    woken: sync::Mutex<bool>,
    /// Condition signaled on every wake.
    wake: sync::Condvar,
}

impl Notifier {
    /// Creates a notifier whose waits end at deadlines on the clock.
    fn new(clock: &Clock) -> Self {
        Self {
            clock: clock.clone(),
            woken: sync::Mutex::new(false),
            wake: sync::Condvar::new(clock),
        }
    }

    /// Wakes the waiting direction, or its next wait if none is on.
    fn notify(&self) {
        *self.woken.lock().expect("USB wake state not poisoned") = true;
        self.wake.notify_all();
    }

    /// Returns whether an optional deadline has passed on the clock.
    fn expired(&self, deadline: Option<Instant>) -> bool {
        deadline.is_some_and(|deadline| self.clock.now() >= deadline)
    }
}

impl Wake for Notifier {
    /// Records a wake before releasing the transfer's notifier reference.
    fn wake(self: Arc<Self>) {
        self.notify();
    }

    /// Records a wake while retaining the notifier for later transfers.
    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
    }
}

/// Waits for the next transfer of the queue to finish, giving up without one
/// once the deadline passes or the connection is closed.
///
/// Without a deadline only a finished transfer or the close end the wait. A
/// deadline already passed makes the wait a look at what has finished.
fn finished<T: Transfers>(
    queue: &mut T,
    notifier: &Arc<Notifier>,
    closed: &AtomicBool,
    deadline: Option<Instant>,
) -> Option<Completion> {
    let waker = Waker::from(notifier.clone());
    let mut cx = Context::from_waker(&waker);
    loop {
        // Take a finished transfer, the poll registering the waker otherwise
        if let Poll::Ready(completion) = queue.poll_finished(&mut cx) {
            return Some(completion);
        }

        // Wait for a wake, giving up at the close or the deadline
        let mut woken = notifier.woken.lock().expect("USB wake state not poisoned");
        while !*woken {
            if closed.load(Ordering::Acquire) || notifier.expired(deadline) {
                return None;
            }
            woken = match deadline {
                None => notifier
                    .wake
                    .wait(woken)
                    .expect("USB wake state not poisoned"),
                Some(deadline) => {
                    notifier
                        .wake
                        .wait_deadline(woken, deadline)
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

/// Returns the error for output refused once the connection is closed.
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "device closed")
}

/// Reader over the bulk IN endpoint, serving finished transfers to the wire.
///
/// Every transfer of the ring is queued ahead, so the device has buffers to
/// fill while the wire reads. Each is served once it finishes and queued again
/// once served out. An empty transfer is a zero length packet the device closed
/// a frame with, not the end of the stream. A wait ends at the deadline the
/// wire installed, if any, or with the end of the stream once the connection
/// is closed.
struct Reader<T: Transfers> {
    /// Transfers in flight on the endpoint.
    queue: T,
    /// Wake signal for a finished transfer or the close.
    notifier: Arc<Notifier>,
    /// Close signal shared with the writer and the shutdown.
    closed: Arc<AtomicBool>,
    /// Finished transfer being served to the wire.
    served: Option<Buffer>,
    /// Bytes of the served transfer handed out so far.
    offset: usize,
    /// Deadline the wire installed for its reads.
    deadline: Option<Instant>,
}

impl<T: Transfers> Reader<T> {
    /// Queues every transfer of the ring on the endpoint.
    fn new(mut queue: T, notifier: Arc<Notifier>, closed: Arc<AtomicBool>) -> Self {
        // Size each transfer in whole packets, as inbound transfers require
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
    /// Serves completed bytes before waiting for another transfer.
    ///
    /// Empty USB packets delimit frames; only closure ends the stream.
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

            // Stop at the close, or fail at the deadline the wire installed
            if self.closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            if self.notifier.expired(self.deadline) {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }

            // Take the next finished transfer, where an empty one closes a frame
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
    /// Returns the connection's clock, which the read deadlines are measured on.
    fn clock(&self) -> Clock {
        self.notifier.clock.clone()
    }

    /// Bounds future waits without discarding bytes from a completed transfer.
    fn set_read_deadline(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        self.deadline = deadline;
        Ok(())
    }
}

/// Writer over the bulk OUT endpoint, queueing each write as transfers of at
/// most [`TRANSFER_SIZE`] behind the ones in flight.
///
/// A write waits for room only once the ring is full. A flush closes a frame
/// ending on a packet boundary with a zero length packet, since the device's
/// read would otherwise wait for the next frame to complete it. It then takes
/// back the transfers already finished for their outcome, without draining
/// the ring. The deadline the wire installed bounds every wait, and closing
/// the connection refuses further output.
struct Writer<T: Transfers> {
    /// Transfers in flight on the endpoint.
    queue: T,
    /// Wake signal for a finished transfer or the close.
    notifier: Arc<Notifier>,
    /// Close signal shared with the reader and the shutdown.
    closed: Arc<AtomicBool>,
    /// Spare buffers of finished transfers, reused by the next writes.
    spare: Vec<Buffer>,
    /// Length of the last write, which decides whether a flush owes a zero
    /// length packet.
    wrote: usize,
    /// Deadline the wire installed for its writes.
    deadline: Option<Instant>,
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

    /// Keeps a finished transfer's buffer for reuse, returning the transfer's
    /// failure.
    ///
    /// Zero length packets travel in buffers with no room, so those are not
    /// kept.
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

    /// Takes back the transfers already finished without waiting for the rest,
    /// returning the first failure among them.
    fn reap(&mut self) -> io::Result<()> {
        while self.queue.in_flight() > 0 {
            let now = Some(self.notifier.clock.now());
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
    /// Queues bytes in order, reporting partial acceptance if a later wait
    /// fails.
    ///
    /// Acceptance means submission to USB; a later reap may report its failure.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Refuse output once closed or past the deadline
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if self.notifier.expired(self.deadline) {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }

        // Chunks already queued stay queued in order, so a failed wait for
        // room reports what was taken, and the rest fails on the next call
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

    /// Terminates an aligned frame and surfaces completed transfer errors.
    ///
    /// Transfers still in flight remain queued so consecutive frames can
    /// overlap.
    fn flush(&mut self) -> io::Result<()> {
        // Refuse output once closed or past the deadline
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if self.notifier.expired(self.deadline) {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }

        // A frame ending on a packet boundary leaves the device's read open
        // until a short packet completes it, so close the frame with an empty
        // one. Whether one is owed depends on the last write alone, since a
        // short one completed the read whatever came before it.
        if self.wrote > 0 && self.wrote.is_multiple_of(self.queue.packet_size()) {
            self.room()?;
            self.queue.queue(Buffer::new(0));
        }

        // Take back what finished, leaving the rest in flight
        self.wrote = 0;
        self.reap()
    }
}

impl<T: Transfers> transport::Write for Writer<T> {
    /// Returns the connection's clock, which the write deadline is measured on.
    fn clock(&self) -> Clock {
        self.notifier.clock.clone()
    }

    /// Installs one bound for subsequent writes, queue waits and frame flushes.
    fn set_write_deadline(&mut self, deadline: Instant) -> io::Result<()> {
        self.deadline = Some(deadline);
        Ok(())
    }
}

/// USB adapter regressions over a fake endpoint, and product name parsing.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{test_clock, wait_deadline};
    use darkbio_clock::TestClock;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    /// Packet size of the fake endpoint, that of a high speed bulk one.
    const PACKET: usize = 512;

    /// Fake endpoint the tests finish transfers on as they please, from any
    /// thread, waking the waiting direction the way the system does.
    #[derive(Default)]
    struct Ring {
        /// Transfers queued, oldest first.
        queued: VecDeque<Buffer>,
        /// Transfers finished and not yet taken back.
        finished: VecDeque<Completion>,
        /// Waker of the direction waiting on the next finish.
        waker: Option<Waker>,
    }

    /// Fake endpoint shared between a direction and the test driving it.
    type Fake = Arc<Mutex<Ring>>;

    impl Transfers for Fake {
        /// Returns the fixed `PACKET` size the tests use.
        fn packet_size(&self) -> usize {
            PACKET
        }

        /// Counts the queued and finished transfers not yet taken back.
        fn in_flight(&self) -> usize {
            let ring = self.lock().unwrap();
            ring.queued.len() + ring.finished.len()
        }

        /// Queues the buffer for the test to finish.
        fn queue(&mut self, buffer: Buffer) {
            self.lock().unwrap().queued.push_back(buffer);
        }

        /// Takes the oldest finished transfer, or keeps the waker until the
        /// test finishes one.
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

    /// Finishes the oldest queued transfer with the status, filling an empty
    /// inbound buffer with `bytes` bytes, and wakes the waiting direction.
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

    /// Waits until a direction has looked at the ring and parked on the
    /// deadline, or without one.
    ///
    /// The direction is the only thread that waits on the clock, and the test
    /// clears the ring's waker before it starts.
    fn parked(fake: &Fake, tester: &TestClock, deadline: Option<Instant>) {
        while fake.lock().unwrap().waker.is_none() {
            thread::yield_now();
        }
        match deadline {
            Some(deadline) => wait_deadline(tester, deadline),
            None => {
                tester.wait_blocked(1);
                assert_eq!(tester.next_deadline(), None);
            }
        }
    }

    /// Returns the lengths of the queued transfers, oldest first.
    fn queued(fake: &Fake) -> Vec<usize> {
        fake.lock()
            .unwrap()
            .queued
            .iter()
            .map(|buffer| buffer.len())
            .collect()
    }

    /// Creates a reader over a fake endpoint, returned with the endpoint and
    /// the close flag.
    fn reader(clock: &Clock) -> (Reader<Fake>, Fake, Arc<AtomicBool>) {
        let fake = Fake::default();
        let closed = Arc::new(AtomicBool::new(false));
        let reader = Reader::new(fake.clone(), Arc::new(Notifier::new(clock)), closed.clone());
        (reader, fake, closed)
    }

    /// Creates a writer over a fake endpoint, returned with the endpoint and
    /// the close flag.
    fn writer(clock: &Clock) -> (Writer<Fake>, Fake, Arc<AtomicBool>) {
        let fake = Fake::default();
        let closed = Arc::new(AtomicBool::new(false));
        let writer = Writer::new(fake.clone(), Arc::new(Notifier::new(clock)), closed.clone());
        (writer, fake, closed)
    }

    /// A read serves finished transfers as they arrive, queues them again once
    /// served out and skips empty ones.
    #[test]
    fn test_read_serves_transfers() {
        // The whole ring is queued ahead
        let (mut reader, fake, _closed) = reader(&test_clock().clock());
        assert_eq!(fake.in_flight(), TRANSFERS);

        // A finished transfer serves across reads
        finish(&fake, 3, Ok(()));
        let mut buf = [0u8; 2];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(buf, [0xab, 0xab]);
        assert_eq!(reader.read(&mut buf).unwrap(), 1);

        // An empty transfer is skipped, and the served-out ones queue again
        finish(&fake, 0, Ok(()));
        finish(&fake, 4, Ok(()));
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 4);
        assert_eq!(fake.in_flight(), TRANSFERS - 1);
    }

    /// A read's wait ends at the wire's deadline, with a transfer finishing
    /// meanwhile, or with the end of the stream at the close.
    #[test]
    fn test_read_waits() {
        // Read from a fake ring, keeping its notifier to wake the close
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut reader, fake, closed) = reader(&clock);
        let notifier = reader.notifier.clone();

        // The read waits on its deadline and ends once the clock reaches it
        let deadline = clock.now() + Duration::from_millis(50);
        transport::Read::set_read_deadline(&mut reader, Some(deadline)).unwrap();
        fake.lock().unwrap().waker = None;
        let reading = thread::spawn(move || {
            let result = reader.read(&mut [0u8; 8]);
            assert!(clock.now() >= deadline);
            (reader, result)
        });
        parked(&fake, &tester, Some(deadline));
        tester.advance_to(deadline);
        let (mut reader, result) = reading.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        transport::Read::set_read_deadline(&mut reader, None).unwrap();

        // A transfer finishing during the wait ends it with its data
        fake.lock().unwrap().waker = None;
        let reading = thread::spawn(move || {
            let result = reader.read(&mut [0u8; 8]);
            (reader, result)
        });
        parked(&fake, &tester, None);
        finish(&fake, 5, Ok(()));
        let (mut reader, result) = reading.join().unwrap();
        assert_eq!(result.unwrap(), 5);

        // Closing the connection ends the wait with the stream
        fake.lock().unwrap().waker = None;
        let reading = thread::spawn(move || reader.read(&mut [0u8; 8]));
        parked(&fake, &tester, None);
        closed.store(true, Ordering::Release);
        notifier.notify();
        assert_eq!(reading.join().unwrap().unwrap(), 0);
    }

    /// A failed transfer fails the read, a vanished device reported as a lost
    /// connection.
    #[test]
    fn test_read_failure() {
        let (mut reader, fake, _closed) = reader(&test_clock().clock());
        finish(&fake, 0, Err(TransferError::Disconnected));
        assert_eq!(
            reader.read(&mut [0u8; 8]).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
    }

    /// A write queues transfers of at most the transfer size, waits for room
    /// within the deadline, reports what it took, and fails once closed.
    #[test]
    fn test_write_chunks() {
        // A write splits into transfers, and further writes fill the ring
        let mut tester = test_clock();
        let clock = tester.clock();
        let (mut writer, fake, closed) = writer(&clock);
        let data = vec![7u8; 100_000];
        assert_eq!(writer.write(&data).unwrap(), 100_000);
        assert_eq!(queued(&fake), [TRANSFER_SIZE, 100_000 - TRANSFER_SIZE]);

        let chunk = vec![7u8; TRANSFER_SIZE];
        for _ in 2..TRANSFERS {
            assert_eq!(writer.write(&chunk).unwrap(), TRANSFER_SIZE);
        }
        assert_eq!(fake.in_flight(), TRANSFERS);

        // A full ring waits for room until the clock reaches the deadline
        let deadline = clock.now() + Duration::from_millis(50);
        transport::Write::set_write_deadline(&mut writer, deadline).unwrap();
        fake.lock().unwrap().waker = None;
        let writing = thread::spawn({
            let clock = clock.clone();
            let chunk = chunk.clone();
            move || {
                let result = writer.write(&chunk);
                assert!(clock.now() >= deadline);
                (writer, result)
            }
        });
        parked(&fake, &tester, Some(deadline));
        tester.advance_to(deadline);
        let (mut writer, result) = writing.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);

        // Make one completion available, so only the second chunk waits for
        // its deadline
        let two = vec![7u8; 2 * TRANSFER_SIZE];
        finish(&fake, 0, Ok(()));
        let deadline = clock.now() + Duration::from_millis(100);
        transport::Write::set_write_deadline(&mut writer, deadline).unwrap();
        fake.lock().unwrap().waker = None;
        let writing = thread::spawn(move || {
            let result = writer.write(&two);
            (writer, result)
        });
        parked(&fake, &tester, Some(deadline));
        tester.advance_to(deadline);
        let (mut writer, result) = writing.join().unwrap();
        assert_eq!(result.unwrap(), TRANSFER_SIZE);

        // The close refuses output
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

    /// A flush closes an aligned frame with a zero length packet, owes none for
    /// a short frame, and surfaces a failure without draining the ring.
    #[test]
    fn test_flush() {
        // Write through a fake ring
        let (mut writer, fake, _closed) = writer(&test_clock().clock());

        // An aligned frame gets one zero length packet, however often it flushes
        assert_eq!(writer.write(&[1u8; 2 * PACKET]).unwrap(), 2 * PACKET);
        writer.flush().unwrap();
        assert_eq!(queued(&fake), [2 * PACKET, 0]);
        writer.flush().unwrap();
        assert_eq!(queued(&fake), [2 * PACKET, 0]);

        // A short frame needs no zero length packet
        assert_eq!(writer.write(&[1u8; PACKET + 1]).unwrap(), PACKET + 1);
        writer.flush().unwrap();
        assert_eq!(queued(&fake), [2 * PACKET, 0, PACKET + 1]);

        // A failed transfer surfaces at the next flush, which leaves the rest
        // in flight
        finish(&fake, 0, Ok(()));
        finish(&fake, 0, Err(TransferError::Fault));
        assert!(writer.flush().is_err());
        assert_eq!(fake.in_flight(), 1);
        assert_eq!(writer.spare.len(), 1);
    }

    /// A name is taken from the product string only when one was given, never
    /// mistaking the model or the revision for one.
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
