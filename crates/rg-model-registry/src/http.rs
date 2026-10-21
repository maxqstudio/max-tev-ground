//! Minimal HTTP/1.1 client for an Owner-configured loopback model runtime.
//!
//! The endpoint is a literal loopback IP address and port; host names are refused, so name
//! resolution can never send a request off the machine. Only the calls in [`RuntimeCall`] exist:
//! there is no way to name a pull, delete, create, copy, push, or blob path. Redirects are
//! reported and never followed, and every response is bounded in time, header size, and body size.
//! Nothing here starts, stops, or signals a runtime process.

use std::{
    fmt,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CHUNK_LINE_BYTES: usize = 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    #[error("model runtime endpoint is invalid: {0}")]
    InvalidEndpoint(String),
    #[error("model runtime request {0} is not permitted")]
    ForbiddenCall(String),
    #[error("model runtime at {endpoint} is unavailable: {reason}")]
    Unavailable { endpoint: String, reason: String },
    #[error("model runtime did not answer within {0} ms")]
    Timeout(u64),
    #[error("model runtime answered with redirect {0}; redirects are never followed")]
    Redirect(u16),
    #[error("model runtime answered HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("model runtime response exceeded the {0} byte limit")]
    Oversized(u64),
    #[error("model runtime response is malformed: {0}")]
    Malformed(String),
    #[error("model runtime request was cancelled")]
    Cancelled,
}

impl RuntimeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidEndpoint(_) => "INVALID_RUNTIME_ENDPOINT",
            Self::ForbiddenCall(_) => "FORBIDDEN_RUNTIME_CALL",
            Self::Unavailable { .. } => "RUNTIME_UNAVAILABLE",
            Self::Timeout(_) => "RUNTIME_TIMEOUT",
            Self::Redirect(_) => "RUNTIME_REDIRECT_REFUSED",
            Self::HttpStatus { .. } => "RUNTIME_HTTP_ERROR",
            Self::Oversized(_) => "RUNTIME_RESPONSE_TOO_LARGE",
            Self::Malformed(_) => "RUNTIME_RESPONSE_MALFORMED",
            Self::Cancelled => "CANCELLED",
        }
    }
}

/// A loopback-only runtime address, written as `http://127.0.0.1:<port>` or `http://[::1]:<port>`.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LoopbackEndpoint {
    address: SocketAddr,
}

impl LoopbackEndpoint {
    pub fn parse(text: &str) -> Result<Self, RuntimeError> {
        let invalid = |reason: &str| RuntimeError::InvalidEndpoint(format!("{text:?}: {reason}"));
        let rest = text
            .strip_prefix("http://")
            .ok_or_else(|| invalid("only plain http:// to a loopback address is supported"))?;
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty()
            || authority.contains(['/', '?', '#', '@', ' ', '\\'])
            || authority.len() > 64
        {
            return Err(invalid(
                "use only a loopback IP address and port, with no path, query, or credentials",
            ));
        }
        let address: SocketAddr = authority.parse().map_err(|_| {
            invalid("host must be a literal loopback IP (127.0.0.1 or [::1]) followed by :port")
        })?;
        let loopback = match address.ip() {
            IpAddr::V4(ip) => ip.is_loopback(),
            IpAddr::V6(ip) => ip.is_loopback(),
        };
        if !loopback {
            return Err(invalid("the address is not a loopback address"));
        }
        if address.port() == 0 {
            return Err(invalid("port must be between 1 and 65535"));
        }
        Ok(Self { address })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl fmt::Display for LoopbackEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "http://{}", self.address)
    }
}

impl fmt::Debug for LoopbackEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl TryFrom<String> for LoopbackEndpoint {
    type Error = RuntimeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<LoopbackEndpoint> for String {
    fn from(value: LoopbackEndpoint) -> Self {
        value.to_string()
    }
}

/// The complete set of runtime calls RG-R04 can make. Model pull, push, create, copy, delete,
/// blob upload, embedding, and process-control endpoints are deliberately absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeCall {
    Version,
    ListModels,
    Generate,
}

