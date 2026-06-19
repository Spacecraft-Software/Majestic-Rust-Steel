// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Language Server Protocol base framing: `Content-Length` headers + a JSON body.
//!
//! Each message is `Content-Length: N\r\n\r\n` followed by `N` bytes of JSON (LSP §3.1). The
//! codec is transport-agnostic ([`BufRead`]/[`Write`]) so it is unit-tested over an in-memory
//! buffer and runs over a language server's stdio at runtime. Messages are carried as
//! [`serde_json::Value`]; the typed LSP payloads are layered above.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// The largest message body the codec will read, bounding allocation from a corrupt header.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Writes `message` with an LSP `Content-Length` header.
///
/// # Errors
/// Returns an I/O error if serialization fails or the underlying writer fails.
pub fn write_message<W: Write>(writer: &mut W, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one LSP message: the headers (only `Content-Length` is significant), then the JSON body.
///
/// # Errors
/// Returns `UnexpectedEof` at a clean end of stream, `InvalidData` for a malformed or missing
/// `Content-Length` / oversized body / non-JSON body, or any underlying read error.
pub fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break; // the blank line ends the headers
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse().map_err(invalid_data)?);
        }
        // Other headers (e.g. Content-Type) are accepted and ignored.
    }

    let length = content_length.ok_or_else(|| invalid_data("message has no Content-Length"))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(invalid_data("message exceeds maximum size"));
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
    use super::{read_message, write_message};
    use serde_json::json;

    #[test]
    fn message_round_trips_through_the_frame() {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        let mut buffer = Vec::new();
        write_message(&mut buffer, &message).unwrap();
        // The frame carries the Content-Length header.
        let text = String::from_utf8(buffer.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        let decoded = read_message(&mut buffer.as_slice()).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn two_messages_decode_in_order() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &json!({"id": 1})).unwrap();
        write_message(&mut buffer, &json!({"id": 2})).unwrap();
        let mut stream = buffer.as_slice();
        assert_eq!(read_message(&mut stream).unwrap(), json!({"id": 1}));
        assert_eq!(read_message(&mut stream).unwrap(), json!({"id": 2}));
    }

    #[test]
    fn a_missing_content_length_is_rejected() {
        let mut bytes = b"X-Other: 1\r\n\r\n".as_slice();
        let error = read_message(&mut bytes).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
