//! The Console tab: a VM's screen, keyboard and mouse.
//!
//! XenServer / XCP-ng serve each running VM's screen as VNC (the RFB protocol), reached through an
//! HTTP CONNECT tunnel on the server's HTTPS port, as the original console does (conn.rs opens the
//! tunnel). This module is a small RFB 3.3 to 3.8 client: it draws the screen into a picture the
//! window shows, and sends keys and mouse moves back. Keys go as PC scancodes when the server takes
//! them (QEMU's extended key events), so the VM's own keyboard layout applies, as with a real
//! keyboard; otherwise as X11 keysyms. The demo server's VMs show a pretend text console instead.

use crate::app::{dialog_button, Look};
use crate::conn::{Connection, ConsoleStream};
use crate::icons::paint_glyph;
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Align2, Color32, Event, FontId, Key, Modifiers, Rect, Rounding, Sense, Stroke, Ui};
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// The id of the screen widget: while it has keyboard focus, keys go to the VM and the window's own
/// shortcuts are off.
pub fn screen_id() -> egui::Id {
    egui::Id::new("vm-console-screen")
}

pub fn has_focus(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(screen_id()))
}

const RETRY_AFTER: Duration = Duration::from_secs(3); // a VM's screen goes away for a moment while it reboots
const MAX_RETRIES: u32 = 10;
const SCREEN_BG: Color32 = Color32::from_rgb(12, 12, 14);

// ---------------------------------------------------------------- shared between window and worker

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Failed(String),
}

struct Screen {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    dirty: bool,
}

impl Screen {
    fn new(width: usize, height: usize) -> Screen {
        let mut rgba = vec![0u8; width * height * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Screen { width, height, rgba, dirty: true }
    }
    fn put(&mut self, x: usize, y: usize, px: [u8; 4]) {
        if x < self.width && y < self.height {
            let i = (y * self.width + x) * 4;
            self.rgba[i..i + 4].copy_from_slice(&px);
        }
    }
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, px: [u8; 4]) {
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.width) {
                let i = (row * self.width + col) * 4;
                self.rgba[i..i + 4].copy_from_slice(&px);
            }
        }
    }
}

struct Shared {
    status: Status,
    screen: Option<Screen>,
    scancodes: bool,
}

enum Input {
    Key { down: bool, keysym: u32, scancode: u32 },
    Pointer { buttons: u8, x: u16, y: u16 },
}

// ---------------------------------------------------------------- the worker thread

/// One connection to a VM's screen, run by a background thread. Dropping it ends the connection.
struct Remote {
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::Sender<Input>,
    stop: Arc<AtomicBool>,
}

impl Remote {
    fn start(conn: Arc<dyn Connection>, vm_ref: String, ctx: egui::Context) -> Remote {
        let shared = Arc::new(Mutex::new(Shared { status: Status::Connecting, screen: None, scancodes: false }));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (sh, st) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            let r = run(conn.as_ref(), &vm_ref, &sh, &rx, &st, &ctx);
            if !st.load(Ordering::Relaxed) {
                let msg = r.err().unwrap_or_else(|| "The console connection was closed.".into());
                sh.lock().unwrap().status = Status::Failed(msg);
                ctx.request_repaint();
            }
        });
        Remote { shared, tx, stop }
    }

    fn send(&self, i: Input) {
        let _ = self.tx.send(i);
    }

    fn status(&self) -> Status {
        self.shared.lock().unwrap().status.clone()
    }

    fn scancodes(&self) -> bool {
        self.shared.lock().unwrap().scancodes
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn io_err(e: std::io::Error) -> String {
    match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => "The console connection was closed.".into(),
        ErrorKind::TimedOut | ErrorKind::WouldBlock => "The console stopped answering.".into(),
        _ => format!("The console connection broke ({}).", e),
    }
}

fn run(conn: &dyn Connection, vm_ref: &str, shared: &Mutex<Shared>, rx: &mpsc::Receiver<Input>, stop: &AtomicBool, ctx: &egui::Context) -> Result<(), String> {
    let mut s = conn.open_console(vm_ref).map_err(|e| e.message())?;
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    s.set_read_timeout(Some(Duration::from_secs(15))).map_err(io_err)?;
    let (w, h) = handshake(&mut s)?;
    {
        let mut sh = shared.lock().unwrap();
        sh.screen = Some(Screen::new(w as usize, h as usize));
        sh.status = Status::Connected;
    }
    ctx.request_repaint();
    s.write_all(&set_pixel_format()).map_err(io_err)?;
    s.write_all(&set_encodings()).map_err(io_err)?;
    s.write_all(&update_request(false, w, h)).map_err(io_err)?;
    s.flush().map_err(io_err)?;

    // From here on reads wait at most a few milliseconds, so keys and mouse moves go out promptly
    // between pieces of screen (the TLS stream can't be read and written from two threads).
    s.set_read_timeout(Some(Duration::from_millis(8))).map_err(io_err)?;
    let mut inb = InBuf::default();
    let mut parser = Parser::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut wrote = false;
        let scancodes = shared.lock().unwrap().scancodes;
        while let Ok(i) = rx.try_recv() {
            s.write_all(&encode_input(i, scancodes)).map_err(io_err)?;
            wrote = true;
        }
        if wrote {
            s.flush().map_err(io_err)?;
        }
        match s.read(&mut chunk) {
            Ok(0) => return Err("The console connection was closed.".into()),
            Ok(n) => inb.data.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
            Err(e) => return Err(io_err(e)),
        }
        loop {
            match parser.step(&mut inb, shared)? {
                Step::NeedMore => break,
                Step::Progress => {}
                Step::UpdateDone => {
                    let (w, h) = shared.lock().unwrap().screen.as_ref().map(|sc| (sc.width as u16, sc.height as u16)).unwrap_or((w, h));
                    let full = std::mem::take(&mut parser.resized);
                    s.write_all(&update_request(!full, w, h)).map_err(io_err)?;
                    s.flush().map_err(io_err)?;
                    ctx.request_repaint();
                }
            }
        }
    }
}

// ---------------------------------------------------------------- RFB messages

