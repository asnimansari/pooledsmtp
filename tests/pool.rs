//! Pool integration tests, ported from the Go `pool_test.go`. They run against
//! an in-process SMTP sink (see `common`) instead of MailHog.

mod common;

use std::sync::Arc;
use std::time::Duration;

use pooledsmtp::{Email, Opt, Pool, SslType};

fn opt(port: u16, max_conns: usize) -> Opt {
    Opt {
        host: "127.0.0.1".into(),
        port,
        max_conns,
        pool_wait_timeout: Duration::from_secs(2),
        ssl: SslType::None,
        ..Default::default()
    }
}

#[test]
fn test_send_email() {
    let sink = common::start();
    let pool = Pool::new(opt(sink.port, 3)).expect("create pool");

    let email = Email {
        from: "sender@example.com".into(),
        to: vec!["recipient@example.com".into()],
        subject: "Test Subject".into(),
        text: b"Test Body".to_vec(),
        ..Default::default()
    };

    pool.send(&email).expect("send email");

    let msgs = sink.received();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].contains("recipient@example.com"));
    pool.close();
}

#[test]
fn test_connection_pooling() {
    let sink = common::start();
    let pool = Arc::new(Pool::new(opt(sink.port, 2)).expect("create pool"));

    // Send more emails than the pool size, concurrently.
    let mut handles = Vec::new();
    for i in 0..5 {
        let pool = pool.clone();
        handles.push(std::thread::spawn(move || {
            let email = Email {
                from: format!("sender{i}@example.com"),
                to: vec!["recipient@example.com".into()],
                subject: "Concurrent Test".into(),
                text: b"Concurrent Body".to_vec(),
                ..Default::default()
            };
            pool.send(&email).expect("concurrent send");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(sink.count(), 5, "expected 5 messages");
    pool.close();
}

#[test]
fn test_pool_close() {
    let sink = common::start();
    let pool = Pool::new(opt(sink.port, 1)).expect("create pool");

    pool.close();

    let res = pool.send(&Email {
        from: "test@example.com".into(),
        to: vec!["recipient@example.com".into()],
        ..Default::default()
    });
    assert!(
        res.is_err(),
        "expected error when sending after pool closed"
    );
}

#[test]
fn test_send_invalid_email() {
    let sink = common::start();
    let pool = Pool::new(opt(sink.port, 1)).expect("create pool");

    // Invalid From address.
    let invalid_from = Email {
        from: "invalid-email-address".into(),
        to: vec!["recipient@example.com".into()],
        subject: "Test Invalid From".into(),
        text: b"Test Body".to_vec(),
        ..Default::default()
    };
    assert!(
        pool.send(&invalid_from).is_err(),
        "expected error for invalid From"
    );

    // Invalid To address.
    let invalid_to = Email {
        from: "sender@example.com".into(),
        to: vec!["invalid-recipient".into()],
        subject: "Test Invalid To".into(),
        text: b"Test Body".to_vec(),
        ..Default::default()
    };
    assert!(
        pool.send(&invalid_to).is_err(),
        "expected error for invalid To"
    );

    // No messages should have been delivered.
    assert_eq!(sink.count(), 0, "expected 0 messages");
    pool.close();
}
