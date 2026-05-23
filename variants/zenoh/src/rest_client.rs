//! Single-mode sync RPC client for the `zenoh-plugin-rest` surface (T14.9b).
//!
//! When the variant is connected in `ThreadingMode::Single`, T14.9a's
//! sidecar lifecycle has already spawned a `zenohd` child with the REST
//! plugin live on `127.0.0.1:<rest_port>`. This module wires
//! `publish` and `poll_receive` through that HTTP surface using sync
//! Rust:
//!
//! * **publish**: blocking `HTTP PUT http://127.0.0.1:<rest_port>/<key>`
//!   with the encoded message body. Powered by `ureq` (sync, std::net
//!   based -- no tokio anywhere in the call graph).
//! * **poll_receive**: dedicated OS thread reads
//!   `http://127.0.0.1:<rest_port>/<key_expr>?_method=SUB` as a
//!   Server-Sent-Events (SSE) stream. Each `event:`/`data:` block
//!   becomes a decoded `ReceivedUpdate` and is enqueued on a bounded
//!   `std::sync::mpsc::sync_channel`. The variant's main thread drains
//!   the channel via `try_recv` on every tick. Drop-on-full semantics
//!   match the Multi-mode receive path (a back-pressured consumer
//!   produces JSONL gaps, not unbounded memory growth).
//!
//! No tokio runtime, no `async` -- the Single mode call graph reachable
//! from this module is genuinely synchronous. See CUSTOM.md
//! "T14.9b tokio-free verification" for the call-graph audit notes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64_STANDARD;
use base64::Engine as _;

use variant_base::types::ReceivedUpdate;

/// Bounded capacity for the SSE reader -> variant main thread channel.
///
/// 4096 mirrors the established log-from-reader / progress_coord
/// convention (T14.10 / T15.3): a few seconds of headroom at the
/// modest 1K msg/s workload T14.9b targets, drop-on-full beyond that.
/// Drops show up as JSONL receive gaps in the analysis, identical to
/// the Multi-mode bridge's drop semantics.
pub const RECEIVE_CHANNEL_CAPACITY: usize = 4096;

/// Wildcard key expression the Single-mode SSE subscriber listens on.
/// Must match the same set the Multi-mode subscriber covers
/// (`bench/**`) so the analysis sees the same path coverage regardless
/// of threading mode.
pub const SUBSCRIBER_WILDCARD: &str = "bench/**";

/// Build the HTTP PUT URL for a workload key on a localhost sidecar.
///
/// `rest_port` is the port the sidecar's REST plugin is bound to (see
/// `Sidecar::rest_port`). `key` is the already-derived Zenoh key
/// (no leading slash, no double `bench/` prefix -- see `path_to_key`
/// in `zenoh.rs`).
pub fn put_url(rest_port: u16, key: &str) -> String {
    format!("http://127.0.0.1:{}/{}", rest_port, key)
}

/// Build the HTTP GET-as-SSE subscription URL.
///
/// Kept as a public helper for diagnostics + unit testing. The
/// actual SSE reader thread issues the request via raw `TcpStream`
/// (so it can apply a per-read timeout that ureq's request-level
/// `timeout_global` cannot model), and therefore composes the path
/// inline rather than reusing this function.
///
/// The `zenoh-plugin-rest` plugin's REST surface upgrades a
/// `GET <key_expr>` request to a Server-Sent-Events stream when the
/// `Accept: text/event-stream` header is present. Each publication
/// on a key matching `<key_expr>` is delivered as one SSE event
/// whose `event:` line carries the sample kind (`PUT` / `DELETE`)
/// and whose `data:` line carries a JSON envelope
/// `{ "key": ..., "value": <base64>, "encoding": ..., "timestamp": ... }`.
///
/// **NB**: the T14.9b task brief suggested `?_method=SUB`. Empirical
/// inspection of `zenoh-plugin-rest` 1.9.0 (and the upstream source
/// at the same revision) shows the real trigger is the `Accept`
/// header; `?_method=SUB` is silently ignored. The audit prediction
/// was incorrect; this is the correct URL.
#[allow(dead_code)]
pub fn sse_url(rest_port: u16, key_expr: &str) -> String {
    format!("http://127.0.0.1:{}/{}", rest_port, key_expr)
}

