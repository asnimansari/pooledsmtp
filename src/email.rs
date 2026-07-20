//! The e-mail message model and MIME serialization.
//!
//! This is a Rust port of the `email.go` half of the Go library, which was
//! itself forked from `jordan-wright/email` (MIT). `Email::bytes` renders an
//! [`Email`] into an RFC 5322 / MIME message; the structure it produces
//! (multipart/mixed > multipart/alternative > single part, with
//! multipart/related for HTML-inline attachments) matches the Go original.

use std::io::Read;
use std::path::Path;

use base64::Engine;
use mailparse::ParsedMail;

use crate::header::{canonicalize, MimeHeader};
use crate::Error;

// Content types.
pub const CONTENT_TYPE_PLAIN: &str = "text/plain";
pub const CONTENT_TYPE_HTML: &str = "text/html";
pub const CONTENT_TYPE_OCTET_STREAM: &str = "application/octet-stream";
pub const CONTENT_TYPE_MULTIPART_ALT: &str = "multipart/alternative";
pub const CONTENT_TYPE_MULTIPART_MIXED: &str = "multipart/mixed";
pub const CONTENT_TYPE_MULTIPART_RELATED: &str = "multipart/related";

/// Maximum line length per RFC 2045.
pub const MAX_LINE_LENGTH: usize = 76;

// SMTP header field names.
pub const HDR_CONTENT_TYPE: &str = "Content-Type";
pub const HDR_SUBJECT: &str = "Subject";
pub const HDR_TO: &str = "To";
pub const HDR_CC: &str = "Cc";
pub const HDR_BCC: &str = "Bcc";
pub const HDR_FROM: &str = "From";
pub const HDR_REPLY_TO: &str = "Reply-To";
pub const HDR_DATE: &str = "Date";
pub const HDR_MESSAGE_ID: &str = "Message-Id";
pub const HDR_MIME_VERSION: &str = "MIME-Version";
pub const HDR_CONTENT_TRANSFER_ENCODING: &str = "Content-Transfer-Encoding";
pub const HDR_CONTENT_DISPOSITION: &str = "Content-Disposition";
pub const HDR_CONTENT_ID: &str = "Content-ID";

const DEFAULT_CHAR_ENCODING: &str = "UTF-8";
const DEFAULT_MIME_VERSION: &str = "1.0";
const DEFAULT_HOSTNAME: &str = "localhost.localdomain";

const CONTENT_ENC_BASE64: &str = "base64";
const CONTENT_ENC_QUOTED_PRINTABLE: &str = "quoted-printable";

/// Header fields, in the order the Go original pulls them from `Email.Headers`.
const MSG_HEADERS: &[&str] = &[
    HDR_REPLY_TO,
    HDR_TO,
    HDR_CC,
    HDR_FROM,
    HDR_SUBJECT,
    HDR_DATE,
    HDR_MESSAGE_ID,
    HDR_MIME_VERSION,
];

/// An e-mail message.
#[derive(Clone, Debug, Default)]
pub struct Email {
    pub reply_to: Vec<String>,
    pub from: String,
    pub to: Vec<String>,
    pub bcc: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,

    /// Optional plain-text body.
    pub text: Vec<u8>,

    /// Optional HTML body.
    pub html: Vec<u8>,

    /// Overrides `from` as the SMTP envelope sender (optional).
    pub sender: String,

    pub headers: MimeHeader,
    pub attachments: Vec<Attachment>,
    pub read_receipt: Vec<String>,
}

/// An e-mail attachment: a filename, MIME header, and content.
#[derive(Clone, Debug, Default)]
pub struct Attachment {
    pub filename: String,
    pub header: MimeHeader,
    pub content: Vec<u8>,
    /// If true, the attachment is embedded via `multipart/related` (e.g. an
    /// image referenced inline by the HTML body) rather than as a normal
    /// `multipart/mixed` attachment.
    pub html_related: bool,
}

