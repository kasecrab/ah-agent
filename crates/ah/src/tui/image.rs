//! Drawing pictures in the transcript: which protocol the terminal speaks,
//! how much room a picture takes, and the escape sequences themselves.
//!
//! Everything here is arithmetic and string building. The writing to the
//! terminal, and the state that decides what needs writing, live in
//! [`super::App`].

use std::path::PathBuf;

use ah_abi::ImageInline;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

/// The graphics protocol this terminal speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Proto {
    /// No pictures: a one-line chip instead.
    #[default]
    None,
    /// kitty, Ghostty, WezTerm, Warp.
    Kitty,
    /// iTerm2's inline images.
    Iterm2,
}

/// Cell size assumed when the terminal will not say: a 10pt monospace font,
/// which is about one to two.
pub const CELL_FALLBACK: (u16, u16) = (9, 18);

/// Most base64 kitty takes in one escape.
const CHUNK: usize = 4096;

/// One picture in the transcript. The bytes are not here: they are read from
/// the file when the terminal actually needs them, so resuming a session with
/// fifty pictures in it does not encode fifty files.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageData {
    /// Unique for the life of the process: the kitty image id, and the id the
    /// `/images` list and the wrapped-line cache use.
    pub id: u32,
    pub path: PathBuf,
    pub mime: String,
    /// Pixel size, `(0, 0)` when it could not be read — which forces the chip,
    /// since there is no aspect ratio to lay the picture out with.
    pub px: (u32, u32),
    pub bytes: usize,
}

/// Where a picture sits inside its block's lines, or `None` when the block is
/// a one-line chip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImageLayout {
    pub id: u32,
    /// Index of the first picture row inside the block's lines.
    pub first: usize,
    pub cols: u16,
    pub rows: u16,
    pub px: (u32, u32),
}

/// A picture on screen this frame, in absolute terminal cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub id: u32,
    /// Top-left of the visible part.
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    /// Rows the whole picture would take.
    pub rows: u16,
    /// Rows cut off above the top of the viewport.
    pub hidden_top: u16,
    /// Rows actually on screen.
    pub visible: u16,
    pub px: (u32, u32),
}

impl Placement {
    /// True when none of the picture is cut off.
    pub fn whole(&self) -> bool {
        self.hidden_top == 0 && self.visible == self.rows
    }

    /// Cells the picture covers on screen.
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.cols,
            height: self.visible,
        }
    }
}

/// What this terminal draws. `env` is injected so a test does not have to
/// touch the process environment, which every other test shares.
pub fn detect(forced: ImageInline, env: &dyn Fn(&str) -> Option<String>) -> Proto {
    match forced {
        ImageInline::Off => return Proto::None,
        ImageInline::Kitty => return Proto::Kitty,
        ImageInline::Iterm2 => return Proto::Iterm2,
        ImageInline::Auto => {}
    }
    let get = |k: &str| env(k).unwrap_or_default().to_ascii_lowercase();
    let term = get("TERM");
    // A multiplexer eats APC and OSC payloads unless it has been configured to
    // pass them through, and half a picture is worse than none.
    if env("TMUX").is_some() || term.starts_with("tmux") || term.starts_with("screen") {
        return Proto::None;
    }
    let prog = get("TERM_PROGRAM");
    if env("KITTY_WINDOW_ID").is_some()
        || env("GHOSTTY_RESOURCES_DIR").is_some()
        || env("WEZTERM_PANE").is_some()
        || env("WARP_SESSION_ID").is_some()
        || matches!(
            prog.as_str(),
            "kitty" | "ghostty" | "wezterm" | "warpterminal"
        )
        || term.contains("kitty")
        || term.contains("ghostty")
    {
        return Proto::Kitty;
    }
    if env("ITERM_SESSION_ID").is_some() || prog == "iterm.app" {
        return Proto::Iterm2;
    }
    Proto::None
}