/// Synchronous HTTP PUT client. Wraps the per-process `ureq::Agent`
/// so the variant can reuse the underlying TCP connection across
/// publishes. ureq is sync and built on `std::net` -- no tokio in the
/// call graph.
pub struct HttpPublisher {
    agent: ureq::Agent,
    rest_port: u16,
}

impl HttpPublisher {
    /// Build a new publisher pointing at `127.0.0.1:<rest_port>`.
    pub fn new(rest_port: u16) -> Self {
        // Tunables: send / recv timeouts sized so a brief sidecar
        // hiccup (GC, plugin reload, etc.) under sustained 1K msg/s
        // does not surface as a publish error. ureq's defaults
        // (~30 s) are too generous and would hide real wedges; 5 s
        // is enough to absorb any localhost burst while still
        // failing fast on a truly broken sidecar. We do NOT cap the
        // entire call via `timeout_global` because that races a
        // legitimate publish against the global deadline; the
        // per-stage `connect` / `send_request` / `send_body` /
        // `recv_response` knobs are bounded individually.
        //
        // HTTP keep-alive: T14.9c reverses T14.9b's explicit disable.
        // The stress fixture at 100K msg/s qos2-4 Single saturates
        // Windows' ~16K ephemeral port range within ~1 s when every
        // PUT opens a fresh TCP connection, surfacing as
        // `io: ... (os error 10048)` (WSAEADDRINUSE). With keep-alive
        // on (ureq 3.x default: `max_idle_connections = 10`,
        // `max_idle_connections_per_host = 3`), the agent pools the
        // localhost connection and reuses it across PUTs; outbound
        // TCP socket count drops from N-per-publish to ~1. ureq's
        // defaults are intentionally left untouched -- they're tuned
        // for the same single-host pattern this variant exhibits.
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_send_request(Some(Duration::from_secs(5)))
            .timeout_send_body(Some(Duration::from_secs(5)))
            .timeout_recv_response(Some(Duration::from_secs(5)))
            .timeout_recv_body(Some(Duration::from_secs(5)))
            .build();
        let agent: ureq::Agent = config.into();
        Self { agent, rest_port }
    }

    /// Issue a synchronous HTTP PUT against `http://127.0.0.1:<port>/<key>`
    /// with `body` as the request body. The `Content-Type:
    /// application/octet-stream` header is set so the
    /// `zenoh-plugin-rest` write path stores the bytes as-is rather
    /// than trying to interpret them as text or JSON (our
    /// `MessageCodec` output is little-endian binary and is NEVER
    /// valid UTF-8).
    ///
    /// Retries once on transient transport errors (connection
    /// refused, read timeout, etc.) before propagating the failure
    /// upstream. A single retry is sufficient in practice: the
    /// failure modes seen on Windows localhost are "stale half-open
    /// connection" / "ECONNRESET during shutdown" -- one fresh
    /// connection clears them. Genuine sidecar wedges fail both
    /// attempts and surface to the variant's publish loop, which
    /// then exits with the error (driver propagates it).
    pub fn put(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let url = put_url(self.rest_port, key);
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..2 {
            let send = self
                .agent
                .put(&url)
                .content_type("application/octet-stream")
                .send(&body[..]);
            match send {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    // T14.9c: drain the response body before dropping
                    // the Response so ureq returns the underlying TCP
                    // connection to the pool (keep-alive reuse). Body
                    // is typically empty (Content-Length: 0) from
                    // zenoh-plugin-rest's PUT handler, so this is a
                    // single bounded read.
                    let _ = response.body_mut().read_to_vec();
                    if (200..300).contains(&status) {
                        return Ok(());
                    }
                    last_err = Some(anyhow::anyhow!(
                        "HTTP PUT {url} returned status {status} (attempt {attempt})",
                    ));
                }
                Err(e) => {
                    last_err = Some(anyhow::anyhow!("HTTP PUT {url} failed: {e}"));
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("HTTP PUT {url} failed (no error captured)")))
    }

    /// REST port the publisher targets (for diagnostics).
    #[allow(dead_code)] // diagnostic accessor; held in case CUSTOM.md trace adds it
    pub fn rest_port(&self) -> u16 {
        self.rest_port
    }
}

