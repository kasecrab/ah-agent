//! One hub per pairing: two sockets, a log, and nothing it can read.
//!
//! Rules this file lives by, because breaking either is expensive rather than
//! merely wrong:
//!
//! * **`accept_websocket_with_tags`, never `accept`.** An accepted socket
//!   keeps the object in memory, and an idle object holding one costs about
//!   10,800 GB-s a day against a 13,000 GB-s daily allowance — one idle user
//!   would spend the whole free plan on doing nothing.
//! * **Trim in batches, never per insert.** Deleted rows count against the
//!   same daily row allowance as written ones.

use ah_remote_proto::{Ctl, Envelope, PROTO, Role};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use worker::*;

use crate::auth::{self, Denied};
use crate::schema;

/// How long a desktop may say nothing before another one may take its place.
/// Shorter than this and a second desktop is turned away, so replaying a
/// captured connect token cannot knock a working session off the air.
const DESK_SILENT_MS: u64 = 90_000;

/// How often a socket's own record of when it was last heard from is brought
/// up to date. Well under [`DESK_SILENT_MS`], so the judgement it feeds stays
/// sound, and far above one per frame.
const SEEN_EVERY_MS: u64 = 10_000;

/// What a socket remembers about itself. In memory is not enough: the object
/// is evicted between messages, and this is what comes back with it.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Attach {
    role: String,
    /// Opaque, and opaque on purpose: a connection is identified by the nonce
    /// it signed with, never by anything a person would recognise.
    dev: String,
    /// Whether this phone has asked to be sent frames yet.
    sub: bool,
    /// The last log number it has been sent.
    since: u64,
    /// When it was last heard from.
    seen: u64,
}

#[durable_object]
pub struct Hub {
    state: State,
    /// Frames taken in today, counted here and written down rarely. Counting
    /// in the table instead would cost a row write per frame on top of the
    /// frame itself, which is three times the storage budget to enforce a
    /// limit on storage. Losing a few counts when the object is evicted is
    /// the cheaper mistake.
    spent: Cell<i64>,
    /// Which day `spent` is counting.
    day: Cell<u64>,
}

impl DurableObject for Hub {
    fn new(state: State, _env: Env) -> Self {
        console_error_panic_hook::set_once();
        // Synchronous, and this runs before any request is dispatched, so the
        // tables are there without an atomic window to hold open.
        let _ = state.storage().sql().exec(schema::SCHEMA, None);
        let hub = Self {
            state,
            spent: Cell::new(0),
            day: Cell::new(0),
        };
        // Picked back up where the last incarnation left off, give or take
        // the few it had not written down yet.
        hub.day.set(hub.meta("day").and_then(|d| d.parse().ok()).unwrap_or(0));
        hub.spent
            .set(hub.meta("frames").and_then(|c| c.parse().ok()).unwrap_or(0));
        hub
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        // The path arrives as the router saw it, so what is matched on is its
        // last segment: `/hub/<id>` is a socket, `/hub/<id>/provision` and
        // `/hub/<id>/revoke` are what they say.
        let url = req.url()?;
        match url.path().rsplit('/').next().unwrap_or_default() {
            "provision" => self.provision(req).await,
            "revoke" => self.revoke(req).await,
            _ => self.socket(req).await,
        }
    }

    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        let WebSocketIncomingMessage::String(text) = message else {
            // The protocol is text. A binary frame is either a different
            // protocol or a probe, and neither is worth a reply.
            let _ = ws.close(Some(1003), Some("text only"));
            return Ok(());
        };
        let Some(mut who) = attach(&ws) else {
            let _ = ws.close(Some(1011), Some("no attachment"));
            return Ok(());
        };
        // Freshened rather than rewritten: liveness is judged in tens of
        // seconds, and an attachment written per message is a write per
        // message for no gain.
        if now().saturating_sub(who.seen) > SEEN_EVERY_MS {
            who.seen = now();
            let _ = ws.serialize_attachment(&who);
        }

        let Ok(frame) = serde_json::from_str::<Envelope>(&text) else {
            return send(&ws, &Envelope::control(Ctl::Offline));
        };
        if frame.version() != PROTO {
            let _ = ws.close(Some(1002), Some("protocol"));
            return Ok(());
        }

