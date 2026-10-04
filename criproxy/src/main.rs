//! k3dev-criproxy: a filtering Docker API proxy that scopes one cri-dockerd to
//! one k3dev cluster. Zero external dependencies (std only).
//!
//! k3s runs with `--docker`, so every cluster's kubelet drives its own embedded
//! cri-dockerd against the *shared* host Docker daemon. With two clusters up,
//! each kubelet's `ListContainers`/`ListPodSandbox` also enumerates the other
//! cluster's pod containers, decides their pod UIDs are unknown, and garbage
//! collects them - the two clusters delete each other's sandboxes in a loop.
//!
//! This proxy sits on the unix socket between them:
//!
//!   kubelet -> cri-dockerd -> /var/run/docker.sock (this proxy)
//!                          -> /var/run/docker-host.sock (real dockerd)
//!
//! and makes that impossible: every container created through it is stamped
//! with a `k3dev.cluster` label, and every listing/event stream is filtered
//! down to that label, so cri-dockerd can only ever see its own cluster.
//!
//! Usage: k3dev-criproxy --listen <path> --upstream <path> --cluster <name>
//!        k3dev-criproxy --version
//!
//! Each accepted connection carries exactly one HTTP/1.1 request: we force
//! `Connection: close` on the way upstream, so after the request head is
//! forwarded the proxy never has to understand HTTP framing again and can just
//! splice raw bytes both ways. That keeps hijacked streams (exec, attach) and
//! long-lived streams (/events, log follow) working untouched.

use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

/// Proxy protocol version, printed by `--version` so the host side can detect
/// an outdated binary baked into a prebuilt image and reinstall a fresh one.
const VERSION: &str = "1";

/// Label stamped on created containers and matched on listings/events.
const LABEL_KEY: &str = "k3dev.cluster";

/// Hard cap on the request head; a client that never sends `\r\n\r\n` must not
/// be able to grow the buffer without bound.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Bodies larger than this are forwarded untouched instead of being buffered
/// for rewriting. Container-create bodies are a few KiB at most.
const MAX_REWRITE_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Read timeout for the request head (and the create body). Cleared before the
/// raw splice so long-lived streams never time out.
const HEAD_TIMEOUT: Duration = Duration::from_secs(60);

macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[k3dev-criproxy] {}", format_args!($($arg)*))
    };
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version") {
        let _ = std::io::stdout().write_all(format!("k3dev-criproxy {}\n", VERSION).as_bytes());
        return;
    }

    let cfg = match parse_args(&args) {
        Some(c) => c,
        None => {
            usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&cfg) {
        log!("fatal: {}", e);
        std::process::exit(1);
    }
}

fn usage() {
    eprintln!(
        "usage: k3dev-criproxy --listen <path> --upstream <path> --cluster <name>\n       k3dev-criproxy --version"
    );
}

struct Config {
    listen: String,
    upstream: String,
    cluster: String,
}

/// Parse `--listen`/`--upstream`/`--cluster`. All three are required and must
/// be non-empty; anything else is a usage error.
fn parse_args(args: &[String]) -> Option<Config> {
    let mut listen = None;
    let mut upstream = None;
    let mut cluster = None;

    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--listen" => &mut listen,
            "--upstream" => &mut upstream,
            "--cluster" => &mut cluster,
            _ => return None,
        };
        let value = args.get(i + 1)?;
        if value.is_empty() || value.starts_with("--") || slot.is_some() {
            return None;
        }
        *slot = Some(value.clone());
        i += 2;
    }

    Some(Config {
        listen: listen?,
        upstream: upstream?,
        cluster: cluster?,
    })
}

