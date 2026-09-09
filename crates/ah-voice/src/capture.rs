//! Getting samples out of a microphone.
//!
//! Two ways in. A recorder process printing raw PCM on stdout needs nothing
//! linked and nothing installed on a desktop Linux, and dies the moment
//! dictation ends. The `mic` feature links `cpal` instead, which is the only
//! way to reach a default input device on macOS and Windows without asking
//! anyone to install a tool first.
//!
//! Either way the samples land in the same ring, and everything downstream is
//! the same code.

use std::process::{Child, Command, Stdio};

use crate::ring::{self, Consumer, Producer};

#[derive(Debug)]
pub enum Error {
    /// Nothing to record with, and what was looked for.
    NoTool(String),
    /// A device exists but would not open.
    Device(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoTool(m) | Error::Device(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

pub struct Opened {
    pub rate: u32,
    pub channels: u16,
    /// What is doing the recording, for the error message and the log.
    pub source: String,
    pub audio: Consumer,
    stop: Stop,
}

impl Opened {
    /// Close the device. Nothing keeps a microphone open past this.
    pub fn close(self) {
        self.stop.close();
    }
}

enum Stop {
    Process {
        child: Child,
        pump: Option<std::thread::JoinHandle<()>>,
    },
    #[cfg(feature = "mic")]
    Thread(std::sync::mpsc::Sender<()>, Option<std::thread::JoinHandle<()>>),
}

impl Stop {
    fn close(self) {
        match self {
            Stop::Process { mut child, pump } => {
                // The recorder runs under `sh`, which may fork rather than
                // exec. Killing only the shell leaves the recorder holding
                // the pipe, and the thread reading it blocked for ever, so
                // the whole group goes.
                kill_group(child.id() as i32);
                let _ = child.kill();
                let _ = child.wait();
                if let Some(p) = pump {
                    let _ = p.join();
                }
            }
            #[cfg(feature = "mic")]
            Stop::Thread(tx, join) => {
                let _ = tx.send(());
                if let Some(j) = join {
                    let _ = j.join();
                }
            }
        }
    }
}

/// What to open, and how much of it to hold.
pub struct Request {
    /// Empty picks the system default.
    pub device: String,
    /// A shell command printing raw signed 16-bit little-endian mono PCM on
    /// stdout, which overrides everything else.
    pub command: String,
    /// 0 asks for the target rate and takes what the device offers.
    pub rate: u32,
    pub ring_ms: u64,
}

pub fn open(req: &Request) -> Result<Opened, Error> {
    let want = if req.rate == 0 {
        crate::resample::TARGET_RATE
    } else {
        req.rate
    };
    if !req.command.trim().is_empty() {
        return open_command(&req.command, want, req.ring_ms);
    }
    #[cfg(feature = "mic")]
    match open_cpal(req, want) {
        Ok(o) => return Ok(o),
        // A machine with no sound card still has a recorder tool sometimes,
        // and over ssh that is the only thing that works.
        Err(Error::NoTool(_)) => {}
        Err(e) => return Err(e),
    }
    open_detected(want, req.ring_ms)
}

// ---- recorder process ----------------------------------------------------

/// In the order they are worth trying: the session's own audio server first,
/// then the one below it, then the general-purpose tools.
const RECORDERS: &[(&str, &str)] = &[
    (
        "pw-record",
        "pw-record --rate {rate} --channels 1 --format s16 -",
    ),
    (
        "parec",
        "parec --format=s16le --rate={rate} --channels=1 --raw",
    ),
    (
        "arecord",
        "arecord -q -t raw -f S16_LE -r {rate} -c 1 -",
    ),
    (
        "ffmpeg",
        "ffmpeg -v quiet -f {ffin} -i {ffdev} -ac 1 -ar {rate} -f s16le -",
    ),
    ("sox", "sox -q -d -t raw -b 16 -e signed -c 1 -r {rate} -"),
];

fn has(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn ffmpeg_input() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("avfoundation", ":default")
    } else if cfg!(target_os = "windows") {
        ("dshow", "audio=default")
    } else {
        ("alsa", "default")
    }
}

fn open_detected(rate: u32, ring_ms: u64) -> Result<Opened, Error> {
    for (program, template) in RECORDERS {
        if !has(program) {
            continue;
        }
        let (ffin, ffdev) = ffmpeg_input();
        let cmd = template
            .replace("{rate}", &rate.to_string())
            .replace("{ffin}", ffin)
            .replace("{ffdev}", ffdev);
        match open_command(&cmd, rate, ring_ms) {
            Ok(mut o) => {
                o.source = (*program).to_string();
                return Ok(o);
            }
            Err(e) => {
                crate::log(&format!("{program} would not start: {e}"));
            }
        }
    }
    let names = RECORDERS
        .iter()
        .map(|(p, _)| *p)
        .collect::<Vec<_>>()
        .join(", ");
    Err(Error::NoTool(format!(
        "no way to record: none of {names} is installed, \
         and this build has no built-in capture. Install one of them, \
         or set voice.capture_cmd to a command that prints raw s16le mono PCM"
    )))
}

fn open_command(cmd: &str, rate: u32, ring_ms: u64) -> Result<Opened, Error> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own group, so stopping it stops whatever it started.
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::NoTool(format!("cannot run `{cmd}`: {e}"))
            } else {
                Error::Device(format!("cannot run `{cmd}`: {e}"))
            }
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Device("recorder produced no output".into()))?;
    let (producer, audio) = ring::ring(samples(rate, 1, ring_ms));
    let reader = std::thread::Builder::new()
        .name("ah-voice-mic".into())
        .spawn(move || pump(&mut stdout, &producer))
        .map_err(|e| Error::Device(e.to_string()))?;
    Ok(Opened {
        rate,
        channels: 1,
        source: cmd.split_whitespace().next().unwrap_or(cmd).to_string(),
        audio,
        stop: Stop::Process {
            child,
            pump: Some(reader),
        },
    })
}

