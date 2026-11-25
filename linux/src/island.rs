//! The island: the pill that grows out of the top edge of the screen during a transfer.
//!
//! It follows the Mac app's design system (`docs/DESIGN.md`): pure night black, bone text, no accent
//! hue, the ring as the leading visual, and the notch's springs. It is a Wayland layer surface, so
//! the compositor places it exactly under the camera bar, and a tiny Unix socket carries what to show.

use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SURFACE_WIDTH: i32 = 720;
const SURFACE_HEIGHT: i32 = 130;
/// Where the island starts: the notch's own size, flat against the screen edge.
const NOTCH_WIDTH: f64 = 190.0;
const NOTCH_HEIGHT: f64 = 32.0;
const PADDING: f64 = 18.0;
const RING: f64 = 26.0;
/// A preview deserves a little more room than the progress ring.
const THUMB: f64 = 34.0;
const GAP: f64 = 10.0;

fn leading_size(kind: Kind) -> f64 {
    if kind == Kind::Done {
        THUMB
    } else {
        RING
    }
}
const HIDE: f64 = 0.30;

// ---- Design tokens, from docs/DESIGN.md -------------------------------------------------------

/// The dark background of the island, opaque: it must read as hardware, not as a card.
const NIGHT: (f64, f64, f64) = (0.031, 0.031, 0.039);
/// Text in dark mode.
const BONE: (f64, f64, f64) = (0.957, 0.953, 0.937);
/// Secondary text.
const SECONDARY: (f64, f64, f64) = (0.635, 0.635, 0.659);
/// The glow: the cool white of a screen's light. Halos only, never a fill.
const LIGHT: (f64, f64, f64) = (0.839, 0.871, 1.0);
const WARNING: (f64, f64, f64) = (0.941, 0.706, 0.353);
const DANGER: (f64, f64, f64) = (1.0, 0.478, 0.447);

const TEXT_FONT: &str = "Geist";
const MONO_FONT: &str = "Geist Mono";

const GEIST: &[u8] = include_bytes!("../fonts/Geist[wght].ttf");
const GEIST_MONO: &[u8] = include_bytes!("../fonts/GeistMono[wght].ttf");

#[link(name = "fontconfig")]
extern "C" {
    fn FcConfigGetCurrent() -> *mut c_void;
    fn FcConfigAppFontAddFile(config: *mut c_void, file: *const c_char) -> i32;
}

/// Registers the two fonts with the process, without touching the person's font directory.
fn load_fonts() {
    let dir = crate::store::runtime_dir().join("fonts");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (name, bytes) in [("geist.ttf", GEIST), ("geist-mono.ttf", GEIST_MONO)] {
        let path = dir.join(name);
        if !path.exists() && std::fs::write(&path, bytes).is_err() {
            return;
        }
        if let Ok(cpath) = CString::new(path.to_string_lossy().as_bytes()) {
            unsafe {
                FcConfigAppFontAddFile(FcConfigGetCurrent(), cpath.as_ptr());
            }
        }
    }
}

/// A small preview of the file that just landed, decoded off the main thread.
struct Thumb {
    width: i32,
    height: i32,
    /// B, G, R, 255 per pixel: what Cairo's RGB24 wants on a little-endian machine.
    data: Vec<u8>,
}

fn load_thumb(path: &std::path::Path) -> Option<Thumb> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > 48 * 1024 * 1024 {
        return None;
    }
    let image = image::open(path).ok()?.thumbnail(96, 96).to_rgba8();
    let (width, height) = image.dimensions();
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for pixel in image.pixels() {
        data.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    Some(Thumb { width: width as i32, height: height as i32, data })
}

/// What a click on one of the island's buttons does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Open,
    Folder,
}

/// The buttons drawn in the last frame, with their hit boxes.
type Buttons = Rc<RefCell<Vec<((f64, f64, f64, f64), Act)>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Something landed on the camera bump: the notch breathes before any transfer starts.
    Detect,
    Receive,
    Send,
    Done,
    Code,
    /// A gentle instruction about how to tap.
    Hint,
    Error,
}

/// One spring per property, integrated frame by frame: a state change retargets it, so the motion
/// keeps its velocity instead of restarting from zero.
struct Spring {
    value: f64,
    velocity: f64,
    target: f64,
}

impl Spring {
    fn new(value: f64) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
        }
    }

    fn to(&mut self, target: f64) {
        self.target = target;
    }

    fn kick(&mut self, velocity: f64) {
        self.velocity += velocity;
    }

    /// Apple's springs are given as a response time and a damping ratio; these are the equivalents.
    fn step(&mut self, dt: f64, response: f64, ratio: f64) {
        let dt = dt.clamp(0.0, 1.0 / 30.0);
        let omega = std::f64::consts::TAU / response;
        let stiffness = omega * omega;
        let damping = 2.0 * ratio * omega;
        let accel = (self.target - self.value) * stiffness - self.velocity * damping;
        self.velocity += accel * dt;
        self.value += self.velocity * dt;
    }
}

struct Motion {
    open: Spring,
    width: Spring,
    height: Spring,
    hover: Spring,
    shake: Spring,
    check: Spring,
    /// Ring sweep used when the total size is unknown.
    spin: f64,
    last_frame: f64,
    reduce: bool,
}

impl Motion {
    fn new() -> Self {
        Self {
            open: Spring::new(0.0),
            width: Spring::new(NOTCH_WIDTH),
            height: Spring::new(NOTCH_HEIGHT),
            hover: Spring::new(0.0),
            shake: Spring::new(0.0),
            check: Spring::new(0.0),
            spin: 0.0,
            last_frame: 0.0,
            reduce: false,
        }
    }

