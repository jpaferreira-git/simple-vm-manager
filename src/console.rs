//! The Console tab: a VM's screen, keyboard and mouse.
//!
//! XenServer / XCP-ng serve each running VM's screen as VNC (the RFB protocol), reached through an
//! HTTP CONNECT tunnel on the server's HTTPS port, as the original console does (conn.rs opens the
//! tunnel). This module is a small RFB 3.3 to 3.8 client: it draws the screen into a picture the
//! window shows, and sends keys and mouse moves back. Keys go as PC scancodes when the server takes
//! them (QEMU's extended key events), so the VM's own keyboard layout applies, as with a real
//! keyboard; otherwise as X11 keysyms. The demo server's VMs show a pretend text console instead.
//!
//! The clipboard: text a console puts on its clipboard (RFB ServerCutText: a server's text console
//! when text is selected with the mouse, a Windows VM with XenServer VM Tools when something is
//! copied there) comes onto this computer's. This computer's goes to a Windows VM's (ClientCutText,
//! which XenServer's QEMU hands its VM Tools), so Ctrl+V pastes it there; anywhere else it can be
//! typed, a key for each character.

use crate::app::{dialog_button, Look};
use crate::conn::{Connection, ConsoleStream};
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
    /// Text the console put on its clipboard, for this computer's.
    cut_text: Option<String>,
    /// Text being typed: the characters typed so far, of how many.
    typing: Option<(usize, usize)>,
}

impl Shared {
    fn new(status: Status, screen: Option<Screen>) -> Shared {
        Shared { status, screen, scancodes: false, cut_text: None, typing: None }
    }
}

/// A key pressed (true) or let go, as its keysym and its scancode (0: none).
type KeyEv = (bool, u32, u32);

enum Input {
    Key { down: bool, keysym: u32, scancode: u32 },
    Pointer { buttons: u8, x: u16, y: u16 },
    /// This computer's clipboard, for the VM's.
    CutText(String),
    /// Keys sent a group at a time (a character's, say), `gap` apart; `shown` while it lasts (its
    /// progress over the screen, with Stop).
    Type { groups: Vec<Vec<KeyEv>>, gap: Duration, shown: bool },
    StopTyping,
}

/// Keys waiting to be sent, a group at a time, so a VM's keyboard (QEMU's PS/2 queue holds a few
/// keys only) takes every one.
#[derive(Default)]
struct Typer {
    left: std::collections::VecDeque<Vec<KeyEv>>,
    done: usize,
    total: usize,
    gap: Duration,
    shown: bool,
    next: Option<Instant>,
}

impl Typer {
    fn add(&mut self, groups: Vec<Vec<KeyEv>>, gap: Duration, shown: bool) {
        if self.left.is_empty() {
            (self.done, self.total, self.next) = (0, 0, None);
        }
        self.total += groups.len();
        self.left.extend(groups);
        self.gap = gap;
        self.shown = shown;
    }

    fn stop(&mut self) {
        self.left.clear();
    }

    /// The groups due by `now` (a few hundred at most, so the screen still comes between).
    fn due(&mut self, now: Instant) -> Vec<Vec<KeyEv>> {
        let mut out = vec![];
        while out.len() < 300 {
            let at = *self.next.get_or_insert(now);
            if at > now {
                break;
            }
            let Some(g) = self.left.pop_front() else { break };
            out.push(g);
            self.done += 1;
            // after a slow moment, no burst to catch up: the gap again from now
            self.next = Some(if at + self.gap < now { now + self.gap } else { at + self.gap });
        }
        out
    }

