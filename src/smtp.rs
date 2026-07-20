//! The SMTP connection layer: pool configuration ([`Opt`]), a single pooled
//! connection ([`Conn`]) built on lettre's low-level `SmtpConnection`, and the
//! mapping from lettre errors to this crate's [`Error`] with the classification
//! the pool needs. This is the Rust port of the connection half of the Go
//! `pool.go`; the pool itself lives in [`crate::pool`].

use std::time::{Duration, Instant};

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{SmtpConnection, TlsParameters};
use lettre::transport::smtp::commands::Rset;
use lettre::transport::smtp::extension::ClientId;
use lettre::Address;

use crate::email::extract_address;
use crate::{Email, Error, SmtpError, SslType};

/// The SMTP authentication mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMechanism {
    /// AUTH PLAIN.
    Plain,
    /// AUTH LOGIN.
    Login,
}

/// SMTP authentication credentials. Mirrors the role of Go's `smtp.Auth` /
/// `LoginAuth`.
#[derive(Clone, Debug)]
pub struct Auth {
    pub username: String,
    pub password: String,
    pub mechanism: AuthMechanism,
}

/// SMTP pool options. Mirrors Go's `Opt`.
#[derive(Clone, Debug)]
pub struct Opt {
    /// The SMTP server's hostname.
    pub host: String,
    /// The SMTP server port.
    pub port: u16,
    /// Optional hostname to pass with the HELO/EHLO command. Default `localhost`.
    pub hello_hostname: String,
    /// The maximum allowed concurrent SMTP connections. Must be >= 1.
    pub max_conns: usize,
    /// Number of times a message is retried if sending fails. Min/default 1.
    pub max_message_retries: usize,
    /// Optional delay before retrying a failed message.
    pub message_retry_delay: Duration,
    /// Idle time before a pooled connection is swept and closed.
    pub idle_timeout: Duration,
    /// Maximum time to wait for a free connection; also the dial timeout.
    pub pool_wait_timeout: Duration,
    /// The kind of SSL/TLS to use.
    pub ssl: SslType,
    /// Optional authentication.
    pub auth: Option<Auth>,
    /// If true, invalid TLS certificates/hostnames are accepted (testing only).
    pub tls_dangerous_accept_invalid_certs: bool,
}

impl Default for Opt {
    fn default() -> Self {
        Opt {
            host: String::new(),
            port: 25,
            hello_hostname: String::new(),
            max_conns: 1,
            max_message_retries: 1,
            message_retry_delay: Duration::ZERO,
            idle_timeout: Duration::ZERO,
            pool_wait_timeout: Duration::from_secs(2),
            ssl: SslType::None,
            auth: None,
            tls_dangerous_accept_invalid_certs: false,
        }
    }
}

/// A single SMTP client connection in the pool.
pub(crate) struct Conn {
    conn: SmtpConnection,
    /// When the last message on this connection was sent; used by the sweeper.
    pub(crate) last_activity: Instant,
}

impl Conn {
    /// Creates a new SMTP connection: dial (optionally over TLS), greet, upgrade
    /// via STARTTLS if requested, and authenticate if configured. Mirrors Go's
    /// `newConn`.
    pub(crate) fn new(opt: &Opt) -> Result<Conn, Error> {
        let hello = ClientId::Domain(if opt.hello_hostname.is_empty() {
            "localhost".to_string()
        } else {
            opt.hello_hostname.clone()
        });
        let timeout = Some(opt.pool_wait_timeout);

        let tls_params = match opt.ssl {
            SslType::None => None,
            SslType::Tls | SslType::StartTls => Some(build_tls(opt)?),
        };

        let mut conn = match opt.ssl {
            // Implicit TLS: connect directly over TLS.
            SslType::Tls => SmtpConnection::connect(
                (opt.host.as_str(), opt.port),
                timeout,
                &hello,
                tls_params.as_ref(),
                None,
            )
            .map_err(map_smtp_error)?,
            // Plain (possibly upgraded via STARTTLS below).
            SslType::None | SslType::StartTls => {
                SmtpConnection::connect((opt.host.as_str(), opt.port), timeout, &hello, None, None)
                    .map_err(map_smtp_error)?
            }
        };

        // Attempt to upgrade to STARTTLS.
        if opt.ssl == SslType::StartTls {
            let tls = tls_params.as_ref().expect("tls params for starttls");
            conn.starttls(tls, &hello).map_err(map_smtp_error)?;
        }

        // Optional auth.
        if let Some(auth) = &opt.auth {
            let mechanism = match auth.mechanism {
                AuthMechanism::Plain => Mechanism::Plain,
                AuthMechanism::Login => Mechanism::Login,
            };
            let creds = Credentials::new(auth.username.clone(), auth.password.clone());
            conn.auth(&[mechanism], &creds).map_err(map_smtp_error)?;
        }

        Ok(Conn {
            conn,
            last_activity: Instant::now(),
        })
    }

