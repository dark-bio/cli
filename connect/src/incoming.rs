// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Request dispatch that forwards cloud traffic and queues the rest for the
//! application.

use crate::cloud::Services;
use darkbio_wire::protocol::{self, Responder, Session, schema};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// Bounded queue of the requests left for the application.
///
/// Closure discards requests and keeps the first ending reason for every
/// subsequent receive.
#[derive(Debug, Default)]
pub(crate) struct Incoming {
    /// Queue and closure state, changed under one lock.
    state: Mutex<State>,
    /// Condition that wakes the application for a request or closure.
    ready: Condvar,
}

/// Queue accounting and closure, changed together under the incoming lock.
#[derive(Debug, Default)]
struct State {
    /// Requests in arrival order, each retaining its unanswered responder.
    queue: VecDeque<(schema::ark_to_host::Content, Responder)>,
    /// Protobuf size of the queued requests, charged against the byte limit.
    bytes: usize,
    /// First reason the original session ended.
    error: Option<protocol::Error>,
}

impl Incoming {
    /// Takes the next queued request, blocking until one arrives or the
    /// session ends.
    ///
    /// The dispatcher owns the wire receives, so this never competes with
    /// relay traffic. A closed queue returns its first ending reason.
    pub(crate) fn recv(
        &self,
    ) -> Result<(schema::ark_to_host::Content, Responder), protocol::Error> {
        let mut state = self.state.lock().expect("incoming requests not poisoned");
        loop {
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            if let Some((request, responder)) = state.queue.pop_front() {
                state.bytes -= request.encoded_len();
                return Ok((request, responder));
            }
            state = self
                .ready
                .wait(state)
                .expect("incoming requests not poisoned");
        }
    }

    /// Discards queued requests and wakes receivers when the session ends.
    ///
    /// Only the first ending reason is kept, and later closes change nothing.
    pub(crate) fn close(&self, error: protocol::Error) {
        let mut state = self.state.lock().expect("incoming requests not poisoned");
        if state.error.is_none() {
            state.error = Some(error);
            state.queue.clear();
            state.bytes = 0;
            self.ready.notify_all();
        }
    }

    /// Queues a request for the application, refusing it when the backlog is
    /// full.
    ///
    /// A full queue refuses the request with `UNAVAILABLE` and leaves the
    /// session open, so relay traffic is never held up. A closed queue drops
    /// the request.
    fn push(&self, request: schema::ark_to_host::Content, responder: Responder) {
        // A closed queue drops the request
        let mut state = self.state.lock().expect("incoming requests not poisoned");
        if state.error.is_some() {
            return;
        }

        // Refuse a request over either limit, answering it outside the lock
        let bytes = request.encoded_len();
        if state.queue.len() >= protocol::DEFAULT_MAX_INBOUND_REQUESTS
            || bytes > protocol::DEFAULT_MAX_INBOUND_BYTES.saturating_sub(state.bytes)
        {
            drop(state);
            let error = schema::Error::reserved(
                schema::ReservedErrors::Unavailable,
                "host request queue full",
            );
            let deadline = responder.clock().now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT;
            let _ = responder.fail(error, deadline);
            return;
        }

        // Queue the request and wake one receiver
        state.bytes += bytes;
        state.queue.push_back((request, responder));
        self.ready.notify_one();
    }
}

/// Receives every request of the session, forwarding relay traffic to cloud
/// services and queueing the rest for the application.
///
/// It owns the wire receiver, so companion traffic progresses without
/// application receives. Once the session ends, it ends cloud setup and the
/// queue with the same reason.
pub(crate) fn dispatch(mut session: Session, services: Arc<Services>, incoming: Arc<Incoming>) {
    let error = loop {
        // Take the next request, stopping at the session's end or at a message
        // that does not convert to an Ark request
        let (message, responder) = match session.recv() {
            Ok(request) => request,
            Err(error) => break error,
        };
        let request = match message.try_into() {
            Ok(request) => request,
            Err(error) => break error,
        };

        // Relay requests go to cloud services, which return any lacking a
        // cloud route
        if let schema::ark_to_host::Content::RelayReq(request) = request {
            if let Some((request, responder)) =
                services.forward(&session.requester(), request, responder)
            {
                incoming.push(schema::ark_to_host::Content::RelayReq(request), responder);
            }
        } else {
            incoming.push(request, responder);
        }
    };

    // End cloud setup and the application queue with the same reason
    services.end(error.clone());
    incoming.close(error);
}
