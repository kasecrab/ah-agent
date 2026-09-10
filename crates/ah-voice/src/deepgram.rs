//! Deepgram's live transcription socket.
//!
//! One socket, one thread, no allocation once it is open. Audio arrives
//! through a ring the capture side fills and leaves as raw `linear16` frames;
//! words come back while the sentence is still being said, which is the whole
//! reason this exists next to the batch route.
//!
//! The thread does both directions itself rather than splitting the socket:
//! a TLS stream cannot be halved, and a short read timeout turns one blocking
//! socket into a loop that writes audio, reads results, and keeps the
//! connection warm on the same 20 ms tick the microphone already runs on.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::ring::Consumer;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    /// `nova-3` and friends.
    pub model: String,
    /// Empty lets Deepgram decide.
    pub language: String,
    /// Words worth biasing towards: names, identifiers, jargon.
    pub keyterms: Vec<String>,
    pub sample_rate: u32,
    /// Silence that closes a phrase, in milliseconds.
    pub endpointing_ms: u64,
    /// How long an unused socket is held before it is dropped. 0 holds it for
    /// as long as dictation is armed.
    pub idle_secs: u64,
}

/// What the socket has to say. Every one of these crosses to the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The socket is up and audio will be listened to.
    Open,
    /// Words that may still change. Each one replaces the last.
    Interim(String),
    /// Words that will not change. These accumulate.
    Final(String),
    /// Deepgram heard the speaker stop.
    UtteranceEnd,
    /// Something went wrong. The socket reconnects on its own; this is for
    /// telling the user why the words paused.
    Trouble(String),
}

