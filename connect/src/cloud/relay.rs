// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Pumps opaque frames between the Ark and one WebSocket to the cloud.
//!
//! The relay joins once the Ark sends its first frame for Ark Companion, or
//! when a caller joins it explicitly. From then on it stays joined until the
//! session ends, replacing a failed socket with backoff. Frames pass through
//! unchanged and in order, and nothing here reads them.
//!
//! A worker thread owns the socket, the joins and both directions of the pump.
//! A second thread follows the session's clock. It wakes the worker when a
//! deadline passes, and refuses a frame whose hold ran out even while a join
//! blocks. Only the heartbeat runs on real time.

use super::{
    Attempt, Failure, Services,
    socket::{Connection, Socket, io_error, socket_error, socket_mut},
};
use crate::{Timing, schema};
use darkbio_clock::{
    Clock,
    crossbeam_channel::{self, Receiver, Sender, select},
};
use darkbio_wire::protocol::{self, Promise, Requester, Responder};
use mio::{Events, Interest, Poll, Token, Waker};
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{Bytes, Message};

/// Most bytes of unanswered frames in each direction, 16 MiB, a host limit of
/// the relay protocol.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Most unanswered frames in each direction, 128, a host limit of the relay
/// protocol.
const MAX_INFLIGHT: usize = 128;
/// Largest message the socket assembles, the most one wire message carries.
const MAX_MESSAGE: usize = darkbio_wire::transport::MAX_MESSAGE_SIZE;
/// Hold of 10 s on each frame of the Ark, the time the Ark waits for its
/// answer before giving up on it.
const OUTBOUND_TIMEOUT: Duration = Duration::from_secs(10);
/// Wait of 10 s for the Ark to answer a frame from the socket, which it does
/// once it opened the frame. Ark Hub waits as long.
const INBOUND_TIMEOUT: Duration = Duration::from_secs(10);
/// Most time, 5 s, that written output may wait to flush into the socket.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval of 15 s from joining or a matching pong to the next liveness
/// probe.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Wait of 10 s for the pong matching a probe.
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// First reconnect delay, 1 s, doubled after each failure as in Ark Hub.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
/// Longest reconnect delay, 30 s, the cap Ark Hub uses.
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Time, 120 s, that reconnects keep trying after a failure, as in Ark Hub.
const RETRY_WINDOW: Duration = Duration::from_secs(120);
/// Cause of a frame whose hold ran out on a working socket or during a join.
const UNSENT: &str = "relay could not send a frame within 10 s";
/// Readiness token of the current socket.
const SOCKET: Token = Token(0);
/// Readiness token for new frames, passed deadlines, answers and closure.
const WAKE: Token = Token(1);

/// What the relay reports to the application, for its user.
#[derive(Clone, Debug)]
pub enum RelayNotice {
    /// The Ark refused a frame from Ark Companion, with the Ark's error.
    Refused(schema::Error),
    /// The relay could not send frames of the Ark, with the cause. It comes
    /// once for each failed attempt, however many frames that refused.
    Failed(String),
}

/// Holder of the application's notice callback.
#[derive(Default)]
pub(super) struct Observer {
    /// Installed callback, cloned out of the lock before it runs.
    callback: Mutex<Option<Callback>>,
}

/// Notice callback of the application.
type Callback = Arc<dyn Fn(RelayNotice) + Send + Sync>;

impl fmt::Debug for Observer {
    /// Omits the application's callback from diagnostics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Observer").finish_non_exhaustive()
    }
}

impl Observer {
    /// Installs the callback, replacing any earlier one.
    pub(super) fn set(&self, callback: Callback) {
        *self.callback.lock().expect("relay observer not poisoned") = Some(callback);
    }

    /// Runs the callback outside the lock, so it may use the connection's
    /// clients.
    pub(super) fn report(&self, notice: RelayNotice) {
        let callback = self
            .callback
            .lock()
            .expect("relay observer not poisoned")
            .clone();
        if let Some(callback) = callback {
            callback(notice);
        }
    }
}

/// The relay of one session, whose frame queue outlives each socket.
#[derive(Debug)]
pub(super) struct Relay {
    /// State shared with the worker and the timer thread.
    shared: Arc<Shared>,
}

/// State shared by the relay, its worker and its timer thread.
#[derive(Debug)]
struct Shared {
    /// Session clock, which measures frame holds, answer deadlines and
    /// reconnects.
    clock: Clock,
    /// Frame queue and join state, changed under one lock.
    state: Mutex<State>,
    /// Wakes the worker's readiness poll.
    wake: Arc<Waker>,
    /// Prompts the timer thread to rearm, coalescing repeated prompts.
    changed: Sender<()>,
    /// The application's notice observer.
    notices: Arc<Observer>,
    /// One-shot test pause between expiry and the next deadline check.
    #[cfg(test)]
    rearming: Mutex<Option<(Sender<()>, Receiver<()>)>>,
}

/// Frame queue, socket and join state of the relay.
#[derive(Debug)]
struct State {
    /// Frames of the Ark in arrival order, including the one being written.
    queue: VecDeque<Frame>,
    /// Bytes of the queued frames.
    bytes: usize,
    /// Whether the queue's first frame is partly written to the socket.
    writing: bool,
    /// Handle that shuts the current socket down, set once its TCP connects.
    socket: Option<TcpStream>,
    /// Whether the current socket finished its upgrade.
    connected: bool,
    /// Whether a join succeeded, which keeps the relay joined for the session.
    wanted: bool,
    /// Whether a frame or an explicit join asks for an attempt now.
    kick: bool,
    /// Whether an attempt ended before, so the next one refreshes cloud sync.
    unsynced: bool,
    /// Whether the session or its owner closed the relay for good.
    closed: bool,
    /// Deadline of the join in progress, its cloud sync included.
    joining: Option<Instant>,
    /// Whether a passed deadline cut short the join or the partly written
    /// frame.
    interrupted: bool,
    /// Outcome and deadline of an explicit join, shared by its callers.
    waiter: Option<(Arc<Attempt>, Instant)>,
    /// When the next reconnect is due, absent while none is scheduled.
    retry: Option<Instant>,
    /// When the current run of failed attempts began.
    failing: Option<Instant>,
    /// Delay before the next reconnect, before its 20% jitter.
    backoff: Duration,
    /// Deadline of the oldest frame the Ark has not answered yet.
    inbound: Option<Instant>,
    /// When the socket's pending output must have flushed, while some waits.
    write_deadline: Option<Instant>,
    /// Latest cause, given to the frames refused for want of a socket.
    cause: String,
    /// Whether this attempt already reported its failure.
    notified: bool,
    /// Whether this spell of a full queue already reported its refusals.
    full: bool,
    /// Bytes a test still lets the next socket write before it blocks.
    #[cfg(test)]
    write_limit: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl Default for State {
    /// Starts with no socket, the first reconnect delay and the cause of an
    /// unsent frame.
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            bytes: 0,
            writing: false,
            socket: None,
            connected: false,
            wanted: false,
            kick: false,
            unsynced: false,
            closed: false,
            joining: None,
            interrupted: false,
            waiter: None,
            retry: None,
            failing: None,
            backoff: RECONNECT_MIN,
            inbound: None,
            write_deadline: None,
            cause: UNSENT.into(),
            notified: false,
            full: false,
            #[cfg(test)]
            write_limit: None,
        }
    }
}

/// A frame of the Ark, waiting until a socket flush took all of it.
#[derive(Debug)]
struct Frame {
    /// Opaque bytes, shared with the socket while the write is pending.
    bytes: Bytes,
    /// The Ark's request, acknowledged after the flush or refused.
    responder: Responder,
    /// End of the frame's hold, counted from its arrival.
    deadline: Instant,
}

impl Relay {
    /// Starts the worker and the timer thread, without contacting the cloud.
    pub(super) fn new(
        services: Weak<Services>,
        requester: Requester,
        notices: Arc<Observer>,
    ) -> Result<Self, Failure> {
        // Set up the worker's wakeups before the dispatcher can queue a frame
        let poll = Poll::new().map_err(io_error)?;
        let (changed, changes) = crossbeam_channel::bounded(1);
        let shared = Arc::new(Shared {
            clock: requester.clock(),
            state: Mutex::new(State::default()),
            wake: Arc::new(Waker::new(poll.registry(), WAKE).map_err(io_error)?),
            changed,
            notices,
            #[cfg(test)]
            rearming: Mutex::new(None),
        });
        let relay = Self {
            shared: shared.clone(),
        };

        // Start the timer thread, which never touches the socket, then the worker
        thread::Builder::new()
            .name("ark-relay-clock".into())
            .spawn({
                let shared = shared.clone();
                move || timers(shared, changes)
            })
            .map_err(io_error)?;
        thread::Builder::new()
            .name("ark-relay".into())
            .spawn(move || worker(services, requester, shared, poll))
            .map_err(io_error)?;
        Ok(relay)
    }

