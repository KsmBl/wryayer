//! Self-contained Avahi stand-in.
//!
//! Sandboxed apps that link avahi-client (Electron/Chromium, KDE, CUPS-linked)
//! call `avahi_client_new()` at startup, which makes *blocking* D-Bus calls to
//! `org.freedesktop.Avahi.Server` (`GetAPIVersion`, `GetState`, …).  When no
//! Avahi is reachable those calls fail and the app prints
//! "Failed to connect to Avahi server: Daemon not running".
//!
//! Rather than run the host avahi-daemon (a host-wide change that also
//! advertises the machine on the LAN) or bind the host system bus into the
//! sandbox, we give each sandbox a *private* system bus with this tiny process
//! owning `org.freedesktop.Avahi` and answering just enough of the Server
//! interface to satisfy `avahi_client_new()`.  It has no networking code, so it
//! can never broadcast anything, and everything it touches — the bus socket, the
//! dbus-daemon config — lives under the app's own directory in ~/.wryayer.
//!
//! Actual service discovery returns "nothing found" (the honest answer for an
//! isolated sandbox); apps that only probe Avahi at startup work as normal.
//!
//! The D-Bus wire protocol is marshaled by hand here to avoid pulling in a
//! D-Bus client crate for such a small surface.  All values are little-endian
//! (matching libdbus on the platforms wryayer targets).

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

// Values mirrored from a live avahi-daemon so the client accepts us.
const AVAHI_API_VERSION: u32 = 516; // Server.GetAPIVersion
const AVAHI_SERVER_RUNNING: i32 = 2; // AVAHI_SERVER_RUNNING from avahi-common/defs.h
const AVAHI_VERSION_STRING: &str = "avahi 0.8";

// D-Bus message types.
const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8 = 3;

// Header field codes.
const F_PATH: u8 = 1;
const F_INTERFACE: u8 = 2;
const F_MEMBER: u8 = 3;
const F_ERROR_NAME: u8 = 4;
const F_REPLY_SERIAL: u8 = 5;
const F_DESTINATION: u8 = 6;
const F_SIGNATURE: u8 = 8;

/// Bring up the private bus and serve the Avahi stub on it until killed.
///
/// Spawns `dbus-daemon` with `config_path` (which listens on `socket_path`) as a
/// child so it dies together with this process, connects to it as a client,
/// claims `org.freedesktop.Avahi`, writes `<socket_path>.ready` once the name is
/// held, and then answers method calls forever.
pub fn run(socket_path: &str, config_path: &str) -> Result<()> {
    // dbus-daemon dies with us: PR_SET_PDEATHSIG on the child.  This process in
    // turn carries PDEATHSIG from run.rs, so the whole chain unwinds when the
    // sandbox exits.
    let mut daemon = {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("dbus-daemon");
        c.arg(format!("--config-file={config_path}")).arg("--nofork");
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            c.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        c.spawn().context("failed to spawn dbus-daemon for avahi stub")?
    };

    // Wait for the bus socket, then connect.  If the daemon dies, bail.
    let stream = connect_with_retry(socket_path, 200)
        .context("avahi stub could not connect to its private bus")?;

    let serve_result = serve(stream, socket_path);

    let _ = daemon.kill();
    let _ = daemon.wait();
    serve_result
}

