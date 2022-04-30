# actix-interop
[![CI](https://github.com/Diggsey/actix-interop/workflows/CI/badge.svg)](https://github.com/Diggsey/actix-interop/actions)

[Documentation](https://docs.rs/actix-interop)

Allows using async/await syntax to implement actix actors, and provides a
convenient way to control access to an actor's state.

```toml
# Cargo.toml
[dependencies]
actix-interop = "0.3"
```

# Example

This example shows how you could implement a generic "pipeline adapter"
which allows turning any `Sink`/`Stream` pair (forming a request/response
pipeline) into an actix Actor.

Responses are matched up with their requests according to the order
in which they were sent (so the first response corresponds to the
first request sent to the `Sink`, etc). This requires that our "send"
operations are strictly ordered, and this is difficult to achieve
in actix because async operations are normally allowed to interleave.

Furthermore, although the sends must be atomic, we also want to be
able to have a large number of requests in-flight at any given time,
so the receiving part of the message handler must not require exclusive
access to the actor while it is waiting. As a result, abstractions
like the `AtomicResponse` type are too simplistic to help.

To solve this problem, we use the [`critical_section`](https://docs.rs/actix-interop/latest/actix_interop/fn.critical_section.html)
function to allow specific parts of our message handler to be  atomic.

```rust
use std::collections::VecDeque;
use std::pin::Pin;

use futures::{Sink, SinkExt, Stream, channel::oneshot};
use actix::prelude::*;

use actix_interop::{FutureInterop, with_ctx, critical_section};

// Define our actor
pub struct PipelineAdapter<Req, Res, Err> {
    sink: Option<Pin<Box<dyn Sink<Req, Error=Err>>>>,
    in_flight_reqs: VecDeque<oneshot::Sender<Result<Res, Err>>>,
}

// Implement a constructor
impl<Req, Res, Err> PipelineAdapter<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    pub fn new<Si, St>(sink: Si, stream: St) -> Addr<Self>
    where
        Si: Sink<Req, Error=Err> + 'static,
        St: Stream<Item=Res> + 'static,
    {
        // Convert to a boxed trait object
        let sink: Box<dyn Sink<Req, Error=Err>> = Box::new(sink);

        Self::create(|ctx| {
            ctx.add_stream(stream);
            Self {
                sink: Some(sink.into()),
                in_flight_reqs: VecDeque::new(),
            }
        })
    }
}

// Tell actix this is an actor using the default Context type
impl<Req, Res, Err> Actor for PipelineAdapter<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    type Context = Context<Self>;
}

// Transform actix messages into the pipelines request/response protocol
impl<Req, Res, Err> Handler<Req> for PipelineAdapter<Req, Res, Err>
where
    Req: Message<Result=Result<Res, Err>> + 'static,
    Res: 'static,
    Err: 'static,
{
    type Result = ResponseActFuture<Self, Result<Res, Err>>; // <- Message response type

    fn handle(&mut self, msg: Req, _ctx: &mut Context<Self>) -> Self::Result {
        async move {
            let (tx, rx) = oneshot::channel();

            // Perform sends in a critical section so they are strictly ordered
            critical_section::<Self, _>(async {
                // Take the sink from the actor state
                let mut sink = with_ctx(|actor: &mut Self, _| actor.sink.take())
                    .expect("Sink to be present");
                
                // Send the request
                let res = sink.send(msg).await;

                // Put the sink back, and if the send was successful,
                // record the in-flight request.
                with_ctx(|actor: &mut Self, _| {
                    actor.sink = Some(sink);
                    match res {
                        Ok(()) => actor.in_flight_reqs.push_back(tx),
                        Err(e) => {
                            // Don't care if the receiver has gone away
                            let _ = tx.send(Err(e));
                        }
                    }
                });
            })
            .await;

            // Wait for the result concurrently, so many requests can
            // be pipelined at the same time.
            rx.await.expect("Sender should not be dropped")
        }
        .interop_actor_boxed(self)
    }
}

// Process responses
impl<Req, Res, Err> StreamHandler<Res> for PipelineAdapter<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    fn handle(&mut self, msg: Res, _ctx: &mut Context<Self>) {
        // When we receive a response, just pull the first in-flight
        // request and forward on the result.
        let _ = self.in_flight_reqs
            .pop_front()
            .expect("There to be an in-flight request")
            .send(Ok(msg));
    }
}
```

# License

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in actix-interop by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
