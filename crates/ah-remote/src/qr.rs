//! A QR code, so pairing is a thing you point a camera at.
//!
//! Byte mode, error correction level M, versions 1 to 10 — which is 213 bytes,
//! comfortably more than a relay URL and a pairing code. Nothing else is
//! implemented, because nothing else is needed and every extra mode is more
//! surface to be quietly wrong in.
//!
//! Hand-written rather than pulled in: the harness takes no dependency it does
//! not already compile, and the hard half of QR is decoding, which the phone's
//! own camera app does for us.
//!
//! Every symbol this builds was compared module for module against
//! `node-qrcode`, across all ten versions and all eight masks, and they agree
//! exactly; each was then handed to `jsQR`, an unrelated decoder, and all 168
//! read back as what went in. `mod cross_check` regenerates the dumps both
//! comparisons used. The
//! one deliberate difference is the fourth penalty rule, where that library
//! rounds the dark-module percentage up and the spec asks for the nearer of
//! the two multiples of five around it. Ours follows the spec, so on three of
//! twenty-one sample payloads it settles on a different mask. Either is a
//! valid symbol — the mask is chosen for how well it reads, not for whether
//! it decodes.

/// Smallest and largest version this builds. Ten is 213 bytes at level M.
const MAX_VERSION: usize = 10;

/// Error correction level M, as the two bits that go in the format string.
const EC_LEVEL_BITS: u32 = 0b00;

/// How one version's codewords are laid out at level M.
struct Layout {
    /// Error-correction codewords per block.
    ec: usize,
    /// Blocks in the first group, and data codewords in each of them.
    g1: (usize, usize),
    /// The same for the second group, all zeroes where there is only one.
    g2: (usize, usize),
}

/// Every version this builds, from the spec's table.
const BLOCKS: [Layout; MAX_VERSION] = [
    Layout {
        ec: 10,
        g1: (1, 16),
        g2: (0, 0),
    },
    Layout {
        ec: 16,
        g1: (1, 28),
        g2: (0, 0),
    },
    Layout {
        ec: 26,
        g1: (1, 44),
        g2: (0, 0),
    },
    Layout {
        ec: 18,
        g1: (2, 32),
        g2: (0, 0),
    },
    Layout {
        ec: 24,
        g1: (2, 43),
        g2: (0, 0),
    },
    Layout {
        ec: 16,
        g1: (4, 27),
        g2: (0, 0),
    },
    Layout {
        ec: 18,
        g1: (4, 31),
        g2: (0, 0),
    },
    Layout {
        ec: 22,
        g1: (2, 38),
        g2: (2, 39),
    },
    Layout {
        ec: 22,
        g1: (3, 36),
        g2: (2, 37),
    },
    Layout {
        ec: 26,
        g1: (4, 43),
        g2: (1, 44),
    },
];

/// Where the alignment patterns sit, by version. Every pair of these centres
/// is a pattern, except the three that would land on a finder.
const ALIGNMENT: [&[usize]; MAX_VERSION] = [
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
];

/// Bits of padding after the last codeword, by version. Versions 7 to 13 need
/// none; 2 to 6 need seven.
const REMAINDER_BITS: [usize; MAX_VERSION] = [0, 7, 7, 7, 7, 7, 0, 0, 0, 0];

/// A finished symbol: `size` by `size` modules, dark or light.
pub struct Qr {
    size: usize,
    modules: Vec<bool>,
}