fn run(cfg: &Config) -> io::Result<()> {
    // A stale socket file from a previous run would make bind() fail.
    if let Err(e) = std::fs::remove_file(&cfg.listen) {
        if e.kind() != ErrorKind::NotFound {
            log!("could not remove stale socket {}: {}", cfg.listen, e);
        }
    }

    let listener = UnixListener::bind(&cfg.listen)?;
    // cri-dockerd may run as a different uid than the proxy.
    std::fs::set_permissions(&cfg.listen, std::fs::Permissions::from_mode(0o666))?;

    log!(
        "listening on {} -> {} (cluster {})",
        cfg.listen,
        cfg.upstream,
        cfg.cluster
    );

    for conn in listener.incoming() {
        match conn {
            Ok(client) => {
                let upstream = cfg.upstream.clone();
                let cluster = cfg.cluster.clone();
                // One thread per connection; a panic here unwinds this thread
                // only and the accept loop keeps going.
                if let Err(e) = std::thread::Builder::new().spawn(move || {
                    if let Err(e) = handle_connection(client, &upstream, &cluster) {
                        log!("connection failed: {}", e);
                    }
                }) {
                    log!("could not spawn connection thread: {}", e);
                }
            }
            Err(e) => log!("accept failed: {}", e),
        }
    }

    Ok(())
}

// --- Connection handling ---

fn handle_connection(mut client: UnixStream, upstream_path: &str, cluster: &str) -> io::Result<()> {
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));

    let (buf, head_end) = read_head(&mut client)?;
    let request = build_request(&mut client, &buf, head_end, cluster)?;

    // No timeouts past this point: /events and log follow stay open for hours.
    let _ = client.set_read_timeout(None);

    let mut upstream = UnixStream::connect(upstream_path)?;
    upstream.write_all(&request)?;
    upstream.flush()?;

    splice(client, upstream)
}

/// Read bytes until the end of the request head (`\r\n\r\n`). Returns the whole
/// buffer read so far plus the index just past the blank line - anything after
/// that index is body (or a pipelined request) already in hand.
fn read_head(stream: &mut UnixStream) -> io::Result<(Vec<u8>, usize)> {
    let mut buf = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8 * 1024];
    // Bytes already searched. Rewind 3 so a marker straddling a read boundary
    // is still found, but never skip past freshly-read bytes.
    let mut searched = 0usize;

    loop {
        let scan_from = searched.saturating_sub(3);
        if let Some(pos) = find(&buf[scan_from..], b"\r\n\r\n") {
            return Ok((buf, scan_from + pos + 4));
        }
        searched = buf.len();
        if buf.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "request head exceeds 64 KiB",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "connection closed before end of request head",
                ))
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Turn the raw client bytes into the request to send upstream, applying the
/// rewrite rules. Anything we cannot confidently parse is forwarded unmodified.
fn build_request(
    client: &mut UnixStream,
    buf: &[u8],
    head_end: usize,
    cluster: &str,
) -> io::Result<Vec<u8>> {
    let rest = &buf[head_end..];

    let head = match std::str::from_utf8(&buf[..head_end]) {
        Ok(h) => h,
        Err(_) => {
            log!("request head is not valid UTF-8, forwarding verbatim");
            return Ok(buf.to_vec());
        }
    };
    let (line, headers) = match parse_head(head) {
        Some(parsed) => parsed,
        None => {
            log!("malformed request head, forwarding verbatim");
            return Ok(buf.to_vec());
        }
    };

    let mut fields = line.splitn(3, ' ');
    let method = fields.next().unwrap_or_default();
    let target = fields.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        log!("malformed request line, forwarding verbatim");
        return Ok(buf.to_vec());
    }
    let path = target.split('?').next().unwrap_or(target);

    // Rule 1: stamp the cluster label onto created containers.
    if method.eq_ignore_ascii_case("POST") && path_matches(path, "/containers/create") {
        return rewrite_create(client, &line, &headers, rest, cluster);
    }

    // Rule 2: constrain listings and the event stream to this cluster.
    if method.eq_ignore_ascii_case("GET")
        && (path_matches(path, "/containers/json") || path_matches(path, "/events"))
    {
        let label = format!("{}={}", LABEL_KEY, cluster);
        let new_target = merge_label_filter(target, &label);
        let version = fields.next().unwrap_or("HTTP/1.1");
        let new_line = format!("{} {} {}", method, new_target, version);
        let mut out = render_head(&new_line, &headers, None);
        out.extend_from_slice(rest);
        return Ok(out);
    }

    // Rule 3: everything else goes through untouched (bar `Connection: close`).
    let mut out = render_head(&line, &headers, None);
    out.extend_from_slice(rest);
    Ok(out)
}

