//! PostgreSQL wire protocol: startup + authentication (FR-9, spec §5.1).
//!
//! Slice scope: protocol 3.0, simple-protocol startup packet, SSLRequest
//! declined ('N'), trust and SCRAM-SHA-256 auth. Authentication completes
//! before routing — the housekeeper runs this on the accepted connection and
//! only then hands the fd onward (T1.5).
//!
//! All reads are length-checked against the declared packet length before
//! `read_exact`, and every truncation/EOF mid-exchange maps to
//! [`HandshakeError::Closed`] — the peer vanished; close quietly.
//!
//! The handshake is also bounded in TIME (`handshake_timeout_ms`, PG's
//! `authentication_timeout` analog): a budget, when given, is enforced
//! as `SO_RCVTIMEO` on the socket (any single blocked read) plus a total
//! deadline checked at every read boundary (a byte-trickling peer).
//! Expiry maps to [`HandshakeError::TimedOut`] and the connection drops
//! with nothing sent — writing to a peer stalled this hard could itself
//! block the ring.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use crate::{
    config::{AuthConfig, AuthMode, AuthUser},
    scram::{AuthMessageInputs, ScramVerifier},
};

/// SQLSTATE codes used by the handshake.
pub mod sqlstate {
    pub const SUCCESSFUL_COMPLETION: &str = "00000";
    pub const INVALID_PASSWORD: &str = "28P01";
    pub const INVALID_AUTHORIZATION_SPECIFICATION: &str = "28000";
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
}

/// Largest startup packet we accept (params are small; libpq caps at 10 000).
const MAX_STARTUP_LEN: u32 = 10_000;
const PROTOCOL_3_0: u32 = 196_608;
const SSL_REQUEST: u32 = 80_877_103;
const CANCEL_REQUEST: u32 = 80_877_102;

/// One user's parsed verifier, ready for the exchange.
#[derive(Debug, Clone)]
pub struct AuthedUser {
    pub name: String,
    pub verifier: ScramVerifier,
}

/// The authentication policy, built from config once (verifier parse errors
/// surface at build time, not mid-handshake).
#[derive(Debug, Clone)]
pub enum AuthPolicy {
    /// Dev mode: no credential check (FR-9 allows this in dev config).
    Trust,
    ScramSha256(HashMap<String, AuthedUser>),
}

impl AuthPolicy {
    pub fn from_config(cfg: &AuthConfig) -> Result<Self, String> {
        match cfg.mode {
            AuthMode::Trust => Ok(AuthPolicy::Trust),
            AuthMode::ScramSha256 => {
                let mut users = HashMap::new();
                for AuthUser { name, verifier } in &cfg.users {
                    let parsed = ScramVerifier::parse(verifier)
                        .map_err(|e| format!("user {name:?}: {e}"))?;
                    users.insert(
                        name.clone(),
                        AuthedUser {
                            name: name.clone(),
                            verifier: parsed,
                        },
                    );
                }
                Ok(AuthPolicy::ScramSha256(users))
            }
        }
    }
}

/// The parsed startup packet.
#[derive(Debug, Clone)]
pub struct Startup {
    pub params: HashMap<String, String>,
}

impl Startup {
    pub fn user(&self) -> Option<&str> {
        self.params.get("user").map(String::as_str)
    }

    pub fn database(&self) -> Option<&str> {
        self.params.get("database").map(String::as_str)
    }
}

/// Outcome of a successful handshake.
#[derive(Debug)]
pub struct Session {
    pub startup: Startup,
    pub backend_pid: i32,
    pub backend_secret: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The client asked to cancel (we never started the query) — close.
    #[error("cancel request")]
    Cancelled,
    /// Peer hung up or the socket died mid-exchange; nothing to report.
    #[error("connection closed mid-handshake")]
    Closed,
    /// The handshake budget ran out — a silent or byte-trickling peer.
    /// Nothing is sent; the client sees the connection drop.
    #[error("handshake deadline exceeded")]
    TimedOut,
    /// A real protocol/auth failure — reported to the client with its
    /// SQLSTATE code before closing.
    #[error("PG error {code}: {message}")]
    Rejected { code: &'static str, message: String },
    #[error("socket io: {0}")]
    Io(#[from] std::io::Error),
}

/// Read one backend-protocol message: type byte + length + payload.
fn read_message<S: Read>(stream: &mut S) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len < 4 || len > MAX_STARTUP_LEN as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message length out of range",
        ));
    }
    let mut payload = vec![0u8; len - 4];
    stream.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn write_message<W: Write>(stream: &mut W, msg_type: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(msg_type);
    buf.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf)
}