impl Email {
    /// Attaches content read from `r`, using `filename` and `content_type`
    /// (falling back to `application/octet-stream` when empty). The created
    /// attachment is appended to `self.attachments` and also returned.
    ///
    /// As in the Go original, the returned value is a copy: mutating it (e.g.
    /// setting `html_related`) does not affect the stored attachment.
    pub fn attach<R: Read>(
        &mut self,
        mut r: R,
        filename: &str,
        content_type: &str,
    ) -> Result<Attachment, Error> {
        let mut content = Vec::new();
        r.read_to_end(&mut content)?;

        let mut header = MimeHeader::new();
        if !content_type.is_empty() {
            header.set(HDR_CONTENT_TYPE, content_type);
        } else {
            header.set(HDR_CONTENT_TYPE, CONTENT_TYPE_OCTET_STREAM);
        }
        header.set(
            HDR_CONTENT_DISPOSITION,
            &format!("attachment;\r\n filename=\"{filename}\""),
        );
        header.set(HDR_CONTENT_ID, &format!("<{filename}>"));
        header.set(HDR_CONTENT_TRANSFER_ENCODING, CONTENT_ENC_BASE64);

        let at = Attachment {
            filename: filename.to_string(),
            header,
            content,
            html_related: false,
        };
        self.attachments.push(at.clone());
        Ok(at)
    }

    /// Attaches the file at `path`, inferring the content type from its
    /// extension and using its basename as the filename. Mirrors Go's
    /// `AttachFile`.
    pub fn attach_file<P: AsRef<Path>>(&mut self, path: P) -> Result<Attachment, Error> {
        let path = path.as_ref();
        let f = std::fs::File::open(path)?;
        let ct = content_type_by_extension(path);
        let basename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.attach(f, &basename, &ct)
    }

    fn categorize_attachments(&self) -> (Vec<&Attachment>, Vec<&Attachment>) {
        let mut html_related = Vec::new();
        let mut others = Vec::new();
        for a in &self.attachments {
            if a.html_related {
                html_related.push(a);
            } else {
                others.push(a);
            }
        }
        (html_related, others)
    }

    /// Renders the message to bytes, including all MIME headers, boundaries and
    /// encodings.
    pub fn bytes(&self) -> Result<Vec<u8>, Error> {
        let mut buff: Vec<u8> = Vec::with_capacity(4096);

        let mut headers = self.msg_headers()?;
        let (html_attach, other_attach) = self.categorize_attachments();
        if self.html.is_empty() && !html_attach.is_empty() {
            return Err(Error::Email(
                "there are HTML attachments, but no HTML body".into(),
            ));
        }

        let is_mixed = !other_attach.is_empty();
        let is_alternative = !self.text.is_empty() && !self.html.is_empty();
        let multipart = is_mixed || is_alternative;

        let mut w: Option<MultipartWriter> = if multipart {
            Some(MultipartWriter::new())
        } else {
            None
        };

        if is_mixed {
            headers.set(
                HDR_CONTENT_TYPE,
                &format!(
                    "{};\r\n boundary={}",
                    CONTENT_TYPE_MULTIPART_MIXED,
                    w.as_ref().unwrap().boundary()
                ),
            );
        } else if is_alternative {
            headers.set(
                HDR_CONTENT_TYPE,
                &format!(
                    "{};\r\n boundary={}",
                    CONTENT_TYPE_MULTIPART_ALT,
                    w.as_ref().unwrap().boundary()
                ),
            );
        } else if !self.html.is_empty() {
            headers.set(
                HDR_CONTENT_TYPE,
                &format!("{CONTENT_TYPE_HTML}; charset={DEFAULT_CHAR_ENCODING}"),
            );
            headers.set(HDR_CONTENT_TRANSFER_ENCODING, CONTENT_ENC_QUOTED_PRINTABLE);
        } else {
            headers.set(
                HDR_CONTENT_TYPE,
                &format!("{CONTENT_TYPE_PLAIN}; charset={DEFAULT_CHAR_ENCODING}"),
            );
            headers.set(HDR_CONTENT_TRANSFER_ENCODING, CONTENT_ENC_QUOTED_PRINTABLE);
        }

        header_to_bytes(&mut buff, &headers);
        buff.extend_from_slice(b"\r\n");

        if !self.text.is_empty() || !self.html.is_empty() {
            // When both mixed and alternative, nest a multipart/alternative
            // part inside the mixed container; otherwise the alternative/mixed
            // writer `w` doubles as the sub-writer.
            let mut sub: Option<MultipartWriter> = None;
            if is_mixed && is_alternative {
                let s = MultipartWriter::new();
                let mut hdr = MimeHeader::new();
                hdr.set(
                    HDR_CONTENT_TYPE,
                    &format!(
                        "{};\r\n boundary={}",
                        CONTENT_TYPE_MULTIPART_ALT,
                        s.boundary()
                    ),
                );
                w.as_mut().unwrap().create_part(&mut buff, &hdr);
                sub = Some(s);
            }

            if !self.text.is_empty() {
                let sw = active_writer(&mut sub, &mut w);
                write_message(&mut buff, &self.text, multipart, CONTENT_TYPE_PLAIN, sw);
            }

            if !self.html.is_empty() {
                let mut related: Option<MultipartWriter> = None;
                if !html_attach.is_empty() {
                    let r = MultipartWriter::new();
                    let mut hdr = MimeHeader::new();
                    hdr.set(
                        HDR_CONTENT_TYPE,
                        &format!(
                            "{};\r\n boundary={}",
                            CONTENT_TYPE_MULTIPART_RELATED,
                            r.boundary()
                        ),
                    );
                    match active_writer(&mut sub, &mut w) {
                        Some(sw) => sw.create_part(&mut buff, &hdr),
                        None => {
                            // Degenerate config (HTML + inline attachment, no
                            // text, no other attachment) that the Go original
                            // panics on; we surface a clean error instead.
                            return Err(Error::Email(
                                "HTML-related attachments require a text part or another attachment"
                                    .into(),
                            ));
                        }
                    }
                    related = Some(r);
                }

                // Message writer is the related writer if present, else the sub.
                match related.as_mut() {
                    Some(rw) => write_message(
                        &mut buff,
                        &self.html,
                        multipart,
                        CONTENT_TYPE_HTML,
                        Some(rw),
                    ),
                    None => {
                        let sw = active_writer(&mut sub, &mut w);
                        write_message(&mut buff, &self.html, multipart, CONTENT_TYPE_HTML, sw)
                    }
                }

                if !html_attach.is_empty() {
                    let rw = related.as_mut().unwrap();
                    for a in &html_attach {
                        rw.create_part(&mut buff, &a.header);
                        base64_wrap(&mut buff, &a.content);
                    }
                    rw.close(&mut buff);
                }
            }

            if is_mixed && is_alternative {
                sub.as_mut().unwrap().close(&mut buff);
            }
        }

        for a in &other_attach {
            w.as_mut().unwrap().create_part(&mut buff, &a.header);
            base64_wrap(&mut buff, &a.content);
        }

        if multipart {
            w.as_mut().unwrap().close(&mut buff);
        }

        Ok(buff)
    }

