// connect-rs: client library for Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Dispatches cloud traffic while retaining other requests for the application.

use crate::cloud::Services;
use darkbio_wire::protocol::{self, Responder, Session, schema};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Requests not claimed by cloud services, followed by the session's ending reason.
#[derive(Debug, Default)]
pub(crate) struct Incoming {
    state: Mutex<State>, // Queue and closure change under the same lock
    ready: Condvar,      // Wakes the application for a request or closure
}

#[derive(Debug, Default)]
struct State {
    queue: VecDeque<(schema::ark_to_host::Content, Responder)>,
    bytes: usize,                   // Decoded messages charged by their protobuf size
    error: Option<protocol::Error>, // First reason the original session ended
}

impl Incoming {
    /// Takes a request without competing with the relay for wire receives.
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
    pub(crate) fn close(&self, error: protocol::Error) {
        let mut state = self.state.lock().expect("incoming requests not poisoned");
        if state.error.is_none() {
            state.error = Some(error);
            state.queue.clear();
            state.bytes = 0;
            self.ready.notify_all();
        }
    }

    /// Keeps application backlog bounded without holding up relay traffic.
    fn push(&self, request: schema::ark_to_host::Content, responder: Responder) {
        let mut state = self.state.lock().expect("incoming requests not poisoned");
        if state.error.is_some() {
            return;
        }
        let bytes = request.encoded_len();
        if state.queue.len() >= protocol::DEFAULT_MAX_INBOUND_REQUESTS
            || bytes > protocol::DEFAULT_MAX_INBOUND_BYTES.saturating_sub(state.bytes)
        {
            drop(state);
            let _ = responder.fail(
                schema::Error::reserved(
                    schema::ReservedErrors::Unavailable,
                    "host request queue full",
                ),
                Instant::now() + protocol::DEFAULT_AUTOREPLY_TIMEOUT,
            );
            return;
        }
        state.bytes += bytes;
        state.queue.push_back((request, responder));
        self.ready.notify_one();
    }
}

/// Owns the wire receiver so companion traffic progresses without application I/O.
pub(crate) fn dispatch(mut session: Session, services: Arc<Services>, incoming: Arc<Incoming>) {
    let error = loop {
        let (message, responder) = match session.recv() {
            Ok(request) => request,
            Err(error) => break error,
        };
        let request = match message.try_into() {
            Ok(request) => request,
            Err(error) => break error,
        };
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
    services.end(error.clone());
    incoming.close(error);
}
