//! The key ladder, and the seal every frame goes through.
//!
//! One pairing code is the only secret. Everything else — the name of the hub,
//! the key the relay is given, the key each direction is sealed under — is
//! derived from it, so pairing moves exactly one thing between two devices and
//! nothing else has to be kept in step.
//!
//! What the relay holds is a leaf of that ladder. HKDF expansions are
//! independent, so a relay that knows its own key learns nothing about the
//! keys the payloads are sealed under, and an attacker who steals it can open
//! a socket but not read a word that goes over it.
//!
//! Every phone paired to a machine shares the desktop's outgoing key: they all
//! watch the same session, so they all have to be able to read it. A phone can
//! therefore forge a frame that looks like the desktop's to another phone.
//! That is the pairing code's trust boundary, not a hole inside it — anyone
//! holding the code could start a session and say anything anyway.

use ah_remote_proto::{
    Dir, INFO_D2P, INFO_HUB, INFO_P2D, INFO_RELAY, LINK_SALT, Role, SALT, aad, connect_message,
};
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{hkdf, hmac};

/// The pairing code, in bytes. 160 bits, which is 32 characters of base32 with
/// nothing left over — long enough that it never has to be stretched, short
/// enough to read off a screen if the QR will not scan.
pub const CODE_BYTES: usize = 20;

/// Names one connection. A fresh one each time a publisher takes the lock, so
/// a restart can never reuse a sequence number under a key it used before.
pub const LINK_BYTES: usize = 16;

/// ring wants a type that knows how long an expansion should be, and ships
/// none for plain bytes.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// Everything the pairing code leads to.
#[derive(Clone)]
pub struct Keys {
    /// Where the two peers meet: 32 lowercase hex characters, derived rather
    /// than random so a phone can find the hub from the code alone.
    pub hub_id: String,
    /// The only key the relay is given.
    pub relay_key: [u8; 32],
    d2p: [u8; 32],
    p2d: [u8; 32],
}

impl Keys {
    pub fn derive(code: &[u8]) -> Self {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, SALT).extract(code);
        let mut hub = [0u8; 16];
        let mut relay_key = [0u8; 32];
        let mut d2p = [0u8; 32];
        let mut p2d = [0u8; 32];
        expand(&prk, &[INFO_HUB], &mut hub);
        expand(&prk, &[INFO_RELAY], &mut relay_key);
        expand(&prk, &[INFO_D2P], &mut d2p);
        expand(&prk, &[INFO_P2D], &mut p2d);
        Self {
            hub_id: HEXLOWER.encode(&hub),
            relay_key,
            d2p,
            p2d,
        }
    }

    /// The key one connection seals under.
    ///
    /// The desktop's key binds only its own link, because every attached phone
    /// has to open it. A phone's key binds both links, so two phones — and the
    /// same phone twice — never share a key, and a reattaching phone starting
    /// its count again at one cannot land on a nonce that has been used.
    pub fn link_key(
        &self,
        dir: Dir,
        link: &[u8; LINK_BYTES],
        plink: &[u8; LINK_BYTES],
    ) -> [u8; 32] {
        let (base, info): (&[u8; 32], &[&[u8]]) = match dir {
            Dir::D2p => (&self.d2p, &[INFO_D2P, link]),
            Dir::P2d => (&self.p2d, &[INFO_P2D, link, plink]),
        };
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, LINK_SALT).extract(base);
        let mut out = [0u8; 32];
        expand(&prk, info, &mut out);
        out
    }

    /// The signature that gets a socket open. Proves the pairing code was
    /// known without handing the relay anything it could read a frame with.
    pub fn sign_connect(&self, role: Role, ts: u64, nonce: &str) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.relay_key);
        let msg = connect_message(&self.hub_id, role, ts, nonce);
        BASE64URL_NOPAD.encode(hmac::sign(&key, msg.as_bytes()).as_ref())
    }

    /// The same check the relay makes, kept here so both halves are tested
    /// against one implementation rather than against each other's bugs.
    pub fn verify_connect(&self, role: Role, ts: u64, nonce: &str, sig: &str) -> bool {
        let Ok(sig) = BASE64URL_NOPAD.decode(sig.as_bytes()) else {
            return false;
        };
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.relay_key);
        let msg = connect_message(&self.hub_id, role, ts, nonce);
        hmac::verify(&key, msg.as_bytes(), &sig).is_ok()
    }
}