    fn step(&mut self, now: f64, view: &View, hovered: bool) {
        let dt = (now - self.last_frame).clamp(0.0, 0.05);
        self.last_frame = now;
        self.spin += dt * 0.9;
        let leaving = view.hide_at.map(|at| Instant::now() > at).unwrap_or(false);
        self.open
            .to(if view.visible && !leaving { 1.0 } else { 0.0 });
        self.width.to(view.target_width);
        self.height.to(if view.kind == Kind::Detect {
            NOTCH_HEIGHT
        } else {
            view.target_height
        });
        self.hover.to(if hovered { 1.0 } else { 0.0 });
        self.shake.to(0.0);
        self.check.to(if view.kind == Kind::Done && view.visible {
            1.0
        } else {
            0.0
        });

        // Island spring for the shape, standard for content, a quick swell for the check.
        self.open.step(dt, 0.42, 0.78);
        self.width.step(dt, 0.42, 0.78);
        self.height.step(dt, 0.42, 0.78);
        self.hover.step(dt, 0.38, 0.86);
        self.shake.step(dt, 0.30, 0.40);
        self.check.step(dt, 0.38, 0.86);
    }
}

#[derive(Default)]
struct Layouts {
    title: Option<pango::Layout>,
    detail: Option<pango::Layout>,
    trailing: Option<pango::Layout>,
    prev_title: Option<pango::Layout>,
    prev_detail: Option<pango::Layout>,
}

struct View {
    kind: Kind,
    prev_kind: Kind,
    kind_since: Instant,
    title: String,
    detail: String,
    prev_title: String,
    prev_detail: String,
    progress: Option<f32>,
    last_progress: f32,
    target_width: f64,
    target_height: f64,
    layouts: Layouts,
    shown_at: Instant,
    updated_at: Instant,
    hide_at: Option<Instant>,
    visible: bool,
}

impl View {
    fn new() -> Self {
        Self {
            kind: Kind::Receive,
            prev_kind: Kind::Receive,
            kind_since: Instant::now(),
            title: String::new(),
            detail: String::new(),
            prev_title: String::new(),
            prev_detail: String::new(),
            progress: None,
            last_progress: 0.0,
            target_width: NOTCH_WIDTH,
            target_height: NOTCH_HEIGHT,
            layouts: Layouts::default(),
            shown_at: Instant::now(),
            updated_at: Instant::now(),
            hide_at: None,
            visible: false,
        }
    }

    fn show(
        &mut self,
        kind: Kind,
        title: &str,
        detail: &str,
        progress: Option<f32>,
        hide: Option<Duration>,
    ) {
        if !self.visible {
            self.shown_at = Instant::now();
        }
        if kind != self.kind {
            self.prev_kind = self.kind;
            self.prev_title = self.title.clone();
            self.prev_detail = self.detail.clone();
            self.kind_since = Instant::now();
        }
        if let Some(value) = progress {
            self.last_progress = value;
        }
        self.updated_at = Instant::now();
        self.visible = true;
        self.kind = kind;
        self.title = title.to_string();
        self.detail = detail.to_string();
        self.progress = progress;
        self.hide_at = hide.map(|d| Instant::now() + d);
    }

    /// The island is as wide as its text needs, and no wider.
    fn measure(&mut self, area: &gtk::DrawingArea) {
        if self.kind == Kind::Detect {
            self.target_width = NOTCH_WIDTH;
            self.target_height = NOTCH_HEIGHT;
            return;
        }
        let (title_size, detail_size) = sizes_for(self.kind);
        let title = measure_line(area, &self.title, TEXT_FONT, title_size, true, -0.15);
        let detail = measure_line(area, &self.detail, TEXT_FONT, detail_size, false, 0.0);
        let trailing = match self.kind {
            Kind::Code => 0.0,
            Kind::Done => 66.0,
            _ => 46.0,
        };
        let lead = leading_size(self.kind);
        let width = title.max(detail) + PADDING * 2.0 + lead + GAP + trailing;
        self.target_width = width.clamp(NOTCH_WIDTH + 60.0, 560.0);
        self.target_height = if self.kind == Kind::Code { PADDING * 2.0 + 40.0 } else { PADDING * 2.0 + 26.0 };

        let inner = (self.target_width - PADDING * 2.0 - lead - GAP - trailing).max(24.0);
        self.layouts.title = Some(make_layout(
            area,
            TEXT_FONT,
            &self.title,
            title_size,
            true,
            -0.15,
            inner,
        ));
        self.layouts.detail = Some(make_layout(
            area,
            TEXT_FONT,
            &self.detail,
            detail_size,
            false,
            0.0,
            inner,
        ));
        if self.kind != Kind::Code {
            let percent = self
                .progress
                .or(Some(self.last_progress))
                .filter(|_| self.progress.is_some());
            if let Some(value) = percent {
                let text = format!("{:.0} %", (value * 100.0).round());
                self.layouts.trailing =
                    Some(make_layout(area, MONO_FONT, &text, 10, false, 1.2, 46.0));
            } else {
                self.layouts.trailing = None;
            }
        } else {
            self.layouts.trailing = None;
        }
        if !self.prev_title.is_empty() {
            let (pt, pd) = sizes_for(self.prev_kind);
            self.layouts.prev_title = Some(make_layout(
                area,
                TEXT_FONT,
                &self.prev_title,
                pt,
                true,
                -0.15,
                inner,
            ));
            self.layouts.prev_detail = Some(make_layout(
                area,
                TEXT_FONT,
                &self.prev_detail,
                pd,
                false,
                0.0,
                inner,
            ));
        }
    }
}

