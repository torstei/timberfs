//! Talking to incus over its unix socket: the small slice of the REST API
//! a console tap needs, and the websocket client it rides on.
//!
//! Hand-rolled for the same reason the other intakes are — one protocol,
//! one direction, no dependency. Three facts about incus shape everything
//! here, and each was measured rather than assumed:
//!
//! * `GET /1.0/instances/<x>/console` is a **destructive drain** of a
//!   128 KiB liblxc ring buffer, not a read. Reading it twice returns the
//!   backlog and then nothing; the buffer wraps mid-line and loses
//!   everything older in silence.
//! * `POST` on the same path opens a websocket that streams the console
//!   live. It is **exclusive** — and reserved by the POST itself, before
//!   any websocket attaches, so an abandoned attempt holds the console
//!   until its operation is cancelled.
//! * The two feeds are independent: a tap does not starve the ring, so
//!   the ring can be drained once after attaching to recover whatever the
//!   instance emitted before we arrived.
//!
//! Only containers have that ring. A VM's console is a file, and reading
//! it is idempotent — which is why the drain below is not used for one.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;

/// Where incus listens by default.
pub const DEFAULT_SOCKET: &str = "/var/lib/incus/unix.socket";

/// The magic RFC 6455 appends to a client key before hashing.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// An HTTP response from the socket: everything a caller here needs.
struct Response {
    status: u16,
    body: Vec<u8>,
}

fn connect(socket: &str) -> anyhow::Result<UnixStream> {
    let s =
        UnixStream::connect(socket).with_context(|| format!("connecting to incus at {socket}"))?;
    // A hung daemon must not hang the tap forever; the read timeout is
    // reset for the websocket, which is legitimately idle for long
    // stretches.
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(30)))?;
    Ok(s)
}

/// Read at least `want` bytes into `buf`. False when the peer closed
/// first, which every caller has to distinguish from "not yet".
fn fill_to(s: &mut UnixStream, buf: &mut Vec<u8>, want: usize, cap: usize) -> anyhow::Result<bool> {
    let mut chunk = [0u8; 16 << 10];
    while buf.len() < want {
        if buf.len() > cap {
            bail!("incus response exceeds {cap} bytes");
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(true)
}

/// Read until `needle` appears, returning the offset just past it.
fn fill_until(
    s: &mut UnixStream,
    buf: &mut Vec<u8>,
    needle: &[u8],
    cap: usize,
) -> anyhow::Result<usize> {
    loop {
        if let Some(i) = find(buf, needle) {
            return Ok(i + needle.len());
        }
        let want = buf.len() + 1;
        if !fill_to(s, buf, want, cap)? {
            bail!("incus closed the connection mid-response");
        }
    }
}

/// Read a whole HTTP/1.1 response. All three framings appear on this
/// socket — `Content-Length` on most replies, `chunked` on the recursive
/// listings, and close-delimited on the console drain — so all three are
/// handled rather than the one that happened to be tried first.
fn read_response(s: &mut UnixStream, cap: usize) -> anyhow::Result<Response> {
    let mut buf = Vec::new();
    let head_end = fill_until(s, &mut buf, b"\r\n\r\n", cap)?;
    let mut rest = buf.split_off(head_end);
    let head = String::from_utf8_lossy(&buf).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("no HTTP status in incus response"))?;
    let lower = head.to_ascii_lowercase();

    if let Some(len) = header_value(&lower, "content-length").and_then(|v| v.parse::<usize>().ok())
    {
        fill_to(s, &mut rest, len, cap)?;
        rest.truncate(len);
        return Ok(Response { status, body: rest });
    }

    if lower.contains("transfer-encoding: chunked") {
        let mut body = Vec::new();
        loop {
            let line_end = fill_until(s, &mut rest, b"\r\n", cap)?;
            let size_text = String::from_utf8_lossy(&rest[..line_end - 2]).to_string();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
                .with_context(|| format!("bad chunk size {size_text:?} from incus"))?;
            // The terminating 0-chunk, then trailers we do not read: the
            // connection is closed after this response either way.
            if size == 0 {
                break;
            }
            if !fill_to(s, &mut rest, line_end + size + 2, cap)? {
                bail!("incus truncated a chunked response");
            }
            body.extend_from_slice(&rest[line_end..line_end + size]);
            rest.drain(..line_end + size + 2);
            if body.len() > cap {
                bail!("incus response exceeds {cap} bytes");
            }
        }
        return Ok(Response { status, body });
    }

    // Close-delimited: read to EOF.
    let mut chunk = [0u8; 16 << 10];
    while let Ok(n) = s.read(&mut chunk) {
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&chunk[..n]);
        if rest.len() > cap {
            bail!("incus response exceeds {cap} bytes");
        }
    }
    Ok(Response { status, body: rest })
}