        match (who.role.as_str(), frame) {
            ("desk", Envelope::Pub { link, seq, ct, .. }) => self.publish(&ws, link, seq, ct),
            ("phone", Envelope::Cmd { .. }) => self.command(&ws, &text),
            ("phone", Envelope::Sub { since, max, .. }) => self.subscribe(&ws, who, since, max),
            // Anything else is a frame going the wrong way. Dropped rather
            // than answered: there is nothing useful to say about it.
            _ => Ok(()),
        }
    }

    async fn websocket_close(
        &self,
        ws: WebSocket,
        _code: usize,
        _reason: String,
        _clean: bool,
    ) -> Result<()> {
        if let Some(who) = attach(&ws) {
            let _ = self.sql().exec(
                "UPDATE devices SET last_ms = ?1 WHERE id = ?2",
                Some(vec![(now() as i64).into(), who.dev.into()]),
            );
        }
        // Nothing left to serve; wake up in six hours to tidy the log.
        if self.state.get_websockets().is_empty() {
            self.state
                .storage()
                .set_alarm(6 * 60 * 60 * 1000)
                .await
                .ok();
        }
        Ok(())
    }

    async fn alarm(&self) -> Result<Response> {
        self.prune();
        if self.state.get_websockets().is_empty() {
            // Still nobody. A hub nobody has used for a month is one nobody
            // is coming back to; pairing again is cheap and storage is not.
            let idle = self
                .meta("last_seen")
                .and_then(|v| v.parse::<u64>().ok())
                .map(|t| now().saturating_sub(t))
                .unwrap_or(0);
            if idle > 30 * 24 * 60 * 60 * 1000 {
                self.state.storage().delete_all().await.ok();
                return Response::ok("gone");
            }
            self.state
                .storage()
                .set_alarm(6 * 60 * 60 * 1000)
                .await
                .ok();
        }
        Response::ok("tidied")
    }
}

impl Hub {
    fn sql(&self) -> SqlStorage {
        self.state.storage().sql()
    }

    fn meta(&self, key: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Row {
            v: String,
        }
        let rows: Vec<Row> = self
            .sql()
            .exec("SELECT v FROM meta WHERE k = ?1", Some(vec![key.into()]))
            .ok()?
            .to_array()
            .ok()?;
        rows.into_iter().next().map(|r| r.v)
    }

    fn set_meta(&self, key: &str, value: &str) {
        let _ = self.sql().exec(
            "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = ?2",
            Some(vec![key.into(), value.into()]),
        );
    }

    /// Tell a hub its key, once and only once. Whoever guesses a hub name that
    /// is already in use gets nothing, and a name nobody has provisioned
    /// stores nothing at all, so guessing costs one request and evicts.
    async fn provision(&self, mut req: Request) -> Result<Response> {
        if self.meta("revoked").is_some() {
            return Response::error("revoked", 410);
        }
        if self.meta("relay_key").is_some() {
            return Response::error("already paired", 409);
        }
        // The name is the one the router resolved this object from, not one
        // the caller asserts: a pairing can only ever be made for the hub it
        // was addressed to.
        let hub = req
            .url()?
            .path()
            .trim_end_matches("/provision")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if !ah_remote_proto::is_hub_id(&hub) {
            return Response::error("not a hub", 400);
        }
        let body: serde_json::Value = req.json().await?;
        let Some(key) = body.get("relay_key").and_then(|v| v.as_str()) else {
            return Response::error("no key", 400);
        };
        self.set_meta("relay_key", key);
        self.set_meta("hub", &hub);
        self.set_meta("created_ms", &now().to_string());
        self.set_meta("last_seen", &now().to_string());
        Response::ok("paired")
    }