/// Where the island listens. The daemon sends one JSON object per datagram here.
pub fn socket_path() -> PathBuf {
    crate::store::runtime_dir().join("island.sock")
}

/// The daemon's end: fire and forget, silence when no island is running.
#[derive(Clone)]
pub struct Push {
    path: PathBuf,
    /// One socket for every event, so the island sees them in the order they happened.
    socket: Arc<std::os::unix::net::UnixDatagram>,
}

impl Push {
    pub fn new() -> Self {
        use std::os::unix::net::UnixDatagram;
        let socket = UnixDatagram::unbound().unwrap_or_else(|_| {
            UnixDatagram::unbound().expect("a Unix socket is always available")
        });
        Self {
            path: socket_path(),
            socket: Arc::new(socket),
        }
    }

    pub fn send(&self, value: Value) {
        let _ = self
            .socket
            .send_to(value.to_string().as_bytes(), &self.path);
    }

    pub fn detected(&self) {
        self.send(json!({ "t": "detect", "title": "", "detail": "" }));
    }

    pub fn receiving(&self, from: &str, what: &str) {
        self.send(json!({ "t": "recv", "title": format!("Reçu de {from}"), "detail": what }));
    }

    pub fn progress(&self, done: u64, total: u64, what: &str) {
        self.send(json!({ "t": "progress", "done": done, "total": total, "detail": what }));
    }

    pub fn sending(&self, to: &str, what: &str) {
        self.send(json!({ "t": "send", "title": format!("Envoi vers {to}"), "detail": what }));
    }

    pub fn done(&self, title: &str, detail: &str) {
        self.send(json!({ "t": "done", "title": title, "detail": detail }));
    }

    /// The same, with the file a click on the island should open.
    pub fn saved(&self, title: &str, detail: &str, target: Option<&std::path::Path>) {
        crate::trace_debug!("tapdrop: îlot -> {title} ({detail})");
        self.send(json!({
            "t": "done", "title": title, "detail": detail,
            "target": target.map(|p| p.display().to_string()).unwrap_or_default(),
        }));
    }

    pub fn code(&self, source: &str, code: &str) {
        self.send(json!({ "t": "code", "title": source, "detail": code }));
    }

    pub fn failed(&self, what: &str) {
        self.send(json!({ "t": "error", "title": "Échec", "detail": what }));
    }

    pub fn hint(&self, text: &str) {
        self.send(json!({ "t": "hint", "title": text, "detail": "" }));
    }

    pub fn hide(&self) {
        self.send(json!({ "t": "hide" }));
    }

    pub fn quit(&self) {
        self.send(json!({ "t": "quit" }));
    }
}

