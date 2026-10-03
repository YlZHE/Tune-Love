//! HTTP transport for model downloads: one GET per attempt, resumable with `Range`, streamed
//! chunk by chunk. The `Fetcher`/`Body` traits let the downloader be tested without a network.

use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Could not connect (refused, DNS, TLS handshake, connect timeout).
    Connect,
    /// A read stalled longer than the read timeout.
    Timeout,
    /// Any status other than 200/206.
    Status(u16),
    /// A 206 whose `Content-Range` is missing, malformed or does not start at the requested offset.
    BadRange,
    Network(String),
    /// This source cannot work however often it is retried: its TLS certificate was rejected,
    /// it redirects in a loop, or the URL is refused before sending (not https). Move on.
    Fatal(String),
}

impl FetchError {
    /// Whether trying the same source again may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            FetchError::Connect | FetchError::Timeout | FetchError::Network(_) => true,
            FetchError::Status(code) => *code == 429 || (500..=599).contains(code),
            FetchError::BadRange | FetchError::Fatal(_) => false,
        }
    }
}

pub struct Opened<B> {
    /// 200 (the whole file from byte 0, even if a range was asked for) or 206.
    pub status: u16,
    /// Full file size: `Content-Range` total for 206, `Content-Length` for 200.
    pub total: Option<u64>,
    /// The body is a web page (a mirror's error or landing page, not the file): `Content-Type`
    /// is `text/html`, or a 200 without a type (or `application/octet-stream`) starts with `<`.
    pub html: bool,
    pub body: B,
}

pub trait Body: Send {
    /// The next piece of the body, or `None` at the end.
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, FetchError>> + Send;
}

pub trait Fetcher: Send + Sync + 'static {
    type Body: Body;
    /// GETs `url`, asking for `bytes=<offset>-` when `offset > 0`.
    fn open(&self, url: &str, offset: u64) -> impl Future<Output = Result<Opened<Self::Body>, FetchError>> + Send;
}

pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    pub fn new(connect_timeout: Duration, read_timeout: Duration) -> Result<Self, String> {
        Self::from_builder(builder(connect_timeout, read_timeout))
    }

    /// 10 s to connect, 30 s per stalled read.
    pub fn standard() -> Result<Self, String> {
        Self::new(Duration::from_secs(10), Duration::from_secs(30))
    }

    /// Same client but ignoring any system/env proxy and allowing plain `http://`, so local test
    /// servers are reached directly.
    #[cfg(test)]
    fn direct(connect_timeout: Duration, read_timeout: Duration) -> Result<Self, String> {
        Self::from_builder(builder(connect_timeout, read_timeout).no_proxy().https_only(false))
    }

    fn from_builder(b: reqwest::ClientBuilder) -> Result<Self, String> {
        let client = b.build().map_err(|e| format!("HTTP client: {e}"))?;
        Ok(ReqwestFetcher { client })
    }
}

fn builder(connect_timeout: Duration, read_timeout: Duration) -> reqwest::ClientBuilder {
    // Explicit: if feature unification ever pulls rustls in, the default backend could change.
    reqwest::Client::builder()
        .tls_backend_native()
        // Every manifest origin and mirror prefix is https; this also refuses https -> http redirects.
        .https_only(true)
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .user_agent(concat!("TuneLove/", env!("CARGO_PKG_VERSION")))
}

fn classify(e: reqwest::Error) -> FetchError {
    let text = error_chain(&e);
    if e.is_redirect() || e.is_builder() || certificate_rejected(&text) {
        FetchError::Fatal(text)
    } else if e.is_connect() {
        FetchError::Connect
    } else if e.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Network(text)
    }
}

/// The error and all its `source()`s, joined with `: ` (reqwest's own text omits the cause).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut next = e.source();
    while let Some(cause) = next {
        let part = cause.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        next = cause.source();
    }
    text
}