#[derive(Debug)]
pub enum Error {
    Config(String),
    Connect(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(m) => write!(f, "{m}"),
            Error::Connect(m) => write!(f, "cannot reach Deepgram: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// A live transcription session. Dropping it closes the socket.
pub struct Live {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    listening: Arc<AtomicBool>,
    /// Set when the talk key comes up, so the socket flushes what it holds
    /// instead of waiting for the endpointing silence to elapse.
    flush: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
}

impl Live {
    /// Start the socket thread. `audio` is drained continuously; frames are
    /// only sent while `listen(true)` is in force.
    pub fn open(
        cfg: Config,
        audio: Consumer,
        on: impl Fn(Event) + Send + 'static,
    ) -> Result<Self, Error> {
        if cfg.api_key.trim().is_empty() {
            return Err(Error::Config("no Deepgram API key".into()));
        }
        let listening = Arc::new(AtomicBool::new(false));
        let flush = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (l, fl, c) = (listening.clone(), flush.clone(), connected.clone());
        let thread = std::thread::Builder::new()
            .name("ah-voice-dg".into())
            .spawn(move || run(cfg, audio, l, fl, c, stop_rx, on))
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self {
            stop: Some(stop_tx),
            thread: Some(thread),
            listening,
            flush,
            connected,
        })
    }

    /// The talk key went down, or came up.
    pub fn listen(&self, on: bool) {
        if !on && self.listening.swap(false, Ordering::AcqRel) {
            // Ask for what is buffered rather than waiting out the silence.
            self.flush.store(true, Ordering::Release);
        } else if on {
            self.listening.store(true, Ordering::Release);
        }
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.listening.store(false, Ordering::Release);
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// How long a read waits before the loop looks at the audio again. This is
/// the tick the whole socket runs on.
const TICK: Duration = Duration::from_millis(20);
/// Deepgram drops a silent socket; this is well inside that.
const KEEPALIVE: Duration = Duration::from_secs(5);
/// After a failure, wait this long before dialling again, doubling to a cap.
const BACKOFF_MIN: Duration = Duration::from_millis(400);
const BACKOFF_MAX: Duration = Duration::from_secs(16);

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: Config,
    audio: Consumer,
    listening: Arc<AtomicBool>,
    flush: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    stop: mpsc::Receiver<()>,
    on: impl Fn(Event),
) {
    let mut socket: Option<WebSocket<MaybeTlsStream<TcpStream>>> = None;
    let mut last_send = Instant::now();
    let mut idle_since = Instant::now();
    let mut backoff = BACKOFF_MIN;
    let mut retry_at: Option<Instant> = None;
    // Reused for every frame, so the steady state allocates nothing.
    let mut pcm: Vec<i16> = Vec::with_capacity(cfg.sample_rate as usize);
    let mut bytes: Vec<u8> = Vec::with_capacity(cfg.sample_rate as usize * 2);

    loop {
        match stop.recv_timeout(TICK) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => break,
        }
        let want = listening.load(Ordering::Acquire);

        // Hold the socket open for a while after the key comes up: the next
        // phrase then costs no handshake at all.
        if !want && socket.is_some() && cfg.idle_secs > 0 {
            let idle = idle_since.elapsed().as_secs();
            if idle >= cfg.idle_secs && !flush.load(Ordering::Acquire) {
                close(socket.take());
                connected.store(false, Ordering::Release);
            }
        }
        if want {
            idle_since = Instant::now();
        }

        if socket.is_none() {
            if !want {
                audio.keep_last(0);
                continue;
            }
            if let Some(at) = retry_at
                && Instant::now() < at
            {
                audio.keep_last(0);
                continue;
            }
            match connect(&cfg) {
                Ok(ws) => {
                    socket = Some(ws);
                    connected.store(true, Ordering::Release);
                    backoff = BACKOFF_MIN;
                    retry_at = None;
                    last_send = Instant::now();
                    // Whatever was said before the socket came up is stale.
                    audio.keep_last(0);
                    on(Event::Open);
                }
                Err(e) => {
                    on(Event::Trouble(e.to_string()));
                    retry_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    audio.keep_last(0);
                    continue;
                }
            }
        }
        let Some(ws) = socket.as_mut() else { continue };

        // ---- audio out ----
        if want {
            pcm.clear();
            audio.drain(&mut pcm);
            if !pcm.is_empty() {
                bytes.clear();
                for s in &pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                if let Err(e) = ws.send(Message::Binary(bytes.clone().into())) {
                    on(Event::Trouble(e.to_string()));
                    close(socket.take());
                    connected.store(false, Ordering::Release);
                    retry_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
                last_send = Instant::now();
            }
        } else {
            // Not listening: throw the audio away rather than letting it pile
            // up, and pay nothing for the silence.
            audio.keep_last(0);
            if flush.swap(false, Ordering::AcqRel) {
                let _ = ws.send(Message::text("{\"type\":\"Finalize\"}"));
                last_send = Instant::now();
            }
            if last_send.elapsed() >= KEEPALIVE {
                let _ = ws.send(Message::text("{\"type\":\"KeepAlive\"}"));
                last_send = Instant::now();
            }
        }

        // ---- results in ----
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => {
                    for ev in parse(&t) {
                        on(ev);
                    }
                }
                Ok(Message::Close(_)) => {
                    close(socket.take());
                    connected.store(false, Ordering::Release);
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    // Nothing waiting. Documented as recoverable: the next
                    // read picks up where this one stopped.
                    break;
                }
                Err(e) => {
                    on(Event::Trouble(e.to_string()));
                    close(socket.take());
                    connected.store(false, Ordering::Release);
                    retry_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    break;
                }
            }
        }
    }
    if let Some(mut ws) = socket.take() {
        let _ = ws.send(Message::text("{\"type\":\"CloseStream\"}"));
        let _ = ws.close(None);
        let _ = ws.flush();
    }
    connected.store(false, Ordering::Release);
}

fn close(socket: Option<WebSocket<MaybeTlsStream<TcpStream>>>) {
    if let Some(mut ws) = socket {
        let _ = ws.send(Message::text("{\"type\":\"CloseStream\"}"));
        let _ = ws.close(None);
        let _ = ws.flush();
    }
}

/// Pull the transcript out of a `Results` message. Deepgram sends other
/// message types on the same socket; they are not errors, they are simply
/// not what dictation needs.
fn parse(text: &str) -> Vec<Event> {
    let Some(v) = json::parse(text) else {
        return Vec::new();
    };
    match v.str("type").unwrap_or("") {
        "Results" => {
            let said = v
                .get("channel")
                .and_then(|c| c.get("alternatives"))
                .and_then(|a| a.first())
                .and_then(|a| a.str("transcript"))
                .unwrap_or("")
                .trim()
                .to_string();
            if said.is_empty() {
                return Vec::new();
            }
            let is_final = v.bool("is_final").unwrap_or(false);
            vec![if is_final {
                Event::Final(said)
            } else {
                Event::Interim(said)
            }]
        }
        "UtteranceEnd" => vec![Event::UtteranceEnd],
        _ => Vec::new(),
    }
}

fn connect(cfg: &Config) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, Error> {
    let mut url = format!(
        "wss://api.deepgram.com/v1/listen\
         ?model={}&encoding=linear16&sample_rate={}&channels=1\
         &interim_results=true&punctuate=true&smart_format=true\
         &endpointing={}&utterance_end_ms=1000&filler_words=false",
        esc(&cfg.model),
        cfg.sample_rate,
        cfg.endpointing_ms
    );
    if !cfg.language.trim().is_empty() {
        url.push_str("&language=");
        url.push_str(&esc(cfg.language.trim()));
    }
    for term in &cfg.keyterms {
        let term = term.trim();
        if !term.is_empty() {
            url.push_str("&keyterm=");
            url.push_str(&esc(term));
        }
    }
    let request = tungstenite::http::Request::builder()
        .uri(&url)
        .header("Authorization", format!("Token {}", cfg.api_key.trim()))
        .header("Host", "api.deepgram.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .header("User-Agent", concat!("ah/", env!("CARGO_PKG_VERSION")))
        .body(())
        .map_err(|e| Error::Config(e.to_string()))?;

    let connector = tungstenite::Connector::Rustls(tls_config());
    let (ws, _resp) = tungstenite::client_tls_with_config(request, tcp()?, None, Some(connector))
        .map_err(|e| Error::Connect(describe(e)))?;
    if let MaybeTlsStream::Rustls(s) = ws.get_ref() {
        // The read timeout is what makes one blocking socket serve both
        // directions without a second thread.
        let _ = s.sock.set_read_timeout(Some(TICK));
        let _ = s.sock.set_nodelay(true);
    }
    Ok(ws)
}

fn tcp() -> Result<TcpStream, Error> {
    use std::net::ToSocketAddrs;
    let addr = ("api.deepgram.com", 443)
        .to_socket_addrs()
        .map_err(|e| Error::Connect(e.to_string()))?
        .next()
        .ok_or_else(|| Error::Connect("api.deepgram.com does not resolve".into()))?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| Error::Connect(e.to_string()))
}

/// A 401 arrives as a plain HTTP response, not a socket error, so say what it
/// means rather than printing the status line.
fn describe<S>(e: tungstenite::HandshakeError<S>) -> String
where
    S: tungstenite::handshake::HandshakeRole,
{
    match e {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => {
            let code = r.status().as_u16();
            if code == 401 || code == 403 {
                "the Deepgram API key was refused".into()
            } else {
                format!("Deepgram answered {code}")
            }
        }
        tungstenite::HandshakeError::Failure(e) => e.to_string(),
        tungstenite::HandshakeError::Interrupted(_) => "the handshake stalled".into(),
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("rustls defaults")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

/// Percent-encode everything that is not plainly safe in a query value.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Just enough JSON to read a result message. Pulling in a parser for four
/// fields would be the only heavyweight thing in this crate.
mod json {
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        Null,
        Bool(bool),
        Num(f64),
        Str(String),
        Arr(Vec<Value>),
        Obj(Vec<(String, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn first(&self) -> Option<&Value> {
            match self {
                Value::Arr(a) => a.first(),
                _ => None,
            }
        }
        pub fn str(&self, key: &str) -> Option<&str> {
            match self.get(key) {
                Some(Value::Str(s)) => Some(s),
                _ => None,
            }
        }
        pub fn bool(&self, key: &str) -> Option<bool> {
            match self.get(key) {
                Some(Value::Bool(b)) => Some(*b),
                _ => None,
            }
        }
    }

    pub fn parse(text: &str) -> Option<Value> {
        let b = text.as_bytes();
        let mut i = 0;
        let v = value(b, &mut i)?;
        Some(v)
    }

    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    }

    fn value(b: &[u8], i: &mut usize) -> Option<Value> {
        ws(b, i);
        match *b.get(*i)? {
            b'{' => object(b, i),
            b'[' => array(b, i),
            b'"' => string(b, i).map(Value::Str),
            b't' => lit(b, i, b"true", Value::Bool(true)),
            b'f' => lit(b, i, b"false", Value::Bool(false)),
            b'n' => lit(b, i, b"null", Value::Null),
            _ => number(b, i),
        }
    }

    fn lit(b: &[u8], i: &mut usize, want: &[u8], v: Value) -> Option<Value> {
        if b.len() >= *i + want.len() && &b[*i..*i + want.len()] == want {
            *i += want.len();
            Some(v)
        } else {
            None
        }
    }

    fn number(b: &[u8], i: &mut usize) -> Option<Value> {
        let start = *i;
        while *i < b.len() && matches!(b[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
            *i += 1;
        }
        std::str::from_utf8(&b[start..*i])
            .ok()?
            .parse::<f64>()
            .ok()
            .map(Value::Num)
    }

    fn string(b: &[u8], i: &mut usize) -> Option<String> {
        if *b.get(*i)? != b'"' {
            return None;
        }
        *i += 1;
        let mut out = String::new();
        loop {
            let c = *b.get(*i)?;
            *i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *b.get(*i)?;
                    *i += 1;
                    match e {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?;
                            *i += 4;
                            let n = u32::from_str_radix(hex, 16).ok()?;
                            // A surrogate pair is two escapes; join them.
                            let ch = if (0xD800..0xDC00).contains(&n) {
                                if b.get(*i) != Some(&b'\\') || b.get(*i + 1) != Some(&b'u') {
                                    return None;
                                }
                                *i += 2;
                                let lo = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?;
                                *i += 4;
                                let lo = u32::from_str_radix(lo, 16).ok()?;
                                char::from_u32(0x10000 + ((n - 0xD800) << 10) + (lo - 0xDC00))?
                            } else {
                                char::from_u32(n)?
                            };
                            out.push(ch);
                        }
                        other => out.push(other as char),
                    }
                }
                _ => {
                    // Multi-byte UTF-8 passes through a byte at a time.
                    let start = *i - 1;
                    let len = utf8_len(c);
                    *i = start + len;
                    out.push_str(std::str::from_utf8(b.get(start..*i)?).ok()?);
                }
            }
        }
    }

    fn utf8_len(first: u8) -> usize {
        match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        }
    }

