// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A JSON-RPC [`Connection`] to a language server over a read/write pair.
//!
//! A dedicated reader thread parses incoming messages and demultiplexes them: a response is
//! matched to its waiting [`Connection::request`] by `id`; a server-initiated notification or
//! request is published to a Morpheus [`EventBus`] for the editor to drain each frame (so
//! `textDocument/publishDiagnostics` flows to the UI). [`Connection::request`] blocks for its
//! reply; [`Connection::notify`] is fire-and-forget. The transport is generic, so the connection
//! drives a real language server's stdio at runtime and a mock server over a socket pair in tests.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use morpheus::{Emitter, EventBus};
use serde_json::{Map, Value};

use crate::codec::{read_message, write_message};

/// A message from the server that is not a response to one of our requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// A server→client notification, e.g. `textDocument/publishDiagnostics`.
    Notification {
        /// The LSP method name.
        method: String,
        /// The method's parameters (`null` when absent).
        params: Value,
    },
    /// A server→client request, which expects a response keyed by `id` (e.g.
    /// `workspace/configuration`). Answering server requests lands with the typed client.
    Request {
        /// The request id to answer with.
        id: Value,
        /// The LSP method name.
        method: String,
        /// The method's parameters (`null` when absent).
        params: Value,
    },
}

/// The result of a [`Connection::request`]: the server's `result`, or an `error` object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The `result` field of a successful response.
    pub result: Option<Value>,
    /// The `error` field of a failed response.
    pub error: Option<Value>,
}

impl Response {
    /// The `result` on success, or the `error` object on failure.
    ///
    /// # Errors
    /// Returns the server's `error` value when the response carried one.
    pub fn into_result(self) -> Result<Value, Value> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.result.unwrap_or(Value::Null)),
        }
    }
}

type Pending = Arc<Mutex<HashMap<i64, mpsc::Sender<Response>>>>;

/// The request-sending half of a connection: the writer, the pending-response map, and the id
/// counter — everything needed to issue a request and correlate its reply, and nothing that is
/// `!Sync`. It is `Send + Sync`, so a [`Connection::requester`] handle can be shared with a worker
/// thread to run an interactive request (completion/hover) without freezing the editor. The
/// `!Sync` incoming-message receiver lives on [`Connection`] instead, off this shared path.
pub struct Requester {
    writer: Mutex<Box<dyn Write + Send>>,
    pending: Pending,
    next_id: AtomicI64,
}

impl fmt::Debug for Requester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Requester")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl Requester {
    /// Sends a request and blocks until the server's matching response arrives.
    ///
    /// # Errors
    /// Returns an I/O error if the write fails, or `BrokenPipe` if the server closes before
    /// answering.
    pub fn request(&self, method: &str, params: Value) -> io::Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        lock(&self.pending).insert(id, sender);
        self.send(&envelope(Some(id), method, params))?;
        receiver.recv().map_err(|error| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("language server closed before responding ({error})"),
            )
        })
    }

    /// Sends a request and blocks until the response arrives or `timeout` elapses.
    ///
    /// Used for interactive requests (completion, hover) that run on a worker thread: a slow or
    /// stuck server must not pin the thread forever, so on timeout the pending entry is evicted and
    /// `TimedOut` is returned (a late response is then dropped harmlessly by [`route`]).
    ///
    /// # Errors
    /// Returns an I/O error if the write fails, or `TimedOut` if no response arrives in `timeout`.
    pub fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> io::Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        lock(&self.pending).insert(id, sender);
        self.send(&envelope(Some(id), method, params))?;
        let Ok(response) = receiver.recv_timeout(timeout) else {
            lock(&self.pending).remove(&id);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "language server did not respond in time",
            ));
        };
        Ok(response)
    }

    /// Sends a fire-and-forget notification (no reply expected).
    ///
    /// # Errors
    /// Returns an I/O error if the write fails.
    pub fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        self.send(&envelope(None, method, params))
    }

    /// Answers a server-initiated request (an [`Incoming::Request`]) with `result`, keyed by its
    /// `id`. Used to reply to `workspace/applyEdit` so the server can proceed.
    ///
    /// # Errors
    /// Returns an I/O error if the write fails.
    pub fn respond(&self, id: Value, result: Value) -> io::Result<()> {
        self.send(&response_envelope(id, result))
    }

    fn send(&self, message: &Value) -> io::Result<()> {
        let mut writer = lock(&self.writer);
        write_message(&mut *writer, message)
    }
}

/// A JSON-RPC connection to a language server: a shared [`Requester`] (the send side) plus the bus
/// of server-initiated messages (the receive side).
#[derive(Debug)]
pub struct Connection {
    requester: Arc<Requester>,
    incoming: EventBus<Incoming>,
}

