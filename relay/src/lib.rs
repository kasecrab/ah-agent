//! The relay two devices meet at.
//!
//! It stores frames and hands them on. It cannot read one: everything inside
//! a frame is sealed with keys derived from a pairing code it is never given,
//! and the one key it does hold only proves a connection is allowed. What it
//! can see is how many frames there are, how big, and when — which is the
//! price of having somewhere to meet at all.
//!
//! Nothing here holds state. Every request is checked for a well-formed hub
//! name and handed to that hub, original request and all: rebuilding it would
//! mean rebuilding a WebSocket upgrade, and an upgrade that arrives as an
//! ordinary request is a bug that only shows up on a real socket.

use worker::*;

mod auth;
mod hub;
mod schema;

pub use hub::Hub;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();
    Router::new()
        .get("/health", |_, _| Response::ok("ok"))
        .on_async("/hub/:hub", to_hub)
        .on_async("/hub/:hub/provision", to_hub)
        .on_async("/hub/:hub/revoke", to_hub)
        .run(req, env)
        .await
}

async fn to_hub(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let Some(hub) = ctx.param("hub") else {
        return Response::error("no hub", 400);
    };
    // The same check the harness makes before it dials, from the same
    // function, so a name one side will build is never one the other refuses.
    if !ah_remote_proto::is_hub_id(hub) {
        return Response::error("not a hub", 400);
    }
    ctx.durable_object("HUB")?
        .id_from_name(hub)?
        .get_stub()?
        .fetch_with_request(req)
        .await
}