fn reject<W: Write>(stream: &mut W, code: &'static str, message: &str) -> HandshakeError {
    let _ = send_fatal(stream, code, message);
    HandshakeError::Rejected {
        code,
        message: message.to_owned(),
    }
}

/// Writes a FATAL `ErrorResponse` (severity S/V, code C, message M) and
/// leaves the stream open for the caller to close. Shared by handshake
/// rejections and the router's post-handshake refusals (FR-12): by then
/// the client is in the query protocol, and FATAL-then-close is how
/// PostgreSQL reports a connection-ending error there too.
pub(crate) fn send_fatal<W: Write>(
    stream: &mut W,
    code: &'static str,
    message: &str,
) -> std::io::Result<()> {
    let mut payload = Vec::new();
    payload.push(b'S');
    payload.extend_from_slice(b"FATAL\0");
    payload.push(b'V');
    payload.extend_from_slice(b"FATAL\0");
    payload.push(b'C');
    payload.extend_from_slice(code.as_bytes());
    payload.push(0);
    payload.push(b'M');
    payload.extend_from_slice(message.as_bytes());
    payload.push(0);
    payload.push(0);
    write_message(stream, b'E', &payload)
}

/// The handshake's time bound: `read` checks the TOTAL deadline before
/// each read — a byte-trickling peer cannot extend the exchange past it —
/// while the socket's `SO_RCVTIMEO` (set by [`handshake`]) bounds any
/// single blocked read, which the deadline check cannot see into. Writes
/// delegate untouched: rejection replies still reach a live peer.
struct Deadline<'a> {
    stream: &'a mut TcpStream,
    deadline: Option<Instant>,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "handshake deadline exceeded",
                ));
            }
        }
        self.stream.read(buf)
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// Read the startup packet (handling SSLRequest by declining), parse params,
/// and run authentication. On success the session is authenticated and the
/// stream is left after ReadyForQuery, ready for the query loop (T1.7).
///
/// `budget` bounds the whole exchange (see the module doc). The bound is
/// self-contained HERE on purpose: the socket-level timeout is set before
/// and cleared after the exchange, so no caller can forget the clear and
/// let the deadline leak into an idle session's reads.
pub fn handshake(
    stream: &mut TcpStream,
    policy: &AuthPolicy,
    budget: Option<Duration>,
) -> Result<Session, HandshakeError> {
    if let Some(b) = budget {
        // Unreachable-failure on a live socket (the serving loop already
        // .expects the listener's identical call); ignoring keeps this
        // path total-failure-shaped either way. The budget's upper bound
        // is config validation (one hour), so the addition below cannot
        // overflow `Instant`.
        let _ = crate::accept::set_stream_recv_timeout(stream, Some(b));
    }
    let result = {
        let mut wire = Deadline {
            stream,
            deadline: budget.map(|b| Instant::now() + b),
        };
        do_handshake(&mut wire, policy)
    };
    if result.is_ok() && budget.is_some() {
        let _ = crate::accept::set_stream_recv_timeout(stream, None);
    }
    result
}

fn do_handshake<S: Read + Write>(
    stream: &mut S,
    policy: &AuthPolicy,
) -> Result<Session, HandshakeError> {
    let startup = read_startup(stream)?;
    let session = authenticate(stream, policy, &startup)?;
    finish_session(stream, &session)?;
    Ok(session)
}

