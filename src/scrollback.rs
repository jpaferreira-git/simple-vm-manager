//! A text console's history, kept by the app. Linux took the scrollback out of its text console
//! (5.9), and XenServer shows a VM only as a picture, so the wheel had nothing to scroll there. The
//! app keeps one instead: it watches the screen, and when it goes up, the rows that leave it at the
//! top are kept, for the wheel to scroll back through.
//!
//! Only what was on the screen at some look can be kept: the screen comes some 30 times a second,
//! so of a command that prints more than a screenful at once, only the end is seen. When a screen
//! changes all at once like that (or a program takes the whole screen), the one before is kept
//! whole, with a mark under it for what went by unseen. Without a key pressed in between (a program
//! such as top redrawing itself), the screen kept that way is the latest only.
//!
//! Only on a text console: a screen of a few colours. A desktop's has many, and its wheel is its own.

use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use std::collections::VecDeque;
use std::io::{Read, Write};

/// At most this many rows of pixels are kept (some 4000 lines of text): the oldest go first.
const MAX_ROWS: usize = 64_000;
/// A screen of more colours than this isn't a text console (Linux's has 16).
const TEXT_COLOURS: usize = 16;
/// The mark under a screen kept whole: what went by after it wasn't seen.
const MARK_ROWS: usize = 12;
const MARK_DOT: [u8; 4] = [90, 90, 98, 255];

/// Rows kept together: those that went up at one look, or a whole screen.
struct Strip {
    rows: usize,
    /// Its pixels (RGBA), compressed.
    data: Vec<u8>,
    /// A whole screen kept as it changed all at once (its mark under it), with no key pressed
    /// since the one before: the next such screen takes its place.
    unkeyed_screen: bool,
}

pub struct Scrollback {
    width: usize,
    height: usize,
    /// The last look at the screen (when a text console): its pixels, and each row's fingerprint.
    last: Vec<u8>,
    last_rows: Vec<Row>,
    strips: VecDeque<Strip>,
    /// Rows kept, all strips together.
    kept: usize,
    /// A key went to the VM since a screen was last kept whole.
    keyed: bool,
    text: bool,
    /// Rows ever kept (some may have gone since): what was kept since a look, even once full.
    pushed: usize,
    /// A line of text's height, as the screen went up (a whole number of them each time).
    line: Option<usize>,
}

/// A row of pixels: a fingerprint of it, and whether it's all one colour (nothing on it: such rows
/// prove nothing when they match).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Row {
    hash: u64,
    plain: bool,
}

impl Default for Scrollback {
    fn default() -> Self {
        Scrollback { width: 0, height: 0, last: vec![], last_rows: vec![], strips: VecDeque::new(), kept: 0, keyed: true, text: false, pushed: 0, line: None }
    }
}

impl Scrollback {
    /// A key went to the VM (the next screen that changes all at once is a new one, not a program
    /// redrawing itself).
    pub fn key(&mut self) {
        self.keyed = true;
    }

    /// The last look was at a text console: the wheel scrolls this history, not the VM.
    pub fn is_text(&self) -> bool {
        self.text
    }

    /// How many rows of pixels are kept above the screen.
    pub fn rows(&self) -> usize {
        self.kept
    }

    /// Rows ever kept, the oldest gone included (it only grows, until the screen changes size).
    pub fn pushed(&self) -> usize {
        self.pushed
    }

    /// A line of text's height in rows of pixels, as the screen went up (16 until it has).
    pub fn line(&self) -> usize {
        self.line.unwrap_or(16)
    }