fn pump(stdout: &mut impl std::io::Read, producer: &Producer) {
    // One read is about 40 ms of audio. Small enough that the run-up to a
    // phrase stays accurate, big enough not to wake for every frame.
    let mut buf = [0u8; 2048];
    let mut odd: Option<u8> = None;
    let mut pcm: Vec<i16> = Vec::with_capacity(1024);
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pcm.clear();
        let mut bytes = &buf[..n];
        if let Some(lo) = odd.take() {
            let Some((hi, rest)) = bytes.split_first() else {
                odd = Some(lo);
                continue;
            };
            pcm.push(i16::from_le_bytes([lo, *hi]));
            bytes = rest;
        }
        let pairs = bytes.len() / 2;
        for c in bytes[..pairs * 2].chunks(2) {
            pcm.push(i16::from_le_bytes([c[0], c[1]]));
        }
        if bytes.len() % 2 == 1 {
            odd = Some(bytes[bytes.len() - 1]);
        }
        producer.write(&pcm);
    }
}

#[cfg(unix)]
fn kill_group(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(-pid, 9);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: i32) {}

fn samples(rate: u32, channels: u16, ms: u64) -> usize {
    (rate as u64 * channels as u64 * ms / 1000) as usize
}

/// Input devices, for the picker. The default is first and unnamed.
pub fn devices() -> Vec<String> {
    #[cfg(feature = "mic")]
    {
        cpal_devices()
    }
    #[cfg(not(feature = "mic"))]
    {
        Vec::new()
    }
}

// ---- cpal ----------------------------------------------------------------

#[cfg(feature = "mic")]
mod native {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    pub fn devices() -> Vec<String> {
        let host = cpal::default_host();
        let mut out = Vec::new();
        if let Ok(list) = host.input_devices() {
            for d in list {
                out.push(d.to_string());
            }
        }
        out
    }