    /// Queues a frame of the Ark for the socket, without waiting on I/O.
    ///
    /// Without a socket, the frame starts a join at once, pulling forward a
    /// scheduled reconnect. A frame past either queue limit is refused.
    pub(super) fn forward(&self, request: schema::RelayOutboundRequest, responder: Responder) {
        let mut state = self.shared.state.lock().expect("relay state not poisoned");
        let reason = if state.closed {
            Some("relay closed")
        } else if state.queue.len() >= MAX_INFLIGHT
            || request.frame.len() > MAX_BYTES.saturating_sub(state.bytes)
        {
            Some("relay queue full")
        } else {
            None
        };

        // Refuse a frame the relay cannot take, reporting a full queue once
        if let Some(reason) = reason {
            let notice = (!state.full).then(|| RelayNotice::Failed(reason.into()));
            state.full = true;
            drop(state);
            self.shared.report(notice);
            fail(responder, reason);
            return;
        }

        // Hold the frame from its arrival, through any join, until it is written
        state.bytes += request.frame.len();
        state.queue.push_back(Frame {
            bytes: request.frame.into(),
            responder,
            deadline: self.shared.clock.now() + OUTBOUND_TIMEOUT,
        });

        // Without a socket, join now, keeping the backoff of reconnects still
        // under way
        if !state.connected && state.joining.is_none() && !state.kick {
            if state.retry.take().is_none() {
                state.failing = None;
                state.backoff = RECONNECT_MIN;
            }
            state.kick = true;
        }
        drop(state);
        self.shared.signal();
    }

    /// Joins now, or reuses the open socket, waiting until the caller's
    /// deadline.
    ///
    /// Concurrent callers share one attempt, and each stops waiting at its own
    /// deadline.
    pub(super) fn attach(&self, deadline: Instant) -> Result<(), Failure> {
        let mut state = self.shared.state.lock().expect("relay state not poisoned");
        if state.closed {
            return Err(protocol::Error::Closed.into());
        }
        if self.shared.clock.now() >= deadline {
            return Err(protocol::Error::Timeout.into());
        }
        if state.connected {
            return Ok(());
        }
        let attempt = match &state.waiter {
            Some((attempt, _)) => attempt.clone(),
            None => {
                let attempt = Arc::new(Attempt::new(&self.shared.clock));
                state.waiter = Some((attempt.clone(), deadline));
                if state.joining.is_none() {
                    state.kick = true;
                    state.retry = None;
                    state.failing = None;
                    state.backoff = RECONNECT_MIN;
                }
                attempt
            }
        };
        drop(state);
        self.shared.signal();
        attempt.wait(deadline)
    }

    /// Closes the socket and refuses the queued frames, without waiting on
    /// I/O.
    pub(super) fn close(&self) {
        self.shared.close();
    }
}

impl Drop for Relay {
    /// Ends the relay along with the connection that owned it.
    fn drop(&mut self) {
        self.close();
    }
}

impl State {
    /// Takes the first frame off the queue, once it is answered or refused.
    fn pop(&mut self) -> Option<Frame> {
        let frame = self.queue.pop_front()?;
        self.bytes -= frame.bytes.len();
        self.full = false;
        Some(frame)
    }

    /// Returns this attempt's failure notice, the first time only.
    fn notice(&mut self, cause: &str) -> Option<RelayNotice> {
        if self.notified {
            return None;
        }
        self.notified = true;
        Some(RelayNotice::Failed(cause.into()))
    }

    /// Shuts the socket down, which also interrupts a stalled upgrade or a
    /// partly written frame.
    fn shutdown(&self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

impl Shared {
    /// Wakes the worker and prompts the timer thread to rearm.
    fn signal(&self) {
        let _ = self.wake.wake();
        let _ = self.changed.try_send(());
    }

    /// Delivers a notice, when there is one, outside the relay's lock.
    fn report(&self, notice: Option<RelayNotice>) {
        if let Some(notice) = notice {
            self.notices.report(notice);
        }
    }

    /// Ends the relay for good, releasing explicit joins and refusing the
    /// queued frames.
    fn close(&self) {
        let mut state = self.state.lock().expect("relay state not poisoned");
        if state.closed {
            return;
        }
        state.closed = true;
        state.shutdown();
        if let Some((attempt, _)) = state.waiter.take() {
            attempt.finish(Err(protocol::Error::Closed.into()));
        }
        while let Some(frame) = state.pop() {
            fail(frame.responder, "relay closed");
        }
        drop(state);
        self.signal();
    }

    /// Keeps a handle that shuts a joining socket down, before its upgrade
    /// starts.
    ///
    /// A closed relay or a passed join deadline abandons the socket instead.
    fn socket(&self, socket: &TcpStream) -> Result<(), Failure> {
        let mut state = self.state.lock().expect("relay state not poisoned");
        if state.closed {
            return Err(protocol::Error::Closed.into());
        }
        if state.interrupted
            || state
                .joining
                .is_none_or(|deadline| self.clock.now() >= deadline)
        {
            return Err(protocol::Error::Timeout.into());
        }
        state.socket = Some(socket.try_clone().map_err(io_error)?);
        Ok(())
    }

    /// Refuses the frames whose hold ran out, and cuts short a join or a
    /// partly written frame whose deadline passed.
    fn expire(&self) {
        let mut state = self.state.lock().expect("relay state not poisoned");
        let now = self.clock.now();
        let mut notice = None;
        let mut refused = Vec::new();
        while state
            .queue
            .front()
            .is_some_and(|frame| frame.deadline <= now)
        {
            if state.writing {
                state.shutdown();
                state.interrupted = true;
                state.writing = false;
            }
            let cause = state.cause.clone();
            notice = notice.or_else(|| state.notice(&cause));
            let frame = state.pop().unwrap();
            refused.push((frame.responder, cause));
        }
        if state.joining.is_some_and(|deadline| deadline <= now) {
            state.interrupted = true;
            state.shutdown();
            if let Some((attempt, _)) = state.waiter.take() {
                attempt.finish(Err(protocol::Error::Timeout.into()));
            }
        }
        drop(state);
        self.report(notice);
        for (responder, cause) in refused {
            fail(responder, &cause);
        }
    }
}

/// Wakes the worker whenever a deadline on the session's clock passes, until
/// the relay closes.
fn timers(shared: Arc<Shared>, changes: Receiver<()>) {
    loop {
        // Act on everything already due, then arm a timer for the next deadline
        shared.expire();
        #[cfg(test)]
        {
            let pause = shared.rearming.lock().unwrap().take();
            if let Some((rearming, resume)) = pause {
                while changes.try_recv().is_ok() {}
                rearming.send(()).unwrap();
                resume.recv().unwrap();
            }
        }
        let timer = {
            let state = shared.state.lock().expect("relay state not poisoned");
            if state.closed {
                return;
            }
            let now = shared.clock.now();
            let joining = state.joining.filter(|_| !state.interrupted);
            if state
                .queue
                .front()
                .is_some_and(|frame| frame.deadline <= now)
                || joining.is_some_and(|deadline| deadline <= now)
            {
                continue;
            }
            if state.retry.is_some_and(|deadline| deadline <= now)
                || state.inbound.is_some_and(|deadline| deadline <= now)
                || state.write_deadline.is_some_and(|deadline| deadline <= now)
            {
                let _ = shared.wake.wake();
            }
            let next = state
                .queue
                .front()
                .map(|frame| frame.deadline)
                .into_iter()
                .chain(joining)
                .chain(state.retry)
                .chain(state.inbound)
                .chain(state.write_deadline)
                .filter(|deadline| *deadline > now)
                .min();
            next.map_or_else(crossbeam_channel::never, |deadline| {
                shared.clock.at(deadline)
            })
        };

        // Sleep until that deadline, or until a change may move it
        select! {
            recv(changes) -> _ => {},
            recv(timer) -> _ => {
                shared.expire();
                let _ = shared.wake.wake();
            },
        }
    }
}

/// Refuses a frame of the Ark with `UNAVAILABLE` and the cause, without
/// waiting on wire I/O.
pub(super) fn fail(responder: Responder, reason: &str) {
    let deadline = responder.clock().now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT;
    let _ = responder.fail(
        schema::Error::reserved(schema::ReservedErrors::Unavailable, reason),
        deadline,
    );
}

/// Returns whether a nonblocking socket needs another readiness notification.
fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(error) if error.kind() == io::ErrorKind::WouldBlock)
}