impl Qr {
    /// Encode `data`, choosing the smallest version that holds it.
    /// `None` when it does not fit in a version 10 symbol.
    pub fn encode(data: &[u8]) -> Option<Self> {
        let version = (1..=MAX_VERSION).find(|v| data.len() <= byte_capacity(*v))?;
        let codewords = codewords(data, version);
        let mut best: Option<(u32, Qr)> = None;
        // Every mask is tried and scored, as the spec requires: the wrong one
        // leaves a symbol a camera struggles with even though it is valid.
        for mask in 0..8 {
            let qr = draw(version, &codewords, mask);
            let score = qr.penalty();
            if best.as_ref().is_none_or(|(b, _)| score < *b) {
                best = Some((score, qr));
            }
        }
        best.map(|(_, qr)| qr)
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn dark(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }

    /// The symbol as lines for a terminal, two rows of modules per line.
    ///
    /// Colours are set rather than assumed: a QR has to be dark on light, and
    /// half the terminals in the world are light on dark. The quiet zone is
    /// four modules, which is what the spec asks for and what a phone's camera
    /// wants in poor light.
    pub fn to_terminal(&self) -> String {
        const QUIET: usize = 4;
        let span = self.size + QUIET * 2;
        let mut out = String::with_capacity(span * span * 4);
        let mut y = 0;
        while y < span {
            out.push_str("\x1b[40;107m");
            for x in 0..span {
                let upper = self.at(x, y, QUIET);
                let lower = self.at(x, y + 1, QUIET);
                // The glyph fills the top half, so the character's own colour
                // is the upper module and the background is the lower one.
                out.push(match (upper, lower) {
                    (true, true) => '\u{2588}',
                    (true, false) => '\u{2580}',
                    (false, true) => '\u{2584}',
                    (false, false) => ' ',
                });
            }
            out.push_str("\x1b[0m\n");
            y += 2;
        }
        out
    }

    /// Whether the module at a point in the quiet-zone-padded grid is dark.
    /// Outside the symbol is light, which is what the quiet zone is.
    fn at(&self, x: usize, y: usize, quiet: usize) -> bool {
        let (Some(x), Some(y)) = (x.checked_sub(quiet), y.checked_sub(quiet)) else {
            return false;
        };
        x < self.size && y < self.size && self.dark(x, y)
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
    }

    /// How badly this symbol reads, by the spec's four rules. Lower is better.
    fn penalty(&self) -> u32 {
        self.runs() + self.blocks() + self.finder_lookalikes() + self.balance()
    }

    /// Rule one: runs of five or more of one colour, in both directions.
    fn runs(&self) -> u32 {
        let n = self.size;
        let mut score = 0;
        for i in 0..n {
            for line in [true, false] {
                let mut run = 1;
                let mut prev = if line {
                    self.dark(0, i)
                } else {
                    self.dark(i, 0)
                };
                for j in 1..n {
                    let cur = if line {
                        self.dark(j, i)
                    } else {
                        self.dark(i, j)
                    };
                    if cur == prev {
                        run += 1;
                    } else {
                        if run >= 5 {
                            score += 3 + (run - 5);
                        }
                        run = 1;
                        prev = cur;
                    }
                }
                if run >= 5 {
                    score += 3 + (run - 5);
                }
            }
        }
        score
    }

    /// Rule two: any two by two block of one colour.
    fn blocks(&self) -> u32 {
        let n = self.size;
        let mut score = 0;
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = self.dark(x, y);
                if c == self.dark(x + 1, y)
                    && c == self.dark(x, y + 1)
                    && c == self.dark(x + 1, y + 1)
                {
                    score += 3;
                }
            }
        }
        score
    }

    /// Rule three: anything that looks like a finder pattern, either way round.
    fn finder_lookalikes(&self) -> u32 {
        const A: u16 = 0b10111010000;
        const B: u16 = 0b00001011101;
        let n = self.size;
        let mut score = 0;
        for i in 0..n {
            let (mut row, mut col) = (0u16, 0u16);
            for j in 0..n {
                row = (row << 1 & 0x7FF) | self.dark(j, i) as u16;
                col = (col << 1 & 0x7FF) | self.dark(i, j) as u16;
                if j >= 10 {
                    if row == A || row == B {
                        score += 40;
                    }
                    if col == A || col == B {
                        score += 40;
                    }
                }
            }
        }
        score
    }

    /// Rule four: how far the whole symbol is from half dark. The spec takes
    /// the multiples of five either side of the percentage and keeps whichever
    /// is nearer to fifty, which is not the same as rounding one way.
    fn balance(&self) -> u32 {
        let dark = self.modules.iter().filter(|m| **m).count();
        let percent = dark * 100 / self.modules.len();
        let below = percent / 5 * 5;
        let above = percent.div_ceil(5) * 5;
        let k = below.abs_diff(50).min(above.abs_diff(50)) / 5;
        k as u32 * 10
    }
}

