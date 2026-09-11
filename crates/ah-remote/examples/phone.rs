//! The phone, from a terminal.
//!
//! Attaches to a paired machine, prints what it says, and sends what you type.
//! It exists so the protocol has a second implementation before the Android
//! one is written: anything this cannot do from the far side of a relay is
//! something the app would not be able to do either.
//!
//!     AH_REMOTE_URL=https://ah-relay.you.workers.dev \
//!     AH_REMOTE_CODE=A3K7-QM2X-... \
//!     cargo run -p ah-remote --example phone
//!
//! Type a message and press return to send it. `/list`, `/attach <id>`,
//! `/new <dir>`, `/resume <id>`, `/interrupt`, `/y`, `/n` and `/quit` do what
//! they look like.

use std::io::BufRead;
use std::sync::mpsc;

use ah_remote::crypto::{Keys, Opener, Sealer};
use ah_remote::link::{Config, Event, Link};
use ah_remote::{code, crypto};
use ah_remote_proto::{Dir, Envelope, FromDesk, FromPhone, PROTO, Role};

fn main() {
    let Some(url) = env("AH_REMOTE_URL") else {
        return fail("set AH_REMOTE_URL to the relay");
    };
    let Some(typed) = env("AH_REMOTE_CODE") else {
        return fail("set AH_REMOTE_CODE to the pairing code");
    };
    let Some(raw) = code::parse(&typed) else {
        return fail("that is not a pairing code");
    };

    let keys = Keys::derive(&raw);
    let plink = crypto::new_link();
    println!("hub {}…, phone {}", &keys.hub_id[..8], hex8(&plink));

    let (tx, rx) = mpsc::channel();
    let from_socket = tx.clone();
    let link = match Link::open(
        Config {
            url,
            role: Role::Phone,
            keys: Keys::derive(&raw),
        },
        move |ev| {
            let _ = from_socket.send(Wake::Socket(ev));
        },
    ) {
        Ok(l) => l,
        Err(e) => return fail(&format!("{e}")),
    };

    // Typing happens on its own thread, so a quiet socket does not block the
    // keyboard and a busy one does not block on it.
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(Wake::Typed(line)).is_err() {
                break;
            }
        }
    });

    let mut phone = Phone {
        keys,
        plink,
        session: String::new(),
        cursor: 0,
        seal: None,
        open: None,
        desk_link: None,
        ask: None,
    };

    while let Ok(wake) = rx.recv() {
        match wake {
            Wake::Socket(Event::Open) => {
                println!("— connected, asking for everything since {}", phone.cursor);
                link.send(text(&Envelope::Sub {
                    v: PROTO,
                    since: phone.cursor,
                    max: 200,
                }));
            }
            Wake::Socket(Event::Frame(raw)) => phone.arrived(&raw, &link),
            Wake::Socket(Event::Lost(why)) => println!("— lost: {why}"),
            Wake::Socket(Event::Fatal(why)) => {
                println!("— {why}");
                break;
            }
            Wake::Typed(line) => {
                if line.trim() == "/quit" {
                    break;
                }
                if let Some(cmd) = phone.typed(&line) {
                    phone.say(cmd, &link);
                }
            }
        }
    }
}

enum Wake {
    Socket(Event),
    Typed(String),
}

struct Phone {
    keys: Keys,
    plink: [u8; crypto::LINK_BYTES],
    session: String,
    /// The relay's own numbering, which is what a phone stores to come back.
    cursor: u64,
    seal: Option<Sealer>,
    open: Option<Opener>,
    desk_link: Option<[u8; crypto::LINK_BYTES]>,
    /// The question on screen, if there is one.
    ask: Option<u64>,
}

impl Phone {
    /// One frame off the socket.
    fn arrived(&mut self, raw: &str, link: &Link) {
        let Ok(frame) = serde_json::from_str::<Envelope>(raw) else {
            return println!("— unreadable frame");
        };
        match frame {
            Envelope::Evt {
                link: desk,
                seq,
                ct,
                n,
                ..
            } => {
                self.cursor = n;
                self.rekey(&desk);
                let Some(open) = self.open.as_mut() else {
                    return;
                };
                match open.open(seq, &ct) {
                    Ok(plain) => match serde_json::from_slice::<FromDesk>(plain) {
                        Ok(payload) => self.show(payload),
                        Err(e) => println!("— frame {n} is not a payload: {e}"),
                    },
                    // Replay is expected: the relay hands back what it kept,
                    // and some of it may already have been read.
                    Err(crypto::OpenError::Replay) => {}
                    Err(e) => println!("— frame {n} did not open: {e:?}"),
                }
            }
            Envelope::Gap { from, .. } => {
                println!("— everything before {from} has been trimmed away");
                self.cursor = from;
            }
            Envelope::Ctl { e, server_ms, .. } => {
                println!(
                    "— relay says {e:?}{}",
                    match server_ms {
                        Some(ms) => format!(" (its clock: {ms})"),
                        None => String::new(),
                    }
                );
            }
            _ => {}
        }
        let _ = link;
    }

    /// The desktop's key changes when it reconnects, and every frame says
    /// which link it belongs to, so the change is noticed rather than
    /// announced.
    fn rekey(&mut self, desk: &str) {
        let Some(desk) = unhex(desk) else { return };
        if self.desk_link == Some(desk) {
            return;
        }
        self.desk_link = Some(desk);
        self.open = Some(Opener::new(
            self.keys.link_key(Dir::D2p, &desk, &self.plink),
            Dir::D2p,
            desk,
            self.plink,
        ));
        self.seal = Some(Sealer::new(
            self.keys.link_key(Dir::P2d, &desk, &self.plink),
            Dir::P2d,
            desk,
            self.plink,
        ));
    }