    /// Sends `e` on this connection. The returned bool indicates whether the
    /// message can be retried on a network-type error. Mirrors Go's `conn.send`.
    pub(crate) fn send(&mut self, e: &Email) -> (bool, Result<(), Error>) {
        self.last_activity = Instant::now();

        let envelope = match build_envelope(e) {
            Ok(env) => env,
            // Address/parse errors are treated as retriable, matching Go.
            Err(err) => return (true, Err(err)),
        };

        let body = match e.bytes() {
            Ok(b) => b,
            Err(err) => return (false, Err(err)),
        };

        match self.conn.send(&envelope, &body) {
            Ok(_) => (false, Ok(())),
            Err(err) => {
                let mapped = map_smtp_error(err);
                let retry = mapped.is_retriable();
                (retry, Err(mapped))
            }
        }
    }

    /// Issues SMTP RSET to clear transaction state before reuse. Mirrors Go's
    /// `conn.Reset`.
    pub(crate) fn reset(&mut self) -> Result<(), Error> {
        self.conn.command(Rset).map(|_| ()).map_err(map_smtp_error)
    }

    /// Gracefully closes the connection (QUIT).
    pub(crate) fn quit(&mut self) {
        let _ = self.conn.quit();
    }

    /// Abruptly closes the connection.
    pub(crate) fn close(&mut self) {
        self.conn.abort();
    }
}

/// Builds the SMTP envelope from the message's sender and combined recipients.
fn build_envelope(e: &Email) -> Result<Envelope, Error> {
    let from = parse_addr(&e.parse_sender()?)?;
    let recipients = combine_emails(e)?;
    Envelope::new(Some(from), recipients).map_err(|err| Error::Address(err.to_string()))
}

/// Combines To/Cc/Bcc into a single list of parsed recipient addresses. Mirrors
/// Go's `combineEmails`.
fn combine_emails(e: &Email) -> Result<Vec<Address>, Error> {
    let mut out = Vec::with_capacity(e.to.len() + e.cc.len() + e.bcc.len());
    for list in [&e.to, &e.cc, &e.bcc] {
        for addr in list {
            out.push(parse_addr(&extract_address(addr)?)?);
        }
    }
    Ok(out)
}

fn parse_addr(bare: &str) -> Result<Address, Error> {
    bare.parse::<Address>()
        .map_err(|err| Error::Address(format!("{bare}: {err}")))
}

fn build_tls(opt: &Opt) -> Result<TlsParameters, Error> {
    let builder = TlsParameters::builder(opt.host.clone());
    let builder = if opt.tls_dangerous_accept_invalid_certs {
        builder
            .dangerous_accept_invalid_certs(true)
            .dangerous_accept_invalid_hostnames(true)
    } else {
        builder
    };
    builder.build().map_err(|err| Error::Smtp(map_lettre(&err)))
}

/// Maps a lettre SMTP error into this crate's [`Error::Smtp`].
fn map_smtp_error(err: lettre::transport::smtp::Error) -> Error {
    Error::Smtp(map_lettre(&err))
}