/// Bytes a version holds at level M, after the mode and length header.
fn byte_capacity(version: usize) -> usize {
    let Layout { g1, g2, .. } = &BLOCKS[version - 1];
    let data_bits = (g1.0 * g1.1 + g2.0 * g2.1) * 8;
    (data_bits - 4 - count_bits(version)) / 8
}

/// Bits the length field takes. Byte mode uses eight up to version 9 and
/// sixteen from ten.
fn count_bits(version: usize) -> usize {
    if version <= 9 { 8 } else { 16 }
}

/// The message as codewords: header, data, padding, then error correction,
/// interleaved the way the spec lays blocks out.
fn codewords(data: &[u8], version: usize) -> Vec<u8> {
    let &Layout {
        ec: ec_len,
        g1: (n1, d1),
        g2: (n2, d2),
    } = &BLOCKS[version - 1];
    let total_data = n1 * d1 + n2 * d2;

    let mut bits = Bits::new(total_data);
    bits.push(0b0100, 4); // byte mode
    bits.push(data.len() as u32, count_bits(version));
    for b in data {
        bits.push(*b as u32, 8);
    }
    // Terminator, then out to a whole byte, then the two pad codewords the
    // spec names, alternating, for as far as there is room.
    bits.push(0, (4).min(total_data * 8 - bits.len()));
    bits.pad_to_byte();
    let mut bytes = bits.into_bytes();
    for pad in [0xEC, 0x11].iter().cycle() {
        if bytes.len() >= total_data {
            break;
        }
        bytes.push(*pad);
    }

    // Split into blocks, giving the shorter group its codewords first.
    let mut blocks: Vec<&[u8]> = Vec::with_capacity(n1 + n2);
    let mut at = 0;
    for _ in 0..n1 {
        blocks.push(&bytes[at..at + d1]);
        at += d1;
    }
    for _ in 0..n2 {
        blocks.push(&bytes[at..at + d2]);
        at += d2;
    }
    let ec: Vec<Vec<u8>> = blocks.iter().map(|b| reed_solomon(b, ec_len)).collect();

    // Interleaved: the first codeword of every block, then the second, and so
    // on. A block shorter than its neighbours simply runs out.
    let mut out = Vec::with_capacity(total_data + ec_len * blocks.len());
    for i in 0..d1.max(d2) {
        for b in &blocks {
            if let Some(c) = b.get(i) {
                out.push(*c);
            }
        }
    }
    for i in 0..ec_len {
        for b in &ec {
            out.push(b[i]);
        }
    }
    out
}

/// Error correction codewords for one block: the remainder of the message
/// divided by the generator polynomial, over GF(256).
fn reed_solomon(data: &[u8], ec_len: usize) -> Vec<u8> {
    let (exp, log) = gf_tables();
    let poly = generator(ec_len, &exp, &log);
    let mut rem = vec![0u8; data.len() + ec_len];
    rem[..data.len()].copy_from_slice(data);
    for i in 0..data.len() {
        let lead = rem[i];
        if lead == 0 {
            continue;
        }
        let lead = log[lead as usize] as usize;
        for (j, g) in poly.iter().enumerate() {
            rem[i + j] ^= exp[(lead + *g as usize) % 255];
        }
    }
    rem[data.len()..].to_vec()
}

/// The generator polynomial for `ec_len` codewords, as logs of its
/// coefficients: the product of (x - a^i) for i below `ec_len`.
fn generator(ec_len: usize, exp: &[u8; 256], log: &[u8; 256]) -> Vec<u8> {
    let mut poly = vec![1u8];
    for i in 0..ec_len {
        poly.push(0);
        let root = exp[i % 255];
        for j in (1..poly.len()).rev() {
            poly[j] ^= mul(poly[j - 1], root, exp, log);
        }
    }
    poly.iter().map(|c| log[*c as usize]).collect()
}

fn mul(a: u8, b: u8, exp: &[u8; 256], log: &[u8; 256]) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    exp[(log[a as usize] as usize + log[b as usize] as usize) % 255]
}

