// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Use async/await syntax with actix actors.

#![deny(missing_docs, warnings)]

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix::{Actor, ActorFuture, AsyncContext, ResponseActFuture};
use pin_project::pin_project;
use scoped_tls_hkt::scoped_thread_local;

use self::local_handle::local_handle;

mod local_handle;

scoped_thread_local!(static mut CURRENT_ACTOR_CTX: for<'a> (&'a mut dyn Any, &'a mut dyn Any));

fn set_actor_context<A, F, R>(actor: &mut A, ctx: &mut A::Context, f: F) -> R
where
    A: Actor,
    F: FnOnce() -> R,
{
    CURRENT_ACTOR_CTX.set((actor, ctx), f)
}

/// May be called from within a future spawned onto an actor context to gain mutable access
/// to the actor's state and/or context. The future must have been wrapped using
/// [`interop_actor`](FutureInterop::interop_actor) or
/// [`interop_actor_boxed`](FutureInterop::interop_actor_boxed).
///
/// Nested calls to this function will panic, as only one mutable borrow can be given out
/// at a time.
pub fn with_ctx<A, F, R>(f: F) -> R
where
    A: Actor + 'static,
    F: FnOnce(&mut A, &mut A::Context) -> R,
{
    CURRENT_ACTOR_CTX.with(|(actor, ctx)| {
        let actor = actor
            .downcast_mut()
            .expect("Future was spawned onto the wrong actor");
        let ctx = ctx
            .downcast_mut()
            .expect("Future was spawned onto the wrong actor");
        f(actor, ctx)
    })
}

/// May be called in the same places as [`with_ctx`](with_ctx) to run a chunk of async
/// code with exclusive access to an actor's state: no other futures spawned to this
/// actor will be polled during the critical section.
/// Unlike [`with_ctx`](with_ctx), calls to this function may be nested, although there
/// is little point in doing so. Calling [`with_ctx`](with_ctx) from within a critical
/// section is allowed (and expected).
pub fn critical_section<A: Actor, F: Future>(f: F) -> impl Future<Output = F::Output>
where
    A::Context: AsyncContext<A>,
    F: 'static,
{
    let (f, handle) = local_handle(f);
    with_ctx(|actor: &mut A, ctx: &mut A::Context| ctx.wait(f.interop_actor(actor)));
    handle
}

/// Future to ActorFuture adapter returned by [`interop_actor`](FutureInterop::interop_actor).
#[pin_project]
#[derive(Debug)]
pub struct FutureInteropWrap<A: Actor, F> {
    #[pin]
    inner: F,
    phantom: PhantomData<fn(&mut A, &mut A::Context)>,
}

impl<A: Actor, F: Future> FutureInteropWrap<A, F> {
    fn new(inner: F) -> Self {
        Self {
            inner,
            phantom: PhantomData,
        }
    }
}

impl<A: Actor, F: Future> ActorFuture for FutureInteropWrap<A, F> {
    type Actor = A;
    type Output = F::Output;

    fn poll(
        self: Pin<&mut Self>,
        actor: &mut A,
        ctx: &mut A::Context,
        task: &mut Context,
    ) -> Poll<Self::Output> {
        set_actor_context(actor, ctx, || self.project().inner.poll(task))
    }
}

/// Extension trait implemented for all futures. Import this trait to bring the
/// [`interop_actor`](FutureInterop::interop_actor) and
/// [`interop_actor_boxed`](FutureInterop::interop_actor_boxed) methods into scope.
pub trait FutureInterop<A: Actor>: Future + Sized {
    /// Convert a future using the `with_ctx` or `critical_section` methods into an ActorFuture.
    fn interop_actor(self, actor: &A) -> FutureInteropWrap<A, Self>;
    /// Convert a future using the `with_ctx` or `critical_section` methods into a boxed
    /// ActorFuture.
    fn interop_actor_boxed(self, actor: &A) -> ResponseActFuture<A, Self::Output>
    where
        Self: 'static,
    {
        Box::new(self.interop_actor(actor))
    }
}

impl<A: Actor, F: Future> FutureInterop<A> for F {
    fn interop_actor(self, _actor: &A) -> FutureInteropWrap<A, Self> {
        FutureInteropWrap::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{critical_section, with_ctx, FutureInterop};
    use actix::prelude::*;

    // this is our Message
    #[derive(Message)]
    #[rtype(result = "Result<usize, ()>")] // we have to define the response type for `Sum` message
    struct Sum(usize, usize);

    // Actor definition
    struct Summator {
        field: i32,
    }

    impl Actor for Summator {
        type Context = Context<Self>;
    }

    // now we need to define `MessageHandler` for the `Sum` message.
    impl Handler<Sum> for Summator {
        type Result = ResponseActFuture<Self, Result<usize, ()>>; // <- Message response type

        fn handle(&mut self, msg: Sum, _ctx: &mut Context<Self>) -> Self::Result {
            async move {
                // Run some code with exclusive access to the actor
                critical_section::<Self, _>(async {
                    with_ctx(|a: &mut Self, _| a.field += 1);
                })
                .await;

                // Return a result
                Ok(msg.0 + msg.1)
            }
            .interop_actor_boxed(self)
        }
    }

    #[test]
    fn it_works() {
        let mut system = System::new("test");

        let res = system
            .block_on(async {
                let addr = Summator { field: 0 }.start();

                addr.send(Sum(3, 4)).await
            })
            .unwrap();

        assert_eq!(res, Ok(7));
    }
}