/// Read the startup-length packet. Returns `None` if the peer sent
/// CancelRequest (the caller closes without an error reply).
fn read_startup<S: Read + Write>(stream: &mut S) -> Result<Startup, HandshakeError> {
    // A hostile peer can ping-pong SSLRequest forever, pinning this thread;
    // a real client negotiates at most once. Two probes, then refuse.
    let mut ssl_probes = 0u32;
    loop {
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.kind() == std::io::ErrorKind::ConnectionReset =>
            {
                return Err(HandshakeError::Closed)
            }
            // WouldBlock/TimedOut = the handshake budget, via io_closed.
            Err(e) => return Err(io_closed(e)),
        }
        let len = u32::from_be_bytes(len_buf);
        if !(8..=MAX_STARTUP_LEN).contains(&len) {
            return Err(reject(
                stream,
                sqlstate::PROTOCOL_VIOLATION,
                "invalid startup packet length",
            ));
        }
        let mut body = vec![0u8; len as usize - 4];
        stream.read_exact(&mut body).map_err(io_closed)?;

        let code = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        match code {
            SSL_REQUEST => {
                ssl_probes += 1;
                if ssl_probes > 2 {
                    return Err(reject(
                        stream,
                        sqlstate::PROTOCOL_VIOLATION,
                        "repeated SSLRequest",
                    ));
                }
                // No TLS in the slice; libpq falls back to cleartext.
                stream.write_all(b"N")?;
                continue;
            }
            CANCEL_REQUEST => return Err(HandshakeError::Cancelled),
            PROTOCOL_3_0 => {
                let params = parse_params(&body[4..]).ok_or_else(|| {
                    reject(
                        stream,
                        sqlstate::PROTOCOL_VIOLATION,
                        "malformed startup parameters",
                    )
                })?;
                return Ok(Startup { params });
            }
            _ => {
                return Err(reject(
                    stream,
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    &format!(
                        "unsupported frontend protocol {}.{}: server supports 3.0",
                        code >> 16,
                        code & 0xffff
                    ),
                ));
            }
        }
    }
}

/// Params are cstring key/value pairs, terminated by an empty key. Anything
/// after the terminator (extended-protocol negotiation bytes) is a violation
/// for the simple-protocol slice.
fn parse_params(mut body: &[u8]) -> Option<HashMap<String, String>> {
    let mut params = HashMap::new();
    loop {
        let key_end = body.iter().position(|&b| b == 0)?;
        let key = std::str::from_utf8(&body[..key_end]).ok()?;
        body = &body[key_end + 1..];
        if key.is_empty() {
            return if body.is_empty() { Some(params) } else { None };
        }
        let val_end = body.iter().position(|&b| b == 0)?;
        let value = std::str::from_utf8(&body[..val_end]).ok()?;
        body = &body[val_end + 1..];
        params.insert(key.to_owned(), value.to_owned());
    }
}

fn authenticate<S: Read + Write>(
    stream: &mut S,
    policy: &AuthPolicy,
    startup: &Startup,
) -> Result<Session, HandshakeError> {
    let user = startup.user().ok_or_else(|| {
        reject(
            stream,
            sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
            "no PostgreSQL user name specified in startup packet",
        )
    })?;

    match policy {
        AuthPolicy::Trust => auth_ok(stream)?,
        AuthPolicy::ScramSha256(users) => {
            let Some(authed) = users.get(user) else {
                // Accepted deviation from real PG (which runs a mock exchange
                // to prevent user enumeration): the slice answers immediately.
                return Err(reject(
                    stream,
                    sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
                    &format!("role \"{user}\" does not exist"),
                ));
            };
            scram_exchange(stream, authed)?;
        }
    }

    let backend_pid = std::process::id() as i32;
    Ok(Session {
        startup: startup.clone(),
        backend_pid,
        backend_secret: rand_backend_secret(),
    })
}

fn rand_backend_secret() -> i32 {
    use rand::RngCore;
    let mut buf = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut buf);
    i32::from_be_bytes(buf)
}

