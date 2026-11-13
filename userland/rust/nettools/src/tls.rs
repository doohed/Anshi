//! TLS for fetch's https:// URLs: rustls on top of a TCP socket. With no
//! std, rustls offers only its "unbuffered" connection -- a state machine
//! that says what it needs next (bytes to send, more bytes received,
//! ready for application data) -- which TlsStream drives through the
//! socket's own non-blocking-but-retried calls.
//!
//! Certificates are checked properly: against Mozilla's root store
//! (webpki-roots, compiled in), for the host name asked for, at the
//! current time from the machine's real-time clock. Randomness comes from
//! the kernel's getrandom().

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use anshi::net::Socket;
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{AppDataRecord, ConnectionState, EncodeError, EncryptError, InsufficientSizeError};

const IO_TIMEOUT_MS: u64 = 15_000;
const READ_CHUNK: usize = 16 * 1024;

/// getrandom's hook (see ../../.cargo/config.toml).
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), getrandom::Error> {
    anshi::fill_random(unsafe { core::slice::from_raw_parts_mut(dest, len) });
    Ok(())
}

#[derive(Debug)]
struct Clock;

impl TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        anshi::unix_time_ms().map(|ms| UnixTime::since_unix_epoch(Duration::from_millis(ms)))
    }
}

fn config() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(rustls_rustcrypto::provider());
    let mut config = rustls::ClientConfig::builder_with_details(provider, Arc::new(Clock))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// What the state machine is being run for, and so when it can stop.
#[derive(Clone, Copy, PartialEq)]
enum Goal {
    Handshake,
    Write,
    Read,
}

pub struct TlsStream {
    sock: Socket,
    conn: UnbufferedClientConnection,
    /// TLS bytes received and not yet processed: `incoming[..used]`.
    incoming: Vec<u8>,
    used: usize,
    outgoing: Vec<u8>,
    /// Decrypted application data not yet handed out: `plain[taken..]`.
    plain: Vec<u8>,
    taken: usize,
    /// Application data waiting for the handshake to finish.
    pending: Vec<u8>,
    closed: bool,
}

fn tls_error(e: rustls::Error) -> String {
    use rustls::CertificateError as C;
    let rustls::Error::InvalidCertificate(c) = &e else { return format!("TLS: {e}") };
    let why = match c {
        C::Expired | C::ExpiredContext { .. } => String::from("it has expired"),
        C::NotValidYet | C::NotValidYetContext { .. } => {
            String::from("it isn't valid yet -- is the clock right? (rtc line in the boot log)")
        }
        C::NotValidForName | C::NotValidForNameContext { .. } => String::from("it is for a different host name"),
        C::UnknownIssuer => String::from("no trusted authority signed it (self-signed, or an unknown CA)"),
        C::Revoked => String::from("it has been revoked"),
        C::BadSignature => String::from("its signature is invalid"),
        other => format!("{other:?}"),
    };
    format!("certificate rejected: {why}")
}

impl TlsStream {
    /// Runs the handshake for `host` over a connected `sock`.
    pub fn connect(sock: Socket, host: &str) -> Result<TlsStream, String> {
        let name = ServerName::try_from(String::from(host)).map_err(|_| format!("{host}: not a valid TLS name"))?;
        let conn = UnbufferedClientConnection::new(config()?, name).map_err(tls_error)?;
        let mut s = TlsStream {
            sock,
            conn,
            incoming: vec![0; READ_CHUNK],
            used: 0,
            outgoing: vec![0; READ_CHUNK],
            plain: Vec::new(),
            taken: 0,
            pending: Vec::new(),
            closed: false,
        };
        s.run(Goal::Handshake)?;
        Ok(s)
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.pending.extend_from_slice(data);
        self.run(Goal::Write)
    }

    /// Decrypted bytes into `buf`; 0 once the server has closed.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        if self.taken == self.plain.len() && !self.closed {
            self.plain.clear();
            self.taken = 0;
            self.run(Goal::Read)?;
        }
        let n = buf.len().min(self.plain.len() - self.taken);
        buf[..n].copy_from_slice(&self.plain[self.taken..self.taken + n]);
        self.taken += n;
        Ok(n)
    }

    fn send(&mut self, len: usize) -> Result<(), String> {
        self.sock.write_all(&self.outgoing[..len], IO_TIMEOUT_MS).map_err(|e| format!("send: {e}"))
    }

    /// Runs the state machine until `goal` is met.
    fn run(&mut self, goal: Goal) -> Result<(), String> {
        loop {
            let mut need_read = false;
            let mut done = false;
            let mut out_len = 0;
            let Self { conn, incoming, used, outgoing, plain, pending, closed, .. } = self;
            let status = conn.process_tls_records(&mut incoming[..*used]);
            let mut discard = status.discard;
            match status.state.map_err(tls_error)? {
                ConnectionState::ReadTraffic(mut r) => {
                    while let Some(record) = r.next_record() {
                        let AppDataRecord { discard: more, payload } = record.map_err(tls_error)?;
                        discard += more;
                        plain.extend_from_slice(payload);
                    }
                    done = goal == Goal::Read;
                }
                ConnectionState::EncodeTlsData(mut e) => loop {
                    match e.encode(&mut outgoing[out_len..]) {
                        Ok(n) => {
                            out_len += n;
                            break;
                        }
                        Err(EncodeError::InsufficientSize(InsufficientSizeError { required_size })) => {
                            outgoing.resize(out_len + required_size, 0);
                        }
                        Err(e) => return Err(format!("TLS: {e}")),
                    }
                },
                ConnectionState::TransmitTlsData(t) => {
                    // Everything encoded so far goes out together.
                    t.done();
                }
                ConnectionState::BlockedHandshake => need_read = true,
                ConnectionState::WriteTraffic(mut w) => match goal {
                    Goal::Handshake => done = true,
                    Goal::Write => {
                        loop {
                            match w.encrypt(pending, &mut outgoing[..]) {
                                Ok(n) => {
                                    out_len = n;
                                    break;
                                }
                                Err(EncryptError::InsufficientSize(InsufficientSizeError { required_size })) => {
                                    outgoing.resize(required_size, 0);
                                }
                                Err(e) => return Err(format!("TLS: {e}")),
                            }
                        }
                        pending.clear();
                        done = true;
                    }
                    Goal::Read => need_read = true,
                },
                ConnectionState::PeerClosed | ConnectionState::Closed => {
                    *closed = true;
                    done = goal == Goal::Read;
                    if goal != Goal::Read {
                        return Err(String::from("TLS: the server closed the connection"));
                    }
                }
                _ => return Err(String::from("TLS: unexpected state")),
            }
            incoming.copy_within(discard..*used, 0);
            *used -= discard;
            if out_len > 0 {
                self.send(out_len)?;
            }
            if done {
                return Ok(());
            }
            if need_read {
                if self.incoming.len() - self.used < READ_CHUNK {
                    let grown = self.used + READ_CHUNK;
                    self.incoming.resize(grown, 0);
                }
                let n = self
                    .sock
                    .read(&mut self.incoming[self.used..], IO_TIMEOUT_MS)
                    .map_err(|e| format!("receive: {e}"))?;
                if n == 0 {
                    // TCP closed without a close_notify: plenty of HTTPS
                    // servers do that after a Connection: close reply.
                    self.closed = true;
                    if goal == Goal::Read {
                        return Ok(());
                    }
                    return Err(String::from("TLS: connection closed during the handshake"));
                }
                self.used += n;
            }
        }
    }
}
