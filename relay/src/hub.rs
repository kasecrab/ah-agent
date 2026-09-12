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
/// Shorter than this and a second desktop is turned away, so a second
/// connection cannot knock a working session off the air.
///
/// Three times the desktop's keepalive: a live one says something every two
/// minutes whether or not it has anything to say, so six minutes of silence
/// means it is gone rather than merely idle.
const DESK_SILENT_MS: u64 = 360_000;

/// How often a socket's own record of when it was last heard from is brought
/// up to date. Well under [`DESK_SILENT_MS`], so the judgement it feeds stays
/// sound, and far above one per frame.
const SEEN_EVERY_MS: u64 = 10_000;

/// How often one socket may ask for a replay. A `sub` can read hundreds of
/// rows and costs the asker nothing, so without this one phone in a loop
/// spends the day's read allowance on its own.
const SUB_EVERY_MS: u64 = 1_000;

/// Longest a relay key may be written. One HKDF leaf is 32 bytes, 43
/// characters of base64url; the rest is room for a longer one later.
const MAX_KEY_CHARS: usize = 128;

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
    /// When it last asked for a replay. Replays are the one thing a phone can
    /// ask for that costs hundreds of row reads, so they are spaced out.
    #[serde(default)]
    sub_ms: u64,
}

#[durable_object]
pub struct Hub {
    state: State,
    /// Kept for the provisioning secret, which is the one thing here that does
    /// not live in the object's own storage.
    env: Env,
    /// The log number the day started at. Frames taken in today are the
    /// distance from it to the newest row, which makes the day's spend a
    /// subtraction rather than a counter — exact, and costing one write a day
    /// instead of one per frame. A counter in memory would be reset by every
    /// hibernation, which is to say by every quiet ten seconds.
    day_n0: Cell<i64>,
    /// Which day `day_n0` belongs to.
    day: Cell<u64>,
    /// The newest log number, cached so the ordinary frame costs no read. Zero
    /// means this incarnation has not seen one yet and must ask.
    last_n: Cell<i64>,
}