/// SCRAM-SHA-256 exchange (RFC 5802 server side, PG framing):
/// AuthenticationSASL → client SASLInitialResponse → AuthenticationSASLContinue
/// (server-first) → client SASLResponse → verify → AuthenticationSASLFinal.
fn scram_exchange<S: Read + Write>(stream: &mut S, user: &AuthedUser) -> Result<(), HandshakeError> {
    // Advertise the one mechanism we implement.
    let mut mech = Vec::new();
    mech.extend_from_slice(b"SCRAM-SHA-256\0");
    mech.push(0);
    write_message(stream, b'R', &prefix(10, &mech))?;

    // 1. SASLInitialResponse: mechanism cstring + int32 initial-response len
    //    + bytes.
    let (t, payload) = read_message(stream).map_err(io_closed)?;
    if t != b'p' {
        return Err(reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "expected SASLInitialResponse",
        ));
    }
    let mech_end = payload.iter().position(|&b| b == 0).ok_or_else(|| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "unterminated mechanism name",
        )
    })?;
    let mechanism = std::str::from_utf8(&payload[..mech_end])
        .map_err(|_| reject(stream, sqlstate::PROTOCOL_VIOLATION, "mechanism not utf8"))?;
    if mechanism != "SCRAM-SHA-256" {
        return Err(reject(
            stream,
            sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
            &format!("unsupported SASL mechanism {mechanism:?}"),
        ));
    }
    let rest = &payload[mech_end + 1..];
    let initial = read_optional_payload(stream, rest)?;

    // 2. client-first-message: gs2 header ("n,," or "y,,"), then bare.
    let client_first = std::str::from_utf8(&initial).map_err(|_| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client-first not utf8",
        )
    })?;
    let (gs2_header, client_first_bare) = split_gs2_header(client_first, stream)?;

    let client_nonce = parse_attr(client_first_bare, 'r').ok_or_else(|| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client-first missing nonce",
        )
    })?;
    if parse_attr(client_first_bare, 'm').is_some() {
        return Err(reject(
            stream,
            sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
            "unsupported mandatory SCRAM extension",
        ));
    }

    // 3. server-first-message.
    let server_nonce = crate::scram::server_nonce();
    let combined_nonce = format!("{client_nonce}{server_nonce}");
    let server_first = format!(
        "r={},s={},i={}",
        combined_nonce,
        B64.encode(&user.verifier.salt),
        user.verifier.iters,
    );
    // AuthenticationSASLContinue body: Int32(11) + server-first-message.
    // No terminator — the message length delimits it.
    let mut cont = Vec::new();
    cont.extend_from_slice(&11i32.to_be_bytes());
    cont.extend_from_slice(server_first.as_bytes());
    write_message(stream, b'R', &cont)?;

    // 4. SASLResponse: client-final-message.
    let (t, final_payload) = read_message(stream).map_err(io_closed)?;
    if t != b'p' {
        return Err(reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "expected SASLResponse",
        ));
    }
    let client_final = std::str::from_utf8(&final_payload).map_err(|_| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client-final not utf8",
        )
    })?;

    // client-final-without-proof is everything before ",p=".
    let Some((client_final_without_proof, proof_b64)) = client_final.rsplit_once(",p=") else {
        return Err(reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client-final missing proof",
        ));
    };
    let channel_binding = parse_attr(client_final_without_proof, 'c').ok_or_else(|| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client-final missing channel binding",
        )
    })?;
    let expected_binding = B64.encode(gs2_header.as_bytes());
    if channel_binding != expected_binding {
        return Err(reject(
            stream,
            sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
            "SCRAM channel binding check failed",
        ));
    }
    let final_nonce = parse_attr(client_final_without_proof, 'r').ok_or_else(|| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client-final missing nonce",
        )
    })?;
    if final_nonce != combined_nonce {
        return Err(reject(
            stream,
            sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
            "SCRAM nonce mismatch",
        ));
    }
    let proof = B64.decode(proof_b64).map_err(|_| {
        reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "client proof not base64",
        )
    })?;

    let server_signature = match user.verifier.verify_exchange(
        AuthMessageInputs {
            client_first_bare,
            server_first: &server_first,
            client_final_without_proof,
        },
        &proof,
    ) {
        Ok(sig) => sig,
        Err(_) => {
            return Err(reject(
                stream,
                sqlstate::INVALID_PASSWORD,
                &format!("password authentication failed for user \"{}\"", user.name),
            ));
        }
    };

    // 5. AuthenticationSASLFinal: Int32(12) + "v=<server-signature>".
    let mut fin = Vec::new();
    fin.extend_from_slice(&12i32.to_be_bytes());
    fin.extend_from_slice(format!("v={}", B64.encode(server_signature)).as_bytes());
    write_message(stream, b'R', &fin)?;

    auth_ok(stream)
}

