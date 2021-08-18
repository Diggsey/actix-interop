// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Use async/await syntax with actix actors.
//!
//! # Example
//!
//! This example shows how you could implement a generic "pipeline adapter"
//! which allows turning any `Sink`/`Stream` pair (forming a request/response
//! pipeline) into an actix Actor.
//!
//! Responses are matched up with their requests according to the order
//! in which they were sent (so the first response corresponds to the
//! first request sent to the `Sink`, etc). This requires that our "send"
//! operations are strictly ordered, and this is difficult to achieve
//! in actix because async operations are normally allowed to interleave.
//!
//! Furthermore, although the sends must be atomic, we also want to be
//! able to have a large number of requests in-flight at any given time,
//! so the receiving part of the message handler must not require exclusive
//! access to the actor while it is waiting. As a result, abstractions
//! like the `AtomicResponse` type are too simplistic to help.
//!
//! To solve this problem, we use the [`critical_section`](critical_section)
//! function to allow specific parts of our message handler to be  atomic.
//!
//! ```rust
//! use std::collections::VecDeque;
//! use std::pin::Pin;
//!
//! use futures::{Sink, SinkExt, Stream, channel::oneshot};
//! use actix::prelude::*;
//!
//! use actix_interop::{FutureInterop, with_ctx, critical_section};
//!
//! // Define our actor
//! pub struct PipelineAdapter<Req, Res, Err> {
//!     sink: Option<Pin<Box<dyn Sink<Req, Error=Err>>>>,
//!     in_flight_reqs: VecDeque<oneshot::Sender<Result<Res, Err>>>,
//! }
//!
//! // Implement a constructor
//! impl<Req, Res, Err> PipelineAdapter<Req, Res, Err>
//! where
//!     Req: 'static,
//!     Res: 'static,
//!     Err: 'static,
//! {
//!     pub fn new<Si, St>(sink: Si, stream: St) -> Addr<Self>
//!     where
//!         Si: Sink<Req, Error=Err> + 'static,
//!         St: Stream<Item=Res> + 'static,
//!     {
//!         // Convert to a boxed trait object
//!         let sink: Box<dyn Sink<Req, Error=Err>> = Box::new(sink);
//!
//!         Self::create(|ctx| {
//!             ctx.add_stream(stream);
//!             Self {
//!                 sink: Some(sink.into()),
//!                 in_flight_reqs: VecDeque::new(),
//!             }
//!         })
//!     }
//! }
//!
//! // Tell actix this is an actor using the default Context type
//! impl<Req, Res, Err> Actor for PipelineAdapter<Req, Res, Err>
//! where
//!     Req: 'static,
//!     Res: 'static,
//!     Err: 'static,
//! {
//!     type Context = Context<Self>;
//! }
//!
//! // Transform actix messages into the pipelines request/response protocol
//! impl<Req, Res, Err> Handler<Req> for PipelineAdapter<Req, Res, Err>
//! where
//!     Req: Message<Result=Result<Res, Err>> + 'static,
//!     Res: 'static,
//!     Err: 'static,
//! {
//!     type Result = ResponseActFuture<Self, Result<Res, Err>>; // <- Message response type
//!
//!     fn handle(&mut self, msg: Req, _ctx: &mut Context<Self>) -> Self::Result {
//!         async move {
//!             let (tx, rx) = oneshot::channel();
//!
//!             // Perform sends in a critical section so they are strictly ordered
//!             critical_section::<Self, _>(async {
//!                 // Take the sink from the actor state
//!                 let mut sink = with_ctx(|actor: &mut Self, _| actor.sink.take())
//!                     .expect("Sink to be present");
//!                 
//!                 // Send the request
//!                 let res = sink.send(msg).await;
//!
//!                 // Put the sink back, and if the send was successful,
//!                 // record the in-flight request.
//!                 with_ctx(|actor: &mut Self, _| {
//!                     actor.sink = Some(sink);
//!                     match res {
//!                         Ok(()) => actor.in_flight_reqs.push_back(tx),
//!                         Err(e) => {
//!                             // Don't care if the receiver has gone away
//!                             let _ = tx.send(Err(e));
//!                         }
//!                     }
//!                 });
//!             })
//!             .await;
//!
//!             // Wait for the result concurrently, so many requests can
//!             // be pipelined at the same time.
//!             rx.await.expect("Sender should not be dropped")
//!         }
//!         .interop_actor_boxed(self)
//!     }
//! }
//!
//! // Process responses
//! impl<Req, Res, Err> StreamHandler<Res> for PipelineAdapter<Req, Res, Err>
//! where
//!     Req: 'static,
//!     Res: 'static,
//!     Err: 'static,
//! {
//!     fn handle(&mut self, msg: Res, _ctx: &mut Context<Self>) {
//!         // When we receive a response, just pull the first in-flight
//!         // request and forward on the result.
//!         let _ = self.in_flight_reqs
//!             .pop_front()
//!             .expect("There to be an in-flight request")
//!             .send(Ok(msg));
//!     }
//! }
//! ```
//!