fn expand(prk: &hkdf::Prk, info: &[&[u8]], out: &mut [u8]) {
    prk.expand(info, Len(out.len()))
        .expect("a length HKDF-SHA256 can produce")
        .fill(out)
        .expect("the same length again");
}

/// A fresh pairing code.
pub fn new_code() -> [u8; CODE_BYTES] {
    let mut out = [0u8; CODE_BYTES];
    fill(&mut out);
    out
}

/// A fresh link id.
pub fn new_link() -> [u8; LINK_BYTES] {
    let mut out = [0u8; LINK_BYTES];
    fill(&mut out);
    out
}

/// A nonce for a connect signature: enough that two dials in the same
/// millisecond do not produce the same signed message.
pub fn new_nonce() -> String {
    let mut out = [0u8; 12];
    fill(&mut out);
    BASE64URL_NOPAD.encode(&out)
}

fn fill(out: &mut [u8]) {
    // A machine that cannot produce randomness cannot keep a secret either,
    // and carrying on with a predictable key would be worse than stopping.
    SystemRandom::new()
        .fill(out)
        .expect("the system random source");
}

/// Seals outgoing frames, in order, numbering them as it goes.
///
/// The sequence number is the nonce, so it is never sent twice under one key
/// and never has to travel separately — it is already in the envelope. That
/// only holds while one of these is the sole sealer for its key, which is what
/// taking `&mut self` enforces: frames leave a link on one thread, in order.
pub struct Sealer {
    key: LessSafeKey,
    link: [u8; LINK_BYTES],
    plink: [u8; LINK_BYTES],
    dir: Dir,
    seq: u64,
    buf: Vec<u8>,
}

impl Sealer {
    pub fn new(key: [u8; 32], dir: Dir, link: [u8; LINK_BYTES], plink: [u8; LINK_BYTES]) -> Self {
        Self {
            key: unbound(&key),
            link,
            plink,
            dir,
            seq: 0,
            buf: Vec::new(),
        }
    }

    /// Seal one payload. Returns the number it went out under and the sealed
    /// bytes, ready for an envelope.
    pub fn seal(&mut self, plaintext: &[u8]) -> (u64, String) {
        self.seq += 1;
        let seq = self.seq;
        self.buf.clear();
        self.buf.extend_from_slice(plaintext);
        let extra = if self.dir == Dir::D2p {
            None
        } else {
            Some(&self.plink)
        };
        self.key
            .seal_in_place_append_tag(
                nonce(seq),
                Aad::from(aad(&self.link, extra, self.dir, seq)),
                &mut self.buf,
            )
            .expect("sealing cannot fail with a valid key and nonce");
        (seq, BASE64URL_NOPAD.encode(&self.buf))
    }
}

/// Why a frame did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Its number has been seen. Either the relay replayed it, or somebody is
    /// trying to make something happen twice.
    Replay,
    /// Not base64.
    Encoding,
    /// It did not open under this key, for this link, at this number. Any of
    /// those being wrong looks exactly the same from here, deliberately.
    Refused,
}

/// Opens incoming frames and refuses to open one twice.
pub struct Opener {
    key: LessSafeKey,
    link: [u8; LINK_BYTES],
    plink: [u8; LINK_BYTES],
    dir: Dir,
    last_seq: u64,
    buf: Vec<u8>,
}

impl Opener {
    pub fn new(key: [u8; 32], dir: Dir, link: [u8; LINK_BYTES], plink: [u8; LINK_BYTES]) -> Self {
        Self {
            key: unbound(&key),
            link,
            plink,
            dir,
            last_seq: 0,
            buf: Vec::new(),
        }
    }

    /// Start from a number already reached, for a reader picking a stream back
    /// up where it left off.
    pub fn resume_from(&mut self, seq: u64) {
        self.last_seq = seq;
    }

    /// The last number opened. What a reader stores to resume later.
    pub fn seq(&self) -> u64 {
        self.last_seq
    }

