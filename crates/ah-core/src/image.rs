//! Images the model generated: taking apart what arrived on the wire,
//! measuring it, and putting it on disk. A conversation only ever holds a
//! reference to the file, never the bytes — a base64 image inlined into a
//! session line would make every later `/resume` scan slower forever.

use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Scheme for an image inside the data directory. The rest of the entry is a
/// relative path, so a session copied to another machine, or an `AH_DATA_DIR`
/// that moved, still resolves.
pub const SCHEME: &str = "ah-image:";

/// Largest image ah decodes and keeps. Big enough for anything a model draws,
/// small enough that a hostile payload cannot ask for a gigabyte.
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Roughly what a provider charges for one image in a prompt. They bill a
/// flat-ish number of tokens whatever the file weighs, so counting the base64
/// instead would put one photograph at a quarter of a million tokens and
/// compact the conversation after every picture.
pub const IMAGE_TOKENS: usize = 1_100;

/// A `data:` URL taken apart.
#[derive(Debug, Clone, PartialEq)]
pub struct DataUrl {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// One generated image, after it was written.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub path: PathBuf,
    /// What goes in `Message.images`: `ah-image:<dir>/<file>` under the data
    /// directory, a `file://` URL anywhere else.
    pub reference: String,
    pub mime: String,
    /// `0` when the format is one ah cannot measure.
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

/// True for an entry a user attached, which is carried verbatim.
pub fn is_data_url(s: &str) -> bool {
    s.starts_with("data:")
}

/// `data:<mime>;base64,<payload>`. `None` for anything that is not one, is
/// not base64, or decodes to nothing.
pub fn parse_data_url(url: &str) -> Option<DataUrl> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime = parts.next()?.trim();
    if !parts.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    if !mime.contains('/') {
        return None;
    }
    let bytes = decode_base64(payload)?;
    if bytes.is_empty() {
        return None;
    }
    Some(DataUrl {
        mime: mime.to_ascii_lowercase(),
        bytes,
    })
}

/// Base64 in either alphabet, whitespace ignored, padding optional. `None`
/// for anything malformed: a stream that was cut in half must not turn into a
/// half-written file.
pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut n, mut pad) = (0u32, 0u32, 0u32);
    for &c in s.as_bytes() {
        match c {
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            b'=' => {
                pad += 1;
                if pad > 2 {
                    return None;
                }
                continue;
            }
            _ => {}
        }
        if pad > 0 {
            return None; // data after the padding
        }
        acc = (acc << 6) | sextet(c)?;
        n += 1;
        if n == 4 {
            out.push((acc >> 16) as u8);
            out.push((acc >> 8) as u8);
            out.push(acc as u8);
            acc = 0;
            n = 0;
        }
    }
    if pad != 0 && pad != (4 - n) % 4 {
        return None;
    }
    match n {
        0 => {}
        2 => out.push((acc >> 4) as u8),
        3 => {
            out.push((acc >> 10) as u8);
            out.push((acc >> 2) as u8);
        }
        // A trailing group of one carries six bits, which is not a byte.
        _ => return None,
    }
    Some(out)
}

/// `(width, height)` from a PNG's IHDR. `None` for every other format: an
/// image ah cannot measure is still saved and still offered, the renderer
/// just has no aspect ratio to lay it out with.
pub fn size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !crate::clipboard::looks_like_png(bytes) || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let be = |o: usize| u32::from_be_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let (w, h) = (be(16), be(20));
    (w > 0 && h > 0).then_some((w, h))
}

/// File extension for a mime type; `bin` for one ah does not know.
pub fn extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// `<millis>-<seq>.<ext>`: sorts by arrival, unique within a millisecond, and
/// claims nothing about content that a model wrote.
pub fn file_name(ms: u128, seq: u32, mime: &str) -> String {
    format!("{ms}-{seq}.{}", extension(mime))
}