impl RuntimeCall {
    pub fn method(self) -> &'static str {
        match self {
            Self::Version | Self::ListModels => "GET",
            Self::Generate => "POST",
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::Version => "/api/version",
            Self::ListModels => "/api/tags",
            Self::Generate => "/api/generate",
        }
    }

    /// Maps a method and path to a permitted call, refusing everything else. Used at the boundary
    /// so no caller can construct a request outside the allowlist.
    pub fn permitted(method: &str, path: &str) -> Result<Self, RuntimeError> {
        [Self::Version, Self::ListModels, Self::Generate]
            .into_iter()
            .find(|call| call.method() == method && call.path() == path)
            .ok_or_else(|| RuntimeError::ForbiddenCall(format!("{method} {path}")))
    }
}

/// Per-request bounds. `timeout` covers connecting, sending, and reading the whole response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpLimits {
    pub timeout: Duration,
    pub max_response_bytes: u64,
}

/// Sends one permitted request and returns the body of a 2xx response.
pub fn send(
    endpoint: &LoopbackEndpoint,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    limits: &HttpLimits,
) -> Result<Vec<u8>, RuntimeError> {
    let call = RuntimeCall::permitted(method, path)?;
    if call.method() == "GET" && body.is_some() {
        return Err(RuntimeError::ForbiddenCall(format!(
            "{method} {path} with a body"
        )));
    }
    let deadline = Instant::now() + limits.timeout;
    let timeout_ms = limits.timeout.as_millis() as u64;
    let unavailable = |reason: String| RuntimeError::Unavailable {
        endpoint: endpoint.to_string(),
        reason,
    };
    let mut stream =
        TcpStream::connect_timeout(&endpoint.address(), limits.timeout).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                RuntimeError::Timeout(timeout_ms)
            } else {
                unavailable(error.to_string())
            }
        })?;
    stream
        .set_nodelay(true)
        .map_err(|error| unavailable(error.to_string()))?;
    let body = body.unwrap_or_default();
    let mut request = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: max-research-ground\r\nAccept: application/json\r\nConnection: close\r\n",
        call.method(),
        call.path(),
        endpoint.address()
    );
    if call.method() == "POST" {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    let mut writer = Deadline {
        stream: &mut stream,
        deadline,
        timeout_ms,
    };
    writer.write_all(request.as_bytes())?;
    writer.write_all(body)?;
    let mut reader = BufferedResponse::new(writer, limits.max_response_bytes);
    let (status, chunked, content_length) = reader.read_head()?;
    let payload = if chunked {
        reader.read_chunked()?
    } else if let Some(length) = content_length {
        reader.read_exact_body(length)?
    } else {
        reader.read_to_close()?
    };
    match status {
        200..=299 => Ok(payload),
        300..=399 => Err(RuntimeError::Redirect(status)),
        _ => Err(RuntimeError::HttpStatus {
            status,
            message: runtime_error_message(&payload),
        }),
    }
}

/// Extracts a short, printable error message from an error body.
fn runtime_error_message(payload: &[u8]) -> String {
    let parsed = serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned));
    let text = parsed.unwrap_or_else(|| String::from_utf8_lossy(payload).into_owned());
    text.chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect()
}

/// Applies the request's overall deadline to every read and write.
struct Deadline<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
    timeout_ms: u64,
}

impl Deadline<'_> {
    fn remaining(&self) -> Result<Duration, RuntimeError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(RuntimeError::Timeout(self.timeout_ms))
    }

    fn map(&self, error: std::io::Error) -> RuntimeError {
        match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                RuntimeError::Timeout(self.timeout_ms)
            }
            _ => RuntimeError::Malformed(format!("connection failed: {error}")),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), RuntimeError> {
        let remaining = self.remaining()?;
        self.stream
            .set_write_timeout(Some(remaining))
            .map_err(|error| self.map(error))?;
        self.stream
            .write_all(bytes)
            .map_err(|error| self.map(error))
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, RuntimeError> {
        let remaining = self.remaining()?;
        self.stream
            .set_read_timeout(Some(remaining))
            .map_err(|error| self.map(error))?;
        self.stream.read(buffer).map_err(|error| self.map(error))
    }
}