/// `POST /containers/create`: buffer the body, add the cluster label, re-emit.
fn rewrite_create(
    client: &mut UnixStream,
    line: &str,
    headers: &[String],
    rest: &[u8],
    cluster: &str,
) -> io::Result<Vec<u8>> {
    let verbatim = |reason: &str| {
        log!("{}: forwarding container create unmodified", reason);
        let mut out = render_head(line, headers, None);
        out.extend_from_slice(rest);
        out
    };

    if header_value(headers, "transfer-encoding").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("chunked"))
    }) {
        return Ok(verbatim("chunked request body"));
    }

    let len = match header_value(headers, "content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(l) => l,
        None => return Ok(verbatim("container create without Content-Length")),
    };
    if len > MAX_REWRITE_BODY_BYTES {
        return Ok(verbatim("container create body too large"));
    }

    let (body, leftover) = read_body(client, rest, len)?;

    let mut out = match insert_label(&body, LABEL_KEY, cluster) {
        Some(new_body) => {
            let mut out = render_head(line, headers, Some(new_body.len()));
            out.extend_from_slice(&new_body);
            out
        }
        None => {
            log!("could not parse container create body, forwarding unmodified");
            let mut out = render_head(line, headers, None);
            out.extend_from_slice(&body);
            out
        }
    };
    out.extend_from_slice(&leftover);
    Ok(out)
}

/// Read exactly `len` body bytes, starting from what is already buffered.
/// Returns the body and any trailing bytes read past it.
fn read_body(stream: &mut UnixStream, prefix: &[u8], len: usize) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let taken = prefix.len().min(len);
    let mut body = Vec::with_capacity(len);
    body.extend_from_slice(&prefix[..taken]);
    let mut leftover = prefix[taken..].to_vec();

    let mut chunk = [0u8; 8 * 1024];
    while body.len() < len {
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "connection closed before end of request body",
                ))
            }
            Ok(n) => {
                let need = len - body.len();
                let used = n.min(need);
                body.extend_from_slice(&chunk[..used]);
                leftover.extend_from_slice(&chunk[used..n]);
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    Ok((body, leftover))
}

/// Copy bytes both ways until both directions are done. This is what carries
/// hijacked (exec/attach) and streaming (/events, log follow) connections.
fn splice(client: UnixStream, upstream: UnixStream) -> io::Result<()> {
    let client_read = client.try_clone()?;
    let client_write = client.try_clone()?;
    let upstream_write = upstream.try_clone()?;

    let forward = std::thread::Builder::new()
        .spawn(move || pump(client_read, upstream_write))
        .ok();

    pump(upstream, client_write);

    // The forward direction may still be blocked reading from a client that
    // has nothing left to say; upstream is gone, so tear the socket down.
    let _ = client.shutdown(Shutdown::Both);
    if let Some(handle) = forward {
        let _ = handle.join();
    }
    Ok(())
}

fn pump(mut from: UnixStream, mut to: UnixStream) {
    let mut buf = [0u8; 32 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = to.shutdown(Shutdown::Write);
    let _ = from.shutdown(Shutdown::Read);
}

// --- HTTP head handling ---

/// Split a request head into its request line and raw header lines.
fn parse_head(head: &str) -> Option<(String, Vec<String>)> {
    let mut lines = head.split("\r\n");
    let line = lines.next()?.trim_end().to_string();
    if line.is_empty() {
        return None;
    }
    let headers = lines
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    Some((line, headers))
}

fn header_name(line: &str) -> &str {
    line.split(':').next().unwrap_or("").trim()
}

fn header_value<'a>(headers: &'a [String], name: &str) -> Option<&'a str> {
    headers.iter().find_map(|h| {
        let (n, v) = h.split_once(':')?;
        if n.trim().eq_ignore_ascii_case(name) {
            Some(v.trim())
        } else {
            None
        }
    })
}

