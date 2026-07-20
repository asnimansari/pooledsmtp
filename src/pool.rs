//! The SMTP connection pool. Rust port of the pool half of the Go `pool.go`:
//! a bounded set of lazily-created, reusable SMTP connections with a background
//! idle sweeper, network-error retries, and the 421-drops / 550-keeps-after-RSET
//! reuse rule.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, select, Receiver, SendTimeoutError, Sender, TrySendError};

use crate::smtp::Conn;
use crate::{Email, Error, Opt};

const SWEEP_INTERVAL: Duration = Duration::from_secs(2);
const ONE_SECOND: Duration = Duration::from_secs(1);

/// Shared pool state, referenced by the `Pool` handle and the sweeper thread.
struct Inner {
    opt: Opt,
    conns_tx: Sender<Conn>,
    conns_rx: Receiver<Conn>,
    created_conns: AtomicI32,
    closed: AtomicBool,
    /// Receiver whose disconnection (all senders dropped on `close`) unblocks
    /// borrow/return waiters immediately. Mirrors Go's `stopBorrow` channel.
    stop_rx: Receiver<()>,
}

/// An SMTP connection pool.
pub struct Pool {
    inner: Arc<Inner>,
    /// Dropped on `close` to unblock all waiters with `ErrPoolClosed`.
    stop_tx: Mutex<Option<Sender<()>>>,
    /// Whether a background sweeper is running.
    has_sweeper: bool,
}

impl Pool {
    /// Initializes and returns a new SMTP pool. Mirrors Go's `New`.
    pub fn new(mut opt: Opt) -> Result<Pool, Error> {
        if opt.max_conns < 1 {
            return Err(Error::Config("MaxConns should be >= 1".into()));
        }
        if opt.max_message_retries == 0 {
            opt.max_message_retries = 1;
        }
        if opt.pool_wait_timeout < ONE_SECOND {
            opt.pool_wait_timeout = Duration::from_secs(2);
        }

        let (conns_tx, conns_rx) = bounded::<Conn>(opt.max_conns);
        let (stop_tx, stop_rx) = bounded::<()>(0);

        // Start the idle connection sweeper.
        let has_sweeper = opt.idle_timeout >= ONE_SECOND && opt.max_conns > 1;

        let inner = Arc::new(Inner {
            opt,
            conns_tx,
            conns_rx,
            created_conns: AtomicI32::new(0),
            closed: AtomicBool::new(false),
            stop_rx,
        });

        if has_sweeper {
            let inner = inner.clone();
            thread::spawn(move || inner.sweep(SWEEP_INTERVAL));
        }

        Ok(Pool {
            inner,
            stop_tx: Mutex::new(Some(stop_tx)),
            has_sweeper,
        })
    }

    /// Sends an e-mail using an available connection in the pool. On a
    /// network-type error the message is retried on a new connection. Mirrors
    /// Go's `Send`.
    pub fn send(&self, e: &Email) -> Result<(), Error> {
        let mut last_err: Option<Error> = None;

        for i in 0..self.inner.opt.max_message_retries {
            if i > 0 && self.inner.opt.message_retry_delay > Duration::ZERO {
                thread::sleep(self.inner.opt.message_retry_delay);
            }

            // Get a connection from the pool.
            let mut c = match self.inner.borrow_conn() {
                Ok(c) => c,
                Err(err) => {
                    let retriable = err.is_retriable();
                    last_err = Some(err);
                    if retriable {
                        continue;
                    }
                    return Err(last_err.unwrap());
                }
            };

            // Send the message.
            let (retry, res) = c.send(e);
            match res {
                Ok(()) => {
                    self.inner.return_conn(c, None);
                    return Ok(());
                }
                Err(err) => {
                    self.inner.return_conn(c, Some(&err));
                    last_err = Some(err);
                    if !retry {
                        return Err(last_err.unwrap());
                    }
                }
            }
        }

        Err(last_err.unwrap_or(Error::PoolTimeout))
    }

