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
//!
//! Every key here is wiped when it is done with, and every wipe goes through
//! `zeroize`. That is not fussiness about which library to use: measured
//! against this workspace's release profile, a `field = [0u8; 32]` in a `Drop`
//! and a `ptr::write_bytes` without a fence are both deleted by the optimiser
//! and leave the key sitting whole in the dead frame. A volatile write is the
//! one form of it the compiler is not allowed to remove.
//!
//! Two things here cannot be wiped, and saying so is better than implying
//! otherwise. ring keeps the pairing code's HKDF state and the expanded AES
//! key schedule in structures that hold a `&'static` pointer beside the key
//! bytes, so overwriting them wholesale would leave a null reference behind —
//! undefined behaviour, in exchange for a partial wipe. What is done instead
//! is to own as few copies as possible, wipe every copy that is ours, and let
//! the ring-held ones die with the frame they were built in.

use ah_remote_proto::{
    Dir, INFO_D2P, INFO_HUB, INFO_P2D, INFO_RELAY, LINK_SALT, Role, SALT, aad, connect_message,
};
use data_encoding::{BASE64URL_NOPAD, HEXLOWER};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{hkdf, hmac};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

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

/// Wiped the moment the last holder lets go, wherever that is.
///
/// This is the one thing in the ladder that lives as long as publishing does,
/// and a window or a daemon goes on running after publishing stops. Shortening
/// its life is not the fix — it is needed for every frame — so the fix is that
/// it clears itself when it is finally dropped, in whichever thread that turns
/// out to be.
impl Zeroize for Keys {
    fn zeroize(&mut self) {
        self.relay_key.zeroize();
        self.d2p.zeroize();
        self.p2d.zeroize();
        // The hub name is public — it is in the URL the relay is dialled on —
        // so it is left alone rather than pretended about.
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for Keys {}

impl Keys {
    /// Borrows anything that can be read as bytes, so that a caller holding
    /// the code in a wrapper that wipes it can lend it as it stands. Taking a
    /// plain `&[u8]` would mean unwrapping it at every call site, and
    /// unwrapping it is how a copy ends up outside the wrapper.
    pub fn derive(code: &impl AsRef<[u8]>) -> Self {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, SALT).extract(code.as_ref());
        let mut hub = [0u8; 16];
        // Wrapped rather than bare, because these three locals are the reason
        // the frame of this function held three whole keys after it returned.
        // Moving them into the struct at the end copies them; the copy left
        // behind here is what gets cleared.
        let mut relay_key = Zeroizing::new([0u8; 32]);
        let mut d2p = Zeroizing::new([0u8; 32]);
        let mut p2d = Zeroizing::new([0u8; 32]);
        expand(&prk, &[INFO_HUB], &mut hub);
        expand(&prk, &[INFO_RELAY], relay_key.as_mut());
        expand(&prk, &[INFO_D2P], d2p.as_mut());
        expand(&prk, &[INFO_P2D], p2d.as_mut());
        Self {
            hub_id: HEXLOWER.encode(&hub),
            relay_key: *relay_key,
            d2p: *d2p,
            p2d: *p2d,
        }
    }

    /// The key one connection seals under.
    ///
    /// The desktop's key binds only its own link, because every attached phone
    /// has to open it. A phone's key binds both links, so two phones — and the
    /// same phone twice — never share a key, and a reattaching phone starting
    /// its count again at one cannot land on a nonce that has been used.
    ///
    /// Handed back wrapped, because a link key is wanted for exactly as long
    /// as it takes to build a sealer out of it and never again: whoever holds
    /// one of these drops it a line or two later and it is cleared on the way
    /// out.
    pub fn link_key(
        &self,
        dir: Dir,
        link: &[u8; LINK_BYTES],
        plink: &[u8; LINK_BYTES],
    ) -> Zeroizing<[u8; 32]> {
        let (base, info): (&[u8; 32], &[&[u8]]) = match dir {
            Dir::D2p => (&self.d2p, &[INFO_D2P, link]),
            Dir::P2d => (&self.p2d, &[INFO_P2D, link, plink]),
        };
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, LINK_SALT).extract(base);
        let mut out = Zeroizing::new([0u8; 32]);
        expand(&prk, info, out.as_mut());
        out
    }

    /// The key a connect signature is made and checked with.
    ///
    /// One place rather than two, so there is one answer to "where does this
    /// live". It cannot be cleared afterwards: ring's `hmac::Key` is a pair of
    /// keyed SHA-256 states sitting next to a `&'static` pointer, and writing
    /// zeros over the whole of it would null that pointer. What limits the
    /// damage is that the thing this is made from — `relay_key` — is cleared,
    /// and that the relay already holds it: it opens a socket, it does not
    /// open a frame.
    fn connect_mac(&self) -> hmac::Key {
        hmac::Key::new(hmac::HMAC_SHA256, &self.relay_key)
    }