/// Joins whenever an attempt is due and pumps each socket, until the relay
/// closes.
fn worker(services: Weak<Services>, requester: Requester, shared: Arc<Shared>, mut poll: Poll) {
    let mut events = Events::with_capacity(8);
    loop {
        // Start an attempt when one is asked for or a reconnect is due, bounded
        // by an explicit join's deadline or else the oldest frame's hold
        shared.expire();
        let attempt = {
            let mut state = shared.state.lock().expect("relay state not poisoned");
            if state.closed {
                return;
            }
            let now = shared.clock.now();
            if !state.wanted && state.queue.is_empty() && state.waiter.is_none() {
                state.kick = false;
            }
            if state.kick || state.retry.is_some_and(|at| at <= now) {
                let deadline = state
                    .waiter
                    .as_ref()
                    .map(|(_, deadline)| *deadline)
                    .or_else(|| state.queue.front().map(|frame| frame.deadline))
                    .unwrap_or(now + OUTBOUND_TIMEOUT);
                state.kick = false;
                state.retry = None;
                state.joining = Some(deadline);
                state.interrupted = false;
                state.notified = false;
                state.cause = UNSENT.into();
                Some((deadline, state.unsynced))
            } else {
                None
            }
        };

        // With nothing due, sleep until a frame, a join or the timer thread
        // wakes the worker
        let Some((deadline, refresh)) = attempt else {
            if let Err(error) = poll.poll(&mut events, None)
                && error.kind() != io::ErrorKind::Interrupted
            {
                shared.close();
                return;
            }
            continue;
        };
        let _ = shared.changed.try_send(());

        // Refresh cloud sync after an earlier attempt ended, then join with a
        // fresh token from the Ark
        let result = (|| {
            let services = services.upgrade().ok_or(protocol::Error::Closed)?;
            if refresh {
                services.ensure(&requester, super::Step::Refresh, Timing::until(deadline))?;
            }
            let socket = services.join(&requester, Timing::until(deadline), &|socket| {
                shared.socket(socket)
            })?;
            drop(services);
            ready(socket, &poll, &shared)
        })();
        let mut socket = match result {
            Ok(socket) => socket,
            Err(error) => {
                ended(&shared, error, true);
                continue;
            }
        };

        // Keep the socket only if its join finished in time and the relay is
        // still open
        {
            let mut state = shared.state.lock().expect("relay state not poisoned");
            if state.closed {
                return;
            }
            if state.interrupted || shared.clock.now() >= deadline {
                drop(state);
                ended(&shared, protocol::Error::Timeout.into(), true);
                continue;
            }
            state.joining = None;
            state.connected = true;
            state.wanted = true;
            state.failing = None;
            state.backoff = RECONNECT_MIN;
            state.cause = UNSENT.into();
            if let Some((attempt, _)) = state.waiter.take() {
                attempt.finish(Ok(()));
            }
        }
        shared.signal();
        tracing::info!(target: "darkbio_connect::setup", "relay attached");

        // Pump until the socket fails, leaving the queued frames to the next
        // socket within their holds
        let result = pump(
            &mut socket,
            &mut poll,
            &requester,
            &shared,
            Heartbeat::attach(),
        );
        ended(&shared, result.unwrap_err(), false);
    }
}

/// Switches an upgraded socket to the worker's readiness poll.
fn ready(mut socket: Connection, poll: &Poll, shared: &Shared) -> Result<Connection, Failure> {
    // Abandon the socket when the relay closed during the upgrade
    if shared
        .state
        .lock()
        .expect("relay state not poisoned")
        .closed
    {
        return Err(protocol::Error::Closed.into());
    }

    // Make the stream nonblocking under its TLS state and register it
    let Socket::Blocking { stream, .. } = socket_mut(&mut socket) else {
        unreachable!()
    };
    stream.set_read_timeout(None).map_err(io_error)?;
    stream.set_write_timeout(None).map_err(io_error)?;
    stream.set_nonblocking(true).map_err(io_error)?;
    let mut connected = mio::net::TcpStream::from_std(stream.try_clone().map_err(io_error)?);
    poll.registry()
        .register(
            &mut connected,
            SOCKET,
            Interest::READABLE | Interest::WRITABLE,
        )
        .map_err(io_error)?;
    *socket_mut(&mut socket) = Socket::Connected {
        stream: connected,
        #[cfg(test)]
        write_limit: shared.state.lock().unwrap().write_limit.clone(),
    };
    Ok(socket)
}

/// Tears down a failed join or socket, and schedules a reconnect while the
/// relay is wanted.
///
/// A failed join refuses the frames waiting on it, while a failed socket
/// leaves its queued frames to the next one.
fn ended(shared: &Shared, error: Failure, joining: bool) {
    let cause = crate::Error::from(error.clone()).to_string();
    let cause = if joining {
        format!("relay join failed: {cause}")
    } else {
        cause
    };

    // Drop the socket, and make the next attempt refresh cloud sync first
    let mut state = shared.state.lock().expect("relay state not poisoned");
    state.shutdown();
    state.socket = None;
    state.connected = false;
    state.joining = None;
    state.inbound = None;
    state.write_deadline = None;
    state.writing = false;
    state.unsynced = true;
    if state.closed {
        return;
    }
    state.cause = cause.clone();
    if let Some((attempt, _)) = state.waiter.take() {
        attempt.finish(Err(error));
    }

    // A failed join refuses the frames waiting on it, reporting the cause once
    let mut notice = None;
    let mut refused = Vec::new();
    if joining && !state.queue.is_empty() {
        notice = state.notice(&cause);
        while let Some(frame) = state.pop() {
            refused.push(frame.responder);
        }
    }

    // A relay that joined before reconnects after jittered, doubling delays,
    // until its retry window closes
    if state.wanted {
        let now = shared.clock.now();
        let since = *state.failing.get_or_insert(now);
        if now < since + RETRY_WINDOW {
            let random = darkbio_crypto::rand::generate(2);
            let fraction =
                f64::from(u16::from_le_bytes([random[0], random[1]])) / f64::from(u16::MAX);
            let delay = state
                .backoff
                .mul_f64(0.8 + 0.4 * fraction)
                .min(RECONNECT_MAX);
            state.backoff = (state.backoff * 2).min(RECONNECT_MAX);
            state.retry = (now + delay < since + RETRY_WINDOW).then_some(now + delay);
        } else {
            state.retry = None;
        }
    }
    drop(state);
    shared.report(notice);
    for responder in refused {
        fail(responder, &cause);
    }
    shared.signal();
}

/// A frame from the socket that the Ark has not answered yet.
struct Inbound {
    /// The Ark's pending answer.
    promise: Promise<protocol::Message>,
    /// Set once the answer arrived, whatever it says.
    ready: Arc<AtomicBool>,
    /// Bytes of the frame, counted until the Ark answers.
    bytes: usize,
    /// When the answer is due, on the session's clock.
    deadline: Instant,
}

