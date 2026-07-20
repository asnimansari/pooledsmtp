//! Integration tests for `Email::bytes`, ported from the pure (no-network)
//! cases in the Go `email_test.go`. The rendered message is parsed back with
//! `mailparse` (standing in for Go's `net/mail` + `mime/multipart`) to verify
//! structure, headers and bodies.

use mailparse::{addrparse, parse_mail, MailAddr, MailHeaderMap, ParsedMail};
use pooledsmtp::Email;

fn prepare_email() -> Email {
    Email {
        from: "Jordan Wright <test@example.com>".into(),
        to: vec!["Bécassine <test@example.com>".into()],
        bcc: vec!["test_bcc@example.com".into()],
        cc: vec!["test_cc@example.com".into()],
        subject: "Awesome Subject".into(),
        ..Default::default()
    }
}

/// Extracts `(display_name, address)` from the first address in a header value.
fn first_addr(value: &str) -> (String, String) {
    let list = addrparse(value).expect("addrparse");
    match list.first().expect("at least one address") {
        MailAddr::Single(info) => (
            info.display_name.clone().unwrap_or_default(),
            info.addr.clone(),
        ),
        MailAddr::Group(_) => panic!("unexpected group address"),
    }
}

/// Collects the MIME types of all leaf parts (parts with no subparts).
fn leaf_mimetypes(parsed: &ParsedMail) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(p: &ParsedMail, out: &mut Vec<String>) {
        if p.subparts.is_empty() {
            out.push(p.ctype.mimetype.clone());
        } else {
            for sp in &p.subparts {
                walk(sp, out);
            }
        }
    }
    walk(parsed, &mut out);
    out
}

/// Renders `e`, parses it back, and asserts the basic addressing headers
/// round-trip. Mirrors Go's `basicTests`.
fn basic_tests(e: &Email) -> Vec<u8> {
    let raw = e.bytes().expect("render message");
    {
        let parsed = parse_mail(&raw).expect("parse rendered message");

        let (to_name, to_addr) = first_addr(&parsed.headers.get_first_value("To").unwrap());
        assert_eq!(to_name, "Bécassine");
        assert_eq!(to_addr, "test@example.com");

        let (from_name, from_addr) = first_addr(&parsed.headers.get_first_value("From").unwrap());
        assert_eq!(from_name, "Jordan Wright");
        assert_eq!(from_addr, "test@example.com");

        let (cc_name, cc_addr) = first_addr(&parsed.headers.get_first_value("Cc").unwrap());
        assert_eq!(cc_name, "");
        assert_eq!(cc_addr, "test_cc@example.com");

        assert_eq!(
            parsed.headers.get_first_value("Subject").unwrap(),
            "Awesome Subject"
        );

        // Bcc must never appear in the rendered headers.
        assert!(parsed.headers.get_first_value("Bcc").is_none());
    }
    raw
}

#[test]
fn test_email_text() {
    let mut e = prepare_email();
    e.text = b"Text Body is, of course, supported!\n".to_vec();

    let raw = basic_tests(&e);
    let parsed = parse_mail(&raw).unwrap();
    assert_eq!(parsed.ctype.mimetype, "text/plain");
}

#[test]
fn test_email_html() {
    let mut e = prepare_email();
    e.html = b"<h1>Fancy Html is supported, too!</h1>\n".to_vec();

    let raw = basic_tests(&e);
    let parsed = parse_mail(&raw).unwrap();
    assert_eq!(parsed.ctype.mimetype, "text/html");
}

#[test]
fn test_email_with_html_attachments() {
    let mut e = prepare_email();
    e.text = b"Text Body is, of course, supported!\n".to_vec();
    e.html = b"<html><body>This is a text.</body></html>".to_vec();

    let mut attachment = e
        .attach(
            &b"Rad attachment"[..],
            "rad.txt",
            "image/png; charset=utf-8",
        )
        .expect("attach");
    attachment.html_related = true;

    let raw = e.bytes().expect("render message");
    let parsed = parse_mail(&raw).unwrap();

    let leaves = leaf_mimetypes(&parsed);
    assert_eq!(leaves.len(), 3, "unexpected parts: {leaves:?}");
    assert!(leaves.iter().any(|c| c == "text/plain"));
    assert!(leaves.iter().any(|c| c == "text/html"));
    assert!(leaves.iter().any(|c| c == "image/png"));
}

#[test]
fn test_email_text_attachment() {
    let mut e = prepare_email();
    e.text = b"Text Body is, of course, supported!\n".to_vec();
    e.attach(
        &b"Rad attachment"[..],
        "rad.txt",
        "text/plain; charset=utf-8",
    )
    .expect("attach");

    let raw = basic_tests(&e);
    let parsed = parse_mail(&raw).unwrap();

    assert_eq!(parsed.ctype.mimetype, "multipart/mixed");
    assert!(parsed.ctype.params.contains_key("boundary"));
    assert_eq!(parsed.subparts.len(), 2);

    let text = &parsed.subparts[0];
    assert_eq!(text.ctype.mimetype, "text/plain");
    assert!(text
        .get_body()
        .unwrap()
        .contains("Text Body is, of course, supported!"));
}

#[test]
fn test_email_text_html_attachment() {
    let mut e = prepare_email();
    e.text = b"Text Body is, of course, supported!\n".to_vec();
    e.html = b"<h1>Fancy Html is supported, too!</h1>\n".to_vec();
    e.attach(
        &b"Rad attachment"[..],
        "rad.txt",
        "text/plain; charset=utf-8",
    )
    .expect("attach");

    let raw = basic_tests(&e);
    let parsed = parse_mail(&raw).unwrap();

    assert_eq!(parsed.ctype.mimetype, "multipart/mixed");
    assert_eq!(parsed.subparts.len(), 2);

    let alt = &parsed.subparts[0];
    assert_eq!(alt.ctype.mimetype, "multipart/alternative");
    assert_eq!(alt.subparts.len(), 2);
    assert!(alt.subparts[0]
        .get_body()
        .unwrap()
        .contains("Text Body is, of course, supported!"));
}

#[test]
fn test_email_attachment() {
    let mut e = prepare_email();
    e.attach(
        &b"Rad attachment"[..],
        "rad.txt",
        "text/plain; charset=utf-8",
    )
    .expect("attach");

    let raw = basic_tests(&e);
    let parsed = parse_mail(&raw).unwrap();

    assert_eq!(parsed.ctype.mimetype, "multipart/mixed");
    assert_eq!(parsed.subparts.len(), 1);
}