struct BufferedResponse<'a> {
    source: Deadline<'a>,
    buffer: Vec<u8>,
    position: usize,
    closed: bool,
    max_body: u64,
}

impl<'a> BufferedResponse<'a> {
    fn new(source: Deadline<'a>, max_body: u64) -> Self {
        Self {
            source,
            buffer: Vec::new(),
            position: 0,
            closed: false,
            max_body,
        }
    }

    fn fill(&mut self) -> Result<bool, RuntimeError> {
        if self.closed {
            return Ok(false);
        }
        if self.position > 0 {
            self.buffer.drain(..self.position);
            self.position = 0;
        }
        let mut chunk = [0u8; 8192];
        let count = self.source.read(&mut chunk)?;
        if count == 0 {
            self.closed = true;
            return Ok(false);
        }
        self.buffer.extend_from_slice(&chunk[..count]);
        Ok(true)
    }

    fn available(&self) -> &[u8] {
        &self.buffer[self.position..]
    }

    /// Reads one CRLF-terminated line of at most `limit` bytes, without the terminator.
    fn line(&mut self, limit: usize) -> Result<Vec<u8>, RuntimeError> {
        loop {
            if let Some(end) = self.available().windows(2).position(|pair| pair == b"\r\n") {
                if end > limit {
                    return Err(RuntimeError::Malformed("response line is too long".into()));
                }
                let line = self.available()[..end].to_vec();
                self.position += end + 2;
                return Ok(line);
            }
            if self.available().len() > limit {
                return Err(RuntimeError::Malformed("response line is too long".into()));
            }
            if !self.fill()? {
                return Err(RuntimeError::Malformed(
                    "connection closed inside the response head".into(),
                ));
            }
        }
    }