impl DurableObject for Hub {
    fn new(state: State, env: Env) -> Self {
        console_error_panic_hook::set_once();
        // No tables yet. This constructor runs for any name anybody asks for,
        // and creating storage here meant a loop of requests for made-up hub
        // names left a SQLite file behind for each of them, on the deployer's
        // quota, with nothing to ever collect them. They are created when a
        // pairing is actually made; until then every read finds no table,
        // which the code below already treats as "no such hub".
        let hub = Self {
            state,
            env,
            day_n0: Cell::new(0),
            day: Cell::new(0),
            last_n: Cell::new(0),
        };
        // Nothing here is a count, so nothing here can be lost by being
        // evicted: both numbers are marks in a log that only goes forwards.
        hub.day
            .set(hub.meta("day").and_then(|d| d.parse().ok()).unwrap_or(0));
        hub.day_n0
            .set(hub.meta("day_n0").and_then(|c| c.parse().ok()).unwrap_or(0));
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
        // Before anything is parsed, let alone stored. A Durable Object will
        // take a 2 MB row, and there is no legitimate frame anywhere near it.
        if text.len() > ah_remote_proto::MAX_RELAY_FRAME_BYTES {
            let _ = ws.close(Some(1009), Some("too big"));
            return Ok(());
        }
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
            // A keepalive has done its whole job by arriving: being heard is
            // what stops an idle desktop being taken for a dead one.
            ("desk", Envelope::Ka { .. }) => {
                if now().saturating_sub(who.seen) > 0 {
                    who.seen = now();
                    let _ = ws.serialize_attachment(&who);
                }
                Ok(())
            }
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
        // The secret first, before anything that would say whether this hub
        // exists. Answering 409 or 410 ahead of it turns the endpoint into a
        // way to ask which hub names are real, which is the question the
        // secret is there to refuse.
        let Ok(want) = self.env.secret("AH_PROVISION_TOKEN") else {
            return Response::error(
                "this relay has no provisioning secret set; see relay/README.md",
                503,
            );
        };
        let given = req
            .headers()
            .get("x-ah-provision")
            .ok()
            .flatten()
            .unwrap_or_default();
        if !same(&given, &want.to_string()) {
            return Response::error("no", 401);
        }
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
        // A relay key is one HKDF leaf, base64url. Anything else is somebody
        // seeing how much of a megabyte they can leave behind.
        if key.len() > MAX_KEY_CHARS
            || key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Response::error("not a key", 400);
        }
        // Synchronous, and nothing is dispatched concurrently with it, so the
        // tables are in place without an atomic window to hold open.
        let _ = self.sql().exec(schema::SCHEMA, None);
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
            // Answered exactly as a wrong signature is. A hub that said "no
            // such hub" here and "no" once provisioned would tell anybody
            // holding a list of guesses which of them are real.
            return Response::error("no", 401);
        };
        let url = req.url()?;
        // The hub's own name is part of what a connect signature covers, and
        // an object does not know the name it was opened under, so it is
        // written down when the pairing is made.
        let Some(hub) = self.meta("hub") else {
            return Response::error("no", 401);
        };
        let raw = auth::decode_key(&key);
        let signature = req.headers().get("x-ah-auth").ok().flatten();
        let proof = match auth::verify(&raw, &hub, &url, signature, now()).await {
            Ok(r) => r,
            Err(Denied::Skew) => {
                return Response::from_json(&serde_json::json!({
                    "e": "skew", "server_ms": now()
                }))
                .map(|r| r.with_status(401));
            }
            Err(Denied::Signature) => return Response::error("no", 401),
        };
        // A signature holds for as long as the clocks allow, so without this
        // one that was copied — out of an access log, or off the wire — opens
        // a second socket for five minutes afterwards. Once used, never again.
        if !self.first_use(&proof.nonce) {
            return Response::error("no", 401);
        }
        let role = proof.role;

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
            sub_ms: 0,
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
        // 401 for everything, as the socket does: a hub that answered 404 when
        // it did not exist would say which names are real to anybody asking.
        let Some(key) = self.meta("relay_key") else {
            return Response::error("no", 401);
        };
        let url = req.url()?;
        let Some(hub) = self.meta("hub") else {
            return Response::error("no", 401);
        };
        let raw = auth::decode_key(&key);
        let signature = req.headers().get("x-ah-auth").ok().flatten();
        let Ok(proof) = auth::verify(&raw, &hub, &url, signature, now()).await else {
            return Response::error("no", 401);
        };
        if !self.first_use(&proof.nonce) {
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
        if !self.within_the_day() {
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
        self.last_n.set(n as i64);

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
        // Two guards, both on what this socket has already been given rather
        // than on what it says it wants. A replay is the only thing a phone
        // can ask for that reads hundreds of rows, and asking again for what
        // it has already had is the shape every loop takes.
        if now().saturating_sub(who.sub_ms) < SUB_EVERY_MS {
            return Ok(());
        }
        if who.sub && since < who.since {
            return Ok(());
        }
        who.sub_ms = now();

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
        // Clamped, not cast. `u64::MAX as i64` is -1, and `WHERE n > -1`
        // replays the whole log from the first row — which is the opposite of
        // what asking for everything after the newest frame should do, and
        // slips past the guard above because the number is not small.
        let from = since.min(i64::MAX as u64) as i64;
        let rows: Vec<Row> = self
            .sql()
            .exec(
                "SELECT n, link, seq, ct FROM log WHERE n > ?1 ORDER BY n LIMIT ?2",
                Some(vec![from.into(), limit.into()]),
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

    /// Whether the day's allowance still has room in it. The allowance belongs
    /// to whoever deployed this, so a code that got out cannot quietly empty
    /// it.
    ///
    /// Measured rather than counted. `n` only ever goes up, so the frames
    /// taken in today are the distance from the number the day started at —
    /// which survives eviction, unlike a counter in memory, and costs one
    /// write a day, unlike a counter in the table. An object that hibernates
    /// between every frame, which is what a slow flood looks like, is counted
    /// exactly the same as one that never sleeps.
    fn within_the_day(&self) -> bool {
        let today = now() / (24 * 60 * 60 * 1000);
        let newest = self.newest();
        if self.day.get() != today {
            self.day.set(today);
            self.day_n0.set(newest);
            self.set_meta("day", &today.to_string());
            self.set_meta("day_n0", &newest.to_string());
            return true;
        }
        newest - self.day_n0.get() < schema::FRAMES_PER_DAY
    }

    /// Whether this nonce has been signed with before. Remembered for twice
    /// the skew window, which is longer than any signature stays valid, and
    /// tidied on the same pass so the table cannot grow.
    fn first_use(&self, nonce: &str) -> bool {
        if nonce.is_empty() || nonce.len() > 64 {
            return false;
        }
        #[derive(Deserialize)]
        struct Row {
            seen: i64,
        }
        let rows: Vec<Row> = self
            .sql()
            .exec(
                "SELECT COUNT(*) AS seen FROM nonces WHERE n = ?1",
                Some(vec![nonce.into()]),
            )
            .ok()
            .and_then(|c| c.to_array().ok())
            .unwrap_or_default();
        if rows.first().map(|r| r.seen).unwrap_or(0) > 0 {
            return false;
        }
        let _ = self.sql().exec(
            "INSERT OR IGNORE INTO nonces (n, ts) VALUES (?1, ?2)",
            Some(vec![nonce.into(), (now() as i64).into()]),
        );
        let _ = self.sql().exec(
            "DELETE FROM nonces WHERE ts < ?1",
            Some(vec![(now() as i64 - schema::NONCE_KEEP_MS).into()]),
        );
        true
    }

    /// The newest log number, asked for only when this incarnation has not
    /// seen one yet.
    fn newest(&self) -> i64 {
        if self.last_n.get() > 0 {
            return self.last_n.get();
        }
        #[derive(Deserialize)]
        struct Row {
            n: Option<i64>,
        }
        let n = self
            .sql()
            .exec("SELECT MAX(n) AS n FROM log", None)
            .ok()
            .and_then(|c| c.to_array::<Row>().ok())
            .and_then(|rows| rows.first().and_then(|r| r.n))
            .unwrap_or(0);
        self.last_n.set(n);
        n
    }

    /// Trim the log, in one pass, to what is worth keeping.
    ///
    /// Three bounds, because a count alone is not one: twenty thousand frames
    /// of the largest size the relay will take is two and a half gigabytes,
    /// and storage is the thing being paid for.
    fn prune(&self) {
        let _ = self.sql().exec(
            "DELETE FROM log WHERE n <= (SELECT MAX(n) - ?1 FROM log)",
            Some(vec![schema::LOG_KEEP.into()]),
        );
        let cutoff = now() as i64 - schema::LOG_RETAIN_MS;
        let _ = self
            .sql()
            .exec("DELETE FROM log WHERE ts < ?1", Some(vec![cutoff.into()]));
        // And by size: keep dropping the oldest until what is left fits.
        // Written as one statement so it costs one pass rather than a loop of
        // reads, and it only ever deletes rows older than what it keeps.
        let _ = self.sql().exec(
            "DELETE FROM log WHERE n <= (
               SELECT COALESCE(MAX(n), 0) FROM (
                 SELECT n, SUM(LENGTH(ct)) OVER (ORDER BY n DESC) AS after
                 FROM log
               ) WHERE after > ?1
             )",
            Some(vec![schema::LOG_BYTES.into()]),
        );
        // A device row is written on every connect and never read after the
        // socket closes; without this the table is a list of every phone that
        // ever attached.
        let _ = self.sql().exec(
            "DELETE FROM devices WHERE last_ms < ?1",
            Some(vec![(now() as i64 - schema::DEVICE_KEEP_MS).into()]),
        );
    }
}

/// Compare without leaking where two strings first differ. The length is not
/// hidden — it is not a secret worth the trouble — but the contents are.
fn same(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
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
