//! Interactive terminal view of an explicitly owned or standalone browser target.

use crate::address::{AddressBook, Suggestion};
use crate::agent::{self, SendKind};
use crate::browser_files::Files;
use crate::cdp::Cdp;
use crate::icons::{self, Icon, Rgb};
use crate::render::{self, Cells, IMAGE_ID, Renderer, SYNC_OFF, SYNC_ON};
use crate::page_tools;
use crate::state::{self, Config};
use crate::{R, chrome, page};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::{cursor, execute, queue, terminal};
use serde_json::{Value, json};
use std::io::{Write, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

enum Msg {
    Frame { generation: u64, data: String, w: f64, h: f64 },
    Gone { generation: u64, error: String },
}

/// What a stream shows: the page's emulated viewport (CSS px, headless only) and the
/// largest frame wanted (device px; small for half blocks, which can't use more).
#[derive(Debug, Clone, Copy, PartialEq)]
struct StreamSize {
    viewport: (u32, u32),
    dpr: f64,
    max_frame: (u32, u32),
    /// JPEG frames for `KittyStream` (cheap for Chrome to encode); else PNG.
    jpeg: bool,
}

/// Screencast `target` on its own connection until `stop`; acks every frame.
fn stream(url: String, target: String, size: StreamSize, headless: bool, tx: Sender<Msg>, stop: Arc<AtomicBool>, generation: u64) {
    let run = || -> R<()> {
        let mut c = Cdp::connect(&url)?;
        let r = c.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
        let sid = r["sessionId"].as_str().ok_or("no session")?.to_string();
        if headless {
            // Session-scoped: the page gets the pane's size only while this view watches it.
            c.call(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": size.viewport.0, "height": size.viewport.1, "deviceScaleFactor": size.dpr, "mobile": false }),
                Some(&sid),
            )?;
        }
        c.call(
            "Page.startScreencast",
            json!({
                "format": if size.jpeg { "jpeg" } else { "png" },
                "quality": 90,
                "maxWidth": size.max_frame.0,
                "maxHeight": size.max_frame.1,
                "everyNthFrame": 1,
            }),
            Some(&sid),
        )?;
        while !stop.load(Ordering::Relaxed) {
            let Some(m) = c.next_message(Duration::from_millis(200))? else { continue };
            match m["method"].as_str() {
                Some("Page.screencastFrame") => {
                    let p = &m["params"];
                    c.send("Page.screencastFrameAck", json!({ "sessionId": p["sessionId"] }), Some(&sid))?;
                    let msg = Msg::Frame {
                        generation,
                        data: p["data"].as_str().unwrap_or_default().to_string(),
                        w: p.pointer("/metadata/deviceWidth").and_then(Value::as_f64).unwrap_or(size.viewport.0 as f64),
                        h: p.pointer("/metadata/deviceHeight").and_then(Value::as_f64).unwrap_or(size.viewport.1 as f64),
                    };
                    if tx.send(msg).is_err() {
                        return Ok(());
                    }
                }
                Some("Target.detachedFromTarget" | "Inspector.detached") => return Err("tab closed".into()),
                _ => {}
            }
        }
        Ok(())
    };
    if let Err(e) = run() {
        let _ = tx.send(Msg::Gone { generation, error: e.to_string() });
    }
}

/// Address-bar editing. Starts with the whole URL selected, like a browser:
/// typing replaces the selection, an arrow key or click drops it; shift+arrows,
/// drag, double-click (word) and triple-click (all) select.
#[derive(Debug, Clone, PartialEq)]
pub struct UrlEdit {
    buf: Vec<char>,
    cur: usize,
    /// Selection runs from here to `cur`.
    anchor: Option<usize>,
}

/// URL separators: word boundaries for double-click and ctrl+w.
const URL_SEPS: &str = "/ .?&=#:";

#[derive(Debug, PartialEq)]
pub enum Edit {
    Editing,
    Go(String),
    Cancel,
}

impl UrlEdit {
    pub fn new(url: &str) -> UrlEdit {
        let buf: Vec<char> = url.chars().collect();
        UrlEdit { cur: buf.len(), anchor: (!buf.is_empty()).then_some(0), buf }
    }

    fn selection(&self) -> Option<std::ops::Range<usize>> {
        let a = self.anchor.filter(|&a| a != self.cur)?;
        Some(a.min(self.cur)..a.max(self.cur))
    }

    /// Delete the selection, if any; true when something was selected.
    fn take_selection(&mut self) -> bool {
        let sel = self.selection();
        self.anchor = None;
        let Some(r) = sel else { return false };
        self.cur = r.start;
        self.buf.drain(r);
        true
    }

    /// Move the cursor; shift extends the selection, otherwise it's dropped.
    fn move_to(&mut self, to: usize, shift: bool) {
        self.anchor = if shift { self.anchor.or(Some(self.cur)) } else { None };
        self.cur = to.min(self.buf.len());
    }
    fn paste(&mut self, text: &str) {
        let mut chars = text.chars().filter(|c| !c.is_control()).peekable();
        if chars.peek().is_none() {
            return;
        }
        self.take_selection();
        let old_len = self.buf.len();
        self.buf.extend(chars);
        let inserted = self.buf.len() - old_len;
        self.buf[self.cur..].rotate_right(inserted);
        self.cur += inserted;
    }

    pub fn key(&mut self, k: KeyEvent) -> Edit {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let super_key = k.modifiers.contains(KeyModifiers::SUPER);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Esc => return Edit::Cancel,
            KeyCode::Enter => return Edit::Go(self.text()),
            KeyCode::Char('a' | 'A') if super_key && !ctrl => {
                self.anchor = Some(0);
                self.cur = self.buf.len();
            }
            KeyCode::Char('a') if ctrl => self.move_to(0, false),
            KeyCode::Char('e') if ctrl => self.move_to(self.buf.len(), false),
            KeyCode::Char('u') if ctrl => {
                if !self.take_selection() {
                    self.buf.drain(..self.cur);
                    self.cur = 0;
                }
            }
            KeyCode::Char('w') if ctrl => {
                if !self.take_selection() {
                    let end = self.cur;
                    while self.cur > 0 && self.buf[self.cur - 1] == ' ' {
                        self.cur -= 1;
                    }
                    while self.cur > 0 && !URL_SEPS.contains(self.buf[self.cur - 1]) {
                        self.cur -= 1;
                    }
                    self.buf.drain(self.cur..end);
                }
            }
            KeyCode::Char(c) if !ctrl && !super_key && !c.is_control() => {
                self.take_selection();
                self.buf.insert(self.cur, c);
                self.cur += 1;
            }
            KeyCode::Backspace => {
                if !self.take_selection() && self.cur > 0 {
                    self.cur -= 1;
                    self.buf.remove(self.cur);
                }
            }
            KeyCode::Delete => {
                if !self.take_selection() && self.cur < self.buf.len() {
                    self.buf.remove(self.cur);
                }
            }
            KeyCode::Left => self.move_to(self.cur.saturating_sub(1), shift),
            KeyCode::Right => self.move_to(self.cur + 1, shift),
            KeyCode::Home => self.move_to(0, shift),
            KeyCode::End => self.move_to(self.buf.len(), shift),
            _ => {}
        }
        Edit::Editing
    }

    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    /// First visible char so the cursor stays inside a `width`-wide field.
    fn scroll(&self, width: usize) -> usize {
        (self.cur + 1).saturating_sub(width.max(1))
    }

    /// Click at field column `col`: 1 places the cursor, 2 selects the word, 3 everything.
    pub fn click(&mut self, col: usize, width: usize, clicks: u8) {
        let at = (self.scroll(width) + col).min(self.buf.len());
        match clicks {
            1 => self.move_to(at, false),
            2 => {
                let sep = |i: usize| URL_SEPS.contains(self.buf[i]);
                let (mut a, mut b) = (at, at);
                if at < self.buf.len() && sep(at) {
                    b += 1; // on a separator: just that character
                } else {
                    while a > 0 && !sep(a - 1) {
                        a -= 1;
                    }
                    while b < self.buf.len() && !sep(b) {
                        b += 1;
                    }
                }
                (self.anchor, self.cur) = (Some(a), b);
            }
            _ => (self.anchor, self.cur) = (Some(0), self.buf.len()),
        }
    }

    /// Drag to field column `col`, extending the selection from where it started.
    pub fn drag(&mut self, col: usize, width: usize) {
        let at = self.scroll(width) + col;
        self.move_to(at, true);
    }
}

/// Output for the writer thread. Frames are large (hundreds of KB), so writing them
/// on the main loop would make keys, clicks and scrolling wait behind every frame.
enum Out {
    Bytes(Vec<u8>),
    /// `raw`: draw with `KittyStream` (only the changed box); `full`: a whole frame, the screen was cleared.
    Frame {
        pre: Vec<u8>,
        b64: String,
        cols: u16,
        img_cols: u16,
        rows: u16,
        renderer: Renderer,
        raw: bool,
        full: bool,
    },
}