    fn read_head(&mut self) -> Result<(u16, bool, Option<u64>), RuntimeError> {
        let mut consumed = 0usize;
        let status_line = self.line(MAX_HEADER_BYTES)?;
        consumed += status_line.len() + 2;
        let status_line = String::from_utf8(status_line)
            .map_err(|_| RuntimeError::Malformed("status line is not UTF-8".into()))?;
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        let status = parts
            .next()
            .and_then(|code| code.parse::<u16>().ok())
            .filter(|code| (100..=599).contains(code));
        let Some(status) = status.filter(|_| version == "HTTP/1.1" || version == "HTTP/1.0") else {
            return Err(RuntimeError::Malformed("status line is invalid".into()));
        };
        let mut chunked = false;
        let mut content_length = None;
        loop {
            let line = self.line(MAX_HEADER_BYTES)?;
            consumed += line.len() + 2;
            if consumed > MAX_HEADER_BYTES {
                return Err(RuntimeError::Malformed(
                    "response headers are too large".into(),
                ));
            }
            if line.is_empty() {
                break;
            }
            let line = String::from_utf8(line)
                .map_err(|_| RuntimeError::Malformed("header is not UTF-8".into()))?;
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| RuntimeError::Malformed("header has no colon".into()))?;
            let value = value.trim();
            if name.eq_ignore_ascii_case("transfer-encoding") {
                if !value.eq_ignore_ascii_case("chunked") {
                    return Err(RuntimeError::Malformed(format!(
                        "transfer encoding {value:?} is not supported"
                    )));
                }
                chunked = true;
            } else if name.eq_ignore_ascii_case("content-length") {
                let length = value
                    .parse::<u64>()
                    .map_err(|_| RuntimeError::Malformed("content length is invalid".into()))?;
                if content_length.is_some_and(|existing| existing != length) {
                    return Err(RuntimeError::Malformed(
                        "conflicting content lengths".into(),
                    ));
                }
                content_length = Some(length);
            } else if name.eq_ignore_ascii_case("content-encoding")
                && !value.eq_ignore_ascii_case("identity")
            {
                return Err(RuntimeError::Malformed(format!(
                    "content encoding {value:?} is not supported"
                )));
            }
        }
        if chunked && content_length.is_some() {
            return Err(RuntimeError::Malformed(
                "response declares both chunked encoding and a content length".into(),
            ));
        }
        if content_length.is_some_and(|length| length > self.max_body) {
            return Err(RuntimeError::Oversized(self.max_body));
        }
        Ok((status, chunked, content_length))
    }

    fn take(&mut self, length: usize, body: &mut Vec<u8>) -> Result<(), RuntimeError> {
        let mut needed = length;
        while needed > 0 {
            if self.available().is_empty() && !self.fill()? {
                return Err(RuntimeError::Malformed(
                    "connection closed before the response body was complete".into(),
                ));
            }
            let count = needed.min(self.available().len());
            body.extend_from_slice(&self.available()[..count]);
            self.position += count;
            needed -= count;
        }
        Ok(())
    }

    fn read_exact_body(&mut self, length: u64) -> Result<Vec<u8>, RuntimeError> {
        let mut body = Vec::with_capacity(length.min(64 * 1024) as usize);
        self.take(length as usize, &mut body)?;
        Ok(body)
    }

    fn read_chunked(&mut self) -> Result<Vec<u8>, RuntimeError> {
        let mut body = Vec::new();
        loop {
            let line = self.line(MAX_CHUNK_LINE_BYTES)?;
            let text = String::from_utf8(line)
                .map_err(|_| RuntimeError::Malformed("chunk size is not UTF-8".into()))?;
            let size_text = text.split(';').next().unwrap_or_default().trim();
            let size = u64::from_str_radix(size_text, 16)
                .map_err(|_| RuntimeError::Malformed("chunk size is invalid".into()))?;
            if size == 0 {
                // Trailers are not used by the runtime; read until the terminating blank line.
                loop {
                    if self.line(MAX_HEADER_BYTES)?.is_empty() {
                        return Ok(body);
                    }
                }
            }
            if body.len() as u64 + size > self.max_body {
                return Err(RuntimeError::Oversized(self.max_body));
            }
            self.take(size as usize, &mut body)?;
            if !self.line(0)?.is_empty() {
                return Err(RuntimeError::Malformed(
                    "chunk is not CRLF-terminated".into(),
                ));
            }
        }
    }

    fn read_to_close(&mut self) -> Result<Vec<u8>, RuntimeError> {
        let mut body = self.available().to_vec();
        self.position = self.buffer.len();
        while self.fill()? {
            body.extend_from_slice(self.available());
            self.position = self.buffer.len();
            if body.len() as u64 > self.max_body {
                return Err(RuntimeError::Oversized(self.max_body));
            }
        }
        if body.len() as u64 > self.max_body {
            return Err(RuntimeError::Oversized(self.max_body));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::ollama::{MockBehavior, MockOllama};

    fn limits() -> HttpLimits {
        HttpLimits {
            timeout: Duration::from_secs(5),
            max_response_bytes: 64 * 1024,
        }
    }

    #[test]
    fn gate_local_runtime_boundary_accepts_only_literal_loopback_endpoints() {
        for accepted in [
            "http://127.0.0.1:11434",
            "http://127.0.0.1:11434/",
            "http://127.9.8.7:8080",
            "http://[::1]:11434",
        ] {
            assert!(LoopbackEndpoint::parse(accepted).is_ok(), "{accepted}");
        }
        for refused in [
            "http://localhost:11434",
            "http://ollama.example.com:11434",
            "https://127.0.0.1:11434",
            "http://10.0.0.5:11434",
            "http://192.168.1.2:11434",
            "http://0.0.0.0:11434",
            "http://[::]:11434",
            "http://[::ffff:8.8.8.8]:11434",
            "http://8.8.8.8:80",
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:11434/api",
            "http://user:secret@127.0.0.1:11434",
            "http://127.0.0.1:11434?x=1",
            "file:///C:/models/model.onnx",
            "",
        ] {
            assert!(
                matches!(
                    LoopbackEndpoint::parse(refused),
                    Err(RuntimeError::InvalidEndpoint(_))
                ),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn gate_local_runtime_boundary_refuses_pull_delete_and_path_taking_calls_before_connecting() {
        let mock = MockOllama::start(MockBehavior::Normal);
        let endpoint = mock.endpoint();
        for (method, path) in [
            ("POST", "/api/pull"),
            ("DELETE", "/api/delete"),
            ("POST", "/api/create"),
            ("POST", "/api/copy"),
            ("POST", "/api/push"),
            ("POST", "/api/blobs/sha256:00"),
            ("HEAD", "/api/blobs/sha256:00"),
            ("POST", "/api/embed"),
            ("POST", "/api/show"),
            ("GET", "/api/ps"),
            ("GET", "/../api/tags"),
            ("GET", "http://example.com/api/tags"),
            ("POST", "/api/tags"),
        ] {
            assert!(
                matches!(
                    send(&endpoint, method, path, Some(b"{}"), &limits()),
                    Err(RuntimeError::ForbiddenCall(_))
                ),
                "{method} {path}"
            );
        }
        assert!(matches!(
            send(&endpoint, "GET", "/api/tags", Some(b"{}"), &limits()),
            Err(RuntimeError::ForbiddenCall(_))
        ));
        assert!(
            mock.requests().is_empty(),
            "no forbidden request reached the runtime"
        );
    }

    #[test]
    fn gate_local_runtime_boundary_never_follows_redirects() {
        let mock = MockOllama::start(MockBehavior::Redirect);
        let result = send(&mock.endpoint(), "GET", "/api/tags", None, &limits());
        assert_eq!(result, Err(RuntimeError::Redirect(307)));
        assert_eq!(mock.requests(), vec!["GET /api/tags".to_owned()]);
    }

    #[test]
    fn gate_local_runtime_boundary_bounds_oversized_and_malformed_responses() {
        let oversized = MockOllama::start(MockBehavior::Oversized);
        assert_eq!(
            send(&oversized.endpoint(), "GET", "/api/tags", None, &limits()),
            Err(RuntimeError::Oversized(64 * 1024))
        );
        let chunked = MockOllama::start(MockBehavior::OversizedChunked);
        assert_eq!(
            send(&chunked.endpoint(), "GET", "/api/tags", None, &limits()),
            Err(RuntimeError::Oversized(64 * 1024))
        );
        let garbage = MockOllama::start(MockBehavior::GarbageHead);
        assert!(matches!(
            send(&garbage.endpoint(), "GET", "/api/tags", None, &limits()),
            Err(RuntimeError::Malformed(_))
        ));
        let truncated = MockOllama::start(MockBehavior::CloseMidResponse);
        assert!(matches!(
            send(&truncated.endpoint(), "GET", "/api/tags", None, &limits()),
            Err(RuntimeError::Malformed(_))
        ));
    }

    #[test]
    fn gate_resource_cancel_recovery_request_timeout_is_bounded() {
        let slow = MockOllama::start(MockBehavior::Stall);
        let started = Instant::now();
        let result = send(
            &slow.endpoint(),
            "GET",
            "/api/tags",
            None,
            &HttpLimits {
                timeout: Duration::from_millis(300),
                max_response_bytes: 1024,
            },
        );
        assert_eq!(result, Err(RuntimeError::Timeout(300)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn gate_local_runtime_boundary_reports_an_absent_runtime_as_unavailable() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let endpoint = LoopbackEndpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        assert!(matches!(
            send(&endpoint, "GET", "/api/tags", None, &limits()),
            Err(RuntimeError::Unavailable { .. }) | Err(RuntimeError::Timeout(_))
        ));
    }
}