    async fn socket(&self, req: Request) -> Result<Response> {
        if self.meta("revoked").is_some() {
            return Response::error("revoked", 410);
        }
        let Some(key) = self.meta("relay_key") else {
            // Not provisioned: nothing is stored and nothing is said about
            // why, so a name that was guessed looks the same as one that is
            // simply wrong.
            return Response::error("no such hub", 404);
        };
        let url = req.url()?;
        // The hub's own name is part of what a connect signature covers, and
        // an object does not know the name it was opened under, so it is
        // written down when the pairing is made.
        let Some(hub) = self.meta("hub") else {
            return Response::error("no such hub", 404);
        };
        let raw = auth::decode_key(&key);
        let role = match auth::verify(&raw, &hub, &url, now()).await {
            Ok(r) => r,
            Err(Denied::Skew) => {
                return Response::from_json(&serde_json::json!({
                    "e": "skew", "server_ms": now()
                }))
                .map(|r| r.with_status(401));
            }
            Err(Denied::Signature) => return Response::error("no", 401),
        };

        let dev = url
            .query_pairs()
            .find(|(k, _)| k == "n")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();

        if role == Role::Desk {
            // One desktop at a time. A live one is not pushed aside by
            // whoever connects next, which is what stops a replayed connect
            // token from knocking a working session off the air.
            for other in self.state.get_websockets_with_tag("desk") {
                let quiet = attach(&other).map(|a| now().saturating_sub(a.seen));
                if quiet.is_some_and(|q| q < DESK_SILENT_MS) {
                    return Response::error("a desktop is already connected", 409);
                }
                let _ = other.close(Some(4010), Some("replaced"));
            }
        } else {
            let phones = self.state.get_websockets_with_tag("phone");
            if phones.len() >= ah_remote_proto::MAX_DEVICES {
                if let Some(oldest) = phones
                    .iter()
                    .min_by_key(|w| attach(w).map(|a| a.seen).unwrap_or(0))
                {
                    let _ = oldest.close(Some(4009), Some("too many"));
                }
            }
        }

        let pair = WebSocketPair::new()?;
        let tag = role.as_str();
        self.state
            .accept_websocket_with_tags(&pair.server, &[tag, &dev]);
        let who = Attach {
            role: tag.to_string(),
            dev: dev.clone(),
            sub: false,
            since: 0,
            seen: now(),
        };
        pair.server.serialize_attachment(&who)?;
        let _ = self.sql().exec(
            "INSERT INTO devices (id, role, first_ms, last_ms) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET last_ms = ?3",
            Some(vec![dev.into(), tag.into(), (now() as i64).into()]),
        );
        self.set_meta("last_seen", &now().to_string());
        Response::from_websocket(pair.client)
    }

    /// End the pairing. The log and the device list go; the fact that it was
    /// revoked stays, so the name can never be provisioned by somebody else.
    async fn revoke(&self, req: Request) -> Result<Response> {
        let Some(key) = self.meta("relay_key") else {
            return Response::error("no such hub", 404);
        };
        let url = req.url()?;
        let Some(hub) = self.meta("hub") else {
            return Response::error("no such hub", 404);
        };
        let raw = auth::decode_key(&key);
        if auth::verify(&raw, &hub, &url, now()).await.is_err() {
            return Response::error("no", 401);
        }
        self.set_meta("revoked", "1");
        let _ = self.sql().exec("DELETE FROM log", None);
        let _ = self.sql().exec("DELETE FROM devices", None);
        for ws in self.state.get_websockets() {
            let _ = send(&ws, &Envelope::control(Ctl::Revoked));
            let _ = ws.close(Some(4012), Some("revoked"));
        }
        Response::ok("revoked")
    }

    /// A frame from the desktop: stored, numbered, and handed to every phone
    /// that has asked for it.
    fn publish(&self, ws: &WebSocket, link: String, seq: u64, ct: String) -> Result<()> {
        if !self.spend_one() {
            return send(ws, &Envelope::control(Ctl::Quota));
        }
        #[derive(Deserialize)]
        struct Row {
            n: i64,
        }
        let rows: Vec<Row> = self
            .sql()
            .exec(
                "INSERT INTO log (ts, link, seq, ct) VALUES (?1, ?2, ?3, ?4) RETURNING n",
                Some(vec![
                    (now() as i64).into(),
                    link.clone().into(),
                    (seq as i64).into(),
                    ct.clone().into(),
                ]),
            )?
            .to_array()?;
        let n = rows.first().map(|r| r.n).unwrap_or(0) as u64;

        // The cursor is not written down here. A phone that is connected is
        // being handed frames as they arrive, and a phone that is not says
        // where it got to when it comes back. Writing it per phone per frame
        // would be the most expensive line in the file and buy nothing: a
        // frame delivered twice is refused by its sequence number anyway.
        let frame = Envelope::event(&link, seq, ct, n);
        for phone in self.state.get_websockets_with_tag("phone") {
            if attach(&phone).is_some_and(|a| a.sub) {
                let _ = send(&phone, &frame);
            }
        }
        if n % schema::PRUNE_EVERY as u64 == 0 {
            self.prune();
        }
        Ok(())
    }