    fn show(&mut self, payload: FromDesk) {
        match payload {
            FromDesk::Hello(h) => {
                println!("— {} ({}), ah {}", h.host, h.os, h.ah_version);
                for root in &h.roots {
                    println!("  /new may go under {root}");
                }
            }
            FromDesk::Sessions { list } => {
                println!("— {} session(s)", list.len());
                for s in &list {
                    let live = if s.live { "*" } else { " " };
                    println!("  {live} {} {} — {}", s.id, s.model, s.title);
                }
                // With nothing attached yet, the running one is the one
                // anything typed here is meant for.
                if self.session.is_empty()
                    && let Some(live) = list.iter().find(|s| s.live)
                {
                    self.session = live.id.clone();
                    println!("— talking to {}", live.id);
                }
            }
            FromDesk::State(s) => println!(
                "— {} {} in {}",
                s.session,
                if s.busy { "busy" } else { "idle" },
                s.cwd
            ),
            FromDesk::Snapshot {
                session, messages, ..
            } => {
                self.session = session;
                println!("— {} message(s) of scrollback", messages.len());
            }
            FromDesk::Events { evs, .. } => {
                for ev in evs {
                    print_event(&ev);
                }
            }
            FromDesk::AskPermission {
                session,
                id,
                call,
                reason,
            } => {
                // Answer the session that asked, not whichever one was being
                // watched when the question arrived.
                self.session = session;
                self.ask = Some(id);
                println!("? {} — {reason}  (/y or /n)", call.function.name);
            }
            FromDesk::AskUser { session, id, ask } => {
                self.session = session;
                self.ask = Some(id);
                for q in &ask.questions {
                    println!("? {}", q.question);
                }
            }
            FromDesk::Answered { id, by, .. } => {
                if self.ask == Some(id) {
                    self.ask = None;
                }
                println!("— answered by {by}");
            }
            FromDesk::Ack {
                ok, session, error, ..
            } => {
                // A started or resumed session is named here, so there is no
                // guessing which of the list that follows is the new one.
                if let Some(started) = session.filter(|_| ok) {
                    self.session = started;
                    println!("— talking to {}", self.session);
                }
                if !ok {
                    println!("— refused: {}", error.unwrap_or_default());
                }
            }
            FromDesk::Notice { text } => println!("— {text}"),
            FromDesk::Bye { reason } => println!("— the desktop is going: {reason:?}"),
            FromDesk::Blob { .. } => println!("— a file arrived"),
        }
    }

    /// A typed line, as something to send.
    fn typed(&mut self, line: &str) -> Option<FromPhone> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let session = self.session.clone();
        Some(match line {
            "/list" => FromPhone::List,
            "/interrupt" => FromPhone::Interrupt { session },
            "/y" | "/n" => FromPhone::AnswerPermission {
                session,
                id: self.ask.take()?,
                allow: line == "/y",
            },
            _ if line.starts_with("/new ") => FromPhone::NewSession {
                cwd: line[5..].trim().to_string(),
                model: None,
                prompt: None,
            },
            _ if line.starts_with("/resume ") => {
                let id = line[8..].trim().to_string();
                self.session = id.clone();
                FromPhone::Resume { session: id }
            }
            _ if line.starts_with("/attach ") => {
                let id = line[8..].trim().to_string();
                self.session = id.clone();
                FromPhone::Attach {
                    session: id,
                    device: "terminal".into(),
                    since: self.cursor,
                }
            }
            _ => FromPhone::Submit {
                session,
                text: line.to_string(),
                images: Vec::new(),
            },
        })
    }

    fn say(&mut self, payload: FromPhone, link: &Link) {
        let (Some(seal), Some(desk)) = (self.seal.as_mut(), self.desk_link) else {
            return println!("— nothing has been heard from the desktop yet");
        };
        let plain = serde_json::to_vec(&payload).unwrap_or_default();
        let (seq, ct) = seal.seal(&plain);
        link.send(text(&Envelope::command(
            &hex(&desk),
            &hex(&self.plink),
            seq,
            ct,
        )));
    }
}

/// The `--json` shapes, printed the way a person would want to read them.
fn print_event(ev: &serde_json::Value) {
    let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    match kind {
        "text" => print!("{}", ev.get("text").and_then(|v| v.as_str()).unwrap_or("")),
        "tool_start" => println!(
            "\n[{}]",
            ev.pointer("/call/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
        ),
        "turn_end" => println!("\n— done"),
        "error" => println!(
            "\n! {}",
            ev.get("error").and_then(|v| v.as_str()).unwrap_or("")
        ),
        _ => {}
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn text(frame: &Envelope) -> String {
    serde_json::to_string(frame).unwrap_or_default()
}

fn hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex8(raw: &[u8]) -> String {
    hex(&raw[..4])
}

fn unhex(s: &str) -> Option<[u8; crypto::LINK_BYTES]> {
    if s.len() != crypto::LINK_BYTES * 2 {
        return None;
    }
    let mut out = [0u8; crypto::LINK_BYTES];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn fail(why: &str) {
    eprintln!("phone: {why}");
    std::process::exit(1);
}