    fn array(b: &[u8], i: &mut usize) -> Option<Value> {
        *i += 1;
        let mut out = Vec::new();
        ws(b, i);
        if *b.get(*i)? == b']' {
            *i += 1;
            return Some(Value::Arr(out));
        }
        loop {
            out.push(value(b, i)?);
            ws(b, i);
            match *b.get(*i)? {
                b',' => *i += 1,
                b']' => {
                    *i += 1;
                    return Some(Value::Arr(out));
                }
                _ => return None,
            }
        }
    }

    fn object(b: &[u8], i: &mut usize) -> Option<Value> {
        *i += 1;
        let mut out = Vec::new();
        ws(b, i);
        if *b.get(*i)? == b'}' {
            *i += 1;
            return Some(Value::Obj(out));
        }
        loop {
            ws(b, i);
            let k = string(b, i)?;
            ws(b, i);
            if *b.get(*i)? != b':' {
                return None;
            }
            *i += 1;
            out.push((k, value(b, i)?));
            ws(b, i);
            match *b.get(*i)? {
                b',' => *i += 1,
                b'}' => {
                    *i += 1;
                    return Some(Value::Obj(out));
                }
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULT: &str = r#"{"type":"Results","channel_index":[0,1],"duration":1.02,"start":0.0,
        "is_final":false,"speech_final":false,
        "channel":{"alternatives":[{"transcript":"fix the auth middleware","confidence":0.99,"words":[]}]},
        "metadata":{"request_id":"abc"}}"#;

    #[test]
    fn an_interim_result_is_words_that_may_change() {
        assert_eq!(
            parse(RESULT),
            vec![Event::Interim("fix the auth middleware".into())]
        );
    }

    #[test]
    fn a_final_result_is_words_that_will_not() {
        let t = RESULT.replace("\"is_final\":false", "\"is_final\":true");
        assert_eq!(
            parse(&t),
            vec![Event::Final("fix the auth middleware".into())]
        );
    }

    #[test]
    fn an_empty_transcript_says_nothing() {
        let t = RESULT.replace("fix the auth middleware", "");
        assert!(parse(&t).is_empty());
    }

    #[test]
    fn the_end_of_an_utterance_is_reported() {
        let t = r#"{"type":"UtteranceEnd","channel":[0,1],"last_word_end":3.2}"#;
        assert_eq!(parse(t), vec![Event::UtteranceEnd]);
    }

    #[test]
    fn other_messages_are_not_errors() {
        assert!(parse(r#"{"type":"Metadata","request_id":"x"}"#).is_empty());
        assert!(parse(r#"{"type":"SpeechStarted","timestamp":1.0}"#).is_empty());
        assert!(parse("not json at all").is_empty());
        assert!(parse("").is_empty());
    }

    #[test]
    fn escapes_and_accents_survive_the_parser() {
        let t = r#"{"type":"Results","is_final":true,"channel":{"alternatives":[
            {"transcript":"café \"quoted\" and 😀"}]}}"#;
        assert_eq!(
            parse(t),
            vec![Event::Final("café \"quoted\" and 😀".into())]
        );
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(esc("nova-3"), "nova-3");
        assert_eq!(esc("customer service"), "customer%20service");
        assert_eq!(esc("a&b=c"), "a%26b%3Dc");
    }

    /// Talks to Deepgram, so it is not part of `just test`. With a real
    /// `DEEPGRAM_API_KEY` it streams a PCM file and prints what comes back;
    /// with a bogus one it checks the refusal is reported as a refusal.
    /// `cargo test -p ah-voice -- --ignored --nocapture the_socket`
    #[test]
    #[ignore]
    fn the_socket_reaches_deepgram() {
        let key = std::env::var("DEEPGRAM_API_KEY").unwrap_or_else(|_| "bogus-key".into());
        let raw = std::env::var("DG_TEST_PCM").unwrap_or_else(|_| "/tmp/say16.raw".into());
        let (producer, consumer) = crate::ring::ring(16_000 * 4);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Event>::new()));
        let sink = seen.clone();
        let live = Live::open(
            Config {
                api_key: key,
                model: "nova-3".into(),
                language: String::new(),
                keyterms: vec!["middleware".into()],
                sample_rate: 16_000,
                endpointing_ms: 300,
                idle_secs: 120,
            },
            consumer,
            move |e| {
                println!("{e:?}");
                sink.lock().unwrap().push(e);
            },
        )
        .expect("start the socket thread");
        live.listen(true);

        // Feed the file at the rate it would have been spoken.
        if let Ok(bytes) = std::fs::read(&raw) {
            let step = 16_000 / 25 * 2; // 40 ms
            for c in bytes.chunks(step) {
                let pcm: Vec<i16> = c
                    .chunks_exact(2)
                    .map(|p| i16::from_le_bytes([p[0], p[1]]))
                    .collect();
                producer.write(&pcm);
                std::thread::sleep(Duration::from_millis(40));
            }
        } else {
            println!("no PCM at {raw}; only the handshake is being checked");
            std::thread::sleep(Duration::from_secs(2));
        }
        live.listen(false);
        std::thread::sleep(Duration::from_secs(3));
        let got = seen.lock().unwrap().clone();
        drop(live);
        let refused = got
            .iter()
            .any(|e| matches!(e, Event::Trouble(m) if m.contains("refused")));
        if refused {
            println!("key refused, which is the expected answer for a bogus one");
            return;
        }
        assert!(
            got.iter().any(|e| matches!(e, Event::Open)),
            "the socket never opened: {got:?}"
        );
        assert!(
            got.iter()
                .any(|e| matches!(e, Event::Final(_) | Event::Interim(_))),
            "no words came back: {got:?}"
        );
    }

    #[test]
    fn a_missing_key_is_refused_before_a_socket_is_opened() {
        let (_p, c) = crate::ring::ring(16);
        let cfg = Config {
            api_key: "  ".into(),
            model: "nova-3".into(),
            language: String::new(),
            keyterms: Vec::new(),
            sample_rate: 16_000,
            endpointing_ms: 300,
            idle_secs: 120,
        };
        assert!(matches!(Live::open(cfg, c, |_| {}), Err(Error::Config(_))));
    }
}
