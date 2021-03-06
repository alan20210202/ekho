/*
Copyright 2021 Chengyuan Ma

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
associated documentation files (the "Software"), to deal in the Software without restriction,
including without limitation the rights to use, copy, modify, merge, publish, distribute, sub-
-license, and/or sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial
portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NON-
-INFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES
OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
*/

//! Build sessions above the raw KCP algorithm

#![allow(dead_code)]
use crate::config::config;
use crate::icmp::Endpoint;

use crate::kcp::{ControlBlock, Error};
use chacha20poly1305::aead::{AeadInPlace, NewAead};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use dashmap::DashMap;
use lazy_static::lazy_static;
use rand::{thread_rng, Rng};
use rustc_hash::FxHasher;
use std::fmt;
use std::hash::BuildHasherDefault;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::select;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, Notify};
use tokio::task;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, Duration};
use tracing::{debug, debug_span, error, instrument, warn};
use tracing_futures::Instrument;

type Control = (Mutex<ControlBlock>, Notify);

lazy_static! {
    static ref CONTROLS: DashMap<(Endpoint, u32), Weak<Control>, BuildHasherDefault<FxHasher>> =
        Default::default();
    static ref CIPHER: ChaCha20Poly1305 = ChaCha20Poly1305::new(&config().key);
    static ref NONCE: Nonce = Nonce::default();
    static ref INCOMING: (UnboundedSender<Session>, Mutex<UnboundedReceiver<Session>>) = {
        let (tx, rx) = unbounded_channel();
        (tx, Mutex::new(rx))
    };
}

const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);

/// A session, built on top of KCP
pub struct Session {
    conv: u32,
    peer: Endpoint,
    updater: JoinHandle<()>,
    control: Arc<Control>,
    peer_closing: Arc<AtomicBool>,
    local_closing: Arc<AtomicBool>,
}

impl Session {
    /// Creates a new session given a peer endpoint and a conv.
    pub fn new(peer: Endpoint, conv: u32) -> Self {
        assert!(!CONTROLS.contains_key(&(peer, conv)));
        // The naming here is very nasty!
        let control = Arc::new((
            Mutex::new(ControlBlock::new(conv, config().kcp.clone())),
            Notify::new(),
        ));
        let control_cloned = control.clone();
        CONTROLS.insert((peer, conv), Arc::downgrade(&control_cloned));
        let peer_closing = Arc::new(AtomicBool::new(false));
        let local_closing = Arc::new(AtomicBool::new(false));
        let peer_closing_cloned = peer_closing.clone();
        let local_closing_cloned = local_closing.clone();
        let updater = task::spawn(
            async move {
                let icmp_tx = crate::icmp::clone_sender().await;
                let mut interval = interval(Duration::from_millis(config().kcp.interval as u64));
                'update_loop: loop {
                    {
                        interval.tick().await;
                        let mut kcp = control_cloned.0.lock().await;
                        kcp.flush();
                        control_cloned.1.notify_waiters();
                        while let Some(mut raw) = kcp.output() {
                            // dissect_headers_from_raw(&raw, "send");
                            if CIPHER.encrypt_in_place(&NONCE, b"", &mut raw).is_ok() {
                                icmp_tx.send((peer, raw)).await.unwrap();
                            } else {
                                error!("error encrypting block");
                                break 'update_loop;
                            }
                        }
                        let peer_closing = peer_closing_cloned.load(Ordering::SeqCst);
                        let local_closing = local_closing_cloned.load(Ordering::SeqCst);
                        if kcp.dead_link() || peer_closing && local_closing && kcp.all_flushed() {
                            if kcp.dead_link() {
                                warn!("dead link");
                            }
                            break;
                        }
                    }
                }
            }, // .instrument(debug_span!("update loop", ?peer, conv)),
        );
        Session {
            conv,
            peer,
            control,
            updater,
            peer_closing,
            local_closing,
        }
    }

    pub fn connect(peer: Endpoint) -> Self {
        loop {
            let conv = thread_rng().gen();
            if !CONTROLS.contains_key(&(peer, conv)) {
                return Session::new(peer, conv);
            }
        }
    }

    pub async fn incoming() -> Self {
        INCOMING.1.lock().await.recv().await.unwrap()
    }

    #[instrument(skip(buf))]
    pub async fn send(&self, buf: &[u8]) {
        loop {
            {
                let mut kcp = self.control.0.lock().await;
                if kcp.wait_send() < kcp.config().send_wnd as usize {
                    if buf.is_empty() {
                        self.local_closing.store(true, Ordering::SeqCst);
                    }
                    kcp.send(buf).unwrap();
                    break;
                }
            }
            self.control.1.notified().await;
        }
    }

    #[instrument]
    pub async fn recv(&self) -> Vec<u8> {
        loop {
            {
                let mut kcp = self.control.0.lock().await;
                match kcp.recv() {
                    Ok(data) => {
                        if data.is_empty() {
                            self.peer_closing.store(true, Ordering::SeqCst);
                        }
                        return data;
                    }
                    Err(Error::NotAvailable) => {}
                    Err(err) => Err(err).unwrap(),
                }
            }
            self.control.1.notified().await;
        }
    }

    #[instrument]
    pub async fn close(self) {
        select! {
            _ = sleep(CLOSE_TIMEOUT) => {}
            _ = async {
                self.send(b"").await;
                while !self.peer_closing.load(Ordering::SeqCst) {
                    let _discarded = self.recv().await;
                }
                self.updater.await.unwrap();
                CONTROLS.remove(&(self.peer, self.conv));
                debug!("session closed, {} remaining", CONTROLS.len());
            } => {}
        }
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.peer, self.conv)
    }
}

#[instrument]
async fn dispatch_loop() {
    let sender = crate::icmp::clone_sender().await;
    loop {
        let (from, mut raw) = crate::icmp::receive_packet()
            .instrument(debug_span!("receive_icmp_packet"))
            .await;
        if CIPHER.decrypt_in_place(&NONCE, b"", &mut raw).is_err() {
            // Mimic real ping behavior
            sender.send((from, raw)).await.unwrap();
            continue;
        }
        let conv = crate::kcp::conv_from_raw(&raw);
        let key = &(from, conv);
        let mut control = CONTROLS.get(key).and_then(|weak| weak.upgrade());
        if control.is_none() && crate::kcp::first_push_packet(&raw) {
            let new_session = Session::new(from, conv);
            INCOMING.0.send(new_session).unwrap_or_default();
            control = CONTROLS.get(key).and_then(|weak| weak.upgrade());
        }
        if let Some(control) = control {
            // dissect_headers_from_raw(&raw, "recv");
            let mut kcp = control.0.lock().await;
            kcp.input(&raw).unwrap();
            control.1.notify_waiters();
        }
    }
}

pub async fn init_dispatch_loop() {
    task::spawn(dispatch_loop());
}