    fn progress(&self) -> Option<(usize, usize)> {
        (self.shown && !self.left.is_empty()).then_some((self.done, self.total))
    }
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
        let shared = Arc::new(Mutex::new(Shared::new(Status::Connecting, None)));
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
    let mut typer = Typer::default();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut wrote = false;
        let scancodes = shared.lock().unwrap().scancodes;
        while let Ok(i) = rx.try_recv() {
            match i {
                Input::Type { groups, gap, shown } => typer.add(groups, gap, shown),
                Input::StopTyping => typer.stop(),
                i => {
                    s.write_all(&encode_input(i, scancodes)).map_err(io_err)?;
                    wrote = true;
                }
            }
        }
        for g in typer.due(Instant::now()) {
            for (down, keysym, scancode) in g {
                s.write_all(&encode_input(Input::Key { down, keysym, scancode }, scancodes)).map_err(io_err)?;
            }
            wrote = true;
        }
        let progress = typer.progress();
        let mut sh = shared.lock().unwrap();
        if sh.typing != progress {
            sh.typing = progress;
            ctx.request_repaint();
        }
        drop(sh);
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

// ---------------------------------------------------------------- a look at the screen

/// One look at a VM's screen, for its Overview: connects (as a shared viewer, so a console open
/// elsewhere stays open), takes the whole screen once and leaves. With `wake`, it first taps Ctrl
/// in the VM, as the Console tab does, to wake a display that went dark to save power, and gives it
/// a moment to come back; otherwise it sends the VM nothing. The picture comes scaled down to
/// `max_w` pixels wide at most: its width, height and RGBA bytes.
pub fn snapshot(conn: &dyn Connection, vm_ref: &str, max_w: usize, wake: bool) -> Result<(usize, usize, Vec<u8>), String> {
    let mut s = conn.open_console(vm_ref).map_err(|e| e.message())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(io_err)?;
    let (w, h) = handshake(&mut s)?;
    let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
    s.write_all(&set_pixel_format()).map_err(io_err)?;
    s.write_all(&set_encodings()).map_err(io_err)?;
    if wake {
        for down in [true, false] {
            s.write_all(&encode_input(Input::Key { down, keysym: CTRL.0, scancode: CTRL.1 }, false)).map_err(io_err)?;
        }
        s.flush().map_err(io_err)?;
        std::thread::sleep(Duration::from_millis(2500));
    }
    s.write_all(&update_request(false, w, h)).map_err(io_err)?;
    s.flush().map_err(io_err)?;
    let (mut inb, mut parser, mut chunk) = (InBuf::default(), Parser::new(), vec![0u8; 256 * 1024]);
    let until = Instant::now() + Duration::from_secs(15);
    // (a screen that changes size first says so, then needs asking again; and news without any of
    // the screen, such as the server saying it takes scancodes, can come before it)
    let mut asks = 0;
    loop {
        match parser.step(&mut inb, &shared)? {
            Step::Progress => continue,
            Step::UpdateDone if std::mem::take(&mut parser.resized) && asks < 3 => {
                asks += 1;
                let (w, h) = shared.lock().unwrap().screen.as_ref().map(|sc| (sc.width as u16, sc.height as u16)).unwrap_or((w, h));
                s.write_all(&update_request(false, w, h)).map_err(io_err)?;
                s.flush().map_err(io_err)?;
                continue;
            }
            Step::UpdateDone if parser.painted => break,
            Step::UpdateDone => continue,
            Step::NeedMore => {}
        }
        if Instant::now() > until {
            return Err("Its screen took too long to come.".into());
        }
        match s.read(&mut chunk) {
            Ok(0) => return Err("The console connection was closed.".into()),
            Ok(n) => inb.data.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(io_err(e)),
        }
    }
    let screen = shared.into_inner().unwrap().screen.ok_or("No screen came.")?;
    Ok(shrink(&screen.rgba, screen.width, screen.height, max_w))
}

/// A picture made smaller (each pixel the average of the ones it covers) to `max_w` wide at most.
fn shrink(rgba: &[u8], w: usize, h: usize, max_w: usize) -> (usize, usize, Vec<u8>) {
    if w <= max_w || w == 0 || h == 0 {
        return (w, h, rgba.to_vec());
    }
    let (nw, nh) = (max_w, ((h * max_w) as f64 / w as f64).round().max(1.0) as usize);
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1));
            let mut sum = [0u32; 3];
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let i = (yy * w + xx) * 4;
                    for c in 0..3 {
                        sum[c] += rgba[i + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (y * nw + x) * 4;
            out[o..o + 4].copy_from_slice(&[(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255]);
        }
    }
    (nw, nh, out)
}

