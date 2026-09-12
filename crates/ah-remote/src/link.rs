//! The socket to the relay.
//!
//! One thread, both directions, on a short read timeout — the shape
//! `ah-voice`'s Deepgram socket already proved out, for the same reason: a TLS
//! stream cannot be split, and a blocking read with a 20 ms bound is a poll
//! loop that costs nothing while idle.
//!
//! Nothing here knows what a frame means. It carries sealed text up and hands
//! sealed text back; the sealing, and everything that depends on understanding
//! it, belongs to whoever opened the link.

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ah_remote_proto::Role;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::crypto::{self, Keys};

/// How long a read waits before the loop looks at the outbound queue again.
/// This is the tick the whole socket runs on.
const TICK: Duration = Duration::from_millis(20);
/// Longest a TCP connect may take before the loop gives up and dials again.
const DIAL: Duration = Duration::from_secs(10);
/// Longest any single read or write of the TLS and upgrade handshake may
/// take. Without it the handshake is the one step with no bound at all, and a
/// stalled one holds the thread and whoever joins it.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// Longest the closing frames may take on the way out. Nobody is waiting for
/// what the relay says back, only for it to have been told.
const CLOSING: Duration = Duration::from_millis(150);
/// A ping this often, so a socket nobody is talking over is still known to be
/// alive. Protocol pings are answered by the runtime and cost nothing.
const KEEPALIVE: Duration = Duration::from_secs(45);
/// After a failure, wait this long before dialling again, doubling to a cap.
const BACKOFF_MIN: Duration = Duration::from_millis(400);
const BACKOFF_MAX: Duration = Duration::from_secs(16);
/// How long to wait after being told a desktop is already connected. Retrying
/// on the usual ladder would be a machine arguing with itself.
const BACKOFF_BUSY: Duration = Duration::from_secs(60);

/// What the relay is, and who we are to it.
pub struct Config {
    /// `https://…` in earnest, `http://127.0.0.1:…` against a local one.
    pub url: String,
    pub role: Role,
    /// Usually a second copy of the caller's ladder, because this one lives on
    /// the socket thread and that one lives on the publisher's. A copy is not
    /// a leak as long as it is a copy somebody is accounted for: this one is
    /// owned by the config, the config is owned by the socket thread, and the
    /// thread is joined before `Link::drop` returns — so the copy is cleared,
    /// not merely abandoned, when the link goes.
    pub keys: Keys,
}

/// What happens on the socket, as the thread sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The socket is up. Anything the far side needs told goes now.
    Open,
    /// One frame, exactly as it arrived.
    Frame(String),
    /// The socket went away and will be dialled again.
    Lost(String),
    /// It will not be dialled again: the reason will not improve by retrying.
    Fatal(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Config(String),
    Connect(String),
    /// Asked to stop while dialling, which is not a failure.
    Stopped,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Connect(m) => write!(f, "connect: {m}"),
            Error::Stopped => write!(f, "stopped"),
        }
    }
}

impl std::error::Error for Error {}

/// A live link. Dropping it closes the socket and waits for the thread.
pub struct Link {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    out: mpsc::Sender<String>,
    connected: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

impl Link {
    /// Open a link and start dialling. Returns as soon as the thread is
    /// running: the dial happens there, so no caller ever blocks on it.
    pub fn open(cfg: Config, on: impl Fn(Event) + Send + 'static) -> Result<Self, Error> {
        if cfg.url.trim().is_empty() {
            return Err(Error::Config("no relay".into()));
        }
        let (stop_tx, stop_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let connected = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("ah-remote".into())
            .spawn({
                let connected = connected.clone();
                let stopping = stopping.clone();
                move || run(cfg, stop_rx, out_rx, connected, stopping, on)
            })
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self {
            stop: Some(stop_tx),
            thread: Some(thread),
            out: out_tx,
            connected,
            stopping,
        })
    }

