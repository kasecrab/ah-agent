//! The two halves, through a relay, for real.
//!
//! Not part of `just test`: it needs something listening. Either is fine —
//! the Worker, or the mock that needs no wasm toolchain:
//!
//!     python3 relay/mock.py 8799 &
//!     AH_RELAY=http://127.0.0.1:8799 \
//!       cargo test -p ah-remote --test relay -- --ignored --nocapture
//!
//!     just relay-dev &      # the real Worker instead
//!     AH_RELAY=http://127.0.0.1:8787 cargo test ... -- --ignored
//!
//! Each test provisions a pairing of its own, so it can be run as often as
//! you like against a relay that remembers things.

use std::sync::mpsc;
use std::time::Duration;

use ah_remote::crypto::{self, Keys, Opener, Sealer};
use ah_remote::link::{Config, Event, Link};
use ah_remote::provision;
use ah_remote_proto::{Bye, Dir, Envelope, FromDesk, FromPhone, PROTO, Role};

fn relay() -> String {
    std::env::var("AH_RELAY").unwrap_or_else(|_| "http://127.0.0.1:8799".into())
}

/// A pairing nothing else has used.
fn pairing() -> ([u8; crypto::CODE_BYTES], Keys) {
    let code = crypto::new_code();
    let keys = Keys::derive(&code);
    provision::provision(&relay(), &keys).expect("the relay should take a fresh pairing");
    (code, keys)
}

fn hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Open a link and hand its events back through a channel.
fn dial(keys: Keys, role: Role) -> (Link, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let link = Link::open(
        Config {
            url: relay(),
            role,
            keys,
        },
        move |ev| {
            let _ = tx.send(ev);
        },
    )
    .expect("the link should open");
    (link, rx)
}

/// Wait for the socket to say it is up.
fn wait_open(rx: &mpsc::Receiver<Event>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::Open) => return,
            Ok(Event::Fatal(why)) => panic!("the relay refused the link: {why}"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    panic!("the link never opened");
}

/// Frames, until `want` of them have arrived or the time runs out.
fn frames(rx: &mpsc::Receiver<Event>, want: usize, secs: u64) -> Vec<Envelope> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut out = Vec::new();
    while out.len() < want && std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Frame(raw)) => {
                if let Ok(f) = serde_json::from_str::<Envelope>(&raw) {
                    out.push(f);
                }
            }
            Ok(Event::Fatal(why)) => panic!("the link gave up: {why}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    out
}

/// What a desktop needs to publish: its link id, and the seal for it.
struct Desk {
    link: Link,
    id: [u8; crypto::LINK_BYTES],
    seal: Sealer,
}

impl Desk {
    fn new(keys: &Keys) -> (Self, mpsc::Receiver<Event>) {
        let id = crypto::new_link();
        let seal = Sealer::new(
            keys.link_key(Dir::D2p, &id, &[0u8; crypto::LINK_BYTES]),
            Dir::D2p,
            id,
            [0u8; crypto::LINK_BYTES],
        );
        let (link, rx) = dial(keys.clone(), Role::Desk);
        wait_open(&rx);
        (Desk { link, id, seal }, rx)
    }

    fn publish(&mut self, payload: &FromDesk) {
        let plain = serde_json::to_vec(payload).unwrap();
        let (seq, ct) = self.seal.seal(&plain);
        self.link
            .send(serde_json::to_string(&Envelope::publish(&hex(&self.id), seq, ct)).unwrap());
    }
}

#[test]
#[ignore]
fn what_the_desktop_seals_is_what_the_phone_reads() {
    let (_code, keys) = pairing();
    let (mut desk, _desk_rx) = Desk::new(&keys);

    let (phone, phone_rx) = dial(keys.clone(), Role::Phone);
    wait_open(&phone_rx);
    phone.send(
        serde_json::to_string(&Envelope::Sub {
            v: PROTO,
            since: 0,
            max: 100,
        })
        .unwrap(),
    );

    desk.publish(&FromDesk::Notice {
        text: "the kettle is on".into(),
    });

    let got = frames(&phone_rx, 1, 10);
    let Some(Envelope::Evt { link, seq, ct, .. }) = got.first() else {
        panic!("the phone heard nothing: {got:?}");
    };

    // The phone knows only what the frame told it: which link it came from.
    let mut id = [0u8; crypto::LINK_BYTES];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&link[i * 2..i * 2 + 2], 16).unwrap();
    }
    let plink = [0u8; crypto::LINK_BYTES];
    let mut open = Opener::new(keys.link_key(Dir::D2p, &id, &plink), Dir::D2p, id, plink);
    let plain = open.open(*seq, ct).expect("it should open");
    let payload: FromDesk = serde_json::from_slice(plain).expect("it should parse");
    assert_eq!(
        payload,
        FromDesk::Notice {
            text: "the kettle is on".into()
        }
    );
}