/// A gs2 header of "n,," (no channel binding) or "y,," (client thinks the
/// server may not support it). 'p' (binding required) is refused — we do not
/// advertise SCRAM-SHA-256-PLUS.
fn split_gs2_header<'a, W: Write>(
    client_first: &'a str,
    stream: &mut W,
) -> Result<(&'a str, &'a str), HandshakeError> {
    for allowed in ["n,,", "y,,"] {
        if let Some(rest) = client_first.strip_prefix(allowed) {
            return Ok((allowed, rest));
        }
    }
    Err(reject(
        stream,
        sqlstate::INVALID_AUTHORIZATION_SPECIFICATION,
        "unsupported SCRAM gs2 header (channel binding not offered)",
    ))
}

/// SASLInitialResponse's initial response may be absent (length -1). Then a
/// bare SASLResponse with the client-first follows.
fn read_optional_payload<S: Read + Write>(
    stream: &mut S,
    rest: &[u8],
) -> Result<Vec<u8>, HandshakeError> {
    if rest.len() < 4 {
        return Err(reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "SASLInitialResponse too short",
        ));
    }
    let declared = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
    if declared >= 0 {
        let declared = declared as usize;
        if rest.len() - 4 < declared {
            return Err(reject(
                stream,
                sqlstate::PROTOCOL_VIOLATION,
                "initial response shorter than declared",
            ));
        }
        return Ok(rest[4..4 + declared].to_vec());
    }
    let (t, payload) = read_message(stream).map_err(io_closed)?;
    if t != b'p' {
        return Err(reject(
            stream,
            sqlstate::PROTOCOL_VIOLATION,
            "expected SASLResponse",
        ));
    }
    Ok(payload)
}

/// `attr=value` scanner over a comma-separated SCRAM message.
fn parse_attr(msg: &str, attr: char) -> Option<&str> {
    for part in msg.split(',') {
        let mut kv = part.splitn(2, '=');
        let k = kv.next()?;
        if k.len() == 1 && k.starts_with(attr) {
            return kv.next();
        }
    }
    None
}

fn io_closed(e: std::io::Error) -> HandshakeError {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
            HandshakeError::Closed
        }
        // Our clock, not the peer's: `SO_RCVTIMEO` expiry (WouldBlock on
        // Linux) and the total deadline (TimedOut) both mean the budget
        // ran out — never a peer-initiated condition.
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HandshakeError::TimedOut,
        _ => HandshakeError::Io(e),
    }
}

fn prefix(code: i32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn auth_ok<W: Write>(stream: &mut W) -> Result<(), HandshakeError> {
    Ok(write_message(stream, b'R', &prefix(0, &[]))?)
}

/// Post-auth, pre-query messages every client expects: parameter statuses,
/// backend key data, ready-for-query.
fn finish_session<W: Write>(stream: &mut W, session: &Session) -> Result<(), HandshakeError> {
    for (name, value) in [
        ("server_version", "16.0"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
    ] {
        let mut payload = Vec::new();
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload.extend_from_slice(value.as_bytes());
        payload.push(0);
        write_message(stream, b'S', &payload)?;
    }
    let mut keydata = Vec::new();
    keydata.extend_from_slice(&session.backend_pid.to_be_bytes());
    keydata.extend_from_slice(&session.backend_secret.to_be_bytes());
    write_message(stream, b'K', &keydata)?;
    write_message(stream, b'Z', b"I")?;
    Ok(())
}