fn read_n(s: &mut ConsoleStream, n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).map_err(io_err)?;
    Ok(b)
}
fn read_u32(s: &mut ConsoleStream) -> Result<u32, String> {
    let b = read_n(s, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
fn read_reason(s: &mut ConsoleStream) -> String {
    let len = read_u32(s).unwrap_or(0).min(4096) as usize;
    let text = read_n(s, len).map(|b| String::from_utf8_lossy(&b).trim().to_string()).unwrap_or_default();
    if text.is_empty() {
        "The console refused the connection.".into()
    } else {
        format!("The console refused the connection: {}", text)
    }
}

/// Version and security handshake, then ClientInit / ServerInit. Returns the screen size.
fn handshake(s: &mut ConsoleStream) -> Result<(u16, u16), String> {
    let v = read_n(s, 12)?;
    if !v.starts_with(b"RFB ") {
        return Err("The server didn't answer like a VM console.".into());
    }
    let num = |b: &[u8]| std::str::from_utf8(b).ok().and_then(|t| t.parse::<u32>().ok()).unwrap_or(0);
    let (major, minor) = (num(&v[4..7]), num(&v[8..11]));
    let minor = if major > 3 || minor >= 8 { 8 } else if minor == 7 { 7 } else { 3 };
    s.write_all(format!("RFB 003.{:03}\n", minor).as_bytes()).map_err(io_err)?;
    s.flush().map_err(io_err)?;
    const NO_PASSWORD: u8 = 1;
    if minor == 3 {
        match read_u32(s)? {
            0 => return Err(read_reason(s)),
            1 => {}
            _ => return Err("The console asks for a password, which this app doesn't support.".into()),
        }
    } else {
        let n = read_n(s, 1)?[0] as usize;
        if n == 0 {
            return Err(read_reason(s));
        }
        if !read_n(s, n)?.contains(&NO_PASSWORD) {
            return Err("The console asks for a password, which this app doesn't support.".into());
        }
        s.write_all(&[NO_PASSWORD]).map_err(io_err)?;
        s.flush().map_err(io_err)?;
        if minor == 8 && read_u32(s)? != 0 {
            return Err(read_reason(s));
        }
    }
    s.write_all(&[1]).map_err(io_err)?; // shared: don't disconnect other viewers
    s.flush().map_err(io_err)?;
    let init = read_n(s, 24)?;
    let (w, h) = (u16::from_be_bytes([init[0], init[1]]), u16::from_be_bytes([init[2], init[3]]));
    let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]).min(1 << 16) as usize;
    read_n(s, name_len)?;
    Ok((w, h))
}

/// 32 bits per pixel, little-endian, red in bits 16-23, green 8-15, blue 0-7: bytes arrive as B G R x.
fn set_pixel_format() -> Vec<u8> {
    vec![0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
}

const ENC_RAW: i32 = 0;
const ENC_COPYRECT: i32 = 1;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;
const ENC_QEMU_KEYS: i32 = -258;

fn set_encodings() -> Vec<u8> {
    // The server uses the first one in this list that it knows.
    let encs = [ENC_ZRLE, ENC_COPYRECT, ENC_RAW, ENC_DESKTOP_SIZE, ENC_QEMU_KEYS];
    let mut m = vec![2, 0];
    m.extend_from_slice(&(encs.len() as u16).to_be_bytes());
    for e in encs {
        m.extend_from_slice(&e.to_be_bytes());
    }
    m
}

fn update_request(incremental: bool, w: u16, h: u16) -> Vec<u8> {
    let mut m = vec![3, incremental as u8, 0, 0, 0, 0];
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    m
}

fn encode_input(i: Input, scancodes: bool) -> Vec<u8> {
    match i {
        Input::Key { down, keysym, scancode } if scancodes && scancode != 0 => {
            let mut m = vec![255, 0, 0, down as u8];
            m.extend_from_slice(&keysym.to_be_bytes());
            m.extend_from_slice(&scancode.to_be_bytes());
            m
        }
        Input::Key { down, keysym, .. } => {
            if keysym == 0 {
                return vec![];
            }
            let mut m = vec![4, down as u8, 0, 0];
            m.extend_from_slice(&keysym.to_be_bytes());
            m
        }
        Input::Pointer { buttons, x, y } => {
            let mut m = vec![5, buttons];
            m.extend_from_slice(&x.to_be_bytes());
            m.extend_from_slice(&y.to_be_bytes());
            m
        }
    }
}

// ---------------------------------------------------------------- reading the server's messages

/// Bytes received but not handled yet. Messages are only taken once they've fully arrived.
#[derive(Default)]
struct InBuf {
    data: Vec<u8>,
    pos: usize,
}

impl InBuf {
    fn avail(&self) -> &[u8] {
        &self.data[self.pos..]
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
        if self.pos >= self.data.len() {
            self.data.clear();
            self.pos = 0;
        } else if self.pos > (1 << 20) {
            self.data.drain(..self.pos);
            self.pos = 0;
        }
    }
}

enum Step {
    NeedMore,
    Progress,
    UpdateDone,
}

struct Parser {
    rects_left: Option<u16>,
    zlib: flate2::Decompress, // one zlib stream for the whole connection, as ZRLE requires
    resized: bool,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Parser {
    fn new() -> Parser {
        Parser { rects_left: None, zlib: flate2::Decompress::new(true), resized: false }
    }

    fn step(&mut self, inb: &mut InBuf, shared: &Mutex<Shared>) -> Result<Step, String> {
        let a = inb.avail();
        let Some(left) = self.rects_left else {
            let Some(&kind) = a.first() else { return Ok(Step::NeedMore) };
            let need = match kind {
                0 => 4, // FramebufferUpdate header
                1 if a.len() >= 6 => 6 + 6 * be16(&a[4..6]) as usize, // colour map: unused with true colour
                1 => 6,
                2 => 1, // bell
                3 if a.len() >= 8 => 8 + be32(&a[4..8]) as usize, // the VM's clipboard
                3 => 8,
                other => return Err(format!("The console sent something this app doesn't understand (message {}).", other)),
            };
            if a.len() < need || (kind == 1 && a.len() < 6) || (kind == 3 && a.len() < 8) {
                return Ok(Step::NeedMore);
            }
            if kind == 0 {
                let n = be16(&a[2..4]);
                inb.consume(4);
                if n == 0 {
                    return Ok(Step::UpdateDone);
                }
                self.rects_left = Some(n);
            } else {
                inb.consume(need);
            }
            return Ok(Step::Progress);
        };

        if a.len() < 12 {
            return Ok(Step::NeedMore);
        }
        let (x, y, w, h) = (be16(&a[0..2]) as usize, be16(&a[2..4]) as usize, be16(&a[4..6]) as usize, be16(&a[6..8]) as usize);
        let enc = be32(&a[8..12]) as i32;
        let body = match enc {
            ENC_RAW => w * h * 4,
            ENC_COPYRECT => 4,
            ENC_DESKTOP_SIZE | ENC_QEMU_KEYS => 0,
            ENC_ZRLE if a.len() >= 16 => 4 + be32(&a[12..16]) as usize,
            ENC_ZRLE => return Ok(Step::NeedMore),
            other => return Err(format!("The console used a screen format this app doesn't understand ({}).", other)),
        };
        if a.len() < 12 + body {
            return Ok(Step::NeedMore);
        }
        let data = &a[12..12 + body];
        let inflated = if enc == ENC_ZRLE { Some(inflate(&mut self.zlib, &data[4..])?) } else { None };
        {
            let mut sh = shared.lock().unwrap();
            match enc {
                ENC_QEMU_KEYS => sh.scancodes = true,
                ENC_DESKTOP_SIZE => {
                    sh.screen = Some(Screen::new(w, h));
                    self.resized = true;
                }
                _ => {
                    let Some(sc) = sh.screen.as_mut() else { return Err("The console sent a picture before its size.".into()) };
                    match enc {
                        ENC_RAW => {
                            for row in 0..h {
                                for col in 0..w {
                                    let i = (row * w + col) * 4;
                                    sc.put(x + col, y + row, [data[i + 2], data[i + 1], data[i], 255]);
                                }
                            }
                        }
                        ENC_COPYRECT => copy_rect(sc, be16(&data[0..2]) as usize, be16(&data[2..4]) as usize, x, y, w, h),
                        _ => zrle(sc, inflated.as_deref().unwrap_or(&[]), x, y, w, h)?,
                    }
                    sc.dirty = true;
                }
            }
        }
        inb.consume(12 + body);
        if left <= 1 {
            self.rects_left = None;
            Ok(Step::UpdateDone)
        } else {
            self.rects_left = Some(left - 1);
            Ok(Step::Progress)
        }
    }
}

fn copy_rect(sc: &mut Screen, sx: usize, sy: usize, x: usize, y: usize, w: usize, h: usize) {
    let mut tmp = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        for col in 0..w {
            let (cx, cy) = (sx + col, sy + row);
            if cx < sc.width && cy < sc.height {
                let i = (cy * sc.width + cx) * 4;
                tmp.extend_from_slice(&sc.rgba[i..i + 4]);
            } else {
                tmp.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    for row in 0..h {
        for col in 0..w {
            let i = (row * w + col) * 4;
            sc.put(x + col, y + row, [tmp[i], tmp[i + 1], tmp[i + 2], 255]);
        }
    }
}

fn inflate(z: &mut flate2::Decompress, input: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "The console sent a damaged picture.".to_string();
    let mut out: Vec<u8> = Vec::with_capacity(input.len() * 4 + 4096);
    let mut pos = 0;
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(64 * 1024));
        }
        let (in0, out0) = (z.total_in(), out.len());
        z.decompress_vec(&input[pos..], &mut out, flate2::FlushDecompress::Sync).map_err(|_| bad())?;
        pos += (z.total_in() - in0) as usize;
        let progressed = z.total_in() != in0 || out.len() != out0;
        if (pos >= input.len() && out.len() < out.capacity()) || !progressed {
            return Ok(out);
        }
    }
}

struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn u8(&mut self) -> Result<u8, String> {
        let b = *self.d.get(self.p).ok_or("The console sent a damaged picture.")?;
        self.p += 1;
        Ok(b)
    }
    /// A ZRLE "compressed pixel": 3 bytes B G R with this pixel format.
    fn cpixel(&mut self) -> Result<[u8; 4], String> {
        let b = self.d.get(self.p..self.p + 3).ok_or("The console sent a damaged picture.")?;
        self.p += 3;
        Ok([b[2], b[1], b[0], 255])
    }
    fn run_length(&mut self) -> Result<usize, String> {
        let mut len = 1;
        loop {
            let b = self.u8()?;
            len += b as usize;
            if b != 255 {
                return Ok(len);
            }
        }
    }
}