    /// The signature that gets a socket open. Proves the pairing code was
    /// known without handing the relay anything it could read a frame with.
    pub fn sign_connect(&self, role: Role, ts: u64, nonce: &str) -> String {
        let key = self.connect_mac();
        let msg = connect_message(&self.hub_id, role, ts, nonce);
        BASE64URL_NOPAD.encode(hmac::sign(&key, msg.as_bytes()).as_ref())
    }

    /// The same check the relay makes, kept here so both halves are tested
    /// against one implementation rather than against each other's bugs.
    pub fn verify_connect(&self, role: Role, ts: u64, nonce: &str, sig: &str) -> bool {
        let Ok(sig) = BASE64URL_NOPAD.decode(sig.as_bytes()) else {
            return false;
        };
        let key = self.connect_mac();
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
///
/// Handed back bare, unlike everything else here, because a code that has just
/// been made is not yet a secret anybody holds: whoever asked for it decides
/// what it is for and is the one who has to keep it wrapped — `ah remote pair`
/// does. Deriving from it takes anything readable as bytes, so wrapping it is
/// the caller's to do and costs them nothing.
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
    /// Takes the link key rather than borrowing it, so that the copy the
    /// caller made to hand over is this one's to clear. ring expands it into a
    /// key schedule that keeps the key verbatim in its first round and that
    /// cannot be cleared from out here (see the note at the top of this file),
    /// so the most that can be done is to leave no copy of it anywhere else.
    pub fn new(
        key: Zeroizing<[u8; 32]>,
        dir: Dir,
        link: [u8; LINK_BYTES],
        plink: [u8; LINK_BYTES],
    ) -> Self {
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
    /// Takes the link key the same way a [`Sealer`] does, and for the same
    /// reason.
    pub fn new(
        key: Zeroizing<[u8; 32]>,
        dir: Dir,
        link: [u8; LINK_BYTES],
        plink: [u8; LINK_BYTES],
    ) -> Self {
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
            Sealer::new(k.clone(), Dir::P2d, link, plink),
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

    #[test]
    fn a_ladder_that_has_been_wiped_has_nothing_left_in_it() {
        // What `Drop` does, done where it can be looked at: dropping the value
        // is the one moment a test cannot read it afterwards.
        let mut k = Keys::derive(&[4u8; CODE_BYTES]);
        assert_ne!(k.relay_key, [0u8; 32], "there was something to wipe");
        let hub = k.hub_id.clone();
        k.zeroize();
        assert_eq!(k.relay_key, [0u8; 32]);
        assert_eq!(k.d2p, [0u8; 32]);
        assert_eq!(k.p2d, [0u8; 32]);
        // The hub name is public and is left alone on purpose, so that a
        // wiped `Keys` still says which pairing it was.
        assert_eq!(k.hub_id, hub);
    }

    #[test]
    fn a_link_key_clears_itself_when_it_is_let_go() {
        let keys = Keys::derive(&[4u8; CODE_BYTES]);
        let link = [1u8; LINK_BYTES];
        let mut k = keys.link_key(Dir::D2p, &link, &link);
        assert_ne!(*k, [0u8; 32], "there was something to wipe");
        k.zeroize();
        assert_eq!(*k, [0u8; 32]);
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

    /// Everything the Android half has to reproduce, so the two can be
    /// checked against each other without either one running.
    ///
    /// `cargo test -p ah-remote --lib vectors::dump_for_the_phone -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_for_the_phone() {
        let code = [9u8; CODE_BYTES];
        let keys = Keys::derive(&code);
        let link = [0x11u8; LINK_BYTES];
        let plink = [0x22u8; LINK_BYTES];
        let d2p = keys.link_key(Dir::D2p, &link, &plink);
        let p2d = keys.link_key(Dir::P2d, &link, &plink);

        println!("code_shown {}", crate::code::format(&code).as_str());
        println!("hub {}", keys.hub_id);
        println!("relay_key {}", BASE64URL_NOPAD.encode(&keys.relay_key));
        println!("link {}", HEXLOWER.encode(&link));
        println!("plink {}", HEXLOWER.encode(&plink));
        println!("k_d2p {}", BASE64URL_NOPAD.encode(&d2p[..]));
        println!("k_p2d {}", BASE64URL_NOPAD.encode(&p2d[..]));

        // A frame the phone has to be able to open, sealed the way the
        // desktop seals one.
        let mut seal = Sealer::new(d2p, Dir::D2p, link, [0u8; LINK_BYTES]);
        let plain = br#"{"k":"notice","text":"the kettle is on"}"#;
        let (seq, ct) = seal.seal(plain);
        println!("d2p_seq {seq}");
        println!("d2p_ct {ct}");
        println!("d2p_plain {}", String::from_utf8_lossy(plain));

        // And one the phone has to be able to make, which the desktop opens.
        let mut up = Sealer::new(p2d, Dir::P2d, link, plink);
        let plain = br#"{"k":"list"}"#;
        let (seq, ct) = up.seal(plain);
        println!("p2d_seq {seq}");
        println!("p2d_ct {ct}");
        println!("p2d_plain {}", String::from_utf8_lossy(plain));
    }
}