/// Re-emit the request head, always forcing `Connection: close` so each client
/// connection carries exactly one request. `content_length` replaces any
/// existing `Content-Length` when the body was rewritten.
fn render_head(line: &str, headers: &[String], content_length: Option<usize>) -> Vec<u8> {
    let mut out = String::with_capacity(line.len() + headers.len() * 32 + 32);
    out.push_str(line);
    out.push_str("\r\n");

    for header in headers {
        let name = header_name(header);
        if name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("keep-alive") {
            continue;
        }
        if content_length.is_some() && name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.push_str(header);
        out.push_str("\r\n");
    }

    if let Some(len) = content_length {
        out.push_str("Content-Length: ");
        out.push_str(&len.to_string());
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

/// Match a Docker API path against an endpoint, allowing the optional API
/// version prefix: `/v1.51/containers/json` matches `/containers/json`,
/// `/containers/jsonx` does not.
fn path_matches(path: &str, endpoint: &str) -> bool {
    if path == endpoint {
        return true;
    }
    let Some(rest) = path.strip_prefix("/v") else {
        return false;
    };
    let Some(slash) = rest.find('/') else {
        return false;
    };
    let version = &rest[..slash];
    !version.is_empty()
        && version.chars().all(|c| c.is_ascii_digit() || c == '.')
        && &rest[slash..] == endpoint
}

// --- Query string / filters ---

/// Merge `label=<label>` into the `filters` query parameter of `target`,
/// creating the parameter (and the query string) when absent. On any parse
/// failure the target is returned unchanged.
fn merge_label_filter(target: &str, label: &str) -> String {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    let mut pairs: Vec<(String, Option<String>)> = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        match part.split_once('=') {
            Some((k, v)) => pairs.push((k.to_string(), Some(v.to_string()))),
            None => pairs.push((part.to_string(), None)),
        }
    }

    let existing = pairs.iter().position(|(k, _)| k == "filters");
    let mut filters = match existing {
        Some(i) => {
            let raw = percent_decode(pairs[i].1.as_deref().unwrap_or(""));
            if raw.trim().is_empty() {
                Vec::new()
            } else {
                match parse_json(raw.as_bytes()) {
                    Some(Json::Obj(fields)) => fields,
                    _ => {
                        log!("could not parse filters parameter, leaving request unfiltered");
                        return target.to_string();
                    }
                }
            }
        }
        None => Vec::new(),
    };

    add_label(&mut filters, label);
    let encoded = percent_encode(&serialize(&Json::Obj(filters)));

    match existing {
        Some(i) => pairs[i].1 = Some(encoded),
        None => pairs.push(("filters".to_string(), Some(encoded))),
    }

    let rebuilt = pairs
        .iter()
        .map(|(k, v)| match v {
            Some(v) => format!("{}={}", k, v),
            None => k.clone(),
        })
        .collect::<Vec<_>>()
        .join("&");

    format!("{}?{}", path, rebuilt)
}

/// Add `label` to a filters object. Docker accepts both the array form
/// (`{"label":["a=b"]}`) and the legacy map form (`{"label":{"a=b":true}}`).
fn add_label(filters: &mut Vec<(String, Json)>, label: &str) {
    match filters.iter_mut().find(|(k, _)| k == "label") {
        Some((_, Json::Arr(items))) => {
            if !items
                .iter()
                .any(|i| matches!(i, Json::Str(s) if s == label))
            {
                items.push(Json::Str(label.to_string()));
            }
        }
        Some((_, Json::Obj(map))) => {
            if !map.iter().any(|(k, _)| k == label) {
                map.push((label.to_string(), Json::Bool(true)));
            }
        }
        Some((_, slot)) => *slot = Json::Arr(vec![Json::Str(label.to_string())]),
        None => filters.push((
            "label".to_string(),
            Json::Arr(vec![Json::Str(label.to_string())]),
        )),
    }
}

/// Percent-encode everything outside the unreserved set.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// Decode a query component. `+` means space, matching Go's `url.ParseQuery`,
/// which is what dockerd itself uses to read this value.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'A' + v - 10) as char,
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

// --- Minimal JSON ---

/// Just enough JSON for two jobs: adding a label to a container-create body and
/// reading/writing the `filters` query parameter. `Num` keeps its original text
/// so numbers round-trip exactly, and `Obj` is an ordered `Vec` so key order is
/// preserved - the body we forward differs from the one we received in exactly
/// one place.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Add `key: value` to the top-level `Labels` object, creating `Labels` when
/// absent. Returns `None` if the body is not a JSON object.
fn insert_label(body: &[u8], key: &str, value: &str) -> Option<Vec<u8>> {
    let mut json = parse_json(body)?;
    let Json::Obj(fields) = &mut json else {
        return None;
    };

    let idx = match fields.iter().position(|(k, _)| k == "Labels") {
        Some(i) => i,
        None => {
            fields.push(("Labels".to_string(), Json::Obj(Vec::new())));
            fields.len() - 1
        }
    };

    // `"Labels": null` is what Docker clients send when there are none.
    let slot = &mut fields[idx].1;
    if !matches!(slot, Json::Obj(_)) {
        *slot = Json::Obj(Vec::new());
    }
    if let Json::Obj(labels) = slot {
        match labels.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = Json::Str(value.to_string()),
            None => labels.push((key.to_string(), Json::Str(value.to_string()))),
        }
    }

    Some(serialize(&json).into_bytes())
}