    /// A new look at the screen (each time an update of it is complete).
    pub fn look(&mut self, w: usize, h: usize, rgba: &[u8]) {
        if (w, h) != (self.width, self.height) {
            // a new size: what was kept doesn't fit it
            *self = Scrollback { width: w, height: h, ..Default::default() };
        }
        self.text = w > 0 && h > 0 && rgba.len() == w * h * 4 && colours_at_most(rgba, TEXT_COLOURS);
        if !self.text {
            self.last.clear();
            self.last_rows.clear();
            return;
        }
        let rows: Vec<Row> = rgba.chunks_exact(w * 4).map(row).collect();
        if rows == self.last_rows {
            return;
        }
        if self.last_rows.len() == h {
            if let Some(s) = went_up(&self.last_rows, &rows) {
                let top = self.last[..s * w * 4].to_vec();
                self.push(top, s, false);
                if s <= 64 {
                    self.line = Some(self.line.map_or(s, |l| gcd(l, s)).max(8));
                }
            } else if changed_all_at_once(&self.last_rows, &rows) {
                let mut screen = std::mem::take(&mut self.last);
                screen.extend(mark(w));
                if !self.keyed && self.strips.back().map_or(false, |b| b.unkeyed_screen) {
                    let b = self.strips.pop_back().unwrap();
                    self.kept -= b.rows;
                }
                self.push(screen, h + MARK_ROWS, !self.keyed);
                self.keyed = false;
            }
        }
        self.last.clear();
        self.last.extend_from_slice(rgba);
        self.last_rows = rows;
    }

    fn push(&mut self, rgba: Vec<u8>, rows: usize, unkeyed_screen: bool) {
        let mut e = DeflateEncoder::new(Vec::new(), Compression::fast());
        let _ = e.write_all(&rgba);
        let data = e.finish().unwrap_or_default();
        self.strips.push_back(Strip { rows, data, unkeyed_screen });
        self.kept += rows;
        self.pushed += rows;
        while self.kept > MAX_ROWS {
            match self.strips.pop_front() {
                Some(s) => self.kept -= s.rows,
                None => break,
            }
        }
    }

    /// The screen as it was `back` rows of pixels up its history (0: the screen now, `live`): a
    /// picture the screen's size, of kept rows above as many of the screen's own as fit under them.
    pub fn view(&self, back: usize, live: &[u8]) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let back = back.min(self.kept);
        let line = w * 4;
        let mut out = Vec::with_capacity(h * line);
        // the kept rows from `kept - back` on, then the screen's
        let first = self.kept - back;
        let mut at = 0; // the first row of the strip looked at
        for s in &self.strips {
            if out.len() >= h * line {
                break;
            }
            if at + s.rows > first {
                let mut px = Vec::with_capacity(s.rows * line);
                let _ = DeflateDecoder::new(&s.data[..]).read_to_end(&mut px);
                px.resize(s.rows * line, 0);
                let from = first.saturating_sub(at);
                let take = (s.rows - from).min(h - out.len() / line);
                out.extend_from_slice(&px[from * line..(from + take) * line]);
            }
            at += s.rows;
        }
        let rest = h - out.len() / line;
        out.extend_from_slice(&live[..(rest * line).min(live.len())]);
        out.resize(h * line, 0);
        out
    }
}

