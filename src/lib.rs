use std::any::Any;
use std::marker::PhantomData;
use std::future::Future;
use std::task::{Poll, Context};
use std::pin::Pin;

use actix::{Actor, ActorFuture, AsyncContext};
use scoped_tls::scoped_thread_local;
use pin_project::pin_project;

use self::local_handle::local_handle;

mod local_handle;

scoped_thread_local!(static mut CURRENT_ACTOR: dyn Any);
scoped_thread_local!(static mut CURRENT_CONTEXT: dyn Any);

fn set_actor_context<A, F, R>(actor: &mut A, ctx: &mut A::Context, f: F) -> R
where
    A: Actor,
    F: FnOnce() -> R,
{
    CURRENT_ACTOR.set(actor, || {
        CURRENT_CONTEXT.set(ctx, f)
    })
}

pub fn with_ctx<A, F, R>(f: F) -> R
where
    A: Actor + 'static,
    F: FnOnce(&mut A, &mut A::Context) -> R
{
    CURRENT_ACTOR.with(|actor| {
        CURRENT_CONTEXT.with(|ctx| {
            let actor = actor.downcast_mut().expect("Future was spawned onto the wrong actor");
            let ctx = ctx.downcast_mut().expect("Future was spawned onto the wrong actor");
            f(actor, ctx)
        })
    })
}

pub fn critical_section<A: Actor, F: Future>(f: F) -> impl Future<Output=F::Output>
where
    A::Context: AsyncContext<A>,
    F: 'static,
{
    let (f, handle) = local_handle(f);
    with_ctx(|actor: &mut A, ctx: &mut A::Context| {
        ctx.wait(f.interop_actor(actor))
    });
    handle
}

#[pin_project]
#[derive(Debug)]
pub struct FutureInteropWrap<A: Actor, F> {
    #[pin]
    inner: F,
    phantom: PhantomData<fn(&mut A, &mut A::Context)>,
}

impl<A: Actor, F: Future> FutureInteropWrap<A, F> {
    pub fn new(inner: F) -> Self {
        Self { inner, phantom: PhantomData }
    }
}

impl<A: Actor, F: Future> ActorFuture for FutureInteropWrap<A, F> {
    type Actor = A;
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, actor: &mut A, ctx: &mut A::Context, task: &mut Context) -> Poll<Self::Output> {
        set_actor_context(actor, ctx, || {
            self.project().inner.poll(task)
        })
    }
}

pub trait FutureInterop<A: Actor>: Future + Sized {
    fn interop_actor(self, actor: &A) -> FutureInteropWrap<A, Self>;
    fn interop_actor_boxed(self, actor: &A) -> Box<FutureInteropWrap<A, Self>> {
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
    use super::{FutureInterop, with_ctx, critical_section};
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
                }).await;

                // Return a result
                Ok(msg.0 + msg.1)
            }.interop_actor_boxed(self)
        }
    }

    #[test]
    fn it_works() {
        let mut system = System::new("test");

        let res = system.block_on(async {
            let addr = Summator { field: 0 }.start();
    
            addr.send(Sum(3, 4)).await
        }).unwrap();

        assert_eq!(res, Ok(7));
    }
}
