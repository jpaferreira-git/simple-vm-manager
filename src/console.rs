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
//! copied there) comes onto this computer's. This computer's never goes to the VM by a key: Ctrl+V
//! and Ctrl+Shift+V reach the VM as keys. It goes only when asked, from the Actions menu: "Type the
//! clipboard" types it, a key for each character; on a Windows VM with VM Tools, "Paste the
//! clipboard" gives it to them (ClientCutText, which XenServer's QEMU hands its VM Tools) and
//! presses Ctrl+V.

use crate::app::{dialog_button, Look};
use crate::conn::{Connection, ConsoleStream};
use crate::scrollback::Scrollback;
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
    /// The screen as the pieces of an update come in.
    rgba: Vec<u8>,
    /// The screen shown: as of the last whole update. An update comes in pieces (a server's
    /// console sends up to 40), and shown one by one they tore the picture.
    frame: Vec<u8>,
    /// Where the pieces painted since the last whole update was shown: x, y, right, bottom.
    damage: Option<(usize, usize, usize, usize)>,
    /// `frame` changed since the window last took it.
    dirty: bool,
}

impl Screen {
    fn new(width: usize, height: usize) -> Screen {
        let mut rgba = vec![0u8; width * height * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Screen { width, height, frame: rgba.clone(), rgba, damage: None, dirty: true }
    }

    /// A piece was painted there.
    fn mark(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let (x1, y1) = ((x + w).min(self.width), (y + h).min(self.height));
        if x >= x1 || y >= y1 {
            return;
        }
        self.damage = Some(match self.damage {
            Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x1), d.max(y1)),
            None => (x, y, x1, y1),
        });
    }

    /// Shows what was painted since it was last shown; whether there was anything.
    fn present(&mut self) -> bool {
        let Some((x0, y0, x1, y1)) = self.damage.take() else { return false };
        for row in y0..y1 {
            let (a, b) = ((row * self.width + x0) * 4, (row * self.width + x1) * 4);
            self.frame[a..b].copy_from_slice(&self.rgba[a..b]);
        }
        self.dirty = true;
        true
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
    /// How the VM takes the mouse, when its console says (QEMU does): true, at a position on the
    /// screen (a USB tablet: the VM's pointer stays under this one); false, as moves (a PS/2 mouse,
    /// whose pointer drifts from this one, so the screen captures the mouse).
    absolute: Option<bool>,
    /// Text the console put on its clipboard, for this computer's.
    cut_text: Option<String>,
    /// Text being typed: the characters typed so far, of how many.
    typing: Option<(usize, usize)>,
    /// What went up a text console's screen, kept for the wheel (a Linux VM's: see scrollback.rs).
    history: Option<Scrollback>,
}

impl Shared {
    fn new(status: Status, screen: Option<Screen>) -> Shared {
        Shared { status, screen, scancodes: false, absolute: None, cut_text: None, typing: None, history: None }
    }
}

/// A key pressed (true) or let go, as its keysym and its scancode (0: none).
type KeyEv = (bool, u32, u32);

enum Input {
    Key { down: bool, keysym: u32, scancode: u32 },
    Pointer { buttons: u8, x: u16, y: u16 },
    /// This computer's clipboard, for the VM's (a Windows VM's VM Tools take it).
    CutText(String),
    /// Keys sent one at a time, `gap` apart, starting `after` from now; a group is a character's
    /// keys, for the progress shown over the screen (with Stop) while it lasts, when `shown`.
    Type { groups: Vec<Vec<KeyEv>>, gap: Duration, after: Duration, shown: bool },
    StopTyping,
}

/// Keys waiting to be sent, one at a time, so the VM takes every one. XenServer's QEMU passes a key
/// on to the VM every 10 ms and drops what comes once about 340 are waiting: a clipboard typed any
/// faster loses keys part way, a Shift let go among them, and the rest comes out scrambled.
#[derive(Default)]
struct Typer {
    /// Each key, and whether it's the last of its character's (for the progress).
    left: std::collections::VecDeque<(KeyEv, bool)>,
    done: usize,
    total: usize,
    gap: Duration,
    shown: bool,
    next: Option<Instant>,
    /// Keys this typing pressed and hasn't let go of yet (Shift across capitals): let go on Stop.
    held: Vec<(u32, u32)>,
}

impl Typer {
    fn add(&mut self, groups: Vec<Vec<KeyEv>>, gap: Duration, after: Duration, shown: bool) {
        if self.left.is_empty() {
            (self.done, self.total) = (0, 0);
            self.next = (after > Duration::ZERO).then(|| Instant::now() + after);
        }
        for g in groups.into_iter().filter(|g| !g.is_empty()) {
            self.total += 1;
            let last = g.len() - 1;
            self.left.extend(g.into_iter().enumerate().map(|(i, k)| (k, i == last)));
        }
        self.gap = gap;
        self.shown = shown;
    }

    /// Drops the keys not sent yet; gives the ones to let go of.
    fn stop(&mut self) -> Vec<KeyEv> {
        self.left.clear();
        self.held.drain(..).map(|(k, s)| (false, k, s)).collect()
    }

    /// The keys due by `now` (a few hundred at most, so the screen still comes between).
    fn due(&mut self, now: Instant) -> Vec<KeyEv> {
        let mut out = vec![];
        while out.len() < 300 {
            let at = *self.next.get_or_insert(now);
            if at > now {
                break;
            }
            let Some((key, last)) = self.left.pop_front() else { break };
            let (down, k, s) = key;
            self.held.retain(|&h| h != (k, s));
            if down {
                self.held.push((k, s));
            }
            out.push(key);
            self.done += last as usize;
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
    /// With `history`, what goes up the screen of a text console is kept, for the wheel.
    fn start(conn: Arc<dyn Connection>, vm_ref: String, ctx: egui::Context, history: bool) -> Remote {
        let mut sh = Shared::new(Status::Connecting, None);
        sh.history = history.then(Scrollback::default);
        let shared = Arc::new(Mutex::new(sh));
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

    /// The VM takes the mouse as moves (a PS/2 mouse), as its console said.
    fn relative(&self) -> bool {
        self.shared.lock().unwrap().absolute == Some(false)
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
    let mut shower = Shower::default();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if shower.due(Instant::now()) {
            show(&mut shared.lock().unwrap());
            ctx.request_repaint();
        }
        let mut wrote = false;
        let scancodes = shared.lock().unwrap().scancodes;
        while let Ok(i) = rx.try_recv() {
            match i {
                Input::Type { groups, gap, after, shown } => typer.add(groups, gap, after, shown),
                Input::StopTyping => {
                    for (down, keysym, scancode) in typer.stop() {
                        s.write_all(&encode_input(Input::Key { down, keysym, scancode }, scancodes)).map_err(io_err)?;
                        wrote = true;
                    }
                }
                i => {
                    if matches!(i, Input::Key { down: true, .. }) {
                        key_pressed(shared);
                    }
                    s.write_all(&encode_input(i, scancodes)).map_err(io_err)?;
                    wrote = true;
                }
            }
        }
        for (down, keysym, scancode) in typer.due(Instant::now()) {
            if down {
                key_pressed(shared);
            }
            s.write_all(&encode_input(Input::Key { down, keysym, scancode }, scancodes)).map_err(io_err)?;
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
                    let (w, h) = {
                        let mut sh = shared.lock().unwrap();
                        if shower.done(parser.update(), Instant::now()) {
                            show(&mut sh);
                        }
                        sh.screen.as_ref().map(|sc| (sc.width as u16, sc.height as u16)).unwrap_or((w, h))
                    };
                    let full = std::mem::take(&mut parser.resized);
                    s.write_all(&update_request(!full, w, h)).map_err(io_err)?;
                    s.flush().map_err(io_err)?;
                    ctx.request_repaint();
                }
            }
        }
    }
}

/// How long a scroll that came alone waits for its new lines before it's shown anyway (a server's
/// console sends them on its next refresh: 30 ms when busy, longer after a quiet while).
const HOLD_SCROLL: Duration = Duration::from_millis(200);

/// When to show what came: each update whole, once it's done; a scroll that came alone with the
/// new lines that follow it (or by itself, HOLD_SCROLL later).
#[derive(Default)]
struct Shower {
    /// The scroll waiting: since when.
    held: Option<Instant>,
}

impl Shower {
    /// An update is done: whether to show the screen now.
    fn done(&mut self, u: Update, now: Instant) -> bool {
        match u {
            Update::Nothing => false,
            Update::MovedOnly => {
                self.held.get_or_insert(now);
                false
            }
            Update::Painted => {
                self.held = None;
                true
            }
        }
    }

