use std::fmt;
use std::future::Future;
use std::panic;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::channel::oneshot;
use futures::future::FutureExt;
use pin_project::pin_project;

/// The handle to a local future returned by
/// [`local_handle`](crate::future::FutureExt::local_handle). When you drop this,
/// the local future will be woken up to be dropped by the executor.
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Debug)]
pub struct LocalHandle<T> {
    rx: oneshot::Receiver<T>,
}

impl<T: 'static> Future for LocalHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.rx.poll_unpin(cx) {
            Poll::Ready(Ok(output)) => Poll::Ready(output),
            // The oneshot sender was dropped.
            Poll::Ready(Err(e)) => panic::resume_unwind(Box::new(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

type SendMsg<Fut> = <Fut as Future>::Output;

/// A future which sends its output to the corresponding `LocalHandle`.
/// Created by [`local_handle`](crate::future::FutureExt::local_handle).
#[pin_project(project = LocalProj)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Local<Fut: Future> {
    tx: Option<oneshot::Sender<SendMsg<Fut>>>,
    #[pin]
    future: Fut,
}

impl<Fut: Future + fmt::Debug> fmt::Debug for Local<Fut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Local").field(&self.future).finish()
    }
}

impl<Fut: Future> Future for Local<Fut> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let LocalProj { tx, future } = self.project();
        if tx.as_mut().unwrap().poll_canceled(cx).is_ready() {
            // Cancelled, bail out
            return Poll::Ready(());
        }

        let output = match future.poll(cx) {
            Poll::Ready(output) => output,
            Poll::Pending => return Poll::Pending,
        };

        // if the receiving end has gone away then that's ok, we just ignore the
        // send error here.
        drop(tx.take().unwrap().send(output));
        Poll::Ready(())
    }
}

pub(super) fn local_handle<Fut: Future>(future: Fut) -> (Local<Fut>, LocalHandle<Fut::Output>) {
    let (tx, rx) = oneshot::channel();

    // Unwind Safety: See the docs for LocalHandle.
    let wrapped = Local {
        future,
        tx: Some(tx),
    };

    (wrapped, LocalHandle { rx })
}