/// Background SSE reader thread + its bounded mpsc to the variant
/// main thread. Created by `SseReader::start`, drained by the
/// variant's `poll_receive`, and torn down by `SseReader::stop`.
pub struct SseReader {
    rx: Receiver<ReceivedUpdate>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Held so `stop` can interrupt a blocked read on the SSE socket
    /// by dropping the agent (closes the connection on most platforms).
    /// Reserved for future use; we currently rely on the atomic flag
    /// + bounded timeout per-read.
    #[allow(dead_code)]
    rest_port: u16,
}

impl SseReader {
    /// Start a fresh SSE reader thread subscribed to `bench/**` on
    /// `127.0.0.1:<rest_port>`. Returns a handle the variant stores
    /// alongside the sidecar; the reader thread runs until `stop` is
    /// called (or the variant process exits).
    ///
    /// `self_runner` is the variant's own runner name; samples whose
    /// decoded `writer` field equals `self_runner` are dropped at the
    /// reader BEFORE they reach the recv channel (per
    /// `compact-log-schema.md` event kind 1 / `receive`). Zenoh's
    /// wildcard `bench/**` subscription matches the variant's own
    /// publishes, so this filter is required to prevent self-echo
    /// inflation in the variant's `inc_received` counter.
    pub fn start(rest_port: u16, self_runner: String, decode: SseDecodeFn) -> Self {
        let (tx, rx) = sync_channel::<ReceivedUpdate>(RECEIVE_CHANNEL_CAPACITY);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.clone();
        let handle = std::thread::Builder::new()
            .name(format!("zenoh-sse-{}", rest_port))
            .spawn(move || {
                sse_reader_loop(rest_port, tx, stop_flag_clone, self_runner, decode);
            })
            .expect("spawn zenoh SSE reader thread");
        Self {
            rx,
            stop_flag,
            handle: Some(handle),
            rest_port,
        }
    }

    /// Non-blocking drain of one decoded update, mirroring
    /// `mpsc::Receiver::try_recv` semantics: `Ok(None)` when the
    /// channel is empty or the reader thread has exited cleanly.
    pub fn try_recv(&self) -> Result<Option<ReceivedUpdate>> {
        match self.rx.try_recv() {
            Ok(update) => Ok(Some(update)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                // Reader thread exited (clean shutdown or sidecar
                // crash). Surface as no-data so the driver can keep
                // ticking until disconnect; the JSONL log will just
                // show fewer receives.
                Ok(None)
            }
        }
    }

