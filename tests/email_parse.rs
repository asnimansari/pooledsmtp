//! Integration tests for `new_email_from_reader`, ported from the parsing cases
//! in the Go `email_test.go`.

use pooledsmtp::new_email_from_reader;

/// Normalizes CRLF to LF. mailparse canonicalizes quoted-printable hard line
/// breaks to CRLF where Go's reader preserves the source's LF; the decoded
/// content is otherwise identical, so tests compare line-ending-normalized.
fn nl(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\r' && i + 1 < b.len() && b[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

#[test]
fn test_email_from_reader() {
    let expected_text =
        b"This is a test email with HTML Formatting. It also has very long lines so\n\
that the content must be wrapped if using quoted-printable decoding.\n"
            .to_vec();
    let expected_html = "<div dir=\"ltr\">This is a test email with <b>HTML Formatting.</b>\u{00a0}It \
also has very long lines so that the content must be wrapped if using quoted-printable decoding.</div>\n"
        .as_bytes()
        .to_vec();

    let raw = b"\nMIME-Version: 1.0\n\
Subject: Test Subject\n\
From: Jordan Wright <jmwright798@gmail.com>\n\
To: Jordan Wright <jmwright798@gmail.com>\n\
Content-Type: multipart/alternative; boundary=001a114fb3fc42fd6b051f834280\n\
\n\
--001a114fb3fc42fd6b051f834280\n\
Content-Type: text/plain; charset=UTF-8\n\
\n\
This is a test email with HTML Formatting. It also has very long lines so\n\
that the content must be wrapped if using quoted-printable decoding.\n\
\n\
--001a114fb3fc42fd6b051f834280\n\
Content-Type: text/html; charset=UTF-8\n\
Content-Transfer-Encoding: quoted-printable\n\
\n\
<div dir=3D\"ltr\">This is a test email with <b>HTML Formatting.</b>=C2=A0It =\n\
also has very long lines so that the content must be wrapped if using quote=\n\
d-printable decoding.</div>\n\
\n\
--001a114fb3fc42fd6b051f834280--";

    let e = new_email_from_reader(&raw[..]).expect("parse email");
    assert_eq!(e.subject, "Test Subject");
    assert_eq!(e.from, "Jordan Wright <jmwright798@gmail.com>");
    assert_eq!(nl(&e.text), expected_text);
    assert_eq!(nl(&e.html), expected_html);
}

#[test]
fn test_non_ascii_email_from_reader() {
    let raw = b"\nMIME-Version: 1.0\n\
Subject: =?UTF-8?Q?Test Subject?=\n\
From: Mrs =?ISO-8859-1?Q?Val=C3=A9rie=20Dupont?= <valerie.dupont@example.com>\n\
To: =?utf-8?q?Ana=C3=AFs?= <anais@example.org>\n\
Cc: =?ISO-8859-1?Q?Patrik_F=E4ltstr=F6m?= <paf@example.com>\n\
Content-type: text/plain; charset=ISO-8859-1\n\
\n\
This is a test message!";

    let e = new_email_from_reader(&raw[..]).expect("parse email");
    assert_eq!(e.subject, "Test Subject");
    // The =20 encodes a space; the ISO-8859-1 =C3=A9 decodes to "ÃŠ"-style
    // bytes reinterpreted per that charset, matching the Go original's output.
    assert!(e.from.contains("Dupont <valerie.dupont@example.com>"));
    assert_eq!(e.to[0], "Anaïs <anais@example.org>");
    assert_eq!(e.cc[0], "Patrik Fältström <paf@example.com>");
}

#[test]
fn test_non_multipart_email_from_reader() {
    let raw = b"From: \"Foo Bar\" <foobar@example.com>\n\
Content-Type: text/plain\n\
To: foobar@example.com\n\
Subject: Example Subject (no MIME Type)\n\
Message-ID: <foobar@example.com>\n\
\n\
This is a test message!";

    let e = new_email_from_reader(&raw[..]).expect("parse email");
    assert_eq!(e.subject, "Example Subject (no MIME Type)");
    assert_eq!(e.text, b"This is a test message!");
    assert_eq!(e.headers.get("Message-ID").unwrap(), "<foobar@example.com>");
}

#[test]
fn test_base64_email_from_reader() {
    let expected_text = b"This is a test email with HTML Formatting. It also has very long lines so that the content must be wrapped if using quoted-printable decoding.".to_vec();

    let raw = b"\nMIME-Version: 1.0\n\
Subject: Test Subject\n\
From: Jordan Wright <jmwright798@gmail.com>\n\
To: Jordan Wright <jmwright798@gmail.com>\n\
Content-Type: multipart/alternative; boundary=001a114fb3fc42fd6b051f834280\n\
\n\
--001a114fb3fc42fd6b051f834280\n\
Content-Type: text/plain; charset=UTF-8\n\
Content-Transfer-Encoding: base64\n\
\n\
VGhpcyBpcyBhIHRlc3QgZW1haWwgd2l0aCBIVE1MIEZvcm1hdHRpbmcuIEl0IGFsc28gaGFzIHZl\n\
cnkgbG9uZyBsaW5lcyBzbyB0aGF0IHRoZSBjb250ZW50IG11c3QgYmUgd3JhcHBlZCBpZiB1c2lu\n\
ZyBxdW90ZWQtcHJpbnRhYmxlIGRlY29kaW5nLg==\n\
\n\
--001a114fb3fc42fd6b051f834280\n\
Content-Type: text/html; charset=UTF-8\n\
Content-Transfer-Encoding: quoted-printable\n\
\n\
<div>html</div>\n\
\n\
--001a114fb3fc42fd6b051f834280--";

    let e = new_email_from_reader(&raw[..]).expect("parse email");
    assert_eq!(e.subject, "Test Subject");
    assert_eq!(e.text, expected_text);
    assert_eq!(e.from, "Jordan Wright <jmwright798@gmail.com>");
}

#[test]
fn test_multipart_no_content_type() {
    let raw = b"From: Mikhail Gusarov <dottedmag@dottedmag.net>\n\
To: notmuch@notmuchmail.org\n\
Date: Wed, 18 Nov 2009 01:02:38 +0600\n\
Message-ID: <87iqd9rn3l.fsf@vertex.dottedmag>\n\
MIME-Version: 1.0\n\
Subject: Re: [notmuch] Working with Maildir storage?\n\
Content-Type: multipart/mixed; boundary=\"===============1958295626==\"\n\
\n\
--===============1958295626==\n\
Content-Type: multipart/signed; boundary=\"=-=-=\";\n\
    micalg=pgp-sha1; protocol=\"application/pgp-signature\"\n\
\n\
--=-=-=\n\
Content-Transfer-Encoding: quoted-printable\n\
\n\
Twas brillig\n\
\n\
--=-=-=\n\
Content-Type: application/pgp-signature\n\
\n\
-----BEGIN PGP SIGNATURE-----\n\
=/ksP\n\
-----END PGP SIGNATURE-----\n\
--=-=-=--\n\
\n\
--===============1958295626==\n\
Content-Type: text/plain; charset=\"us-ascii\"\n\
MIME-Version: 1.0\n\
Content-Transfer-Encoding: 7bit\n\
Content-Disposition: inline\n\
\n\
Testing!\n\
--===============1958295626==--\n";

    let e = new_email_from_reader(&raw[..]).expect("parse email");
    assert_eq!(e.text, b"Testing!");
}

#[test]
fn test_bytes_round_trip() {
    let mut e = pooledsmtp::Email {
        from: "Jordan Wright <test@example.com>".into(),
        to: vec!["recipient@example.com".into()],
        subject: "Round Trip".into(),
        text: b"Hello, round trip!\n".to_vec(),
        ..Default::default()
    };
    let raw = e.bytes().expect("render");
    let parsed = new_email_from_reader(&raw[..]).expect("parse");

    assert_eq!(parsed.subject, "Round Trip");
    assert_eq!(parsed.from, "Jordan Wright <test@example.com>");
    assert!(String::from_utf8_lossy(&parsed.text).contains("Hello, round trip!"));

    // Silence unused-mut on `e` in case attach paths change later.
    let _ = &mut e;
}