pub fn run() -> anyhow::Result<()> {
    load_fonts();
    let app = gtk::Application::builder()
        .application_id("app.tapdrop.linux.island")
        .build();
    app.connect_activate(build);
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build(app: &gtk::Application) {
    let window = gtk::Window::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .default_width(SURFACE_WIDTH)
        .default_height(SURFACE_HEIGHT)
        .build();
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Top, true);
    window.set_margin(Edge::Top, 0);
    window.set_keyboard_mode(KeyboardMode::None);
    // -1 ignores the panel's exclusive zone: the island sits at the physical top edge, like a notch,
    // instead of being pushed below the top bar.
    window.set_exclusive_zone(-1);
    window.set_namespace(Some("tapdrop-island"));

    let css = gtk::CssProvider::new();
    css.load_from_data("window { background-color: transparent; }");
    gtk::style_context_add_provider_for_display(
        &gdk_display(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let area = gtk::DrawingArea::new();
    area.set_content_width(SURFACE_WIDTH);
    area.set_content_height(SURFACE_HEIGHT);
    area.set_hexpand(true);
    area.set_vexpand(true);
    window.set_child(Some(&area));

    let view = Rc::new(RefCell::new(View::new()));
    let motion = Rc::new(RefCell::new(Motion::new()));
    {
        let mut state = motion.borrow_mut();
        state.reduce = !gtk::Settings::default().map(|s| s.is_gtk_enable_animations()).unwrap_or(true);
    }
    let hovered = Rc::new(Cell::new(false));
    let pointer = Rc::new(Cell::new((0.0f64, 0.0f64)));
    let pill = Rc::new(Cell::new(None::<(i32, i32, i32, i32)>));
    let target = Rc::new(RefCell::new(None::<PathBuf>));
    let thumb = Rc::new(RefCell::new(None::<Thumb>));
    let buttons: Buttons = Rc::new(RefCell::new(Vec::new()));
    let thumb_gen = Rc::new(Cell::new(0u64));
    let (thumb_tx, thumb_rx) = std::sync::mpsc::channel::<(u64, Option<Thumb>)>();

    area.add_tick_callback({
        let view = view.clone();
        let motion = motion.clone();
        let hovered = hovered.clone();
        let area_for_tick = area.clone();
        move |_area, clock| {
            let now = clock.frame_time() as f64 / 1_000_000.0;
            motion.borrow_mut().step(now, &view.borrow(), hovered.get());
            if view.borrow().visible {
                area_for_tick.queue_draw();
            }
            glib::ControlFlow::Continue
        }
    });

    area.set_draw_func({
        let view = view.clone();
        let motion = motion.clone();
        let pill = pill.clone();
        let pointer = pointer.clone();
        let thumb = thumb.clone();
        let buttons = buttons.clone();
        move |area, cr, _w, _h| {
            let view = view.borrow();
            let motion = motion.borrow();
            let thumb = thumb.borrow();
            let scene = Scene {
                view: &view,
                motion: &motion,
                pill: &pill,
                pointer: pointer.get(),
                thumb: &thumb,
                buttons: &buttons,
            };
            draw(area, cr, &scene);
        }
    });

    let pointer_controller = gtk::EventControllerMotion::new();
    pointer_controller.connect_motion({
        let pointer = pointer.clone();
        move |_, x, y| pointer.set((x, y))
    });
    pointer_controller.connect_enter({
        let (hovered, area) = (hovered.clone(), area.clone());
        move |_, _, _| {
            hovered.set(true);
            area.set_cursor_from_name(Some("pointer"));
        }
    });
    pointer_controller.connect_leave({
        let (hovered, area) = (hovered.clone(), area.clone());
        move |_| {
            hovered.set(false);
            area.set_cursor_from_name(None);
        }
    });
    area.add_controller(pointer_controller);

    let click = gtk::GestureClick::new();
    click.connect_released({
        let (pill, target, buttons) = (pill.clone(), target.clone(), buttons.clone());
        move |_, _, x, y| {
            for (rect, act) in buttons.borrow().iter() {
                let hit = x >= rect.0 && x <= rect.0 + rect.2 && y >= rect.1 && y <= rect.1 + rect.3;
                if hit {
                    act_on(*act, target.borrow().as_deref());
                    return;
                }
            }
            let inside = pill
                .get()
                .map(|(px, py, pw, ph)| {
                    x >= px as f64
                        && x <= (px + pw) as f64
                        && y >= py as f64
                        && y <= (py + ph) as f64
                })
                .unwrap_or(false);
            if inside {
                open_target(target.borrow().as_deref());
            }
        }
    });
    area.add_controller(click);

    let (sender, receiver) = std::sync::mpsc::channel();
    listen(sender);
    glib::timeout_add_local(Duration::from_millis(8), {
        let view = view.clone();
        let area = area.clone();
        let target = target.clone();
        let motion = motion.clone();
        let thumb = thumb.clone();
        let thumb_tx = thumb_tx.clone();
        let thumb_gen = thumb_gen.clone();
        move || {
            while let Ok(value) = receiver.try_recv() {
                apply(&view, &area, &target, &motion, &thumb, &thumb_tx, &thumb_gen, &value);
            }
            while let Ok((generation, decoded)) = thumb_rx.try_recv() {
                if generation == thumb_gen.get() {
                    *thumb.borrow_mut() = decoded;
                    area.queue_draw();
                }
            }
            glib::ControlFlow::Continue
        }
    });

    // State and surface lifecycle. While idle the window is unmapped entirely: no surface, so no
    // stale frame can ever stay on screen.
    let last_region = Rc::new(Cell::new((0, 0, 0, 0)));
    let window_for_region = window.clone();
    let mapped = Rc::new(Cell::new(true));
    glib::timeout_add_local(Duration::from_millis(20), {
        let view = view.clone();
        let area = area.clone();
        let pill = pill.clone();
        let motion = motion.clone();
        let hovered_for_timer = hovered.clone();
        move || {
            let now = Instant::now();
            {
                let mut state = view.borrow_mut();
                // A result waits while the person is aiming at its buttons.
                if state.hide_at.is_some() && hovered_for_timer.get() {
                    state.hide_at = Some(now + Duration::from_secs(2));
                }
                let mut visible = state.visible;
                if visible
                    && state.hide_at.is_none()
                    && now.duration_since(state.updated_at).as_secs() >= 8
                {
                    // A transfer that stopped talking must not leave the card on screen for ever.
                    state.hide_at = Some(now);
                }
                if visible {
                    if let Some(at) = state.hide_at {
                        let settled = motion.borrow().open.value < 0.02;
                        if now >= at + Duration::from_secs_f64(HIDE) && settled {
                            state.visible = false;
                            state.hide_at = None;
                            visible = false;
                        }
                    }
                }
                drop(state);
                if visible {
                    if !mapped.get() {
                        window_for_region.set_visible(true);
                        mapped.set(true);
                        area.queue_draw();
                    }
                } else if mapped.get() {
                    window_for_region.set_visible(false);
                    mapped.set(false);
                }
            }
            if let Some(surface) = window_for_region.surface() {
                let scale = area.scale_factor().max(1);
                let rect = if view.borrow().visible {
                    pill.get()
                } else {
                    None
                };
                let key = rect.unwrap_or((0, 0, 0, 0));
                if key != last_region.get() {
                    let region = match rect {
                        Some((x, y, w, h)) => cairo::Region::create_rectangle(
                            &cairo::RectangleInt::new(x * scale, y * scale, w * scale, h * scale),
                        ),
                        None => {
                            cairo::Region::create_rectangle(&cairo::RectangleInt::new(0, 0, 0, 0))
                        }
                    };
                    surface.set_input_region(Some(&region));
                    last_region.set(key);
                }
            }
            glib::ControlFlow::Continue
        }
    });

    window.present();
}

fn gdk_display() -> gdk::Display {
    gdk::Display::default().expect("an island needs a display")
}

fn listen(sender: std::sync::mpsc::Sender<Value>) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(socket) = std::os::unix::net::UnixDatagram::bind(&path) else {
        eprintln!("tapdrop: l'îlot ne peut pas écouter sur {}", path.display());
        return;
    };
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            let Ok(size) = socket.recv(&mut buffer) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&buffer[..size]) else {
                continue;
            };
            if sender.send(value).is_err() {
                return;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn apply(
    view: &Rc<RefCell<View>>,
    area: &gtk::DrawingArea,
    target: &Rc<RefCell<Option<PathBuf>>>,
    motion: &Rc<RefCell<Motion>>,
    thumb: &Rc<RefCell<Option<Thumb>>>,
    thumb_tx: &std::sync::mpsc::Sender<(u64, Option<Thumb>)>,
    thumb_gen: &Rc<Cell<u64>>,
    value: &Value,
) {
    let title = value["title"].as_str().unwrap_or_default();
    let detail = value["detail"].as_str().unwrap_or_default();
    // A new transfer forgets the file of the previous one: a click must never open a stale photo.
    if matches!(value["t"].as_str(), Some("recv" | "send" | "detect")) {
        *target.borrow_mut() = None;
        *thumb.borrow_mut() = None;
        thumb_gen.set(thumb_gen.get().wrapping_add(1));
    }
    if value["t"] == "done" {
        let path = value["target"].as_str().unwrap_or_default();
        *target.borrow_mut() = if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        };
        // The preview is decoded off the main thread: a phone photo would hitch the animation.
        if !path.is_empty() {
            let generation = thumb_gen.get().wrapping_add(1);
            thumb_gen.set(generation);
            let sender = thumb_tx.clone();
            let file = PathBuf::from(path);
            std::thread::spawn(move || {
                let decoded = load_thumb(&file);
                let _ = sender.send((generation, decoded));
            });
        } else {
            *thumb.borrow_mut() = None;
        }
    }

    let mut state = view.borrow_mut();
    match value["t"].as_str().unwrap_or("") {
        "detect" => state.show(Kind::Detect, "", "", None, Some(Duration::from_secs(6))),
        "recv" => state.show(Kind::Receive, title, detail, Some(0.0), None),
        "send" => state.show(Kind::Send, title, detail, Some(0.0), None),
        "progress" if state.kind == Kind::Done => return,
        "progress" => {
            let (done, total) = (
                value["done"].as_u64().unwrap_or(0),
                value["total"].as_u64().unwrap_or(0),
            );
            let progress = if total > 0 {
                Some((done as f32 / total as f32).clamp(0.0, 1.0))
            } else {
                None
            };
            let (kind, title) = (state.kind, state.title.clone());
            state.show(kind, &title, detail, progress, None);
        }
        "done" => state.show(
            Kind::Done,
            title,
            detail,
            None,
            Some(Duration::from_secs(3)),
        ),
        "error" => {
            state.show(
                Kind::Error,
                title,
                detail,
                None,
                Some(Duration::from_secs(4)),
            );
            motion.borrow_mut().shake.kick(-9.0);
        }
        "hint" => state.show(
            Kind::Hint,
            title,
            detail,
            None,
            Some(Duration::from_secs(3)),
        ),
        "code" => state.show(
            Kind::Code,
            title,
            detail,
            None,
            Some(Duration::from_secs(10)),
        ),
        "hide" => state.hide_at = Some(Instant::now()),
        "quit" => std::process::exit(0),
        _ => return,
    }
    state.measure(area);
    drop(state);
    area.queue_draw();
}

/// Everything one frame needs, so the drawing function keeps a readable signature.
struct Scene<'a> {
    view: &'a View,
    motion: &'a Motion,
    pill: &'a Cell<Option<(i32, i32, i32, i32)>>,
    pointer: (f64, f64),
    thumb: &'a Option<Thumb>,
    buttons: &'a Buttons,
}