    /// Signal the reader thread to exit. Idempotent. The thread may
    /// take up to `SSE_READ_TIMEOUT` to actually return because the
    /// blocking read on the SSE socket carries that bound.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SseReader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bound on the per-read blocking call inside the SSE reader. The
/// thread checks the stop-flag between reads, so this caps the
/// worst-case shutdown latency at one timeout's worth.
const SSE_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Function pointer that decodes raw message bytes (as produced by
/// `MessageCodec::encode`) into a `ReceivedUpdate`. The variant's
/// `MessageCodec::decode` matches this signature; passing it in as a
/// fn pointer keeps this module decoupled from the codec details and
/// easy to unit test.
pub type SseDecodeFn = fn(&[u8]) -> Result<ReceivedUpdate>;

/// Extract the raw payload bytes from an SSE event's `data:` field
/// content. The `zenoh-plugin-rest` plugin always wraps the sample
/// in a JSON envelope of the form:
///
/// ```json
/// { "key": "<keyexpr>", "value": "<base64>", "encoding": "<enc>",
///   "timestamp": "<ts-or-null>" }
/// ```
///
/// `value` is base64-encoded (standard alphabet, padded) whenever the
/// sample's encoding does not match a JSON / UTF-8 text encoding. The
/// variant's `MessageCodec::encode` always emits binary bytes that
/// fall into that "other" bucket on the plugin side, so we
/// unconditionally base64-decode `value`.
///
/// Returns the decoded payload bytes or an error if the envelope is
/// malformed or `value` is missing / not base64.
pub fn extract_payload_from_sse_data(data: &str) -> Result<Vec<u8>> {
    let json: serde_json::Value = serde_json::from_str(data)
        .with_context(|| format!("SSE data is not valid JSON: {data}"))?;
    let value = json
        .get("value")
        .ok_or_else(|| anyhow::anyhow!("SSE data missing `value` field: {data}"))?;
    let s = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("SSE data `value` is not a string: {data}"))?;
    B64_STANDARD
        .decode(s)
        .with_context(|| format!("SSE data `value` is not valid base64: {s}"))
}

/// Main loop for the SSE reader thread. Opens a long-lived HTTP GET
/// against the sidecar's subscription endpoint and parses incoming
/// SSE events line-by-line, pushing each decoded update onto `tx`.
///
/// Reconnect policy: on any transport / parse error the loop sleeps
/// briefly and retries. The stop flag is checked between retries
/// and on every read timeout.
///
/// Implementation note: we use raw `TcpStream` here rather than
/// `ureq`'s response body so we can apply a per-read timeout
/// (`set_read_timeout`) without a request-level global cap. ureq's
/// `timeout_global` would expire on a long-poll SSE that has no
/// inbound traffic, even though the connection is healthy.
fn sse_reader_loop(
    rest_port: u16,
    tx: SyncSender<ReceivedUpdate>,
    stop_flag: Arc<AtomicBool>,
    self_runner: String,
    decode: SseDecodeFn,
) {
    let host = "127.0.0.1";
    let addr: SocketAddr = format!("{host}:{rest_port}")
        .parse()
        .expect("static localhost addr is valid");

    while !stop_flag.load(Ordering::Acquire) {
        let stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(s) => s,
            Err(_) => {
                sleep_or_stop(Duration::from_millis(100), &stop_flag);
                continue;
            }
        };
        if stream.set_read_timeout(Some(SSE_READ_TIMEOUT)).is_err() {
            sleep_or_stop(Duration::from_millis(100), &stop_flag);
            continue;
        }
        if stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .is_err()
        {
            sleep_or_stop(Duration::from_millis(100), &stop_flag);
            continue;
        }

        // Issue the HTTP/1.1 GET. `Accept: text/event-stream` is
        // the trigger that makes zenoh-plugin-rest upgrade the
        // response to a long-lived SSE stream instead of returning
        // a one-shot JSON snapshot.
        let path = format!("/{}", SUBSCRIBER_WILDCARD);
        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{rest_port}\r\n\
             Accept: text/event-stream\r\n\
             Connection: keep-alive\r\n\
             \r\n"
        );
        {
            let mut s = &stream;
            if s.write_all(req.as_bytes()).is_err() {
                sleep_or_stop(Duration::from_millis(100), &stop_flag);
                continue;
            }
            if s.flush().is_err() {
                sleep_or_stop(Duration::from_millis(100), &stop_flag);
                continue;
            }
        }

        let mut reader = BufReader::new(stream);

        // Read & discard response headers up to the empty line. If
        // any read errors (other than a brief timeout) abort the
        // session and reconnect.
        let mut status_line = String::new();
        if !read_line_with_retry(&mut reader, &mut status_line, &stop_flag) {
            continue;
        }
        if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
            // Sidecar returned non-200 -- maybe REST plugin is
            // still initialising. Back off and try again.
            sleep_or_stop(Duration::from_millis(200), &stop_flag);
            continue;
        }
        loop {
            if stop_flag.load(Ordering::Acquire) {
                return;
            }
            let mut hdr = String::new();
            if !read_line_with_retry(&mut reader, &mut hdr, &stop_flag) {
                // Connection died mid-headers; reconnect.
                break;
            }
            if hdr == "\r\n" || hdr == "\n" || hdr.is_empty() {
                break;
            }
        }

        // The response body is `Transfer-Encoding: chunked`. Strip
        // the chunk-length prefixes and feed payload lines into the
        // SSE parser. Lines that look like hex-only (chunk size)
        // are skipped.
        let mut parser = SseParser::new();
        loop {
            if stop_flag.load(Ordering::Acquire) {
                return;
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // EOF: sidecar closed the connection. Reconnect.
                    break;
                }
                Ok(_) => {
                    // Skip chunked-transfer length lines (hex
                    // followed by `\r\n` with no other content).
                    if is_chunk_size_line(&line) {
                        continue;
                    }
                    if let Some(event) = parser.feed_line(&line) {
                        // Each SSE event's `data:` is a JSON envelope
                        // produced by zenoh-plugin-rest. Pull out the
                        // base64 `value` field, decode it, and hand
                        // the raw bytes to the codec. Silent skip on
                        // any decode failure -- one bad sample must
                        // not stop the reader loop.
                        let payload = match extract_payload_from_sse_data(&event.data) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        if let Ok(update) = decode(&payload) {
                            // Self-writer filter (same contract as
                            // the Multi-mode subscriber_task): per
                            // `compact-log-schema.md` event kind 1
                            // (`receive`), variants MUST drop
                            // payloads whose decoded `writer` equals
                            // the variant's own runner BEFORE they
                            // reach the recv channel (and thus
                            // before `inc_received`). The SSE
                            // subscription is on `bench/**` and
                            // matches the sidecar's reflection of
                            // our own publishes -- so without this
                            // filter the variant's `received`
                            // counter double-counts self-echoes
                            // (e.g. two-runner localhost spawns
                            // would log `received` == 2 × `sent`).
                            if update.writer == self_runner {
                                continue;
                            }
                            // Drop-on-full: same semantics as the
                            // Multi-mode bridge. A blocked consumer
                            // is preferable to unbounded memory
                            // growth on a sustained burst.
                            let _ = tx.try_send(update);
                        }
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Bounded-read timeout fired -- normal under no
                    // traffic. Re-check stop_flag and continue.
                    continue;
                }
                Err(_) => break,
            }
        }
    }
}

