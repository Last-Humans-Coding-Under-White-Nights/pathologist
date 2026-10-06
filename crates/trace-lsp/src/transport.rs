//! Bounded Content-Length framing. Stdout is reserved for JSON-RPC messages.

use crate::Server;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};

const MAX_HEADER: usize = 8192;
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut total = 0;
    let mut length = None;
    loop {
        let mut line = Vec::new();
        let n = reader
            .by_ref()
            .take((MAX_HEADER + 1 - total) as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            if total == 0 {
                return Ok(None);
            }
            bail!("unexpected EOF in protocol header");
        }
        total += n;
        if total > MAX_HEADER {
            bail!("protocol header exceeds {MAX_HEADER} bytes");
        }
        if !line.ends_with(b"\r\n") {
            bail!("protocol header must end with CRLF");
        }
        if line == b"\r\n" {
            break;
        }
        let line = std::str::from_utf8(&line)?.trim_end();
        let (name, value) = line.split_once(':').context("invalid protocol header")?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                bail!("duplicate Content-Length header");
            }
            let n: usize = value.trim().parse().context("invalid Content-Length")?;
            if n > MAX_MESSAGE {
                bail!("protocol message exceeds {MAX_MESSAGE} bytes");
            }
            length = Some(n);
        }
    }
    let mut body = vec![0; length.context("missing Content-Length header")?];
    reader
        .read_exact(&mut body)
        .context("unexpected EOF in protocol body")?;
    Ok(Some(body))
}

pub fn write_frame(writer: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn error(writer: &mut impl Write, id: Value, code: i32, message: &str) -> Result<()> {
    write_frame(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

/// Return the LSP exit status (0 after shutdown, 1 otherwise).
pub fn serve(
    server: &mut Server,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<i32> {
    while let Some(body) = read_frame(reader)? {
        let message: Value = match serde_json::from_slice(&body) {
            Ok(message) => message,
            Err(_) => {
                error(writer, Value::Null, -32700, "invalid JSON")?;
                continue;
            }
        };
        let id = message.get("id");
        let valid_id = id.is_none_or(|v| v.is_string() || v.as_i64().is_some());
        let method = message.get("method").and_then(Value::as_str);
        if !message.is_object()
            || message.get("jsonrpc") != Some(&json!("2.0"))
            || method.is_none()
            || !valid_id
        {
            error(writer, Value::Null, -32600, "invalid JSON-RPC request")?;
            continue;
        }
        let method = method.unwrap();
        if id.is_none() {
            if method == "exit" {
                return Ok(if server.shutdown_requested() { 0 } else { 1 });
            }
            // initialized, cancellation, document changes, and unknown
            // notifications need no reply. Analysis remains a fixed snapshot.
            continue;
        }
        let id = id.unwrap().clone();
        match server.request(
            method,
            message.get("params").cloned().unwrap_or(Value::Null),
        ) {
            Ok(result) => write_frame(
                writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            )?,
            Err(e) => error(writer, id, e.code, &e.message)?,
        }
    }
    Ok(if server.shutdown_requested() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_counts_bytes_and_accepts_extra_headers() {
        let value = json!({"name": "é😀"});
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).unwrap();
        let body = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), value);
        let mut input = Cursor::new(b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}");
        assert_eq!(read_frame(&mut input).unwrap().unwrap(), b"{}");
        assert!(read_frame(&mut input).unwrap().is_none());
    }

    #[test]
    fn malformed_and_truncated_frames_fail_without_panicking() {
        for data in [
            "Content-Length: -1\r\n\r\n",
            "Content-Length: 99999999\r\n\r\n",
            "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            "Bad\r\n\r\n",
            "Content-Length: 2\r\n\r\n{",
            "Content-Length: 2\n\n{}",
            "\r\n",
        ] {
            assert!(read_frame(&mut Cursor::new(data)).is_err(), "{data}");
        }
        assert!(read_frame(&mut Cursor::new(vec![b'x'; MAX_HEADER + 1])).is_err());
    }
}
