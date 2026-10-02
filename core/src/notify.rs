//! Wakes the task that does the I/O. The client and the server notify it when
//! something an app did needs a transmit: a call started, a reply or an item
//! sent, an item consumed (which frees credit), a call dropped (which cancels
//! it). It's a flag and a `Waker`, so nothing here depends on an executor.
//!
//! Like the rest of the core it assumes one executor: `Cell`s, not atomics.

use alloc::rc::Rc;
use core::cell::Cell;
use core::future::poll_fn;
use core::task::{Context, Poll, Waker};

#[derive(Default)]
pub struct Notify {
    notified: Cell<bool>,
    waker: Cell<Option<Waker>>,
}

impl Notify {
    /// Wakes the task waiting in `poll_notified`, or its next wait returns at
    /// once. Notifications don't accumulate: one wait consumes them all.
    pub fn notify(&self) {
        self.notified.set(true);
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn poll_notified(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.notified.replace(false) {
            return Poll::Ready(());
        }
        self.waker.set(Some(cx.waker().clone()));
        Poll::Pending
    }

    /// Completes when notified.
    pub async fn notified(&self) {
        poll_fn(|cx| self.poll_notified(cx)).await;
    }
}

/// What the client and the server share with their handles, and the I/O task.
pub type SharedNotify = Rc<Notify>;

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_notification_wakes_the_waiting_task_once() {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let notify = Notify::default();

        assert_eq!(notify.poll_notified(&mut cx), Poll::Pending);
        notify.notify();
        notify.notify();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert_eq!(notify.poll_notified(&mut cx), Poll::Ready(()));
        // They don't accumulate.
        assert_eq!(notify.poll_notified(&mut cx), Poll::Pending);
    }

    #[test]
    fn a_notification_before_the_wait_isnt_lost() {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let notify = Notify::default();
        notify.notify();
        assert_eq!(notify.poll_notified(&mut cx), Poll::Ready(()));
    }
}