/// Pumps one socket in both directions until it fails, acknowledging each
/// frame of the Ark once its flush completed.
#[expect(
    clippy::disallowed_methods,
    reason = "socket heartbeat uses real time with mio readiness"
)]
fn pump(
    socket: &mut Connection,
    poll: &mut Poll,
    requester: &Requester,
    shared: &Shared,
    mut heartbeat: Heartbeat,
) -> Result<(), Failure> {
    let mut inbound: VecDeque<Inbound> = VecDeque::new();
    let mut bytes = 0usize;
    let mut backlog = Backlog::default();
    let mut events = Events::with_capacity(8);
    loop {
        // Settle the Ark's answers in any order, ending the socket on a missed one
        shared.expire();
        let mut index = 0;
        while index < inbound.len() {
            if !inbound[index].ready.load(Ordering::Acquire)
                && shared.clock.now() < inbound[index].deadline
            {
                index += 1;
                continue;
            }
            let frame = inbound.remove(index).unwrap();
            bytes -= frame.bytes;
            match frame.promise.wait::<schema::RelayInboundResponse>() {
                Ok(_) => {}
                Err(protocol::Error::Remote(error)) => {
                    shared.notices.report(RelayNotice::Refused(error))
                }
                Err(protocol::Error::Timeout) => {
                    return Err(Failure::Relay(
                        "the Ark did not answer a relay frame within 10 s".into(),
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }

        // Stop on closure or a cut short frame, else start writing the next
        // frame once the previous one flushed
        let mut state = shared.state.lock().expect("relay state not poisoned");
        if state.closed {
            return Err(protocol::Error::Closed.into());
        }
        if state.interrupted {
            return Err(Failure::Relay(state.cause.clone()));
        }
        if state
            .queue
            .front()
            .is_some_and(|frame| shared.clock.now() >= frame.deadline)
        {
            drop(state);
            continue;
        }
        if !backlog.active()
            && !state.writing
            && let Some(frame) = state.queue.front()
        {
            let message = Message::Binary(frame.bytes.clone());
            state.writing = true;
            backlog.start(shared.clock.now());
            match socket.write(message) {
                Ok(()) => {}
                Err(error) if would_block(&error) => {}
                Err(error) => return Err(socket_error(error)),
            }
        }

        // Flush, acknowledging a frame once all of it went out, and end the
        // socket when its output stalls
        if state.writing
            && state
                .queue
                .front()
                .is_some_and(|frame| shared.clock.now() >= frame.deadline)
        {
            drop(state);
            continue;
        }
        match socket.flush() {
            Ok(()) => {
                backlog.flushed();
                if state.writing {
                    state.writing = false;
                    let frame = state.pop().unwrap();
                    let deadline = shared.clock.now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT;
                    let _ = frame
                        .responder
                        .reply(schema::RelayOutboundResponse {}, deadline);
                }
            }
            Err(error) if would_block(&error) => backlog.start(shared.clock.now()),
            Err(error) => return Err(socket_error(error)),
        }
        let queued = !state.queue.is_empty();
        drop(state);
        if backlog.expired(shared.clock.now()) {
            return Err(Failure::Relay("relay socket write stalled for 5 s".into()));
        }

        // Read frames for the Ark while both limits leave room for a largest
        // message, in batches so writes and probes keep their turn
        let mut batch_full = false;
        for index in 0..32 {
            if inbound.len() >= MAX_INFLIGHT || bytes > MAX_BYTES - MAX_MESSAGE {
                break;
            }
            match socket.read() {
                Ok(Message::Binary(frame)) => {
                    let deadline = shared.clock.now() + INBOUND_TIMEOUT;
                    let length = frame.len();
                    let mut promise = requester.request(
                        schema::RelayInboundRequest {
                            frame: frame.to_vec(),
                        },
                        deadline,
                    )?;
                    let ready = Arc::new(AtomicBool::new(false));
                    let completed = ready.clone();
                    let wake = shared.wake.clone();
                    promise.notify(move || {
                        completed.store(true, Ordering::Release);
                        let _ = wake.wake();
                    });
                    inbound.push_back(Inbound {
                        promise,
                        ready,
                        bytes: length,
                        deadline,
                    });
                    bytes += length;
                }
                Ok(Message::Pong(bytes)) => heartbeat.pong(&bytes, Instant::now()),
                Ok(Message::Ping(_)) => {}
                Ok(Message::Close(_)) => {
                    return Err(Failure::Relay("cloud closed the relay".into()));
                }
                Ok(_) => {
                    return Err(Failure::Relay(
                        "cloud sent a text message on the relay".into(),
                    ));
                }
                Err(error) if would_block(&error) => break,
                Err(error) => return Err(socket_error(error)),
            }
            batch_full = index == 31;
        }

        // Probe the cloud when due, even while output or answers are backlogged
        if let Some(ping) = heartbeat.ping(Instant::now())? {
            backlog.start(shared.clock.now());
            match socket.write(Message::Ping(ping.to_vec().into())) {
                Ok(()) => {}
                Err(error) if would_block(&error) => {}
                Err(error) => return Err(socket_error(error)),
            }
        }

        // Have the timer thread wake the worker when an answer or write is due,
        // and loop at once while work is ready
        {
            let mut state = shared.state.lock().expect("relay state not poisoned");
            state.inbound = inbound.front().map(|frame| frame.deadline);
            state.write_deadline = backlog.deadline;
        }
        let _ = shared.changed.try_send(());
        if batch_full || (queued && !backlog.active()) {
            continue;
        }

        // Wait for readiness or a wakeup, bounded by the heartbeat
        let now = Instant::now();
        let timeout = heartbeat.deadline().saturating_duration_since(now);
        match poll.poll(&mut events, Some(timeout)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(error)),
        }
    }
}

/// Liveness probes of the socket, which other traffic cannot postpone.
#[derive(Debug)]
struct Heartbeat {
    /// When the next probe is due, while none awaits its pong.
    next: Instant,
    /// Id of the latest probe, wrapping, sent in network byte order.
    sequence: u64,
    /// Id and pong deadline of the probe awaiting its pong.
    pending: Option<(u64, Instant)>,
}

impl Heartbeat {
    /// Schedules the first probe 15 s after joining.
    #[expect(
        clippy::disallowed_methods,
        reason = "socket heartbeat uses real time with mio readiness"
    )]
    fn attach() -> Self {
        Self::new(Instant::now())
    }

    /// Schedules the first probe an interval after `now`.
    fn new(now: Instant) -> Self {
        Self {
            next: now + PING_INTERVAL,
            sequence: 0,
            pending: None,
        }
    }

    /// Returns a probe when one is due, and fails once a probe's pong is
    /// overdue.
    fn ping(&mut self, now: Instant) -> Result<Option<[u8; 8]>, Failure> {
        if let Some((_, deadline)) = self.pending {
            if now >= deadline {
                return Err(Failure::Relay("relay heartbeat timed out".into()));
            }
        } else if now >= self.next {
            self.sequence = self.sequence.wrapping_add(1);
            self.pending = Some((self.sequence, now + PONG_TIMEOUT));
            return Ok(Some(self.sequence.to_be_bytes()));
        }
        Ok(None)
    }

    /// Accepts only a timely pong matching the outstanding ping.
    fn pong(&mut self, bytes: &[u8], now: Instant) {
        if let Some((sequence, deadline)) = self.pending
            && now < deadline
            && bytes == sequence.to_be_bytes()
        {
            self.pending = None;
            self.next = now + PING_INTERVAL;
        }
    }

    /// Returns the next ping time or the current pong deadline.
    fn deadline(&self) -> Instant {
        self.pending.map_or(self.next, |(_, deadline)| deadline)
    }
}

/// Time bound on output the socket has not flushed yet.
#[derive(Debug, Default)]
struct Backlog {
    /// When the pending output must have flushed, while there is some.
    deadline: Option<Instant>,
}

impl Backlog {
    /// Starts the 5 s bound, keeping the deadline of a bound already running.
    fn start(&mut self, now: Instant) {
        self.deadline.get_or_insert(now + WRITE_TIMEOUT);
    }

    /// Clears the bound once a flush took all output.
    fn flushed(&mut self) {
        self.deadline = None;
    }

    /// Checks whether output still waits for a flush.
    fn active(&self) -> bool {
        self.deadline.is_some()
    }

    /// Checks whether the pending output missed its deadline by `now`.
    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}

/// Relay behavior over a loopback cloud and scripted Ark sessions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{
        http,
        tests::{TIMEOUT, response},
    };
    use crate::schema::host_to_ark::Content;
    use crate::testing::{Peer, test_clock, wait_deadline};
    use crate::{Ark, Error, TrustMode, trust::Realm};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use tungstenite::{
        WebSocket,
        handshake::server::{Request, Response},
    };

    /// Scripted Ark and its host, with relay requests delivered to the test.
    struct Fixture {
        /// Owner dropped before the peer's serving thread is joined.
        ark: Ark,
        /// Session services retained for observing worker state.
        services: Arc<Services>,
        /// Ark's requester, sending reverse frames through the real dispatcher.
        requester: Requester,
        /// Closes the peer's session to exercise dispatcher shutdown.
        peer_closer: protocol::Closer,
        /// Requests the test answers on behalf of the Ark.
        requests: mpsc::Receiver<(Content, Responder)>,
        /// Notices installed through the application's public observer API.
        notices: mpsc::Receiver<RelayNotice>,
        /// Peer whose lifetime includes all requests of the test.
        _peer: Peer,
    }

    impl Fixture {
        /// Attaches a synchronized Ark routed only to the loopback cloud.
        fn new(clock: &Clock, url: String) -> Self {
            let (requests, received) = mpsc::channel();
            let (send, requester) = mpsc::channel();
            let mut peer = Peer::spawn(
                clock,
                Box::new(move |session, request, responder| {
                    let deadline = session.clock().now() + Duration::from_secs(60);
                    match request {
                        Content::DeviceInfo(_) => {
                            let _ = send.send((session.requester(), session.closer()));
                            responder
                                .reply(
                                    schema::DeviceInfoResponse {
                                        cloud_synced: true,
                                        cloud_clock: session
                                            .clock()
                                            .system_time()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs(),
                                        ..Default::default()
                                    },
                                    deadline,
                                )
                                .unwrap();
                        }
                        Content::CloudSyncStart(_) => {
                            responder
                                .reply(
                                    schema::CloudSyncStartResponse { challenge: vec![3] },
                                    deadline,
                                )
                                .unwrap();
                        }
                        Content::CloudSyncFinish(_) => {
                            responder
                                .reply(schema::CloudSyncFinishResponse { accepted: 123 }, deadline)
                                .unwrap();
                        }
                        request => {
                            requests.send((request, responder)).unwrap();
                        }
                    }
                    true
                }),
            );

            // Keep the services, so tests can watch the relay's state
            let verifier = TrustMode::Recover(Box::new(peer.identity.clone()));
            let (session, _) = protocol::connect(peer.stream(), &verifier).unwrap();
            let services = Arc::new(Services {
                clock: clock.clone(),
                cloud: Some(http::tests::api(url, Realm::Hardware, clock)),
                state: Mutex::new(super::super::State::default()),
                updating: Mutex::new(()),
                notices: Arc::new(Observer::default()),
            });
            let mut ark = Ark::start(session, services.clone()).unwrap();
            let (notices, observed) = mpsc::channel();
            ark.set_relay_observer(move |notice| {
                let _ = notices.send(notice);
            });
            ark.client()
                .call(schema::DeviceInfoRequest {}, clock.now() + TIMEOUT)
                .unwrap();
            let (requester, peer_closer) = requester.recv().unwrap();
            Self {
                ark,
                services,
                requester,
                peer_closer,
                requests: received,
                notices: observed,
                _peer: peer,
            }
        }

        /// Sends a frame of the Ark with a long deadline, so the test sees the
        /// host's answer rather than a timeout.
        fn outbound(&self, frame: Vec<u8>) -> Promise<protocol::Message> {
            self.requester
                .request(
                    schema::RelayOutboundRequest { frame },
                    self.requester.clock().now() + Duration::from_secs(600),
                )
                .unwrap()
        }

        /// Answers the next join with the token expected by the loopback server.
        fn join(&self) {
            let (request, responder) = self.requests.recv().unwrap();
            assert!(matches!(request, Content::RelayJoin(_)));
            responder
                .reply(
                    schema::RelayJoinResponse {
                        auth: vec![0xfb, 0xff],
                    },
                    self.requester.clock().now() + TIMEOUT,
                )
                .unwrap();
        }

        /// Returns the relay's shared state, once the first frame created it.
        fn shared(&self) -> Arc<Shared> {
            self.services
                .state
                .lock()
                .unwrap()
                .relay
                .as_ref()
                .unwrap()
                .shared
                .clone()
        }
    }

    /// Creates a loopback listener and the API URL selected by the fixture.
    fn cloud() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        (listener, url)
    }

    /// Accepts one connection, bounding its I/O in case a test fails.
    fn accept(listener: &TcpListener) -> TcpStream {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TIMEOUT)).unwrap();
        stream
    }

    /// Reads the HTTP headers without consuming WebSocket bytes.
    fn headers(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).unwrap()
    }

    /// Serves the cloud's side of one forced sync, which precedes a later join.
    fn sync(listener: &TcpListener) {
        for (path, body) in [
            (
                "/v1/cloudsync/identity",
                r#"{"signer":"AQ==","crypto":"Ag=="}"#,
            ), // cloud keys
            (
                "/v1/cloudsync/time?challenge=03",
                r#"{"unixmilli":123,"signature":"BA=="}"#,
            ), // signed clock
        ] {
            let mut stream = accept(listener);
            assert!(headers(&mut stream).starts_with(&format!("GET {path} HTTP/1.1\r\n")));
            stream.write_all(response(200, body).as_bytes()).unwrap();
        }
    }

    /// Accepts a relay upgrade after checking its route and join token.
    #[allow(clippy::result_large_err)] // the callback's error is an HTTP response
    fn upgrade(stream: TcpStream) -> WebSocket<TcpStream> {
        tungstenite::accept_hdr(stream, |request: &Request, mut response: Response| {
            assert_eq!(request.uri().path(), "/v1/relaying");
            assert_eq!(
                request.headers()["Sec-WebSocket-Protocol"],
                "Relaying, Dark-Auth|-_8"
            );
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "Relaying".parse().unwrap());
            Ok(response)
        })
        .unwrap()
    }

    /// Reads the next binary message, skipping pings and pongs.
    fn frame(socket: &mut WebSocket<TcpStream>) -> Vec<u8> {
        loop {
            match socket.read().unwrap() {
                Message::Binary(bytes) => return bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected message {other:?}"),
            }
        }
    }

    /// Requires EOF, a reset or an abort from a raw socket read.
    fn assert_stream_closed(result: io::Result<usize>) {
        match result {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("expected a closed stream, got {result:?}"),
        }
    }

    /// Requires a close frame, EOF, a reset or an abort from the relay socket.
    fn assert_socket_closed(result: tungstenite::Result<Message>) {
        match result {
            Ok(Message::Close(_))
            | Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            )) => {}
            Err(tungstenite::Error::Io(error)) => assert_stream_closed(Err(error)),
            result => panic!("expected a closed relay socket, got {result:?}"),
        }
    }

    /// Checks that the host refused a frame with `UNAVAILABLE`, returning the
    /// cause.
    fn refused(promise: Promise<protocol::Message>) -> String {
        let protocol::Error::Remote(error) =
            promise.wait::<schema::RelayOutboundResponse>().unwrap_err()
        else {
            panic!("expected frame refusal")
        };
        assert_eq!(error.code, schema::ReservedErrors::Unavailable as u64);
        error.msg
    }

    /// Waits until the queue holds `count` frames.
    fn queued(shared: &Shared, count: usize) {
        while shared.state.lock().unwrap().queue.len() != count {
            thread::yield_now();
        }
    }

    /// Waits until a reconnect is scheduled, returning when it is due.
    fn retry(shared: &Shared) -> Instant {
        loop {
            if let Some(at) = shared.state.lock().unwrap().retry {
                return at;
            }
            thread::yield_now();
        }
    }

    /// Every request that may wait on an approval reaches the Ark without a join,
    /// and the Ark's frame joins the relay.
    #[test]
    fn test_approval_joins_on_frame() {
        /// Sends one such request through its typed setup.
        type Send = fn(&crate::Client) -> Result<(), Error>;
        let cases: [Send; 6] = [
            |client| {
                client
                    .send(schema::UnlockRequest {}, Timing::inactivity(TIMEOUT))
                    .map(drop)
            }, // unlock
            |client| {
                client
                    .send(
                        schema::ExecutionScheduleRequest::default(),
                        Timing::inactivity(TIMEOUT),
                    )
                    .map(drop)
            }, // execution
            |client| {
                client
                    .send(
                        schema::SlotRepairRequest::default(),
                        Timing::inactivity(TIMEOUT),
                    )
                    .map(drop)
            }, // repair
            |client| {
                client
                    .send(
                        schema::SlotDeleteRequest::default(),
                        Timing::inactivity(TIMEOUT),
                    )
                    .map(drop)
            }, // deletion
            |client| {
                client
                    .send(
                        schema::FirmwareUpdatePrepRequest::default(),
                        Timing::inactivity(TIMEOUT),
                    )
                    .map(drop)
            }, // firmware
            |client| {
                client
                    .send(
                        schema::SlotUploadStartRequest::default(),
                        Timing::inactivity(TIMEOUT),
                    )
                    .map(drop)
            }, // upload
        ];
        for (i, send) in cases.into_iter().enumerate() {
            // Neither a status nor the request itself joins the relay
            let clock = test_clock().clock();
            let (listener, url) = cloud();
            let fixture = Fixture::new(&clock, url);
            assert!(
                fixture.services.state.lock().unwrap().relay.is_none(),
                "{i}"
            );
            send(&fixture.ark.client()).unwrap();
            let (request, _approval) = fixture.requests.recv().unwrap();
            assert!(!matches!(request, Content::RelayJoin(_)), "{i}");
            assert!(
                fixture.services.state.lock().unwrap().relay.is_none(),
                "{i}"
            );

            // The Ark's frame joins the relay and reaches the cloud unchanged,
            // acknowledged only afterwards
            let (seen, received) = mpsc::channel();
            let (done, finish) = mpsc::channel();
            let server = thread::spawn(move || {
                let mut socket = upgrade(accept(&listener));
                seen.send(frame(&mut socket)).unwrap();
                finish.recv().unwrap();
            });
            let mut sent = fixture.outbound(vec![0xff, 0, i as u8, 0x80]);
            let (acked, acks) = mpsc::channel();
            sent.notify(move || {
                acked.send(()).unwrap();
            });
            let (request, responder) = fixture.requests.recv().unwrap();
            assert!(matches!(request, Content::RelayJoin(_)), "{i}");
            assert!(acks.try_recv().is_err(), "{i}");
            responder
                .reply(
                    schema::RelayJoinResponse {
                        auth: vec![0xfb, 0xff],
                    },
                    clock.now() + TIMEOUT,
                )
                .unwrap();
            assert_eq!(received.recv().unwrap(), [0xff, 0, i as u8, 0x80], "{i}");
            sent.wait::<schema::RelayOutboundResponse>().unwrap();
            done.send(()).unwrap();
            fixture.ark.close();
            server.join().unwrap();
        }
    }

    /// Multiple frames in both directions retain their bytes and arrival order.
    #[test]
    fn test_bidirectional_order() {
        let clock = test_clock().clock();
        let (listener, url) = cloud();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            for i in 0..12u8 {
                socket
                    .send(Message::Binary(vec![0xff, i, 0].into()))
                    .unwrap();
            }
            for i in 0..12u8 {
                assert_eq!(frame(&mut socket), [0, i, 0xfe], "{i}");
            }
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent: Vec<_> = (0..12)
            .map(|i| fixture.outbound(vec![0, i, 0xfe]))
            .collect();
        fixture.join();

        // Keep several incoming frames unanswered while outgoing frames flush
        let mut responders = Vec::new();
        for i in 0..12u8 {
            let (request, responder) = fixture.requests.recv().unwrap();
            let Content::RelayInbound(request) = request else {
                panic!("expected frame")
            };
            assert_eq!(request.frame, [0xff, i, 0], "{i}");
            responders.push(responder);
        }
        for sent in sent {
            sent.wait::<schema::RelayOutboundResponse>().unwrap();
        }
        for responder in responders.into_iter().rev() {
            responder
                .reply(schema::RelayInboundResponse {}, clock.now() + TIMEOUT)
                .unwrap();
        }
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// Both outbound bounds refuse excess frames and recover after the queue drains.
    #[test]
    fn test_outbound_limits() {
        for (i, (count, size)) in [(128, 1), (16, 1024 * 1024)].into_iter().enumerate() {
            // frame and byte limits
            let clock = test_clock().clock();
            let (listener, url) = cloud();
            let (done, finish) = mpsc::channel();
            let server = thread::spawn(move || {
                let mut socket = upgrade(accept(&listener));
                for index in 0..count {
                    assert_eq!(frame(&mut socket), vec![index as u8; size], "{index}");
                }
                assert_eq!(frame(&mut socket), [99]);
                finish.recv().unwrap();
            });
            let fixture = Fixture::new(&clock, url);
            let mut sent = vec![fixture.outbound(vec![0; size])];
            let (request, join) = fixture.requests.recv().unwrap();
            assert!(matches!(request, Content::RelayJoin(_)), "{i}");
            let shared = fixture.shared();
            for index in 1..count {
                sent.push(fixture.outbound(vec![index as u8; size]));
            }
            queued(&shared, count);
            assert!(
                refused(fixture.outbound(vec![1])).contains("queue full"),
                "{i}"
            );

            // The dispatcher still serves status while its relay join waits
            fixture
                .ark
                .client()
                .call(schema::DeviceInfoRequest {}, clock.now() + TIMEOUT)
                .unwrap();
            join.reply(
                schema::RelayJoinResponse {
                    auth: vec![0xfb, 0xff],
                },
                clock.now() + TIMEOUT,
            )
            .unwrap();
            for sent in sent {
                sent.wait::<schema::RelayOutboundResponse>().unwrap();
            }
            fixture
                .outbound(vec![99])
                .wait::<schema::RelayOutboundResponse>()
                .unwrap();
            done.send(()).unwrap();
            fixture.ark.close();
            server.join().unwrap();
        }
    }

    /// Socket reads stop at 128 unanswered frames and resume after one answer.
    #[test]
    fn test_inbound_limit() {
        let clock = test_clock().clock();
        let (listener, url) = cloud();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            for i in 0..129u16 {
                socket
                    .send(Message::Binary(i.to_be_bytes().to_vec().into()))
                    .unwrap();
            }
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        let mut held = Vec::new();
        for i in 0..128u16 {
            let (request, responder) = fixture.requests.recv().unwrap();
            let Content::RelayInbound(request) = request else {
                panic!("expected frame")
            };
            assert_eq!(request.frame, i.to_be_bytes(), "{i}");
            held.push(responder);
        }

        // A status round trip gives the worker time to read past the limit,
        // which it must not
        fixture
            .ark
            .client()
            .call(schema::DeviceInfoRequest {}, clock.now() + TIMEOUT)
            .unwrap();
        assert!(fixture.requests.try_recv().is_err());
        held.pop()
            .unwrap()
            .reply(schema::RelayInboundResponse {}, clock.now() + TIMEOUT)
            .unwrap();
        let (request, responder) = fixture.requests.recv().unwrap();
        let Content::RelayInbound(request) = request else {
            panic!("expected resumed frame")
        };
        assert_eq!(request.frame, 128u16.to_be_bytes());
        responder
            .reply(schema::RelayInboundResponse {}, clock.now() + TIMEOUT)
            .unwrap();
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// A frame whose hold runs out during a stalled upgrade is refused, and never
    /// sent later.
    #[test]
    fn test_hold_deadline() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (stalled, stalls) = mpsc::channel();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            headers(&mut stream);
            stalled.send(()).unwrap();
            assert_stream_closed(stream.read(&mut [0]));
            sync(&listener);
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [2]);
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        stalls.recv().unwrap();

        // Let the frame's hold run out on the test clock, cutting the upgrade short
        let shared = fixture.shared();
        let (rearming, paused) = crossbeam_channel::bounded(0);
        let (resume, resumed) = crossbeam_channel::bounded(0);
        *shared.rearming.lock().unwrap() = Some((rearming, resumed));
        shared.signal();
        paused.recv().unwrap();
        tester.advance(Duration::from_secs(10));
        resume.send(()).unwrap();
        assert!(refused(sent).contains("10 s"));
        while !shared.state.lock().unwrap().unsynced {
            thread::yield_now();
        }
        let sent = fixture.outbound(vec![2]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// A join due while rearming is interrupted while later timers keep working.
    #[test]
    fn test_join_deadline_rearm() {
        // Pause the timer thread after its first expiry check
        let mut tester = test_clock();
        let clock = tester.clock();
        let deadline = clock.now() + Duration::from_secs(10);
        let attempt = Arc::new(Attempt::new(&clock));
        let poll = Poll::new().unwrap();
        let (changed, changes) = crossbeam_channel::bounded(1);
        let (rearming, paused) = crossbeam_channel::bounded(0);
        let (resume, resumed) = crossbeam_channel::bounded(0);
        let shared = Arc::new(Shared {
            clock: clock.clone(),
            state: Mutex::new(State {
                joining: Some(deadline),
                waiter: Some((attempt.clone(), deadline)),
                ..Default::default()
            }),
            wake: Arc::new(Waker::new(poll.registry(), WAKE).unwrap()),
            changed,
            notices: Arc::new(Observer::default()),
            rearming: Mutex::new(Some((rearming, resumed))),
        });
        let timer = thread::spawn({
            let shared = shared.clone();
            move || timers(shared, changes)
        });
        paused.recv().unwrap();

        // Pass the join deadline before the thread chooses its next timer
        tester.advance_to(deadline);
        resume.send(()).unwrap();
        assert!(matches!(
            attempt.wait(clock.now() + Duration::from_secs(1)),
            Err(Failure::Wire(protocol::Error::Timeout))
        ));

        // Leave the interrupted join in place while a later deadline is armed
        {
            let mut state = shared.state.lock().unwrap();
            assert!(state.interrupted);
            assert_eq!(state.joining, Some(deadline));
            state.retry = Some(clock.now() + Duration::from_secs(1));
        }
        shared.signal();
        tester.wait_timers(1);
        shared.close();
        timer.join().unwrap();
    }

    /// A refused join refuses every waiting frame with its cause, reported once.
    #[test]
    #[allow(clippy::result_large_err)] // the callback's error is an HTTP response
    fn test_join_failures() {
        for i in 0..3 {
            let clock = test_clock().clock();
            let (listener, url) = cloud();
            let (done, finish) = mpsc::channel();
            let server = thread::spawn(move || {
                if i == 1 {
                    let mut stream = accept(&listener);
                    headers(&mut stream);
                    stream
                        .write_all(response(503, "join unavailable").as_bytes())
                        .unwrap();
                } else if i == 2 {
                    let _ = tungstenite::accept_hdr(
                        accept(&listener),
                        |_: &Request, mut response: Response| {
                            response
                                .headers_mut()
                                .insert("Sec-WebSocket-Protocol", "Dark-Auth|-_8".parse().unwrap());
                            Ok(response)
                        },
                    )
                    .unwrap();
                }
                sync(&listener);
                let mut socket = upgrade(accept(&listener));
                assert_eq!(frame(&mut socket), [4]);
                finish.recv().unwrap();
            });
            let fixture = Fixture::new(&clock, url);
            let sent: Vec<_> = (0..3).map(|i| fixture.outbound(vec![i])).collect();
            let (request, responder) = fixture.requests.recv().unwrap();
            assert!(matches!(request, Content::RelayJoin(_)), "{i}");
            queued(&fixture.shared(), 3);
            if i == 0 {
                responder
                    .fail(
                        schema::Error::new(900, "example join refusal"),
                        clock.now() + TIMEOUT,
                    )
                    .unwrap();
            } else {
                responder
                    .reply(
                        schema::RelayJoinResponse {
                            auth: vec![0xfb, 0xff],
                        },
                        clock.now() + TIMEOUT,
                    )
                    .unwrap();
            }

            // Every waiting frame gets the same cause, and the observer hears it once
            let causes: Vec<_> = sent.into_iter().map(refused).collect();
            assert!(causes.iter().all(|cause| cause == &causes[0]), "{i}");
            let expected = ["example join refusal", "503", "subprotocol"][i];
            assert!(causes[0].contains(expected), "{i}");
            assert!(
                matches!(fixture.notices.recv().unwrap(), RelayNotice::Failed(cause) if cause == causes[0]),
                "{i}"
            );
            assert!(fixture.notices.try_recv().is_err(), "{i}");

            // A later frame joins again, after a forced sync
            let sent = fixture.outbound(vec![4]);
            fixture.join();
            sent.wait::<schema::RelayOutboundResponse>().unwrap();
            done.send(()).unwrap();
            fixture.ark.close();
            server.join().unwrap();
        }
    }

    /// A stalled join refuses its frame with the unsent cause, not an earlier refusal.
    #[test]
    fn test_stalled_join_cause() {
        // Refuse the first join before it opens a cloud socket
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (stalled, stalls) = mpsc::channel();
        let server = thread::spawn(move || {
            sync(&listener);
            let mut stream = accept(&listener);
            headers(&mut stream);
            stalled.send(()).unwrap();
            assert_stream_closed(stream.read(&mut [0]));
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        let (request, responder) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayJoin(_)));
        responder
            .fail(
                schema::Error::new(900, "example join refusal"),
                clock.now() + TIMEOUT,
            )
            .unwrap();
        let cause = refused(sent);
        assert!(cause.contains("example join refusal"));
        assert!(
            matches!(fixture.notices.recv().unwrap(), RelayNotice::Failed(notice) if notice == cause)
        );

        // Let the next frame's hold expire while its upgrade waits
        let sent = fixture.outbound(vec![2]);
        fixture.join();
        stalls.recv().unwrap();
        tester.wait_timers(1);
        tester.advance(Duration::from_secs(10));
        assert_eq!(refused(sent), UNSENT);
        assert!(
            matches!(fixture.notices.recv().unwrap(), RelayNotice::Failed(cause) if cause == UNSENT)
        );
        assert!(fixture.notices.try_recv().is_err());
        fixture.ark.close();
        server.join().unwrap();
    }

    /// A refused inbound frame is reported with its remote cause and never retried.
    #[test]
    fn test_inbound_refusal() {
        let clock = test_clock().clock();
        let (listener, url) = cloud();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            socket.send(Message::Binary(vec![9].into())).unwrap();
            socket.send(Message::Binary(vec![8].into())).unwrap();
            assert_eq!(frame(&mut socket), [2]);
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        let (request, responder) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayInbound(request) if request.frame == [9]));
        let error = schema::Error::new(712, "example frame refusal");
        responder
            .fail(error.clone(), clock.now() + TIMEOUT)
            .unwrap();
        assert!(
            matches!(fixture.notices.recv().unwrap(), RelayNotice::Refused(refused) if refused == error)
        );

        // A following inbound frame and another outbound frame keep flowing
        let (request, responder) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayInbound(request) if request.frame == [8]));
        responder
            .reply(schema::RelayInboundResponse {}, clock.now() + TIMEOUT)
            .unwrap();
        fixture
            .outbound(vec![2])
            .wait::<schema::RelayOutboundResponse>()
            .unwrap();
        assert!(fixture.requests.try_recv().is_err());
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// An unanswered inbound frame closes the socket at its 10 s request deadline.
    #[test]
    fn test_inbound_timeout() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            socket.send(Message::Binary(vec![9].into())).unwrap();
            assert_socket_closed(socket.read());
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        let (request, _held) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayInbound(_)));
        tester.wait_timers(1);
        tester.advance(Duration::from_secs(10));
        server.join().unwrap();
        let shared = fixture.shared();
        retry(&shared);
        assert!(shared.state.lock().unwrap().cause.contains("10 s"));
    }

    /// A dropped socket is replaced only after its jittered backoff and a forced sync.
    #[test]
    fn test_reconnect() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            socket.close(None).unwrap();
            drop(socket);
            sync(&listener);
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [2]);
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        let shared = fixture.shared();
        let deadline = retry(&shared);
        assert!(
            (Duration::from_millis(800)..=Duration::from_millis(1200))
                .contains(&deadline.duration_since(clock.now()))
        );

        // The relay rejoins after backoff even without another frame
        tester.wait_timers(1);
        assert!(fixture.requests.try_recv().is_err());
        tester.advance_to(deadline);
        fixture.join();
        fixture
            .outbound(vec![2])
            .wait::<schema::RelayOutboundResponse>()
            .unwrap();
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// A frame pulls a long scheduled retry forward without resetting its failure window.
    #[test]
    fn test_frame_pulls_retry_forward() {
        // Drop the first socket and synchronize before each replacement attempt
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            socket.close(None).unwrap();
            drop(socket);
            for _ in 0..5 {
                sync(&listener);
            }
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [2]);
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        let shared = fixture.shared();

        // Four failed replacements schedule a delay longer than the frame hold
        for _ in 0..4 {
            let deadline = retry(&shared);
            tester.wait_timers(1);
            tester.advance_to(deadline);
            let (request, responder) = fixture.requests.recv().unwrap();
            assert!(matches!(request, Content::RelayJoin(_)));
            responder
                .fail(
                    schema::Error::new(900, "example reconnect refusal"),
                    clock.now() + TIMEOUT,
                )
                .unwrap();
        }
        let scheduled = retry(&shared);
        assert!(scheduled > clock.now() + Duration::from_secs(10));
        let (failing, backoff) = {
            let state = shared.state.lock().unwrap();
            (state.failing, state.backoff)
        };

        // A new frame joins at once, before the scheduled retry is due
        let sent = fixture.outbound(vec![2]);
        let (request, responder) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayJoin(_)));
        assert!(clock.now() < scheduled);
        {
            let state = shared.state.lock().unwrap();
            assert!(state.retry.is_none());
            assert_eq!(state.failing, failing);
            assert_eq!(state.backoff, backoff);
        }
        responder
            .reply(
                schema::RelayJoinResponse {
                    auth: vec![0xfb, 0xff],
                },
                clock.now() + TIMEOUT,
            )
            .unwrap();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// Consecutive failures stop retrying after 120 s until another Ark frame arrives.
    #[test]
    fn test_retry_window() {
        // Serve synchronization on demand until the test allows a successful join
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (done, finish) = mpsc::channel();
        let (refresh, refreshes) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            socket.close(None).unwrap();
            drop(socket);
            while let Ok(()) = refreshes.recv() {
                sync(&listener);
            }
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [2]);
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        let shared = fixture.shared();
        retry(&shared);
        let end = shared.state.lock().unwrap().failing.unwrap() + Duration::from_secs(120);

        // Each failure doubles the delay to 30 s, with 20% jitter on each wait
        let mut base = Duration::from_secs(1);
        loop {
            let Some(next) = shared.state.lock().unwrap().retry else {
                break;
            };
            assert!(next < end);
            assert!(
                (base.mul_f64(0.8)..=base.mul_f64(1.2).min(Duration::from_secs(30)))
                    .contains(&next.duration_since(clock.now()))
            );
            refresh.send(()).unwrap();
            tester.wait_timers(1);
            tester.advance_to(next);
            let (request, responder) = fixture.requests.recv().unwrap();
            assert!(matches!(request, Content::RelayJoin(_)));
            responder
                .fail(
                    schema::Error::new(900, "example reconnect refusal"),
                    clock.now() + TIMEOUT,
                )
                .unwrap();
            while shared.state.lock().unwrap().joining.is_some() {
                thread::yield_now();
            }
            base = (base * 2).min(Duration::from_secs(30));
        }
        assert!(clock.now() < end);
        tester.advance_to(end + Duration::from_secs(60));
        assert!(fixture.requests.try_recv().is_err());

        // A new frame starts a fresh window at once, after a forced sync
        refresh.send(()).unwrap();
        drop(refresh);
        let sent = fixture.outbound(vec![2]);
        let (request, responder) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayJoin(_)));
        {
            let state = shared.state.lock().unwrap();
            assert!(state.failing.is_none());
            assert_eq!(state.backoff, Duration::from_secs(1));
        }
        responder
            .reply(
                schema::RelayJoinResponse {
                    auth: vec![0xfb, 0xff],
                },
                clock.now() + TIMEOUT,
            )
            .unwrap();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// An explicit join needs no frame, and waits only until the caller's
    /// deadline.
    #[test]
    fn test_explicit_attachment() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let fixture = Fixture::new(&clock, url);
        let client = fixture.ark.client();
        let deadline = clock.now() + Duration::from_secs(2);
        let waiting = thread::spawn(move || client.attach_relay(deadline));
        let (request, _held) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayJoin(_)));
        wait_deadline(&tester, deadline);
        tester.advance_to(deadline);
        assert!(matches!(waiting.join().unwrap(), Err(Error::Timeout)));
        drop(listener);
    }

    /// Closing the owner or losing the session stops both threads and closes the
    /// socket.
    #[test]
    fn test_owner_and_session_close() {
        for (i, owner) in [true, false].into_iter().enumerate() {
            let clock = test_clock().clock();
            let (listener, url) = cloud();
            let server = thread::spawn(move || {
                let mut socket = upgrade(accept(&listener));
                assert_eq!(frame(&mut socket), [1]);
                assert_socket_closed(socket.read());
            });
            let fixture = Fixture::new(&clock, url);
            let sent = fixture.outbound(vec![1]);
            fixture.join();
            sent.wait::<schema::RelayOutboundResponse>().unwrap();
            let shared = fixture.shared();
            let weak = Arc::downgrade(&shared);
            let retained = fixture.ark.client();
            if owner {
                drop(fixture.ark);
            } else {
                fixture.peer_closer.close();
            }
            server.join().unwrap();
            assert!(shared.state.lock().unwrap().queue.is_empty(), "{i}");
            drop(shared);
            while weak.upgrade().is_some() {
                thread::yield_now();
            }
            assert!(
                matches!(
                    retained.attach_relay(clock.now() + TIMEOUT),
                    Err(Error::Closed | Error::Disconnected(_))
                ),
                "{i}"
            );
        }
    }

    /// Ending a relay refuses queued frames and interrupts an unfinished upgrade.
    #[test]
    fn test_close_during_join() {
        let clock = test_clock().clock();
        let (listener, url) = cloud();
        let (stalled, stalls) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            headers(&mut stream);
            stalled.send(()).unwrap();
            assert_stream_closed(stream.read(&mut [0]));
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        stalls.recv().unwrap();
        fixture.services.close();
        assert!(refused(sent).contains("closed"));
        server.join().unwrap();
    }

    /// A partly written frame expires without completing its message or being acknowledged.
    #[test]
    fn test_partial_write_deadline() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (read, reading) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            reading.recv().unwrap();
            assert_socket_closed(socket.read());
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![0; 1024 * 1024]);
        let (request, join) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayJoin(_)));
        let shared = fixture.shared();
        shared.state.lock().unwrap().write_limit =
            Some(Arc::new(std::sync::atomic::AtomicUsize::new(1024)));

        // Spend 6 s of both frame holds waiting for the join
        let held = fixture.outbound(vec![1]);
        queued(&shared, 2);
        tester.advance(Duration::from_secs(6));
        join.reply(
            schema::RelayJoinResponse {
                auth: vec![0xfb, 0xff],
            },
            clock.now() + TIMEOUT,
        )
        .unwrap();

        // Wait until a partial write leaves the first frame on the queue
        loop {
            let state = shared.state.lock().unwrap();
            if let Some(deadline) = state.write_deadline {
                assert!(state.writing);
                assert_eq!(state.bytes, 1024 * 1024 + 1);
                assert_eq!(state.queue.len(), 2);
                assert_eq!(deadline, clock.now() + Duration::from_secs(5));
                break;
            }
            drop(state);
            thread::yield_now();
        }
        tester.wait_timers(1);
        tester.advance(Duration::from_secs(4));

        // Expiry refuses both the partial frame and the frames waiting behind it
        assert!(refused(sent).contains("10 s"));
        assert!(refused(held).contains("10 s"));
        read.send(()).unwrap();
        server.join().unwrap();
    }

    /// A 5 s write stall replaces the socket and preserves frames still in their hold.
    #[test]
    fn test_write_stall_reconnect() {
        // Leave the first message incomplete, then accept its replacement
        let mut tester = test_clock();
        let clock = tester.clock();
        let (listener, url) = cloud();
        let (closed, closing) = mpsc::channel();
        let (done, finish) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            closing.recv().unwrap();
            assert_socket_closed(socket.read());
            drop(socket);
            sync(&listener);
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), vec![0; 1024 * 1024]);
            assert_eq!(frame(&mut socket), [1]);
            assert_eq!(frame(&mut socket), [2]);
            finish.recv().unwrap();
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![0; 1024 * 1024]);
        let (request, join) = fixture.requests.recv().unwrap();
        assert!(matches!(request, Content::RelayJoin(_)));
        let shared = fixture.shared();
        shared.state.lock().unwrap().write_limit =
            Some(Arc::new(std::sync::atomic::AtomicUsize::new(1024)));
        join.reply(
            schema::RelayJoinResponse {
                auth: vec![0xfb, 0xff],
            },
            clock.now() + TIMEOUT,
        )
        .unwrap();
        let deadline = clock.now() + Duration::from_secs(5);
        loop {
            if let Some(published) = shared.state.lock().unwrap().write_deadline {
                assert_eq!(published, deadline);
                break;
            }
            thread::yield_now();
        }
        wait_deadline(&tester, deadline);

        // Queuing another frame after 3 s keeps the original stall deadline
        tester.advance(Duration::from_secs(3));
        let held = fixture.outbound(vec![1]);
        queued(&shared, 2);
        {
            let mut state = shared.state.lock().unwrap();
            assert!(state.connected);
            assert!(state.writing);
            assert_eq!(state.write_deadline, Some(deadline));
            state.write_limit = None;
        }
        tester.advance_to(deadline);
        let reconnect = retry(&shared);
        {
            let state = shared.state.lock().unwrap();
            assert!(!state.connected);
            assert!(state.write_deadline.is_none());
            assert_eq!(state.queue.len(), 2);
            assert!(state.cause.contains("write stalled for 5 s"));
        }
        closed.send(()).unwrap();

        // Both frames leave on the replacement before the first hold runs out
        assert!(reconnect < deadline + Duration::from_secs(5));
        tester.advance_to(reconnect);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        held.wait::<schema::RelayOutboundResponse>().unwrap();

        // Completed flushes leave the socket open past their former deadlines
        tester.advance(Duration::from_secs(6));
        fixture
            .outbound(vec![2])
            .wait::<schema::RelayOutboundResponse>()
            .unwrap();
        assert!(fixture.requests.try_recv().is_err());
        done.send(()).unwrap();
        fixture.ark.close();
        server.join().unwrap();
    }

    /// An explicit join succeeds without outbound traffic and is shared by later callers.
    #[test]
    fn test_explicit_join_reuse() {
        let clock = test_clock().clock();
        let (listener, url) = cloud();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_socket_closed(socket.read());
        });
        let fixture = Fixture::new(&clock, url);
        let client = fixture.ark.client();
        let joining = thread::spawn(move || client.attach_relay(Timing::inactivity(TIMEOUT)));
        fixture.join();
        joining.join().unwrap().unwrap();

        // A later explicit join reuses the open socket
        fixture
            .ark
            .client()
            .attach_relay(Timing::inactivity(TIMEOUT))
            .unwrap();
        assert!(fixture.requests.try_recv().is_err());
        fixture.ark.close();
        server.join().unwrap();
    }

    /// A text message ends the socket instead of reaching the Ark.
    #[test]
    fn test_text_refusal() {
        let clock = test_clock().clock();
        let (listener, url) = cloud();
        let server = thread::spawn(move || {
            let mut socket = upgrade(accept(&listener));
            assert_eq!(frame(&mut socket), [1]);
            socket.send(Message::Text("example text".into())).unwrap();
            assert_socket_closed(socket.read());
        });
        let fixture = Fixture::new(&clock, url);
        let sent = fixture.outbound(vec![1]);
        fixture.join();
        sent.wait::<schema::RelayOutboundResponse>().unwrap();
        server.join().unwrap();
        retry(&fixture.shared());
        assert!(fixture.requests.try_recv().is_err());
    }

    /// Only timely matching pongs acknowledge probes and schedule the next ping.
    #[test]
    fn test_heartbeat() {
        let mut tester = test_clock();
        let clock = tester.clock();
        let mut heartbeat = Heartbeat::new(clock.now());
        assert!(heartbeat.ping(clock.now()).unwrap().is_none());
        tester.advance(Duration::from_secs(15));
        let first = heartbeat.ping(clock.now()).unwrap().unwrap();
        let deadline = heartbeat.deadline();
        heartbeat.pong(&[0; 8], clock.now());
        assert_eq!(heartbeat.deadline(), deadline);
        heartbeat.pong(&first, clock.now());

        // A later probe needs its own pong before its fixed 10 s deadline
        tester.advance(Duration::from_secs(15));
        let second = heartbeat.ping(clock.now()).unwrap().unwrap();
        assert_ne!(first, second);
        heartbeat.pong(&first, clock.now());
        tester.advance(Duration::from_secs(10));
        heartbeat.pong(&second, clock.now());
        assert!(heartbeat.ping(clock.now()).is_err());
    }
}