fn connect_with_retry(path: &str, tries: u32) -> Result<UnixStream> {
    for _ in 0..tries {
        if Path::new(path).exists() {
            if let Ok(s) = UnixStream::connect(path) {
                return Ok(s);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    bail!("bus socket {path} never appeared")
}

fn serve(mut stream: UnixStream, socket_path: &str) -> Result<()> {
    authenticate(&mut stream)?;

    let mut serial: u32 = 0;
    let mut next_serial = || {
        serial += 1;
        serial
    };

    // Say hello (the bus assigns us a unique name) and claim org.freedesktop.Avahi.
    let hello = build_call(next_serial(), "org.freedesktop.DBus", "/org/freedesktop/DBus",
        "org.freedesktop.DBus", "Hello", "", &[]);
    stream.write_all(&hello)?;

    let req_serial = next_serial();
    let mut body = Marshal::new();
    body.string("org.freedesktop.Avahi");
    body.u32(0); // no flags
    let request = build_call(req_serial, "org.freedesktop.DBus", "/org/freedesktop/DBus",
        "org.freedesktop.DBus", "RequestName", "su", &body.buf);
    stream.write_all(&request)?;

    let mut announced = false;
    loop {
        let msg = match read_message(&mut stream) {
            Ok(m) => m,
            Err(_) => break, // peer closed / bus gone
        };

        // Once RequestName has returned, the well-known name is ours: signal the
        // launcher (which waits on this marker) that the sandbox may start.
        if !announced && msg.mtype == MSG_METHOD_RETURN && msg.reply_serial == req_serial {
            announced = true;
            let _ = std::fs::write(format!("{socket_path}.ready"), b"1");
            continue;
        }

        if msg.mtype != MSG_METHOD_CALL {
            continue; // replies to our own calls, signals — ignore
        }

        let reply = dispatch(&msg, next_serial());
        if let Some(bytes) = reply {
            if stream.write_all(&bytes).is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// Build the reply for an incoming method call, or None if none is warranted.
fn dispatch(msg: &Msg, serial: u32) -> Option<Vec<u8>> {
    let dest = msg.sender.as_deref().unwrap_or("");
    let member = msg.member.as_deref().unwrap_or("");
    let iface = msg.interface.as_deref().unwrap_or("");

    // Peer / introspection housekeeping that libdbus may issue.
    if iface == "org.freedesktop.DBus.Peer" {
        match member {
            "Ping" => return Some(build_return(serial, msg.serial, dest, "", &[])),
            "GetMachineId" => {
                let mut b = Marshal::new();
                b.string(&"0".repeat(32));
                return Some(build_return(serial, msg.serial, dest, "s", &b.buf));
            }
            _ => {}
        }
    }

    // The org.freedesktop.Avahi.Server surface avahi_client_new() depends on.
    // Anything we don't model returns an error, which avahi-client treats as an
    // empty/negative result rather than a hard failure — so browsing simply
    // finds nothing, which is the truth inside an isolated sandbox.
    match member {
        "GetAPIVersion" => {
            let mut b = Marshal::new();
            b.u32(AVAHI_API_VERSION);
            Some(build_return(serial, msg.serial, dest, "u", &b.buf))
        }
        "GetState" => {
            let mut b = Marshal::new();
            b.i32(AVAHI_SERVER_RUNNING);
            Some(build_return(serial, msg.serial, dest, "i", &b.buf))
        }
        "GetVersionString" => {
            let mut b = Marshal::new();
            b.string(AVAHI_VERSION_STRING);
            Some(build_return(serial, msg.serial, dest, "s", &b.buf))
        }
        "GetHostName" => reply_string(serial, msg, dest, "localhost"),
        "GetHostNameFqdn" => reply_string(serial, msg, dest, "localhost.local"),
        "GetDomainName" => reply_string(serial, msg, dest, "local"),
        _ => Some(build_error(
            serial,
            msg.serial,
            dest,
            "org.freedesktop.DBus.Error.NotSupported",
            "not supported by the wryayer avahi stub",
        )),
    }
}

fn reply_string(serial: u32, msg: &Msg, dest: &str, value: &str) -> Option<Vec<u8>> {
    let mut b = Marshal::new();
    b.string(value);
    Some(build_return(serial, msg.serial, dest, "s", &b.buf))
}

// ── SASL EXTERNAL authentication (client side) ─────────────────────────────────

fn authenticate(stream: &mut UnixStream) -> Result<()> {
    // The protocol requires a leading NUL byte before the SASL exchange.
    stream.write_all(b"\0")?;
    let uid = unsafe { libc::getuid() };
    let uid_hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
    stream.write_all(format!("AUTH EXTERNAL {uid_hex}\r\n").as_bytes())?;

    let line = read_line(stream)?;
    if !line.starts_with("OK ") {
        bail!("unexpected auth response: {line}");
    }
    stream.write_all(b"BEGIN\r\n")?;
    Ok(())
}

fn read_line(stream: &mut UnixStream) -> Result<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        out.push(byte[0]);
        if out.ends_with(b"\r\n") {
            out.truncate(out.len() - 2);
            break;
        }
        if out.len() > 4096 {
            bail!("auth line too long");
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ── Incoming message parsing ───────────────────────────────────────────────────

struct Msg {
    mtype: u8,
    serial: u32,
    reply_serial: u32,
    member: Option<String>,
    interface: Option<String>,
    sender: Option<String>,
    /// Method-call arguments. The stub's methods take none, so this is only
    /// inspected by the round-trip tests, not by `dispatch`.
    #[allow(dead_code)]
    body: Vec<u8>,
}

fn read_message<R: Read>(stream: &mut R) -> Result<Msg> {
    // Fixed header is 12 bytes, immediately followed by the u32 length of the
    // header-field array — read all 16 up front.
    let mut head = [0u8; 16];
    stream.read_exact(&mut head)?;
    if head[0] != b'l' {
        bail!("only little-endian messages are supported");
    }
    let mtype = head[1];
    let body_len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let serial = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let fields_len = u32::from_le_bytes([head[12], head[13], head[14], head[15]]) as usize;

    // Header fields, then padding up to an 8-byte boundary, then the body.
    let mut fields = vec![0u8; fields_len];
    stream.read_exact(&mut fields)?;
    let consumed = 16 + fields_len;
    let pad = (8 - (consumed % 8)) % 8;
    let mut scratch = vec![0u8; pad];
    stream.read_exact(&mut scratch)?;
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body)?;

    let (mut member, mut interface, mut sender, mut reply_serial) = (None, None, None, 0u32);
    let mut pos = 0usize;
    while pos < fields.len() {
        pos = align(pos, 8);
        if pos >= fields.len() {
            break;
        }
        let code = fields[pos];
        pos += 1;
        // Variant: 1-byte signature length, signature, NUL.
        let sig_len = fields[pos] as usize;
        let sig = fields[pos + 1..pos + 1 + sig_len].to_vec();
        pos += 1 + sig_len + 1;
        match sig.as_slice() {
            b"s" | b"o" => {
                pos = align(pos, 4);
                let len =
                    u32::from_le_bytes([fields[pos], fields[pos + 1], fields[pos + 2], fields[pos + 3]])
                        as usize;
                pos += 4;
                let s = String::from_utf8_lossy(&fields[pos..pos + len]).into_owned();
                pos += len + 1; // + NUL
                match code {
                    F_MEMBER => member = Some(s),
                    F_INTERFACE => interface = Some(s),
                    F_DESTINATION => {}
                    F_ERROR_NAME | F_PATH => {}
                    7 => sender = Some(s), // F_SENDER
                    _ => {}
                }
            }
            b"g" => {
                let l = fields[pos] as usize;
                pos += 1 + l + 1;
            }
            b"u" => {
                pos = align(pos, 4);
                let v =
                    u32::from_le_bytes([fields[pos], fields[pos + 1], fields[pos + 2], fields[pos + 3]]);
                pos += 4;
                if code == F_REPLY_SERIAL {
                    reply_serial = v;
                }
            }
            _ => break, // unknown field type — stop parsing, we have what we need
        }
    }

    Ok(Msg { mtype, serial, reply_serial, member, interface, sender, body })
}

// ── Outgoing message building ──────────────────────────────────────────────────

fn build_call(
    serial: u32,
    dest: &str,
    path: &str,
    iface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut fields: Vec<(u8, FieldVal)> = vec![
        (F_PATH, FieldVal::ObjPath(path.into())),
        (F_DESTINATION, FieldVal::Str(dest.into())),
        (F_INTERFACE, FieldVal::Str(iface.into())),
        (F_MEMBER, FieldVal::Str(member.into())),
    ];
    if !signature.is_empty() {
        fields.push((F_SIGNATURE, FieldVal::Sig(signature.into())));
    }
    assemble(MSG_METHOD_CALL, serial, &fields, body)
}

fn build_return(serial: u32, reply_serial: u32, dest: &str, signature: &str, body: &[u8]) -> Vec<u8> {
    let mut fields: Vec<(u8, FieldVal)> = vec![
        (F_REPLY_SERIAL, FieldVal::U32(reply_serial)),
    ];
    if !dest.is_empty() {
        fields.push((F_DESTINATION, FieldVal::Str(dest.into())));
    }
    if !signature.is_empty() {
        fields.push((F_SIGNATURE, FieldVal::Sig(signature.into())));
    }
    assemble(MSG_METHOD_RETURN, serial, &fields, body)
}

fn build_error(serial: u32, reply_serial: u32, dest: &str, name: &str, message: &str) -> Vec<u8> {
    let mut body = Marshal::new();
    body.string(message);
    let mut fields: Vec<(u8, FieldVal)> = vec![
        (F_ERROR_NAME, FieldVal::Str(name.into())),
        (F_REPLY_SERIAL, FieldVal::U32(reply_serial)),
    ];
    if !dest.is_empty() {
        fields.push((F_DESTINATION, FieldVal::Str(dest.into())));
    }
    fields.push((F_SIGNATURE, FieldVal::Sig("s".into())));
    assemble(MSG_ERROR, serial, &fields, &body.buf)
}

enum FieldVal {
    Str(String),
    ObjPath(String),
    Sig(String),
    U32(u32),
}

fn assemble(mtype: u8, serial: u32, fields: &[(u8, FieldVal)], body: &[u8]) -> Vec<u8> {
    let mut m = Marshal::new();
    m.buf.push(b'l'); // little-endian
    m.buf.push(mtype);
    m.buf.push(0); // flags
    m.buf.push(1); // protocol version
    let body_len_at = m.buf.len();
    m.buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.buf.extend_from_slice(&serial.to_le_bytes());

    // Header-field array: a u32 byte-length followed by 8-aligned struct entries.
    let array_len_at = m.buf.len();
    m.buf.extend_from_slice(&0u32.to_le_bytes()); // placeholder
    let array_start = m.buf.len();
    for (code, val) in fields {
        m.align(8);
        m.buf.push(*code);
        match val {
            FieldVal::Str(s) => {
                m.signature("s");
                m.string(s);
            }
            FieldVal::ObjPath(s) => {
                m.signature("o");
                m.string(s);
            }
            FieldVal::Sig(s) => {
                m.signature("g");
                m.sig_value(s);
            }
            FieldVal::U32(v) => {
                m.signature("u");
                m.u32(*v);
            }
        }
    }
    let array_len = (m.buf.len() - array_start) as u32;
    m.buf[array_len_at..array_len_at + 4].copy_from_slice(&array_len.to_le_bytes());

    // Body starts on an 8-byte boundary.
    m.align(8);
    m.buf.extend_from_slice(body);

    let _ = body_len_at; // body length was written above from body.len()
    m.buf
}

// ── Low-level marshaling primitives ────────────────────────────────────────────

struct Marshal {
    buf: Vec<u8>,
}

impl Marshal {
    fn new() -> Self {
        Marshal { buf: Vec::new() }
    }
    fn align(&mut self, n: usize) {
        while self.buf.len() % n != 0 {
            self.buf.push(0);
        }
    }
    fn u32(&mut self, v: u32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// A STRING/OBJECT_PATH: 4-byte length (excl. NUL), bytes, trailing NUL.
    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    /// The signature marker inside a header-field variant (1-byte length).
    fn signature(&mut self, s: &str) {
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    /// A SIGNATURE-typed value in a body (same encoding as `signature`).
    fn sig_value(&mut self, s: &str) {
        self.signature(s);
    }
}

fn align(pos: usize, n: usize) -> usize {
    (pos + n - 1) & !(n - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A method call survives marshal → parse with all header fields intact.
    #[test]
    fn method_call_round_trips() {
        let bytes = build_call(
            42,
            "org.freedesktop.Avahi",
            "/",
            "org.freedesktop.Avahi.Server",
            "GetState",
            "",
            &[],
        );
        let msg = read_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(msg.mtype, MSG_METHOD_CALL);
        assert_eq!(msg.serial, 42);
        assert_eq!(msg.member.as_deref(), Some("GetState"));
        assert_eq!(msg.interface.as_deref(), Some("org.freedesktop.Avahi.Server"));
    }

    /// A method return carries its reply-serial and body back out intact.
    #[test]
    fn method_return_round_trips_with_u32_body() {
        let mut b = Marshal::new();
        b.u32(516);
        let bytes = build_return(7, 42, ":1.5", "u", &b.buf);
        let msg = read_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(msg.mtype, MSG_METHOD_RETURN);
        assert_eq!(msg.reply_serial, 42);
        assert_eq!(msg.body, 516u32.to_le_bytes());
    }

    /// dispatch answers the calls avahi_client_new() makes at startup with the
    /// exact values a real avahi-daemon returns, so the client accepts the stub.
    #[test]
    fn dispatch_answers_startup_probes() {
        // GetAPIVersion -> u32 516
        let call = read_message(&mut Cursor::new(build_call(
            1, "org.freedesktop.Avahi", "/", "org.freedesktop.Avahi.Server", "GetAPIVersion", "", &[],
        )))
        .unwrap();
        let reply = read_message(&mut Cursor::new(dispatch(&call, 100).unwrap())).unwrap();
        assert_eq!(reply.mtype, MSG_METHOD_RETURN);
        assert_eq!(reply.reply_serial, 1);
        assert_eq!(reply.body, AVAHI_API_VERSION.to_le_bytes());

        // GetState -> i32 RUNNING (2)
        let call = read_message(&mut Cursor::new(build_call(
            2, "org.freedesktop.Avahi", "/", "org.freedesktop.Avahi.Server", "GetState", "", &[],
        )))
        .unwrap();
        let reply = read_message(&mut Cursor::new(dispatch(&call, 101).unwrap())).unwrap();
        assert_eq!(reply.mtype, MSG_METHOD_RETURN);
        assert_eq!(reply.body, AVAHI_SERVER_RUNNING.to_le_bytes());
    }

    /// Unknown methods get an error reply (not a hang or a panic), which
    /// avahi-client treats as "feature unavailable" rather than a hard failure.
    #[test]
    fn dispatch_errors_unknown_method() {
        let call = read_message(&mut Cursor::new(build_call(
            3, "org.freedesktop.Avahi", "/", "org.freedesktop.Avahi.Server", "ServiceBrowserNew", "", &[],
        )))
        .unwrap();
        let reply = read_message(&mut Cursor::new(dispatch(&call, 102).unwrap())).unwrap();
        assert_eq!(reply.mtype, MSG_ERROR);
        assert_eq!(reply.reply_serial, 3);
    }
}