    /// Merges the message's fields and custom headers into a MIME header,
    /// generating a Message-ID and Date when absent. Does not alter
    /// `self.headers`.
    fn msg_headers(&self) -> Result<MimeHeader, Error> {
        let mut res = MimeHeader::new();

        // Pull recognized headers already present in the user's headers.
        for &h in MSG_HEADERS {
            if let Some(vals) = self.headers.get_all(h) {
                for v in vals {
                    res.add(h, v);
                }
            }
        }

        if !res.contains(HDR_REPLY_TO) && !self.reply_to.is_empty() {
            res.set(HDR_REPLY_TO, &format_addresses(&self.reply_to)?.join(", "));
        }
        if !res.contains(HDR_TO) && !self.to.is_empty() {
            res.set(HDR_TO, &format_addresses(&self.to)?.join(", "));
        }
        if !res.contains(HDR_CC) && !self.cc.is_empty() {
            res.set(HDR_CC, &format_addresses(&self.cc)?.join(", "));
        }
        if !res.contains(HDR_SUBJECT) && !self.subject.is_empty() {
            res.set(HDR_SUBJECT, &self.subject);
        }
        if !res.contains(HDR_MESSAGE_ID) {
            res.set(HDR_MESSAGE_ID, &generate_message_id()?);
        }
        if !res.contains(HDR_FROM) {
            res.set(HDR_FROM, &format_address(&self.from)?);
        }
        if !res.contains(HDR_DATE) {
            res.set(
                HDR_DATE,
                &chrono::Local::now()
                    .format("%a, %d %b %Y %H:%M:%S %z")
                    .to_string(),
            );
        }
        if !res.contains(HDR_MIME_VERSION) {
            res.set(HDR_MIME_VERSION, DEFAULT_MIME_VERSION);
        }

        // Copy over any remaining custom headers not already set.
        for (field, vals) in self.headers.iter() {
            if !res.contains(field) {
                for v in vals {
                    res.add(field, v);
                }
            }
        }

        Ok(res)
    }