/// Decode `url` and write it into `dir`, which is created if missing. `seq`
/// separates images that arrived in the same millisecond.
pub fn save(dir: &Path, url: &str, seq: u32, max_bytes: u64) -> Result<Saved> {
    // Refuse before allocating: the decoder reserves three bytes per four of
    // input, so an enormous payload would be an enormous allocation first and
    // an error afterwards.
    let cap = max_bytes
        .saturating_div(3)
        .saturating_mul(4)
        .saturating_add(128);
    if url.len() as u64 > cap {
        return Err(Error::Config(format!(
            "generated image is larger than {max_bytes} bytes"
        )));
    }
    let d = parse_data_url(url).ok_or_else(|| Error::Config("not an image data url".into()))?;
    if d.bytes.len() as u64 > max_bytes {
        return Err(Error::Config(format!(
            "generated image is larger than {max_bytes} bytes"
        )));
    }
    std::fs::create_dir_all(dir)?;
    let ms = now_ms();
    let mut seq = seq;
    let path = loop {
        let p = dir.join(file_name(ms, seq, &d.mime));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&p)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(&d.bytes)?;
                break p;
            }
            // Another image landed on this name: a resumed session writing in
            // the same millisecond, or two images in one reply.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => seq += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let (width, height) = size(&d.bytes).unwrap_or((0, 0));
    Ok(Saved {
        reference: reference(&path),
        path,
        mime: d.mime,
        width,
        height,
        bytes: d.bytes.len(),
    })
}

/// The string that stands for `path` in a stored message.
pub fn reference(path: &Path) -> String {
    reference_in(&crate::paths::images_dir(), path)
}

/// The file a stored `images` entry names. `None` for a data URL, and for an
/// `ah-image:` entry that tries to point outside the store — these strings
/// come out of a session file, which is not a trusted input.
pub fn resolve(entry: &str) -> Option<PathBuf> {
    resolve_in(&crate::paths::images_dir(), entry)
}

/// What an image that has aged out of the prompt leaves behind. Derived only
/// from the reference, so that once an image has been dropped every later
/// request sends the same bytes there and the provider's prefix cache still
/// matches.
pub fn omitted_note(reference: &str) -> String {
    format!("\n\n[generated image saved as {reference}; not resent]")
}

fn reference_in(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => {
            let rel: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            format!("{SCHEME}{}", rel.join("/"))
        }
        Err(_) => format!("file://{}", path.display()),
    }
}