/// ZRLE: 64x64 tiles, each raw, one colour, a small palette, or run-length coded.
fn zrle(sc: &mut Screen, d: &[u8], x: usize, y: usize, w: usize, h: usize) -> Result<(), String> {
    let mut c = Cur { d, p: 0 };
    for ty in (0..h).step_by(64) {
        for tx in (0..w).step_by(64) {
            let (tw, th) = ((w - tx).min(64), (h - ty).min(64));
            let (ox, oy) = (x + tx, y + ty);
            let sub = c.u8()?;
            match sub {
                0 => {
                    for j in 0..th {
                        for i in 0..tw {
                            let px = c.cpixel()?;
                            sc.put(ox + i, oy + j, px);
                        }
                    }
                }
                1 => {
                    let px = c.cpixel()?;
                    sc.fill(ox, oy, tw, th, px);
                }
                2..=16 => {
                    let pal = (0..sub).map(|_| c.cpixel()).collect::<Result<Vec<_>, _>>()?;
                    let bits = match sub {
                        2 => 1,
                        3 | 4 => 2,
                        _ => 4,
                    };
                    for j in 0..th {
                        let (mut byte, mut left) = (0u8, 0);
                        for i in 0..tw {
                            if left == 0 {
                                byte = c.u8()?;
                                left = 8;
                            }
                            left -= bits;
                            let idx = ((byte >> left) & ((1 << bits) - 1)) as usize;
                            sc.put(ox + i, oy + j, *pal.get(idx).ok_or("The console sent a damaged picture.")?);
                        }
                    }
                }
                128 | 130..=255 => {
                    let pal = if sub == 128 { vec![] } else { (0..sub - 128).map(|_| c.cpixel()).collect::<Result<Vec<_>, _>>()? };
                    let (total, mut n) = (tw * th, 0);
                    while n < total {
                        let (px, len) = if sub == 128 {
                            let px = c.cpixel()?;
                            (px, c.run_length()?)
                        } else {
                            let b = c.u8()?;
                            let px = *pal.get((b & 127) as usize).ok_or("The console sent a damaged picture.")?;
                            (px, if b & 128 != 0 { c.run_length()? } else { 1 })
                        };
                        for _ in 0..len.min(total - n) {
                            sc.put(ox + n % tw, oy + n / tw, px);
                            n += 1;
                        }
                    }
                }
                _ => return Err("The console sent a damaged picture.".into()),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- keys

// (keysym, scancode) pairs
const SHIFT: (u32, u32) = (0xffe1, 0x2a);
const CTRL: (u32, u32) = (0xffe3, 0x1d);
const ALT: (u32, u32) = (0xffe9, 0x38);
const SUPER: (u32, u32) = (0xffeb, 0xdb);
const DELETE: (u32, u32) = (0xffff, 0xd3);
const ESCAPE: (u32, u32) = (0xff1b, 0x01);
const TAB: (u32, u32) = (0xff09, 0x0f);
const F4: (u32, u32) = (0xffc1, 0x3e);

/// Key combinations the desktop keeps for itself (the Windows key, Alt+Tab…) and the window never
/// sees, so the Console tab sends them from its "Send keys" menu.
const KEY_MENU: [(&str, &[(u32, u32)]); 8] = [
    ("Windows key (Start menu)", &[SUPER]),
    ("Alt+Tab", &[ALT, TAB]),
    ("Alt+F4", &[ALT, F4]),
    ("Ctrl+Shift+Esc (Task Manager)", &[CTRL, SHIFT, ESCAPE]),
    ("Windows+R (Run)", &[SUPER, (0x72, 0x13)]),
    ("Windows+E (File Explorer)", &[SUPER, (0x65, 0x12)]),
    ("Windows+D (Desktop)", &[SUPER, (0x64, 0x20)]),
    ("Windows+L (Lock)", &[SUPER, (0x6c, 0x26)]),
];

/// F1 to F12 as (keysym, scancode).
fn function_key(n: usize) -> (u32, u32) {
    (0xffbe + n as u32 - 1, if n <= 10 { 0x3a + n as u32 } else { 0x57 + n as u32 - 11 })
}

/// X11 keysym of a key that doesn't type a character.
fn special_keysym(k: Key) -> Option<u32> {
    use Key::*;
    Some(match k {
        Escape => 0xff1b,
        Tab => 0xff09,
        Backspace => 0xff08,
        Enter => 0xff0d,
        Insert => 0xff63,
        Delete => 0xffff,
        Home => 0xff50,
        End => 0xff57,
        PageUp => 0xff55,
        PageDown => 0xff56,
        ArrowLeft => 0xff51,
        ArrowUp => 0xff52,
        ArrowRight => 0xff53,
        ArrowDown => 0xff54,
        F1 => 0xffbe,
        F2 => 0xffbf,
        F3 => 0xffc0,
        F4 => 0xffc1,
        F5 => 0xffc2,
        F6 => 0xffc3,
        F7 => 0xffc4,
        F8 => 0xffc5,
        F9 => 0xffc6,
        F10 => 0xffc7,
        F11 => 0xffc8,
        F12 => 0xffc9,
        _ => return None,
    })
}

/// The character a key types without Shift on a US keyboard (sent with Ctrl or Alt held, when no
/// text arrives for it).
fn key_char(k: Key) -> Option<char> {
    use Key::*;
    let name = k.name();
    if name.len() == 1 {
        return name.chars().next().map(|c| c.to_ascii_lowercase());
    }
    Some(match k {
        Space => ' ',
        Minus => '-',
        Equals => '=',
        OpenBracket => '[',
        CloseBracket => ']',
        Backslash => '\\',
        Semicolon => ';',
        Quote => '\'',
        Backtick => '`',
        Comma => ',',
        Period => '.',
        Slash => '/',
        _ => return None,
    })
}

fn char_keysym(c: char) -> u32 {
    let n = c as u32;
    if (0x20..0x7f).contains(&n) || (0xa0..0x100).contains(&n) {
        n
    } else {
        0x0100_0000 | n
    }
}

/// The PC (XT set 1) scancode at a key's position; keys after an 0xE0 prefix have the top bit set,
/// as QEMU's extended key events expect.
fn scancode(k: Key) -> Option<u32> {
    use Key::*;
    Some(match k {
        Escape => 0x01,
        Num1 => 0x02,
        Num2 => 0x03,
        Num3 => 0x04,
        Num4 => 0x05,
        Num5 => 0x06,
        Num6 => 0x07,
        Num7 => 0x08,
        Num8 => 0x09,
        Num9 => 0x0a,
        Num0 => 0x0b,
        Minus => 0x0c,
        Equals => 0x0d,
        Backspace => 0x0e,
        Tab => 0x0f,
        Q => 0x10,
        W => 0x11,
        E => 0x12,
        R => 0x13,
        T => 0x14,
        Y => 0x15,
        U => 0x16,
        I => 0x17,
        O => 0x18,
        P => 0x19,
        OpenBracket => 0x1a,
        CloseBracket => 0x1b,
        Enter => 0x1c,
        A => 0x1e,
        S => 0x1f,
        D => 0x20,
        F => 0x21,
        G => 0x22,
        H => 0x23,
        J => 0x24,
        K => 0x25,
        L => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backtick => 0x29,
        Backslash => 0x2b,
        Z => 0x2c,
        X => 0x2d,
        C => 0x2e,
        V => 0x2f,
        B => 0x30,
        N => 0x31,
        M => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        Space => 0x39,
        F1 => 0x3b,
        F2 => 0x3c,
        F3 => 0x3d,
        F4 => 0x3e,
        F5 => 0x3f,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        F11 => 0x57,
        F12 => 0x58,
        Home => 0xc7,
        ArrowUp => 0xc8,
        PageUp => 0xc9,
        ArrowLeft => 0xcb,
        ArrowRight => 0xcd,
        End => 0xcf,
        ArrowDown => 0xd0,
        PageDown => 0xd1,
        Insert => 0xd2,
        Delete => 0xd3,
        _ => return None,
    })
}

/// The keyboard follows the mouse: it goes to the VM while the pointer is over its screen (or on a
/// click or drag there), and back to the window when the pointer leaves, unless a mouse button is
/// still held. Hovering doesn't take it from a text box being typed in; a click does. With a dialog
/// open (`allow` false) the VM never has it.
fn follow_pointer(ui: &Ui, resp: &egui::Response, focused: bool, allow: bool) {
    if !allow {
        if focused {
            ui.memory_mut(|m| m.surrender_focus(screen_id()));
        }
        return;
    }
    let typing_elsewhere = ui.memory(|m| m.focused().map_or(false, |id| id != screen_id()));
    if resp.clicked() || resp.drag_started() || (!focused && !typing_elsewhere && resp.hovered()) {
        resp.request_focus();
    }
    if focused {
        // Tab, arrows and Escape belong to the VM while it has the keyboard.
        ui.memory_mut(|m| m.set_focus_lock_filter(screen_id(), egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
        if !resp.hovered() && !resp.dragged() {
            ui.memory_mut(|m| m.surrender_focus(screen_id()));
        }
    }
}

const ICON_BUTTON: egui::Vec2 = vec2(30.0, 22.0);

// ---------------------------------------------------------------- the demo server's pretend console

#[derive(PartialEq)]
enum DemoStage {
    Login,
    Password,
    Shell,
}

/// A text console with a login prompt, so the demo VMs have something to type into.
struct DemoTerm {
    host: String,
    banner: String,
    lines: Vec<String>,
    input: String,
    user: String,
    stage: DemoStage,
}

impl DemoTerm {
    fn new(vm_name: &str, os: &str) -> DemoTerm {
        let host: String = vm_name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let banner = format!("{} {} tty1", if os.is_empty() { "Linux" } else { os }, host);
        DemoTerm { host, lines: vec![banner.clone(), String::new()], banner, input: String::new(), user: String::new(), stage: DemoStage::Login }
    }

    fn prompt(&self) -> String {
        match self.stage {
            DemoStage::Login => format!("{} login: ", self.host),
            DemoStage::Password => "Password: ".into(),
            DemoStage::Shell => format!("{}@{}:~$ ", self.user, self.host),
        }
    }

    fn type_text(&mut self, t: &str) {
        self.input.extend(t.chars().filter(|c| !c.is_control()));
    }

    fn enter(&mut self) {
        let input = std::mem::take(&mut self.input);
        let shown = if self.stage == DemoStage::Password { String::new() } else { input.clone() };
        self.lines.push(format!("{}{}", self.prompt(), shown));
        match self.stage {
            DemoStage::Login if input.trim().is_empty() => {}
            DemoStage::Login => {
                self.user = input.trim().to_string();
                self.stage = DemoStage::Password;
            }
            DemoStage::Password => {
                self.lines.push(String::new());
                self.lines.push("Welcome! This is the demo server: any password works here.".into());
                self.lines.push("Type 'help' to see what this pretend VM understands.".into());
                self.lines.push(String::new());
                self.stage = DemoStage::Shell;
            }
            DemoStage::Shell => match input.trim() {
                "" => {}
                "help" => self.lines.push("help  uptime  ls  whoami  clear  logout".into()),
                "uptime" => self.lines.push(" 10:21:03 up 12 days,  3:04,  1 user,  load average: 0.08, 0.05, 0.01".into()),
                "ls" => self.lines.push("backups  notes.txt  www".into()),
                "whoami" => self.lines.push(self.user.clone()),
                "clear" => self.lines.clear(),
                "logout" | "exit" => {
                    self.lines = vec![String::new(), self.banner.clone(), String::new()];
                    self.stage = DemoStage::Login;
                }
                cmd => self.lines.push(format!("{}: command not found", cmd.split_whitespace().next().unwrap_or(cmd))),
            },
        }
    }
}

// ---------------------------------------------------------------- the tab

pub struct ConsoleView {
    pub sid: u32,
    pub vm: String,
    conn: Arc<dyn Connection>,
    ctx: egui::Context,
    remote: Option<Remote>,
    demo: Option<DemoTerm>,
    texture: Option<egui::TextureHandle>,
    tex_size: [usize; 2],
    fit: bool,
    sent_mods: Modifiers,
    skip_text: bool,
    pointer: Option<(u8, u16, u16)>,
    retry_at: Option<Instant>,
    retries: u32,
    was_connected: bool,
    last_pass: u64,
    wake_pending: bool,
    /// No room for the controls next to the tabs: they float over the screen's corner instead.
    overlay_controls: bool,
}

impl ConsoleView {
    pub fn new(sid: u32, vm: String, vm_name: &str, os: &str, conn: Arc<dyn Connection>, ctx: egui::Context) -> ConsoleView {
        let demo = conn.is_demo() && crate::conn::debug_console_address().is_none();
        let mut v = ConsoleView {
            sid,
            vm,
            conn,
            ctx,
            remote: None,
            demo: if demo { Some(DemoTerm::new(vm_name, os)) } else { None },
            texture: None,
            tex_size: [0, 0],
            fit: true,
            sent_mods: Modifiers::NONE,
            skip_text: false,
            pointer: None,
            retry_at: None,
            retries: 0,
            was_connected: false,
            last_pass: 0,
            wake_pending: true,
            overlay_controls: false,
        };
        if !demo {
            v.connect();
        }
        v
    }

    fn connect(&mut self) {
        self.remote = Some(Remote::start(self.conn.clone(), self.vm.clone(), self.ctx.clone()));
        self.retry_at = None;
    }

    fn status(&self) -> Status {
        match &self.remote {
            Some(r) => r.status(),
            None if self.demo.is_some() => Status::Connected,
            None => Status::Connecting,
        }
    }

    /// `allow_keyboard` is false while a dialog is open: then the VM doesn't take the keyboard.
    pub fn ui(&mut self, ui: &mut Ui, pal: &Pal, allow_keyboard: bool) {
        let status = self.status();
        if status == Status::Connected {
            self.was_connected = true;
            self.retries = 0;
        }
        // Each time the screen comes into view (the tab opened, or another VM picked), tap Ctrl in
        // the VM: it wakes a display that went dark to save power (Windows turns its screen off
        // when idle) and does nothing else.
        let pass = ui.ctx().cumulative_pass_nr();
        if pass > self.last_pass + 1 {
            self.wake_pending = true;
        }
        self.last_pass = pass;
        if self.wake_pending && status == Status::Connected && self.remote.is_some() {
            self.wake_pending = false;
            self.tap(CTRL.0, CTRL.1);
        }
        // After a reboot the VM's screen comes back on a new connection: try again by itself.
        if let Status::Failed(_) = &status {
            if self.was_connected && self.retries < MAX_RETRIES {
                let at = *self.retry_at.get_or_insert_with(|| Instant::now() + RETRY_AFTER);
                if Instant::now() >= at {
                    self.retries += 1;
                    self.connect();
                } else {
                    ui.ctx().request_repaint_after(at - Instant::now());
                }
            }
        }
        let focused = ui.memory(|m| m.has_focus(screen_id()));

        ui.add_space(8.0);

        let area = ui.available_rect_before_wrap();
        let area = Rect::from_min_max(area.min, pos2(area.max.x, area.max.y - 10.0));
        ui.painter().rect_filled(area, Rounding::same(4.0), SCREEN_BG);

        if let Some(demo) = self.demo.as_mut() {
            Self::demo_screen(ui, area, demo, focused, allow_keyboard);
            ui.allocate_rect(area, Sense::hover());
            self.overlays(ui, area, pal, &status);
            return;
        }
        match &status {
            Status::Connecting => {
                ui.painter().text(area.center(), Align2::CENTER_CENTER, "Connecting…", FontId::proportional(15.0), Color32::from_gray(170));
                ui.allocate_rect(area, Sense::hover());
                return;
            }
            Status::Failed(msg) if self.retry_at.is_none() => {
                let msg = msg.clone();
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(area.shrink(24.0)), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((area.height() / 2.0 - 60.0).max(0.0));
                        ui.label(egui::RichText::new("Couldn't show the VM's console").size(16.0).color(Color32::from_gray(225)));
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(msg).color(Color32::from_gray(170)));
                        ui.add_space(12.0);
                        if dialog_button(ui, pal, "retry", "Try again", Look::Plain, true) {
                            self.retries = 0;
                            self.connect();
                        }
                    });
                });
                ui.allocate_rect(area, Sense::hover());
                return;
            }
            _ => {}
        }
        self.upload(ui.ctx());
        let Some(tex) = self.texture.as_ref().map(|t| t.id()) else {
            ui.allocate_rect(area, Sense::hover());
            return;
        };
        let size = vec2(self.tex_size[0] as f32, self.tex_size[1] as f32);
        if self.fit {
            let s = (area.width() / size.x).min(area.height() / size.y);
            let img = Rect::from_center_size(area.center(), size * s);
            let resp = ui.interact(img, screen_id(), Sense::click_and_drag());
            ui.painter().image(tex, img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            ui.allocate_rect(area, Sense::hover());
            self.input(ui, &resp, img, s, focused, allow_keyboard);
        } else {
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(area), |ui| {
                egui::ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
                    let (img, _) = ui.allocate_exact_size(size, Sense::hover());
                    let resp = ui.interact(img, screen_id(), Sense::click_and_drag());
                    ui.painter().image(tex, img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                    self.input(ui, &resp, img, 1.0, focused, allow_keyboard);
                });
            });
        }
        if focused {
            ui.painter().rect_stroke(area, Rounding::same(4.0), Stroke::new(1.5, pal.accent));
        }
        self.overlays(ui, area, pal, &status);
    }

    /// Over the screen: a note while the console reconnects, and the controls when the tab row
    /// had no room for them.
    fn overlays(&mut self, ui: &mut Ui, area: Rect, pal: &Pal, status: &Status) {
        if matches!(status, Status::Failed(_)) && self.retry_at.is_some() {
            let text = "The VM's screen went away (it may be restarting). Connecting again…";
            let g = ui.painter().layout(text.into(), FontId::proportional(13.0), Color32::from_gray(235), area.width() - 60.0);
            let r = Rect::from_center_size(pos2(area.center().x, area.min.y + 30.0), g.size() + vec2(24.0, 12.0));
            ui.painter().rect_filled(r, Rounding::same(6.0), Color32::from_black_alpha(200));
            ui.painter().galley(r.min + vec2(12.0, 6.0), g, Color32::from_gray(235));
        }
        if self.overlay_controls {
            let r = Rect::from_min_max(pos2(area.max.x - 80.0, area.min.y + 6.0), pos2(area.max.x - 6.0, area.min.y + 32.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::right_to_left(egui::Align::Center)));
            self.controls(&mut child, pal, true);
        }
    }

    /// The console's controls in the row of tabs, right-aligned in `free`: with their names when
    /// there's room, otherwise as two icons (a keyboard for the keys, a monitor for Scale to fit),
    /// or, without room even for those, over the screen's corner.
    pub fn tab_controls(&mut self, ui: &mut Ui, free: Rect, pal: &Pal) {
        let text_w = |t: &str| ui.painter().layout_no_wrap(t.to_string(), egui::TextStyle::Button.resolve(ui.style()), pal.ink).size().x;
        // each: 10 + icon 16 + 6 + label + 10, Send keys its arrow too, and 4 between them
        let full = text_w("Send Ctrl+Alt+Del") + text_w("Send keys") + text_w("Scale to fit") + 3.0 * 42.0 + 16.0 + 2.0 * 4.0;
        let icons = 2.0 * ICON_BUTTON.x + 8.0;
        self.overlay_controls = free.width() < icons;
        if !self.overlay_controls {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
            self.controls(&mut child, pal, free.width() < full);
        }
    }

    fn controls(&mut self, ui: &mut Ui, pal: &Pal, compact: bool) {
        ui.spacing_mut().item_spacing.x = 4.0;
        let connected = self.status() == Status::Connected;
        let mut combo: Option<Vec<(u32, u32)>> = None;
        let menu = |ui: &mut Ui, combo: &mut Option<Vec<(u32, u32)>>, with_cad: bool| {
            ui.set_min_width(230.0);
            if with_cad && ui.button("Send Ctrl+Alt+Del").clicked() {
                *combo = Some(vec![CTRL, ALT, DELETE]);
                ui.close_menu();
            }
            if with_cad {
                ui.separator();
            }
            for (label, keys) in KEY_MENU {
                if ui.button(label).clicked() {
                    *combo = Some(keys.to_vec());
                    ui.close_menu();
                }
            }
            ui.separator();
            ui.menu_button("Linux text consoles", |ui| {
                for n in 1..=12 {
                    if ui.button(format!("Ctrl+Alt+F{}", n)).clicked() {
                        *combo = Some(vec![CTRL, ALT, function_key(n)]);
                        ui.close_menu();
                    }
                }
            });
        };
        // Flat buttons: no frame, a soft background while hovered, while their menu is open, or (the
        // monitor) while Scale to fit is on. The background goes into a slot reserved under the button.
        let flat_bg = |ui: &Ui, slot: egui::layers::ShapeIdx, resp: &egui::Response, on: bool| {
            let hovered = resp.hovered() && resp.enabled();
            if hovered || on {
                let fill = if hovered { pal.hover } else { pal.nav_on };
                ui.painter().set(slot, egui::epaint::RectShape::filled(resp.rect, Rounding::same(5.0), fill));
            }
        };
        ui.spacing_mut().button_padding = vec2(8.0, 3.0);
        let row = vec2(0.0, 24.0);
        if compact {
            // a monitor: Scale to fit; a keyboard: every key combination
            let slot = ui.painter().add(egui::Shape::Noop);
            let b = ui.add(egui::Button::new("").frame(false).min_size(ICON_BUTTON)).on_hover_text(if self.fit { "Scale to fit: on" } else { "Scale to fit: off" });
            flat_bg(ui, slot, &b, self.fit);
            paint_glyph(ui.painter(), b.rect, if self.fit { "fit" } else { "screen" }, pal.ink_soft);
            if b.clicked() {
                self.fit = !self.fit;
            }
            ui.add_enabled_ui(connected, |ui| {
                let slot = ui.painter().add(egui::Shape::Noop);
                let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(ICON_BUTTON), |ui| menu(ui, &mut combo, true));
                flat_bg(ui, slot, &m.response, m.inner.is_some());
                paint_glyph(ui.painter(), m.response.rect, "keyboard", if connected { pal.ink_soft } else { pal.ink_faint });
                m.response.on_hover_text("Send keys to the VM");
            });
        } else {
            // egui gives frameless buttons no padding, so these draw their own icon and label inside a
            // sized, empty button: 10 points each side, and room for the menu's arrow.
            let font = egui::TextStyle::Button.resolve(ui.style());
            let label = |ui: &Ui, r: Rect, glyph: &str, text: &str, arrow: bool, enabled: bool| {
                let (col, soft) = if enabled { (pal.ink, pal.ink_soft) } else { (pal.ink_faint, pal.ink_faint) };
                paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.min.x + 18.0, r.center().y), vec2(16.0, 16.0)), glyph, soft);
                ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, text, font.clone(), col);
                if arrow {
                    let c = pos2(r.max.x - 13.0, r.center().y);
                    ui.painter().add(egui::Shape::line(vec![pos2(c.x - 3.5, c.y - 1.5), pos2(c.x, c.y + 2.0), pos2(c.x + 3.5, c.y - 1.5)], Stroke::new(1.4, soft)));
                }
            };
            let width = |ui: &Ui, text: &str| ui.painter().layout_no_wrap(text.to_string(), font.clone(), pal.ink).size().x + 42.0;
            // Scale to fit: on while its background is lit
            let slot = ui.painter().add(egui::Shape::Noop);
            let b = ui.add(egui::Button::new("").frame(false).min_size(vec2(width(ui, "Scale to fit"), row.y))).on_hover_text(if self.fit { "Scale to fit: on" } else { "Scale to fit: off" });
            flat_bg(ui, slot, &b, self.fit);
            label(ui, b.rect, if self.fit { "fit" } else { "screen" }, "Scale to fit", false, true);
            if b.clicked() {
                self.fit = !self.fit;
            }
            ui.add_enabled_ui(connected, |ui| {
                let slot = ui.painter().add(egui::Shape::Noop);
                let size = vec2(width(ui, "Send keys") + 16.0, row.y);
                let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(size), |ui| menu(ui, &mut combo, false));
                flat_bg(ui, slot, &m.response, m.inner.is_some());
                label(ui, m.response.rect, "keyboard", "Send keys", true, connected);
                let slot = ui.painter().add(egui::Shape::Noop);
                let b = ui.add(egui::Button::new("").frame(false).min_size(vec2(width(ui, "Send Ctrl+Alt+Del"), row.y)));
                flat_bg(ui, slot, &b, false);
                label(ui, b.rect, "lock", "Send Ctrl+Alt+Del", false, connected);
                if b.clicked() {
                    combo = Some(vec![CTRL, ALT, DELETE]);
                }
            });
        }
        if let Some(keys) = combo {
            self.send_combo(&keys);
        }
    }

    /// Copies a changed screen into the texture the window draws.
    fn upload(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.remote else { return };
        let img = {
            let mut sh = r.shared.lock().unwrap();
            match sh.screen.as_mut() {
                Some(sc) if sc.dirty || self.texture.is_none() => {
                    sc.dirty = false;
                    Some(egui::ColorImage::from_rgba_unmultiplied([sc.width, sc.height], &sc.rgba))
                }
                _ => None,
            }
        };
        if let Some(img) = img {
            self.tex_size = img.size;
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("vm-console", img, egui::TextureOptions::LINEAR)),
            }
        }
    }

    fn send_key(&self, down: bool, keysym: u32, scancode: u32) {
        if let Some(r) = &self.remote {
            r.send(Input::Key { down, keysym, scancode });
        }
    }

    fn tap(&self, keysym: u32, scancode: u32) {
        self.send_key(true, keysym, scancode);
        self.send_key(false, keysym, scancode);
    }

    /// Presses the keys in order, then lets go of them in reverse.
    fn send_combo(&mut self, keys: &[(u32, u32)]) {
        if let Some(d) = self.demo.as_mut() {
            d.lines.push("(The demo VM ignores key combinations.)".into());
            return;
        }
        for &(k, s) in keys {
            self.send_key(true, k, s);
        }
        for &(k, s) in keys.iter().rev() {
            self.send_key(false, k, s);
        }
    }

    /// Presses or lets go of Shift, Ctrl and Alt so the VM's state matches the real keyboard's.
    fn sync_mods(&mut self, m: Modifiers) {
        for (want, had, key) in [(m.shift, self.sent_mods.shift, SHIFT), (m.ctrl, self.sent_mods.ctrl, CTRL), (m.alt, self.sent_mods.alt, ALT)] {
            if want != had {
                self.send_key(want, key.0, key.1);
            }
        }
        self.sent_mods = Modifiers { shift: m.shift, ctrl: m.ctrl, alt: m.alt, ..Modifiers::NONE };
    }

    /// Keyboard and mouse from the window to the VM.
    fn input(&mut self, ui: &Ui, resp: &egui::Response, img: Rect, scale: f32, focused: bool, allow: bool) {
        follow_pointer(ui, resp, focused, allow);
        let (events, mods, pos, buttons) = ui.input(|i| {
            let p = &i.pointer;
            let buttons = (p.button_down(egui::PointerButton::Primary) as u8) | (p.button_down(egui::PointerButton::Middle) as u8) << 1 | (p.button_down(egui::PointerButton::Secondary) as u8) << 2;
            (i.events.clone(), i.modifiers, p.latest_pos(), buttons)
        });
        if !focused {
            if self.sent_mods != Modifiers::NONE {
                self.sync_mods(Modifiers::NONE); // never leave Shift or Ctrl held down in the VM
            }
        } else {
            let scancodes = self.remote.as_ref().map_or(false, |r| r.scancodes());
            for e in events {
                match e {
                    Event::Key { key, physical_key, pressed, modifiers, .. } => {
                        self.sync_mods(modifiers);
                        let keysym = special_keysym(key).or_else(|| key_char(key).map(char_keysym)).unwrap_or(0);
                        if scancodes {
                            // Scancodes: the VM's keyboard layout turns them into characters, so the
                            // text this window would type for the key isn't sent as well.
                            match physical_key.and_then(scancode) {
                                Some(code) => {
                                    self.send_key(pressed, keysym, code);
                                    if pressed {
                                        self.skip_text = true;
                                    }
                                }
                                None if pressed => self.skip_text = false,
                                None => {}
                            }
                        } else if special_keysym(key).is_some() || modifiers.ctrl || modifiers.alt {
                            // Keysyms: characters arrive as text; other keys and shortcuts go here.
                            self.send_key(pressed, keysym, 0);
                        }
                    }
                    Event::Text(t) => {
                        if std::mem::take(&mut self.skip_text) {
                            continue;
                        }
                        for ch in t.chars() {
                            self.tap(char_keysym(ch), 0);
                        }
                    }
                    // The window turns Ctrl+C / X / V into copy, cut and paste; the VM gets the keys.
                    Event::Copy => self.tap('c' as u32, if scancodes { 0x2e } else { 0 }),
                    Event::Cut => self.tap('x' as u32, if scancodes { 0x2d } else { 0 }),
                    Event::Paste(_) => self.tap('v' as u32, if scancodes { 0x2f } else { 0 }),
                    _ => {}
                }
            }
            self.sync_mods(mods);
        }

        // The mouse: where it is over the screen, its buttons, and the wheel as buttons 4 and 5.
        let over = resp.hovered() || resp.dragged();
        if let (true, Some(p)) = (over, pos) {
            let x = ((p.x - img.min.x) / scale).clamp(0.0, self.tex_size[0].saturating_sub(1) as f32) as u16;
            let y = ((p.y - img.min.y) / scale).clamp(0.0, self.tex_size[1].saturating_sub(1) as f32) as u16;
            if self.pointer != Some((buttons, x, y)) {
                self.pointer = Some((buttons, x, y));
                if let Some(r) = &self.remote {
                    r.send(Input::Pointer { buttons, x, y });
                }
            }
            let wheel = ui.input(|i| i.raw_scroll_delta.y);
            if wheel != 0.0 {
                let b = if wheel > 0.0 { 1 << 3 } else { 1 << 4 };
                if let Some(r) = &self.remote {
                    r.send(Input::Pointer { buttons: buttons | b, x, y });
                    r.send(Input::Pointer { buttons, x, y });
                }
            }
        }
    }

    /// The demo console: an 80x25 text screen.
    fn demo_screen(ui: &mut Ui, area: Rect, demo: &mut DemoTerm, focused: bool, allow: bool) {
        let size = vec2(720.0, 400.0);
        let s = (area.width() / size.x).min(area.height() / size.y);
        let img = Rect::from_center_size(area.center(), size * s);
        let resp = ui.interact(img, screen_id(), Sense::click());
        follow_pointer(ui, &resp, focused, allow);
        if focused {
            for e in ui.input(|i| i.events.clone()) {
                match e {
                    Event::Text(t) => demo.type_text(&t),
                    Event::Key { key: Key::Enter, pressed: true, .. } => demo.enter(),
                    Event::Key { key: Key::Backspace, pressed: true, .. } => {
                        demo.input.pop();
                    }
                    _ => {}
                }
            }
        }
        let p = ui.painter().with_clip_rect(img);
        p.rect_filled(img, 0.0, Color32::BLACK);
        let font = FontId::monospace(16.0 * s * 0.9);
        let line_h = img.height() / 25.0;
        let mut lines: Vec<String> = demo.lines.clone();
        let typed = if demo.stage == DemoStage::Password { String::new() } else { demo.input.clone() };
        let cursor = if focused && (ui.input(|i| i.time) * 2.0) as i64 % 2 == 0 { "_" } else { " " };
        lines.push(format!("{}{}{}", demo.prompt(), typed, cursor));
        let first = lines.len().saturating_sub(24);
        for (i, l) in lines[first..].iter().enumerate() {
            p.text(pos2(img.min.x + 8.0 * s, img.min.y + 4.0 * s + i as f32 * line_h), Align2::LEFT_TOP, l, font.clone(), Color32::from_gray(200));
        }
        if focused {
            ui.ctx().request_repaint_after(Duration::from_millis(500)); // the blinking cursor
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(s: &mut ConsoleStream, inb: &mut InBuf, parser: &mut Parser, shared: &Mutex<Shared>, seen: &mut Vec<i32>, dur: Duration) {
        let end = Instant::now() + dur;
        let mut chunk = vec![0u8; 256 * 1024];
        while Instant::now() < end {
            match s.read(&mut chunk) {
                Ok(0) => panic!("the VNC server closed the connection"),
                Ok(n) => inb.data.extend_from_slice(&chunk[..n]),
                Err(_) => {}
            }
            loop {
                if parser.rects_left.is_some() && inb.avail().len() >= 12 {
                    let e = be32(&inb.avail()[8..12]) as i32;
                    if !seen.contains(&e) {
                        seen.push(e);
                    }
                }
                match parser.step(inb, shared).unwrap() {
                    Step::NeedMore => break,
                    Step::Progress => {}
                    Step::UpdateDone => {
                        let (w, h) = shared.lock().unwrap().screen.as_ref().map(|sc| (sc.width as u16, sc.height as u16)).unwrap();
                        let full = std::mem::take(&mut parser.resized);
                        s.write_all(&update_request(!full, w, h)).unwrap();
                    }
                }
            }
        }
    }

    fn save(shared: &Mutex<Shared>, path: &str) {
        let sh = shared.lock().unwrap();
        let sc = sh.screen.as_ref().unwrap();
        image::RgbaImage::from_raw(sc.width as u32, sc.height as u32, sc.rgba.clone()).unwrap().save(path).unwrap();
    }

    /// Against a real VNC server, e.g. `qemu-system-x86_64 -display none -vnc 127.0.0.1:7 -boot
    /// menu=on,splash-time=60000`: SVM_TEST_VNC=127.0.0.1:5907 SVM_TEST_OUT=<dir>
    /// [SVM_TEST_KEYS=keysyms] cargo test -- --ignored. Presses Escape, which opens SeaBIOS's boot
    /// menu, and saves the screen before and after.
    #[test]
    #[ignore]
    fn live_vnc_server() {
        let addr = std::env::var("SVM_TEST_VNC").expect("SVM_TEST_VNC=host:port");
        let out = std::env::var("SVM_TEST_OUT").unwrap_or_else(|_| ".".into());
        let keysyms = std::env::var("SVM_TEST_KEYS").map_or(false, |k| k == "keysyms");
        let mut s = ConsoleStream::Plain(std::net::TcpStream::connect(&addr).unwrap());
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (w, h) = handshake(&mut s).unwrap();
        let shared = Mutex::new(Shared { status: Status::Connected, screen: Some(Screen::new(w as usize, h as usize)), scancodes: false });
        s.write_all(&set_pixel_format()).unwrap();
        s.write_all(&set_encodings()).unwrap();
        s.write_all(&update_request(false, w, h)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(8))).unwrap();
        let (mut inb, mut parser, mut seen) = (InBuf::default(), Parser::new(), vec![]);
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(1500));
        save(&shared, &format!("{}/vnc-before.png", out));
        let scancodes = shared.lock().unwrap().scancodes && !keysyms;
        for down in [true, false] {
            s.write_all(&encode_input(Input::Key { down, keysym: 0xff1b, scancode: if keysyms { 0 } else { 0x01 } }, scancodes)).unwrap();
        }
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(1500));
        save(&shared, &format!("{}/vnc-after.png", out));
        println!("screen {}x{}, encodings seen {:?}, scancodes {}", w, h, seen, scancodes);
    }

    #[test]
    fn raw_and_copyrect() {
        let shared = Mutex::new(Shared { status: Status::Connected, screen: Some(Screen::new(4, 2)), scancodes: false });
        let mut inb = InBuf::default();
        // one update, two rectangles: a 2x1 raw picture at (0,0), then a copy of it to (2,1)
        inb.data.extend_from_slice(&[0, 0, 0, 2]);
        inb.data.extend_from_slice(&[0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0]);
        inb.data.extend_from_slice(&[30, 20, 10, 0, 3, 2, 1, 0]);
        inb.data.extend_from_slice(&[0, 2, 0, 1, 0, 2, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0]);
        let mut p = Parser::new();
        let mut steps = 0;
        loop {
            match p.step(&mut inb, &shared).unwrap() {
                Step::UpdateDone => break,
                Step::NeedMore => panic!("ran out of bytes"),
                Step::Progress => steps += 1,
            }
        }
        assert_eq!(steps, 2);
        let sh = shared.lock().unwrap();
        let px = |x: usize, y: usize| sh.screen.as_ref().unwrap().rgba[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(px(0, 0), vec![10, 20, 30, 255]);
        assert_eq!(px(1, 0), vec![1, 2, 3, 255]);
        assert_eq!(px(2, 1), vec![10, 20, 30, 255]);
        assert_eq!(px(3, 1), vec![1, 2, 3, 255]);
    }

    #[test]
    fn function_keys() {
        assert_eq!(function_key(1), (0xffbe, 0x3b));
        assert_eq!(function_key(10), (0xffc7, 0x44));
        assert_eq!(function_key(11), (0xffc8, 0x57));
        assert_eq!(function_key(12), (0xffc9, 0x58));
        assert_eq!(function_key(4), F4);
    }

    #[test]
    fn partial_message_waits() {
        let shared = Mutex::new(Shared { status: Status::Connected, screen: Some(Screen::new(2, 1)), scancodes: false });
        let mut inb = InBuf::default();
        let mut p = Parser::new();
        inb.data.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 9, 9]);
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::Progress));
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::NeedMore));
        inb.data.extend_from_slice(&[9, 0, 8, 8, 8, 0]);
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::UpdateDone));
    }
}