    /// Selects and parses the SMTP envelope sender: `sender` if set, else `from`.
    pub(crate) fn parse_sender(&self) -> Result<String, Error> {
        if !self.sender.is_empty() {
            return Ok(parse_address(&self.sender)?.1);
        }
        Ok(parse_address(&self.from)?.1)
    }
}

/// Parses `s` (`"Name <addr>"` or bare `"addr"`) and returns the bare address.
/// Used by the SMTP send path to build the envelope recipients. Mirrors Go's
/// use of `mail.ParseAddress` in `combineEmails`.
pub(crate) fn extract_address(s: &str) -> Result<String, Error> {
    Ok(parse_address(s)?.1)
}

/// Returns the active sub-writer: the nested alternative writer if present,
/// otherwise the top-level writer.
fn active_writer<'a>(
    sub: &'a mut Option<MultipartWriter>,
    w: &'a mut Option<MultipartWriter>,
) -> Option<&'a mut MultipartWriter> {
    if sub.is_some() {
        sub.as_mut()
    } else {
        w.as_mut()
    }
}

/// A minimal multipart writer mirroring Go's `mime/multipart.Writer`. Because
/// all writers in `Email::bytes` share a single output buffer, the buffer is
/// passed to each method rather than owned by the writer.
struct MultipartWriter {
    boundary: String,
    first: bool,
}

impl MultipartWriter {
    fn new() -> Self {
        MultipartWriter {
            boundary: random_boundary(),
            first: true,
        }
    }

    fn boundary(&self) -> &str {
        &self.boundary
    }

    /// Writes a part delimiter and the given header, matching Go's
    /// `Writer.CreatePart` (headers emitted in sorted key order).
    fn create_part(&mut self, buf: &mut Vec<u8>, header: &MimeHeader) {
        if self.first {
            buf.extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        } else {
            buf.extend_from_slice(format!("\r\n--{}\r\n", self.boundary).as_bytes());
        }
        self.first = false;

        let mut keys: Vec<(&String, &Vec<String>)> = header.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        for (field, vals) in keys {
            for v in vals {
                buf.extend_from_slice(format!("{field}: {v}\r\n").as_bytes());
            }
        }
        buf.extend_from_slice(b"\r\n");
    }

    fn close(&mut self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(format!("\r\n--{}--\r\n", self.boundary).as_bytes());
    }
}

fn random_boundary() -> String {
    use rand::RngCore;
    let mut b = [0u8; 30];
    rand::thread_rng().fill_bytes(&mut b);
    let mut s = String::with_capacity(60);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Writes `msg` as a message body, optionally as a new multipart part, encoding
/// the body as quoted-printable. Mirrors Go's `writeMessage`.
fn write_message(
    buff: &mut Vec<u8>,
    msg: &[u8],
    multipart: bool,
    media_type: &str,
    w: Option<&mut MultipartWriter>,
) {
    if multipart {
        let mut header = MimeHeader::new();
        header.set(
            HDR_CONTENT_TYPE,
            &format!("{media_type}; charset={DEFAULT_CHAR_ENCODING}"),
        );
        header.set(HDR_CONTENT_TRANSFER_ENCODING, CONTENT_ENC_QUOTED_PRINTABLE);
        // `multipart` is only true when a writer exists.
        w.expect("multipart write without a writer")
            .create_part(buff, &header);
    }
    qp_encode(buff, msg);
}

/// Formats an address as a valid RFC 5322 address, RFC 2047-encoding the
/// display name if it contains non-ASCII characters. Mirrors Go's
/// `formatAddress`/`mail.Address.String`.
fn format_address(addr: &str) -> Result<String, Error> {
    let (name, email) = parse_address(addr)?;
    if name.is_empty() {
        return Ok(format!("<{email}>"));
    }
    Ok(format!("{} <{}>", encode_display_name(&name), email))
}

fn format_addresses(addrs: &[String]) -> Result<Vec<String>, Error> {
    addrs.iter().map(|a| format_address(a)).collect()
}

/// Parses `"Name <local@domain>"` or a bare `"local@domain"` into
/// `(name, address)`, validating that the address has non-empty local and
/// domain parts around a single `@`. Mirrors the parts of Go's
/// `net/mail.ParseAddress` that this library relies on.
fn parse_address(s: &str) -> Result<(String, String), Error> {
    let s = s.trim();
    let (name, addr) = if let Some(open) = s.rfind('<') {
        let rest = &s[open + 1..];
        let close = rest
            .find('>')
            .ok_or_else(|| Error::Address(format!("invalid address: {s}")))?;
        let addr = rest[..close].trim().to_string();
        let name = s[..open].trim().trim_matches('"').trim().to_string();
        (name, addr)
    } else {
        (String::new(), s.to_string())
    };

    if !is_valid_addr(&addr) {
        return Err(Error::Address(format!("invalid address: {s}")));
    }
    Ok((name, addr))
}

fn is_valid_addr(addr: &str) -> bool {
    let mut parts = addr.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => !local.is_empty() && !domain.is_empty(),
        _ => false,
    }
}

