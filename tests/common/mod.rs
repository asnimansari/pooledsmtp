//! A minimal in-process SMTP sink used by the pool integration tests. It stands
//! in for MailHog (used by the Go tests) so the suite is self-contained and
//! needs no external service. It accepts any number of concurrent connections,
//! speaks just enough SMTP for lettre's client, and records each DATA payload.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct Sink {
    pub port: u16,
    received: Arc<Mutex<Vec<String>>>,
}

impl Sink {
    /// The DATA payloads received so far.
    pub fn received(&self) -> Vec<String> {
        self.received.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

/// Starts a sink on an ephemeral port and returns a handle. The server accepts
/// connections on a detached thread until the process exits.
pub fn start() -> Sink {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sink");
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();

    thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let r = r.clone();
            thread::spawn(move || handle(s, r));
        }
    });

    Sink { port, received }
}

fn handle(stream: TcpStream, sink: Arc<Mutex<Vec<String>>>) {
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
