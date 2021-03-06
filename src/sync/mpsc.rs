// Copyright 2015 The coio Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Multi-producer, single-consumer FIFO queue communication primitives.

pub use std::sync::mpsc::{TrySendError, SendError, TryRecvError, RecvError};

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use coroutine::HandleList;
use runtime::Processor;
use scheduler::Scheduler;

#[derive(Clone)]
pub struct Sender<T> {
    inner: Option<mpsc::Sender<T>>,

    wait_list: Arc<Mutex<HandleList>>,
}

unsafe impl<T: Send> Send for Sender<T> {}

impl<T> Sender<T> {
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        match self.inner.as_ref().unwrap().send(t) {
            Ok(..) => {
                let mut wait_list = self.wait_list.lock().unwrap();
                if let Some(coro) = wait_list.pop_front() {
                    Scheduler::ready(coro);
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Drop the inner Sender first
        let _ = self.inner.take();

        // Try to wake up all the pending coroutines if this is the last Sender.
        // Because if this is the last Sender, there won't be another one to push
        // items into this queue, so we have to wake the coroutine up explicitly,
        // who ownes the other end of this channel.
        if Arc::strong_count(&self.wait_list) <= 2 {
            let mut wait_list = self.wait_list.lock().unwrap();
            while let Some(hdl) = wait_list.pop_front() {
                trace!("{:?} is awaken by dropping Sender in wait_list", hdl);
                Scheduler::ready(hdl);
            }
        }
    }
}

pub struct Receiver<T> {
    inner: mpsc::Receiver<T>,

    wait_list: Arc<Mutex<HandleList>>,
}

unsafe impl<T: Send> Send for Receiver<T> {}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        while let Some(processor) = Processor::current() {
            // 1. Try to receive first
            let mut r = self.try_recv();
            match r {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(RecvError),
            }

            // 2. Yield
            processor.park_with(|p, coro| {
                // 3. Lock the wait list
                let mut wait_list = self.wait_list.lock().unwrap();

                // 4. Try to receive again, to ensure no one sent items into the queue while
                //    we are locking the wait list
                r = self.try_recv();

                match r {
                    Err(TryRecvError::Empty) => {
                        // 5.1. Push ourselves into the wait list
                        wait_list.push_back(coro);
                    }
                    _ => {
                        // 5.2. Success!
                        p.ready(coro);
                    }
                }
            });

            // 6. Check it again after being waken up (if 5.2 succeeded)
            match r {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(RecvError),
            }
        }

        // What? The processor is gone? Then fallback to blocking recv
        self.inner.recv()
    }
}

/// Create a channel pair
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::channel();
    let wait_list = Arc::new(Mutex::new(HandleList::new()));

    let sender = Sender {
        inner: Some(tx),
        wait_list: wait_list.clone(),
    };

    let receiver = Receiver {
        inner: rx,
        wait_list: wait_list,
    };

    (sender, receiver)
}

#[derive(Clone)]
pub struct SyncSender<T> {
    inner: Option<mpsc::SyncSender<T>>,

    send_wait_list: Arc<Mutex<HandleList>>,
    recv_wait_list: Arc<Mutex<HandleList>>,
}

unsafe impl<T: Send> Send for SyncSender<T> {}