impl Connection {
    /// Starts a connection over `reader`/`writer` (a language server's stdout/stdin, or a socket
    /// pair in tests). Spawns the reader thread; it exits when the server closes the stream.
    #[must_use]
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let incoming = EventBus::new();
        let emitter = incoming.emitter();
        let reader_pending = Arc::clone(&pending);
        // Detached: the reader runs for the life of the stream and exits on EOF (server gone).
        thread::spawn(move || reader_loop(reader, &reader_pending, &emitter));
        Self {
            requester: Arc::new(Requester {
                writer: Mutex::new(Box::new(writer)),
                pending,
                next_id: AtomicI64::new(1),
            }),
            incoming,
        }
    }

    /// A shared handle to the send side, for issuing a request from a worker thread (the manager
    /// hands this to the completion/hover worker; the editor keeps draining [`Self::drain_incoming`]).
    #[must_use]
    pub fn requester(&self) -> Arc<Requester> {
        Arc::clone(&self.requester)
    }

    /// Sends a request and blocks until the server's matching response arrives.
    ///
    /// # Errors
    /// Returns an I/O error if the write fails, or `BrokenPipe` if the server closes before
    /// answering.
    pub fn request(&self, method: &str, params: Value) -> io::Result<Response> {
        self.requester.request(method, params)
    }

    /// Sends a fire-and-forget notification (no reply expected).
    ///
    /// # Errors
    /// Returns an I/O error if the write fails.
    pub fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        self.requester.notify(method, params)
    }

    /// Answers a server-initiated request by `id` (see [`Requester::respond`]).
    ///
    /// # Errors
    /// Returns an I/O error if the write fails.
    pub fn respond(&self, id: Value, result: Value) -> io::Result<()> {
        self.requester.respond(id, result)
    }

    /// Takes the server-initiated messages received since the last drain (the editor drains these
    /// each frame to surface diagnostics and the like).
    #[must_use]
    pub fn drain_incoming(&self) -> Vec<Incoming> {
        self.incoming.drain()
    }
}

/// Builds a JSON-RPC envelope, moving `params` into it (a request carries an `id`, a notification
/// does not).
fn envelope(id: Option<i64>, method: &str, params: Value) -> Value {
    let mut object = Map::new();
    object.insert("jsonrpc".to_owned(), Value::from("2.0"));
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::from(id));
    }
    object.insert("method".to_owned(), Value::from(method));
    object.insert("params".to_owned(), params);
    Value::Object(object)
}

/// Builds a JSON-RPC response envelope answering the request with the given `id` (`{id, result}`,
/// no `method`).
fn response_envelope(id: Value, result: Value) -> Value {
    let mut object = Map::new();
    object.insert("jsonrpc".to_owned(), Value::from("2.0"));
    object.insert("id".to_owned(), id);
    object.insert("result".to_owned(), result);
    Value::Object(object)
}

/// Locks `mutex`, recovering the guard if a holder panicked (a poisoned control channel should not
/// take the whole editor down — Stability P1).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads messages until the stream ends, routing each to its waiter or to the incoming bus.
fn reader_loop(
    reader: impl Read,
    pending: &Mutex<HashMap<i64, mpsc::Sender<Response>>>,
    incoming: &Emitter<Incoming>,
) {
    let mut buffered = BufReader::new(reader);
    while let Ok(message) = read_message(&mut buffered) {
        route(&message, pending, incoming);
    }
}

/// Routes one parsed message: a `method` means a server notification/request (→ bus); otherwise an
/// `id` means a response to one of our requests (→ the waiting channel).
fn route(
    message: &Value,
    pending: &Mutex<HashMap<i64, mpsc::Sender<Response>>>,
    incoming: &Emitter<Incoming>,
) {
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        let method = method.to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let event = match message.get("id") {
            Some(id) => Incoming::Request {
                id: id.clone(),
                method,
                params,
            },
            None => Incoming::Notification { method, params },
        };
        let _ = incoming.emit(event);
    } else if let Some(id) = message.get("id").and_then(Value::as_i64) {
        let response = Response {
            result: message.get("result").cloned(),
            error: message.get("error").cloned(),
        };
        if let Some(sender) = lock(pending).remove(&id) {
            let _ = sender.send(response);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::thread;

    use serde_json::json;

    use super::{Connection, Incoming};
    use crate::codec::{read_message, write_message};

    #[test]
    fn request_correlates_its_response_and_notifications_surface() {
        let (client, server) = UnixStream::pair().unwrap();

        // A mock language server: read the client's one request, push a notification, then answer.
        let mock = thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut writer = server;
            let request = read_message(&mut reader).unwrap();
            assert_eq!(request["method"], "initialize");
            let id = request["id"].clone();
            write_message(
                &mut writer,
                &json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"message": "ready"}}),
            )
            .unwrap();
            write_message(
                &mut writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}),
            )
            .unwrap();
            // Dropping `writer`/`server` here closes the stream, ending the client's reader thread.
        });

        let connection = Connection::new(client.try_clone().unwrap(), client);
        let response = connection
            .request("initialize", json!({"processId": null}))
            .unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap()["capabilities"], json!({}));

        // The notification was sent before the response, so by the time `request` returns the
        // reader has already published it.
        let incoming = connection.drain_incoming();
        assert!(incoming.iter().any(|message| matches!(
            message,
            Incoming::Notification { method, .. } if method == "window/logMessage"
        )));

        mock.join().unwrap();
    }

    #[test]
    fn request_timeout_gives_up_when_the_server_stays_silent() {
        use std::time::Duration;

        let (client, server) = UnixStream::pair().unwrap();
        // The server holds the connection open but never answers, so only the timeout can fire.
        let mock = thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let _request = read_message(&mut reader).unwrap();
            thread::sleep(Duration::from_millis(200));
            drop(server);
        });
        let connection = Connection::new(client.try_clone().unwrap(), client);
        let error = connection
            .requester()
            .request_timeout("initialize", json!({}), Duration::from_millis(20))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        mock.join().unwrap();
    }

    #[test]
    fn request_errors_when_the_server_closes_first() {
        let (client, server) = UnixStream::pair().unwrap();
        // The server immediately drops its end without answering.
        drop(server);
        let connection = Connection::new(client.try_clone().unwrap(), client);
        let error = connection.request("initialize", json!({})).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