/// Read one line into `buf` from the SSE stream, retrying on
/// timeouts as long as the stop flag isn't set. Returns true if a
/// line was read, false if the connection is dead or stop was
/// requested.
fn read_line_with_retry<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
    stop_flag: &AtomicBool,
) -> bool {
    loop {
        if stop_flag.load(Ordering::Acquire) {
            return false;
        }
        match reader.read_line(buf) {
            Ok(0) => return false,
            Ok(_) => return true,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => return false,
        }
    }
}

/// Return true if the line is a chunked-transfer chunk-size prefix
/// (hex chars only, terminated by `\r\n` or `\n`, may include
/// chunk extensions after a `;`). Empty lines are NOT chunk-size
/// lines -- they terminate SSE events and must reach the parser.
fn is_chunk_size_line(line: &str) -> bool {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return false;
    }
    // Allow `<hex>;<extensions>` chunked-transfer format.
    let size_part = trimmed.split(';').next().unwrap_or(trimmed);
    !size_part.is_empty() && size_part.chars().all(|c| c.is_ascii_hexdigit())
}

fn sleep_or_stop(d: Duration, stop_flag: &AtomicBool) {
    // Break the sleep into ~20 ms slices so a stop signal during a
    // back-off doesn't wedge shutdown for the full duration.
    let slice = Duration::from_millis(20);
    let mut remaining = d;
    while remaining > Duration::ZERO {
        if stop_flag.load(Ordering::Acquire) {
            return;
        }
        let s = slice.min(remaining);
        std::thread::sleep(s);
        remaining = remaining.saturating_sub(s);
    }
}

/// Parsed Server-Sent-Events event with the two fields we actually
/// consume. Per the SSE spec, `event:` is the event-name (optional;
/// defaults to "message") and `data:` is the payload (multi-line
/// `data:` fields are concatenated with `\n`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Minimal SSE parser sufficient for the `zenoh-plugin-rest` surface.
/// Accumulates `event:` and `data:` lines until a blank line (the
/// SSE event terminator), then yields one `SseEvent`.
///
/// Public-in-crate so unit tests can exercise the parser without
/// spinning up a fake HTTP server.
pub struct SseParser {
    current_event: String,
    current_data: String,
    has_content: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            current_event: String::new(),
            current_data: String::new(),
            has_content: false,
        }
    }

    /// Feed one input line (with or without trailing newline). Returns
    /// `Some(event)` exactly when the line terminates a non-empty
    /// SSE event (a blank line per the spec).
    pub fn feed_line(&mut self, raw: &str) -> Option<SseEvent> {
        // Strip a single trailing `\r\n` or `\n` so we treat
        // line-terminated and line-included inputs uniformly.
        let line = raw.strip_suffix("\r\n").unwrap_or(raw);
        let line = line.strip_suffix('\n').unwrap_or(line);

        if line.is_empty() {
            // End of event. Emit only if we accumulated something.
            if self.has_content {
                let event = SseEvent {
                    event: std::mem::take(&mut self.current_event),
                    data: std::mem::take(&mut self.current_data),
                };
                self.has_content = false;
                return Some(event);
            }
            return None;
        }

        // SSE comment lines start with `:` -- ignore.
        if line.starts_with(':') {
            return None;
        }

        // `field:value` (the SSE spec allows an optional single space
        // after the colon; consume it if present).
        if let Some((field, value)) = line.split_once(':') {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    self.current_event = value.to_string();
                    self.has_content = true;
                }
                "data" => {
                    if !self.current_data.is_empty() {
                        self.current_data.push('\n');
                    }
                    self.current_data.push_str(value);
                    self.has_content = true;
                }
                // SSE spec defines `id` and `retry` too; we don't
                // need them and the spec says to ignore unknown
                // fields silently.
                _ => {}
            }
        }
        None
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper used by the publish path: a `ureq::Agent` that errors out
/// on a slow read. Wrapping the agent in a `Read`-impl lets the
/// blocking SSE reader thread receive a `WouldBlock`/`TimedOut` error
/// rather than wedging forever.
///
/// (Kept here for symmetry with future work; the SSE loop above
/// already configures a per-call timeout on `agent.get(...)`.)
#[allow(dead_code)]
pub struct BoundedReader<R: Read> {
    inner: R,
}