/// Encodes a display name: left as-is if a safe ASCII token, quoted if it
/// contains ASCII specials, RFC 2047 Q-encoded if it contains non-ASCII.
fn encode_display_name(name: &str) -> String {
    if name.is_ascii() {
        if name.bytes().all(is_atext_or_space) {
            name.to_string()
        } else {
            format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
        }
    } else {
        q_encode_word(name)
    }
}

fn is_atext_or_space(b: u8) -> bool {
    b == b' ' || b.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&b)
}

/// RFC 2047 "Q" encoded-word for a UTF-8 string, as a single word.
fn q_encode_word(s: &str) -> String {
    let mut enc = String::from("=?utf-8?q?");
    for &b in s.as_bytes() {
        match b {
            b' ' => enc.push('_'),
            b if b.is_ascii_alphanumeric() => enc.push(b as char),
            _ => enc.push_str(&format!("={b:02X}")),
        }
    }
    enc.push_str("?=");
    enc
}

/// Renders `header` to `buff`. Content-Type and Content-Disposition values are
/// written verbatim; all others are RFC 2047 Q-encoded if they need it.
fn header_to_bytes(buff: &mut Vec<u8>, header: &MimeHeader) {
    for (field, vals) in header.iter() {
        for subval in vals {
            buff.extend_from_slice(field.as_bytes());
            buff.extend_from_slice(b": ");
            if field == HDR_CONTENT_TYPE || field == HDR_CONTENT_DISPOSITION {
                buff.extend_from_slice(subval.as_bytes());
            } else {
                buff.extend_from_slice(q_encode_if_needed(subval).as_bytes());
            }
            buff.extend_from_slice(b"\r\n");
        }
    }
}

/// Q-encodes the whole value as one encoded-word if it contains bytes outside
/// printable ASCII (tab excepted); otherwise returns it unchanged. Mirrors Go's
/// `mime.QEncoding.Encode` behavior for the short header values this library
/// produces.
fn q_encode_if_needed(s: &str) -> String {
    let needs = s.bytes().any(|b| !(b' '..=b'~').contains(&b) && b != b'\t');
    if needs {
        q_encode_word(s)
    } else {
        s.to_string()
    }
}

/// Base64-encodes `b`, wrapping at 76 characters per RFC 2045, writing to
/// `buff`. Mirrors Go's `base64Wrap`.
fn base64_wrap(buff: &mut Vec<u8>, b: &[u8]) {
    // 57 raw bytes encode to exactly 76 base64 chars.
    const MAX_RAW: usize = 57;
    let mut b = b;
    let engine = base64::engine::general_purpose::STANDARD;
    while b.len() >= MAX_RAW {
        buff.extend_from_slice(engine.encode(&b[..MAX_RAW]).as_bytes());
        buff.extend_from_slice(b"\r\n");
        b = &b[MAX_RAW..];
    }
    if !b.is_empty() {
        buff.extend_from_slice(engine.encode(b).as_bytes());
        buff.extend_from_slice(b"\r\n");
    }
}

