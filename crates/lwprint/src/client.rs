//! Minimal HTTP client for talking to a running `lwprint serve`, so the CLI can print
//! through the service instead of competing with it for the printer. Plain HTTP only
//! (the service runs on the local network), no extra dependencies.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Client {
    host: String,
    port: u16,
    prefix: String,
    token: Option<String>,
}

impl Client {
    /// `url` like `http://labelprinter.lan:8080` (an optional path prefix is kept).
    pub fn new(url: &str, token: Option<String>) -> Result<Self> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("the server URL must start with http:// (got {url})"))?;
        let (authority, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().context("invalid port in the server URL")?),
            None => (authority, 80),
        };
        if host.is_empty() {
            bail!("no host in the server URL {url}");
        }
        Ok(Self { host: host.to_string(), port, prefix: prefix.to_string(), token })
    }

    /// Send a request and return the status code and the JSON body.
    pub async fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .with_context(|| format!("could not reach the lwprint service at {}:{}", self.host, self.port))?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!(
            "{method} {}{path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n",
            self.prefix, self.host
        );
        if let Some(t) = &self.token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        if method != "GET" {
            req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        parse_response(&raw)
    }
}

fn parse_response(raw: &[u8]) -> Result<(u16, Value)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("invalid response from the lwprint service"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("invalid status line from the lwprint service"))?;
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body = dechunk(&body)?;
    }
    let json = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
    Ok((status, json))
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n").ok_or_else(|| anyhow!("bad chunked body"))?;
        let size_str = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| anyhow!("bad chunk size"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            bail!("truncated chunked body");
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// Error message from a JSON error response, or a generic one.
pub fn error_of(status: u16, body: &Value) -> anyhow::Error {
    match body.get("error").and_then(|e| e.as_str()) {
        Some(e) => anyhow!("{e}"),
        None => anyhow!("the lwprint service answered with HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let c = Client::new("http://labelprinter.lan:8080", None).unwrap();
        assert_eq!((c.host.as_str(), c.port, c.prefix.as_str()), ("labelprinter.lan", 8080, ""));
        let c = Client::new("http://10.0.0.5/labels/", None).unwrap();
        assert_eq!((c.host.as_str(), c.port, c.prefix.as_str()), ("10.0.0.5", 80, "/labels"));
        assert!(Client::new("https://x", None).is_err());
    }

    #[test]
    fn parses_responses() {
        let raw = b"HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 8\r\n\r\n{\"id\":3}";
        let (s, v) = parse_response(raw).unwrap();
        assert_eq!((s, v["id"].as_u64()), (202, Some(3)));
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let (_, v) = parse_response(raw).unwrap();
        assert_eq!(v["a"].as_u64(), Some(1));
    }
}