/// Write output in order, but when frames pile up show only the newest. Owns the Kitty
/// double-buffer flip so a dropped frame can't make the next one replace the visible image.
fn spawn_writer() -> (Sender<Out>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Out>();
    let handle = std::thread::spawn(move || {
        let mut flip = false;
        let mut raw_frames = render::KittyStream::default();
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            batch.extend(rx.try_iter());
            let newest = batch.iter().rposition(|o| matches!(o, Out::Frame { .. }));
            // A skipped frame may have been the whole one the screen needed after a clear.
            let any_full = batch.iter().any(|o| matches!(o, Out::Frame { full: true, .. }));
            let mut buf = Vec::new();
            for (i, item) in batch.into_iter().enumerate() {
                match item {
                    Out::Bytes(b) => buf.extend_from_slice(&b),
                    Out::Frame { pre, b64, cols, img_cols, rows, renderer, raw, full } if Some(i) == newest => {
                        buf.extend_from_slice(SYNC_ON.as_bytes());
                        buf.extend_from_slice(&pre);
                        let drawn = match renderer {
                            Renderer::Kitty if raw => raw_frames.draw(&mut buf, &b64, img_cols, rows, full || any_full),
                            Renderer::Kitty => {
                                let (id, old) = if flip { (IMAGE_ID + 1, IMAGE_ID) } else { (IMAGE_ID, IMAGE_ID + 1) };
                                flip = !flip;
                                render::kitty_swap(&mut buf, &b64, id, old, img_cols, rows)
                            }
                            Renderer::Blocks => render::blocks(&mut buf, &b64, PAGE_TOP, cols, rows),
                        };
                        if drawn.is_err() {
                            buf.extend_from_slice(b"\x1b[0m");
                        }
                        buf.extend_from_slice(SYNC_OFF.as_bytes());
                    }
                    Out::Frame { .. } => {} // superseded by a newer frame
                }
            }
            let mut so = stdout().lock();
            if so.write_all(&buf).and_then(|_| so.flush()).is_err() {
                return;
            }
        }
    });
    (tx, handle)
}

struct Term;

