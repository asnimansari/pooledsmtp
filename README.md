# pooledsmtp (Rust)

A pool of reusable SMTP connections for high-throughput e-mailing, with graceful
handling of idle connections, timeouts, and retries. The e-mail
formatting/parsing is forked from `jordan-wright/email`; the SMTP wire protocol
and TLS come from [`lettre`](https://crates.io/crates/lettre)'s low-level
`SmtpConnection`.

## Design

This is a faithful, behavior-preserving port (a "hybrid" approach): the
connection **pool** and the MIME **`Email::bytes`** serialization are ported by
hand, while `lettre` supplies the raw SMTP protocol and TLS. Notable behaviors
preserved from the Go original:

- **Connection reuse rule**: after a failed send, a network error or SMTP `421`
  (rate-limit) closes the connection; other SMTP errors (e.g. `550`) issue
  `RSET` and return the connection to the pool.
- **Retries**: messages are retried up to `max_message_retries` on network-type
  errors, with an optional `message_retry_delay`.
- **Lazy growth** up to `max_conns`, a background idle sweeper, and a single
  `pool_wait_timeout` used for dialing, borrowing, and returning connections.

## Usage

```rust,no_run
use std::time::Duration;
use pooledsmtp::{Email, Opt, Pool, SslType};

let pool = Pool::new(Opt {
    host: "localhost".into(),
    port: 1025,
    max_conns: 10,
    idle_timeout: Duration::from_secs(10),
    pool_wait_timeout: Duration::from_secs(3),
    ssl: SslType::None,
    ..Default::default()
})
.unwrap();

let mut e = Email {
    from: "John Doe <john@example.com>".into(),
    to: vec!["doe@example.com".into()],
    cc: vec!["doecc@example.com".into()],
    subject: "Hello, World".into(),
    text: b"This is a test e-mail".to_vec(),
    html: b"<strong>This is a test e-mail</strong>".to_vec(),
    ..Default::default()
};
e.attach_file("test.txt").unwrap();

pool.send(&e).unwrap();
pool.close();
```

## Tests

```
cargo test
```

The tests are self-contained: the pure e-mail/MIME cases run in-process, and the
connection/pool tests run against a minimal embedded SMTP sink (standing in for
MailHog), so no external SMTP server is required.

Licensed under the MIT license.