fn draw(area: &gtk::DrawingArea, cr: &cairo::Context, scene: &Scene) {
    let Scene { view, motion, pill, pointer, thumb, buttons } = scene;
    let pointer = *pointer;
    let _ = area;
    let open = motion.open.value;
    let alpha = open.clamp(0.0, 1.0);
    if !view.visible || (view.title.is_empty() && view.kind != Kind::Detect) || alpha <= 0.01 {
        cr.set_operator(cairo::Operator::Source);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        let _ = cr.paint();
        pill.set(None);
        return;
    }

    buttons.borrow_mut().clear();
    let now = Instant::now();
    let morph = if view.kind == view.prev_kind {
        1.0
    } else {
        spring(now.duration_since(view.kind_since).as_secs_f64()).clamp(0.0, 1.0)
    };
    let hover = motion.hover.value.clamp(0.0, 1.0);
    let width = motion.width.value.max(NOTCH_WIDTH) * (1.0 + 0.010 * hover);
    let height = (motion.height.value * open.clamp(0.0, 1.2)).max(2.0);
    let x = (SURFACE_WIDTH as f64 - width) / 2.0 + motion.shake.value;
    let y = 0.0;
    let radius = height / 2.0;
    pill.set(Some((x as i32, y as i32, width as i32, height as i32)));

    // The glow of a screen's light, heavily blurred behind the island. Halos only.
    if open > 0.4 {
        for (spread, strength) in [(16.0, 0.05), (10.0, 0.07), (5.0, 0.09)] {
            rounded_bottom(
                cr,
                x - spread,
                y,
                width + 2.0 * spread,
                height + 2.0 * spread,
                radius + spread,
            );
            cr.set_source_rgba(LIGHT.0, LIGHT.1, LIGHT.2, strength * alpha);
            let _ = cr.fill();
        }
    }

    // Night black, opaque: the island must look like hardware, not like a card over the wallpaper.
    rounded_bottom(cr, x, y, width, height, radius);
    cr.set_source_rgba(NIGHT.0, NIGHT.1, NIGHT.2, alpha);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.08 * alpha);
    cr.set_line_width(0.7);
    let _ = cr.stroke();

    if height < 24.0 {
        return;
    }
    // Content arrives a beat after the shape, with the standard spring.
    let content = ((open - 0.55) / 0.45).clamp(0.0, 1.0);
    if content <= 0.01 {
        return;
    }
    let elapsed = now.duration_since(view.shown_at).as_secs_f64();

    let center_y = height / 2.0;
    let lead = leading_size(view.kind);
    let ring_x = x + PADDING + lead / 2.0;
    if morph < 0.999 {
        leading(
            cr,
            ring_x,
            center_y,
            view.prev_kind,
            alpha * content * (1.0 - morph),
            elapsed,
            0.0,
            1.0,
            motion,
            -1.0,
            None,
        );
    }
    let preview = if view.kind == Kind::Done { thumb.as_ref() } else { None };
    leading(
        cr,
        ring_x,
        center_y,
        view.kind,
        alpha * content * morph,
        elapsed,
        hover,
        morph,
        motion,
        view.progress.map(f64::from).unwrap_or(-1.0),
        preview,
    );

    let text_x = x + PADDING + lead + GAP;
    let text_alpha = alpha * content;
    let detail_color = if view.kind == Kind::Code { BONE } else { SECONDARY };

    // The two lines are measured and centred as a block, on the ring's own axis, so nothing sits
    // high or low depending on the font's ascent.
    let (title_height, detail_height) = block_heights(&view.layouts);
    let gap = if detail_height > 0.0 { 3.0 } else { 0.0 };
    let block = title_height + gap + detail_height;
    let top = ((height - block) / 2.0).max(6.0);
    let title_y = top;
    let detail_y = top + title_height + gap;

    if morph < 0.999 {
        let prev_title_height = layout_height(&view.layouts.prev_title);
        let prev_detail_height = layout_height(&view.layouts.prev_detail);
        let prev_gap = if prev_detail_height > 0.0 { 3.0 } else { 0.0 };
        let prev_block = prev_title_height + prev_gap + prev_detail_height;
        let prev_top = ((height - prev_block) / 2.0).max(6.0);
        if let (Some(title), Some(detail)) = (&view.layouts.prev_title, &view.layouts.prev_detail) {
            draw_layout(cr, title, text_x, prev_top, BONE, text_alpha * (1.0 - morph));
            draw_layout(
                cr,
                detail,
                text_x,
                prev_top + prev_title_height + prev_gap,
                detail_color,
                text_alpha * (1.0 - morph),
            );
        }
    }
    if let (Some(title), Some(detail)) = (&view.layouts.title, &view.layouts.detail) {
        draw_layout(cr, title, text_x, title_y, BONE, text_alpha * morph);
        draw_layout(cr, detail, text_x, detail_y, detail_color, text_alpha * morph);
    }

    // The trailing number sits on the title's baseline, not floating in a corner.
    if let Some(trailing) = &view.layouts.trailing {
        let (trailing_width, trailing_height) = trailing.pixel_size();
        let y = title_y + (title_height - f64::from(trailing_height)) / 2.0;
        draw_layout(
            cr,
            trailing,
            x + width - PADDING - f64::from(trailing_width),
            y,
            SECONDARY,
            text_alpha * morph,
        );
    }

    // Two quiet buttons on the right: open the file, or its folder.
    if view.kind == Kind::Done {
        let folder = (x + width - PADDING - 13.0, center_y);
        let open = (folder.0 - 32.0, center_y);
        for (center, act, glyph) in [(open, Act::Open, 0), (folder, Act::Folder, 1)] {
            let hovered = (pointer.0 - center.0).powi(2) + (pointer.1 - center.1).powi(2) < 13.0 * 13.0;
            cr.set_source_rgba(1.0, 1.0, 1.0, (if hovered { 0.14 } else { 0.06 }) * alpha * content);
            cr.arc(center.0, center.1, 13.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            glyph_action(cr, center.0, center.1, glyph, BONE, alpha * content * 0.9);
            buttons.borrow_mut().push(((center.0 - 13.0, center.1 - 13.0, 26.0, 26.0), act));
        }
    }

    // The pointer light: a soft radial highlight under the cursor while hovering.
    if hover > 0.02 && view.keep_pointer_light() {
        let light =
            cairo::RadialGradient::new(pointer.0, pointer.1, 0.0, pointer.0, pointer.1, 150.0);
        light.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.05 * hover * alpha);
        light.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
        let _ = cr.save();
        rounded_bottom(cr, x, y, width, height, radius);
        cr.clip();
        cr.arc(pointer.0, pointer.1, 150.0, 0.0, std::f64::consts::TAU);
        let _ = cr.set_source(&light);
        let _ = cr.fill();
        let _ = cr.restore();
    }
}