    /// Queue a frame. Never blocks: if the socket is down the frame waits in
    /// the channel, and whoever is filling it decides how much to keep.
    pub fn send(&self, frame: String) {
        let _ = self.out.send(frame);
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Ask the thread to stop. It notices within a tick, or between steps of
    /// a dial.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(s) = &self.stop {
            let _ = s.send(());
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shutdown();
        self.stop = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    cfg: Config,
    stop: mpsc::Receiver<()>,
    out: mpsc::Receiver<String>,
    connected: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    on: impl Fn(Event),
) {
    let mut socket: Option<WebSocket<MaybeTlsStream<TcpStream>>> = None;
    let mut backoff = BACKOFF_MIN;
    let mut retry_at: Option<Instant> = None;
    let mut last_send = Instant::now();

    // The stop channel is the tick as well as the shutdown: waiting on it is
    // the sleep, and anything arriving on it ends the loop.
    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(TICK) {
        if stopping.load(Ordering::Acquire) {
            break;
        }

        if socket.is_none() {
            if retry_at.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            match connect(&cfg, &stopping) {
                Ok(ws) => {
                    socket = Some(ws);
                    connected.store(true, Ordering::Release);
                    backoff = BACKOFF_MIN;
                    retry_at = None;
                    last_send = Instant::now();
                    on(Event::Open);
                }
                Err(Error::Stopped) => break,
                Err(e) => {
                    let wait = match &e {
                        // The relay is telling us another desktop holds this
                        // pairing. Hammering it would not change its mind.
                        Error::Connect(m) if m.contains("already connected") => BACKOFF_BUSY,
                        _ => backoff,
                    };
                    if fatal(&e) {
                        on(Event::Fatal(e.to_string()));
                        break;
                    }
                    on(Event::Lost(e.to_string()));
                    retry_at = Some(Instant::now() + wait);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            }
        }

        let Some(ws) = socket.as_mut() else { continue };

        // Out first: a frame waiting is worth more than a frame arriving.
        let mut broke = None;
        while let Ok(frame) = out.try_recv() {
            if let Err(e) = ws.send(Message::text(frame)) {
                broke = Some(e.to_string());
                break;
            }
            last_send = Instant::now();
        }
        if broke.is_none() && last_send.elapsed() >= KEEPALIVE {
            if let Err(e) = ws.send(Message::Ping(Vec::new().into())) {
                broke = Some(e.to_string());
            }
            last_send = Instant::now();
        }

        // In: drain whatever is there, then go back to waiting.
        if broke.is_none() {
            loop {
                match ws.read() {
                    Ok(Message::Text(t)) => on(Event::Frame(t.to_string())),
                    // Pings are answered inside `read`, and nothing here sends
                    // anything a pong or a binary frame would mean.
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(e) => {
                        broke = Some(e.to_string());
                        break;
                    }
                }
            }
        }

        if let Some(why) = broke {
            close(socket.take());
            connected.store(false, Ordering::Release);
            on(Event::Lost(why));
            retry_at = Some(Instant::now() + backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    connected.store(false, Ordering::Release);
    if let Some(ws) = socket.as_mut() {
        // Give the close frames a moment, but not the write timeout a live
        // socket runs with: nobody is waiting on the answer.
        if let MaybeTlsStream::Rustls(s) = ws.get_ref() {
            let _ = s.sock.set_write_timeout(Some(CLOSING));
        }
    }
    close(socket);
}

/// Whether a failure is worth another try. A refused signature or a revoked
/// pairing will be refused again in exactly the same way.
fn fatal(e: &Error) -> bool {
    match e {
        Error::Connect(m) => {
            m.contains("refused the pairing") || m.contains("the pairing was revoked")
        }
        _ => false,
    }
}

fn close(socket: Option<WebSocket<MaybeTlsStream<TcpStream>>>) {
    if let Some(mut ws) = socket {
        let _ = ws.close(None);
        let _ = ws.flush();
    }
}

/// Dial the relay, in steps, checking between each whether anybody still
/// wants the connection.
fn connect(
    cfg: &Config,
    stopping: &AtomicBool,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, Error> {
    let (scheme, host, port, base) = parts(&cfg.url)?;
    let ts = now_ms();
    let nonce = crypto::new_nonce();
    let sig = cfg.keys.sign_connect(cfg.role, ts, &nonce);
    // The signature goes in a header, not in the query. A URL is written down
    // in access logs, in proxies and in whatever keeps a request record, and a
    // signature sitting in one of those is a signature somebody reading them
    // can use for as long as the clocks allow.
    let url = format!(
        "{scheme}://{base}/hub/{}?r={}&ts={ts}&n={}",
        cfg.keys.hub_id,
        cfg.role.as_str(),
        esc(&nonce),
    );

    let request = tungstenite::http::Request::builder()
        .uri(&url)
        .header("Host", format!("{host}:{port}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .header("User-Agent", concat!("ah/", env!("CARGO_PKG_VERSION")))
        .header("x-ah-auth", &sig)
        .body(())
        .map_err(|e| Error::Config(e.to_string()))?;

    let sock = tcp(&host, port, stopping)?;
    if stopping.load(Ordering::Acquire) {
        return Err(Error::Stopped);
    }
    // The handshake reads and writes before tungstenite hands the socket
    // back, so the bound has to be in place before it starts.
    let _ = sock.set_read_timeout(Some(HANDSHAKE));
    let _ = sock.set_write_timeout(Some(HANDSHAKE));

    let connector = tungstenite::Connector::Rustls(tls_config());
    let (ws, _resp) = tungstenite::client_tls_with_config(request, sock, None, Some(connector))
        .map_err(|e| Error::Connect(describe(e)))?;
    if stopping.load(Ordering::Acquire) {
        return Err(Error::Stopped);
    }
    if let MaybeTlsStream::Rustls(s) = ws.get_ref() {
        // The short read timeout is what makes one blocking socket serve both
        // directions. The handshake's generous one is replaced now that every
        // read is expected to come up empty.
        let _ = s.sock.set_read_timeout(Some(TICK));
        let _ = s.sock.set_write_timeout(Some(HANDSHAKE));
        let _ = s.sock.set_nodelay(true);
    }
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_read_timeout(Some(TICK));
        let _ = s.set_write_timeout(Some(HANDSHAKE));
        let _ = s.set_nodelay(true);
    }
    Ok(ws)
}

/// `(ws scheme, host, port, host:port with any path)` from a relay URL.
fn parts(url: &str) -> Result<(&'static str, String, u16, String), Error> {
    let (scheme, rest) = match url.trim().trim_end_matches('/') {
        u if u.starts_with("https://") => ("wss", &u[8..]),
        u if u.starts_with("http://") => ("ws", &u[7..]),
        u => return Err(Error::Config(format!("not a relay URL: {u}"))),
    };
    let authority = rest.split('/').next().unwrap_or(rest).to_string();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| Error::Config(format!("not a port: {p}")))?,
        ),
        None => (authority.clone(), if scheme == "wss" { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(Error::Config("no host in the relay URL".into()));
    }
    Ok((scheme, host, port, authority))
}

fn tcp(host: &str, port: u16, stopping: &AtomicBool) -> Result<TcpStream, Error> {
    use std::net::ToSocketAddrs;
    // The one step with no timeout of its own: the resolver's is the only
    // bound on it.
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Connect(e.to_string()))?
        .next()
        .ok_or_else(|| Error::Connect(format!("{host} does not resolve")))?;
    if stopping.load(Ordering::Acquire) {
        return Err(Error::Stopped);
    }
    TcpStream::connect_timeout(&addr, DIAL).map_err(|e| Error::Connect(e.to_string()))
}

/// A refusal arrives as an HTTP response, not a socket error, so say what it
/// means rather than printing a status line.
fn describe<S>(e: tungstenite::HandshakeError<S>) -> String
where
    S: tungstenite::handshake::HandshakeRole,
{
    match e {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => {
            let status = r.status().as_u16();
            match status {
                401 => {
                    let body = r
                        .body()
                        .as_ref()
                        .map(|b| String::from_utf8_lossy(b).to_string())
                        .unwrap_or_default();
                    if body.contains("skew") {
                        format!("the relay and this machine disagree about the time: {body}")
                    } else {
                        "the relay refused the pairing".into()
                    }
                }
                404 => "the relay has never heard of this pairing".into(),
                409 => "a desktop is already connected to this pairing".into(),
                410 => "the pairing was revoked".into(),
                _ => format!("the relay answered {status}"),
            }
        }
        tungstenite::HandshakeError::Failure(e) => e.to_string(),
        tungstenite::HandshakeError::Interrupted(_) => "the handshake stalled".into(),
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    // Built once and cloned per dial. The provider is named rather than left
    // to the process-wide default, which two crates can disagree about.
    //
    // The same thirty lines live in `ah-voice`. Sharing them would mean this
    // crate depending on the voice crate, or `ah-core` growing its first
    // feature flag to make a TLS config optional; a second root store built
    // from the same list is the cheaper of the three.
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Percent-encode what would otherwise end a query value.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_url_becomes_a_socket_url() {
        let (scheme, host, port, base) = parts("https://ah-relay.x.workers.dev").unwrap();
        assert_eq!(
            (scheme, host.as_str(), port),
            ("wss", "ah-relay.x.workers.dev", 443)
        );
        assert_eq!(base, "ah-relay.x.workers.dev");
    }

    #[test]
    fn a_local_relay_keeps_its_port_and_stays_plain() {
        let (scheme, host, port, base) = parts("http://127.0.0.1:8787").unwrap();
        assert_eq!((scheme, host.as_str(), port), ("ws", "127.0.0.1", 8787));
        assert_eq!(base, "127.0.0.1:8787");
    }

    #[test]
    fn a_trailing_slash_and_a_path_do_not_confuse_it() {
        let (_, host, port, _) = parts("https://relay.example.com/").unwrap();
        assert_eq!((host.as_str(), port), ("relay.example.com", 443));
    }

    #[test]
    fn something_that_is_not_a_relay_url_is_refused() {
        assert!(parts("ftp://relay.example.com").is_err());
        assert!(parts("relay.example.com").is_err());
        assert!(parts("https://").is_err());
        assert!(parts("https://host:notaport").is_err());
    }

    #[test]
    fn a_missing_relay_is_refused_before_a_socket_is_opened() {
        let cfg = Config {
            url: String::new(),
            role: Role::Desk,
            keys: Keys::derive(&[1u8; crypto::CODE_BYTES]),
        };
        assert_eq!(
            Link::open(cfg, |_| {}).err(),
            Some(Error::Config("no relay".into()))
        );
    }

    #[test]
    fn what_the_relay_says_is_turned_into_what_it_means() {
        // The refusals that matter are the ones a person has to act on, and
        // each of them arrives as a status rather than as a socket error.
        for (status, expect) in [
            (404u16, "never heard of"),
            (409, "already connected"),
            (410, "revoked"),
        ] {
            let response = tungstenite::http::Response::builder()
                .status(status)
                .body(None)
                .unwrap();
            let text = describe::<tungstenite::ClientHandshake<TcpStream>>(
                tungstenite::HandshakeError::Failure(tungstenite::Error::Http(Box::new(response))),
            );
            assert!(text.contains(expect), "{status} became {text:?}");
        }
    }

    #[test]
    fn a_pairing_that_will_never_work_is_not_dialled_again() {
        assert!(fatal(&Error::Connect("the pairing was revoked".into())));
        assert!(fatal(&Error::Connect(
            "the relay refused the pairing".into()
        )));
        // Everything else is worth another go.
        assert!(!fatal(&Error::Connect("connection reset".into())));
        assert!(!fatal(&Error::Connect(
            "a desktop is already connected to this pairing".into()
        )));
    }
}