/// Terminal cell size in pixels. `custom` (`"9x18"`) wins; otherwise the
/// terminal is asked, and `(0, 0)` means it would not say.
pub fn query_cell_px(custom: &str) -> (u16, u16) {
    if let Some((w, h)) = custom.split_once(['x', 'X'])
        && let (Ok(w), Ok(h)) = (w.trim().parse::<u16>(), h.trim().parse::<u16>())
        && w > 0
        && h > 0
    {
        return (w, h);
    }
    match crossterm::terminal::window_size() {
        Ok(s) if s.width > 0 && s.height > 0 && s.columns > 0 && s.rows > 0 => {
            (s.width / s.columns, s.height / s.rows)
        }
        // Windows consoles, and any terminal that does not answer.
        _ => (0, 0),
    }
}

/// Cells a picture of `px` takes at `cell` pixels per cell, capped by
/// `max_cols` and `max_rows` with the aspect ratio kept. `None` when the
/// pixel size is unknown.
pub fn cells(px: (u32, u32), cell: (u16, u16), max_cols: u16, max_rows: u16) -> Option<(u16, u16)> {
    if px.0 == 0 || px.1 == 0 {
        return None;
    }
    let (w, h) = (px.0 as u64, px.1 as u64);
    let (cw, ch) = (cell.0.max(1) as u64, cell.1.max(1) as u64);
    let max_cols = max_cols.max(1) as u64;
    let max_rows = max_rows.max(1) as u64;
    let mut cols = max_cols.min(w.div_ceil(cw)).max(1);
    let mut rows = (h * cols * cw).div_ceil(w * ch).max(1);
    if rows > max_rows {
        // Too tall for the room: shrink by height, so the picture stays its
        // own shape instead of being squashed into the box.
        rows = max_rows;
        cols = ((w * rows * ch) / (h * cw)).clamp(1, max_cols);
    }
    Some((cols as u16, rows as u16))
}

/// kitty: send the pixels, without placing them. Chunked, because kitty takes
/// at most 4 KB of payload per escape.
pub fn kitty_transmit(id: u32, b64: &str) -> String {
    // `q=2` on every command: without it kitty answers with a report that
    // would arrive in the key stream as garbage.
    let head = format!("a=t,f=100,q=2,i={id}");
    if b64.len() <= CHUNK {
        return format!("\x1b_G{head};{b64}\x1b\\");
    }
    let mut out = String::with_capacity(b64.len() + b64.len() / CHUNK * 24 + 64);
    let mut offset = 0;
    let mut first = true;
    while offset < b64.len() {
        let end = (offset + CHUNK).min(b64.len());
        let last = end == b64.len();
        let chunk = &b64[offset..end];
        if first {
            out.push_str(&format!("\x1b_G{head},m=1;{chunk}\x1b\\"));
            first = false;
        } else if last {
            out.push_str(&format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
        offset = end;
    }
    out
}

/// kitty: put image `id` at the cursor, cropped to the rows on screen.
pub fn kitty_place(p: &Placement) -> String {
    let mut s = format!(
        "\x1b_Ga=p,i={},p=1,q=2,C=1,c={},r={}",
        p.id, p.cols, p.visible
    );
    if !p.whole() {
        // Source rectangle in pixels; kitty scales it into the c×r cells.
        let h = p.px.1 as u64;
        let rows = p.rows.max(1) as u64;
        let y = h * p.hidden_top as u64 / rows;
        let end = (h * (p.hidden_top + p.visible) as u64).div_ceil(rows);
        s.push_str(&format!(",y={y},h={}", end.saturating_sub(y).max(1)));
    }
    s.push_str("\x1b\\");
    s
}

/// kitty: drop the placements of `id`, keeping the pixels.
pub fn kitty_unplace(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\")
}

/// kitty: drop `id` entirely, freeing the terminal's copy.
pub fn kitty_delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// kitty: drop everything, pixels included. For the panic hook, which has no
/// list of what was on screen.
pub fn kitty_delete_all() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

/// iTerm2: the whole picture, at the cursor. There are no ids and no crop, so
/// this is the payload every time it moves.
pub fn iterm2(b64: &str, bytes: usize, cols: u16, rows: u16) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={bytes};width={cols};height={rows};preserveAspectRatio=1:{b64}\x07"
    )
}