/// The demo server's VMs' screen in small (their pretend text console, waiting for a login), in
/// `r`.
pub fn paint_demo_screen(p: &egui::Painter, r: Rect, vm_name: &str, os: &str) {
    let demo = DemoTerm::new(vm_name, os);
    let size = vec2(720.0, 400.0);
    let s = (r.width() / size.x).min(r.height() / size.y);
    let img = Rect::from_center_size(r.center(), size * s);
    let p = p.with_clip_rect(img);
    p.rect_filled(img, 0.0, Color32::BLACK);
    let line_h = img.height() / 25.0;
    let font = FontId::monospace(16.0 * s * 0.9);
    let mut lines = demo.lines.clone();
    lines.push(format!("{}_", demo.prompt()));
    for (i, l) in lines.iter().enumerate() {
        p.text(pos2(img.min.x + 6.0 * s, img.min.y + 6.0 * s + i as f32 * line_h), Align2::LEFT_TOP, l, font.clone(), Color32::from_gray(200));
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
        return Err("The server didn't answer like a console.".into());
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
        Input::CutText(t) => {
            // Latin-1, as RFB has it: what's beyond it becomes "?"
            let bytes: Vec<u8> = t.chars().map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' }).collect();
            let mut m = vec![6, 0, 0, 0];
            m.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            m.extend_from_slice(&bytes);
            m
        }
        Input::Type { .. } | Input::StopTyping => vec![],
    }
}

/// The text of a console's clipboard as it sent it: UTF-8 when it is, else Latin-1 (RFB's).
fn cut_text(b: &[u8]) -> String {
    let t = match std::str::from_utf8(b) {
        Ok(t) => t.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    };
    t.trim_end_matches('\0').replace("\r\n", "\n")
}