    /// Closes the pool. Sets the closed flag, unblocks all waiters, and drains
    /// connections (sending SMTP QUIT). Mirrors Go's `Close`.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);

        // Drop the stop sender to unblock all borrow/return waiters.
        if let Ok(mut guard) = self.stop_tx.lock() {
            guard.take();
        }

        // If no background sweeper is running, drain synchronously.
        if !self.has_sweeper {
            self.inner.sweep(ONE_SECOND);
        }
    }
}

impl Inner {
    /// Borrows a connection from the pool, creating one if there is room and
    /// none is idle. Mirrors Go's `borrowConn`.
    fn borrow_conn(&self) -> Result<Conn, Error> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(Error::PoolClosed);
        }

        // If there are no idle connections and there is room, create a new one.
        if (self.created_conns.load(Ordering::SeqCst) as usize) < self.opt.max_conns
            && self.conns_rx.is_empty()
        {
            self.created_conns.fetch_add(1, Ordering::SeqCst);
            return match Conn::new(&self.opt) {
                Ok(c) => Ok(c),
                Err(err) => {
                    // Decrement on failed connection creation.
                    self.created_conns.fetch_sub(1, Ordering::SeqCst);
                    Err(err)
                }
            };
        }

        // Otherwise wait for a free connection, pool closure, or timeout.
        select! {
            recv(self.conns_rx) -> msg => msg.map_err(|_| Error::PoolClosed),
            recv(self.stop_rx) -> _ => Err(Error::PoolClosed),
            default(self.opt.pool_wait_timeout) => Err(Error::PoolTimeout),
        }
    }

    /// Returns a connection to the pool, or discards it, based on the error from
    /// the last transaction. Mirrors Go's `returnConn`. The result is advisory;
    /// callers ignore it, as in the Go original.
    fn return_conn(&self, mut c: Conn, last_err: Option<&Error>) {
        // Network/non-SMTP errors and SMTP 421 close the connection.
        if let Some(err) = last_err {
            if err.should_close_conn() {
                self.discard(c);
                return;
            }
        }

        // Always RSET before reusing, as some servers throw "sender already
        // specified" or "commands out of sequence" errors otherwise.
        if c.reset().is_err() {
            self.discard(c);
            return;
        }

        match self.conns_tx.send_timeout(c, self.opt.pool_wait_timeout) {
            Ok(()) => {}
            Err(SendTimeoutError::Timeout(c)) | Err(SendTimeoutError::Disconnected(c)) => {
                self.discard(c);
            }
        }
    }

    /// Closes a connection and decrements the live-connection count.
    fn discard(&self, mut c: Conn) {
        c.close();
        self.created_conns.fetch_sub(1, Ordering::SeqCst);
    }

    /// Periodically sweeps idle connections and closes them. When the pool is
    /// closed it drains all connections (via QUIT) and exits once none remain.
    /// Mirrors Go's `sweepConns`. Blocking; run on its own thread (or called
    /// once synchronously by `close`).
    fn sweep(&self, interval: Duration) {
        let mut active: Vec<Conn> = Vec::with_capacity(self.opt.max_conns);
        loop {
            thread::sleep(interval);
            active.clear();

            let num = self.conns_rx.len();
            let created = self.created_conns.load(Ordering::SeqCst);
            let closed = self.closed.load(Ordering::SeqCst);
            if closed && created == 0 {
                return;
            }

            for _ in 0..num {
                // Pick a connection from the pool without blocking.
                let mut c = match self.conns_rx.try_recv() {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if closed || c.last_activity.elapsed() > self.opt.idle_timeout {
                    self.created_conns.fetch_sub(1, Ordering::SeqCst);
                    if closed {
                        c.quit();
                    } else {
                        c.close();
                    }
                } else {
                    active.push(c);
                }
            }

            // Put the active connections back.
            for c in active.drain(..) {
                match self.conns_tx.try_send(c) {
                    Ok(()) => {}
                    Err(TrySendError::Full(mut c)) | Err(TrySendError::Disconnected(mut c)) => {
                        c.close();
                        self.created_conns.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }
        }
    }
}