#[cfg(test)]
mod tests {
    use super::*;
    use variant_base::types::Qos;

    #[test]
    fn put_url_localhost_port_key() {
        assert_eq!(put_url(20100, "bench/0"), "http://127.0.0.1:20100/bench/0");
        assert_eq!(
            put_url(20200, "bench/42"),
            "http://127.0.0.1:20200/bench/42"
        );
    }

    #[test]
    fn sse_url_localhost_port_keyexpr() {
        assert_eq!(
            sse_url(20100, "bench/**"),
            "http://127.0.0.1:20100/bench/**"
        );
    }

    /// Unit: the parser yields a complete event when `data:` is
    /// followed by a blank line. Mirrors the trivial happy path the
    /// SSE reader walks once per inbound Zenoh sample.
    #[test]
    fn sse_parser_complete_event() {
        let mut p = SseParser::new();
        assert_eq!(p.feed_line("event: PUT\n"), None);
        assert_eq!(p.feed_line("data: hello\n"), None);
        let e = p.feed_line("\n").expect("blank line terminates event");
        assert_eq!(e.event, "PUT");
        assert_eq!(e.data, "hello");
    }

    /// Unit: a partial event (no blank line yet) does NOT emit. The
    /// SSE reader must wait for the terminator.
    #[test]
    fn sse_parser_partial_event_no_emit() {
        let mut p = SseParser::new();
        assert!(p.feed_line("event: PUT\n").is_none());
        assert!(p.feed_line("data: partial\n").is_none());
        // No blank line yet -- nothing emitted.
    }

    /// Unit: multi-line `data:` fields are concatenated with `\n`
    /// per the SSE spec.
    #[test]
    fn sse_parser_multi_line_data() {
        let mut p = SseParser::new();
        p.feed_line("data: line-one\n");
        p.feed_line("data: line-two\n");
        let e = p.feed_line("\n").expect("blank line terminates event");
        assert_eq!(e.event, "");
        assert_eq!(e.data, "line-one\nline-two");
    }

    /// Unit: a blank line with no preceding fields is ignored (no
    /// spurious empty event).
    #[test]
    fn sse_parser_blank_line_alone_emits_nothing() {
        let mut p = SseParser::new();
        assert!(p.feed_line("\n").is_none());
        assert!(p.feed_line("\n").is_none());
    }

    /// Unit: comment lines (`: foo`) are ignored.
    #[test]
    fn sse_parser_comment_lines_ignored() {
        let mut p = SseParser::new();
        p.feed_line(": ping\n");
        p.feed_line("data: hello\n");
        let e = p.feed_line("\n").unwrap();
        assert_eq!(e.data, "hello");
    }

    /// Unit: the optional space after the colon is consumed.
    #[test]
    fn sse_parser_colon_space_consumed() {
        let mut p = SseParser::new();
        // With space.
        p.feed_line("data: with-space\n");
        let e = p.feed_line("\n").unwrap();
        assert_eq!(e.data, "with-space");
        // Without space.
        let mut p = SseParser::new();
        p.feed_line("data:no-space\n");
        let e = p.feed_line("\n").unwrap();
        assert_eq!(e.data, "no-space");
    }