/// Whether the error text carries a certificate rejection. Schannel's message is localised, but
/// `io::Error` always appends `(os error <code>)`, so the code is matched instead: any
/// `CERT_E_*`/`TRUST_E_*` (facility 0x0B) or the schannel certificate `SEC_E_*` codes.
fn certificate_rejected(text: &str) -> bool {
    const SEC_E_CERT: [u32; 5] = [
        0x8009_0322, // SEC_E_WRONG_PRINCIPAL
        0x8009_0325, // SEC_E_UNTRUSTED_ROOT
        0x8009_0327, // SEC_E_CERT_UNKNOWN
        0x8009_0328, // SEC_E_CERT_EXPIRED
        0x8009_0349, // SEC_E_CERT_WRONG_USAGE
    ];
    let mut codes = text.match_indices("os error ").filter_map(|(at, marker)| {
        let rest = &text[at + marker.len()..];
        let end = rest
            .char_indices()
            .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && c == '-')))
            .map_or(rest.len(), |(i, _)| i);
        rest[..end].parse::<i64>().ok().map(|code| code as i32 as u32)
    });
    codes.any(|code| code & 0xFFFF_0000 == 0x800B_0000 || SEC_E_CERT.contains(&code))
        || text.contains("certificate verify failed")
}

/// A body that starts (after an optional UTF-8 BOM and whitespace) with `<` is a web page; no
/// model file starts that way.
fn looks_like_html(first: &[u8]) -> bool {
    let rest = first.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(first);
    rest.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<')
}

/// No `Content-Type`, or the generic binary one: the header says nothing, so look at the bytes.
fn untyped(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|ct| {
        let essence = ct.split(';').next().unwrap_or_default().trim();
        essence.is_empty() || essence.eq_ignore_ascii_case("application/octet-stream")
    })
}

/// `bytes <start>-<end>/<total|*>` → (start, total), checking `start <= end < total`.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok()?),
    };
    if start > end || total.is_some_and(|t| end >= t) {
        return None;
    }
    Some((start, total))
}

pub struct ReqwestBody {
    response: reqwest::Response,
    /// A first chunk already read to sniff for HTML; returned before anything else.
    pending: Option<Vec<u8>>,
}

impl Body for ReqwestBody {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, FetchError> {
        if let Some(chunk) = self.pending.take() {
            return Ok(Some(chunk));
        }
        self.response.chunk().await.map(|c| c.map(|b| b.to_vec())).map_err(classify)
    }
}

impl Fetcher for ReqwestFetcher {
    type Body = ReqwestBody;