/// The line shown for a picture the terminal cannot draw, and under one it
/// can. `key` is the binding that opens it, empty for none.
pub fn chip(d: &ImageData, key: &str) -> String {
    let size = if d.bytes >= 1024 * 1024 {
        format!("{:.1} MB", d.bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", d.bytes.div_ceil(1024))
    };
    let kind = d.mime.strip_prefix("image/").unwrap_or(&d.mime);
    let mut s = if d.px.0 > 0 {
        format!("▣ {}×{} {kind} · {size}", d.px.0, d.px.1)
    } else {
        format!("▣ {kind} · {size}")
    };
    if !key.is_empty() {
        s.push_str(&format!(" · {key} opens it"));
    }
    s
}

/// Program and arguments that open `path`. `custom` wins, with `{path}`
/// substituted or the path appended.
pub fn opener(os: &str, custom: &str, path: &str) -> Option<(String, Vec<String>)> {
    let custom = custom.trim();
    if !custom.is_empty() {
        let mut parts = custom.split_whitespace().map(String::from);
        let prog = parts.next()?;
        let mut args: Vec<String> = parts.collect();
        if args.iter().any(|a| a.contains("{path}")) {
            for a in &mut args {
                *a = a.replace("{path}", path);
            }
        } else {
            args.push(path.to_string());
        }
        return Some((prog, args));
    }
    match os {
        "linux" | "freebsd" | "openbsd" | "netbsd" => {
            Some(("xdg-open".into(), vec![path.to_string()]))
        }
        "macos" => Some(("open".into(), vec![path.to_string()])),
        "windows" => Some((
            "cmd".into(),
            vec!["/C".into(), "start".into(), String::new(), path.to_string()],
        )),
        _ => None,
    }
}

/// Screen placements for the pictures in a transcript. `entries` is one
/// `(line count, layout)` per block in order, `scroll` the first visible line.
pub fn placements(
    entries: &[(usize, Option<ImageLayout>)],
    scroll: usize,
    viewport: usize,
    area: Rect,
) -> Vec<Placement> {
    let mut out = Vec::new();
    let end = scroll + viewport;
    let mut line = 0usize;
    for (len, layout) in entries {
        if let Some(l) = layout {
            let top = line + l.first;
            let bottom = top + l.rows as usize;
            if top < end && bottom > scroll {
                let hidden_top = scroll.saturating_sub(top);
                let visible = bottom.min(end) - top.max(scroll);
                out.push(Placement {
                    id: l.id,
                    x: area.x,
                    y: area.y + (top.max(scroll) - scroll) as u16,
                    cols: l.cols,
                    rows: l.rows,
                    hidden_top: hidden_top as u16,
                    visible: visible as u16,
                    px: l.px,
                });
            }
        }
        line += len;
    }
    out
}

/// Tell ratatui's diff what to do with the cells the pictures cover.
///
/// A cell under a picture that has not moved is skipped: writing there would
/// punch a hole in it. A cell a picture just entered or just left is written
/// whatever the diff thinks — without that, two identical blank buffers agree
/// nothing changed and the terminal keeps showing a picture that has gone.
pub fn mark(buf: &mut Buffer, cur: &[Placement], prev: &[Placement]) {
    for p in prev {
        if !cur.contains(p) {
            set_opt(buf, p.rect(), CellDiffOption::AlwaysUpdate);
        }
    }
    for p in cur {
        let opt = if prev.contains(p) {
            CellDiffOption::Skip
        } else {
            CellDiffOption::AlwaysUpdate
        };
        set_opt(buf, p.rect(), opt);
    }
}

fn set_opt(buf: &mut Buffer, r: Rect, opt: CellDiffOption) {
    let area = buf.area;
    for y in r.y..r.y.saturating_add(r.height).min(area.bottom()) {
        for x in r.x..r.x.saturating_add(r.width).min(area.right()) {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_diff_option(opt);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| owned.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    }

    fn data(px: (u32, u32), bytes: usize) -> ImageData {
        ImageData {
            id: 7,
            path: PathBuf::from("/tmp/a.png"),
            mime: "image/png".into(),
            px,
            bytes,
        }
    }

    fn layout(id: u32, rows: u16) -> ImageLayout {
        ImageLayout {
            id,
            first: 0,
            cols: 20,
            rows,
            px: (400, 400),
        }
    }

    #[test]
    fn the_terminal_is_read_off_the_environment() {
        use Proto::*;
        let cases: [(&[(&str, &str)], Proto); 8] = [
            (&[("KITTY_WINDOW_ID", "1")], Kitty),
            (&[("TERM_PROGRAM", "ghostty")], Kitty),
            (&[("WEZTERM_PANE", "0")], Kitty),
            (&[("TERM_PROGRAM", "WarpTerminal")], Kitty),
            (&[("ITERM_SESSION_ID", "w0")], Iterm2),
            (&[("TERM", "xterm-256color")], None),
            // A multiplexer swallows the payload, whatever is outside it.
            (&[("TMUX", "/tmp/s"), ("KITTY_WINDOW_ID", "1")], None),
            (&[("TERM", "screen-256color")], None),
        ];
        for (env, want) in cases {
            assert_eq!(detect(ImageInline::Auto, &env_of(env)), want, "{env:?}");
        }
        // The setting wins in both directions.
        assert_eq!(
            detect(ImageInline::Off, &env_of(&[("KITTY_WINDOW_ID", "1")])),
            None
        );
        assert_eq!(
            detect(ImageInline::Kitty, &env_of(&[("TMUX", "/tmp/s")])),
            Kitty
        );
    }

    #[test]
    fn a_picture_is_fitted_to_the_room_it_has() {
        // 1000x500 at 10x20 pixel cells: 100 cells wide, capped at 50, and
        // half as many rows again for the aspect.
        assert_eq!(cells((1000, 500), (10, 20), 50, 40), Some((50, 13)));
        // Too tall for the room: columns shrink rather than the picture
        // being squashed.
        let (c, r) = cells((1000, 500), (10, 20), 50, 5).unwrap();
        assert_eq!(r, 5);
        assert!(c < 50);
        assert_eq!(cells((0, 0), (10, 20), 50, 40), None);
        assert_eq!(cells((1, 1), (10, 20), 50, 40), Some((1, 1)));
    }

    #[test]
    fn kitty_sends_the_payload_in_chunks_and_places_it_by_id() {
        let small = "A".repeat(100);
        let one = kitty_transmit(3, &small);
        assert!(one.starts_with("\x1b_Ga=t,f=100,q=2,i=3;"));
        assert!(!one.contains("m=1"));

        let big = "B".repeat(10_000);
        let many = kitty_transmit(3, &big);
        assert_eq!(many.matches("\x1b_G").count(), 3);
        assert!(many.contains(",m=1;"));
        assert!(many.contains("\x1b_Gm=0;"));
        // Every byte of the payload is sent exactly once.
        let sent: String = many
            .split("\x1b_G")
            .filter_map(|part| part.split_once(';'))
            .map(|(_, rest)| rest.trim_end_matches("\x1b\\").to_string())
            .collect();
        assert_eq!(sent, big);
    }

    #[test]
    fn a_cropped_placement_names_the_pixels_that_are_visible() {
        let whole = Placement {
            id: 1,
            x: 0,
            y: 0,
            cols: 10,
            rows: 20,
            hidden_top: 0,
            visible: 20,
            px: (400, 400),
        };
        let s = kitty_place(&whole);
        assert!(s.contains("a=p,i=1"), "{s}");
        assert!(s.contains("r=20") && !s.contains("y="), "{s}");

        let cut = Placement {
            hidden_top: 5,
            visible: 15,
            ..whole
        };
        let s = kitty_place(&cut);
        // Five rows of twenty are above the viewport: a quarter of 400px.
        assert!(s.contains("y=100"), "{s}");
        assert!(s.contains("h=300"), "{s}");
        assert!(s.contains("r=15"), "{s}");
    }

    #[test]
    fn iterm2_carries_its_own_size() {
        let s = iterm2("QUJD", 3, 12, 6);
        assert!(s.starts_with("\x1b]1337;File="));
        assert!(s.ends_with('\x07'));
        for want in [
            "inline=1",
            "size=3",
            "width=12",
            "height=6",
            "preserveAspectRatio=1",
        ] {
            assert!(s.contains(want), "missing {want}: {s}");
        }
    }

    #[test]
    fn the_chip_says_what_the_picture_is() {
        let s = chip(&data((1024, 768), 640 * 1024), "Ctrl-O");
        assert!(s.contains("1024×768"), "{s}");
        assert!(s.contains("png"), "{s}");
        assert!(s.contains("640 KB"), "{s}");
        assert!(s.contains("Ctrl-O opens it"), "{s}");
        // No key offered, and a picture of unknown size.
        let s = chip(&data((0, 0), 2 * 1024 * 1024), "");
        assert!(!s.contains("opens it"), "{s}");
        assert!(s.contains("2.0 MB"), "{s}");
    }

    #[test]
    fn opening_falls_back_to_what_the_desktop_uses() {
        assert_eq!(
            opener("linux", "", "/a b.png"),
            Some(("xdg-open".into(), vec!["/a b.png".into()]))
        );
        assert_eq!(
            opener("macos", "", "/a.png"),
            Some(("open".into(), vec!["/a.png".into()]))
        );
        assert_eq!(
            opener("windows", "", "/a.png").unwrap().0,
            "cmd".to_string()
        );
        assert_eq!(opener("haiku", "", "/a.png"), None);
        // A command of one's own, with and without a placeholder.
        assert_eq!(
            opener("linux", "feh {path} --scale", "/a.png"),
            Some(("feh".into(), vec!["/a.png".into(), "--scale".into()]))
        );
        assert_eq!(
            opener("linux", "feh", "/a.png"),
            Some(("feh".into(), vec!["/a.png".into()]))
        );
    }

    #[test]
    fn placements_follow_the_scroll() {
        let area = Rect::new(0, 2, 80, 10);
        // Ten lines of text, then a six-row picture, then more text.
        let entries = vec![(10, None), (7, Some(layout(1, 6))), (5, None)];

        let p = placements(&entries, 0, 30, area);
        assert_eq!(p.len(), 1);
        assert!(p[0].whole());
        assert_eq!(p[0].y, area.y + 10);

        // Scrolled so two rows of the picture are above the viewport.
        let p = placements(&entries, 12, 30, area);
        assert_eq!((p[0].hidden_top, p[0].visible, p[0].y), (2, 4, area.y));
        assert!(!p[0].whole());

        // The viewport ends inside the picture.
        let p = placements(&entries, 8, 6, area);
        assert_eq!((p[0].hidden_top, p[0].visible), (0, 4));

        // Scrolled past it entirely.
        assert!(placements(&entries, 20, 5, area).is_empty());

        // A block with no picture still moves the line counter along.
        let two = vec![(10, None), (7, Some(layout(1, 6))), (7, Some(layout(2, 6)))];
        let p = placements(&two, 0, 40, area);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].y, area.y + 17);
    }

    #[test]
    fn cells_under_a_picture_are_skipped_and_cells_it_left_are_repainted() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        let a = Placement {
            id: 1,
            x: 0,
            y: 0,
            cols: 4,
            rows: 2,
            hidden_top: 0,
            visible: 2,
            px: (40, 40),
        };
        let moved = Placement { y: 5, ..a };
        // Same place as last frame: leave the pixels alone.
        mark(&mut buf, &[a], &[a]);
        assert_eq!(buf[(0, 0)].diff_option, CellDiffOption::Skip);
        // Moved: both the old rows and the new ones must reach the terminal.
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        mark(&mut buf, &[moved], &[a]);
        assert_eq!(buf[(0, 0)].diff_option, CellDiffOption::AlwaysUpdate);
        assert_eq!(buf[(0, 5)].diff_option, CellDiffOption::AlwaysUpdate);
        // Untouched cells keep the default.
        assert_eq!(buf[(10, 9)].diff_option, CellDiffOption::None);
    }
}