fn parse_json(input: &[u8]) -> Option<Json> {
    let mut p = Parser { b: input, i: 0 };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.i == p.b.len() {
        Some(value)
    } else {
        None
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Some(())
        } else {
            None
        }
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        if self.b.len() >= self.i + word.len() && &self.b[self.i..self.i + word.len()] == word {
            self.i += word.len();
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self) -> Option<Json> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Some(Json::Str(self.string()?)),
            b't' => self.literal(b"true").map(|_| Json::Bool(true)),
            b'f' => self.literal(b"false").map(|_| Json::Bool(false)),
            b'n' => self.literal(b"null").map(|_| Json::Null),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.eat(b'{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.eat(b'}').is_some() {
            return Some(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            let value = self.value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(Json::Obj(fields));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(b']').is_some() {
            return Some(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(Json::Arr(items));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let esc = self.peek()?;
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return None,
                    }
                }
                // Multi-byte UTF-8 passes through as raw bytes; collect them and
                // decode at the end of the run.
                _ => {
                    let start = self.i - 1;
                    while !matches!(self.peek(), None | Some(b'"') | Some(b'\\')) {
                        self.i += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.b[start..self.i]).ok()?);
                }
            }
        }
    }

    /// `\uXXXX`, joining surrogate pairs when both halves are present.
    fn unicode_escape(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if (0xd800..0xdc00).contains(&hi) {
            let save = self.i;
            if self.eat(b'\\').is_some() && self.eat(b'u').is_some() {
                if let Some(lo) = self.hex4() {
                    if (0xdc00..0xe000).contains(&lo) {
                        let cp = 0x10000 + ((hi as u32 - 0xd800) << 10) + (lo as u32 - 0xdc00);
                        return char::from_u32(cp);
                    }
                }
            }
            self.i = save;
        }
        Some(char::from_u32(hi as u32).unwrap_or('\u{fffd}'))
    }

    fn hex4(&mut self) -> Option<u16> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = hex_value(self.peek()?)?;
            self.i += 1;
            v = (v << 4) | d as u16;
        }
        Some(v)
    }

    /// Numbers keep their original text, so they re-serialize byte for byte.
    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        if matches!(self.peek(), Some(b'-') | Some(b'+')) {
            self.i += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9') | Some(b'.') | Some(b'e') | Some(b'E') | Some(b'-') | Some(b'+')
        ) {
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        text.parse::<f64>().ok()?;
        Some(Json::Num(text.to_string()))
    }
}