    /// A scroll has waited long enough: show it as it is.
    fn due(&mut self, now: Instant) -> bool {
        let due = self.held.map_or(false, |t| now.duration_since(t) >= HOLD_SCROLL);
        if due {
            self.held = None;
        }
        due
    }
}

/// Shows the screen as the updates so far left it, and lets the history look at it.
fn show(sh: &mut Shared) {
    let Shared { screen, history, .. } = sh;
    if let Some(sc) = screen.as_mut() {
        if sc.present() {
            if let Some(hist) = history.as_mut() {
                hist.look(sc.width, sc.height, &sc.frame);
            }
        }
    }
}

/// A key went to the VM: the history's next screen that changes all at once is a new one.
fn key_pressed(shared: &Mutex<Shared>) {
    if let Some(h) = shared.lock().unwrap().history.as_mut() {
        h.key();
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

/// What a console says of itself as it connects, for a look at a real pool's (a test's): its
/// screen's size, whether it takes scancodes, and how the VM takes the mouse.
#[cfg(test)]
pub(crate) fn probe(s: &mut ConsoleStream) -> Result<String, String> {
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(io_err)?;
    let (w, h) = handshake(s)?;
    let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
    s.write_all(&set_pixel_format()).map_err(io_err)?;
    s.write_all(&set_encodings()).map_err(io_err)?;
    s.write_all(&update_request(false, w, h)).map_err(io_err)?;
    s.flush().map_err(io_err)?;
    let (mut inb, mut parser, mut chunk) = (InBuf::default(), Parser::new(), vec![0u8; 256 * 1024]);
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until && !parser.painted {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => inb.data.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(io_err(e)),
        }
        while !matches!(parser.step(&mut inb, &shared)?, Step::NeedMore) {}
    }
    let sh = shared.lock().unwrap();
    let (sw, shh) = sh.screen.as_ref().map_or((0, 0), |sc| (sc.width, sc.height));
    Ok(format!("screen {}x{} (first {}x{}), scancodes {}, mouse {}", sw, shh, w, h, sh.scancodes, match sh.absolute {
        Some(true) => "absolute (tablet)",
        Some(false) => "RELATIVE (PS/2)",
        None => "not said",
    }))
}

/// Watches a console for a while, sending it nothing, as the Console tab keeps its history (a
/// test's look at a real pool's): whether it's a text console, and what was kept.
#[cfg(test)]
pub(crate) fn watch_history(s: &mut ConsoleStream, dur: Duration) -> Result<String, String> {
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(io_err)?;
    let (w, h) = handshake(s)?;
    let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
    shared.lock().unwrap().history = Some(Scrollback::default());
    s.write_all(&set_pixel_format()).map_err(io_err)?;
    s.write_all(&set_encodings()).map_err(io_err)?;
    s.write_all(&update_request(false, w, h)).map_err(io_err)?;
    s.flush().map_err(io_err)?;
    s.set_read_timeout(Some(Duration::from_millis(8))).map_err(io_err)?;
    let (mut inb, mut parser, mut chunk) = (InBuf::default(), Parser::new(), vec![0u8; 256 * 1024]);
    let (until, mut looks) = (Instant::now() + dur, 0);
    // what the updates were like: the formats used, rectangles in each, bytes
    let (mut encs, mut rects, mut most, mut in_this, mut bytes) = (vec![], 0, 0, 0, 0);
    while Instant::now() < until {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes += n;
                inb.data.extend_from_slice(&chunk[..n]);
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
            Err(e) => return Err(io_err(e)),
        }
        loop {
            if parser.rects_left.is_some() && inb.avail().len() >= 12 {
                let e = be32(&inb.avail()[8..12]) as i32;
                if !encs.contains(&e) {
                    encs.push(e);
                }
            }
            let rect = parser.rects_left.is_some();
            match parser.step(&mut inb, &shared)? {
                Step::NeedMore => break,
                Step::Progress => {
                    if rect && parser.rects_left.is_some() {
                        (rects, in_this) = (rects + 1, in_this + 1);
                    }
                }
                Step::UpdateDone => {
                    (rects, in_this) = (rects + rect as usize, in_this + rect as usize);
                    most = most.max(in_this);
                    in_this = 0;
                    looks += 1;
                    let mut sh = shared.lock().unwrap();
                    let Shared { screen, history, .. } = &mut *sh;
                    let sc = screen.as_ref().unwrap();
                    history.as_mut().unwrap().look(sc.width, sc.height, &sc.rgba);
                    let (w, h) = (sc.width as u16, sc.height as u16);
                    drop(sh);
                    s.write_all(&update_request(!std::mem::take(&mut parser.resized), w, h)).map_err(io_err)?;
                    s.flush().map_err(io_err)?;
                }
            }
        }
    }
    let sh = shared.lock().unwrap();
    let hist = sh.history.as_ref().unwrap();
    Ok(format!("{} looks, text console {}, {} rows kept; screen {}x{}, formats {:?}, {} rectangles ({} at most in one update), {} KB", looks, hist.is_text(), hist.rows(), w, h, encs, rects, most, bytes / 1024))
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
/// A server's console (vncterm) knows only Raw and Hextile: Hextile is a fraction of the bytes.
const ENC_HEXTILE: i32 = 5;
const ENC_ZRLE: i32 = 16;
const ENC_DESKTOP_SIZE: i32 = -223;
const ENC_QEMU_KEYS: i32 = -258;
/// QEMU says whether the VM takes the mouse at positions or as moves (in its x), and again each
/// time that changes. Asked for, a move is sent as 0x7FFF plus how far it went.
const ENC_POINTER_TYPE: i32 = -257;

fn set_encodings() -> Vec<u8> {
    // The server uses the first one in this list that it knows.
    let mut encs = vec![ENC_ZRLE, ENC_HEXTILE, ENC_COPYRECT, ENC_RAW, ENC_DESKTOP_SIZE, ENC_QEMU_KEYS, ENC_POINTER_TYPE];
    if cfg!(test) && std::env::var("SVM_TEST_RAW").is_ok() {
        encs.retain(|&e| e != ENC_HEXTILE); // (a test's: the picture as it was before Hextile)
    }
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
/// else). On a VM's keyboard a capital or a symbol gets Shift, as on a US keyboard, held across a
/// run of them (fewer keys to send); the server's text console takes each character as it is.
fn typing_keys(text: &str, host: bool) -> (Vec<Vec<KeyEv>>, usize) {
    let mut out: Vec<Vec<KeyEv>> = vec![];
    let mut left_out = 0;
    let mut shifted = false;
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
        let shift = !host && (ch.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(ch));
        let mut g = vec![];
        if shift != shifted {
            g.push((shift, SHIFT.0, SHIFT.1));
            shifted = shift;
        }
        g.extend([(true, keysym, scancode), (false, keysym, scancode)]);
        out.push(g);
    }
    if let (true, Some(g)) = (shifted, out.last_mut()) {
        g.push((false, SHIFT.0, SHIFT.1));
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
    /// The update coming in (or the last one) painted something, and only by moving what was
    /// there (CopyRect: a server's console sends a scroll that way, the new lines after it).
    this_painted: bool,
    this_moved_only: bool,
}

/// What an update was, for showing it.
#[derive(PartialEq, Debug)]
enum Update {
    /// Nothing painted (news such as its size, or nothing).
    Nothing,
    /// Only what was there moved: a server's console scrolls so, and sends the new lines a moment
    /// later in another update. Shown alone, the old bottom showed twice meanwhile.
    MovedOnly,
    Painted,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Parser {
    /// What the update just done was.
    fn update(&self) -> Update {
        match (self.this_painted, self.this_moved_only) {
            (false, _) => Update::Nothing,
            (true, true) => Update::MovedOnly,
            (true, false) => Update::Painted,
        }
    }

    fn new() -> Parser {
        Parser { rects_left: None, zlib: flate2::Decompress::new(true), resized: false, painted: false, this_painted: false, this_moved_only: true }
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
                (self.this_painted, self.this_moved_only) = (false, true);
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
            ENC_DESKTOP_SIZE | ENC_QEMU_KEYS | ENC_POINTER_TYPE => 0,
            ENC_ZRLE if a.len() >= 16 => 4 + be32(&a[12..16]) as usize,
            ENC_ZRLE => return Ok(Step::NeedMore),
            // (its length shows only by going through its tiles)
            ENC_HEXTILE => match hextile(&a[12..], w, h, None) {
                Some(n) => n,
                None => return Ok(Step::NeedMore),
            },
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
                ENC_POINTER_TYPE => sh.absolute = Some(x != 0),
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
                        ENC_HEXTILE => {
                            hextile(data, w, h, Some((sc, x, y)));
                        }
                        _ => zrle(sc, inflated.as_deref().unwrap_or(&[]), x, y, w, h)?,
                    }
                    sc.mark(x, y, w, h);
                    self.painted = true;
                    self.this_painted = true;
                    self.this_moved_only &= enc == ENC_COPYRECT;
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

/// A Hextile rectangle: 16x16 tiles, each its pixels as they come, or a background with
/// rectangles of a foreground (or of their own colours) over it. The background and foreground
/// carry on from tile to tile. Paints it, when given the screen and where; how many bytes it
/// takes, or None if more are still to come.
fn hextile(data: &[u8], w: usize, h: usize, mut paint: Option<(&mut Screen, usize, usize)>) -> Option<usize> {
    let px = |b: &[u8]| [b[2], b[1], b[0], 255];
    let (mut at, mut bg, mut fg) = (0usize, [0u8, 0, 0, 255], [0u8, 0, 0, 255]);
    for ty in (0..h).step_by(16) {
        for tx in (0..w).step_by(16) {
            let (tw, th) = (16.min(w - tx), 16.min(h - ty));
            let flags = *data.get(at)?;
            at += 1;
            if flags & 1 != 0 {
                let raw = data.get(at..at + tw * th * 4)?;
                if let Some((sc, x, y)) = paint.as_mut() {
                    for (i, p) in raw.chunks_exact(4).enumerate() {
                        sc.put(*x + tx + i % tw, *y + ty + i / tw, px(p));
                    }
                }
                at += tw * th * 4;
                continue;
            }
            if flags & 2 != 0 {
                bg = px(data.get(at..at + 4)?);
                at += 4;
            }
            if flags & 4 != 0 {
                fg = px(data.get(at..at + 4)?);
                at += 4;
            }
            if let Some((sc, x, y)) = paint.as_mut() {
                sc.fill(*x + tx, *y + ty, tw, th, bg);
            }
            if flags & 8 != 0 {
                let n = *data.get(at)? as usize;
                at += 1;
                let coloured = flags & 16 != 0;
                let each = if coloured { 6 } else { 2 };
                let rects = data.get(at..at + n * each)?;
                if let Some((sc, x, y)) = paint.as_mut() {
                    for r in rects.chunks_exact(each) {
                        let (colour, xy, wh) = if coloured { (px(&r[..4]), r[4], r[5]) } else { (fg, r[0], r[1]) };
                        let (sx, sy) = ((xy >> 4) as usize, (xy & 15) as usize);
                        let (sw, sh) = ((wh >> 4) as usize + 1, (wh & 15) as usize + 1);
                        sc.fill(*x + tx + sx, *y + ty + sy, sw.min(tw.saturating_sub(sx)), sh.min(th.saturating_sub(sy)), colour);
                    }
                }
                at += n * each;
            }
        }
    }
    Some(at)
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
/// Between keys typed into a VM: a little over the 10 ms QEMU waits between the keys it passes on.
const TYPING_GAP: Duration = Duration::from_millis(12);
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
        keep_keys(ui);
        if !resp.hovered() && !resp.dragged() {
            ui.memory_mut(|m| m.surrender_focus(screen_id()));
        }
    }
}

/// Tab, arrows and Escape belong to the VM while it has the keyboard.
fn keep_keys(ui: &Ui) {
    ui.memory_mut(|m| m.set_focus_lock_filter(screen_id(), egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
}

/// How the window holds the mouse while the screen has it: locked in place, its moves still
/// coming (Wayland, macOS); kept inside the window where locking isn't there (X11, Windows).
fn grab_mode() -> egui::viewport::CursorGrab {
    if cfg!(target_os = "macos") || (cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()) {
        egui::viewport::CursorGrab::Locked
    } else {
        egui::viewport::CursorGrab::Confined
    }
}

/// A move of `d` pixels as QEMU takes it from a VM with a PS/2 mouse: 0x7FFF plus the move.
fn rel_coord(d: f32) -> u16 {
    (0x7FFF as f32 + d.clamp(-0x7FFF as f32, 0x7FFF as f32)) as u16
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
    /// Settings' "Share the clipboard with the consoles": off, nothing copied there comes here
    /// (typing this computer's, asked for, works either way).
    pub share_clipboard: bool,
    /// The mouse wheel's turn not sent yet (a part of a step).
    wheel: f32,
    /// A Windows VM with XenServer VM Tools (the app's): they take this computer's clipboard,
    /// for Paste the clipboard.
    pub vm_tools: bool,
    /// The clipboard asked of the window (it comes as a Paste event a frame later), what for, and
    /// frames waited.
    paste: Option<(Paste, u8)>,
    /// This frame's Paste event was that one.
    paste_taken: bool,
    /// Text to type, once no key is held any more.
    to_type: Option<String>,
    /// The console's clipboard as last copied here, and when the connection came (the VM may tell
    /// its own clipboard as it connects: that isn't a copy).
    shared_clip: Option<String>,
    connected_at: Option<Instant>,
    /// A short note over the screen ("Copied…"), until then.
    note: Option<(String, Instant)>,
    /// The screen has the mouse: the VM's is a PS/2 mouse, which takes moves, not positions, so
    /// this window's pointer is hidden and held, and its moves go to the VM. Ctrl+Alt lets go.
    captured: bool,
    /// Parts of a pixel moved and not sent yet, and the buttons the VM was last told of.
    rel_rest: egui::Vec2,
    rel_buttons: u8,
    /// Ctrl and Alt were both down last frame.
    ctrl_alt: bool,
    /// The VM's setting for a USB tablet (the app's; true for a server's console).
    pub usb_tablet: bool,
    /// Asked for one: the app makes the change, once.
    pub want_tablet: bool,
    tablet_asked: bool,
    /// Keep what goes up a text console's screen, for the wheel: a VM's that isn't Windows (a Linux
    /// VM's text console keeps none of its own). A server's keeps its own.
    text_history: bool,
    /// How far back the wheel went, in rows of the screen's pixels (0: the screen as it is), and
    /// a part of a row not gone yet.
    back: usize,
    back_rest: f32,
    /// The screen as it was that far back, and what it was made from (how far back, rows ever
    /// kept, the screen's uploads).
    back_tex: Option<egui::TextureHandle>,
    back_made: Option<(usize, usize, u64)>,
    /// The screen's uploads so far (that picture shows the screen's own rows under the kept ones).
    uploads: u64,
    /// The history as last seen: rows kept, rows ever kept (while back there, the view stays on
    /// the same lines as more go up), a line's height; None when the screen isn't a text console.
    hist: Option<(usize, usize, usize)>,
}

/// What this computer's clipboard is asked for.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Paste {
    /// To type, a key for each character.
    Type,
    /// To give a Windows VM's VM Tools, then press Ctrl+V.
    Give,
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
            share_clipboard: true,
            wheel: 0.0,
            vm_tools: false,
            paste: None,
            paste_taken: false,
            to_type: None,
            shared_clip: None,
            connected_at: None,
            note: None,
            captured: false,
            rel_rest: egui::Vec2::ZERO,
            rel_buttons: 0,
            ctrl_alt: false,
            usb_tablet: true,
            want_tablet: false,
            tablet_asked: false,
            text_history: !os.starts_with("XenServer ") && !os.to_lowercase().contains("windows"),
            back: 0,
            back_rest: 0.0,
            back_tex: None,
            back_made: None,
            uploads: 0,
            hist: None,
        };
        if !demo {
            v.connect();
        }
        v
    }

    fn connect(&mut self) {
        self.remote = Some(Remote::start(self.conn.clone(), self.vm.clone(), self.ctx.clone(), self.text_history));
        (self.back, self.back_made, self.hist) = (0, None, None);
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
        if status != Status::Connected {
            self.release_mouse(); // its screen went away (a restart): the mouse comes back
        }
        let focused = ui.memory(|m| m.has_focus(screen_id()));
        self.clipboard(ui, &status);

        ui.add_space(8.0);

        let area = ui.available_rect_before_wrap();
        let area = Rect::from_min_max(area.min, pos2(area.max.x, area.max.y - 10.0));
        ui.painter().rect_filled(area, Rounding::same(4.0), SCREEN_BG);

        if let Some(demo) = self.demo.as_mut() {
            Self::demo_screen(ui, area, demo, focused, allow_keyboard);
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
        let shown = self.history_texture(ui.ctx()).unwrap_or(tex);
        ui.painter().image(shown, img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        self.history_bar(ui, img);
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
        } else if !self.captured && self.hist.is_none() && *status == Status::Connected && (ui.rect_contains_pointer(area) || std::env::var("SVM_SHOT_MOUSE").is_ok()) && self.remote.as_ref().map_or(false, |r| r.relative()) {
            self.mouse_hint(ui, pal, area);
        }
        if let Some((_, _, line)) = self.hist.filter(|_| self.back > 0) {
            let r = Rect::from_center_size(pos2(area.center().x, area.max.y - 34.0), vec2(440.0_f32.min(area.width() - 20.0), 40.0));
            ui.painter().rect_filled(r, Rounding::same(8.0), Color32::from_black_alpha(215));
            let lines = self.back.div_ceil(line.max(1));
            let mut live = false;
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(r.shrink2(vec2(12.0, 6.0))), |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new(format!("Showing earlier lines · {} up", lines)).color(Color32::from_gray(235)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        live = dialog_button(ui, pal, "down", "Back to the live screen", Look::Plain, true);
                    });
                });
            });
            if live {
                self.back = 0;
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

    /// The clipboard, each frame: what the console copied comes onto this computer's; this
    /// computer's, asked for by Type or Paste the clipboard, is taken; and text waiting to be
    /// typed goes once no key is held. (Nothing of this computer's goes to the VM unasked.)
    fn clipboard(&mut self, ui: &Ui, status: &Status) {
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
                self.shared_clip = Some(text); // the same copy isn't told twice
                self.show_note("Copied to this computer's clipboard");
            }
        }

        self.paste_taken = false;
        if let Some((mode, waited)) = self.paste.take() {
            let text = ui.input(|i| i.events.iter().find_map(|e| if let Event::Paste(t) = e { Some(t.clone()) } else { None }));
            match text {
                Some(t) => {
                    self.paste_taken = true;
                    match mode {
                        Paste::Give if self.demo.is_none() => self.give(&t),
                        _ => self.type_text_later(t),
                    }
                }
                None if waited < 3 => {
                    self.paste = Some((mode, waited + 1));
                    ctx.request_repaint();
                }
                None => self.show_note(if mode == Paste::Type { "The clipboard has no text to type." } else { "The clipboard has no text to paste." }),
            }
        }
        if self.to_type.is_some() && ui.input(|i| i.keys_down.is_empty() && i.modifiers.is_none()) {
            let text = self.to_type.take().unwrap_or_default();
            self.type_now(&text);
        }
    }