impl<T> SyncSender<T> {
    pub fn try_send(&self, t: T) -> Result<(), TrySendError<T>> {
        match self.inner.as_ref().unwrap().try_send(t) {
            Ok(..) => {
                let mut recv_wait_list = self.recv_wait_list.lock().unwrap();
                if let Some(coro) = recv_wait_list.pop_front() {
                    trace!("{:?} is waken up in SyncSender receive_wait_list, {} \
                            remains",
                           coro,
                           recv_wait_list.len());
                    Scheduler::ready(coro);
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    pub fn send(&self, mut t: T) -> Result<(), SendError<T>> {
        while let Some(p) = Processor::current() {
            let mut r = self.try_send(t);

            match r {
                Ok(..) => return Ok(()),
                Err(TrySendError::Disconnected(e)) => return Err(SendError(e)),
                Err(TrySendError::Full(t_)) => {
                    t = t_;
                }
            }

            r = Ok(());
            {
                let r_ptr = &mut r;
                p.park_with(move |p, coro| {
                    let mut send_wait_list = self.send_wait_list.lock().unwrap();
                    let r = self.try_send(t);

                    match r {
                        Err(TrySendError::Full(..)) => {
                            send_wait_list.push_back(coro);
                        }
                        _ => {
                            p.ready(coro);
                        }
                    }

                    *r_ptr = r;
                });
            }

            match r {
                Ok(..) => return Ok(()),
                Err(TrySendError::Disconnected(e)) => return Err(SendError(e)),
                Err(TrySendError::Full(t_)) => {
                    t = t_;
                }
            }
        }

        match self.inner.as_ref().unwrap().send(t) {
            Ok(..) => {
                let mut recv_wait_list = self.recv_wait_list.lock().unwrap();
                if let Some(coro) = recv_wait_list.pop_front() {
                    // Wake them up ...
                    Scheduler::ready(coro);
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

impl<T> Drop for SyncSender<T> {
    fn drop(&mut self) {
        // Drop the inner SyncSender first
        {
            self.inner.take();
        }

        // Try to wake up all the pending coroutines if this is the last SyncSender.
        // Because if this is the last SyncSender, there won't be another one to push
        // items into this queue, so we have to wake the coroutine up explicitly,
        // who ownes the other end of this channel.
        if Arc::strong_count(&self.recv_wait_list) <= 2 {
            let mut recv_wait_list = self.recv_wait_list.lock().unwrap();
            while let Some(hdl) = recv_wait_list.pop_front() {
                trace!("{:?} is awaken by dropping SyncSender in recv_wait_list",
                       hdl);
                Scheduler::ready(hdl);
            }
        }
    }
}

pub struct SyncReceiver<T> {
    inner: Option<mpsc::Receiver<T>>,

    send_wait_list: Arc<Mutex<HandleList>>,
    recv_wait_list: Arc<Mutex<HandleList>>,
}

unsafe impl<T: Send> Send for SyncReceiver<T> {}

impl<T> SyncReceiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.inner.as_ref().unwrap().try_recv() {
            Ok(t) => {
                let mut send_wait_list = self.send_wait_list.lock().unwrap();
                if let Some(coro) = send_wait_list.pop_front() {
                    trace!("{:?} is waken up in SyncReceiver send_wait_list, {} remains",
                           coro,
                           send_wait_list.len());
                    Scheduler::ready(coro);
                }
                Ok(t)
            }
            Err(err) => Err(err),
        }
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        while let Some(processor) = Processor::current() {
            let mut r = self.try_recv();

            match r {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(RecvError),
            }

            processor.park_with(|p, coro| {
                let mut recv_wait_list = self.recv_wait_list.lock().unwrap();

                r = self.try_recv();

                match r {
                    Err(TryRecvError::Empty) => {
                        recv_wait_list.push_back(coro);
                    }
                    _ => {
                        p.ready(coro);
                    }
                }
            });

            match r {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(RecvError),
            }
        }

        // What? The processor is gone? Then use blocking recv
        match self.inner.as_ref().unwrap().recv() {
            Ok(t) => {
                let mut send_wait_list = self.send_wait_list.lock().unwrap();
                if let Some(coro) = send_wait_list.pop_front() {
                    Scheduler::ready(coro);
                }
                Ok(t)
            }
            Err(err) => Err(err),
        }
    }
}

impl<T> Drop for SyncReceiver<T> {
    fn drop(&mut self) {
        // Drop the inner SyncReceiver first
        {
            self.inner.take();
        }

        // Try to wake up all the pending coroutines if this is the last SyncReceiver.
        // Because there won't be another one to push items into this queue, so we
        // have to wake the coroutine up explicitly, who ownes the other end of this channel.
        let mut send_wait_list = self.send_wait_list.lock().unwrap();
        while let Some(hdl) = send_wait_list.pop_front() {
            trace!("{:?} is awaken by dropping SyncReceiver in send_wait_list",
                   hdl);
            Scheduler::ready(hdl);
        }
    }
}

/// Create a bounded channel pair
///
/// NOTE: Due to the implementation of sync channel in libstd, you will get `RecvError` from `SyncReceiver::recv`
/// no matter if there still have data inside the queue after you dropped the `SyncSender`.
///
/// Tracking issue: https://github.com/zonyitoo/coio-rs/issues/31
pub fn sync_channel<T>(bound: usize) -> (SyncSender<T>, SyncReceiver<T>) {
    let (tx, rx) = mpsc::sync_channel(bound);
    let send_wait_list = Arc::new(Mutex::new(HandleList::new()));
    let recv_wait_list = Arc::new(Mutex::new(HandleList::new()));

    let sender = SyncSender {
        inner: Some(tx),
        send_wait_list: send_wait_list.clone(),
        recv_wait_list: recv_wait_list.clone(),
    };

    let receiver = SyncReceiver {
        inner: Some(rx),
        send_wait_list: send_wait_list,
        recv_wait_list: recv_wait_list,
    };

    (sender, receiver)
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use scheduler::Scheduler;

    #[test]
    fn test_channel_basic() {
        Scheduler::new()
            .run(move || {
                let (tx, rx) = channel();

                let h = Scheduler::spawn(move || {
                    assert_eq!(rx.try_recv(), Ok(1));
                    assert_eq!(rx.try_recv(), Ok(2));
                    assert_eq!(rx.try_recv(), Ok(3));

                    for i in 1..10 {
                        assert_eq!(rx.recv(), Ok(i));
                    }
                });

                assert_eq!(tx.send(1), Ok(()));
                assert_eq!(tx.send(2), Ok(()));
                assert_eq!(tx.send(3), Ok(()));

                Scheduler::sched();

                for i in 1..10 {
                    assert_eq!(tx.send(i), Ok(()));
                }

                h.join().unwrap();
            })
            .unwrap();
    }

    #[test]
    fn test_sync_channel_basic() {
        Scheduler::new()
            .run(move || {
                let (tx, rx) = sync_channel(2);

                let h = Scheduler::spawn(move || {
                    assert_eq!(rx.try_recv(), Ok(1));
                    assert_eq!(rx.try_recv(), Ok(2));
                    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

                    for i in 1..10 {
                        assert_eq!(rx.recv(), Ok(i));
                    }
                });

                assert_eq!(tx.try_send(1), Ok(()));
                assert_eq!(tx.try_send(2), Ok(()));
                assert_eq!(tx.try_send(3), Err(TrySendError::Full(3)));

                Scheduler::sched();

                for i in 1..10 {
                    assert_eq!(tx.send(i), Ok(()));
                }

                h.join().unwrap();
            })
            .unwrap();
    }

    #[test]
    fn test_channel_without_processor() {
        let (tx1, rx1) = channel();
        let (tx2, rx2) = channel();
        let barrier = Arc::new(Barrier::new(2));

        {
            let barrier = barrier.clone();

            thread::spawn(move || {
                Scheduler::new()
                    .run(move || {
                        barrier.wait();
                        assert_eq!(rx1.recv(), Ok(1));
                        assert_eq!(tx2.send(2), Ok(()));
                    })
                    .unwrap();
            });
        }

        // ensure that rx1.recv() above has been called
        barrier.wait();
        thread::sleep(Duration::from_millis(10));

        assert_eq!(tx1.send(1), Ok(()));
        assert_eq!(rx2.recv(), Ok(2));
    }

    #[test]
    fn test_sync_channel_without_processor() {
        let (tx1, rx1) = sync_channel(1);
        let (tx2, rx2) = sync_channel(1);
        let barrier = Arc::new(Barrier::new(2));

        {
            let barrier = barrier.clone();

            thread::spawn(move || {
                Scheduler::new()
                    .run(move || {
                        barrier.wait();
                        assert_eq!(rx1.recv(), Ok(1));
                        assert_eq!(tx2.send(2), Ok(()));
                    })
                    .unwrap();
            });
        }

        // ensure that rx1.recv() above has been called
        barrier.wait();
        thread::sleep(Duration::from_millis(10));

        assert_eq!(tx1.send(1), Ok(()));
        assert_eq!(rx2.recv(), Ok(2));
    }

    #[test]
    fn test_channel_passing_ring() {
        Scheduler::new()
            .with_workers(10)
            .run(|| {
                let mut handlers = Vec::new();

                {
                    let (tx, mut rx) = channel();

                    for _ in 0..10000 {
                        let (ltx, lrx) = channel();
                        let h = Scheduler::spawn(move || {
                            loop {
                                let value = match rx.recv() {
                                    Ok(v) => v,
                                    Err(..) => break,
                                };
                                ltx.send(value).unwrap();
                            }
                        });
                        handlers.push(h);

                        rx = lrx;
                    }

                    for i in 0..10 {
                        tx.send(i).unwrap();
                        let value = rx.recv().unwrap();
                        assert_eq!(i, value);
                    }
                }

                for h in handlers {
                    h.join().unwrap();
                }
            })
            .unwrap();
    }

    #[test]
    fn test_sync_channel_passing_ring() {
        Scheduler::new()
            .with_workers(10)
            .run(|| {
                let mut handlers = Vec::new();

                {
                    let (tx, mut rx) = sync_channel(1);

                    for _ in 0..1000 {
                        let (ltx, lrx) = sync_channel(1);
                        let h = Scheduler::spawn(move || {
                            loop {
                                let value = match rx.recv() {
                                    Ok(v) => v,
                                    Err(..) => break,
                                };
                                ltx.send(value).unwrap();
                            }
                        });
                        handlers.push(h);

                        rx = lrx;
                    }

                    for i in 0..10 {
                        tx.send(i).unwrap();
                        let value = rx.recv().unwrap();
                        assert_eq!(i, value);
                    }
                }

                for h in handlers {
                    h.join().unwrap();
                }

            })
            .unwrap();
    }
}