    /// Open one frame. The plaintext borrows this opener's buffer, so it holds
    /// only until the next frame — read it before asking for another.
    pub fn open(&mut self, seq: u64, ct: &str) -> Result<&[u8], OpenError> {
        // Before the cipher, not after: the cheap check turns a flood of
        // replayed frames into a flood of integer comparisons.
        if seq <= self.last_seq {
            return Err(OpenError::Replay);
        }
        self.buf.clear();
        let raw = BASE64URL_NOPAD
            .decode_mut(ct.as_bytes(), resize(&mut self.buf, decoded_len(ct)?))
            .map_err(|_| OpenError::Encoding)?;
        self.buf.truncate(raw);
        let extra = if self.dir == Dir::D2p {
            None
        } else {
            Some(&self.plink)
        };
        let len = self
            .key
            .open_in_place(
                nonce(seq),
                Aad::from(aad(&self.link, extra, self.dir, seq)),
                &mut self.buf,
            )
            .map_err(|_| OpenError::Refused)?
            .len();
        // Only now. A frame that did not open must not move the window, or
        // anyone able to inject one with a high number could wedge the link
        // shut for good.
        self.last_seq = seq;
        self.buf.truncate(len);
        Ok(&self.buf)
    }
}

fn decoded_len(ct: &str) -> Result<usize, OpenError> {
    BASE64URL_NOPAD
        .decode_len(ct.len())
        .map_err(|_| OpenError::Encoding)
}

fn resize(buf: &mut Vec<u8>, len: usize) -> &mut [u8] {
    buf.resize(len, 0);
    buf.as_mut_slice()
}

fn unbound(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("a 32 byte key for AES-256-GCM"))
}

