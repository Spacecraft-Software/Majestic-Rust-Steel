// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The client↔daemon wire protocol: request/response messages and a length-prefixed JSON codec.
//!
//! Each frame is a 4-byte big-endian length followed by that many bytes of JSON. The codec is
//! transport-agnostic ([`Read`]/[`Write`]) so it is exercised over an in-memory buffer in tests
//! and over a Unix socket at runtime. JSON keeps the protocol inspectable and versionable.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A request from a client to the daemon. The first frame on a connection; `Attach` turns it into
/// a bidirectional interactive stream, the rest are one-shot control requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Report the daemon's owned-session summary and uptime.
    Status,
    /// Persist the owned session to disk now.
    Save,
    /// Shut the daemon down (after responding).
    Shutdown,
    /// Attach an interactive client of the given terminal size; the daemon then streams rendered
    /// frames and consumes input until the client detaches. Handled by the session host, not by
    /// [`Daemon::handle`].
    Attach {
        /// The client terminal width in columns.
        cols: u16,
        /// The client terminal height in rows.
        rows: u16,
    },
}

/// A response from the daemon to a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// The daemon's current status (answer to [`Request::Status`]).
    Status(DaemonStatus),
    /// The request succeeded with nothing to report.
    Ok,
    /// The request failed; the string is a human-readable reason.
    Error(String),
}

/// A summary of the session a daemon owns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Number of open panes in the owned session.
    pub panes: usize,
    /// The focused pane's in-order ordinal.
    pub focused: usize,
    /// Where the session is persisted, if a path is known.
    pub session_path: Option<String>,
}

/// The largest frame the codec will read, to bound allocation from a corrupt or hostile length.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Writes `value` as a length-prefixed JSON frame (4-byte big-endian length, then the JSON body).
///
/// # Errors
/// Returns an I/O error if serialization fails, the body exceeds [`MAX_FRAME_BYTES`], or the
/// underlying writer fails.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(invalid_data)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(invalid_data("frame exceeds maximum size"));
    }
    // `body.len() <= MAX_FRAME_BYTES` (< u32::MAX), so the cast cannot truncate.
    let length = u32::try_from(body.len()).map_err(invalid_data)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one length-prefixed JSON frame and deserializes it into `T`.
///
/// # Errors
/// Returns an I/O error on a short read, a length exceeding [`MAX_FRAME_BYTES`], or a body that
/// does not deserialize into `T`. A clean EOF before any bytes surfaces as `UnexpectedEof`.
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut length_buf = [0u8; 4];
    reader.read_exact(&mut length_buf)?;
    let length = u32::from_be_bytes(length_buf) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(invalid_data("frame exceeds maximum size"));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(invalid_data)
}

/// Wraps any error as an `io::Error` of kind `InvalidData`.
fn invalid_data<E>(error: E) -> io::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::{read_frame, write_frame, DaemonStatus, Request, Response};

    #[test]
    fn request_round_trips_through_a_frame() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Request::Save).unwrap();
        let decoded: Request = read_frame(&mut buffer.as_slice()).unwrap();
        assert_eq!(decoded, Request::Save);
    }

    #[test]
    fn response_round_trips_through_a_frame() {
        let status = Response::Status(DaemonStatus {
            panes: 3,
            focused: 1,
            session_path: Some("/run/user/1000/majestic/session.json".to_owned()),
        });
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &status).unwrap();
        let decoded: Response = read_frame(&mut buffer.as_slice()).unwrap();
        assert_eq!(decoded, status);
    }

    #[test]
    fn two_frames_decode_in_order_from_one_stream() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Request::Status).unwrap();
        write_frame(&mut buffer, &Request::Shutdown).unwrap();
        let mut stream = buffer.as_slice();
        assert_eq!(
            read_frame::<_, Request>(&mut stream).unwrap(),
            Request::Status
        );
        assert_eq!(
            read_frame::<_, Request>(&mut stream).unwrap(),
            Request::Shutdown
        );
    }

    #[test]
    fn an_oversized_length_is_rejected_without_allocating() {
        // A 4-byte length of 0xFFFFFFFF must be refused (it exceeds the max), not allocated.
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF];
        let error = read_frame::<_, Request>(&mut bytes.as_slice()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