fn map_lettre(err: &lettre::transport::smtp::Error) -> SmtpError {
    let code = err
        .status()
        .map(|c| (c.severity as u16) * 100 + (c.category as u16) * 10 + (c.detail as u16));

    // A network-type (retriable) error is one where the server did not respond
    // with an SMTP status and which is not an internal client or TLS error.
    let retriable = code.is_none() && !err.is_response() && !err.is_client() && !err.is_tls();

    SmtpError {
        message: err.to_string(),
        code,
        retriable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SmtpError;

    /// Verifies the connection-reuse rule that the pool relies on:
    /// 421 (rate-limit) and network errors discard the connection; other SMTP
    /// response errors (e.g. 550) keep it. Mirrors the Go 421-drops/550-keeps
    /// behavior.
    #[test]
    fn test_conn_reuse_classification() {
        let e421 = Error::Smtp(SmtpError {
            message: "rate limited".into(),
            code: Some(421),
            retriable: false,
        });
        let e550 = Error::Smtp(SmtpError {
            message: "bad recipient".into(),
            code: Some(550),
            retriable: false,
        });
        let net = Error::Smtp(SmtpError {
            message: "broken pipe".into(),
            code: None,
            retriable: true,
        });

        assert!(e421.should_close_conn(), "421 should close the conn");
        assert!(!e550.should_close_conn(), "550 should keep the conn");
        assert!(
            net.should_close_conn(),
            "network error should close the conn"
        );

        assert!(net.is_retriable());
        assert!(!e550.is_retriable());
        assert!(!e421.is_retriable());
    }

    // --- Embedded SMTP sink integration (single connection, no pool) ---
    //
    // A minimal in-process SMTP server stands in for MailHog so the send path
    // is verified end-to-end without any external service.

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Spawns a minimal SMTP sink that accepts `count` connections and records
    /// each received DATA payload. Returns the bound port, the shared record of
    /// payloads, and the server thread handle.
    pub(crate) fn spawn_sink(
        count: usize,
    ) -> (u16, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind sink");
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();

        let handle = thread::spawn(move || {
            for _ in 0..count {
                let (stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let sink = sink.clone();
                thread::spawn(move || handle_client(stream, sink));
            }
        });

        (port, received, handle)
    }

    fn handle_client(stream: std::net::TcpStream, sink: Arc<Mutex<Vec<String>>>) {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let _ = writer.write_all(b"220 localhost ESMTP test\r\n");

        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let cmd = line.trim_end().to_uppercase();
            if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                let _ = writer.write_all(b"250-localhost\r\n250 OK\r\n");
            } else if cmd.starts_with("DATA") {
                let _ = writer.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n");
                let mut data = String::new();
                let mut l = String::new();
                loop {
                    l.clear();
                    if reader.read_line(&mut l).unwrap_or(0) == 0 {
                        break;
                    }
                    if l == ".\r\n" || l == ".\n" {
                        break;
                    }
                    data.push_str(&l);
                }
                sink.lock().unwrap().push(data);
                let _ = writer.write_all(b"250 OK: queued\r\n");
            } else if cmd.starts_with("QUIT") {
                let _ = writer.write_all(b"221 Bye\r\n");
                break;
            } else {
                // MAIL FROM, RCPT TO, RSET, NOOP, etc.
                let _ = writer.write_all(b"250 OK\r\n");
            }
        }
    }

    #[test]
    fn test_single_connection_send() {
        let (port, received, _handle) = spawn_sink(1);

        let opt = Opt {
            host: "127.0.0.1".into(),
            port,
            ssl: SslType::None,
            pool_wait_timeout: Duration::from_secs(2),
            ..Default::default()
        };

        let mut conn = Conn::new(&opt).expect("connect to sink");
        let email = Email {
            from: "sender@example.com".into(),
            to: vec!["recipient@example.com".into()],
            subject: "Phase 3 Single Conn".into(),
            text: b"Single-connection send test.".to_vec(),
            ..Default::default()
        };

        let (_retry, res) = conn.send(&email);
        res.expect("first send should succeed");

        // A second message reuses the same connection after RSET.
        conn.reset().expect("rset");
        let (_r2, res2) = conn.send(&email);
        res2.expect("second send should succeed");

        conn.quit();

        let msgs = received.lock().unwrap();
        assert_eq!(msgs.len(), 2, "sink should have received two messages");
        assert!(
            msgs[0].contains("recipient@example.com"),
            "recipient not found in received data: {}",
            msgs[0]
        );
        assert!(msgs[0].contains("Subject: Phase 3 Single Conn"));
    }
}