/// Quoted-printable encodes `data` into `out`, converting bare line breaks to
/// CRLF, escaping `=` and non-printable bytes as `=XX`, encoding trailing
/// whitespace, and soft-wrapping at 76 columns. Matches the observable output
/// of Go's `mime/quotedprintable.Writer` for the message bodies this library
/// produces.
fn qp_encode(out: &mut Vec<u8>, data: &[u8]) {
    let mut line_len = 0usize;
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];

        // Hard line breaks: normalize CR, CRLF and LF to CRLF.
        if b == b'\r' {
            out.extend_from_slice(b"\r\n");
            line_len = 0;
            if i + 1 < data.len() && data[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if b == b'\n' {
            out.extend_from_slice(b"\r\n");
            line_len = 0;
            i += 1;
            continue;
        }

        let is_printable = (0x21..=0x7e).contains(&b) && b != b'=';
        let is_ws = b == b' ' || b == b'\t';

        if is_printable {
            if line_len >= MAX_LINE_LENGTH - 1 {
                out.extend_from_slice(b"=\r\n");
                line_len = 0;
            }
            out.push(b);
            line_len += 1;
            i += 1;
        } else if is_ws {
            let next_is_break = i + 1 >= data.len() || data[i + 1] == b'\n' || data[i + 1] == b'\r';
            if next_is_break {
                if line_len >= MAX_LINE_LENGTH - 3 {
                    out.extend_from_slice(b"=\r\n");
                    line_len = 0;
                }
                out.extend_from_slice(format!("={b:02X}").as_bytes());
                line_len += 3;
                i += 1;
            } else {
                if line_len >= MAX_LINE_LENGTH - 1 {
                    out.extend_from_slice(b"=\r\n");
                    line_len = 0;
                }
                out.push(b);
                line_len += 1;
                i += 1;
            }
        } else {
            if line_len >= MAX_LINE_LENGTH - 3 {
                out.extend_from_slice(b"=\r\n");
                line_len = 0;
            }
            out.extend_from_slice(format!("={b:02X}").as_bytes());
            line_len += 3;
            i += 1;
        }
    }
}

/// Generates an RFC 2822-compliant Message-ID:
/// `<{unix_nanos}.{pid}.{rand_i64}@{hostname}>`, falling back to
/// `localhost.localdomain` when the hostname is missing or not an FQDN.
fn generate_message_id() -> Result<String, Error> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let rint: i64 = rand::random::<i64>().abs();

    let h = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| h.contains('.'))
        .unwrap_or_else(|| DEFAULT_HOSTNAME.to_string());

    Ok(format!("<{nanos}.{pid}.{rint}@{h}>"))
}

/// Reads an RFC 5322 message from `r` and parses it into an [`Email`]. Leading
/// whitespace is trimmed, the Subject/To/Cc/Bcc/From headers are decoded and
/// lifted onto the struct fields, remaining headers are retained, and the MIME
/// body parts are distributed into `text`, `html` and `attachments`. Mirrors
/// Go's `NewEmailFromReader`.
pub fn new_email_from_reader<R: Read>(mut r: R) -> Result<Email, Error> {
    let mut raw = Vec::new();
    r.read_to_end(&mut raw)?;

    // Trim any leading whitespace (mirrors trimReader).
    let start = raw
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(raw.len());
    let raw = &raw[start..];

    let parsed = mailparse::parse_mail(raw).map_err(|e| Error::Parse(e.to_string()))?;

    let mut e = Email {
        headers: MimeHeader::new(),
        ..Default::default()
    };

    // Distribute the top-level headers. Addressing headers are lifted onto the
    // struct fields (decoded); everything else is retained in `headers`.
    for h in &parsed.headers {
        let key = h.get_key();
        let val = h.get_value();
        match canonicalize(&key).as_str() {
            HDR_SUBJECT => e.subject = val,
            HDR_TO => e.to.push(val),
            HDR_CC => e.cc.push(val),
            HDR_BCC => e.bcc.push(val),
            HDR_FROM => e.from = val,
            _ => e.headers.add(&key, &val),
        }
    }

    distribute_parts(&parsed, &mut e, false);
    Ok(e)
}

/// Recursively walks parsed MIME parts, assigning text/plain to `text`,
/// text/html to `html`, and everything else as an attachment. Matches the flat
/// distribution done by Go's `parseMIMEParts` + `NewEmailFromReader`.
///
/// `in_multipart` is true for parts nested inside a multipart container. For
/// those, the single trailing line-ending that RFC 2046 attaches to the
/// following boundary delimiter is stripped, matching Go's `mime/multipart`
/// reader (mailparse retains it).
fn distribute_parts(p: &ParsedMail, e: &mut Email, in_multipart: bool) {
    if p.subparts.is_empty() {
        let ct = p.ctype.mimetype.as_str();
        let mut body = p.get_body_raw().unwrap_or_default();
        if in_multipart {
            strip_one_line_ending(&mut body);
        }
        match ct {
            CONTENT_TYPE_PLAIN => e.text = body,
            CONTENT_TYPE_HTML => e.html = body,
            _ => {
                let filename = p
                    .get_content_disposition()
                    .params
                    .get("filename")
                    .cloned()
                    .unwrap_or_default();
                let mut header = MimeHeader::new();
                for h in &p.headers {
                    header.add(&h.get_key(), &h.get_value());
                }
                e.attachments.push(Attachment {
                    filename,
                    header,
                    content: body,
                    html_related: false,
                });
            }
        }
    } else {
        for sp in &p.subparts {
            distribute_parts(sp, e, true);
        }
    }
}