impl View {
    /// The hover light is only for text states, where the island is a real object to aim at.
    fn keep_pointer_light(&self) -> bool {
        self.kind != Kind::Detect
    }
}

/// The leading visual: a breathing dot while something lands, the progress ring during a transfer,
/// a bone disc with an ink check when it lands, and a glyph for codes, hints and errors.
#[allow(clippy::too_many_arguments)]
fn leading(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    kind: Kind,
    alpha: f64,
    elapsed: f64,
    hover: f64,
    sweep: f64,
    motion: &Motion,
    progress: f64,
    thumb: Option<&Thumb>,
) {
    if alpha <= 0.01 {
        return;
    }
    let diameter = if kind == Kind::Done { THUMB } else { RING };
    let radius = diameter / 2.0;

    match kind {
        Kind::Detect => {
            // A live dot with the light's halo, breathing. Never blinks.
            let breath = 0.5 + 0.5 * (elapsed * 2.2).sin();
            cr.set_source_rgba(LIGHT.0, LIGHT.1, LIGHT.2, (0.18 + 0.12 * breath) * alpha);
            cr.arc(cx, cy, 9.0 + 2.0 * breath, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgba(BONE.0, BONE.1, BONE.2, alpha);
            cr.arc(cx, cy, 4.5, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        Kind::Receive | Kind::Send => {
            ring(
                cr,
                cx,
                cy,
                radius,
                3.5,
                alpha,
                progress,
                motion,
            );
        }
        Kind::Done => {
            // A photo shows itself; anything else gets a bone disc and an ink check.
            if let Some(thumb) = thumb {
                draw_thumb(cr, cx - radius, cy - radius, diameter, thumb, alpha);
            } else {
                let r = radius * (1.0 + 0.02 * hover);
                cr.set_source_rgba(BONE.0, BONE.1, BONE.2, alpha);
                cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
                check_scaled(cr, cx, cy, NIGHT, alpha, motion.check.value, diameter / 26.0);
            }
        }
        Kind::Code | Kind::Hint | Kind::Error => {
            let color = match kind {
                Kind::Hint => WARNING,
                Kind::Error => DANGER,
                _ => BONE,
            };
            cr.set_source_rgba(color.0, color.1, color.2, 0.14 * alpha);
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            glyph(cr, cx, cy, kind, color, alpha * sweep);
        }
    }
}

/// Progress ring: a track, then the arc, then a small dot at its head. Indeterminate when the total
/// is unknown: a quarter turn that keeps going.
#[allow(clippy::too_many_arguments)]
fn ring(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    radius: f64,
    width: f64,
    alpha: f64,
    progress: f64,
    motion: &Motion,
) {
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15 * alpha);
    cr.set_line_width(width);
    cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();

    let start = -std::f64::consts::FRAC_PI_2
        + if progress < 0.0 {
            motion.spin * std::f64::consts::TAU
        } else {
            0.0
        };
    let sweep = if progress < 0.0 {
        0.28
    } else {
        progress.clamp(0.02, 1.0)
    };
    cr.set_source_rgba(BONE.0, BONE.1, BONE.2, alpha);
    cr.set_line_width(width);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.arc(cx, cy, radius, start, start + sweep * std::f64::consts::TAU);
    let _ = cr.stroke();

    if progress >= 0.0 {
        let angle = start + sweep * std::f64::consts::TAU;
        cr.set_source_rgba(LIGHT.0, LIGHT.1, LIGHT.2, 0.55 * alpha);
        cr.arc(
            cx + radius * angle.cos(),
            cy + radius * angle.sin(),
            1.7,
            0.0,
            std::f64::consts::TAU,
        );
        let _ = cr.fill();
    }
}

fn check_scaled(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    color: (f64, f64, f64),
    alpha: f64,
    progress: f64,
    scale: f64,
) {
    let p = progress.clamp(0.0, 1.0);
    let (x1, y1) = (cx - 5.5 * scale, cy + 0.5 * scale);
    let (x2, y2) = (cx - 1.5 * scale, cy + 4.5 * scale);
    let (x3, y3) = (cx + 5.5 * scale, cy - 4.0 * scale);
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(2.0 * scale);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.move_to(x1, y1);
    if p <= 0.4 {
        let t = p / 0.4;
        cr.line_to(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
    } else {
        cr.line_to(x2, y2);
        let t = (p - 0.4) / 0.6;
        cr.line_to(x2 + (x3 - x2) * t, y2 + (y3 - y2) * t);
    }
    let _ = cr.stroke();
}

fn glyph(cr: &cairo::Context, cx: f64, cy: f64, kind: Kind, color: (f64, f64, f64), alpha: f64) {
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(1.7);
    cr.set_line_cap(cairo::LineCap::Round);
    match kind {
        Kind::Code => {
            cr.arc(cx - 2.0, cy - 2.0, 4.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
            cr.move_to(cx + 1.0, cy + 1.0);
            cr.line_to(cx + 7.0, cy + 7.0);
            let _ = cr.stroke();
        }
        Kind::Hint | Kind::Error => {
            cr.move_to(cx, cy - 6.5);
            cr.line_to(cx, cy + 0.5);
            let _ = cr.stroke();
            cr.arc(cx, cy + 5.0, 1.1, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        _ => {}
    }
}

/// Draws the preview, cropped to fill its square, with rounded corners.
fn draw_thumb(cr: &cairo::Context, x: f64, y: f64, size: f64, thumb: &Thumb, alpha: f64) {
    let stride = thumb.width * 4;
    let Ok(surface) = cairo::ImageSurface::create_for_data(
        thumb.data.clone(),
        cairo::Format::Rgb24,
        thumb.width,
        thumb.height,
        stride,
    ) else {
        return;
    };
    let _ = cr.save();
    let corner = (size * 0.3).min(10.0);
    rounded_rect(cr, x, y, size, size, corner);
    cr.clip();
    let scale = (size / f64::from(thumb.width)).max(size / f64::from(thumb.height));
    let width = f64::from(thumb.width) * scale;
    let height = f64::from(thumb.height) * scale;
    cr.translate(x + (size - width) / 2.0, y + (size - height) / 2.0);
    cr.scale(scale, scale);
    let _ = cr.set_source_surface(&surface, 0.0, 0.0);
    let _ = cr.paint_with_alpha(alpha);
    let _ = cr.restore();
    rounded_rect(cr, x, y, size, size, corner);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.10 * alpha);
    cr.set_line_width(0.7);
    let _ = cr.stroke();
}

fn glyph_action(cr: &cairo::Context, cx: f64, cy: f64, which: u8, color: (f64, f64, f64), alpha: f64) {
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(1.6);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    if which == 0 {
        // Open: an arrow leaving its box.
        cr.move_to(cx - 1.0, cy - 2.0);
        cr.line_to(cx + 5.0, cy - 6.0);
        let _ = cr.stroke();
        cr.move_to(cx + 1.0, cy - 6.0);
        cr.line_to(cx + 5.0, cy - 6.0);
        cr.line_to(cx + 5.0, cy - 2.0);
        let _ = cr.stroke();
        cr.move_to(cx + 3.0, cy + 2.0);
        cr.line_to(cx + 3.0, cy + 5.0);
        cr.line_to(cx - 5.0, cy + 5.0);
        cr.line_to(cx - 5.0, cy - 3.0);
        cr.line_to(cx - 2.0, cy - 3.0);
        let _ = cr.stroke();
    } else {
        // Folder.
        cr.move_to(cx - 6.0, cy - 4.0);
        cr.line_to(cx - 2.0, cy - 4.0);
        cr.line_to(cx - 0.5, cy - 2.0);
        cr.line_to(cx + 6.0, cy - 2.0);
        cr.line_to(cx + 6.0, cy + 4.0);
        cr.line_to(cx - 6.0, cy + 4.0);
        cr.close_path();
        let _ = cr.stroke();
    }
}

fn act_on(act: Act, target: Option<&std::path::Path>) {
    let path = match act {
        Act::Open => target.map(|p| p.to_path_buf()),
        Act::Folder => target
            .and_then(|p| p.parent().map(|parent| parent.to_path_buf()))
            .or_else(dirs::download_dir),
    };
    if let Some(path) = path {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let r = radius.min(width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + width - r, y + height - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + r, y + height - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + r, y + r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
}

/// The pixel height of a text line, which is what centres the block honestly.
fn layout_height(layout: &Option<pango::Layout>) -> f64 {
    layout
        .as_ref()
        .map(|l| f64::from(l.pixel_size().1))
        .unwrap_or(0.0)
}

fn block_heights(layouts: &Layouts) -> (f64, f64) {
    (layout_height(&layouts.title), layout_height(&layouts.detail))
}

fn sizes_for(kind: Kind) -> (i32, i32) {
    match kind {
        Kind::Code => (10, 20),
        _ => (15, 12),
    }
}

/// A rectangle with square top corners and a rounded bottom: the island grows out of the screen.
fn rounded_bottom(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let r = radius.min(width / 2.0).min(height);
    cr.new_sub_path();
    cr.move_to(x, y);
    cr.line_to(x + width, y);
    cr.arc(
        x + width - r,
        y + height - r,
        r,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x + r,
        y + height - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.close_path();
}

fn attrs_for(font: &str, size: i32, bold: bool, spacing: f64) -> pango::AttrList {
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrString::new_family(font));
    attrs.insert(pango::AttrSize::new(size * pango::SCALE));
    attrs.insert(pango::AttrInt::new_weight(if bold {
        pango::Weight::Semibold
    } else {
        pango::Weight::Normal
    }));
    attrs.insert(pango::AttrInt::new_letter_spacing(
        (spacing * 1024.0) as i32,
    ));
    attrs
}

fn make_layout(
    area: &gtk::DrawingArea,
    font: &str,
    value: &str,
    size: i32,
    bold: bool,
    spacing: f64,
    width: f64,
) -> pango::Layout {
    let layout = area.create_pango_layout(Some(value));
    layout.set_attributes(Some(&attrs_for(font, size, bold, spacing)));
    layout.set_width((width * pango::SCALE as f64) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::Middle);
    layout.set_single_paragraph_mode(true);
    layout
}

fn measure_line(
    area: &gtk::DrawingArea,
    value: &str,
    font: &str,
    size: i32,
    bold: bool,
    spacing: f64,
) -> f64 {
    let layout = area.create_pango_layout(Some(value));
    layout.set_attributes(Some(&attrs_for(font, size, bold, spacing)));
    layout.set_single_paragraph_mode(true);
    let (width, _) = layout.pixel_size();
    f64::from(width)
}

fn draw_layout(
    cr: &cairo::Context,
    layout: &pango::Layout,
    x: f64,
    y: f64,
    color: (f64, f64, f64),
    alpha: f64,
) {
    if alpha <= 0.01 {
        return;
    }
    cr.set_source_rgba(color.0, color.1, color.2, alpha.min(1.0));
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, layout);
}

/// A damped spring, in seconds, for the short crossfades (the shape itself uses `Spring`).
fn spring(seconds: f64) -> f64 {
    let (damping, frequency): (f64, f64) = (0.78, 2.0 * std::f64::consts::PI / 0.42);
    let decay = (-damping * frequency * seconds).exp();
    let damped = frequency * (1.0 - damping * damping).sqrt();
    1.0 - decay
        * ((damped * seconds).cos() + (damping * frequency / damped) * (damped * seconds).sin())
}

fn open_target(target: Option<&std::path::Path>) {
    let path = match target {
        Some(path) => path.to_path_buf(),
        None => dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
    };
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