    /// Over a screen whose VM has a PS/2 mouse: a click there gives it the mouse; and, when the VM
    /// has no USB tablet, the button that gives it one.
    fn mouse_hint(&mut self, ui: &mut Ui, pal: &Pal, area: Rect) {
        let offer = !self.usb_tablet && !self.tablet_asked;
        let (text, button) = ("Click to use the mouse · Ctrl+Alt lets go", "Add USB tablet");
        let tip = "This VM's mouse is a PS/2 mouse: it takes moves, not where the pointer is, so its pointer would drift from yours. The screen holds your mouse instead while you use it there. A USB tablet follows your pointer exactly; it comes at the VM's next start.";
        let width = |t: &str, font: FontId| ui.painter().layout_no_wrap(t.into(), font, Color32::WHITE).size().x;
        let button_w = if offer { (50.0 + width(button, egui::TextStyle::Button.resolve(ui.style()))).max(80.0) + 10.0 } else { 0.0 };
        let w = (width(text, FontId::proportional(13.0)) + 28.0 + button_w).min(area.width() - 12.0);
        let r = Rect::from_center_size(pos2(area.center().x, area.min.y + 34.0), vec2(w, 40.0));
        ui.painter().rect_filled(r, Rounding::same(8.0), Color32::from_black_alpha(215));
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(r.shrink2(vec2(12.0, 6.0))).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
            if offer && dialog_button(ui, pal, "usb", button, Look::Plain, true) {
                self.want_tablet = true;
                self.tablet_asked = true;
                self.show_note("The VM gets its USB tablet when it restarts.");
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(text).size(13.0).color(Color32::from_gray(235))).truncate()).on_hover_text(tip);
            });
        });
    }

    /// The screen takes the mouse (a VM with a PS/2 mouse).
    fn capture(&mut self, ctx: &egui::Context) {
        (self.captured, self.rel_rest, self.rel_buttons) = (true, egui::Vec2::ZERO, 0);
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(grab_mode()));
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
        self.show_note("Ctrl+Alt lets go of the mouse");
    }

    /// Gives the mouse back to the window, its buttons let go in the VM.
    pub fn release_mouse(&mut self) {
        if !std::mem::take(&mut self.captured) {
            return;
        }
        if std::mem::take(&mut self.rel_buttons) != 0 {
            if let Some(r) = &self.remote {
                r.send(Input::Pointer { buttons: 0, x: 0x7FFF, y: 0x7FFF });
            }
        }
        self.ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::viewport::CursorGrab::None));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
        self.ctx.request_repaint();
    }

    fn show_note(&mut self, text: &str) {
        self.note = Some((text.to_string(), Instant::now() + Duration::from_millis(2500)));
        self.ctx.request_repaint();
    }

    /// Types this computer's clipboard on the console's keyboard (Type the clipboard, in the
    /// Actions menu: the only way this computer's clipboard reaches the VM). The window is asked
    /// for its clipboard's text; it comes as a Paste event in a frame or two.
    pub fn type_clipboard(&mut self, ctx: &egui::Context) {
        self.ask_paste(ctx, Paste::Type);
    }

    /// Pastes this computer's clipboard in a Windows VM (Paste the clipboard, in the Actions
    /// menu): its VM Tools get it, then Ctrl+V is pressed. At once, accents and all.
    pub fn paste_clipboard(&mut self, ctx: &egui::Context) {
        self.ask_paste(ctx, Paste::Give);
    }

    fn ask_paste(&mut self, ctx: &egui::Context, mode: Paste) {
        self.back = 0;
        if self.paste.is_none() {
            self.paste = Some((mode, 0));
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ctx.request_repaint();
        }
    }

    /// Gives the VM Tools this computer's clipboard, and presses Ctrl+V a moment later (they take
    /// a moment to have it).
    fn give(&mut self, text: &str) {
        let Some(r) = &self.remote else { return };
        if text.len() > 1_000_000 {
            self.show_note("The clipboard is too big to paste (over a million characters).");
            return;
        }
        r.send(Input::CutText(text.to_string()));
        let v = ('v' as u32, if r.scancodes() { 0x2f } else { 0 });
        let keys = vec![vec![(true, CTRL.0, CTRL.1), (true, v.0, v.1), (false, v.0, v.1), (false, CTRL.0, CTRL.1)]];
        r.send(Input::Type { groups: keys, gap: TYPING_GAP, after: Duration::from_millis(300), shown: false });
        let odd = text.chars().filter(|c| *c as u32 >= 0x100).count();
        if odd > 0 {
            self.show_note(&format!("{} character{} came as \"?\": the console passes only Western European letters.", odd, if odd == 1 { "" } else { "s" }));
        }
    }

    /// Presses Ctrl+Alt+Del on the VM (the desktop keeps those keys for itself).
    pub fn send_ctrl_alt_del(&mut self) {
        self.send_combo(&[CTRL, ALT, DELETE]);
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
        // a VM takes a key every 10 ms (about 40 characters a second); the server's console, any
        let gap = if self.host { Duration::ZERO } else { TYPING_GAP };
        r.send(Input::Type { groups, gap, after: Duration::ZERO, shown: true });
        if left_out > 0 {
            self.show_note(&format!("{} character{} left out: only plain letters, digits and symbols can be typed.", left_out, if left_out == 1 { "" } else { "s" }));
        }
    }

    /// Whether the screen is there to send keys to.
    pub fn connected(&self) -> bool {
        self.demo.is_some() || self.status() == Status::Connected
    }

    /// The other keys the desktop keeps for itself, as a menu's items (Send other keys, in the
    /// Actions menu, after Type the clipboard and Send Ctrl+Alt+Del): the combinations, then the
    /// Linux text consoles.
    pub fn keys_menu(&mut self, ui: &mut Ui) {
        ui.set_min_width(230.0);
        let mut combo: Option<Vec<(u32, u32)>> = None;
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
                    Some(egui::ColorImage::from_rgba_unmultiplied([sc.width, sc.height], &sc.frame))
                }
                _ => None,
            }
        };
        if let Some(img) = img {
            self.uploads += 1;
            self.tex_size = img.size;
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("vm-console", img, egui::TextureOptions::LINEAR)),
            }
        }
    }

    /// The wheel over a text console the app keeps a history of: it goes up and down that (three
    /// lines a step, as a terminal), not to the VM. False on any other screen.
    fn wheel_history(&mut self, steps: f32) -> bool {
        let Some((kept, _, line)) = self.hist else { return false };
        if steps != 0.0 {
            self.back_rest += steps * 3.0 * line as f32;
            let rows = self.back_rest.trunc();
            self.back_rest -= rows;
            self.back = (self.back as f32 + rows).clamp(0.0, kept as f32) as usize;
            self.ctx.request_repaint();
        }
        true
    }

    /// The history as it is now (`hist`), and while back up it, the screen as it was there: made
    /// again when how far back, what's kept or the screen changed.
    fn history_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureId> {
        let r = self.remote.as_ref()?;
        let sh = r.shared.lock().unwrap();
        let (h, sc) = match (sh.history.as_ref(), sh.screen.as_ref()) {
            (Some(h), Some(sc)) if h.is_text() => (h, sc),
            _ => {
                (self.back, self.hist) = (0, None);
                return None;
            }
        };
        let (kept, pushed) = (h.rows(), h.pushed());
        if let Some((_, seen, _)) = self.hist {
            if self.back > 0 && pushed > seen {
                self.back += pushed - seen; // the same lines stay in view as more go up
            }
        }
        if let Ok(v) = std::env::var("SVM_SHOT_BACK") {
            // (for --screenshot: that many lines back)
            self.back = self.back.max(v.parse::<usize>().unwrap_or(0) * h.line());
        }
        self.back = self.back.min(kept);
        self.hist = Some((kept, pushed, h.line()));
        if self.back == 0 {
            return None;
        }
        let made = (self.back, pushed, self.uploads);
        if self.back_made != Some(made) || self.back_tex.is_none() {
            let img = egui::ColorImage::from_rgba_unmultiplied([sc.width, sc.height], &h.view(self.back, &sc.frame));
            match &mut self.back_tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.back_tex = Some(ctx.load_texture("vm-console-history", img, egui::TextureOptions::LINEAR)),
            }
            self.back_made = Some(made);
        }
        self.back_tex.as_ref().map(|t| t.id())
    }

    /// While back up the history: a thin bar on the screen's right showing where.
    fn history_bar(&self, ui: &Ui, img: Rect) {
        let Some((kept, _, _)) = self.hist.filter(|_| self.back > 0) else { return };
        let h = self.tex_size[1].max(1) as f32;
        let total = kept as f32 + h;
        let top = img.min.y + img.height() * (kept - self.back) as f32 / total;
        let len = (img.height() * h / total).max(18.0);
        let r = Rect::from_min_size(pos2(img.max.x - 6.0, top.min(img.max.y - len)), vec2(4.0, len));
        ui.painter().rect_filled(r, Rounding::same(2.0), Color32::from_white_alpha(70));
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
        self.back = 0;
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
        let (events, mods, pos, buttons, window_focused) = ui.input(|i| {
            let p = &i.pointer;
            let buttons = (p.button_down(egui::PointerButton::Primary) as u8) | (p.button_down(egui::PointerButton::Middle) as u8) << 1 | (p.button_down(egui::PointerButton::Secondary) as u8) << 2;
            (i.events.clone(), i.modifiers, p.latest_pos(), buttons, i.focused)
        });
        // The mouse is let go with Ctrl+Alt (pressed together), when the window goes to the back
        // or a dialog opens, when the VM starts taking positions after all (its USB tablet), and
        // on a text console (no mouse there).
        let relative = self.remote.as_ref().map_or(false, |r| r.relative());
        let ctrl_alt = mods.ctrl && mods.alt;
        if self.captured && ((ctrl_alt && !self.ctrl_alt) || !window_focused || !allow || !relative || self.hist.is_some()) {
            self.release_mouse();
        }
        self.ctrl_alt = ctrl_alt;
        if self.captured {
            // the keyboard stays with the VM while it has the mouse
            resp.request_focus();
            keep_keys(ui);
        } else {
            follow_pointer(ui, resp, focused, allow);
        }
        if !focused {
            if self.sent_mods != Modifiers::NONE {
                self.sync_mods(Modifiers::NONE); // never leave Shift or Ctrl held down in the VM
            }
        } else {
            let scancodes = self.remote.as_ref().map_or(false, |r| r.scancodes());
            for e in events.clone() {
                // a key back up the history: the screen as it is again, and the key goes on
                if matches!(e, Event::Key { pressed: true, .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_)) {
                    self.back = 0;
                }
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
                    // (the clipboard Type the clipboard asked for: it's typed, not a key)
                    Event::Paste(_) if self.paste_taken => {}
                    // Ctrl+V and Ctrl+Shift+V never paste this computer's clipboard: the VM gets
                    // the keys, for its own clipboard (Type the clipboard, in Actions, types ours)
                    Event::Paste(_) => self.tap('v' as u32, if scancodes { 0x2f } else { 0 }),
                    _ => {}
                }
            }
            self.sync_mods(mods);
        }

        // The wheel over a text console with a history scrolls that, not the VM.
        let over = resp.hovered() || resp.dragged();
        let wheel: f32 = events.iter().map(wheel_steps).sum();
        let wheel = if (over || self.captured) && self.wheel_history(wheel) { 0.0 } else { wheel };

        if relative {
            self.relative_mouse(ui, resp, &events, buttons, allow, wheel);
            return;
        }

        // The mouse: where it is over the screen, its buttons, and the wheel as buttons 4 and 5.
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
            self.wheel += wheel * if self.host { 3.0 } else { 1.0 };
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

    /// A VM with a PS/2 mouse: a click on its screen gives it the mouse; from then on, the mouse's
    /// moves as the system reports them (not where the pointer is: it's held still), its buttons
    /// and its wheel.
    fn relative_mouse(&mut self, ui: &Ui, resp: &egui::Response, events: &[Event], buttons: u8, allow: bool, wheel: f32) {
        if !self.captured {
            self.wheel = 0.0;
            // (a text console has no mouse to give: it stays this window's)
            if allow && resp.clicked() && self.hist.is_none() {
                self.capture(ui.ctx());
            }
            return;
        }
        let Some(r) = &self.remote else { return };
        let mut d = self.rel_rest;
        for e in events {
            if let Event::MouseMoved(m) = e {
                d += *m;
            }
        }
        let whole = vec2(d.x.trunc(), d.y.trunc());
        self.rel_rest = d - whole;
        if whole != egui::Vec2::ZERO || buttons != self.rel_buttons {
            r.send(Input::Pointer { buttons, x: rel_coord(whole.x), y: rel_coord(whole.y) });
            self.rel_buttons = buttons;
        }
        self.wheel += wheel;
        let clicks = self.wheel.trunc();
        self.wheel -= clicks;
        let b = if clicks > 0.0 { 1 << 3 } else { 1 << 4 };
        for _ in 0..(clicks.abs() as u32).min(30) {
            r.send(Input::Pointer { buttons: buttons | b, x: 0x7FFF, y: 0x7FFF });
            r.send(Input::Pointer { buttons, x: 0x7FFF, y: 0x7FFF });
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
                    // (Ctrl+V pastes nothing here either: Type the clipboard, in Actions, does)
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

impl Drop for ConsoleView {
    fn drop(&mut self) {
        self.release_mouse();
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
                        let mut sh = shared.lock().unwrap();
                        let Shared { screen, history, .. } = &mut *sh;
                        let sc = screen.as_ref().unwrap();
                        if let Some(hist) = history.as_mut() {
                            hist.look(sc.width, sc.height, &sc.rgba);
                        }
                        let (w, h) = (sc.width as u16, sc.height as u16);
                        drop(sh);
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

    /// Types SVM_TEST_TEXT into a real VNC server as the console does, with the pacing it uses, for
    /// QEMU's input trace to show which keys reached the VM: SVM_TEST_VNC=127.0.0.1:5977 cargo test
    /// live_typing -- --ignored --nocapture.
    #[test]
    #[ignore]
    fn live_typing() {
        let addr = std::env::var("SVM_TEST_VNC").expect("SVM_TEST_VNC=host:port");
        let text = std::env::var("SVM_TEST_TEXT").unwrap_or_else(|_| "The Quick Brown Fox Jumps Over The Lazy Dog! echo $HOME && ls -la /tmp | grep X".into());
        let mut s = ConsoleStream::Plain(std::net::TcpStream::connect(&addr).unwrap());
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (w, h) = handshake(&mut s).unwrap();
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
        s.write_all(&set_pixel_format()).unwrap();
        s.write_all(&set_encodings()).unwrap();
        s.write_all(&update_request(false, w, h)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(8))).unwrap();
        let (mut inb, mut parser, mut seen) = (InBuf::default(), Parser::new(), vec![]);
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(800));
        let scancodes = shared.lock().unwrap().scancodes;
        let (groups, _) = typing_keys(&text, false);
        let events: usize = groups.iter().map(|g| g.len()).sum();
        let mut typer = Typer::default();
        typer.add(groups, TYPING_GAP, Duration::ZERO, true);
        let start = Instant::now();
        while typer.progress().is_some() {
            for (down, keysym, scancode) in typer.due(Instant::now()) {
                s.write_all(&encode_input(Input::Key { down, keysym, scancode }, scancodes)).unwrap();
            }
            s.flush().unwrap();
            pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(1));
        }
        println!("{} characters, {} key events in {:?} (scancodes {})", text.chars().count(), events, start.elapsed(), scancodes);
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(1500));
    }

    /// A Linux text console's history, against a real one: QEMU running a kernel with a shell on
    /// tty0 (see scrollback.rs; SVM_TEST_VNC=host:port SVM_TEST_OUT=<dir>). Types commands whose
    /// output goes up the screen a little, by more than a screen at once, and slowly (or
    /// SVM_TEST_COMMANDS, split by ";;"), and saves the screen and what was kept.
    #[test]
    #[ignore]
    fn live_history() {
        let addr = std::env::var("SVM_TEST_VNC").expect("SVM_TEST_VNC=host:port");
        let out = std::env::var("SVM_TEST_OUT").unwrap_or_else(|_| ".".into());
        let mut s = ConsoleStream::Plain(std::net::TcpStream::connect(&addr).unwrap());
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (w, h) = handshake(&mut s).unwrap();
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
        shared.lock().unwrap().history = Some(Scrollback::default());
        s.write_all(&set_pixel_format()).unwrap();
        s.write_all(&set_encodings()).unwrap();
        s.write_all(&update_request(false, w, h)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(8))).unwrap();
        let (mut inb, mut parser, mut seen) = (InBuf::default(), Parser::new(), vec![]);
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(1500));
        let scancodes = shared.lock().unwrap().scancodes;
        let report = |what: &str, shared: &Mutex<Shared>| {
            let sh = shared.lock().unwrap();
            let hist = sh.history.as_ref().unwrap();
            println!("{:28} text {}  kept {} rows ({} lines of {})", what, hist.is_text(), hist.rows(), hist.rows() / hist.line(), hist.line());
        };
        report("at first", &shared);
        let commands = std::env::var("SVM_TEST_COMMANDS").unwrap_or_else(|_| "seq 1 12;;seq 100 160;;for i in $(seq 1 30); do echo slowly $i; sleep 0.1; done".into());
        for cmd in commands.split(";;").filter(|c| !c.is_empty()) {
            let (groups, _) = typing_keys(&format!("{}\n", cmd), false);
            let mut typer = Typer::default();
            typer.add(groups, TYPING_GAP, Duration::ZERO, false);
            while !typer.left.is_empty() {
                for (down, keysym, scancode) in typer.due(Instant::now()) {
                    if down {
                        key_pressed(&shared);
                    }
                    s.write_all(&encode_input(Input::Key { down, keysym, scancode }, scancodes)).unwrap();
                }
                s.flush().unwrap();
                pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(2));
            }
            pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(4500));
            report(cmd, &shared);
        }
        save(&shared, &format!("{}/history-screen.png", out));
        let sh = shared.lock().unwrap();
        let (hist, sc) = (sh.history.as_ref().unwrap(), sh.screen.as_ref().unwrap());
        let all = hist.kept_pixels();
        image::RgbaImage::from_raw(sc.width as u32, (all.len() / 4 / sc.width) as u32, all).unwrap().save(format!("{}/history-kept.png", out)).unwrap();
        let back = hist.view(hist.line() * 10, &sc.rgba);
        image::RgbaImage::from_raw(sc.width as u32, sc.height as u32, back).unwrap().save(format!("{}/history-10-back.png", out)).unwrap();
    }

    /// A VM with a PS/2 mouse as QEMU has it with no guest running (SVM_TEST_VNC=host:port, its
    /// input traced): the console says so, and moves go as 0x7FFF plus the move.
    #[test]
    #[ignore]
    fn live_relative_mouse() {
        let addr = std::env::var("SVM_TEST_VNC").expect("SVM_TEST_VNC=host:port");
        let mut s = ConsoleStream::Plain(std::net::TcpStream::connect(&addr).unwrap());
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (w, h) = handshake(&mut s).unwrap();
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(w as usize, h as usize))));
        s.write_all(&set_pixel_format()).unwrap();
        s.write_all(&set_encodings()).unwrap();
        s.write_all(&update_request(false, w, h)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(8))).unwrap();
        let (mut inb, mut parser, mut seen) = (InBuf::default(), Parser::new(), vec![]);
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(800));
        assert_eq!(shared.lock().unwrap().absolute, Some(false));
        for (buttons, dx, dy) in [(0, 10.0, -5.0), (1, 0.0, 0.0), (0, 0.0, 0.0), (0, -300.0, 40000.0)] {
            s.write_all(&encode_input(Input::Pointer { buttons, x: rel_coord(dx), y: rel_coord(dy) }, true)).unwrap();
        }
        s.flush().unwrap();
        pump(&mut s, &mut inb, &mut parser, &shared, &mut seen, Duration::from_millis(300));
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
        // what's shown is the whole update, once it's done (and only the part it changed)
        assert_eq!(p.update(), Update::Painted);
    }

    /// Runs `bytes` through a parser until an update is done: what it was.
    fn one_update(p: &mut Parser, shared: &Mutex<Shared>, bytes: &[u8]) -> Update {
        let mut inb = InBuf::default();
        inb.data.extend_from_slice(bytes);
        loop {
            match p.step(&mut inb, shared).unwrap() {
                Step::UpdateDone => return p.update(),
                Step::NeedMore => panic!("ran out of bytes"),
                Step::Progress => {}
            }
        }
    }

    #[test]
    fn the_picture_shown_is_whole_updates() {
        let shared = Mutex::new(Shared::new(Status::Connected, Some(Screen::new(4, 2))));
        let mut p = Parser::new();
        // a 1x1 raw piece at (1,1)
        let u = one_update(&mut p, &shared, &[0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 9, 8, 7, 0]);
        assert_eq!(u, Update::Painted);
        let mut sh = shared.lock().unwrap();
        let sc = sh.screen.as_mut().unwrap();
        sc.dirty = false;
        let at = (1 * 4 + 1) * 4;
        assert_eq!(&sc.rgba[at..at + 4], &[7, 8, 9, 255]);
        assert_eq!(&sc.frame[at..at + 4], &[0, 0, 0, 255], "not shown until it's presented");
        assert!(sc.present() && sc.dirty);
        assert_eq!(&sc.frame[at..at + 4], &[7, 8, 9, 255]);
        assert!(!sc.present(), "nothing new to show");
        drop(sh);
        // a server's console scrolling: a copy alone, then the new line
        let u = one_update(&mut p, &shared, &[0, 0, 0, 1, 0, 0, 0, 0, 0, 4, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(u, Update::MovedOnly);
        let u = one_update(&mut p, &shared, &[0, 0, 0, 0]);
        assert_eq!(u, Update::Nothing);
    }

    #[test]
    fn a_scroll_alone_waits_for_its_new_lines() {
        let (mut sw, t) = (Shower::default(), Instant::now());
        let ms = |n: u64| t + Duration::from_millis(n);
        assert!(!sw.done(Update::MovedOnly, t), "a server's console moved its lines up: not shown yet");
        assert!(!sw.due(ms(100)));
        assert!(sw.done(Update::Painted, ms(120)), "the new lines came: both shown together");
        assert!(!sw.due(ms(500)), "nothing waits any more");
        assert!(!sw.done(Update::MovedOnly, ms(600)));
        assert!(!sw.done(Update::Nothing, ms(650)), "an empty update doesn't show it");
        assert!(sw.due(ms(600) + HOLD_SCROLL), "the new lines never came: shown as it is");
        assert!(!sw.due(ms(2000)));
        assert!(sw.done(Update::Painted, ms(2100)));
    }

    #[test]
    fn hextile() {
        // 20x18: four tiles, 16x16, 4x16, 16x2 and 4x2
        let (w, h) = (20, 18);
        let mut d = vec![];
        // 1: a background (1,2,3) and foreground (4,5,6), with a 3x2 rectangle of it at (2,1)
        d.extend_from_slice(&[2 | 4 | 8, 3, 2, 1, 0, 6, 5, 4, 0, 1, 0x21, 0x21]);
        // 2: the same background, no rectangles
        d.push(0);
        // 3: rectangles of their own colours over a new background (9,9,9): a 1x1 of (7,7,7) at (15,1)
        d.extend_from_slice(&[2 | 8 | 16, 9, 9, 9, 0, 1, 7, 7, 7, 0, 0xf1, 0x00]);
        // 4: its pixels as they are (4x2)
        d.push(1);
        for i in 0..8u8 {
            d.extend_from_slice(&[i, i, i, 0]);
        }
        assert_eq!(super::hextile(&d, w, h, None), Some(d.len()));
        assert_eq!(super::hextile(&d[..d.len() - 1], w, h, None), None, "more to come");
        let mut sc = Screen::new(w, h);
        super::hextile(&d, w, h, Some((&mut sc, 0, 0)));
        let px = |x: usize, y: usize| sc.rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].to_vec();
        // (pixels come as blue, green, red)
        assert_eq!(px(0, 0), vec![1, 2, 3, 255]);
        assert_eq!((px(2, 1), px(4, 2), px(5, 2), px(2, 3)), (vec![4, 5, 6, 255], vec![4, 5, 6, 255], vec![1, 2, 3, 255], vec![1, 2, 3, 255]));
        assert_eq!(px(17, 5), vec![1, 2, 3, 255], "the background carries on");
        assert_eq!((px(15, 17), px(0, 16)), (vec![7, 7, 7, 255], vec![9, 9, 9, 255]));
        assert_eq!((px(16, 16), px(19, 17)), (vec![0, 0, 0, 255], vec![7, 7, 7, 255]));
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
        // this computer's (ClientCutText, for Paste the clipboard), in Latin-1
        assert_eq!(encode_input(Input::CutText("ç€".into()), false), vec![6, 0, 0, 0, 0, 0, 0, 2, 0xe7, b'?']);
    }

    #[test]
    fn typing_the_clipboard() {
        let (keys, left_out) = typing_keys("aB!\nç", false);
        assert_eq!(left_out, 1);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0], vec![(true, 'a' as u32, 0), (false, 'a' as u32, 0)]);
        // Shift goes down for "B" and stays down for "!", the next one that needs it
        assert_eq!(keys[1], vec![(true, SHIFT.0, SHIFT.1), (true, 'B' as u32, 0), (false, 'B' as u32, 0)]);
        assert_eq!(keys[2], vec![(true, '!' as u32, 0), (false, '!' as u32, 0)]);
        assert_eq!(keys[3], vec![(false, SHIFT.0, SHIFT.1), (true, 0xff0d, 0x1c), (false, 0xff0d, 0x1c)]); // Enter
        // ending on a capital lets go of Shift
        let (keys, _) = typing_keys("xY", false);
        assert_eq!(keys[1].last(), Some(&(false, SHIFT.0, SHIFT.1)));
        // the server's text console takes the characters as they are, without Shift
        let (keys, _) = typing_keys("B", true);
        assert_eq!(keys, vec![vec![(true, 'B' as u32, 0), (false, 'B' as u32, 0)]]);
    }

    #[test]
    fn typing_is_paced() {
        let mut t = Typer::default();
        let start = Instant::now();
        let gap = Duration::from_millis(12);
        // one key at a time, a character's keys too
        t.add(vec![vec![(true, 1, 0), (false, 1, 0)], vec![(true, 2, 0), (false, 2, 0)]], gap, Duration::ZERO, true);
        assert_eq!(t.due(start), vec![(true, 1, 0)]);
        assert_eq!(t.due(start + Duration::from_millis(5)), vec![]);
        assert_eq!(t.progress(), Some((0, 2)));
        assert_eq!(t.due(start + gap), vec![(false, 1, 0)]);
        assert_eq!(t.progress(), Some((1, 2)));
        // after a slow moment, still one at a time
        assert_eq!(t.due(start + Duration::from_millis(500)).len(), 1);
        assert_eq!(t.due(start + Duration::from_millis(600)).len(), 1);
        assert_eq!(t.progress(), None);
        // no gap: all at once; Stop drops the rest and lets go of what is still held
        t.add((0..500).map(|i| vec![(true, i, 0)]).collect(), Duration::ZERO, Duration::ZERO, true);
        assert_eq!(t.due(start).len(), 300);
        assert_eq!(t.stop().len(), 300);
        assert_eq!((t.due(start).len(), t.progress()), (0, None));
        // Stop in the middle of a run of capitals lets go of Shift
        let (keys, _) = typing_keys("ABC", false);
        t.add(keys, gap, Duration::ZERO, true);
        let mut now = start + Duration::from_secs(1);
        for _ in 0..3 {
            t.due(now);
            now += gap;
        }
        assert_eq!(t.stop(), vec![(false, SHIFT.0, SHIFT.1)]);
        // a later start
        t.add(vec![vec![(true, 9, 0)]], gap, Duration::from_secs(60), false);
        assert_eq!(t.due(Instant::now()), vec![]);
    }

    #[test]
    fn wheel_in_steps() {
        let ev = |unit, y| Event::MouseWheel { unit, delta: vec2(0.0, y), modifiers: Modifiers::NONE };
        assert_eq!(wheel_steps(&ev(egui::MouseWheelUnit::Line, -1.0)), -1.0);
        assert_eq!(wheel_steps(&ev(egui::MouseWheelUnit::Point, 20.0)), 0.5);
        assert_eq!(wheel_steps(&Event::Copy), 0.0);
    }
}