impl Term {
    fn enter() -> R<Term> {
        terminal::enable_raw_mode()?;
        let term = Term;
        execute!(stdout(), terminal::EnterAlternateScreen, cursor::Hide, event::EnableMouseCapture, event::EnableBracketedPaste)?;
        Ok(term)
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let mut out = stdout();
        render::kitty_clear(&mut out);
        for id in chrome_ids() {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
        let _ = execute!(out, event::DisableBracketedPaste, event::DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Chrome colors. On herdr desktop they come from its live theme (synced by the
/// `herdr-web-browser-theme` mod), so the view matches the panes around it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Palette {
    bar: Rgb,
    hover: Rgb,
    fg: Rgb,
    muted: Rgb,
    dim: Rgb,
    field: Rgb,
    field_fg: Rgb,
    selection: Rgb,
    ok: Rgb,
    err: Rgb,
}

impl Palette {
    /// herdr desktop's default dark look, for terminals without a synced theme.
    const DEFAULT: Palette = Palette {
        bar: (33, 37, 43),
        hover: (53, 57, 62),
        fg: (238, 238, 239),
        muted: (156, 158, 161),
        dim: (111, 113, 117),
        field: (52, 56, 61),
        field_fg: (238, 238, 239),
        selection: (50, 56, 68),
        ok: (97, 175, 239),
        err: (224, 108, 117),
    };

    fn from_theme(t: &Value) -> Palette {
        let d = Palette::DEFAULT;
        // The terminal's own background, so the bars blend into the pane's padding
        // instead of showing as darker strips; the field and hover sit slightly raised.
        let bar = color(t, "background", (0, 0, 0)).unwrap_or(d.bar);
        let field = color(t, "selected", bar).unwrap_or(d.field);
        Palette {
            bar,
            hover: color(t, "raised", bar).unwrap_or(d.hover),
            fg: color(t, "foreground", bar).unwrap_or(d.fg),
            muted: color(t, "muted", bar).unwrap_or(d.muted),
            dim: color(t, "dim", bar).unwrap_or(d.dim),
            field,
            field_fg: color(t, "foreground", field).unwrap_or(d.field_fg),
            selection: color(t, "selection", field).unwrap_or(d.selection),
            ok: color(t, "done", bar).unwrap_or(d.ok),
            err: color(t, "blocked", bar).unwrap_or(d.err),
        }
    }
}

/// `#rrggbb`, or `#rrggbbaa` blended over `base` (herdr's hover/border are translucent).
fn parse_hex(s: &str, base: Rgb) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
    let a = if h.len() == 8 { byte(6)? as f32 / 255.0 } else { 1.0 };
    let mix = |c: u8, b: u8| (c as f32 * a + b as f32 * (1.0 - a)).round() as u8;
    Some((mix(r, base.0), mix(g, base.1), mix(b, base.2)))
}

fn color(t: &Value, key: &str, base: Rgb) -> Option<Rgb> {
    parse_hex(t[key].as_str()?, base)
}

const TILE_BASE: u32 = 6000;
const TILE_HOVER_BASE: u32 = 6100;
const CAP_LEFT: u32 = 6300;
const CAP_RIGHT: u32 = 6301;
const CONTROL_CAP_BASE: u32 = 6400;

fn control_edge(hot: bool, selected: bool, edge: u32) -> u32 {
    CONTROL_CAP_BASE + (if hot { 2 } else if selected { 1 } else { 0 }) * 3 + edge
}

fn icon_index(icon: Icon) -> u32 {
    icons::ALL.iter().position(|x| *x == icon).unwrap_or(0) as u32
}

fn icon_id(icon: Icon, hovered: bool) -> u32 {
    if hovered { TILE_HOVER_BASE + icon_index(icon) } else { TILE_BASE + icon_index(icon) }
}

/// Every chrome image id, for deleting placements and cleanup.
fn chrome_ids() -> Vec<u32> {
    let mut ids: Vec<u32> = icons::ALL.iter().flat_map(|i| [icon_id(*i, false), icon_id(*i, true)]).collect();
    ids.extend([CAP_LEFT, CAP_RIGHT]);
    ids.extend(CONTROL_CAP_BASE..CONTROL_CAP_BASE + 9);
    ids
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Back,
    Forward,
    Reload,
    Field,
    Zoom(i8),
    Find,
    FindNext(i8),
    NewTab,
    SelectTab(usize),
    CycleTab,
    CloseTab,
    Bookmark,
    Search,
    Suggestion,
    Files,
    DownloadDirectory,
    SendSelection,
    Screenshot,
    Quit,
}

#[derive(Debug, Clone, Copy)]
struct Hit {
    row: u16,
    x0: u16,
    x1: u16,
    act: Act,
}

/// A chrome image slot: (col, row, image id, width in text columns).
type Slot = (u16, u16, u32, u16);

/// One chrome row being painted, with its clickable spans and icon slots.
struct Line {
    buf: Vec<u8>,
    row: u16,
    col: u16,
    hover: Option<(u16, u16)>,
    hits: Vec<Hit>,
    pal: Palette,
    /// Draw icons as Kitty images (else glyphs).
    images: bool,
    /// (col, row, icon, hovered) for each image slot.
    icons: Vec<Slot>,
}

impl Line {
    fn new(row: u16, hover: Option<(u16, u16)>, pal: Palette, images: bool) -> Line {
        let mut buf = Vec::new();
        let _ = queue!(buf, cursor::MoveTo(0, row));
        Line { buf, row, col: 0, hover, hits: Vec::new(), pal, images, icons: Vec::new() }
    }

    fn text(&mut self, s: &str, fg: Rgb, bg: Rgb) {
        let _ = write!(self.buf, "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m{s}", fg.0, fg.1, fg.2, bg.0, bg.1, bg.2);
        self.col += s.chars().count() as u16;
    }

    fn hovered(&self, x0: u16, x1: u16) -> bool {
        self.hover.is_some_and(|(c, r)| r == self.row && (x0..x1).contains(&c))
    }

    /// Three-column tile and a one-column gap; only the tile is clickable.
    fn button(&mut self, icon: Icon, act: Act) {
        let x0 = self.col;
        let hot = self.hovered(x0, x0 + 3);
        let bg = control_bg(self.pal, hot, false);
        if self.images {
            self.icons.push((x0, self.row, icon_id(icon, hot), 3));
            self.text("   ", self.pal.fg, self.pal.bar);
        } else {
            self.text(&format!("[{}]", icon.glyph()), contrast_fg(bg), bg);
        }
        self.hits.push(Hit { row: self.row, x0, x1: self.col, act });
        self.gap();
    }

    fn control(&mut self, cols: u16, label: &str, act: Act, selected: bool) {
        if cols.saturating_sub(self.col) < 5 { return; }
        let start = self.col;
        let width = (cols - start) as usize;
        let text = label.trim();
        let text_len = text.chars().count();
        let len = (text_len + 4).min(width);
        let end = start + len as u16;
        let hot = self.hovered(start, end);
        let bg = control_bg(self.pal, hot, selected);
        let fg = contrast_fg(bg);
        if self.images {
            self.icons.push((self.col, self.row, control_edge(hot, selected, 0), 1));
            self.text(" ", fg, self.pal.bar);
        } else {
            self.text("[", fg, bg);
        }
        if self.images {
            for col in start + 1..end - 1 {
                self.icons.push((col, self.row, control_edge(hot, selected, 1), 1));
            }
        }
        self.text(" ", fg, bg);
        if text_len > len - 4 {
            self.text(&clip(text, len - 4), fg, bg);
        } else {
            self.text(text, fg, bg);
        }
        self.text(" ", fg, bg);
        if self.images {
            self.icons.push((self.col, self.row, control_edge(hot, selected, 2), 1));
            self.text(" ", fg, self.pal.bar);
        } else {
            self.text("]", fg, bg);
        }
        self.hits.push(Hit { row: self.row, x0: start, x1: end, act });
        if self.col < cols { self.gap(); }
    }

    fn editor(&mut self, edit: &UrlEdit, cols: u16) {
        let width = cols.saturating_sub(self.col) as usize;
        let first = edit.scroll(width);
        let selection = edit.selection();
        for i in first..first + width {
            let ch = edit.buf.get(i).copied().unwrap_or(' ');
            let (fg, bg) = if i == edit.cur {
                (self.pal.field, self.pal.field_fg)
            } else if selection.as_ref().is_some_and(|s| s.contains(&i)) {
                (self.pal.field_fg, self.pal.selection)
            } else {
                (self.pal.field_fg, self.pal.field)
            };
            self.text(ch.encode_utf8(&mut [0; 4]), fg, bg);
        }
    }
    fn gap(&mut self) {
        self.text(" ", self.pal.fg, self.pal.bar);
    }

    fn fill(&mut self, to: u16) {
        if to > self.col {
            self.text(&" ".repeat((to - self.col) as usize), self.pal.fg, self.pal.bar);
        }
    }

    fn finish(mut self) -> (Vec<u8>, Vec<Hit>, Vec<Slot>) {
        self.buf.extend_from_slice(b"\x1b[0m");
        (self.buf, self.hits, self.icons)
    }
}

/// Derive control colors from the background, even when theme foregrounds are low contrast.
fn contrast_fg(bg: Rgb) -> Rgb {
    let luminance = bg.0 as u32 * 299 + bg.1 as u32 * 587 + bg.2 as u32 * 114;
    if luminance >= 128_000 { (0, 0, 0) } else { (255, 255, 255) }
}

fn control_bg(pal: Palette, hot: bool, selected: bool) -> Rgb {
    let weight = if hot { 38 } else if selected { 30 } else { 18 };
    let fg = contrast_fg(pal.bar);
    let mix = |a: u8, b: u8| ((a as u16 * (100 - weight) + b as u16 * weight) / 100) as u8;
    (mix(pal.bar.0, fg.0), mix(pal.bar.1, fg.1), mix(pal.bar.2, fg.2))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn terminal_text(text: &str, limit: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(limit).collect()
}

/// A running screencast; stopping joins it so its session's emulation can't
/// clear the next stream's (Chrome resets overrides when a session detaches).
struct Stream {
    target: String,
    size: StreamSize,
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Stream {
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Header: a gap, back/forward/reload (4 columns each), a gap, then the address field.
const FIELD_START: u16 = 14;
/// Address, tools, tabs and the active tool's input, separated by blank rows.
const PAGE_TOP: u16 = 7;

enum Panel {
    None,
    Find { edit: UrlEdit, current: usize, total: usize },
    Search(UrlEdit),
    Files,
}

struct View {
    cfg: Config,
    cells: Cells,
    renderer: Renderer,
    /// Frames as raw RGBA with only the changed box resent (render::KittyStream).
    /// ponytail: herdr desktop only, the one terminal where Kitty frame edits are verified;
    /// probe with `a=q` before enabling it for kitty/Ghostty/WezTerm.
    raw_frames: bool,
    owner: Option<String>,
    /// Root target, distinct from the currently displayed descendant.
    tab: Option<String>,
    page: Option<page::Page>,
    streaming: Option<Stream>,
    generation: u64,
    tx: Sender<Msg>,
    out: Sender<Out>,
    frame: Option<(f64, f64, u16, u16)>,
    /// Last frame received, redrawn after the screen is cleared.
    last_frame: Option<(String, f64, f64)>,
    /// Pane size the screen was last laid out for.
    laid_out: (u16, u16),
    /// Which of the two Kitty image ids holds the frame on screen.
    pal: Palette,
    /// Icons uploaded as Kitty images for the current palette.
    icons_ready: bool,
    /// Modification time of the synced theme file in use.
    theme_at: Option<std::time::SystemTime>,
    /// Chrome bytes as last written; redraw only on change.
    drawn: Vec<u8>,
    hits: Vec<Hit>,
    hover: Option<(u16, u16)>,
    /// Wheel events gathered from one input batch: (x, y, deltaY).
    wheel: Option<(f64, f64, f64)>,
    url: String,
    title: String,
    message: String,
    message_at: Instant,
    url_input: Option<UrlEdit>,
    address: AddressBook,
    suggestions: Vec<Suggestion>,
    suggestion: Option<usize>,
    panel: Panel,
    zoom: u16,
    zoom_session: String,
    tabs: Vec<page::BrowserTab>,
    files: Option<Files>,
    file_target: String,
    download_dir: Option<std::path::PathBuf>,
    file_notice: usize,
    /// When, where, and the click count (1-3) of the last left press.
    last_click: (Instant, u16, u16, u8),
}

impl View {
    fn size() -> (u16, u16) {
        terminal::size().unwrap_or((80, 24))
    }

    fn area() -> (u16, u16) {
        let (c, r) = Self::size();
        // herdr desktop pads terminals on the left only (~14px); the right gets just the
        // leftover sliver. Leaving the last column empty roughly balances the page.
        // ponytail: ±4px of balance, exact centering needs the pane's pixel width.
        let inset = u16::from(std::env::var_os("HC_TERMINAL_ID").is_some());
        (c.saturating_sub(inset).max(1), r.saturating_sub(PAGE_TOP).max(1))
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.message = msg.into();
        self.message_at = Instant::now();
    }

    /// Reconnect only to this view's owner or its own standalone target.
    fn attach(&mut self, initial_url: Option<&str>) -> R<()> {
        let p = match (&self.owner, &self.tab) {
            (Some(owner), tab) => page::resolve(&self.cfg, owner, tab.is_none().then_some(page::HOME_URL))?,
            (None, Some(target)) => page::attach(target)?,
            (None, None) => page::standalone(&self.cfg, initial_url.unwrap_or(page::HOME_URL))?,
        };
        if self.tab.is_none() {
            self.tab = Some(match &self.owner {
                Some(owner) => state::load()?.tabs.get(owner).cloned().ok_or("owner browser root is missing")?,
                None => p.tab.id.clone(),
            });
        }
        self.page = Some(p);
        Ok(())
    }

    fn stream_size(&self) -> StreamSize {
        let (c, r) = Self::area();
        let viewport = ((c as f64 * self.cells.w / self.cells.dpr) as u32, (r as f64 * self.cells.h / self.cells.dpr) as u32);
        let max_frame = match self.renderer {
            Renderer::Kitty => ((viewport.0 as f64 * self.cells.dpr) as u32, (viewport.1 as f64 * self.cells.dpr) as u32),
            // Two pixels per cell; 4x that leaves room for box-averaging.
            Renderer::Blocks => (c as u32 * 4, r as u32 * 8),
        };
        StreamSize { viewport, dpr: self.cells.dpr, max_frame, jpeg: self.raw_frames }
    }

    fn ensure_stream(&mut self) -> R<()> {
        if self.page.is_none() {
            self.attach(None)?;
        }
        if let (Some(p), Some(root)) = (&mut self.page, &self.tab) {
            page::refresh_target(p, root)?;
            if p.root() != root {
                self.tab = Some(p.root().to_owned());
            }
        }
        let target = self.page.as_ref().map(|p| p.tab.id.clone()).unwrap_or_default();
        if self.zoom_session != self.page.as_ref().expect("attached page").session {
            let p = self.page.as_mut().expect("attached page");
            if self.zoom != 100 {
                page_tools::apply_zoom(p, self.zoom)?;
            }
            self.zoom_session.clone_from(&p.session);
        }
        let size = self.stream_size();
        if let Some(st) = &self.streaming
            && st.target == target
            && st.size == size
        {
            return Ok(());
        }
        if let Some(st) = self.streaming.take() {
            st.stop();
        }
        let headless = true;
        // Hidden (background) tabs don't paint, so no frames. Headed, activating also
        // raises Chrome's window over herdr on every reattach, so leave it be there.
        if headless && let Some(p) = self.page.as_mut() {
            let _ = p.activate();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (url, tx, s2, t2) = (chrome::endpoint().ok_or("browser is not running")?, self.tx.clone(), stop.clone(), target.clone());
        self.generation += 1;
        let generation = self.generation;
        let thread = std::thread::spawn(move || stream(url, t2, size, headless, tx, s2, generation));
        self.streaming = Some(Stream { target, size, stop, thread });
        Ok(())
    }

    fn reset(&mut self) {
        self.generation += 1;
        if let Some(st) = self.streaming.take() {
            st.stop();
        }
        self.page = None;
    }

    /// Detect changes to the owner's root, not to its active descendant.
    fn owner_moved(&mut self) -> bool {
        let (Some(o), Some(root)) = (&self.owner, &self.tab) else { return false };
        if let Some(id) = state::load().ok().and_then(|s| s.tabs.get(o).cloned()).filter(|id| id != root) {
            self.tab = Some(id);
            return true;
        }
        false
    }

    fn refresh_header(&mut self) {
        let Some(p) = self.page.as_mut() else { return };
        let Ok(info) = p.info() else { return };
        let url = info["url"].as_str().unwrap_or("");
        let title = info["title"].as_str().unwrap_or("");
        if self.url != url || self.title != title {
            if let Err(e) = self.address.record(url, title) {
                self.say(format!("error: history: {e}"));
            }
            self.url = terminal_text(url, usize::MAX);
        }
        if title != self.title {
            // The pane header (herdr desktop and TUI) shows the terminal title.
            let clean: String = title.chars().filter(|c| !c.is_control()).take(80).collect();
            let name = if clean.is_empty() { "Browser".to_string() } else { format!("Browser · {clean}") };
            self.write(format!("\x1b]2;{name}\x07").into_bytes());
        }
        if let Some(p) = self.page.as_mut() {
            match page::tabs(p) {
                Ok(tabs) => self.tabs = tabs,
                Err(e) => self.say(format!("error: tabs: {e}")),
            }
        }
        if title != self.title {
            self.title = terminal_text(title, 256);
        }
    }

    fn images(&self) -> bool {
        self.renderer == Renderer::Kitty && self.icons_ready
    }

    /// Address field: (first inner column, inner width).
    fn field(&self) -> (u16, usize) {
        let cols = Self::size().0 as usize;
        (FIELD_START + 1, cols.saturating_sub(FIELD_START as usize + 11).max(1))
    }

    fn header(&self) -> (Vec<u8>, Vec<Hit>, Vec<Slot>) {
        let cols = Self::size().0;
        let p = self.pal;
        let mut l = Line::new(0, self.hover, p, self.images());
        l.gap();
        l.button(Icon::Back, Act::Back);
        l.button(Icon::Forward, Act::Forward);
        l.button(Icon::Reload, Act::Reload);
        l.gap();
        let (start, width) = self.field();
        let x0 = l.col;
        let rounded = l.images;
        if rounded {
            l.icons.push((l.col, 0, CAP_LEFT, 1));
            l.text(" ", p.field_fg, p.bar);
        } else {
            l.text(" ", p.field_fg, p.field);
        }
        match &self.url_input {
            Some(e) => {
                let first = e.scroll(width);
                for i in 0..width {
                    let idx = first + i;
                    let ch = e.buf.get(idx).copied().unwrap_or(' ');
                    let (fg, bg) = if idx == e.cur {
                        (p.field, p.field_fg) // block cursor
                    } else if e.selection().is_some_and(|r| r.contains(&idx)) {
                        (p.field_fg, p.selection)
                    } else {
                        (p.field_fg, p.field)
                    };
                    l.text(&ch.to_string(), fg, bg);
                }
            }
            None => {
                let (text, fg) = self.field_text(start, width);
                let shown = clip(&text, width);
                let pad = width - shown.chars().count();
                l.text(&format!("{shown}{}", " ".repeat(pad)), fg, p.field);
            }
        }
        if rounded {
            l.icons.push((l.col, 0, CAP_RIGHT, 1));
            l.text(" ", p.field_fg, p.bar);
        } else {
            l.text(" ", p.field_fg, p.field);
        }
        l.hits.push(Hit { row: 0, x0, x1: l.col, act: Act::Field });
        l.gap();
        l.button(Icon::Selection, Act::SendSelection);
        l.button(Icon::Camera, Act::Screenshot);
        l.fill(cols);
        let (mut bytes, mut hits, mut icons) = l.finish();
        for row in 1..PAGE_TOP.min(Self::size().1) {
            let line = self.tool_line(row, cols);
            let (mut text, mut buttons, mut control_icons) = line.finish();
            bytes.append(&mut text);
            hits.append(&mut buttons);
            icons.append(&mut control_icons);
        }
        (bytes, hits, icons)
    }

    fn tool_line(&self, row: u16, cols: u16) -> Line {
        let mut l = Line::new(row, self.hover, self.pal, self.images());
        if matches!(row, 1 | 3 | 5) {
            l.fill(cols);
            return l;
        }
        if cols > 0 && (row != 4 || cols > 12) {
            l.gap();
        }
        if row == 2 {
            l.control(cols, " − ", Act::Zoom(-1), false);
            l.control(cols, &format!(" {}% ", self.zoom), Act::Zoom(0), false);
            l.control(cols, " + ", Act::Zoom(1), false);
            l.control(cols, " Find ", Act::Find, matches!(self.panel, Panel::Find { .. }));
            l.control(cols, " Bookmark ", Act::Bookmark, false);
            l.control(cols, " Search ", Act::Search, matches!(self.panel, Panel::Search(_)));
            l.control(cols, " Files ", Act::Files, matches!(self.panel, Panel::Files));
        } else if row == 4 {
            // Reserve the right edge for quitting the view, not closing a browser tab.
            let tab_end = cols.saturating_sub(17);
            l.control(tab_end, " + Tab ", Act::NewTab, false);
            l.control(tab_end, " Next ", Act::CycleTab, false);
            l.control(tab_end, " Close ", Act::CloseTab, false);

            let first = self.tabs.iter().position(|tab| tab.active).unwrap_or(0);
            let fits = (tab_end.saturating_sub(l.col) as usize / 13).max(1);
            let start = first.saturating_sub(fits - 1);
            for (i, tab) in self.tabs.iter().enumerate().skip(start).take(fits) {
                let title = terminal_text(&tab.title, 10);
                let title = if title.is_empty() { "New tab" } else { &title };
                l.control(tab_end, &format!(" {title} "), Act::SelectTab(i), tab.active);
            }
            l.fill(tab_end);
            l.control(cols, " × Close pane ", Act::Quit, false);
        } else if self.url_input.is_some() {
            if let Some(s) = self.suggestions.get(self.suggestion.unwrap_or(0)) {
                let title = terminal_text(&s.title, 18);
                let address = terminal_text(&s.url, cols as usize);
                let hint = if s.bookmarked { "★" } else { "↳" };
                l.control(cols, &format!(" {hint} {title} {address} "), Act::Suggestion, self.suggestion.is_some());
            }
        } else {
            match &self.panel {
                Panel::None => {}
                Panel::Find { edit, current, total } => {
                    l.control(cols, " Prev ", Act::FindNext(-1), false);
                    l.control(cols, " Next ", Act::FindNext(1), false);
                    l.control(cols, " Done ", Act::Find, false);
                    l.text(&format!(" {current}/{total}  Find: "), self.pal.muted, self.pal.bar);
                    l.editor(edit, cols);
                }
                Panel::Search(edit) => {
                    l.text(" Search URL ({query}): ", self.pal.muted, self.pal.bar);
                    l.editor(edit, cols);
                }
                Panel::Files => {
                    l.control(cols, " Download folder ", Act::DownloadDirectory, false);
                    let history = self.files.as_ref().map(Files::history);
                    if let Some(notice) = history.and_then(|h| h.get(self.file_notice)) {
                        let position = format!(" {}/{}  ", self.file_notice + 1, history.map_or(0, |h| h.len()));
                        l.text(&position, self.pal.muted, self.pal.bar);
                        let shown = terminal_text(notice, cols.saturating_sub(l.col) as usize);
                        l.text(&shown, self.pal.fg, self.pal.bar);
                    } else if let Some(dir) = &self.download_dir {
                        let shown = terminal_text(&dir.to_string_lossy(), cols.saturating_sub(l.col) as usize);
                        l.text(&shown, self.pal.muted, self.pal.bar);
                    }
                }
            }
        }
        l.fill(cols);
        l
    }

    /// The idle address field doubles as the status line: a hovered button's name,
    /// else the latest message for a few seconds, else the URL.
    fn field_text(&self, start: u16, width: usize) -> (String, Rgb) {
        let p = self.pal;
        let hovered = self.hover.and_then(|(c, r)| self.hits.iter().find(|h| h.row == r && (h.x0..h.x1).contains(&c)));
        let recent = !self.message.is_empty() && self.message_at.elapsed() < Duration::from_secs(6);
        let bookmark_feedback = self.message.starts_with("Bookmark") || self.message.starts_with("error: bookmark");
        if let Some(h) = hovered.filter(|h| h.act != Act::Field && !(h.act == Act::Bookmark && recent && bookmark_feedback)) {
            return (self.describe(h.act).to_string(), p.fg);
        }
        if recent {
            let bad = self.message.starts_with("error") || self.message.starts_with("view lost");
            return (self.message.clone(), if bad { p.err } else { p.ok });
        }
        (self.url.clone(), if self.hovered_field(start, width) { p.fg } else { p.muted })
    }

    fn hovered_field(&self, start: u16, width: usize) -> bool {
        self.hover.is_some_and(|(c, r)| r == 0 && c >= start && ((c - start) as usize) < width)
    }

    /// What a control does, shown while it's hovered.
    fn describe(&self, act: Act) -> &'static str {
        match act {
            Act::Back => "Back",
            Act::Forward => "Forward",
            Act::Reload => "Reload",
            Act::Field => "Edit address",
            Act::Zoom(-1) => "Zoom out",
            Act::Zoom(1) => "Zoom in",
            Act::Zoom(_) => "Reset page zoom",
            Act::Find => "Find in page",
            Act::FindNext(_) => "Find another match",
            Act::NewTab => "New browser tab",
            Act::SelectTab(_) => "Switch browser tab",
            Act::CycleTab => "Next browser tab",
            Act::CloseTab => "Close browser tab",
            Act::Bookmark => "Toggle bookmark",
            Act::Search => "Set search engine URL",
            Act::Suggestion => "Open suggested address",
            Act::Files => "File activity",
            Act::DownloadDirectory => "Choose profile-wide download folder",
            Act::SendSelection => "Send selection to agent",
            Act::Screenshot => "Send screenshot to agent",
            Act::Quit => "Close browser pane (Ctrl-Q)",
        }
    }

    /// Upload every icon, plain and hovered, once per palette.
    fn upload_icons(&mut self) -> R<()> {
        if self.renderer != Renderer::Kitty {
            return Ok(());
        }
        use base64::Engine;
        let h = (self.cells.h * 2.0).round() as u32;
        let mut out = Vec::new();
        let b64 = |png: Vec<u8>| base64::engine::general_purpose::STANDARD.encode(png);
        let cell_w = (self.cells.w * 2.0).round() as u32;
        for icon in icons::ALL {
            for hot in [false, true] {
                let bg = control_bg(self.pal, hot, false);
                let tile = icons::pill_png(icon, contrast_fg(bg), bg, contrast_fg(self.pal.bar), cell_w * 3, h)?;
                render::kitty_upload(&mut out, &b64(tile), icon_id(icon, hot))?;
            }
        }
        render::kitty_upload(&mut out, &b64(icons::cap_png(true, self.pal.field, cell_w, h)?), CAP_LEFT)?;
        render::kitty_upload(&mut out, &b64(icons::cap_png(false, self.pal.field, cell_w, h)?), CAP_RIGHT)?;
        for (hot, selected) in [(false, false), (false, true), (true, false)] {
            let bg = control_bg(self.pal, hot, selected);
            for (edge, id) in [(Some(true), 0), (None, 1), (Some(false), 2)] {
                let png = icons::control_edge_png(edge, bg, contrast_fg(self.pal.bar), cell_w, h)?;
                render::kitty_upload(&mut out, &b64(png), control_edge(hot, selected, id))?;
            }
        }
        self.write(out);
        self.icons_ready = true;
        self.drawn.clear();
        Ok(())
    }

    fn draw_chrome(&mut self) -> R<()> {
        let (h, hits, slots) = self.header();
        self.hits = hits;
        let mut out = Vec::with_capacity(h.len() + 64 * slots.len() + 16);
        out.extend_from_slice(SYNC_ON.as_bytes());
        out.extend_from_slice(&h);
        if !slots.is_empty() {
            // Re-place every icon: drop old placements (keeping the images), put new ones.
            for id in chrome_ids() {
                write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\")?;
            }
            for (n, (col, row, id, cols)) in slots.iter().enumerate() {
                queue!(out, cursor::MoveTo(*col, *row))?;
                write!(out, "\x1b_Ga=p,i={id},p={},c={},r=1,C=1,q=2\x1b\\", n + 1, self.cells.img_cols(*cols))?;
            }
        }
        out.extend_from_slice(SYNC_OFF.as_bytes());
        if out == self.drawn {
            return Ok(());
        }
        self.write(out.clone());
        self.drawn = out;
        Ok(())
    }

    /// Pick up a changed herdr desktop theme (light/dark switch, theme change).
    fn reload_theme(&mut self) -> R<()> {
        let path = state::state_dir().join("theme.json");
        let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return Ok(()) };
        if self.theme_at == Some(modified) {
            return Ok(());
        }
        self.theme_at = Some(modified);
        let t: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        let pal = Palette::from_theme(&t);
        if pal != self.pal || !self.icons_ready {
            self.pal = pal;
            self.upload_icons()?;
        }
        Ok(())
    }

    /// Re-lay everything out when the pane size changed. Panes are often resized right
    /// after they open, sometimes before a Resize event can be read, so poll for it.
    fn relayout_if_resized(&mut self, force: bool) -> R<()> {
        let size = Self::size();
        if !force && size == self.laid_out {
            return Ok(());
        }
        self.laid_out = size;
        let mut clear = Vec::new();
        render::kitty_clear(&mut clear);
        queue!(clear, terminal::Clear(terminal::ClearType::All))?;
        self.write(clear);
        self.frame = None;
        self.drawn.clear();
        // Clearing the screen frees images nothing shows, icons included: upload them again.
        if self.icons_ready {
            self.upload_icons()?;
        }
        self.draw_chrome()?;
        if let Some((data, w, h)) = self.last_frame.clone() {
            self.draw_frame(&data, w, h)?;
        }
        Ok(())
    }

    fn write(&self, bytes: Vec<u8>) {
        let _ = self.out.send(Out::Bytes(bytes));
    }

    fn draw_frame(&mut self, data: &str, w: f64, h: f64) -> R<()> {
        let (cols, rows) = Self::area();
        let (c, r) = render::fit(w, h, cols, rows, self.cells.aspect());
        let mut pre = Vec::new();
        if self.frame.is_some_and(|(_, _, pc, pr)| (pc, pr) != (c, r)) {
            for row in PAGE_TOP..PAGE_TOP + rows {
                queue!(pre, cursor::MoveTo(0, row), terminal::Clear(terminal::ClearType::CurrentLine))?;
            }
        }
        queue!(pre, cursor::MoveTo(0, PAGE_TOP))?;
        let (raw, full) = (self.raw_frames, self.frame.is_none());
        let frame = Out::Frame { pre, b64: data.to_string(), cols: c, img_cols: self.cells.img_cols(c), rows: r, renderer: self.renderer, raw, full };
        let _ = self.out.send(frame);
        self.frame = Some((w, h, c, r));
        if self.last_frame.as_ref().is_none_or(|(d, _, _)| d != data) {
            self.last_frame = Some((data.to_string(), w, h));
        }
        Ok(())
    }

    /// Terminal cell -> page CSS px, if the cell is on the frame.
    fn to_page(&self, col: u16, row: u16) -> Option<(f64, f64)> {
        let (w, h, c, r) = self.frame?;
        let y = row.checked_sub(PAGE_TOP)?;
        (col < c && y < r).then(|| ((col as f64 + 0.5) / c as f64 * w, (y as f64 + 0.5) / r as f64 * h))
    }

    fn navigate(&mut self, url: &str) {
        if let Some(p) = self.page.as_mut() {
            match p.navigate(url) {
                Ok(_) => self.message.clear(),
                Err(e) => self.say(format!("error: {e}")),
            }
        }
        self.refresh_header();
    }

    fn history(&mut self, delta: i32) {
        if let Some(p) = self.page.as_mut() {
            let r = match delta {
                0 => p.reload(),
                d if d < 0 => p.back(),
                _ => p.forward(),
            };
            if let Err(e) = r {
                self.say(format!("error: {e}"));
            }
        }
        self.refresh_header();
    }

    fn mouse(&mut self, kind: &str, x: f64, y: f64, buttons: u8, clicks: u8, wheel: f64) -> R<()> {
        let Some(p) = self.page.as_mut() else { return Ok(()) };
        let mut params = json!({ "type": kind, "x": x, "y": y, "button": if buttons > 0 || kind != "mouseMoved" { "left" } else { "none" }, "buttons": buttons, "clickCount": clicks });
        if kind == "mouseWheel" {
            params = json!({ "type": kind, "x": x, "y": y, "deltaX": 0, "deltaY": wheel });
        }
        let sid = p.session.clone();
        p.cdp.call("Input.dispatchMouseEvent", params, Some(&sid))?;
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> R<()> {
        let Some(p) = self.page.as_mut() else { return Ok(()) };
        let sid = p.session.clone();
        let named = |key: &str, code: &str, vk: u32| json!({ "key": key, "code": code, "windowsVirtualKeyCode": vk });
        let base = match k.code {
            KeyCode::Enter => json!({ "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r" }),
            KeyCode::Backspace => named("Backspace", "Backspace", 8),
            KeyCode::Tab => named("Tab", "Tab", 9),
            KeyCode::Esc => named("Escape", "Escape", 27),
            KeyCode::Left => named("ArrowLeft", "ArrowLeft", 37),
            KeyCode::Up => named("ArrowUp", "ArrowUp", 38),
            KeyCode::Right => named("ArrowRight", "ArrowRight", 39),
            KeyCode::Down => named("ArrowDown", "ArrowDown", 40),
            KeyCode::Delete => named("Delete", "Delete", 46),
            KeyCode::Home => named("Home", "Home", 36),
            KeyCode::End => named("End", "End", 35),
            KeyCode::PageUp => named("PageUp", "PageUp", 33),
            KeyCode::PageDown => named("PageDown", "PageDown", 34),
            KeyCode::Char(c) if !c.is_control() => {
                let s = c.to_string();
                if c.is_ascii_alphabetic() {
                    let upper = c.to_ascii_uppercase();
                    json!({ "key": s, "code": format!("Key{upper}"), "windowsVirtualKeyCode": upper as u32, "text": s, "unmodifiedText": s })
                } else if c.is_ascii_digit() {
                    json!({ "key": s, "code": format!("Digit{c}"), "windowsVirtualKeyCode": c as u32, "text": s, "unmodifiedText": s })
                } else {
                    json!({ "key": s, "text": s, "unmodifiedText": s })
                }
            }
            _ => return Ok(()),
        };
        let mut mods = 0;
        if k.modifiers.contains(KeyModifiers::ALT) {
            mods |= 1;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            mods |= 2;
        }
        if k.modifiers.contains(KeyModifiers::SUPER) {
            mods |= 4;
        }
        if k.modifiers.contains(KeyModifiers::SHIFT) {
            mods |= 8;
        }
        let shortcut = k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
        for kind in ["keyDown", "keyUp"] {
            let mut ev = base.clone();
            ev["type"] = json!(if shortcut && kind == "keyDown" { "rawKeyDown" } else { kind });
            ev["modifiers"] = json!(mods);
            if shortcut || kind == "keyUp" {
                if let Some(o) = ev.as_object_mut() {
                    o.remove("text");
                    o.remove("unmodifiedText");
                }
            }
            p.cdp.call("Input.dispatchKeyEvent", ev, Some(&sid))?;
        }
        Ok(())
    }

    fn on_paste(&mut self, text: &str) -> R<bool> {
        if let Some(edit) = self.url_input.as_mut() {
            edit.paste(text);
            self.update_suggestions();
        } else if let Panel::Find { edit, .. } | Panel::Search(edit) = &mut self.panel {
            edit.paste(text);
        } else if let Some(p) = self.page.as_mut() {
            let sid = p.session.clone();
            p.cdp.call("Input.insertText", json!({ "text": text }), Some(&sid))?;
        }
        Ok(true)
    }

    fn send(&mut self, kind: SendKind) {
        let Some(owner) = self.owner.clone() else {
            self.say("Open from an agent pane to send");
            return;
        };
        self.say("sending…");
        let _ = self.draw_chrome();
        match agent::send_from_browser(kind, &owner, &self.cfg) {
            Ok(()) => self.say("sent to agent"),
            Err(e) => self.say(format!("error: {e}")),
        }
    }

    fn update_suggestions(&mut self) {
        self.suggestion = None;
        self.suggestions = self.url_input.as_ref()
            .map_or_else(Vec::new, |edit| self.address.suggest(&edit.text()));
    }

    fn open_address(&mut self, input: &str) -> R<()> {
        if !input.trim().is_empty() {
            let destination = self.address.destination(input)?;
            self.navigate(&destination);
        }
        Ok(())
    }

    fn find(&mut self, step: i8) -> R<()> {
        if let Panel::Find { edit, current, total } = &mut self.panel
            && let Some(p) = self.page.as_mut()
        {
            let result = page_tools::find(p, &edit.text(), step)?;
            *current = result.current as usize;
            *total = result.total as usize;
        }
        Ok(())
    }

    fn close_panel(&mut self) -> R<()> {
        if matches!(self.panel, Panel::Find { .. }) && let Some(p) = self.page.as_mut() {
            page_tools::find(p, "", 0)?;
        }
        self.panel = Panel::None;
        Ok(())
    }

    fn switch_tab(&mut self, action: Act) -> R<()> {
        self.close_panel()?;
        self.url_input = None;
        let Some(p) = self.page.as_mut() else { return Ok(()) };
        match action {
            Act::NewTab => page::new_tab(p, None)?,
            Act::SelectTab(index) => {
                if let Some(tab) = self.tabs.get(index) {
                    page::select_tab(p, &tab.id)?;
                }
            }
            Act::CycleTab if self.tabs.len() > 1 => {
                let next = self.tabs.iter().position(|tab| tab.active).map_or(0, |i| (i + 1) % self.tabs.len());
                page::select_tab(p, &self.tabs[next].id)?;
            }
            Act::CloseTab => page::close_tab(p)?,
            _ => {}
        }
        self.tab = Some(p.root().to_owned());
        self.ensure_stream()?;
        self.refresh_header();
        if action == Act::NewTab {
            self.url_input = Some(UrlEdit::new(""));
            self.update_suggestions();
        }
        Ok(())
    }

    fn service_files(&mut self) -> R<()> {
        let Some(page) = self.page.as_ref() else { return Ok(()) };
        let target = &page.tab.id;
        if self.files.is_none() {
            self.files = Some(Files::new()?);
        }
        let files = self.files.as_mut().expect("initialized files");
        if self.file_target != *target {
            files.attach(target)?;
            self.file_target.clone_from(target);
        }
        if let Some(notice) = files.poll()?.pop() {
            self.file_notice = files.history().len().saturating_sub(1);
            self.say(notice);
        }
        Ok(())
    }

    fn file_input(&mut self) {
        if let Some(files) = self.files.as_mut() {
            files.note_user_input();
        }
    }

    /// Returns false to quit.
    fn act(&mut self, act: Act, col: u16) -> R<bool> {
        match act {
            Act::Back => self.history(-1),
            Act::Forward => self.history(1),
            Act::Reload => self.history(0),
            Act::Field => {
                let (start, width) = self.field();
                match self.url_input.as_mut() {
                    Some(e) => e.click(col.saturating_sub(start) as usize, width, 1),
                    None => self.url_input = Some(UrlEdit::new(&self.url)),
                }
                self.update_suggestions();
            }
            Act::Zoom(direction) => {
                if let Some(p) = self.page.as_mut() {
                    let zoom = page_tools::step_zoom(self.zoom, direction);
                    page_tools::apply_zoom(p, zoom)?;
                    self.zoom = zoom;
                    self.zoom_session.clone_from(&p.session);
                }
            }
            Act::Find => {
                let closing = matches!(self.panel, Panel::Find { .. });
                self.url_input = None;
                self.close_panel()?;
                if !closing {
                    self.panel = Panel::Find { edit: UrlEdit::new(""), current: 0, total: 0 };
                }
            }
            Act::FindNext(step) => self.find(step)?,
            Act::NewTab | Act::SelectTab(_) | Act::CycleTab | Act::CloseTab => self.switch_tab(act)?,
            Act::Bookmark => {
                let info = self.page.as_mut().map(|p| p.info());
                match info {
                    Some(Ok(info)) => {
                        let url = info["url"].as_str().unwrap_or("");
                        let title = info["title"].as_str().unwrap_or("");
                        match self.address.toggle_bookmark(url, title) {
                            Ok(Some(saved)) => {
                                self.say(if saved { "Bookmark added" } else { "Bookmark removed" });
                                self.update_suggestions();
                            }
                            Ok(None) => self.say("Bookmark: open a web page without URL credentials first"),
                            Err(e) => self.say(format!("error: bookmark: {}", terminal_text(&e.to_string(), 200))),
                        }
                    }
                    Some(Err(_)) => self.say("error: bookmark: current page unavailable"),
                    None => self.say("error: bookmark: no page available"),
                }
            }
            Act::Search => {
                let closing = matches!(self.panel, Panel::Search(_));
                self.url_input = None;
                self.close_panel()?;
                if !closing {
                    self.panel = Panel::Search(UrlEdit::new(self.address.search_template()));
                }
            }
            Act::Suggestion => {
                let index = self.suggestion.unwrap_or(0);
                if let Some(item) = self.suggestions.get(index) {
                    let url = item.url.clone();
                    self.url_input = None;
                    self.navigate(&url);
                }
            }
            Act::Files => {
                let closing = matches!(self.panel, Panel::Files);
                self.url_input = None;
                self.close_panel()?;
                if !closing {
                    self.service_files()?;
                    self.download_dir = self.files.as_ref().map(Files::download_directory).transpose()?;
                    self.panel = Panel::Files;
                }
            }
            Act::DownloadDirectory => {
                self.service_files()?;
                if let Some(files) = self.files.as_mut()
                    && let Some(path) = files.choose_download_directory()?
                {
                    self.say(format!("Download folder: {}", terminal_text(&path.to_string_lossy(), 512)));
                    self.download_dir = Some(path);
                }
            }
            Act::SendSelection => self.send(SendKind::Selection),
            Act::Screenshot => self.send(SendKind::Screenshot),
            Act::Quit => return Ok(false),
        }
        Ok(true)
    }

    /// Returns false to quit.
    fn on_key(&mut self, k: KeyEvent) -> R<bool> {
        let mods = k.modifiers;
        let ctrl = mods.contains(KeyModifiers::CONTROL) && !mods.intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
        let command = mods.contains(KeyModifiers::SUPER) && !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let tool = match k.code {
            KeyCode::F(2) => Some(Act::Find),
            KeyCode::F(3) => Some(Act::FindNext(1)),
            KeyCode::F(4) => Some(Act::FindNext(-1)),
            KeyCode::F(5) => Some(Act::Zoom(-1)),
            KeyCode::F(6) => Some(Act::Zoom(1)),
            KeyCode::F(7) => Some(Act::Zoom(0)),
            KeyCode::F(8) => Some(Act::NewTab),
            KeyCode::F(9) => Some(Act::CycleTab),
            KeyCode::F(10) => Some(Act::CloseTab),
            KeyCode::F(11) => Some(Act::Bookmark),
            KeyCode::F(12) => Some(Act::Files),
            KeyCode::Char('+' | '=') if command => Some(Act::Zoom(1)),
            KeyCode::Char('-') if command => Some(Act::Zoom(-1)),
            KeyCode::Char('0') if command => Some(Act::Zoom(0)),
            KeyCode::Char('f' | 'F') if command => Some(Act::Find),
            _ => None,
        };
        if let Some(action) = tool {
            return self.act(action, 0);
        }
        if (ctrl || command) && !mods.contains(KeyModifiers::SHIFT) {
            let action = match k.code {
                KeyCode::Char('l' | 'L') => {
                    match self.url_input.as_mut() {
                        Some(edit) => {
                            edit.anchor = Some(0);
                            edit.cur = edit.buf.len();
                        }
                        None => self.url_input = Some(UrlEdit::new(&self.url)),
                    }
                    self.update_suggestions();
                    return Ok(true);
                }
                KeyCode::Char('r' | 'R') => Some(Act::Reload),
                KeyCode::Char('[') if command => Some(Act::Back),
                KeyCode::Char(']') if command => Some(Act::Forward),
                KeyCode::Char('b') if ctrl => Some(Act::Back),
                KeyCode::Char('f') if ctrl => Some(Act::Forward),
                KeyCode::Char('q') if ctrl => Some(Act::Quit),
                KeyCode::Char('s') if ctrl && self.url_input.is_none() => Some(Act::SendSelection),
                KeyCode::Char('x') if ctrl && self.url_input.is_none() => Some(Act::Screenshot),
                _ => None,
            };
            if let Some(action) = action {
                self.url_input = None;
                return self.act(action, 0);
            }
        }
        if let Some(edit) = self.url_input.as_mut() {
            if !self.suggestions.is_empty() && matches!(k.code, KeyCode::Up | KeyCode::Down) {
                let len = self.suggestions.len();
                self.suggestion = Some(self.suggestion.map_or(
                    if k.code == KeyCode::Up { len - 1 } else { 0 },
                    |i| if k.code == KeyCode::Up { (i + len - 1) % len } else { (i + 1) % len },
                ));
                return Ok(true);
            }
            match edit.key(k) {
                Edit::Editing => self.update_suggestions(),
                Edit::Cancel => self.url_input = None,
                Edit::Go(url) => {
                    if self.suggestion.is_some() {
                        return self.act(Act::Suggestion, 0);
                    }
                    self.url_input = None;
                    self.open_address(&url)?;
                }
            }
            return Ok(true);
        }
        match &mut self.panel {
            Panel::Find { edit, .. } => match edit.key(k) {
                Edit::Editing => {}
                Edit::Cancel => self.close_panel()?,
                Edit::Go(_) => self.find(if mods.contains(KeyModifiers::SHIFT) { -1 } else { 0 })?,
            },
            Panel::Search(edit) => match edit.key(k) {
                Edit::Editing => {}
                Edit::Cancel => self.close_panel()?,
                Edit::Go(template) => {
                    if let Err(e) = self.address.set_search_template(template.trim()) {
                        self.say(format!("error: {e}"));
                    } else {
                        self.panel = Panel::None;
                        self.say("Search engine saved");
                    }
                }
            },
            Panel::Files => match k.code {
                KeyCode::Esc => self.close_panel()?,
                KeyCode::Enter => return self.act(Act::DownloadDirectory, 0),
                KeyCode::Up => self.file_notice = self.file_notice.saturating_sub(1),
                KeyCode::Down => {
                    let len = self.files.as_ref().map_or(0, |files| files.history().len());
                    self.file_notice = (self.file_notice + 1).min(len.saturating_sub(1));
                }
                _ => {}
            },
            Panel::None => {
                if matches!(k.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    self.file_input();
                }
                self.key(k)?;
            }
        }
        Ok(true)
    }

    /// Returns false to quit.
    fn on_mouse(&mut self, m: event::MouseEvent) -> R<bool> {
        // The spacer row under the address bar belongs to the chrome too.
        let on_chrome = m.row < PAGE_TOP;
        if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) {
            self.hover = on_chrome.then_some((m.column, m.row));
        }
        let mut clicks = 1;
        if m.kind == MouseEventKind::Down(MouseButton::Left) {
            let (t, c, r, n) = self.last_click;
            let again = t.elapsed() < Duration::from_millis(400) && (c, r) == (m.column, m.row);
            clicks = if again { n % 3 + 1 } else { 1 };
            self.last_click = (Instant::now(), m.column, m.row, clicks);
        }
        if on_chrome {
            let (start, width) = self.field();
            let col = m.column.saturating_sub(start) as usize;
            if m.kind == MouseEventKind::Drag(MouseButton::Left)
                && m.row == 0
                && let Some(e) = self.url_input.as_mut()
            {
                e.drag(col, width);
            }
            if m.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(h) = self.hits.iter().find(|h| h.row == m.row && (h.x0..h.x1).contains(&m.column)).copied()
            {
                if let (Act::Field, Some(e)) = (h.act, self.url_input.as_mut()) {
                    e.click(col, width, clicks);
                    return Ok(true);
                }
                return self.act(h.act, m.column);
            }
            return Ok(true);
        }
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.url_input = None; // clicking the page leaves the address bar
            self.close_panel()?;
        }
        let Some((x, y)) = self.to_page(m.column, m.row) else { return Ok(true) };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.file_input();
                self.mouse("mouseMoved", x, y, 0, 0, 0.0)?;
                self.mouse("mousePressed", x, y, 1, clicks, 0.0)?;
            }
            MouseEventKind::Up(MouseButton::Left) => self.mouse("mouseReleased", x, y, 0, 1, 0.0)?,
            MouseEventKind::Drag(MouseButton::Left) => self.mouse("mouseMoved", x, y, 1, 0, 0.0)?,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d = if m.kind == MouseEventKind::ScrollDown { 120.0 } else { -120.0 };
                let sum = self.wheel.map_or(0.0, |w| w.2);
                self.wheel = Some((x, y, sum + d));
            }
            _ => {}
        }
        Ok(true)
    }
}

pub fn run(cfg: Config, owner: Option<String>, start_url: Option<&str>) -> R<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let _term = Term::enter()?;
    let (cells, renderer) = render::detect(&cfg);
    let (out, writer) = spawn_writer();
    let mut v = View {
        cfg,
        cells,
        renderer,
        raw_frames: renderer == Renderer::Kitty && std::env::var_os("HC_TERMINAL_ID").is_some(),
        owner,
        tab: None,
        page: None,
        streaming: None,
        generation: 0,
        tx,
        out,
        frame: None,
        last_frame: None,
        laid_out: (0, 0),
        pal: Palette::DEFAULT,
        icons_ready: false,
        theme_at: None,
        drawn: Vec::new(),
        hits: Vec::new(),
        hover: None,
        wheel: None,
        url: String::new(),
        title: "starting browser…".into(),
        message: String::new(),
        message_at: Instant::now(),
        url_input: None,
        address: AddressBook::load()?,
        suggestions: Vec::new(),
        suggestion: None,
        panel: Panel::None,
        zoom: 100,
        zoom_session: String::new(),
        tabs: Vec::new(),
        files: None,
        file_target: String::new(),
        download_dir: None,
        file_notice: 0,
        last_click: (Instant::now(), u16::MAX, u16::MAX, 0),
    };
    if v.renderer == Renderer::Kitty {
        let _ = v.upload_icons();
    }
    // Initial failures are fatal and reach stderr after the terminal is restored.
    // Only established views should retry lost connections in the event loop.
    let result = (|| -> R<()> {
        v.attach(start_url)?;
        v.ensure_stream()?;
        if let Err(e) = v.service_files() {
            v.say(format!("error: file handling: {e}"));
        }
        event_loop(&mut v, &rx)
    })();
    if let Some(stream) = v.streaming.take() {
        stream.stop();
    }
    if v.owner.is_none() {
        if v.page.is_none() {
            if let Some(target) = &v.tab {
                v.page = page::attach(target).ok();
            }
        }
        if let (Some(p), Some(root)) = (v.page.as_mut(), v.tab.as_deref()) {
            if let Err(e) = page::close_target(p, root) {
                eprintln!("herdr-browser: failed to close standalone target: {e}");
            }
        }
    }
    // Let the writer finish before the terminal is restored.
    drop(v);
    let _ = writer.join();
    result
}