fn resolve_in(root: &Path, entry: &str) -> Option<PathBuf> {
    if is_data_url(entry) {
        return None;
    }
    if let Some(rel) = entry.strip_prefix(SCHEME) {
        let rel = Path::new(rel);
        if rel.as_os_str().is_empty()
            || !rel.components().all(|c| matches!(c, Component::Normal(_)))
        {
            return None;
        }
        return Some(root.join(rel));
    }
    if let Some(p) = entry.strip_prefix("file://") {
        return (!p.is_empty()).then(|| PathBuf::from(p));
    }
    let p = Path::new(entry);
    p.is_absolute().then(|| p.to_path_buf())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::base64;

    /// The smallest real PNG: 1x1, one opaque pixel.
    fn png_1x1() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0, 0x90, 0x77, 0x53, 0xDE]);
        v
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ah-image-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn base64_round_trips_for_every_padding() {
        for n in 0..9usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 5) as u8).collect();
            let encoded = base64(&data);
            assert_eq!(decode_base64(&encoded), Some(data), "length {n}");
        }
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        // Line breaks are what a wrapped payload looks like.
        assert_eq!(decode_base64("aG\nk="), Some(b"hi".to_vec()));
        // Padding is optional.
        assert_eq!(decode_base64("aGk"), Some(b"hi".to_vec()));
        // URL-safe alphabet.
        assert_eq!(decode_base64("-_8="), decode_base64("+/8="));
    }

    #[test]
    fn base64_rejects_what_is_not_base64() {
        assert_eq!(decode_base64("a!k="), None);
        assert_eq!(decode_base64("aGk=x"), None);
        assert_eq!(decode_base64("aGk==="), None);
        assert_eq!(decode_base64("A"), None);
        assert_eq!(decode_base64("aGkx="), None);
    }

    #[test]
    fn a_data_url_comes_apart_into_a_mime_type_and_bytes() {
        let d = parse_data_url("data:image/png;base64,aGk=").unwrap();
        assert_eq!(d.mime, "image/png");
        assert_eq!(d.bytes, b"hi");
        assert_eq!(
            parse_data_url("data:image/PNG;charset=utf-8;base64,aGk=")
                .unwrap()
                .mime,
            "image/png"
        );
        // Not base64, empty, not a data url, no mime type.
        assert_eq!(parse_data_url("data:image/png,hi"), None);
        assert_eq!(parse_data_url("data:image/png;base64,"), None);
        assert_eq!(parse_data_url("https://example.com/a.png"), None);
        assert_eq!(parse_data_url("data:;base64,aGk="), None);
    }

    #[test]
    fn png_dimensions_come_from_the_header() {
        assert_eq!(size(&png_1x1()), Some((1, 1)));
        assert_eq!(size(&png_1x1()[..20]), None);
        assert_eq!(size(&[0xFF, 0xD8, 0xFF]), None);
        let mut zero = png_1x1();
        zero[16..20].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(size(&zero), None);
    }

    #[test]
    fn a_mime_type_picks_the_extension_and_the_file_name() {
        assert_eq!(extension("image/png"), "png");
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/webp"), "webp");
        assert_eq!(extension("image/gif"), "gif");
        assert_eq!(extension("application/pdf"), "bin");
        assert_eq!(
            file_name(1757100000000, 0, "image/png"),
            "1757100000000-0.png"
        );
    }

    #[test]
    fn a_reference_round_trips_and_cannot_escape_the_store() {
        let root = PathBuf::from("/data/images");
        let path = root.join("abc123").join("1757100000000-0.png");
        let r = reference_in(&root, &path);
        assert_eq!(r, "ah-image:abc123/1757100000000-0.png");
        assert_eq!(resolve_in(&root, &r), Some(path));

        assert_eq!(resolve_in(&root, "ah-image:../../etc/passwd"), None);
        assert_eq!(resolve_in(&root, "ah-image:a/../../x"), None);
        assert_eq!(resolve_in(&root, "ah-image:/etc/passwd"), None);
        assert_eq!(resolve_in(&root, "ah-image:"), None);
        assert_eq!(resolve_in(&root, "data:image/png;base64,aGk="), None);
        assert_eq!(resolve_in(&root, "relative/path.png"), None);
        assert_eq!(
            resolve_in(&root, "file:///tmp/a.png"),
            Some(PathBuf::from("/tmp/a.png"))
        );
        assert_eq!(
            resolve_in(&root, "/tmp/a.png"),
            Some(PathBuf::from("/tmp/a.png"))
        );
        // Outside the store the reference is absolute.
        assert_eq!(
            reference_in(&root, Path::new("/elsewhere/a.png")),
            "file:///elsewhere/a.png"
        );
    }

    #[test]
    fn saving_writes_the_file_and_measures_it() {
        let dir = temp_dir("save");
        let url = format!("data:image/png;base64,{}", base64(&png_1x1()));
        let a = save(&dir, &url, 0, MAX_IMAGE_BYTES).unwrap();
        assert_eq!((a.width, a.height), (1, 1));
        assert_eq!(a.mime, "image/png");
        assert_eq!(a.bytes, png_1x1().len());
        assert_eq!(std::fs::read(&a.path).unwrap(), png_1x1());

        // A second image in the same millisecond takes the next sequence
        // number instead of overwriting the first.
        let b = save(&dir, &url, 0, MAX_IMAGE_BYTES).unwrap();
        assert_ne!(a.path, b.path);
        assert!(a.path.is_file() && b.path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_or_broken_image_writes_nothing() {
        let dir = temp_dir("reject");
        let url = format!("data:image/png;base64,{}", base64(&png_1x1()));
        assert!(save(&dir, &url, 0, 8).is_err());
        assert!(save(&dir, "not a data url", 0, MAX_IMAGE_BYTES).is_err());
        assert!(!dir.join("").exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_note_left_by_a_dropped_image_depends_only_on_the_reference() {
        let a = omitted_note("ah-image:s/1-0.png");
        assert_eq!(a, omitted_note("ah-image:s/1-0.png"));
        assert!(a.contains("ah-image:s/1-0.png"));
        assert_ne!(a, omitted_note("ah-image:s/2-0.png"));
    }
}
