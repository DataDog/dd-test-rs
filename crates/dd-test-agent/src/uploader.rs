// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Batches events and sends them from a background thread, so reading the
//! test binary's messages does not wait for each request.

use crate::event::Span;
use dd_test_core::warn;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const FLUSH_EVENTS: usize = 500;
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
/// Events waiting while a batch is sent. When uploads fall behind, `send`
/// waits instead of building a backlog, so finishing only sends what is left
/// within the test binary's finish timeout.
pub const MAX_QUEUED_EVENTS: usize = FLUSH_EVENTS;

pub struct Uploader {
    sender: Option<SyncSender<Span>>,
    thread: Option<JoinHandle<()>>,
}

impl Uploader {
    /// Calls `send` with a batch every `FLUSH_EVENTS` events, or `FLUSH_INTERVAL`
    /// after the first event of a batch, whichever comes first.
    pub fn start(mut send: impl FnMut(Vec<Span>) + Send + 'static) -> Uploader {
        let (sender, events) = mpsc::sync_channel::<Span>(MAX_QUEUED_EVENTS);
        let thread = std::thread::Builder::new()
            .name("dd-test-uploader".into())
            .spawn(move || {
                let mut batch = Vec::new();
                let mut deadline: Option<Instant> = None;
                loop {
                    let received = match deadline {
                        Some(deadline) => events.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                        None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match received {
                        Ok(span) => {
                            deadline.get_or_insert_with(|| Instant::now() + FLUSH_INTERVAL);
                            batch.push(span);
                            if batch.len() < FLUSH_EVENTS {
                                continue;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            if !batch.is_empty() {
                                send(batch);
                            }
                            return;
                        }
                    }
                    deadline = None;
                    send(std::mem::take(&mut batch));
                }
            });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                warn!("test results will not be sent to Datadog: {error}");
                None
            }
        };
        Uploader {
            sender: Some(sender),
            thread,
        }
    }

    pub fn send(&self, span: Span) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(span);
        }
    }

    /// Sends the last batch and waits until every event is handled.
    pub fn close(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;

    fn span() -> Span {
        Span::new(Kind::Test, "libtest.test", "s.t", "svc", 0)
    }

    #[test]
    fn sends_full_batches_on_time_and_on_close() {
        let (batches, received) = mpsc::channel();
        let mut uploader = Uploader::start(move |spans| {
            let _ = batches.send(spans.len());
        });
        for _ in 0..FLUSH_EVENTS {
            uploader.send(span());
        }
        assert_eq!(received.recv_timeout(Duration::from_secs(1)), Ok(FLUSH_EVENTS));

        uploader.send(span());
        let started = Instant::now();
        assert_eq!(received.recv_timeout(FLUSH_INTERVAL * 2), Ok(1));
        assert!(started.elapsed() >= FLUSH_INTERVAL - Duration::from_millis(100));

        uploader.send(span());
        uploader.send(span());
        uploader.close();
        assert_eq!(received.try_recv(), Ok(2));
    }

    #[test]
    fn a_slow_send_only_blocks_new_events_once_the_queue_is_full() {
        let mut uploader = Uploader::start(|_| std::thread::sleep(Duration::from_millis(500)));
        let started = Instant::now();
        for _ in 0..FLUSH_EVENTS + MAX_QUEUED_EVENTS {
            uploader.send(span());
        }
        assert!(started.elapsed() < Duration::from_millis(200));
        for _ in 0..FLUSH_EVENTS {
            uploader.send(span());
        }
        assert!(started.elapsed() >= Duration::from_millis(400));
        uploader.close();
    }
}