fn event_loop(v: &mut View, rx: &Receiver<Msg>) -> R<()> {
    let mut last_tick = Instant::now() - Duration::from_secs(10);
    // A desktop terminal can receive its first image before it is attached to a pane.
    // Retry only during startup, using a frame from the current stream generation.
    let mut first_frame_at: Option<Instant> = None;
    let mut painted_generation = None;
    let mut startup_redraw = 0;
    const STARTUP_REDRAWS_MS: [u64; 4] = [500, 1000, 2000, 4000];
    loop {
        v.relayout_if_resized(false)?;
        if let Some(start) = first_frame_at
            && startup_redraw < STARTUP_REDRAWS_MS.len()
            && start.elapsed() >= Duration::from_millis(STARTUP_REDRAWS_MS[startup_redraw])
        {
            startup_redraw += 1;
            if painted_generation == Some(v.generation) {
                v.relayout_if_resized(true)?;
            }
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            if let Err(e) = v.reload_theme() {
                v.say(format!("error: theme: {e}"));
            }
            if v.owner_moved() {
                v.reset();
            }
            match v.ensure_stream() {
                Ok(()) => v.refresh_header(),
                Err(e) => {
                    v.reset();
                    v.say(format!("view lost: {e}; reconnecting"));
                }
            }
            if let Err(e) = v.service_files() {
                v.files = None;
                v.file_target.clear();
                v.say(format!("error: file handling: {e}"));
            }
            v.draw_chrome()?;
        }
        let mut latest = None;
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Frame { generation, data, w, h } if generation == v.generation => latest = Some((data, w, h)),
                Msg::Gone { generation, error } if generation == v.generation => {
                    v.reset();
                    v.say(format!("view lost: {error}; reattaching"));
                }
                _ => {}
            }
        }
        if let Some((data, w, h)) = latest {
            match v.draw_frame(&data, w, h) {
                Ok(()) => {
                    painted_generation = Some(v.generation);
                    if v.raw_frames && first_frame_at.is_none() {
                        first_frame_at = Some(Instant::now());
                    }
                }
                Err(e) => v.say(format!("error: {e}")),
            }
        }
        if !event::poll(Duration::from_millis(30))? {
            continue;
        }
        // Take everything already queued, so a fast scroll becomes one wheel event.
        loop {
            let res = match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => v.on_key(k),
                Event::Paste(text) => v.on_paste(&text),
                Event::Mouse(m) => v.on_mouse(m),
                Event::Resize(..) => {
                    // relayout_if_resized repaints; the tick restarts the stream at the new size.
                    last_tick = Instant::now() - Duration::from_secs(10);
                    Ok(true)
                }
                _ => Ok(true),
            };
            match res {
                Ok(false) => return Ok(()),
                Ok(true) => {}
                Err(e) => {
                    v.say(format!("error: {e}"));
                    v.reset();
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
        if let Some((x, y, d)) = v.wheel.take()
            && let Err(e) = v.mouse("mouseWheel", x, y, 0, 0, d)
        {
            v.say(format!("error: {e}"));
        }
        v.draw_chrome()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_replaces_the_selected_url() {
        let mut e = UrlEdit::new("https://example.com/");
        e.key(k(KeyCode::Char('x')));
        assert_eq!(e.key(k(KeyCode::Enter)), Edit::Go("x".into()));
    }

    #[test]
    fn pasted_address_filters_controls_and_waits_for_enter() {
        let mut e = UrlEdit::new("https://old.example/");
        e.paste("https://example.com/a%20b?q=x%26y#part\r\n\x1b[31m\u{7}");
        assert_eq!(e.text(), "https://example.com/a%20b?q=x%26y#part[31m");
        assert_eq!(e.key(k(KeyCode::Enter)), Edit::Go("https://example.com/a%20b?q=x%26y#part[31m".into()));
    }

    #[test]
    fn arrows_drop_the_selection_and_edit_in_place() {
        let mut e = UrlEdit::new("https://a.com/");
        e.key(k(KeyCode::Left));
        e.key(k(KeyCode::Backspace));
        e.key(k(KeyCode::Char('x')));
        assert_eq!(e.text(), "https://a.cox/");
    }

    #[test]
    fn double_click_selects_the_word_under_it() {
        let mut e = UrlEdit::new("https://example.com/path");
        e.click(10, 80, 2); // inside "example"
        e.key(k(KeyCode::Char('x')));
        assert_eq!(e.text(), "https://x.com/path");
    }

    #[test]
    fn triple_click_selects_everything() {
        let mut e = UrlEdit::new("https://a.com/");
        e.click(3, 80, 1);
        e.click(3, 80, 3);
        e.key(k(KeyCode::Char('x')));
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn drag_and_shift_arrows_extend_the_selection() {
        let mut e = UrlEdit::new("https://a.com/");
        e.click(8, 80, 1);
        e.drag(10, 80); // "a."
        e.key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT)); // "a.c"
        e.key(k(KeyCode::Backspace));
        assert_eq!(e.text(), "https://om/");
    }

    #[test]
    fn ctrl_w_deletes_one_url_segment() {
        let mut e = UrlEdit::new("https://a.com/path");
        e.key(k(KeyCode::End));
        e.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(e.text(), "https://a.com/");
    }

    #[test]
    fn long_urls_scroll_to_keep_the_cursor_visible() {
        let mut e = UrlEdit::new("abcdefghij");
        e.key(k(KeyCode::End));
        assert_eq!(e.scroll(4), 7); // shows "hij" plus the cursor cell
        e.click(0, 4, 1);
        assert_eq!(e.cur, 7);
    }

    #[test]
    fn buttons_record_their_click_spans() {
        let mut l = Line::new(0, None, Palette::DEFAULT, true);
        l.gap();
        l.button(Icon::Back, Act::Back);
        l.button(Icon::Camera, Act::Screenshot);
        let (_, hits, slots) = l.finish();
        assert_eq!((hits[0].x0, hits[0].x1, hits[0].act), (1, 4, Act::Back));
        assert_eq!((hits[1].x0, hits[1].x1), (5, 8));
        assert_eq!(slots.iter().map(|s| (s.0, s.3)).collect::<Vec<_>>(), [(1, 3), (5, 3)]);
    }

    #[test]
    fn text_controls_keep_spans_when_clipped() {
        let mut l = Line::new(1, None, Palette::DEFAULT, false);
        l.gap();
        l.control(20, " Find ", Act::Find, false);
        l.control(20, " Next ", Act::FindNext(1), true);
        l.control(20, " Done ", Act::Find, false);
        let (_, hits, _) = l.finish();
        assert_eq!(hits.iter().map(|h| (h.x0, h.x1)).collect::<Vec<_>>(), [(1, 9), (10, 18)]);
    }

    #[test]
    fn kitty_text_control_has_border_slots_without_changing_click_span() {
        let mut l = Line::new(2, None, Palette::DEFAULT, true);
        l.gap();
        l.control(20, " Find ", Act::Find, false);
        let (_, hits, slots) = l.finish();
        assert_eq!((hits[0].row, hits[0].x0, hits[0].x1), (2, 1, 9));
        assert_eq!(slots.len(), 8);
        assert_eq!(slots[0], (1, 2, control_edge(false, false, 0), 1));
        assert_eq!(slots[1], (2, 2, control_edge(false, false, 1), 1));
        assert_eq!(slots[7], (8, 2, control_edge(false, false, 2), 1));
    }

    #[test]
    fn kitty_icon_hover_keeps_the_same_hit_span() {
        let mut l = Line::new(0, Some((2, 0)), Palette::DEFAULT, true);
        l.gap();
        l.button(Icon::Back, Act::Back);
        let (_, hits, slots) = l.finish();
        assert_eq!((hits[0].x0, hits[0].x1), (1, 4));
        assert_eq!(slots[0], (1, 0, icon_id(Icon::Back, true), 3));
    }

    #[test]
    fn translucent_theme_colors_blend_over_their_base() {
        assert_eq!(parse_hex("#ffffff00", (10, 20, 30)), Some((10, 20, 30)));
        assert_eq!(parse_hex("#ff0000", (0, 0, 0)), Some((255, 0, 0)));
        assert_eq!(parse_hex("#ffffff80", (0, 0, 0)), Some((128, 128, 128)));
    }

    #[test]
    fn clip_marks_truncation() {
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
    }
}
