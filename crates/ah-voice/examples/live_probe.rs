//! The dictation pipeline on its own, with the keyboard taken out of it.
//!
//! Microphone → downmix → 16 kHz → Deepgram's socket → words, exactly as
//! `/voice` does it. The difference is that listening is started and stopped
//! with Enter rather than a held key, so whatever the terminal does or does
//! not report about key releases cannot affect the result.
//!
//! If the words are good here and bad in `ah`, the fault is in the key
//! handling. If they are bad here too, it is in the audio or the socket, and
//! the numbers printed alongside say which.
//!
//!   cargo run -p ah-voice --example live_probe
//!
//! The key comes from DEEPGRAM_API_KEY or ~/.config/ah/credentials.toml. It
//! is never printed.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ah_voice::{capture, deepgram, resample, ring};

fn main() {
    let Some(key) = deepgram_key() else {
        eprintln!("no Deepgram key.");
        eprintln!("  export DEEPGRAM_API_KEY=…");
        eprintln!("  or put deepgram_api_key = \"…\" in ~/.config/ah/credentials.toml");
        std::process::exit(2);
    };
    let model = std::env::var("DG_MODEL").unwrap_or_else(|_| "nova-3".into());

    // ---- the microphone, opened the way `ah` opens it ----
    let opened = match capture::open(&capture::Request {
        device: std::env::var("DG_DEVICE").unwrap_or_default(),
        command: std::env::var("DG_CAPTURE_CMD").unwrap_or_default(),
        rate: 0,
        ring_ms: 2000,
    }) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("cannot record: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "recording with {} at {} Hz, {} channel(s) → 16000 Hz mono → Deepgram {model}",
        opened.source, opened.rate, opened.channels
    );

    // ---- the socket ----
    let (producer, consumer) = ring::ring(resample::TARGET_RATE as usize * 2);
    let said = Arc::new(Mutex::new(String::new()));
    let guess = Arc::new(Mutex::new(String::new()));
    let start = Instant::now();
    let (s, g) = (said.clone(), guess.clone());
    let live = match deepgram::Live::open(
        deepgram::Config {
            api_key: key,
            model: model.clone(),
            language: std::env::var("DG_LANGUAGE").unwrap_or_default(),
            keyterms: Vec::new(),
            sample_rate: resample::TARGET_RATE,
            endpointing_ms: 400,
            idle_secs: 0,
        },
        consumer,
        move |e| {
            let at = start.elapsed().as_millis();
            match e {
                deepgram::Event::Open => println!("\r{at:>6} ms  socket open"),
                deepgram::Event::Interim(t) => {
                    *g.lock().unwrap() = t.clone();
                    print!("\r\x1b[2K{at:>6} ms  …{t}");
                    let _ = std::io::stdout().flush();
                }
                deepgram::Event::Final(t) => {
                    g.lock().unwrap().clear();
                    let mut all = s.lock().unwrap();
                    if !all.is_empty() {
                        all.push(' ');
                    }
                    all.push_str(&t);
                    println!("\r\x1b[2K{at:>6} ms  FINAL  {t}");
                }
                deepgram::Event::UtteranceEnd => {
                    println!("\r\x1b[2K{at:>6} ms  (utterance end)")
                }
                deepgram::Event::Trouble(m) => println!("\r\x1b[2K{at:>6} ms  trouble: {m}"),
            }
        },
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot reach Deepgram: {e}");
            std::process::exit(2);
        }
    };

    // ---- the pump: exactly what the dictation worker does ----
    let sent = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let listening = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (sent, peak, listening, stop) =
            (sent.clone(), peak.clone(), listening.clone(), stop.clone());
        let device_rate = opened.rate;
        let channels = opened.channels;
        std::thread::spawn(move || {
            let mut resampler = resample::Resampler::new(device_rate);
            let (mut raw, mut mono, mut pcm) = (Vec::new(), Vec::new(), Vec::new());
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                if !listening.load(Ordering::Acquire) {
                    opened.audio.keep_last(0);
                    continue;
                }
                raw.clear();
                opened.audio.drain(&mut raw);
                if raw.is_empty() {
                    continue;
                }
                mono.clear();
                resample::downmix(&raw, channels, &mut mono);
                pcm.clear();
                resampler.process(&mono, &mut pcm);
                if let Some(m) = pcm.iter().map(|s| s.unsigned_abs()).max() {
                    peak.fetch_max(m as u64, Ordering::Relaxed);
                }
                sent.fetch_add(pcm.len() as u64, Ordering::Relaxed);
                producer.write(&pcm);
            }
            opened.close();
        });
    }

    println!("\nEnter starts listening, Enter again stops it. `q` then Enter quits.\n");
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim() == "q" {
            break;
        }
        let on = !listening.load(Ordering::Acquire);
        if on {
            sent.store(0, Ordering::Relaxed);
            peak.store(0, Ordering::Relaxed);
            said.lock().unwrap().clear();
            guess.lock().unwrap().clear();
            listening.store(true, Ordering::Release);
            live.listen(true);
            println!("listening — speak, then press Enter");
        } else {
            listening.store(false, Ordering::Release);
            live.listen(false);
            let n = sent.load(Ordering::Relaxed);
            let p = peak.load(Ordering::Relaxed);
            println!(
                "stopped — {:.2} s of audio sent, loudest sample {p} of 32768 ({:.0}%)",
                n as f64 / resample::TARGET_RATE as f64,
                p as f64 / 32768.0 * 100.0
            );
            if p < 1000 {
                println!("  that is very quiet: the microphone may be muted or the wrong one");
            }
            // Give the last words a moment to arrive.
            std::thread::sleep(Duration::from_millis(1500));
            let full = said.lock().unwrap().clone();
            let tail = guess.lock().unwrap().clone();
            println!(
                "\n  heard: {full}{}{tail}\n",
                if tail.is_empty() { "" } else { " " }
            );
            println!("Enter to go again, `q` to quit.");
        }
    }
    stop.store(true, Ordering::Relaxed);
    drop(live);
}

/// Never printed, never logged, never put on a command line.
fn deepgram_key() -> Option<String> {
    if let Ok(k) = std::env::var("DEEPGRAM_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Some(k);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let text = std::fs::read_to_string(format!("{home}/.config/ah/credentials.toml")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("deepgram_api_key") {
            let v = rest.trim_start_matches([' ', '=']).trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}