#[cfg(test)]
impl Scrollback {
    /// Every kept row's pixels, oldest first.
    pub(crate) fn kept_pixels(&self) -> Vec<u8> {
        let mut out = vec![];
        for s in &self.strips {
            let _ = DeflateDecoder::new(&s.data[..]).read_to_end(&mut out);
        }
        out
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A row's fingerprint, and whether it's all one colour.
fn row(px: &[u8]) -> Row {
    let first = &px[..4.min(px.len())];
    let plain = px.chunks_exact(4).all(|p| p == first);
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ px.len() as u64;
    let mut words = px.chunks_exact(8);
    for c in &mut words {
        hash = (hash.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x5170_4fd1_b727_220b);
    }
    for &b in words.remainder() {
        hash = (hash.rotate_left(5) ^ b as u64).wrapping_mul(0x5170_4fd1_b727_220b);
    }
    Row { hash, plain }
}

/// The picture has no more than `n` colours.
fn colours_at_most(rgba: &[u8], n: usize) -> bool {
    let mut seen: Vec<[u8; 4]> = Vec::with_capacity(n + 1);
    let mut prev = [0u8, 0, 0, 0];
    let mut have_prev = false;
    for p in rgba.chunks_exact(4) {
        let p = [p[0], p[1], p[2], p[3]];
        if have_prev && p == prev {
            continue;
        }
        (prev, have_prev) = (p, true);
        if !seen.contains(&p) {
            if seen.len() == n {
                return false;
            }
            seen.push(p);
        }
    }
    true
}

/// How many rows of pixels the screen went up between two looks, if it did: what was on the old
/// screen that far down is on the new one at the top (all but a few rows: the cursor moves, a
/// status line stays put), and that fits better than nothing having moved. Where the old screen
/// had nothing, the new one may have anything: a screen not full yet is written on as it goes up.
fn went_up(old: &[Row], new: &[Row]) -> Option<usize> {
    let h = old.len();
    let fits = |s: usize| -> Option<usize> {
        let overlap = h - s;
        let allowed = (overlap / 20).max(4);
        let (mut wrong, mut text) = (0, 0);
        for y in 0..overlap {
            let was = old[y + s];
            if new[y] == was {
                text += !was.plain as usize;
            } else if !was.plain {
                wrong += 1;
                if wrong > allowed {
                    return None;
                }
            }
        }
        Some(text)
    };
    let still: usize = (0..h).filter(|&y| new[y] == old[y] && !new[y].plain).count();
    let mut best: Option<(usize, usize)> = None; // (text rows that fit, how far)
    for s in 1..h {
        if let Some(text) = fits(s) {
            if text >= 8 && text > still && best.map_or(true, |(t, _)| text > t) {
                best = Some((text, s));
            }
        }
    }
    best.map(|(_, s)| s)
}

/// The screen changed all at once: most of what was on it (rows with something on them) isn't
/// where it was any more, and the new one has something on it (not a screen gone dark).
fn changed_all_at_once(old: &[Row], new: &[Row]) -> bool {
    let had = old.iter().filter(|r| !r.plain).count();
    let stayed = old.iter().zip(new).filter(|(o, n)| o == n && !o.plain).count();
    had >= 8 && stayed * 2 < had && new.iter().any(|r| !r.plain)
}

/// The mark under a screen kept whole: dots across the middle of a dark band.
fn mark(w: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * MARK_ROWS * 4];
    for p in px.chunks_exact_mut(4) {
        p[3] = 255;
    }
    let y = MARK_ROWS / 2;
    for x in (2..w).step_by(6) {
        for dx in 0..2 {
            let i = (y * w + x + dx).min(w * MARK_ROWS - 1) * 4;
            px[i..i + 4].copy_from_slice(&MARK_DOT);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 80;
    const LINE: usize = 16;

    /// A pretend text console `lines` lines high, each line's text drawn from its number (0: an
    /// empty line), in two colours.
    fn screen(lines: &[u32]) -> Vec<u8> {
        let mut px = Vec::with_capacity(W * LINE * lines.len() * 4);
        for &n in lines {
            for y in 0..LINE {
                for x in 0..W {
                    // glyph-like: nothing on a cell's top and bottom rows, a pattern by line and cell
                    let ink = n != 0 && (3..13).contains(&y) && (x % 8) != 0 && {
                        let mut k = (n as u64) << 32 | (y as u64) << 16 | x as u64;
                        k = (k ^ (k >> 31)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                        (k ^ (k >> 29)) % 5 < 2
                    };
                    px.extend_from_slice(if ink { &[206, 207, 206, 255] } else { &[0, 0, 0, 255] });
                }
            }
        }
        px
    }

    fn look(sb: &mut Scrollback, lines: &[u32]) {
        sb.look(W, LINE * lines.len(), &screen(lines));
    }

    /// The kept rows, as the lines they show (0 for the mark's or an empty line's).
    fn kept_lines(sb: &Scrollback, h_lines: usize) -> Vec<u32> {
        let _ = h_lines;
        let all = sb.kept_pixels();
        let line_px = W * LINE * 4;
        (0..sb.rows() / LINE).map(|i| (1..10_000).find(|&n| screen(&[n])[..] == all[i * line_px..(i + 1) * line_px]).unwrap_or(0)).collect()
    }

    #[test]
    fn keeps_the_lines_that_go_up() {
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 4, 5, 6]);
        assert!(sb.is_text());
        look(&mut sb, &[3, 4, 5, 6, 7, 8]); // two lines up
        assert_eq!(sb.rows(), 2 * LINE);
        assert_eq!(sb.line(), 2 * LINE, "two lines at once, so far");
        look(&mut sb, &[4, 5, 6, 7, 8, 9]);
        assert_eq!(kept_lines(&sb, 6), vec![1, 2, 3]);
        assert_eq!((sb.line(), sb.pushed()), (LINE, 3 * LINE));
        // and the view a line back: the last kept line over the screen's first five
        let v = sb.view(LINE, &screen(&[4, 5, 6, 7, 8, 9]));
        assert_eq!(v, screen(&[3, 4, 5, 6, 7, 8]));
        assert_eq!(sb.view(0, &screen(&[4, 5, 6, 7, 8, 9])), screen(&[4, 5, 6, 7, 8, 9]));
        assert_eq!(sb.view(999_999, &screen(&[4, 5, 6, 7, 8, 9])), screen(&[1, 2, 3, 4, 5, 6]), "no further back than what's kept");
    }

    #[test]
    fn a_screen_not_full_yet_written_on_as_it_goes_up() {
        // (as a command's output fills a screen's empty bottom and goes on: seen on a real one)
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 0, 2, 3, 0, 0]);
        look(&mut sb, &[2, 3, 4, 5, 6, 7]);
        assert_eq!(kept_lines(&sb, 6), vec![1, 0]);
        // and the same, its new lines written over its old empty ones a line up
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 0, 0, 0]);
        look(&mut sb, &[2, 3, 8, 9, 10, 11]);
        assert_eq!(kept_lines(&sb, 6), vec![1]);
    }

    #[test]
    fn typing_and_the_cursor_keep_nothing() {
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 0, 0, 0]);
        look(&mut sb, &[1, 2, 3, 4, 0, 0]); // a line written under, nothing moved
        look(&mut sb, &[1, 2, 3, 4, 5, 0]);
        let mut s = screen(&[1, 2, 3, 4, 5, 0]);
        // the cursor: two rows of the last line's first cell
        for y in 5 * LINE + 13..5 * LINE + 15 {
            for x in 0..8 {
                s[(y * W + x) * 4..(y * W + x) * 4 + 4].copy_from_slice(&[206, 207, 206, 255]);
            }
        }
        sb.look(W, 6 * LINE, &s);
        look(&mut sb, &[1, 2, 3, 4, 5, 0]);
        assert_eq!(sb.rows(), 0);
    }

    #[test]
    fn a_line_up_with_the_cursor_moving_too() {
        let mut sb = Scrollback::default();
        let mut s = screen(&[1, 2, 3, 4, 5, 6]);
        for x in 0..8 {
            let i = ((5 * LINE + 14) * W + x) * 4;
            s[i..i + 4].copy_from_slice(&[206, 207, 206, 255]);
        }
        sb.look(W, 6 * LINE, &s);
        look(&mut sb, &[2, 3, 4, 5, 6, 7]);
        assert_eq!(kept_lines(&sb, 6), vec![1]);
    }

    #[test]
    fn a_screen_changed_all_at_once_is_kept_whole_under_a_mark() {
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 4, 5, 6]);
        sb.key();
        look(&mut sb, &[50, 51, 52, 53, 54, 55]); // a long output: only its end was seen
        assert_eq!(sb.rows(), 6 * LINE + MARK_ROWS);
        let all = sb.kept_pixels();
        assert_eq!(&all[..6 * LINE * W * 4], &screen(&[1, 2, 3, 4, 5, 6])[..]);
        assert_eq!(&all[6 * LINE * W * 4..(6 * LINE + MARK_ROWS) * W * 4], &mark(W)[..]);
        // then it goes on scrolling under the mark
        look(&mut sb, &[51, 52, 53, 54, 55, 56]);
        assert_eq!(sb.rows(), 7 * LINE + MARK_ROWS);
    }

    #[test]
    fn a_program_redrawing_itself_is_kept_once() {
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 4, 5, 6]);
        sb.key(); // top, say
        look(&mut sb, &[70, 71, 72, 73, 74, 75]);
        look(&mut sb, &[80, 81, 82, 83, 84, 85]); // it redraws (no key): its last screen is kept
        look(&mut sb, &[90, 91, 92, 93, 94, 95]);
        look(&mut sb, &[100, 101, 102, 103, 104, 105]);
        let screen_rows = 6 * LINE + MARK_ROWS;
        assert_eq!(sb.rows(), 2 * screen_rows, "the shell's screen, and top's latest");
        let all = sb.kept_pixels();
        assert_eq!(&all[..6 * LINE * W * 4], &screen(&[1, 2, 3, 4, 5, 6])[..]);
        assert_eq!(&all[screen_rows * W * 4..(screen_rows + 6 * LINE) * W * 4], &screen(&[90, 91, 92, 93, 94, 95])[..]);
    }

    #[test]
    fn a_screen_gone_dark_keeps_nothing() {
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 4, 5, 6]);
        look(&mut sb, &[0, 0, 0, 0, 0, 0]);
        look(&mut sb, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(sb.rows(), 0);
    }

    #[test]
    fn a_desktop_is_not_a_text_console() {
        let mut sb = Scrollback::default();
        let px: Vec<u8> = (0..W * 64).flat_map(|i| [(i % 251) as u8, (i % 13) as u8, 40, 255]).collect();
        sb.look(W, 64, &px);
        assert!(!sb.is_text());
        sb.look(W, 64, &px[W * 4 * 16..].iter().chain(&px[..W * 4 * 16]).copied().collect::<Vec<u8>>());
        assert_eq!(sb.rows(), 0);
    }

    #[test]
    fn a_new_size_starts_again() {
        let mut sb = Scrollback::default();
        look(&mut sb, &[1, 2, 3, 4, 5, 6]);
        look(&mut sb, &[2, 3, 4, 5, 6, 7]);
        assert_eq!(sb.rows(), LINE);
        look(&mut sb, &[2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(sb.rows(), 0);
    }

    #[test]
    fn keeps_no_more_than_max_rows() {
        let mut sb = Scrollback::default();
        let mut lines: Vec<u32> = (1..=6).collect();
        for n in 7..(MAX_ROWS / LINE + 50) as u32 {
            look(&mut sb, &lines);
            lines.remove(0);
            lines.push(n);
        }
        assert!(sb.rows() <= MAX_ROWS && sb.rows() > MAX_ROWS - 10 * LINE);
    }

    /// A real console's picture (SVM_TEST_SHOT=<file.png>, e.g. from conn.rs's console_capture_live),
    /// scrolled a line, two and ten as Linux does: each time, the lines that went up are kept.
    #[test]
    #[ignore]
    fn a_real_screen_going_up() {
        let path = std::env::var("SVM_TEST_SHOT").expect("SVM_TEST_SHOT");
        let img = image::open(path).unwrap().to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let px = img.into_raw();
        let line = 16;
        let text_h = h / line * line;
        let up = |n: usize| -> Vec<u8> {
            // the screen n lines up, with empty lines under (the rows under the last whole line stay)
            let mut out = px[n * line * w * 4..text_h * w * 4].to_vec();
            out.resize(text_h * w * 4, 0);
            for p in out[(text_h - n * line) * w * 4..].chunks_exact_mut(4) {
                p.copy_from_slice(&[0, 0, 0, 255]);
            }
            out.extend_from_slice(&px[text_h * w * 4..]);
            out
        };
        let mut sb = Scrollback::default();
        sb.look(w, h, &px);
        assert!(sb.is_text(), "Linux's console is a text console");
        let (a, b) = (up(1), up(2));
        let t = std::time::Instant::now();
        let ra: Vec<Row> = a.chunks_exact(w * 4).map(row).collect();
        let rb: Vec<Row> = b.chunks_exact(w * 4).map(row).collect();
        println!("rows: {:?}", t.elapsed() / 2);
        let t = std::time::Instant::now();
        assert!(colours_at_most(&a, TEXT_COLOURS));
        println!("colours: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        assert_eq!(went_up(&ra, &rb), Some(line));
        println!("went_up: {:?}", t.elapsed());
        let started = std::time::Instant::now();
        sb.look(w, h, &up(1));
        println!("a look: {:?}", started.elapsed());
        assert_eq!(sb.rows(), line);
        sb.look(w, h, &up(3));
        assert_eq!(sb.rows(), 3 * line);
        sb.look(w, h, &up(13));
        assert_eq!(sb.rows(), 13 * line);
        let bytes: usize = sb.strips.iter().map(|s| s.data.len()).sum();
        println!("kept: {} lines in {} bytes ({} a line; the screen as it was: {} bytes)", sb.rows() / line, bytes, bytes * line / sb.rows(), sb.last.len());
        assert_eq!(&sb.view(13 * line, &up(13))[..], &px[..], "scrolled back all the way: the first screen");
    }
}