    /// A frame from a phone. Commands are not stored: if the desktop is not
    /// here, "your computer is not connected" is a better answer than delivery
    /// an hour from now.
    fn command(&self, ws: &WebSocket, text: &str) -> Result<()> {
        let desks = self.state.get_websockets_with_tag("desk");
        let Some(desk) = desks.first() else {
            return send(ws, &Envelope::control(Ctl::Offline));
        };
        desk.send_with_str(text)
    }

    /// A phone saying where it got to. What it missed is replayed from the
    /// log, sealed exactly as it was stored, so it still opens.
    fn subscribe(&self, ws: &WebSocket, mut who: Attach, since: u64, max: u32) -> Result<()> {
        #[derive(Deserialize)]
        struct Row {
            n: i64,
            link: String,
            seq: i64,
            ct: String,
        }
        #[derive(Deserialize)]
        struct Oldest {
            n: Option<i64>,
        }
        let oldest: Vec<Oldest> = self
            .sql()
            .exec("SELECT MIN(n) AS n FROM log", None)?
            .to_array()?;
        let first = oldest.first().and_then(|o| o.n).unwrap_or(0) as u64;
        if since > 0 && first > since + 1 {
            // What it asked for has been trimmed away. Better to say so than
            // to hand over a stream with a hole in the middle of it.
            send(
                ws,
                &Envelope::Gap {
                    v: PROTO,
                    from: first,
                },
            )?;
        }

        // One replay, not a whole history: a phone further behind than this
        // asks again, and the handler stays short.
        let limit = max.clamp(1, 500) as i64;
        let rows: Vec<Row> = self
            .sql()
            .exec(
                "SELECT n, link, seq, ct FROM log WHERE n > ?1 ORDER BY n LIMIT ?2",
                Some(vec![(since as i64).into(), limit.into()]),
            )?
            .to_array()?;
        for r in &rows {
            send(
                ws,
                &Envelope::event(&r.link, r.seq as u64, r.ct.clone(), r.n as u64),
            )?;
        }
        who.sub = true;
        who.since = rows.last().map(|r| r.n as u64).unwrap_or(since);
        ws.serialize_attachment(&who)?;
        let _ = self.sql().exec(
            "UPDATE devices SET cursor = ?1 WHERE id = ?2",
            Some(vec![(who.since as i64).into(), who.dev.clone().into()]),
        );
        Ok(())
    }

    /// Count a frame against the day's allowance. The allowance belongs to
    /// whoever deployed this, so a code that got out cannot quietly empty it.
    fn spend_one(&self) -> bool {
        let today = now() / (24 * 60 * 60 * 1000);
        if self.day.get() != today {
            self.day.set(today);
            self.spent.set(0);
            self.set_meta("day", &today.to_string());
            self.set_meta("frames", "0");
        }
        let count = self.spent.get() + 1;
        self.spent.set(count);
        if count > schema::FRAMES_PER_DAY {
            return false;
        }
        // Written down occasionally rather than every time, on the same beat
        // as the trim, so the count survives an eviction without costing a
        // row per frame.
        if count % schema::PRUNE_EVERY == 0 {
            self.set_meta("frames", &count.to_string());
        }
        true
    }

    /// Trim the log, in one pass, to what is worth keeping.
    fn prune(&self) {
        let _ = self.sql().exec(
            "DELETE FROM log WHERE n <= (SELECT MAX(n) - ?1 FROM log)",
            Some(vec![schema::LOG_KEEP.into()]),
        );
        let cutoff = now() as i64 - schema::LOG_RETAIN_MS;
        let _ = self.sql().exec(
            "DELETE FROM log WHERE ts < ?1",
            Some(vec![cutoff.into()]),
        );
    }
}

fn attach(ws: &WebSocket) -> Option<Attach> {
    ws.deserialize_attachment::<Attach>().ok().flatten()
}

fn send(ws: &WebSocket, frame: &Envelope) -> Result<()> {
    ws.send_with_str(serde_json::to_string(frame).unwrap_or_default())
}

/// The runtime's clock. `SystemTime::now()` panics on this target.
fn now() -> u64 {
    Date::now().as_millis()
}
