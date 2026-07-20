//! `pooledsmtp` creates a pool of reusable SMTP connections for high-throughput
//! e-mailing. It gracefully handles idle connections, timeouts, and retries. The
//! wire protocol and TLS are provided by `lettre`'s low-level `SmtpConnection`,
//! while the pool and MIME serialization are ported directly.
//!
//! The e-mail layer provides the message model and MIME serialization
//! ([`Email`], [`Attachment`], [`Email::bytes`]), parsing
//! ([`new_email_from_reader`]), and attachments ([`Email::attach`],
//! [`Email::attach_file`]). The [`Pool`] (configured via [`Opt`]) sends
//! messages over a bounded set of reusable connections.
//!
//! ```no_run
//! use std::time::Duration;
//! use pooledsmtp::{Email, Opt, Pool, SslType};
//!
//! let pool = Pool::new(Opt {
//!     host: "localhost".into(),
//!     port: 1025,
//!     max_conns: 10,
//!     idle_timeout: Duration::from_secs(10),
//!     pool_wait_timeout: Duration::from_secs(3),
//!     ssl: SslType::None,
//!     ..Default::default()
//! })
//! .unwrap();
//!
//! let mut e = Email {
//!     from: "John Doe <john@example.com>".into(),
//!     to: vec!["doe@example.com".into()],
//!     subject: "Hello, World".into(),
//!     text: b"This is a test e-mail".to_vec(),
//!     ..Default::default()
//! };
//! e.attach_file("test.txt").unwrap();
//!
//! pool.send(&e).unwrap();
//! pool.close();
//! ```

mod email;
mod header;
mod pool;
mod smtp;

pub use email::{
    new_email_from_reader, Attachment, Email, CONTENT_TYPE_HTML, CONTENT_TYPE_MULTIPART_ALT,
    CONTENT_TYPE_MULTIPART_MIXED, CONTENT_TYPE_MULTIPART_RELATED, CONTENT_TYPE_OCTET_STREAM,
    CONTENT_TYPE_PLAIN, HDR_BCC, HDR_CC, HDR_CONTENT_DISPOSITION, HDR_CONTENT_ID,
    HDR_CONTENT_TRANSFER_ENCODING, HDR_CONTENT_TYPE, HDR_DATE, HDR_FROM, HDR_MESSAGE_ID,
    HDR_MIME_VERSION, HDR_REPLY_TO, HDR_SUBJECT, HDR_TO, MAX_LINE_LENGTH,
};
pub use header::MimeHeader;
pub use pool::Pool;
pub use smtp::{Auth, AuthMechanism, Opt};

use std::fmt;

/// The kind of SSL/TLS to use for an SMTP connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SslType {
    /// Plain, unencrypted connection.
    #[default]
    None,
    /// Implicit SSL/TLS connection (no STARTTLS).
    Tls,
    /// Plain connection upgraded via STARTTLS.
    StartTls,
}

/// Errors produced by this crate.
#[derive(Debug)]
pub enum Error {
    /// An e-mail construction error (e.g. inconsistent attachments).
    Email(String),
    /// An address failed to parse.
    Address(String),
    /// A message failed to parse.
    Parse(String),
    /// An underlying I/O error.
    Io(std::io::Error),
    /// An error from the SMTP transaction or connection.
    Smtp(SmtpError),
    /// The pool has been closed.
    PoolClosed,
    /// Timed out waiting for a free connection in the pool.
    PoolTimeout,
    /// Invalid pool configuration.
    Config(String),
}

/// Details of an SMTP-layer error, carrying enough classification to drive the
/// pool's retry and connection-reuse decisions.
#[derive(Debug, Clone)]
pub struct SmtpError {
    /// Human-readable message.
    pub message: String,
    /// The SMTP response code, if the server responded with one (e.g. 421, 550).
    pub code: Option<u16>,
    /// Whether the error is network-related and hence retriable.
    pub retriable: bool,
}

impl fmt::Display for SmtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(c) => write!(f, "smtp error {c}: {}", self.message),
            None => write!(f, "smtp error: {}", self.message),
        }
    }
}

impl Error {
    /// Whether this error is retriable (network-related). Mirrors Go's
    /// `canRetry`.
    pub fn is_retriable(&self) -> bool {
        matches!(self, Error::Smtp(e) if e.retriable)
    }

    /// Whether a connection that produced this error should be discarded rather
    /// than returned to the pool. Mirrors Go's `returnConn`: network/non-SMTP
    /// errors and SMTP 421 (rate-limit) close the connection; other SMTP
    /// response errors (e.g. 550) keep it (after RSET).
    pub(crate) fn should_close_conn(&self) -> bool {
        match self {
            Error::Smtp(e) => !matches!(e.code, Some(code) if code != 421),
            _ => true,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Email(m) => write!(f, "{m}"),
            Error::Address(m) => write!(f, "{m}"),
            Error::Parse(m) => write!(f, "{m}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Smtp(e) => write!(f, "{e}"),
            Error::PoolClosed => write!(f, "pool closed"),
            Error::PoolTimeout => write!(f, "timed out waiting for free conn in pool"),
            Error::Config(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