/// Powers of two and their logs in GF(256), modulo the polynomial QR uses.
fn gf_tables() -> ([u8; 256], [u8; 256]) {
    let mut exp = [0u8; 256];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    for (i, e) in exp.iter_mut().enumerate().take(255) {
        *e = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11D;
        }
    }
    exp[255] = exp[0];
    (exp, log)
}

/// A run of bits being built up, most significant first.
struct Bits {
    bytes: Vec<u8>,
    bits: usize,
}

impl Bits {
    fn new(capacity_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity_bytes),
            bits: 0,
        }
    }

    fn len(&self) -> usize {
        self.bits
    }

    fn push(&mut self, value: u32, width: usize) {
        for i in (0..width).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if value >> i & 1 == 1 {
                let at = self.bits;
                self.bytes[at / 8] |= 0x80 >> (at % 8);
            }
            self.bits += 1;
        }
    }

    fn pad_to_byte(&mut self) {
        while !self.bits.is_multiple_of(8) {
            self.push(0, 1);
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Lay out one symbol: the patterns that are always there, then the data,
/// then the mask, then the strings that say which mask was used.
fn draw(version: usize, codewords: &[u8], mask: u8) -> Qr {
    let size = version * 4 + 17;
    let mut qr = Qr {
        size,
        modules: vec![false; size * size],
    };
    // Which modules belong to a pattern, and so are not to be written over or
    // masked. Format and version areas are reserved here and filled last.
    let mut fixed = vec![false; size * size];
    let reserve = |qr: &mut Qr, fixed: &mut Vec<bool>, x: usize, y: usize, dark: bool| {
        qr.set(x, y, dark);
        fixed[y * size + x] = true;
    };

    for (fx, fy) in [(0, 0), (size - 7, 0), (0, size - 7)] {
        for dy in 0..7 {
            for dx in 0..7 {
                let edge = dx == 0 || dx == 6 || dy == 0 || dy == 6;
                let core = (2..=4).contains(&dx) && (2..=4).contains(&dy);
                reserve(&mut qr, &mut fixed, fx + dx, fy + dy, edge || core);
            }
        }
    }
    // The light separator on the inner side of each finder, so a reader can
    // tell where the pattern stops.
    for i in 0..8 {
        reserve(&mut qr, &mut fixed, i, 7, false);
        reserve(&mut qr, &mut fixed, 7, i, false);
        reserve(&mut qr, &mut fixed, size - 8, i, false);
        reserve(&mut qr, &mut fixed, size - 1 - i, 7, false);
        reserve(&mut qr, &mut fixed, i, size - 8, false);
        reserve(&mut qr, &mut fixed, 7, size - 1 - i, false);
    }

    // Timing: the alternating line that tells a reader how big a module is.
    for i in 8..size - 8 {
        let dark = i % 2 == 0;
        reserve(&mut qr, &mut fixed, i, 6, dark);
        reserve(&mut qr, &mut fixed, 6, i, dark);
    }

    // Alignment patterns, at every pair of centres that misses a finder.
    let centres = ALIGNMENT[version - 1];
    for (i, cy) in centres.iter().enumerate() {
        for (j, cx) in centres.iter().enumerate() {
            let corner = (i == 0 && j == 0)
                || (i == 0 && j == centres.len() - 1)
                || (i == centres.len() - 1 && j == 0);
            if corner {
                continue;
            }
            for dy in 0..5 {
                for dx in 0..5 {
                    let edge = dx == 0 || dx == 4 || dy == 0 || dy == 4;
                    let centre = dx == 2 && dy == 2;
                    reserve(
                        &mut qr,
                        &mut fixed,
                        cx + dx - 2,
                        cy + dy - 2,
                        edge || centre,
                    );
                }
            }
        }
    }

    // The one module that is always dark, and has no other job.
    reserve(&mut qr, &mut fixed, 8, size - 8, true);

    // Reserve where the format and version strings will go.
    for i in 0..9 {
        fixed[i * size + 8] = true;
        fixed[8 * size + i] = true;
    }
    for i in 0..8 {
        fixed[8 * size + (size - 1 - i)] = true;
        fixed[(size - 1 - i) * size + 8] = true;
    }
    if version >= 7 {
        for i in 0..18 {
            fixed[(size - 11 + i % 3) * size + i / 3] = true;
            fixed[(i / 3) * size + (size - 11 + i % 3)] = true;
        }
    }

    // The data, up the right-hand side and down again, two columns at a time.
    let mut bit = 0usize;
    let total_bits = codewords.len() * 8 + REMAINDER_BITS[version - 1];
    let mut col = size as isize - 1;
    let mut upward = true;
    while col >= 0 {
        // Column six is the vertical timing pattern; the pairs step over it.
        if col == 6 {
            col -= 1;
            continue;
        }
        for step in 0..size {
            let y = if upward { size - 1 - step } else { step };
            for dx in 0..2 {
                let x = (col - dx) as usize;
                if fixed[y * size + x] || bit >= total_bits {
                    continue;
                }
                let dark =
                    bit < codewords.len() * 8 && codewords[bit / 8] >> (7 - bit % 8) & 1 == 1;
                qr.set(x, y, dark ^ masked(mask, x, y));
                bit += 1;
            }
        }
        upward = !upward;
        col -= 2;
    }

    // The format string, twice, so losing a corner is survivable. Each copy
    // is itself split: column eight runs down the left of the symbol and
    // picks up again at the bottom, row eight runs along the top and picks up
    // again at the right. The two skips are the timing line and the module
    // that is always dark.
    let format = format_bits(mask);
    for i in 0..15 {
        let dark = format >> i & 1 == 1;
        let y = if i < 6 {
            i
        } else if i < 8 {
            i + 1
        } else {
            size - 15 + i
        };
        qr.set(8, y, dark);
        let x = if i < 8 {
            size - 1 - i
        } else if i == 8 {
            7
        } else {
            14 - i
        };
        qr.set(x, 8, dark);
    }

    if version >= 7 {
        let info = version_bits(version);
        for i in 0..18 {
            let dark = info >> i & 1 == 1;
            qr.set(i / 3, size - 11 + i % 3, dark);
            qr.set(size - 11 + i % 3, i / 3, dark);
        }
    }

    qr
}

/// Whether a mask flips the module at this point.
fn masked(mask: u8, x: usize, y: usize) -> bool {
    let (i, j) = (y, x);
    match mask {
        0 => (i + j) % 2 == 0,
        1 => i % 2 == 0,
        2 => j % 3 == 0,
        3 => (i + j) % 3 == 0,
        4 => (i / 2 + j / 3) % 2 == 0,
        5 => (i * j) % 2 + (i * j) % 3 == 0,
        6 => ((i * j) % 2 + (i * j) % 3) % 2 == 0,
        _ => ((i + j) % 2 + (i * j) % 3) % 2 == 0,
    }
}

/// The fifteen bits that say which error correction level and mask were used,
/// with their BCH check bits and the spec's fixed XOR so they are never all
/// the same colour.
fn format_bits(mask: u8) -> u32 {
    let data = EC_LEVEL_BITS << 3 | mask as u32;
    let mut rem = data << 10;
    for i in (0..5).rev() {
        if rem >> (i + 10) & 1 == 1 {
            rem ^= 0x537 << i;
        }
    }
    ((data << 10) | rem) ^ 0x5412
}

/// The eighteen bits that say which version this is, for the versions large
/// enough that a reader cannot simply count.
fn version_bits(version: usize) -> u32 {
    let data = version as u32;
    let mut rem = data << 12;
    for i in (0..6).rev() {
        if rem >> (i + 12) & 1 == 1 {
            rem ^= 0x1F25 << i;
        }
    }
    (data << 12) | rem
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator polynomials the spec prints, as logs of their
    /// coefficients. Getting these right is most of getting Reed-Solomon
    /// right, and they are published, so they can simply be checked.
    #[test]
    fn the_generator_polynomials_are_the_published_ones() {
        let (exp, log) = gf_tables();
        assert_eq!(
            generator(10, &exp, &log),
            vec![0, 251, 67, 46, 61, 118, 70, 64, 94, 32, 45]
        );
        assert_eq!(
            generator(16, &exp, &log),
            vec![
                0, 120, 104, 107, 109, 102, 161, 76, 3, 91, 191, 147, 169, 182, 194, 225, 120
            ]
        );
        assert_eq!(
            generator(26, &exp, &log),
            vec![
                0, 173, 125, 158, 2, 103, 182, 118, 17, 145, 201, 111, 28, 165, 53, 161, 21, 245,
                142, 13, 102, 48, 227, 153, 145, 218, 70
            ]
        );
    }

    #[test]
    fn the_field_is_the_one_qr_uses() {
        let (exp, log) = gf_tables();
        assert_eq!(exp[0], 1);
        assert_eq!(exp[1], 2);
        // Where the polynomial first folds back on itself.
        assert_eq!(exp[8], 0x1D);
        for i in 1..255usize {
            assert_eq!(log[exp[i] as usize] as usize, i, "a^{i}");
        }
    }

    /// Also published, for every mask at this error correction level.
    #[test]
    fn the_format_strings_are_the_published_ones() {
        let expected = [
            0b101010000010010,
            0b101000100100101,
            0b101111001111100,
            0b101101101001011,
            0b100010111111001,
            0b100000011001110,
            0b100111110010111,
            0b100101010100000,
        ];
        for (mask, want) in expected.iter().enumerate() {
            assert_eq!(format_bits(mask as u8), *want, "mask {mask}");
        }
    }

    /// Likewise for the versions large enough to carry a version string.
    #[test]
    fn the_version_strings_are_the_published_ones() {
        assert_eq!(version_bits(7), 0b000111110010010100);
        assert_eq!(version_bits(8), 0b001000010110111100);
        assert_eq!(version_bits(9), 0b001001101010011001);
        assert_eq!(version_bits(10), 0b001010010011010011);
    }

    #[test]
    fn a_version_is_as_big_as_the_spec_says() {
        for (v, want) in [(1, 21), (2, 25), (7, 45), (10, 57)] {
            let qr = draw(v, &[0u8; 8], 0);
            assert_eq!(qr.size, want, "version {v}");
        }
    }

    #[test]
    fn the_capacities_are_the_published_ones() {
        let want = [14, 26, 42, 62, 84, 106, 122, 152, 180, 213];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(byte_capacity(i + 1), *w, "version {}", i + 1);
        }
    }

    #[test]
    fn the_smallest_version_that_fits_is_the_one_used() {
        assert_eq!(Qr::encode(&[b'x'; 14]).unwrap().size, 21, "version 1");
        assert_eq!(Qr::encode(&[b'x'; 15]).unwrap().size, 25, "version 2");
        assert_eq!(Qr::encode(&[b'x'; 213]).unwrap().size, 57, "version 10");
        assert!(Qr::encode(&[b'x'; 214]).is_none(), "past version 10");
    }

    #[test]
    fn the_patterns_a_reader_looks_for_are_where_it_looks() {
        let qr = Qr::encode(b"razorback://pair?u=https://example.workers.dev&c=AAAA").unwrap();
        let n = qr.size;
        for (fx, fy) in [(0, 0), (n - 7, 0), (0, n - 7)] {
            // A finder is a dark ring with a dark core and a light gap.
            assert!(qr.dark(fx, fy) && qr.dark(fx + 6, fy) && qr.dark(fx, fy + 6));
            assert!(!qr.dark(fx + 1, fy + 1), "the light ring");
            assert!(qr.dark(fx + 3, fy + 3), "the core");
        }
        // Timing runs between the finders, alternating from dark.
        for i in 8..n - 8 {
            assert_eq!(qr.dark(i, 6), i % 2 == 0, "timing at {i}");
            assert_eq!(qr.dark(6, i), i % 2 == 0, "timing at {i}");
        }
        assert!(qr.dark(8, n - 8), "the module that is always dark");
    }

    #[test]
    fn the_quiet_zone_is_there_and_is_light() {
        let qr = Qr::encode(b"hello").unwrap();
        for x in 0..qr.size + 8 {
            assert!(!qr.at(x, 0, 4), "top row of the quiet zone");
            assert!(!qr.at(x, qr.size + 7, 4), "bottom row");
        }
    }

    #[test]
    fn a_terminal_symbol_is_square_and_has_its_margins() {
        let qr = Qr::encode(b"hello").unwrap();
        let text = qr.to_terminal();
        let lines: Vec<&str> = text.lines().collect();
        let span = qr.size + 8;
        assert_eq!(lines.len(), span.div_ceil(2));
        for line in &lines {
            let modules = line
                .replace("\x1b[40;107m", "")
                .replace("\x1b[0m", "")
                .chars()
                .count();
            assert_eq!(modules, span, "every line is the same width");
        }
    }

    /// Reads a symbol back the way a decoder would: undo the mask, walk the
    /// same zigzag, and take the data codewords off the front. It shares this
    /// module's idea of the layout, so it does not prove the layout is right —
    /// what it catches is a symbol that is not even self-consistent, which is
    /// every mistake in interleaving, padding and bit order.
    fn read_back(qr: &Qr, version: usize, mask: u8) -> Vec<u8> {
        let size = qr.size;
        let blank = draw(version, &[], mask);
        let mut fixed = vec![false; size * size];
        // Anything the empty symbol wrote is a pattern, not data.
        for y in 0..size {
            for x in 0..size {
                fixed[y * size + x] = blank.dark(x, y) != masked(mask, x, y) && blank.dark(x, y)
                    || is_function(version, size, x, y);
            }
        }
        let mut bits: Vec<bool> = Vec::new();
        let mut col = size as isize - 1;
        let mut upward = true;
        while col >= 0 {
            if col == 6 {
                col -= 1;
                continue;
            }
            for step in 0..size {
                let y = if upward { size - 1 - step } else { step };
                for dx in 0..2 {
                    let x = (col - dx) as usize;
                    if is_function(version, size, x, y) {
                        continue;
                    }
                    bits.push(qr.dark(x, y) ^ masked(mask, x, y));
                }
            }
            upward = !upward;
            col -= 2;
        }
        bits.chunks(8)
            .filter(|c| c.len() == 8)
            .map(|c| c.iter().fold(0u8, |acc, b| acc << 1 | *b as u8))
            .collect()
    }

    /// Whether a module belongs to a pattern rather than to the message.
    fn is_function(version: usize, size: usize, x: usize, y: usize) -> bool {
        if x == 6 || y == 6 {
            return true;
        }
        for (fx, fy) in [(0usize, 0usize), (size - 8, 0), (0, size - 8)] {
            if x >= fx && x < fx + 8 && y >= fy && y < fy + 8 {
                return true;
            }
        }
        if x <= 8 && y <= 8 || x >= size - 8 && y <= 8 || x <= 8 && y >= size - 8 {
            return true;
        }
        if version >= 7 && (x < 6 && y >= size - 11 || y < 6 && x >= size - 11) {
            return true;
        }
        let centres = ALIGNMENT[version - 1];
        for (i, cy) in centres.iter().enumerate() {
            for (j, cx) in centres.iter().enumerate() {
                let corner = (i == 0 && j == 0)
                    || (i == 0 && j == centres.len() - 1)
                    || (i == centres.len() - 1 && j == 0);
                if !corner && x.abs_diff(*cx) <= 2 && y.abs_diff(*cy) <= 2 {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn a_symbol_holds_the_codewords_it_was_given() {
        for version in 1..=MAX_VERSION {
            let data: Vec<u8> = (0..byte_capacity(version))
                .map(|i| (i as u8).wrapping_mul(29).wrapping_add(7))
                .collect();
            let words = codewords(&data, version);
            for mask in 0..8 {
                let qr = draw(version, &words, mask);
                let back = read_back(&qr, version, mask);
                assert_eq!(
                    &back[..words.len()],
                    &words[..],
                    "version {version}, mask {mask}"
                );
            }
        }
    }

    #[test]
    fn the_balance_rule_takes_the_nearer_multiple_of_five() {
        let of = |dark: usize| {
            let mut qr = Qr {
                size: 21,
                modules: vec![false; 441],
            };
            for m in qr.modules.iter_mut().take(dark) {
                *m = true;
            }
            qr.balance()
        };
        // 225 of 441 is 51%, which sits between 50 and 55 and is nearer 50.
        // Rounding the percentage up instead would score this a penalty.
        assert_eq!(of(225), 0, "just over half is still half");
        assert_eq!(of(220), 0, "just under");
        assert_eq!(of(194), 10, "44%, nearer 45 than 40");
        assert_eq!(of(0), 100, "nothing dark at all");
        assert_eq!(of(441), 100, "everything dark");
    }

    #[test]
    fn error_correction_leaves_the_message_alone() {
        // The codewords a block is built from come back unchanged; only the
        // check bytes are added.
        let data: Vec<u8> = (0..16).collect();
        let ec = reed_solomon(&data, 10);
        assert_eq!(ec.len(), 10);
        // A known-good property of the remainder: dividing the whole
        // codeword by the generator again leaves nothing.
        let mut whole = data.clone();
        whole.extend_from_slice(&ec);
        assert!(reed_solomon(&whole, 10).iter().all(|b| *b == 0));
    }

    #[test]
    fn a_pairing_link_fits_with_room_to_spare() {
        let link = "razorback://pair?u=https://ah-relay.a-fairly-long-subdomain.workers.dev\
                    &c=A3K7QM2X9WRT4BND6HFE2PSV8CJY5LZQ";
        let qr = Qr::encode(link.as_bytes()).unwrap();
        assert!(qr.size <= 45, "version 7 or smaller, got {}", qr.size);
    }
}

#[cfg(test)]
mod cross_check {
    use super::*;

    /// Payloads chosen to reach every version this builds.
    fn payloads() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for version in 1..=MAX_VERSION {
            let n = byte_capacity(version);
            out.push((0..n).map(|i| b'!' + ((i * 7) % 90) as u8).collect());
            out.push(vec![b'A'; n]);
        }
        out.push(
            b"razorback://pair?u=https://ah-relay.example.workers.dev\
                   &c=A3K7QM2X9WRT4BND6HFE2PSV8CJY5LZQ"
                .to_vec(),
        );
        out
    }

    /// Dumps every symbol this crate can build, for comparison against an
    /// implementation that is known to scan. Not part of `just test`: it
    /// exists so the matrices can be diffed against `node-qrcode`.
    ///
    /// `cargo test -p ah-remote --lib cross_check -- --ignored --nocapture`
    /// Each rule on its own, so a difference against another implementation
    /// can be pinned to the rule it comes from.
    ///
    /// `cargo test -p ah-remote --lib cross_check::dump_penalties -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_penalties() {
        for (i, data) in payloads().iter().enumerate() {
            let version = (1..=MAX_VERSION)
                .find(|v| data.len() <= byte_capacity(*v))
                .unwrap();
            let words = codewords(data, version);
            for mask in 0..8 {
                let qr = draw(version, &words, mask);
                println!(
                    "{i} {mask} {} {} {} {}",
                    qr.runs(),
                    qr.blocks(),
                    qr.finder_lookalikes(),
                    qr.balance()
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn dump_chosen_masks() {
        for (i, data) in payloads().iter().enumerate() {
            let qr = Qr::encode(data).unwrap();
            let bits: String = (0..qr.size * qr.size)
                .map(|k| if qr.modules[k] { '1' } else { '0' })
                .collect();
            println!("{i} {} {bits}", qr.size);
        }
    }

    #[test]
    #[ignore]
    fn dump_every_symbol() {
        for (i, data) in payloads().iter().enumerate() {
            let version = (1..=MAX_VERSION)
                .find(|v| data.len() <= byte_capacity(*v))
                .unwrap();
            let words = codewords(data, version);
            for mask in 0..8 {
                let qr = draw(version, &words, mask);
                let bits: String = (0..qr.size * qr.size)
                    .map(|k| if qr.modules[k] { '1' } else { '0' })
                    .collect();
                println!("{i} {version} {mask} {} {bits}", qr.size);
            }
        }
    }
}