    /// `cpal`'s stream is not `Send` on every backend, so it is built, played
    /// and dropped on one thread of its own, which then sits on a channel.
    pub fn open(req: &Request, want: u32) -> Result<Opened, Error> {
        let device = req.device.clone();
        let ring_ms = req.ring_ms;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(u32, u16, String, Consumer), Error>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let join = std::thread::Builder::new()
            .name("ah-voice-mic".into())
            .spawn(move || {
                let built = build(&device, want, ring_ms);
                match built {
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                    Ok((stream, info)) => {
                        if ready_tx.send(Ok(info)).is_err() {
                            return;
                        }
                        if let Err(e) = stream.play() {
                            crate::log(&format!("microphone would not start: {e}"));
                            return;
                        }
                        let _ = stop_rx.recv();
                        drop(stream);
                    }
                }
            })
            .map_err(|e| Error::Device(e.to_string()))?;
        let (rate, channels, source, audio) = ready_rx
            .recv()
            .map_err(|_| Error::Device("microphone thread stopped".into()))??;
        Ok(Opened {
            rate,
            channels,
            source,
            audio,
            stop: Stop::Thread(stop_tx, Some(join)),
        })
    }

    type Built = (cpal::Stream, (u32, u16, String, Consumer));

    fn build(device: &str, want: u32, ring_ms: u64) -> Result<Built, Error> {
        // ALSA writes its own complaints to stderr, which in a full-screen
        // terminal lands on top of the conversation. Hold it shut while the
        // device is probed and opened.
        let _hush = Hush::new();
        let host = cpal::default_host();
        let dev = if device.trim().is_empty() {
            host.default_input_device()
        } else {
            host.input_devices()
                .ok()
                .and_then(|mut l| l.find(|d| d.to_string() == device))
        };
        let Some(dev) = dev else {
            return Err(Error::NoTool(if device.trim().is_empty() {
                "no input device: nothing is set as the system microphone".into()
            } else {
                format!("no input device named `{device}`")
            }));
        };
        let name = dev.to_string();
        let default = dev
            .default_input_config()
            .map_err(|e| Error::Device(format!("{name}: {e}")))?;
        // Ask for the rate the pipeline wants; take the device's own if it
        // refuses, and resample later.
        let mut config: cpal::StreamConfig = default.into();
        config.channels = config.channels.min(2);
        let supports_want = dev
            .supported_input_configs()
            .map(|mut it| it.any(|c| c.min_sample_rate() <= want && want <= c.max_sample_rate()))
            .unwrap_or(false);
        if supports_want {
            config.sample_rate = want;
        }
        config.buffer_size = cpal::BufferSize::Default;
        let rate = config.sample_rate;
        let channels = config.channels;
        let (producer, audio) = ring::ring(super::samples(rate, channels, ring_ms));
        let format = default.sample_format();
        let err = |e| crate::log(&format!("microphone: {e}"));
        let stream = match format {
            cpal::SampleFormat::I16 => dev.build_input_stream(
                config,
                move |data: &[i16], _: &_| producer.write(data),
                err,
                None,
            ),
            cpal::SampleFormat::U16 => dev.build_input_stream(
                config,
                move |data: &[u16], _: &_| {
                    let mut pcm = Vec::with_capacity(data.len());
                    pcm.extend(data.iter().map(|s| (*s as i32 - 32768) as i16));
                    producer.write(&pcm);
                },
                err,
                None,
            ),
            _ => dev.build_input_stream(
                config,
                move |data: &[f32], _: &_| {
                    let mut pcm = Vec::with_capacity(data.len());
                    pcm.extend(data.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
                    producer.write(&pcm);
                },
                err,
                None,
            ),
        }
        .map_err(|e| Error::Device(format!("{name}: {e}")))?;
        Ok((stream, (rate, channels, name, audio)))
    }

    /// stderr, pointed at nothing, until this is dropped.
    struct Hush(#[cfg(unix)] Option<i32>);

    impl Hush {
        #[cfg(unix)]
        fn new() -> Self {
            unsafe extern "C" {
                fn dup(fd: i32) -> i32;
                fn dup2(old: i32, new: i32) -> i32;
                fn open(path: *const u8, flags: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            unsafe {
                let saved = dup(2);
                if saved < 0 {
                    return Hush(None);
                }
                let null = open(c"/dev/null".as_ptr() as *const u8, 1);
                if null < 0 {
                    close(saved);
                    return Hush(None);
                }
                dup2(null, 2);
                close(null);
                Hush(Some(saved))
            }
        }

        #[cfg(not(unix))]
        fn new() -> Self {
            Hush()
        }
    }

    #[cfg(unix)]
    impl Drop for Hush {
        fn drop(&mut self) {
            unsafe extern "C" {
                fn dup2(old: i32, new: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            if let Some(saved) = self.0.take() {
                unsafe {
                    dup2(saved, 2);
                    close(saved);
                }
            }
        }
    }
}

#[cfg(feature = "mic")]
fn open_cpal(req: &Request, want: u32) -> Result<Opened, Error> {
    native::open(req, want)
}

#[cfg(feature = "mic")]
fn cpal_devices() -> Vec<String> {
    native::devices()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(command: &str) -> Request {
        Request {
            device: String::new(),
            command: command.into(),
            rate: 16_000,
            ring_ms: 2000,
        }
    }

    #[test]
    fn a_command_feeds_the_ring() {
        // Three little-endian samples: 1, -1, 258.
        let o = open(&req("printf '\\001\\000\\377\\377\\002\\001'")).unwrap();
        let mut pcm = Vec::new();
        for _ in 0..200 {
            o.audio.drain(&mut pcm);
            if pcm.len() >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        o.close();
        assert_eq!(pcm, vec![1, -1, 258]);
    }

    #[test]
    fn an_odd_byte_is_held_until_its_partner_arrives() {
        let o = open(&req("printf '\\001'; sleep 0.05; printf '\\000\\002\\000'")).unwrap();
        let mut pcm = Vec::new();
        for _ in 0..200 {
            o.audio.drain(&mut pcm);
            if pcm.len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        o.close();
        assert_eq!(pcm, vec![1, 2]);
    }

    /// Needs a real microphone, so it is not part of `just test`:
    /// `cargo test -p ah-voice -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn the_system_microphone_produces_samples() {
        let o = open(&Request {
            device: String::new(),
            command: String::new(),
            rate: 0,
            ring_ms: 2000,
        })
        .expect("open the default input");
        println!("{} at {} Hz, {} channels", o.source, o.rate, o.channels);
        let mut pcm = Vec::new();
        for _ in 0..100 {
            o.audio.drain(&mut pcm);
            if pcm.len() > o.rate as usize / 10 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        println!("{} samples, peak {peak}", pcm.len());
        o.close();
        assert!(!pcm.is_empty(), "no samples arrived from {}", o_source());
    }

    fn o_source() -> &'static str {
        "the default input"
    }

    /// A recorder that would run for ever must not outlive `close`. `close`
    /// joins the thread reading it, so if the recorder survived, this would
    /// never return rather than merely fail.
    #[test]
    fn closing_stops_a_recorder_that_would_never_stop_on_its_own() {
        let o = open(&req("while true; do printf '\\001\\000'; sleep 0.01; done")).unwrap();
        let mut pcm = Vec::new();
        for _ in 0..200 {
            o.audio.drain(&mut pcm);
            if !pcm.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!pcm.is_empty(), "recorder produced nothing");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            o.close();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "close did not come back: the recorder or its reader outlived it"
        );
    }

    #[test]
    fn a_recorder_that_exits_leaves_an_empty_ring_rather_than_hanging() {
        let o = open(&req("exec ah-no-such-recorder-9x")).expect("sh itself starts");
        std::thread::sleep(std::time::Duration::from_millis(60));
        let mut pcm = Vec::new();
        o.audio.drain(&mut pcm);
        o.close();
        assert!(pcm.is_empty());
    }
}