/// The keys that type `text`, a group for each character, and how many characters were left out
/// (only plain ASCII can be typed: QEMU's keyboard map and the server's text console have nothing
/// else). On a VM's keyboard a capital or a symbol gets Shift, as on a US keyboard; the server's
/// text console takes each character as it is.
fn typing_keys(text: &str, host: bool) -> (Vec<Vec<KeyEv>>, usize) {
    let mut out = vec![];
    let mut left_out = 0;
    for ch in text.chars() {
        let (keysym, scancode) = match ch {
            '\n' => (0xff0d, 0x1c),
            '\t' => (0xff09, 0x0f),
            c if c.is_control() => continue,
            c if !c.is_ascii() => {
                left_out += 1;
                continue;
            }
            c => (char_keysym(c), 0),
        };
        let tap = vec![(true, keysym, scancode), (false, keysym, scancode)];
        let shift = !host && (ch.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(ch));
        out.push(if shift { [vec![(true, SHIFT.0, SHIFT.1)], tap, vec![(false, SHIFT.0, SHIFT.1)]].concat() } else { tap });
    }
    (out, left_out)
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
    /// Some of the screen has come (not only news such as its size).
    painted: bool,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Parser {
    fn new() -> Parser {
        Parser { rects_left: None, zlib: flate2::Decompress::new(true), resized: false, painted: false }
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
                if kind == 3 {
                    let text = cut_text(&a[8..need]);
                    if !text.trim().is_empty() {
                        shared.lock().unwrap().cut_text = Some(text);
                    }
                }
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
                    self.painted = true;
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

/// A mouse wheel event's turn in steps (lines): positive up. A touchpad's comes in points, 40 a
/// step, as egui scrolls a line.
fn wheel_steps(e: &Event) -> f32 {
    match e {
        Event::MouseWheel { unit, delta, .. } => match unit {
            egui::MouseWheelUnit::Line => delta.y,
            egui::MouseWheelUnit::Point => delta.y / 40.0,
            egui::MouseWheelUnit::Page => delta.y * 10.0,
        },
        _ => 0.0,
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
                self.lines.push("Type 'help' to see what this pretend system understands.".into());
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
    sent_mods: Modifiers,
    skip_text: bool,
    pointer: Option<(u8, u16, u16)>,
    retry_at: Option<Instant>,
    retries: u32,
    was_connected: bool,
    last_pass: u64,
    wake_pending: bool,
    /// A server's own text console (vncterm), not a VM's screen.
    host: bool,
    /// A Windows VM: its XenServer VM Tools take this computer's clipboard, and give theirs.
    shares_clipboard: bool,
    /// Settings' "Share the clipboard with the consoles": off, nothing copied there comes here
    /// and no VM gets this computer's clipboard (typing it, asked for, still works).
    pub share_clipboard: bool,
    /// The mouse wheel's turn not sent yet (a part of a step).
    wheel: f32,
    /// A paste asked of the window (it comes as a Paste event a frame later), and frames waited.
    paste: Option<(Paste, u8)>,
    /// This frame's Paste event was that one.
    paste_taken: bool,
    /// Text to type, once no key is held any more (Ctrl+Shift+V's would reach the VM otherwise).
    to_type: Option<String>,
    /// The clipboard as last given to the VM, and when the connection came (the VM may tell its
    /// own clipboard as it connects: that isn't a copy).
    shared_clip: Option<String>,
    connected_at: Option<Instant>,
    was_focused: bool,
    window_focused: bool,
    /// A short note over the screen ("Copied…"), until then.
    note: Option<(String, Instant)>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Paste {
    /// To give the VM (a Windows VM's clipboard).
    Share,
    /// To type, a key for each character.
    Type,
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
            sent_mods: Modifiers::NONE,
            skip_text: false,
            pointer: None,
            retry_at: None,
            retries: 0,
            was_connected: false,
            last_pass: 0,
            wake_pending: true,
            host: os.starts_with("XenServer "), // what the app passes for a server's console
            shares_clipboard: os.to_lowercase().contains("windows"),
            share_clipboard: true,
            wheel: 0.0,
            paste: None,
            paste_taken: false,
            to_type: None,
            shared_clip: None,
            connected_at: None,
            was_focused: false,
            window_focused: true,
            note: None,
        };
        if !demo {
            v.connect();
        }
        v
    }

    fn connect(&mut self) {
        self.remote = Some(Remote::start(self.conn.clone(), self.vm.clone(), self.ctx.clone()));
        self.retry_at = None;
        self.connected_at = None;
        self.shared_clip = None;
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
        self.clipboard(ui, &status, focused);

        ui.add_space(8.0);

        let area = ui.available_rect_before_wrap();
        let area = Rect::from_min_max(area.min, pos2(area.max.x, area.max.y - 10.0));
        ui.painter().rect_filled(area, Rounding::same(4.0), SCREEN_BG);

        if let Some(demo) = self.demo.as_mut() {
            Self::demo_screen(ui, area, demo, focused, allow_keyboard, self.paste_taken);
            ui.allocate_rect(area, Sense::hover());
            self.overlays(ui, pal, area, &status);
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
                        ui.label(egui::RichText::new("Couldn't show the console").size(16.0).color(Color32::from_gray(225)));
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
        // always scaled to fit the room there is
        let s = (area.width() / size.x).min(area.height() / size.y);
        let img = Rect::from_center_size(area.center(), size * s);
        let resp = ui.interact(img, screen_id(), Sense::click_and_drag());
        ui.painter().image(tex, img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        ui.allocate_rect(area, Sense::hover());
        self.input(ui, &resp, img, s, focused, allow_keyboard);
        if focused {
            ui.painter().rect_stroke(area, Rounding::same(4.0), Stroke::new(1.5_f32, pal.accent));
        }
        self.overlays(ui, pal, area, &status);
    }

    /// Over the screen: a note while the console reconnects, a short one after a copy, and the
    /// clipboard's typing as it goes, with Stop.
    fn overlays(&mut self, ui: &mut Ui, pal: &Pal, area: Rect, status: &Status) {
        let pill = |ui: &mut Ui, text: &str, y: f32| {
            let g = ui.painter().layout(text.into(), FontId::proportional(13.0), Color32::from_gray(235), area.width() - 60.0);
            let r = Rect::from_center_size(pos2(area.center().x, y), g.size() + vec2(24.0, 12.0));
            ui.painter().rect_filled(r, Rounding::same(6.0), Color32::from_black_alpha(200));
            ui.painter().galley(r.min + vec2(12.0, 6.0), g, Color32::from_gray(235));
        };
        if matches!(status, Status::Failed(_)) && self.retry_at.is_some() {
            pill(ui, "The screen went away (it may be restarting). Connecting again…", area.min.y + 30.0);
        } else if let Some((text, until)) = &self.note {
            if Instant::now() < *until {
                pill(ui, text, area.min.y + 30.0);
                ui.ctx().request_repaint_after(*until - Instant::now());
            } else {
                self.note = None;
            }
        }
        let typing = self.remote.as_ref().and_then(|r| r.shared.lock().unwrap().typing);
        let typing = typing.or_else(|| std::env::var("SVM_SHOT_TYPING").ok().map(|_| (312, 1480))); // for --screenshot
        if let Some((done, total)) = typing {
            let r = Rect::from_center_size(pos2(area.center().x, area.max.y - 34.0), vec2(330.0_f32.min(area.width() - 20.0), 40.0));
            ui.painter().rect_filled(r, Rounding::same(8.0), Color32::from_black_alpha(215));
            let mut stop = false;
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(r.shrink2(vec2(12.0, 6.0))), |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new(format!("Typing the clipboard… {} of {}", done, total)).color(Color32::from_gray(235)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        stop = dialog_button(ui, pal, "close", "Stop", Look::Plain, true);
                    });
                });
            });
            if stop {
                if let Some(rem) = &self.remote {
                    rem.send(Input::StopTyping);
                }
            }
        }
    }

    /// The clipboard, each frame: what the console copied comes onto this computer's; a Windows
    /// VM gets this computer's as its screen takes the keyboard (or the window comes back to the
    /// front); a paste asked of the window is taken; and text waiting to be typed goes once no
    /// key is held.
    fn clipboard(&mut self, ui: &Ui, status: &Status, focused: bool) {
        let ctx = ui.ctx().clone();
        let connected = *status == Status::Connected;
        if connected && self.connected_at.is_none() {
            self.connected_at = Some(Instant::now());
        }
        if let Some(r) = &self.remote {
            let copied = r.shared.lock().unwrap().cut_text.take();
            let settled = self.connected_at.map_or(false, |t| t.elapsed() > Duration::from_secs(2));
            if let Some(text) = copied.filter(|t| self.share_clipboard && settled && Some(t) != self.shared_clip.as_ref()) {
                ctx.copy_text(text.clone());
                self.shared_clip = Some(text); // the same text isn't given back to it
                self.show_note("Copied to this computer's clipboard");
            }
        }
        let window_focused = ui.input(|i| i.focused);
        if self.shares_clipboard && self.share_clipboard && connected && focused && (!self.was_focused || (window_focused && !self.window_focused)) {
            self.ask_paste(&ctx, Paste::Share);
        }
        (self.was_focused, self.window_focused) = (focused, window_focused);

        self.paste_taken = false;
        if let Some((mode, waited)) = self.paste.take() {
            let text = ui.input(|i| i.events.iter().find_map(|e| if let Event::Paste(t) = e { Some(t.clone()) } else { None }));
            match text {
                Some(t) => {
                    self.paste_taken = true;
                    match mode {
                        Paste::Share => {
                            self.share(&t);
                        }
                        Paste::Type => self.type_text_later(t),
                    }
                }
                None if waited < 3 => {
                    self.paste = Some((mode, waited + 1));
                    ctx.request_repaint();
                }
                None if mode == Paste::Type => self.show_note("The clipboard has no text to type."),
                None => {}
            }
        }
        if self.to_type.is_some() && ui.input(|i| i.keys_down.is_empty() && i.modifiers.is_none()) {
            let text = self.to_type.take().unwrap_or_default();
            self.type_now(&text);
        }
    }

    fn show_note(&mut self, text: &str) {
        self.note = Some((text.to_string(), Instant::now() + Duration::from_millis(2500)));
        self.ctx.request_repaint();
    }

    /// Asks the window for its clipboard's text; it comes as a Paste event in a frame or two.
    fn ask_paste(&mut self, ctx: &egui::Context, mode: Paste) {
        if self.paste.map_or(true, |(m, _)| m == Paste::Share) {
            self.paste = Some((mode, 0));
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ctx.request_repaint();
        }
    }

    /// Gives a Windows VM this computer's clipboard; whether it was new to it.
    fn share(&mut self, text: &str) -> bool {
        if !self.shares_clipboard || !self.share_clipboard || text.len() > 1_000_000 || self.shared_clip.as_deref() == Some(text) {
            return false;
        }
        let Some(r) = &self.remote else { return false };
        r.send(Input::CutText(text.to_string()));
        self.shared_clip = Some(text.to_string());
        true
    }

    /// Types this computer's clipboard on the console's keyboard (the item in Send keys).
    pub fn type_clipboard(&mut self, ctx: &egui::Context) {
        self.ask_paste(ctx, Paste::Type);
    }

    fn type_text_later(&mut self, text: String) {
        if let Some(d) = self.demo.as_mut() {
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    d.enter();
                }
                d.type_text(line);
            }
            return;
        }
        self.to_type = Some(text);
    }

    fn type_now(&mut self, text: &str) {
        let Some(r) = &self.remote else { return };
        let (groups, left_out) = typing_keys(text, self.host);
        if groups.is_empty() {
            self.show_note("The clipboard has nothing that can be typed.");
            return;
        }
        // a VM's keyboard takes about 80 characters a second safely; the server's console, any
        let gap = if self.host { Duration::ZERO } else { Duration::from_millis(12) };
        r.send(Input::Type { groups, gap, shown: true });
        if left_out > 0 {
            self.show_note(&format!("{} character{} left out: only plain letters, digits and symbols can be typed.", left_out, if left_out == 1 { "" } else { "s" }));
        }
    }

    /// Whether the screen is there to send keys to.
    pub fn connected(&self) -> bool {
        self.demo.is_some() || self.status() == Status::Connected
    }

    /// The keys the desktop keeps for itself, as a menu's items (the keyboard button in the row of
    /// tabs): Ctrl+Alt+Del first, then the other combinations and the Linux text consoles.
    pub fn keys_menu(&mut self, ui: &mut Ui) {
        ui.set_min_width(230.0);
        let mut combo: Option<Vec<(u32, u32)>> = None;
        if ui.add(egui::Button::new("Type the clipboard").shortcut_text("Ctrl+Shift+V")).on_hover_text("Types the text on this computer's clipboard, a key for each character: for a Linux console, an installer, a login screen…").clicked() {
            self.type_clipboard(ui.ctx());
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Send Ctrl+Alt+Del").clicked() {
            combo = Some(vec![CTRL, ALT, DELETE]);
            ui.close_menu();
        }
        ui.separator();
        for (label, keys) in KEY_MENU {
            if ui.button(label).clicked() {
                combo = Some(keys.to_vec());
                ui.close_menu();
            }
        }
        ui.separator();
        ui.menu_button("Linux text consoles", |ui| {
            for n in 1..=12 {
                if ui.button(format!("Ctrl+Alt+F{}", n)).clicked() {
                    combo = Some(vec![CTRL, ALT, function_key(n)]);
                    ui.close_menu();
                }
            }
        });
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
            d.lines.push("(The demo ignores key combinations.)".into());
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
                    Event::Paste(_) if self.paste_taken => {}
                    // Ctrl+Shift+V types the clipboard; on a server's console Ctrl+V does too (it
                    // has no clipboard). A Windows VM gets it first, then Ctrl+V pastes it there,
                    // a moment later when it was new to it (its VM Tools take a moment).
                    Event::Paste(t) if mods.shift || (self.host && self.share_clipboard) => self.type_text_later(t),
                    Event::Paste(t) => {
                        let v = ('v' as u32, if scancodes { 0x2f } else { 0 });
                        if self.share(&t) {
                            let keys = vec![vec![], vec![(true, CTRL.0, CTRL.1), (true, v.0, v.1), (false, v.0, v.1), (false, CTRL.0, CTRL.1)]];
                            if let Some(r) = &self.remote {
                                r.send(Input::Type { groups: keys, gap: Duration::from_millis(300), shown: false });
                            }
                        } else {
                            self.tap(v.0, v.1);
                        }
                    }
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
            // The wheel: a click of buttons 4 or 5 for each step. A server's text console scrolls
            // one line a click through what went by, so it gets three, as a terminal scrolls.
            let steps: f32 = ui.input(|i| i.events.iter().map(wheel_steps).sum());
            self.wheel += steps * if self.host { 3.0 } else { 1.0 };
            let clicks = self.wheel.trunc();
            self.wheel -= clicks;
            if clicks != 0.0 {
                let b = if clicks > 0.0 { 1 << 3 } else { 1 << 4 };
                if let Some(r) = &self.remote {
                    for _ in 0..(clicks.abs() as u32).min(30) {
                        r.send(Input::Pointer { buttons: buttons | b, x, y });
                        r.send(Input::Pointer { buttons, x, y });
                    }
                }
            }
        } else {
            self.wheel = 0.0;
        }
    }

    /// The demo console: an 80x25 text screen.
    fn demo_screen(ui: &mut Ui, area: Rect, demo: &mut DemoTerm, focused: bool, allow: bool, paste_taken: bool) {
        let size = vec2(720.0, 400.0);
        let s = (area.width() / size.x).min(area.height() / size.y);
        let img = Rect::from_center_size(area.center(), size * s);
        let resp = ui.interact(img, screen_id(), Sense::click());
        follow_pointer(ui, &resp, focused, allow);
        if focused {
            for e in ui.input(|i| i.events.clone()) {
                match e {
                    Event::Text(t) => demo.type_text(&t),
                    Event::Paste(t) if !paste_taken => {
                        for (i, line) in t.split('\n').enumerate() {
                            if i > 0 {
                                demo.enter();
                            }
                            demo.type_text(line);
                        }
                    }
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
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
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
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(4, 2))));
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
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(2, 1))));
        let mut inb = InBuf::default();
        let mut p = Parser::new();
        inb.data.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 9, 9]);
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::Progress));
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::NeedMore));
        inb.data.extend_from_slice(&[9, 0, 8, 8, 8, 0]);
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::UpdateDone));
    }

    #[test]
    fn clipboard_both_ways() {
        // the console's clipboard (ServerCutText) is kept for this computer's, once whole
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(2, 1))));
        let mut inb = InBuf::default();
        let mut p = Parser::new();
        inb.data.extend_from_slice(&[3, 0, 0, 0, 0, 0, 0, 9, b'r', b'o', b'o', b't']);
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::NeedMore));
        inb.data.extend_from_slice(b"@tico");
        assert!(matches!(p.step(&mut inb, &shared).unwrap(), Step::Progress));
        assert_eq!(shared.lock().unwrap().cut_text.as_deref(), Some("root@tico"));
        assert_eq!(cut_text(&[b'a', 0xe7, b'\r', b'\n', 0]), "a\u{e7}\n"); // Latin-1 when not UTF-8
        assert_eq!(cut_text("ação".as_bytes()), "ação");
        // this computer's (ClientCutText), in Latin-1
        assert_eq!(encode_input(Input::CutText("ç€".into()), false), vec![6, 0, 0, 0, 0, 0, 0, 2, 0xe7, b'?']);
    }

    #[test]
    fn typing_the_clipboard() {
        let (keys, left_out) = typing_keys("aB!\nç", false);
        assert_eq!(left_out, 1);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0], vec![(true, 'a' as u32, 0), (false, 'a' as u32, 0)]);
        assert_eq!(keys[1], vec![(true, SHIFT.0, SHIFT.1), (true, 'B' as u32, 0), (false, 'B' as u32, 0), (false, SHIFT.0, SHIFT.1)]);
        assert_eq!(keys[2][1], (true, '!' as u32, 0));
        assert_eq!(keys[3], vec![(true, 0xff0d, 0x1c), (false, 0xff0d, 0x1c)]); // Enter
        // the server's text console takes the characters as they are, without Shift
        let (keys, _) = typing_keys("B", true);
        assert_eq!(keys, vec![vec![(true, 'B' as u32, 0), (false, 'B' as u32, 0)]]);
    }

    #[test]
    fn typing_is_paced() {
        let mut t = Typer::default();
        let start = Instant::now();
        t.add(vec![vec![(true, 1, 0)], vec![(true, 2, 0)], vec![(true, 3, 0)]], Duration::from_millis(12), true);
        assert_eq!(t.due(start).len(), 1);
        assert_eq!(t.due(start + Duration::from_millis(5)).len(), 0);
        assert_eq!(t.progress(), Some((1, 3)));
        assert_eq!(t.due(start + Duration::from_millis(12)).len(), 1);
        // after a slow moment, still one at a time
        assert_eq!(t.due(start + Duration::from_millis(500)).len(), 1);
        assert_eq!(t.progress(), None);
        // no gap: all at once; Stop drops the rest
        t.add((0..500).map(|i| vec![(true, i, 0)]).collect(), Duration::ZERO, true);
        assert_eq!(t.due(start).len(), 300);
        t.stop();
        assert_eq!((t.due(start).len(), t.progress()), (0, None));
    }

    #[test]
    fn wheel_in_steps() {
        let ev = |unit, y| Event::MouseWheel { unit, delta: vec2(0.0, y), modifiers: Modifiers::NONE };
        assert_eq!(wheel_steps(&ev(egui::MouseWheelUnit::Line, -1.0)), -1.0);
        assert_eq!(wheel_steps(&ev(egui::MouseWheelUnit::Point, 20.0)), 0.5);
        assert_eq!(wheel_steps(&Event::Copy), 0.0);
    }
}