#![deny(missing_docs, warnings)]

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix::{Actor, ActorFuture, ActorStream, AsyncContext, ResponseActFuture};
use futures::Stream;
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

impl<A: Actor, F: Future> ActorFuture<A> for FutureInteropWrap<A, F> {
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
        Box::pin(self.interop_actor(actor))
    }
}

impl<A: Actor, F: Future> FutureInterop<A> for F {
    fn interop_actor(self, _actor: &A) -> FutureInteropWrap<A, Self> {
        FutureInteropWrap::new(self)
    }
}

/// Stream to ActorStream adapter returned by [`interop_actor`](StreamInterop::interop_actor).
#[pin_project]
#[derive(Debug)]
pub struct StreamInteropWrap<A: Actor, S> {
    #[pin]
    inner: S,
    phantom: PhantomData<fn(&mut A, &mut A::Context)>,
}

impl<A: Actor, S: Stream> StreamInteropWrap<A, S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            phantom: PhantomData,
        }
    }
}

impl<A: Actor, S: Stream> ActorStream<A> for StreamInteropWrap<A, S> {
    type Item = S::Item;

    fn poll_next(
        self: Pin<&mut Self>,
        actor: &mut A,
        ctx: &mut A::Context,
        task: &mut Context,
    ) -> Poll<Option<Self::Item>> {
        set_actor_context(actor, ctx, || self.project().inner.poll_next(task))
    }
}

/// Extension trait implemented for all streams. Import this trait to bring the
/// [`interop_actor`](StreamInterop::interop_actor) and
/// [`interop_actor_boxed`](StreamInterop::interop_actor_boxed) methods into scope.
pub trait StreamInterop<A: Actor>: Stream + Sized {
    /// Convert a stream using the `with_ctx` or `critical_section` methods into an ActorStream.
    fn interop_actor(self, actor: &A) -> StreamInteropWrap<A, Self>;
    /// Convert a stream using the `with_ctx` or `critical_section` methods into a boxed
    /// ActorStream.
    fn interop_actor_boxed(self, actor: &A) -> Box<dyn ActorStream<A, Item = Self::Item>>
    where
        Self: 'static,
    {
        Box::new(self.interop_actor(actor))
    }
}

impl<A: Actor, S: Stream> StreamInterop<A> for S {
    fn interop_actor(self, _actor: &A) -> StreamInteropWrap<A, Self> {
        StreamInteropWrap::new(self)
    }
}

#[cfg(test)]
mod tests {

    use super::{critical_section, with_ctx, FutureInterop};
    use actix::prelude::*;

    #[derive(Message)]
    #[rtype(result = "Result<i32, ()>")] // we have to define the response type for `Sum` message
    struct Sum(i32);

    struct Summator {
        field: i32,
    }

    impl Actor for Summator {
        type Context = Context<Self>;
    }

    impl Handler<Sum> for Summator {
        type Result = ResponseActFuture<Self, Result<i32, ()>>; // <- Message response type

        fn handle(&mut self, msg: Sum, _ctx: &mut Context<Self>) -> Self::Result {
            async move {
                // Run some code with exclusive access to the actor
                let accum = critical_section::<Self, _>(async {
                    with_ctx(move |a: &mut Self, _| {
                        a.field += msg.0;
                        a.field
                    })
                })
                .await;

                // Return a result
                Ok(accum)
            }
            .interop_actor_boxed(self)
        }
    }

    impl StreamHandler<i32> for Summator {
        fn handle(&mut self, msg: i32, _ctx: &mut Context<Self>) {
            self.field += msg;
        }
        fn finished(&mut self, _ctx: &mut Context<Self>) {
            assert_eq!(self.field, 10);
            System::current().stop();
        }
    }

    #[actix::test]
    async fn can_run_future() -> Result<(), Box<dyn std::error::Error>> {
        let addr = Summator { field: 0 }.start();

        addr.send(Sum(3)).await.unwrap().unwrap();
        let res = addr.send(Sum(4)).await?;

        assert_eq!(res, Ok(7));
        Ok(())
    }

    #[actix::test]
    async fn can_run_stream() {
        Summator::create(|ctx| {
            ctx.add_stream(futures::stream::iter(1..5));
            Summator { field: 0 }
        });
    }

    mod pipeline {}
}