/// The frame's number, as a nonce. The leading bytes are a reserved epoch: the
/// key is already per-connection, so there is nothing a random prefix would
/// add, and a number that can be recomputed from the envelope is one fewer
/// thing that can arrive wrong.
fn nonce(seq: u64) -> Nonce {
    let mut raw = [0u8; NONCE_LEN];
    raw[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::assume_unique_for_key(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Sealer, Opener) {
        let keys = Keys::derive(&[7u8; CODE_BYTES]);
        let link = [1u8; LINK_BYTES];
        let plink = [2u8; LINK_BYTES];
        let k = keys.link_key(Dir::P2d, &link, &plink);
        (
            Sealer::new(k, Dir::P2d, link, plink),
            Opener::new(k, Dir::P2d, link, plink),
        )
    }

    #[test]
    fn what_was_sealed_comes_back_out() {
        let (mut s, mut o) = pair();
        let (seq, ct) = s.seal(b"carry on");
        assert_eq!(o.open(seq, &ct).unwrap(), b"carry on");
    }

    #[test]
    fn the_same_code_on_both_sides_derives_the_same_keys() {
        let a = Keys::derive(&[3u8; CODE_BYTES]);
        let b = Keys::derive(&[3u8; CODE_BYTES]);
        assert_eq!(a.hub_id, b.hub_id);
        assert_eq!(a.relay_key, b.relay_key);
    }

    #[test]
    fn one_changed_byte_changes_everything_derived() {
        let mut code = [3u8; CODE_BYTES];
        let a = Keys::derive(&code);
        code[CODE_BYTES - 1] ^= 1;
        let b = Keys::derive(&code);
        assert_ne!(a.hub_id, b.hub_id);
        assert_ne!(a.relay_key, b.relay_key);
        let link = [1u8; LINK_BYTES];
        assert_ne!(
            a.link_key(Dir::D2p, &link, &link),
            b.link_key(Dir::D2p, &link, &link)
        );
    }

    #[test]
    fn the_hub_name_gives_nothing_away() {
        let k = Keys::derive(&[3u8; CODE_BYTES]);
        assert!(ah_remote_proto::is_hub_id(&k.hub_id));
        // The name is public — it is in the URL — so it must not be any part
        // of a key.
        let hub = data_encoding::HEXLOWER.decode(k.hub_id.as_bytes()).unwrap();
        assert!(!k.relay_key.starts_with(&hub[..8]));
        assert!(!k.d2p.starts_with(&hub[..8]));
    }

    #[test]
    fn the_relay_key_is_a_leaf_and_not_a_root() {
        let k = Keys::derive(&[3u8; CODE_BYTES]);
        // Whatever a relay can do with what it holds, deriving the payload
        // keys from it is not among them.
        let from_relay = Keys::derive(&k.relay_key);
        assert_ne!(from_relay.d2p, k.d2p);
        assert_ne!(from_relay.p2d, k.p2d);
    }

    #[test]
    fn a_frame_altered_by_one_bit_does_not_open() {
        let (mut s, mut o) = pair();
        let (seq, ct) = s.seal(b"carry on");
        let mut bad: Vec<u8> = ct.into_bytes();
        bad[0] = if bad[0] == b'A' { b'B' } else { b'A' };
        let bad = String::from_utf8(bad).unwrap();
        assert_eq!(o.open(seq, &bad), Err(OpenError::Refused));
    }

    #[test]
    fn a_frame_replayed_with_its_own_number_is_refused() {
        let (mut s, mut o) = pair();
        let (seq, ct) = s.seal(b"spend the money");
        assert!(o.open(seq, &ct).is_ok());
        assert_eq!(o.open(seq, &ct), Err(OpenError::Replay));
    }

    #[test]
    fn a_frame_renumbered_on_the_way_does_not_open() {
        let (mut s, mut o) = pair();
        let (_, ct) = s.seal(b"carry on");
        // The number is sealed in, so moving it is the same as breaking it.
        assert_eq!(o.open(9, &ct), Err(OpenError::Refused));
    }

    #[test]
    fn a_frame_that_did_not_open_does_not_move_the_window() {
        let (mut s, mut o) = pair();
        let (seq, ct) = s.seal(b"carry on");
        // Somebody injects nonsense numbered far ahead.
        assert_eq!(o.open(u64::MAX, &ct), Err(OpenError::Refused));
        // The real frame still arrives and is still read.
        assert_eq!(o.open(seq, &ct).unwrap(), b"carry on");
    }

    #[test]
    fn a_frame_from_another_link_does_not_open() {
        let keys = Keys::derive(&[7u8; CODE_BYTES]);
        let plink = [2u8; LINK_BYTES];
        let mine = [1u8; LINK_BYTES];
        let theirs = [9u8; LINK_BYTES];
        let mut s = Sealer::new(
            keys.link_key(Dir::P2d, &theirs, &plink),
            Dir::P2d,
            theirs,
            plink,
        );
        let mut o = Opener::new(
            keys.link_key(Dir::P2d, &mine, &plink),
            Dir::P2d,
            mine,
            plink,
        );
        let (seq, ct) = s.seal(b"from a link you retired");
        assert_eq!(o.open(seq, &ct), Err(OpenError::Refused));
    }

    #[test]
    fn a_frame_reflected_back_at_its_sender_does_not_open() {
        let keys = Keys::derive(&[7u8; CODE_BYTES]);
        let link = [1u8; LINK_BYTES];
        let plink = [2u8; LINK_BYTES];
        let mut up = Sealer::new(
            keys.link_key(Dir::P2d, &link, &plink),
            Dir::P2d,
            link,
            plink,
        );
        let mut down = Opener::new(
            keys.link_key(Dir::D2p, &link, &plink),
            Dir::D2p,
            link,
            plink,
        );
        let (seq, ct) = up.seal(b"do the thing");
        assert_eq!(down.open(seq, &ct), Err(OpenError::Refused));
    }

    #[test]
    fn two_phones_on_one_link_do_not_share_a_key() {
        let keys = Keys::derive(&[7u8; CODE_BYTES]);
        let link = [1u8; LINK_BYTES];
        assert_ne!(
            keys.link_key(Dir::P2d, &link, &[2u8; LINK_BYTES]),
            keys.link_key(Dir::P2d, &link, &[3u8; LINK_BYTES])
        );
        // The desktop's direction is shared on purpose: every phone attached
        // has to be able to read the same stream.
        assert_eq!(
            keys.link_key(Dir::D2p, &link, &[2u8; LINK_BYTES]),
            keys.link_key(Dir::D2p, &link, &[3u8; LINK_BYTES])
        );
    }

    #[test]
    fn numbering_starts_at_one_and_never_repeats() {
        let (mut s, _) = pair();
        let first = s.seal(b"a").0;
        assert_eq!(first, 1, "zero is never used, so a fresh window rejects it");
        assert_eq!(s.seal(b"b").0, 2);
        assert_eq!(s.seal(b"c").0, 3);
    }

    #[test]
    fn a_reader_can_be_put_back_where_it_left_off() {
        let (mut s, mut o) = pair();
        let mut sealed = Vec::new();
        for i in 0..4 {
            sealed.push(s.seal(&[b'a' + i]));
        }
        o.resume_from(2);
        // What it already had is refused, and what it missed is not.
        assert_eq!(o.open(sealed[1].0, &sealed[1].1), Err(OpenError::Replay));
        assert_eq!(o.open(sealed[2].0, &sealed[2].1).unwrap(), b"c");
        assert_eq!(o.seq(), 3);
    }

    #[test]
    fn a_signature_is_accepted_only_for_what_it_signed() {
        let k = Keys::derive(&[5u8; CODE_BYTES]);
        let sig = k.sign_connect(Role::Desk, 1700, "nonce");
        assert!(k.verify_connect(Role::Desk, 1700, "nonce", &sig));
        assert!(!k.verify_connect(Role::Phone, 1700, "nonce", &sig), "role");
        assert!(!k.verify_connect(Role::Desk, 1701, "nonce", &sig), "time");
        assert!(!k.verify_connect(Role::Desk, 1700, "other", &sig), "nonce");
        assert!(
            !k.verify_connect(Role::Desk, 1700, "nonce", "bm90"),
            "signature"
        );
        assert!(!k.verify_connect(Role::Desk, 1700, "nonce", "not base64!"));
    }

    #[test]
    fn a_signature_from_another_pairing_is_refused() {
        let mine = Keys::derive(&[5u8; CODE_BYTES]);
        let theirs = Keys::derive(&[6u8; CODE_BYTES]);
        let sig = theirs.sign_connect(Role::Desk, 1700, "nonce");
        assert!(!mine.verify_connect(Role::Desk, 1700, "nonce", &sig));
    }

    #[test]
    fn an_empty_payload_still_seals() {
        let (mut s, mut o) = pair();
        let (seq, ct) = s.seal(b"");
        assert_eq!(o.open(seq, &ct).unwrap(), b"");
    }

    #[test]
    fn a_big_payload_survives_the_buffer_being_reused() {
        let (mut s, mut o) = pair();
        let big = vec![b'x'; 200_000];
        let (seq, ct) = s.seal(&big);
        assert_eq!(o.open(seq, &ct).unwrap(), &big[..]);
        // And a small one after it does not read the tail of the big one.
        let (seq, ct) = s.seal(b"small");
        assert_eq!(o.open(seq, &ct).unwrap(), b"small");
    }

    #[test]
    fn two_codes_are_not_the_same_code() {
        assert_ne!(new_code(), new_code());
        assert_ne!(new_link(), new_link());
        assert_ne!(new_nonce(), new_nonce());
    }
}

#[cfg(test)]
mod vectors {
    use super::*;

    /// Values a shell script can hand the relay, so the relay is tested
    /// against the real ladder rather than against a second copy of it.
    ///
    /// `cargo test -p ah-remote --lib vectors -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_connect_vectors() {
        let keys = Keys::derive(&[42u8; CODE_BYTES]);
        let ts = 1_757_000_000_000u64;
        let nonce = "AAAAAAAAAAAAAAAA";
        println!("hub {}", keys.hub_id);
        println!("relay_key {}", BASE64URL_NOPAD.encode(&keys.relay_key));
        println!("ts {ts}");
        println!("nonce {nonce}");
        println!("sig_desk {}", keys.sign_connect(Role::Desk, ts, nonce));
        println!("sig_phone {}", keys.sign_connect(Role::Phone, ts, nonce));
        // Same signature, but for a moment far outside the window.
        let stale = ts - 3_600_000;
        println!("stale_ts {stale}");
        println!("sig_stale {}", keys.sign_connect(Role::Desk, stale, nonce));
    }
}