/// Removes a single trailing line-ending (`\r\n`, `\n`, or `\r`) from `body`,
/// if present.
fn strip_one_line_ending(body: &mut Vec<u8>) {
    if body.last() == Some(&b'\n') {
        body.pop();
        if body.last() == Some(&b'\r') {
            body.pop();
        }
    } else if body.last() == Some(&b'\r') {
        body.pop();
    }
}

/// Returns a content type for a path based on its extension, or an empty string
/// (which makes `attach` fall back to `application/octet-stream`). A small
/// built-in table stands in for Go's `mime.TypeByExtension`.
fn content_type_by_extension(path: &Path) -> String {
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ct = match ext.as_str() {
        "txt" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "json" => "application/json",
        "xml" => "text/xml; charset=utf-8",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        _ => "",
    };
    ct.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_wrap() {
        let file = "I'm a file long enough to force the function to wrap a\n\
                    couple of lines, but I stop short of the end of one line and\n\
                    have some padding dangling at the end.";
        let expected = "SSdtIGEgZmlsZSBsb25nIGVub3VnaCB0byBmb3JjZSB0aGUgZnVuY3Rpb24gdG8gd3JhcCBhCmNv\r\n\
                        dXBsZSBvZiBsaW5lcywgYnV0IEkgc3RvcCBzaG9ydCBvZiB0aGUgZW5kIG9mIG9uZSBsaW5lIGFu\r\n\
                        ZApoYXZlIHNvbWUgcGFkZGluZyBkYW5nbGluZyBhdCB0aGUgZW5kLg==\r\n";
        let mut buf = Vec::new();
        base64_wrap(&mut buf, file.as_bytes());
        assert_eq!(String::from_utf8(buf).unwrap(), expected);
    }

    #[test]
    fn test_parse_sender() {
        // (email, want, has_err)
        let cases: &[(Email, &str, bool)] = &[
            (
                Email {
                    from: "from@test.com".into(),
                    ..Default::default()
                },
                "from@test.com",
                false,
            ),
            (
                Email {
                    sender: "sender@test.com".into(),
                    from: "from@test.com".into(),
                    ..Default::default()
                },
                "sender@test.com",
                false,
            ),
            (
                Email {
                    sender: "bad_address_sender".into(),
                    ..Default::default()
                },
                "",
                true,
            ),
            (
                Email {
                    sender: "good@sender.com".into(),
                    from: "bad_address_from".into(),
                    ..Default::default()
                },
                "good@sender.com",
                false,
            ),
        ];

        for (i, (e, want, has_err)) in cases.iter().enumerate() {
            match e.parse_sender() {
                Ok(got) => {
                    assert!(!*has_err, "case {}: expected error, got {got}", i + 1);
                    assert_eq!(&got, want, "case {}", i + 1);
                }
                Err(_) => assert!(*has_err, "case {}: unexpected error", i + 1),
            }
        }
    }

    #[test]
    fn test_qp_encode_body() {
        // A body ending in a bare LF becomes CRLF; printable ASCII is untouched.
        let mut out = Vec::new();
        qp_encode(&mut out, b"Text Body is, of course, supported!\n");
        assert_eq!(out, b"Text Body is, of course, supported!\r\n");
    }

    #[test]
    fn test_format_address() {
        assert_eq!(
            format_address("Jordan Wright <test@example.com>").unwrap(),
            "Jordan Wright <test@example.com>"
        );
        assert_eq!(
            format_address("test_cc@example.com").unwrap(),
            "<test_cc@example.com>"
        );
        // Non-ASCII display name gets RFC 2047 Q-encoded.
        assert_eq!(
            format_address("Bécassine <test@example.com>").unwrap(),
            "=?utf-8?q?B=C3=A9cassine?= <test@example.com>"
        );
        assert!(format_address("not-an-address").is_err());
    }
}