    /// Unit: chunked-transfer chunk-size prefix detection. SSE
    /// blank lines (event terminators) and `event:`/`data:` lines
    /// must NOT be treated as chunk sizes; hex-only lines must.
    #[test]
    fn is_chunk_size_line_classification() {
        // Hex chunk sizes (with and without chunk extensions).
        assert!(is_chunk_size_line("1a\r\n"));
        assert!(is_chunk_size_line("FF\n"));
        assert!(is_chunk_size_line("100;ext=foo\r\n"));
        // SSE event terminator -- empty line.
        assert!(!is_chunk_size_line("\r\n"));
        assert!(!is_chunk_size_line("\n"));
        assert!(!is_chunk_size_line(""));
        // SSE field lines.
        assert!(!is_chunk_size_line("event: PUT\r\n"));
        assert!(!is_chunk_size_line("data:{\"key\":\"x\"}\r\n"));
        // Mixed alphanumeric (not pure hex).
        assert!(!is_chunk_size_line("notHex\r\n"));
    }

    /// Unit: a complete back-to-back stream of two events parses
    /// cleanly.
    #[test]
    fn sse_parser_two_events_in_sequence() {
        let mut p = SseParser::new();
        p.feed_line("event: PUT\n");
        p.feed_line("data: one\n");
        let e1 = p.feed_line("\n").unwrap();
        assert_eq!(e1.data, "one");
        p.feed_line("event: PUT\n");
        p.feed_line("data: two\n");
        let e2 = p.feed_line("\n").unwrap();
        assert_eq!(e2.data, "two");
    }

    /// Integration-ish: HTTP PUT request shape via a tiny mock TCP
    /// listener that records what `HttpPublisher::put` sends. The
    /// listener accepts one connection, reads up to the body
    /// terminator, replies with `HTTP/1.1 200 OK`, and shuts down.
    /// We assert on the method, target path, content-type header,
    /// and body bytes.
    #[test]
    fn http_publisher_put_request_shape() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();

        let body = vec![1u8, 2, 3, 4, 5];
        let body_clone = body.clone();