fn serialize(value: &Json) -> String {
    let mut out = String::new();
    write_json(&mut out, value);
    out
}

fn write_json(out: &mut String, value: &Json) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(text) => out.push_str(text),
        Json::Str(s) => write_string(out, s),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Json::Obj(fields) => {
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_json(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                for shift in [12, 8, 4, 0] {
                    out.push(hex_digit((((c as u32) >> shift) & 0xf) as u8));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    // --- Argument parsing ---

    #[test]
    fn parses_all_three_arguments() {
        let args = vec![
            s("--listen"),
            s("/a.sock"),
            s("--upstream"),
            s("/b.sock"),
            s("--cluster"),
            s("alpha"),
        ];
        let cfg = parse_args(&args).expect("valid arguments");
        assert_eq!(cfg.listen, "/a.sock");
        assert_eq!(cfg.upstream, "/b.sock");
        assert_eq!(cfg.cluster, "alpha");
    }

    #[test]
    fn rejects_incomplete_or_unknown_arguments() {
        assert!(parse_args(&[s("--listen"), s("/a.sock")]).is_none());
        assert!(parse_args(&[s("--listen")]).is_none());
        assert!(parse_args(&[
            s("--listen"),
            s("/a.sock"),
            s("--upstream"),
            s("/b.sock"),
            s("--cluster"),
            s("")
        ])
        .is_none());
        assert!(parse_args(&[s("--bogus"), s("x")]).is_none());
        assert!(parse_args(&[]).is_none());
    }

    // --- Label insertion ---

    #[test]
    fn inserts_label_into_existing_labels() {
        let body = br#"{"Image":"nginx","Labels":{"a":"b"},"Tty":false}"#;
        let out = insert_label(body, "k3dev.cluster", "alpha").expect("rewritten");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"Image":"nginx","Labels":{"a":"b","k3dev.cluster":"alpha"},"Tty":false}"#
        );
    }

    #[test]
    fn inserts_label_when_labels_absent() {
        let body = br#"{"Image":"nginx"}"#;
        let out = insert_label(body, "k3dev.cluster", "alpha").expect("rewritten");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"Image":"nginx","Labels":{"k3dev.cluster":"alpha"}}"#
        );
    }

    #[test]
    fn replaces_null_labels_and_overwrites_existing_key() {
        let out = insert_label(br#"{"Labels":null}"#, "k3dev.cluster", "alpha").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"Labels":{"k3dev.cluster":"alpha"}}"#
        );

        let out = insert_label(
            br#"{"Labels":{"k3dev.cluster":"old"}}"#,
            "k3dev.cluster",
            "new",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"Labels":{"k3dev.cluster":"new"}}"#
        );
    }

    #[test]
    fn refuses_bodies_that_are_not_objects() {
        assert!(insert_label(b"[1,2]", "k", "v").is_none());
        assert!(insert_label(b"not json", "k", "v").is_none());
        assert!(insert_label(br#"{"a":1"#, "k", "v").is_none());
        assert!(insert_label(br#"{"a":1} trailing"#, "k", "v").is_none());
    }

    // --- JSON round trip ---

    #[test]
    fn round_trips_order_nesting_arrays_escapes_and_numbers() {
        // `\/` decodes to `/` and is re-emitted unescaped; everything else is
        // byte-for-byte identical, key order included.
        let src = r#"{"z":1,"a":{"nested":[1,-2.5,1e10,0.0,true,false,null]},"b":"","s":"q\"b\\s\/n\nt\tr\r\u00e9"}"#;
        let parsed = parse_json(src.as_bytes()).expect("parses");
        let out = serialize(&parsed);
        assert_eq!(
            out,
            "{\"z\":1,\"a\":{\"nested\":[1,-2.5,1e10,0.0,true,false,null]},\"b\":\"\",\"s\":\"q\\\"b\\\\s/n\\nt\\tr\\r\u{e9}\"}"
        );
        assert_eq!(parse_json(out.as_bytes()).as_ref(), Some(&parsed));
    }

    #[test]
    fn round_trips_surrogate_pairs_and_control_characters() {
        let parsed = parse_json(br#"{"e":"\ud83d\ude00","c":"\u0001"}"#).expect("parses");
        assert_eq!(
            serialize(&parsed),
            "{\"e\":\"\u{1f600}\",\"c\":\"\\u0001\"}"
        );
    }

    #[test]
    fn preserves_number_text_exactly() {
        let parsed = parse_json(br#"[1.50,1e+10,-0,3000000000000000000000]"#).unwrap();
        assert_eq!(serialize(&parsed), "[1.50,1e+10,-0,3000000000000000000000]");
    }

    // --- Filters merging ---

    #[test]
    fn merges_into_existing_label_array() {
        let filters = percent_encode(r#"{"label":["io.kubernetes.docker.type=container"]}"#);
        let target = format!("/v1.51/containers/json?all=1&filters={}", filters);
        let out = merge_label_filter(&target, "k3dev.cluster=alpha");

        let (path, query) = out.split_once('?').unwrap();
        assert_eq!(path, "/v1.51/containers/json");
        let value = query
            .split('&')
            .find_map(|p| p.strip_prefix("filters="))
            .expect("filters present");
        assert!(query.starts_with("all=1&"));
        assert_eq!(
            percent_decode(value),
            r#"{"label":["io.kubernetes.docker.type=container","k3dev.cluster=alpha"]}"#
        );
    }

    #[test]
    fn merges_into_filters_without_label_key() {
        let filters = percent_encode(r#"{"status":["running"]}"#);
        let target = format!("/containers/json?filters={}", filters);
        let out = merge_label_filter(&target, "k3dev.cluster=alpha");
        let value = out.split_once("filters=").unwrap().1;
        assert_eq!(
            percent_decode(value),
            r#"{"status":["running"],"label":["k3dev.cluster=alpha"]}"#
        );
    }

    #[test]
    fn creates_filters_when_target_has_no_query() {
        let out = merge_label_filter("/events", "k3dev.cluster=alpha");
        let value = out.strip_prefix("/events?filters=").expect("filters added");
        assert_eq!(
            percent_decode(value),
            r#"{"label":["k3dev.cluster=alpha"]}"#
        );
    }

    #[test]
    fn creates_filters_alongside_other_query_parameters() {
        let out = merge_label_filter("/containers/json?all=1&limit=5", "k3dev.cluster=alpha");
        assert!(out.starts_with("/containers/json?all=1&limit=5&filters="));
    }

    #[test]
    fn handles_legacy_map_filters_and_duplicate_labels() {
        let filters = percent_encode(r#"{"label":{"a=b":true}}"#);
        let out = merge_label_filter(
            &format!("/containers/json?filters={}", filters),
            "k3dev.cluster=alpha",
        );
        let value = out.split_once("filters=").unwrap().1;
        assert_eq!(
            percent_decode(value),
            r#"{"label":{"a=b":true,"k3dev.cluster=alpha":true}}"#
        );

        let filters = percent_encode(r#"{"label":["k3dev.cluster=alpha"]}"#);
        let target = format!("/containers/json?filters={}", filters);
        let out = merge_label_filter(&target, "k3dev.cluster=alpha");
        assert_eq!(out, target);
    }

    #[test]
    fn leaves_target_untouched_when_filters_do_not_parse() {
        let target = "/containers/json?filters=%7Bbroken";
        assert_eq!(merge_label_filter(target, "k3dev.cluster=alpha"), target);
    }

    // --- Percent coding ---

    #[test]
    fn percent_codes_round_trip() {
        let raw = r#"{"label":["a=b c/é"]}"#;
        let encoded = percent_encode(raw);
        assert!(!encoded.contains('{'));
        assert_eq!(percent_decode(&encoded), raw);
        // Go encodes a space as '+' in query values.
        assert_eq!(percent_decode("a+b"), "a b");
    }

    // --- Path matching ---

    #[test]
    fn matches_versioned_path_prefixes() {
        assert!(path_matches("/containers/json", "/containers/json"));
        assert!(path_matches("/v1.51/containers/json", "/containers/json"));
        assert!(path_matches("/v1.24/events", "/events"));
        assert!(!path_matches("/containers/jsonx", "/containers/json"));
        assert!(!path_matches("/v1.51/containers/jsonx", "/containers/json"));
        assert!(!path_matches("/vfoo/containers/json", "/containers/json"));
        assert!(!path_matches("/containers/json/extra", "/containers/json"));
        assert!(!path_matches(
            "/v1.51/containers/create",
            "/containers/json"
        ));
    }

    // --- Head rendering ---

    #[test]
    fn forces_connection_close_over_keep_alive() {
        let headers = vec![
            s("Host: docker"),
            s("Connection: keep-alive"),
            s("Keep-Alive: timeout=5"),
            s("User-Agent: cri-dockerd"),
        ];
        let out = render_head("GET /containers/json HTTP/1.1", &headers, None);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET /containers/json HTTP/1.1\r\nHost: docker\r\nUser-Agent: cri-dockerd\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn replaces_content_length_only_when_body_was_rewritten() {
        let headers = vec![s("Content-Length: 10"), s("Content-Type: application/json")];
        let kept = String::from_utf8(render_head("POST /x HTTP/1.1", &headers, None)).unwrap();
        assert!(kept.contains("Content-Length: 10\r\n"));

        let replaced =
            String::from_utf8(render_head("POST /x HTTP/1.1", &headers, Some(42))).unwrap();
        assert!(!replaced.contains("Content-Length: 10\r\n"));
        assert!(replaced.contains("Content-Length: 42\r\n"));
        assert!(replaced.contains("Content-Type: application/json\r\n"));
    }

    // --- Head parsing ---

    #[test]
    fn parses_head_and_header_values() {
        let (line, headers) =
            parse_head("POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Length: 7\r\n\r\n")
                .expect("parses");
        assert_eq!(line, "POST /containers/create HTTP/1.1");
        assert_eq!(headers.len(), 2);
        assert_eq!(header_value(&headers, "content-length"), Some("7"));
        assert_eq!(header_value(&headers, "CONTENT-LENGTH"), Some("7"));
        assert_eq!(header_value(&headers, "missing"), None);
    }

    #[test]
    fn reads_a_head_split_across_several_reads() {
        // The `\r\n\r\n` marker straddles two writes, and body bytes trail it.
        let (mut a, mut b) = UnixStream::pair().expect("socketpair");
        std::thread::spawn(move || {
            for part in [
                &b"GET /containers/json HTTP/1.1\r\nHost: d\r"[..],
                &b"\n\r"[..],
                &b"\nBODY"[..],
            ] {
                a.write_all(part).expect("write");
                a.flush().expect("flush");
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let (buf, head_end) = read_head(&mut b).expect("head");
        assert_eq!(
            std::str::from_utf8(&buf[..head_end]).unwrap(),
            "GET /containers/json HTTP/1.1\r\nHost: d\r\n\r\n"
        );
        assert_eq!(&buf[head_end..], b"BODY");
    }

    #[test]
    fn finds_marker_across_a_buffer() {
        assert_eq!(find(b"abc\r\n\r\nrest", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"abc", b"\r\n\r\n"), None);
    }
}