fn header_value<'a>(lower_head: &'a str, name: &str) -> Option<&'a str> {
    lower_head
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .map(str::trim)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A read-only client for the incus REST API over its unix socket.
pub struct Incus {
    socket: String,
    project: String,
}

impl Incus {
    pub fn new(socket: &str, project: &str) -> Incus {
        Incus {
            socket: socket.to_string(),
            project: project.to_string(),
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Response> {
        let mut s = connect(&self.socket)?;
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let sep = if path.contains('?') { '&' } else { '?' };
        let req = format!(
            "{method} {path}{sep}project={}  HTTP/1.1\r\nHost: incus\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            self.project,
            payload.len()
        )
        .replace("  HTTP/1.1", " HTTP/1.1");
        s.write_all(req.as_bytes())?;
        s.write_all(payload.as_bytes())?;
        s.flush()?;
        read_response(&mut s, 64 << 20)
    }

    /// A sync API call, returning the `metadata` incus wraps every reply
    /// in. An error reply carries a human reason; surface it rather than
    /// the status code alone.
    fn sync(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let r = self.request(method, path, body)?;
        let v: Value = serde_json::from_slice(&r.body)
            .with_context(|| format!("{method} {path}: incus sent no JSON (HTTP {})", r.status))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            if !err.is_empty() {
                bail!("{method} {path}: {err}");
            }
        }
        Ok(v.get("metadata").cloned().unwrap_or(Value::Null))
    }

    /// Every instance in the project, with the fields a tap labels a store
    /// from. One recursive call rather than one per instance.
    pub fn instances(&self) -> anyhow::Result<Vec<Instance>> {
        let md = self.sync("GET", "/1.0/instances?recursion=1", None)?;
        Ok(md
            .as_array()
            .map(|a| a.iter().map(Instance::from_json).collect())
            .unwrap_or_default())
    }

    pub fn instance(&self, name: &str) -> anyhow::Result<Instance> {
        let md = self.sync("GET", &format!("/1.0/instances/{name}"), None)?;
        if md.is_null() {
            bail!("no instance {name}");
        }
        Ok(Instance::from_json(&md))
    }

    /// The server's own name, which is what `host` means for a store whose
    /// entries came from a container: the machine the container runs on,
    /// not the container's idea of its hostname.
    pub fn server_name(&self) -> anyhow::Result<String> {
        let md = self.sync("GET", "/1.0", None)?;
        Ok(md
            .get("environment")
            .and_then(|e| e.get("server_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Reserve an instance's console and return what is needed to attach:
    /// the operation to cancel afterwards, and the data websocket's secret.
    ///
    /// ⚠ The console is EXCLUSIVE, and this POST is what takes it — before
    /// any websocket connects. An attempt abandoned here holds the console
    /// against everyone (including a human running `incus console`) until
    /// its operation is cancelled, so a caller that does not go on to
    /// attach must call `cancel_operation`.
    pub fn console_attach(&self, name: &str) -> anyhow::Result<Console> {
        let body = serde_json::json!({"width": 80, "height": 24, "type": "console"});
        let md = self.sync(
            "POST",
            &format!("/1.0/instances/{name}/console"),
            Some(&body),
        )?;
        let id = md
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("console attach to {name} returned no operation"))?
            .to_string();
        let secret = md
            .get("metadata")
            .and_then(|m| m.get("fds"))
            .and_then(|f| f.get("0"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("console attach to {name} returned no data fd"))?
            .to_string();
        Ok(Console { id, secret })
    }

    /// Release a console reservation. Called on every exit path that took
    /// one, because incus holds it until the operation ends.
    pub fn cancel_operation(&self, id: &str) -> anyhow::Result<()> {
        self.sync("DELETE", &format!("/1.0/operations/{id}"), None)?;
        Ok(())
    }

    /// Attach to the data websocket of a reserved console.
    pub fn console_stream(&self, c: &Console) -> anyhow::Result<WebSocket> {
        WebSocket::connect(
            &self.socket,
            &format!("/1.0/operations/{}/websocket?secret={}", c.id, c.secret),
        )
    }

    /// The lifecycle event stream: what tells a supervisor an instance
    /// started, restarted or went away.
    pub fn events(&self) -> anyhow::Result<WebSocket> {
        WebSocket::connect(
            &self.socket,
            &format!("/1.0/events?type=lifecycle&project={}", self.project),
        )
    }
}

/// A reserved console: the operation holding it, and the secret its data
/// websocket is opened with.
pub struct Console {
    pub id: String,
    pub secret: String,
}

impl Incus {
    /// Drain the console ring buffer. ⚠ DESTRUCTIVE: what this returns is
    /// gone from the ring, so anyone else reading it — a human running
    /// `incus console --show-log` — gets what is left, not what was there.
    /// Exactly-once and in order, which is what makes it safe to stitch in
    /// front of a live tap.
    pub fn console_drain(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        let r = self.request("GET", &format!("/1.0/instances/{name}/console"), None)?;
        if r.status != 200 {
            bail!("draining {name}'s console ring: HTTP {}", r.status);
        }
        Ok(r.body)
    }
}

/// The parts of an instance a console tap cares about.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub project: String,
    /// `container` or `virtual-machine`.
    pub kind: String,
    pub status: String,
    /// The cluster member running it, or empty when unclustered.
    pub location: String,
    /// `image.id`, e.g. `visena-gateway:0.0.2-LOCAL`. Episodic — it
    /// changes when the instance is rebuilt, so it belongs in the log's
    /// timeline rather than in the store's labels.
    pub image: String,
    pub base_image: String,
    pub entrypoint: String,
    /// `user.*` keys, which is where an operator already puts labels.
    pub user_keys: Vec<(String, String)>,
}

impl Instance {
    fn from_json(v: &Value) -> Instance {
        let cfg = |k: &str| -> String {
            v.get("config")
                .and_then(|c| c.get(k))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string()
        };
        let str_at =
            |k: &str| -> String { v.get(k).and_then(|s| s.as_str()).unwrap_or("").to_string() };
        let user_keys = v
            .get("expanded_config")
            .or_else(|| v.get("config"))
            .and_then(|c| c.as_object())
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| k.starts_with("user."))
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Instance {
            name: str_at("name"),
            project: str_at("project"),
            kind: str_at("type"),
            status: str_at("status"),
            // Unclustered incus reports the string "none" rather than an
            // absent field, and "none" is not a hostname.
            location: match str_at("location").as_str() {
                "none" => String::new(),
                other => other.to_string(),
            },
            image: cfg("image.id"),
            base_image: cfg("volatile.base_image"),
            entrypoint: cfg("oci.entrypoint"),
            user_keys,
        }
    }

    pub fn is_container(&self) -> bool {
        self.kind == "container"
    }

    pub fn is_running(&self) -> bool {
        self.status == "Running"
    }
}

// ---------------------------------------------------------------- websocket

/// A client websocket over the incus socket. Text and binary frames come
/// back as bytes; pings are answered and control frames handled here, so a
/// caller only ever sees payload.
pub struct WebSocket {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl WebSocket {
    /// Open `path` as a websocket. The handshake is verified, including
    /// `Sec-WebSocket-Accept`: the peer is incusd over a unix socket so
    /// there is no one to impersonate it, but a server that answers 101
    /// without being a websocket would otherwise be read as frames.
    pub fn connect(socket: &str, path: &str) -> anyhow::Result<WebSocket> {
        let mut s = connect(socket)?;
        let key = base64(&random_bytes::<16>()?);
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: incus\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        s.write_all(req.as_bytes())?;
        s.flush()?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i + 4;
            }
            let n = s.read(&mut chunk)?;
            if n == 0 {
                bail!("incus closed the connection during the websocket handshake");
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 64 << 10 {
                bail!("websocket handshake headers are implausibly large");
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        if !head.starts_with("HTTP/1.1 101") {
            let first = head.lines().next().unwrap_or("").to_string();
            bail!("websocket upgrade refused: {first}");
        }
        let lower = head.to_ascii_lowercase();
        let want = base64(&sha1(format!("{key}{WS_GUID}").as_bytes()));
        match header_value(&lower, "sec-websocket-accept") {
            Some(got) if got.eq_ignore_ascii_case(&want) => {}
            Some(got) => bail!("websocket accept mismatch: {got} is not {want}"),
            None => bail!("websocket upgrade had no Sec-WebSocket-Accept"),
        }
        // A console can be legitimately silent for hours.
        s.set_read_timeout(None)?;
        Ok(WebSocket {
            stream: s,
            buf: buf[head_end..].to_vec(),
        })
    }

    fn fill(&mut self, want: usize) -> anyhow::Result<bool> {
        let mut chunk = [0u8; 32 << 10];
        while self.buf.len() < want {
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Ok(false);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
        Ok(true)
    }

    /// The next payload, or None when the peer closed. Control frames are
    /// handled here: a ping is ponged (a console left unanswered is a
    /// console incus eventually drops), a close ends the stream.
    pub fn read(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            if !self.fill(2)? {
                return Ok(None);
            }
            let b0 = self.buf[0];
            let b1 = self.buf[1];
            let opcode = b0 & 0x0f;
            let masked = b1 & 0x80 != 0;
            let mut len = (b1 & 0x7f) as usize;
            let mut off = 2;
            if len == 126 {
                if !self.fill(4)? {
                    return Ok(None);
                }
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                off = 4;
            } else if len == 127 {
                if !self.fill(10)? {
                    return Ok(None);
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[2..10]);
                len = u64::from_be_bytes(b) as usize;
                off = 10;
            }
            let mask: Option<[u8; 4]> = if masked {
                if !self.fill(off + 4)? {
                    return Ok(None);
                }
                let m = [
                    self.buf[off],
                    self.buf[off + 1],
                    self.buf[off + 2],
                    self.buf[off + 3],
                ];
                off += 4;
                Some(m)
            } else {
                None
            };
            if len > 64 << 20 {
                bail!("websocket frame of {len} bytes is implausible for a console");
            }
            if !self.fill(off + len)? {
                return Ok(None);
            }
            let mut payload = self.buf[off..off + len].to_vec();
            self.buf.drain(..off + len);
            if let Some(m) = mask {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= m[i % 4];
                }
            }
            match opcode {
                // Continuation frames carry console bytes just as well as
                // a first frame: this is a byte stream, not messages, so
                // there is nothing to reassemble.
                0x0..=0x2 => return Ok(Some(payload)),
                0x8 => return Ok(None),
                0x9 => self.send(0xa, &payload)?,
                0xa => {}
                other => bail!("unknown websocket opcode {other:#x} from incus"),
            }
        }
    }

    /// Send a frame. Client frames must be masked, per RFC 6455 — a server
    /// is entitled to drop the connection over an unmasked one.
    fn send(&mut self, opcode: u8, payload: &[u8]) -> anyhow::Result<()> {
        let mut out = vec![0x80 | opcode];
        let mask = random_bytes::<4>()?;
        match payload.len() {
            n if n < 126 => out.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(0x80 | 127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(())
    }
}

fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

fn base64(input: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// SHA-1, for the handshake and nothing else. Present so the client
/// verifies `Sec-WebSocket-Accept` rather than trusting a 101 — the RFC's
/// own test vectors pin it below.
fn sha1(msg: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut data = msg.to_vec();
    let bits = (msg.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bits.to_be_bytes());
    for block in data.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_published_vectors() {
        assert_eq!(
            sha1(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha1(b"")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        // Spans several blocks, which is where a padding or length bug
        // hides.
        assert_eq!(
            sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn base64_pads_like_the_rfc() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_handshake_accept_is_the_rfc_6455_example() {
        // RFC 6455 §1.3: this key must produce this accept, or a correct
        // server's answer would be rejected as a mismatch.
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(
            base64(&sha1(format!("{key}{WS_GUID}").as_bytes())),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn an_unclustered_location_is_not_a_hostname() {
        // Unclustered incus reports the string "none", which would
        // otherwise become a store's `host` label.
        let v: Value = serde_json::from_str(
            r#"{"name":"a","project":"default","type":"container",
                "status":"Running","location":"none","config":{}}"#,
        )
        .unwrap();
        assert_eq!(Instance::from_json(&v).location, "");
    }

    #[test]
    fn user_keys_are_the_operators_own_labels() {
        let v: Value = serde_json::from_str(
            r#"{"name":"a","expanded_config":{"user.datacentre":"osl1",
                "image.id":"x","user.team":"platform"}}"#,
        )
        .unwrap();
        let mut keys = Instance::from_json(&v).user_keys;
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ("user.datacentre".to_string(), "osl1".to_string()),
                ("user.team".to_string(), "platform".to_string())
            ]
        );
    }
    /// Against the real daemon, so the HTTP parsing is proved rather than
    /// reasoned about. Ignored by default: needs an incus on this host.
    ///   cargo test --release --lib incus:: -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_socket_answers() {
        let c = Incus::new(DEFAULT_SOCKET, "default");
        let server = c.server_name().unwrap();
        assert!(!server.is_empty(), "server_name was empty");
        let list = c.instances().unwrap();
        assert!(!list.is_empty(), "no instances");
        println!("server={server} instances={}", list.len());
        for i in &list {
            println!(
                "  {:16} {:16} {:10} image={:32} loc={:?}",
                i.name, i.kind, i.status, i.image, i.location
            );
        }
    }

    /// The console tap, end to end, against a real container. Ignored by
    /// default; drive it with an instance that prints to its console:
    ///   TIMBERFS_TEST_INSTANCE=x cargo test --release --lib incus:: \
    ///     -- --ignored --nocapture live_console_tap
    #[test]
    #[ignore]
    fn live_console_tap() {
        let Ok(name) = std::env::var("TIMBERFS_TEST_INSTANCE") else {
            println!("set TIMBERFS_TEST_INSTANCE to run this");
            return;
        };
        let c = Incus::new(DEFAULT_SOCKET, "default");
        let console = c.console_attach(&name).unwrap();
        println!("[operation {}]", console.id);
        let mut ws = c.console_stream(&console).unwrap();
        // The ring holds whatever was emitted before we attached; draining
        // AFTER attaching overlaps rather than gapping.
        let backlog = c.console_drain(&name).unwrap();
        println!("[backlog {} bytes]", backlog.len());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = 0usize;
        while std::time::Instant::now() < deadline {
            match ws.read() {
                Ok(Some(b)) => {
                    got += b.len();
                    print!("{}", String::from_utf8_lossy(&b).replace('\r', ""));
                }
                Ok(None) => {
                    println!("[closed]");
                    break;
                }
                Err(e) => {
                    println!("[error {e}]");
                    break;
                }
            }
        }
        c.cancel_operation(&console.id).unwrap();
        println!("[streamed {got} bytes, console released]");
        assert!(got > 0, "nothing streamed from {name}'s console");
    }
}