        // Mock server thread: accept one connection, drain the
        // request, reply 200, hang up. Send the response promptly
        // so the client (under a global timeout) sees a response.
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_millis(1500)))
                .unwrap();
            // Read until we've seen the full body. We size the
            // buffer big enough that one or two reads suffice.
            let mut buf = vec![0u8; 8192];
            let mut total = 0usize;
            // Heuristic: keep reading until we see `\r\n\r\n`
            // followed by `body_clone.len()` more bytes. We don't
            // parse chunked encoding -- ureq uses Content-Length
            // for small Vec<u8> bodies.
            loop {
                let n = match stream.read(&mut buf[total..]) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                total += n;
                if let Some(headers_end) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n") {
                    let body_seen = total - (headers_end + 4);
                    if body_seen >= body_clone.len() {
                        break;
                    }
                }
                if total >= buf.len() {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf[..total]).to_string();
            let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp);
            let _ = stream.flush();
            // Linger briefly so the client receives the response
            // before the FIN. On Windows immediate shutdown can
            // race and surface as RST -> client timeout.
            std::thread::sleep(Duration::from_millis(50));
            let _ = stream.shutdown(std::net::Shutdown::Both);
            drop(stream);
            req
        });

        // Client side: issue the PUT.
        let publisher = HttpPublisher::new(port);
        publisher.put("bench/7", body.clone()).expect("PUT");

        let req = server.join().expect("mock server thread");
        assert!(req.starts_with("PUT "), "expected PUT method, got:\n{req}");
        assert!(
            req.contains(" /bench/7 "),
            "request line must include /bench/7, got:\n{req}"
        );
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: application/octet-stream"),
            "request must set Content-Type: application/octet-stream, got:\n{req}"
        );
        let body_pos = req.find("\r\n\r\n").expect("headers terminator");
        let req_body = &req.as_bytes()[body_pos + 4..];
        assert!(
            req_body.windows(body.len()).any(|w| w == body.as_slice()),
            "body bytes not found in request payload (got {} body bytes)",
            req_body.len()
        );
    }

    /// Integration: HttpPublisher surfaces non-2xx as an error.
    #[test]
    fn http_publisher_put_errors_on_non_2xx() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            // Drain something so the client's send doesn't block.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.read(&mut buf);
            let resp = b"HTTP/1.1 500 Internal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });

        let publisher = HttpPublisher::new(port);
        let r = publisher.put("bench/0", vec![0u8; 4]);
        assert!(r.is_err(), "non-2xx must surface as Err");
        let _ = server.join();
    }

    /// Decoder hook: the SSE reader hands raw bytes (after the JSON
    /// envelope has been unwrapped + base64-decoded) to the codec fn
    /// pointer. We verify the wiring with a tiny decoder that just
    /// echoes a fixed update -- the real `MessageCodec::decode` is
    /// exercised in `zenoh.rs` tests.
    fn fixed_decode(_: &[u8]) -> Result<ReceivedUpdate> {
        Ok(ReceivedUpdate {
            writer: "fixed".to_string(),
            seq: 1,
            path: "/bench/0".to_string(),
            qos: Qos::BestEffort,
            payload: vec![],
        })
    }

    /// Round-trip: build a JSON envelope shaped like what
    /// zenoh-plugin-rest sends and verify
    /// `extract_payload_from_sse_data` returns the raw bytes.
    #[test]
    fn extract_payload_decodes_base64_value() {
        let payload = b"\x01\x02\x03\xFF\x00binary";
        let b64 = B64_STANDARD.encode(payload);
        let envelope = serde_json::json!({
            "key": "bench/7",
            "value": b64,
            "encoding": "application/octet-stream",
            "timestamp": null
        });
        let s = serde_json::to_string(&envelope).unwrap();
        let decoded = extract_payload_from_sse_data(&s).expect("decode");
        assert_eq!(decoded, payload);
    }

    /// Bad JSON -> Err.
    #[test]
    fn extract_payload_rejects_bad_json() {
        assert!(extract_payload_from_sse_data("not json {").is_err());
    }

    /// Missing `value` field -> Err.
    #[test]
    fn extract_payload_rejects_missing_value() {
        let s = r#"{"key":"bench/0"}"#;
        assert!(extract_payload_from_sse_data(s).is_err());
    }

    /// `value` not a string -> Err.
    #[test]
    fn extract_payload_rejects_non_string_value() {
        let s = r#"{"key":"bench/0","value":42}"#;
        assert!(extract_payload_from_sse_data(s).is_err());
    }

    /// `value` not valid base64 -> Err.
    #[test]
    fn extract_payload_rejects_bad_base64() {
        let s = r#"{"key":"bench/0","value":"!!!not-base64!!!"}"#;
        assert!(extract_payload_from_sse_data(s).is_err());
    }

    /// T14.9c: the publisher's ureq Agent must have HTTP keep-alive
    /// enabled so consecutive PUTs reuse the same TCP connection.
    /// With keep-alive disabled the Single-mode stress workload (100K
    /// msg/s qos2-4) exhausts Windows' ~16K ephemeral port pool within
    /// ~1 s and fails as WSAEADDRINUSE (`os error 10048`).
    ///
    /// We assert on the Agent's config directly: both
    /// `max_idle_connections` and `max_idle_connections_per_host` must
    /// be > 0. Zero on either knob disables pooling for this Agent.
    #[test]
    fn http_publisher_has_keepalive_enabled() {
        let publisher = HttpPublisher::new(20100);
        let cfg = publisher.agent.config();
        assert!(
            cfg.max_idle_connections() > 0,
            "T14.9c: max_idle_connections must be > 0 (keep-alive on); got {}",
            cfg.max_idle_connections()
        );
        assert!(
            cfg.max_idle_connections_per_host() > 0,
            "T14.9c: max_idle_connections_per_host must be > 0 (keep-alive on); got {}",
            cfg.max_idle_connections_per_host()
        );
    }

    #[test]
    fn sse_reader_stop_is_idempotent() {
        // Point the reader at a bogus port -- it will fail to connect
        // and back off. `stop` must still terminate the thread cleanly.
        let mut r = SseReader::start(1, "test-runner".to_string(), fixed_decode);
        // Give the loop a chance to spin once.
        std::thread::sleep(Duration::from_millis(60));
        r.stop();
        // Calling again must be a no-op (no panic, no hang).
        r.stop();
    }
}