#[test]
#[ignore]
fn a_phone_that_was_away_is_given_what_it_missed() {
    let (_code, keys) = pairing();
    let (mut desk, _desk_rx) = Desk::new(&keys);

    desk.publish(&FromDesk::Notice { text: "one".into() });
    desk.publish(&FromDesk::Notice { text: "two".into() });
    desk.publish(&FromDesk::Notice {
        text: "three".into(),
    });
    std::thread::sleep(Duration::from_millis(500));

    // Arriving after all three, and asking from the start, gets all three.
    let (phone, rx) = dial(keys.clone(), Role::Phone);
    wait_open(&rx);
    phone.send(
        serde_json::to_string(&Envelope::Sub {
            v: PROTO,
            since: 0,
            max: 100,
        })
        .unwrap(),
    );
    let got = frames(&rx, 3, 10);
    assert_eq!(got.len(), 3, "three were published: {got:?}");

    let numbers: Vec<u64> = got
        .iter()
        .filter_map(|f| match f {
            Envelope::Evt { n, .. } => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(numbers, vec![1, 2, 3], "in the order they were published");
}

#[test]
#[ignore]
fn what_a_phone_says_reaches_the_desktop_unchanged() {
    let (_code, keys) = pairing();
    let (desk, desk_rx) = Desk::new(&keys);
    let _ = &desk;

    let plink = crypto::new_link();
    let (phone, phone_rx) = dial(keys.clone(), Role::Phone);
    wait_open(&phone_rx);

    let mut seal = Sealer::new(
        keys.link_key(Dir::P2d, &desk.id, &plink),
        Dir::P2d,
        desk.id,
        plink,
    );
    let payload = FromPhone::Submit {
        session: "abc".into(),
        text: "carry on".into(),
        images: Vec::new(),
    };
    let (seq, ct) = seal.seal(&serde_json::to_vec(&payload).unwrap());
    phone.send(
        serde_json::to_string(&Envelope::command(&hex(&desk.id), &hex(&plink), seq, ct)).unwrap(),
    );

    let got = frames(&desk_rx, 1, 10);
    let Some(Envelope::Cmd { seq, ct, .. }) = got.first() else {
        panic!("the desktop heard nothing: {got:?}");
    };
    let mut open = Opener::new(
        keys.link_key(Dir::P2d, &desk.id, &plink),
        Dir::P2d,
        desk.id,
        plink,
    );
    let plain = open.open(*seq, ct).expect("it should open");
    assert_eq!(
        serde_json::from_slice::<FromPhone>(plain).unwrap(),
        payload,
        "what arrived is what was sent"
    );
}

#[test]
#[ignore]
fn a_second_desktop_does_not_push_a_live_one_aside() {
    let (_code, keys) = pairing();
    let (_desk, _rx) = Desk::new(&keys);

    // The relay should refuse the newcomer, not the incumbent, and the link
    // should say so rather than retrying on the usual ladder.
    let (_second, rx2) = dial(keys.clone(), Role::Desk);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match rx2.recv_timeout(Duration::from_secs(10)) {
            Ok(Event::Lost(why)) => {
                assert!(why.contains("already connected"), "it said {why:?}");
                return;
            }
            Ok(Event::Open) => panic!("the second desktop should not have got in"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    panic!("the second desktop was neither let in nor turned away");
}

#[test]
#[ignore]
fn a_pairing_the_relay_has_never_heard_of_is_refused() {
    // Derived but never provisioned.
    let keys = Keys::derive(&crypto::new_code());
    let (_link, rx) = dial(keys, Role::Desk);
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Event::Lost(why)) => assert!(why.contains("never heard of"), "it said {why:?}"),
        other => panic!("expected to be turned away, got {other:?}"),
    }
}

#[test]
#[ignore]
fn a_farewell_is_a_payload_like_any_other() {
    let (_code, keys) = pairing();
    let (mut desk, _desk_rx) = Desk::new(&keys);
    desk.publish(&FromDesk::Bye { reason: Bye::Quit });
    // Nothing to assert beyond it going out without complaint: the shape is
    // covered by the proto crate's own tests.
}