    async fn open(&self, url: &str, offset: u64) -> Result<Opened<ReqwestBody>, FetchError> {
        let mut req = self.client.get(url);
        if offset > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let mut resp = req.send().await.map_err(classify)?;
        let headers = resp.headers();
        let content_type = headers.get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
        let mut html = content_type.is_some_and(|ct| ct.trim_start().to_ascii_lowercase().starts_with("text/html"));
        let sniff = !html && untyped(content_type);
        let status = resp.status().as_u16();
        let total = match status {
            200 => resp.content_length(),
            206 => {
                let (start, total) = headers
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range)
                    .ok_or(FetchError::BadRange)?;
                if start != offset {
                    return Err(FetchError::BadRange);
                }
                total
            }
            other => return Err(FetchError::Status(other)),
        };
        // Only a body that starts at byte 0 is sniffed: the middle of a model can hold a `<`.
        let mut pending = None;
        if sniff && status == 200 {
            pending = resp.chunk().await.map_err(classify)?.map(|b| b.to_vec());
            html = pending.as_deref().is_some_and(looks_like_html);
        }
        Ok(Opened { status, total, html, body: ReqwestBody { response: resp, pending } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    enum Reply {
        /// Write this raw response and close.
        Full(Vec<u8>),
        /// Write this response head, then hold the connection open without sending more.
        Stall(Vec<u8>),
    }

    type Log = Arc<Mutex<Vec<String>>>;

    /// A scripted HTTP/1.1 server on 127.0.0.1; returns its base URL and the request heads it saw.
    fn serve(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> (String, Log) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log: Log = Arc::default();
        let handler = Arc::new(handler);
        let seen = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let (handler, seen) = (handler.clone(), seen.clone());
                std::thread::spawn(move || handle(stream, &*handler, &seen));
            }
        });
        (base, log)
    }

    fn handle(mut stream: TcpStream, handler: &dyn Fn(&str) -> Reply, seen: &Log) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => return,
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        seen.lock().unwrap().push(head.clone());
        match handler(&head) {
            Reply::Full(bytes) => {
                let _ = stream.write_all(&bytes);
            }
            Reply::Stall(bytes) => {
                let _ = stream.write_all(&bytes);
                let _ = stream.flush();
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }

    fn response(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut out = out.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn path_of(head: &str) -> &str {
        head.split(' ').nth(1).unwrap_or_default()
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }

    fn fetcher() -> ReqwestFetcher {
        ReqwestFetcher::direct(Duration::from_secs(10), Duration::from_secs(5)).unwrap()
    }

    async fn drain<B: Body>(body: &mut B) -> Result<Vec<u8>, FetchError> {
        let mut all = Vec::new();
        while let Some(chunk) = body.next_chunk().await? {
            all.extend_from_slice(&chunk);
        }
        Ok(all)
    }

    #[tokio::test]
    async fn full_get_reports_200_total_and_streams_all_bytes() {
        let payload = data(200_000);
        let body = payload.clone();
        let (base, log) = serve(move |_| {
            Reply::Full(response(
                "200 OK",
                &[("Content-Length", body.len().to_string()), ("Content-Type", "application/octet-stream".into())],
                &body,
            ))
        });
        let mut opened = fetcher().open(&format!("{base}/m.onnx"), 0).await.unwrap();
        assert_eq!(opened.status, 200);
        assert_eq!(opened.total, Some(payload.len() as u64));
        assert!(!opened.html);
        assert_eq!(drain(&mut opened.body).await.unwrap(), payload);
        let head = log.lock().unwrap()[0].clone();
        assert_eq!(header(&head, "Range"), None);
        assert!(header(&head, "User-Agent").unwrap().starts_with("TuneLove/"));
    }

    #[tokio::test]
    async fn range_get_parses_content_range_total() {
        let payload = data(100);
        let tail = payload[10..].to_vec();
        let (base, log) = serve(move |_| {
            Reply::Full(response(
                "206 Partial Content",
                &[("Content-Length", tail.len().to_string()), ("Content-Range", "bytes 10-99/100".into())],
                &tail,
            ))
        });
        let mut opened = fetcher().open(&format!("{base}/m.onnx"), 10).await.unwrap();
        assert_eq!(opened.status, 206);
        assert_eq!(opened.total, Some(100));
        assert_eq!(drain(&mut opened.body).await.unwrap(), payload[10..]);
        assert_eq!(header(&log.lock().unwrap()[0], "Range"), Some("bytes=10-"));
    }

    #[tokio::test]
    async fn range_start_mismatch_is_bad_range() {
        let payload = data(100);
        let (base, _) = serve(move |_| {
            Reply::Full(response(
                "206 Partial Content",
                &[("Content-Length", "100".into()), ("Content-Range", "bytes 0-99/100".into())],
                &payload,
            ))
        });
        let r = fetcher().open(&format!("{base}/m.onnx"), 10).await;
        assert_eq!(r.err(), Some(FetchError::BadRange));
    }

    #[tokio::test]
    async fn malformed_or_missing_content_range_is_bad_range() {
        for value in [None, Some("bytes 10-5/100"), Some("bytes 10-100/100"), Some("items 10-99/100"), Some("garbage")] {
            let (base, _) = serve(move |_| {
                let mut headers = vec![("Content-Length", "0".to_string())];
                if let Some(v) = value {
                    headers.push(("Content-Range", v.into()));
                }
                Reply::Full(response("206 Partial Content", &headers, b""))
            });
            let r = fetcher().open(&format!("{base}/m.onnx"), 10).await;
            assert_eq!(r.err(), Some(FetchError::BadRange), "{value:?}");
        }
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 10-99/100"), Some((10, Some(100))));
        assert_eq!(parse_content_range("bytes 10-99/*"), Some((10, None)));
        assert_eq!(parse_content_range("bytes 0-0/1"), Some((0, Some(1))));
        for bad in ["", "bytes */100", "bytes 10-/100", "bytes a-b/c", "bytes 5-4/10", "bytes 0-10/10"] {
            assert_eq!(parse_content_range(bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn redirect_keeps_range_header() {
        let payload = data(100);
        let (base, log) = serve(move |head| match path_of(head) {
            "/a" => Reply::Full(response("302 Found", &[("Location", "/b".into()), ("Content-Length", "0".into())], b"")),
            _ => Reply::Full(response(
                "206 Partial Content",
                &[("Content-Length", "90".into()), ("Content-Range", "bytes 10-99/100".into())],
                &payload[10..],
            )),
        });
        let opened = fetcher().open(&format!("{base}/a"), 10).await.unwrap();
        assert_eq!((opened.status, opened.total), (206, Some(100)));
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(path_of(&log[1]), "/b");
        assert_eq!(header(&log[1], "Range"), Some("bytes=10-"));
    }

    #[tokio::test]
    async fn html_content_type_is_flagged() {
        let (base, _) = serve(|_| {
            let page = b"<html>blocked</html>";
            Reply::Full(response(
                "200 OK",
                &[("Content-Length", page.len().to_string()), ("Content-Type", "text/html; charset=utf-8".into())],
                page,
            ))
        });
        let opened = fetcher().open(&format!("{base}/m.onnx"), 0).await.unwrap();
        assert!(opened.html);
    }

    #[tokio::test]
    async fn untyped_200_starting_with_a_tag_is_html_and_keeps_its_bytes() {
        for (content_type, body, html) in [
            (None, b"\xEF\xBB\xBF \r\n<!DOCTYPE html><p>busy</p>".to_vec(), true),
            (Some("application/octet-stream"), b"  <html>".to_vec(), true),
            (Some("Application/Octet-Stream; x=1"), b"<html>".to_vec(), true),
            (None, data(5000), false),
            (Some("application/x-binary"), b"<html>".to_vec(), false),
        ] {
            let sent = body.clone();
            let (base, _) = serve(move |_| {
                let mut headers = vec![("Content-Length", sent.len().to_string())];
                if let Some(ct) = content_type {
                    headers.push(("Content-Type", ct.into()));
                }
                Reply::Full(response("200 OK", &headers, &sent))
            });
            let mut opened = fetcher().open(&format!("{base}/m.onnx"), 0).await.unwrap();
            assert_eq!(opened.html, html, "{content_type:?}");
            assert_eq!(drain(&mut opened.body).await.unwrap(), body, "sniffed chunk is not lost");
        }
    }

    #[tokio::test]
    async fn partial_bodies_are_not_sniffed() {
        let (base, _) = serve(|_| {
            Reply::Full(response(
                "206 Partial Content",
                &[("Content-Length", "2".into()), ("Content-Range", "bytes 10-11/12".into())],
                b"<x",
            ))
        });
        let opened = fetcher().open(&format!("{base}/m.onnx"), 10).await.unwrap();
        assert!(!opened.html);
    }

    #[tokio::test]
    async fn redirect_loop_is_fatal_not_retryable() {
        let (base, _) = serve(|_| Reply::Full(response("302 Found", &[("Location", "/a".into()), ("Content-Length", "0".into())], b"")));
        let err = fetcher().open(&format!("{base}/a"), 0).await.err().unwrap();
        assert!(matches!(err, FetchError::Fatal(_)), "{err:?}");
        assert!(!err.retryable());
    }

    #[tokio::test]
    async fn standard_client_refuses_plain_http_without_sending() {
        let (base, log) = serve(|_| Reply::Full(response("200 OK", &[("Content-Length", "0".into())], b"")));
        let err = ReqwestFetcher::standard().unwrap().open(&format!("{base}/m.onnx"), 0).await.err().unwrap();
        assert!(matches!(err, FetchError::Fatal(_)), "{err:?}");
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn certificate_rejections_are_recognised_by_os_error_code() {
        // Localised schannel text; only the code matters.
        assert!(certificate_rejected("error sending request: 证书链是由不受信任的颁发机构颁发的。 (os error -2146762487)"));
        assert!(certificate_rejected("x: The target principal name is incorrect. (os error -2146893022)"));
        assert!(certificate_rejected("x (os error 2148204809)"), "unsigned spelling of CERT_E_UNTRUSTEDROOT");
        assert!(certificate_rejected("ssl: certificate verify failed"));
        assert!(!certificate_rejected("connection reset (os error 10054)"));
        assert!(!certificate_rejected("os error -"));
        assert!(!certificate_rejected("tcp connect error: refused"));
    }

    #[test]
    fn error_chain_includes_sources() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = Outer(std::io::Error::other("tls handshake eof"));
        assert_eq!(error_chain(&e), "error sending request: tls handshake eof");
    }

    #[tokio::test]
    async fn error_statuses_map_to_status() {
        for (line, code, retry) in [
            ("404 Not Found", 404, false),
            ("503 Service Unavailable", 503, true),
            ("403 Forbidden", 403, false),
            ("429 Too Many Requests", 429, true),
        ] {
            let (base, _) = serve(move |_| Reply::Full(response(line, &[("Content-Length", "0".into())], b"")));
            let err = fetcher().open(&format!("{base}/m.onnx"), 0).await.err().unwrap();
            assert_eq!(err, FetchError::Status(code));
            assert_eq!(err.retryable(), retry, "{code}");
        }
        assert!(FetchError::Status(500).retryable() && FetchError::Status(599).retryable());
        assert!(!FetchError::Status(600).retryable() && !FetchError::BadRange.retryable());
        assert!(!FetchError::Fatal("x".into()).retryable());
        assert!(FetchError::Connect.retryable() && FetchError::Timeout.retryable());
        assert!(FetchError::Network("x".into()).retryable());
    }

    #[tokio::test]
    async fn stalled_body_times_out() {
        // A typed body is not sniffed, so the stall shows while reading it.
        let typed = vec![("Content-Length", "100".to_string()), ("Content-Type", "application/x-onnx".into())];
        let (base, _) = serve(move |_| Reply::Stall(response("200 OK", &typed, b"")));
        let f = ReqwestFetcher::direct(Duration::from_secs(1), Duration::from_millis(300)).unwrap();
        let mut opened = f.open(&format!("{base}/m.onnx"), 0).await.unwrap();
        let started = std::time::Instant::now();
        assert_eq!(drain(&mut opened.body).await.err(), Some(FetchError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn stall_before_the_sniffed_first_chunk_times_out_in_open() {
        let (base, _) = serve(|_| Reply::Stall(response("200 OK", &[("Content-Length", "100".into())], b"")));
        let f = ReqwestFetcher::direct(Duration::from_secs(1), Duration::from_millis(300)).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(f.open(&format!("{base}/m.onnx"), 0).await.err(), Some(FetchError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn refused_connection_is_connect() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let r = fetcher().open(&format!("http://127.0.0.1:{port}/m.onnx"), 0).await;
        assert_eq!(r.err(), Some(FetchError::Connect));
    }

    #[test]
    fn standard_client_builds() {
        assert!(ReqwestFetcher::standard().is_ok());
        assert!(ReqwestFetcher::new(Duration::from_secs(1), Duration::from_secs(1)).is_ok());
    }
}
