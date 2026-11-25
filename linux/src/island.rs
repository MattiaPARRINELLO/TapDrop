//! The island: the pill that slides out of the top of the screen during a transfer.
//!
//! It is a Wayland layer surface (top anchor, centered), placed exactly under the camera bar. A tiny
//! Unix socket carries what to show; the drawing and the motion happen here, with real springs
//! stepped on the display's frame clock, so a state change mid-animation keeps its momentum.

use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SURFACE_WIDTH: i32 = 640;
const SURFACE_HEIGHT: i32 = 132;
const PILL_TOP: f64 = 4.0;
const SHRINK: f64 = 0.34;
const FONT: &str = "Inter";

const INK: (f64, f64, f64) = (0.97, 0.97, 0.98);
const DIM: (f64, f64, f64) = (0.64, 0.64, 0.68);
const BLUE: (f64, f64, f64) = (0.40, 0.63, 1.00);
const GREEN: (f64, f64, f64) = (0.19, 0.84, 0.60);
const AMBER: (f64, f64, f64) = (1.00, 0.78, 0.20);
const RED: (f64, f64, f64) = (0.98, 0.42, 0.42);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Something landed on the camera bump: the pill wakes up before any transfer starts.
    Detect,
    Receive,
    Send,
    Done,
    Code,
    /// A gentle hint about how to tap, in amber rather than red.
    Hint,
    Error,
}

/// One spring per property, integrated frame by frame: a state change just retargets it, so the
/// motion keeps its velocity instead of restarting from zero.
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

    fn step(&mut self, dt: f64, stiffness: f64, damping: f64) {
        let dt = dt.clamp(0.0, 1.0 / 30.0);
        let accel = (self.target - self.value) * stiffness - self.velocity * damping;
        self.velocity += accel * dt;
        self.value += self.velocity * dt;
    }
}

struct Motion {
    /// 0 hidden, 1 fully open.
    open: Spring,
    width: Spring,
    height: Spring,
    hover: Spring,
    shake: Spring,
    /// How far the check has traced itself.
    check: Spring,
    bar: Spring,
    last_frame: f64,
}

impl Motion {
    fn new() -> Self {
        Self {
            open: Spring::new(0.0),
            width: Spring::new(260.0),
            height: Spring::new(30.0),
            hover: Spring::new(0.0),
            shake: Spring::new(0.0),
            check: Spring::new(0.0),
            bar: Spring::new(0.0),
            last_frame: 0.0,
        }
    }

    fn step(&mut self, now: f64, view: &View, hovered: bool) {
        let dt = (now - self.last_frame).clamp(0.0, 0.05);
        self.last_frame = now;
        let leaving = view.hide_at.map(|at| Instant::now() > at).unwrap_or(false);
        self.open
            .to(if view.visible && !leaving { 1.0 } else { 0.0 });
        self.width.to(view.target_width);
        self.height.to(height_for(view.kind));
        self.hover.to(if hovered { 1.0 } else { 0.0 });
        self.shake.to(0.0);
        self.check.to(if view.kind == Kind::Done && view.visible {
            1.0
        } else {
            0.0
        });
        self.bar
            .to(f64::from(view.progress.unwrap_or(view.last_progress)));

        self.open.step(dt, 170.0, 24.0);
        self.width.step(dt, 210.0, 26.0);
        self.height.step(dt, 190.0, 24.0);
        self.hover.step(dt, 300.0, 28.0);
        self.shake.step(dt, 420.0, 26.0);
        self.check.step(dt, 120.0, 20.0);
        self.bar.step(dt, 90.0, 22.0);
    }
}

#[derive(Default)]
struct Layouts {
    title: Option<pango::Layout>,
    detail: Option<pango::Layout>,
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
    /// The width the text asks for, measured once per state change.
    target_width: f64,
    /// Pango layouts, built once per state change: building four per frame at 120 Hz was the last
    /// real source of jitter.
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
            target_width: 300.0,
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

    /// The pill is only as wide as its text needs, within sane bounds.
    fn measure(&mut self, area: &gtk::DrawingArea) {
        if self.kind == Kind::Detect {
            self.target_width = 62.0;
            return;
        }
        let (title_size, detail_size) = sizes_for(self.kind);
        let title = measure_line(area, &self.title, title_size, true, -0.35);
        let detail = measure_line(area, &self.detail, detail_size, false, -0.15);
        let text = title.max(detail);
        let width = if self.kind == Kind::Code { text + 78.0 } else { text + 72.0 };
        self.target_width = width.clamp(if self.kind == Kind::Code { 300.0 } else { 320.0 }, 520.0);

        // Build the layouts once here: drawing at 120 Hz only shows them.
        let inner = (self.target_width - 66.0).max(24.0);
        self.layouts.title = Some(make_layout(area, &self.title, title_size, true, -0.35, inner));
        self.layouts.detail = Some(make_layout(area, &self.detail, detail_size, false, -0.15, inner));
        if !self.prev_title.is_empty() {
            let (pt, pd) = sizes_for(self.prev_kind);
            self.layouts.prev_title = Some(make_layout(area, &self.prev_title, pt, true, -0.35, inner));
            self.layouts.prev_detail = Some(make_layout(area, &self.prev_detail, pd, false, -0.15, inner));
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

    /// The camera bump just felt something: a pulse, long before the phone has sent anything.
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
        eprintln!("tapdrop: îlot -> {title} ({detail})");
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

    /// A short instruction, not an error: "turn the phone around", "hold it a second longer".
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
    window.set_exclusive_zone(0);
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
    let hovered = Rc::new(Cell::new(false));
    let pointer = Rc::new(Cell::new((0.0f64, 0.0f64)));
    let pill = Rc::new(Cell::new(None::<(i32, i32, i32, i32)>));
    let target = Rc::new(RefCell::new(None::<PathBuf>));

    // The frame clock drives the springs and the redraw: 120 steps a second on this display.
    area.add_tick_callback({
        let view = view.clone();
        let motion = motion.clone();
        let hovered = hovered.clone();
        let area_for_tick = area.clone();
        move |area, clock| {
            let now = clock.frame_time() as f64 / 1_000_000.0;
            motion.borrow_mut().step(now, &view.borrow(), hovered.get());
            if view.borrow().visible {
                area_for_tick.queue_draw();
            }
            let _ = area;
            glib::ControlFlow::Continue
        }
    });

    area.set_draw_func({
        let view = view.clone();
        let motion = motion.clone();
        let pill = pill.clone();
        let pointer = pointer.clone();
        move |area, cr, _w, _h| {
            draw(area, cr, &view.borrow(), &motion.borrow(), &pill, pointer.get());
        }
    });

    let motion_controller = gtk::EventControllerMotion::new();
    motion_controller.connect_motion({
        let pointer = pointer.clone();
        move |_, x, y| pointer.set((x, y))
    });
    motion_controller.connect_enter({
        let (hovered, area) = (hovered.clone(), area.clone());
        move |_, _, _| {
            hovered.set(true);
            area.set_cursor_from_name(Some("pointer"));
        }
    });
    motion_controller.connect_leave({
        let (hovered, area) = (hovered.clone(), area.clone());
        move |_| {
            hovered.set(false);
            area.set_cursor_from_name(None);
        }
    });
    area.add_controller(motion_controller);

    let click = gtk::GestureClick::new();
    click.connect_released({
        let (pill, target) = (pill.clone(), target.clone());
        move |_, _, x, y| {
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
        move || {
            while let Ok(value) = receiver.try_recv() {
                apply(&view, &area, &target, &motion, &value);
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
        move || {
            let now = Instant::now();
            {
                let mut state = view.borrow_mut();
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
                        if now >= at + Duration::from_secs_f64(SHRINK) && settled {
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
                let scale = area.scale_factor().max(1) as i32;
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

fn apply(
    view: &Rc<RefCell<View>>,
    area: &gtk::DrawingArea,
    target: &Rc<RefCell<Option<PathBuf>>>,
    motion: &Rc<RefCell<Motion>>,
    value: &Value,
) {
    let title = value["title"].as_str().unwrap_or_default();
    let detail = value["detail"].as_str().unwrap_or_default();
    // A new transfer forgets the file of the previous one: a click must never open a stale photo.
    if matches!(value["t"].as_str(), Some("recv" | "send" | "detect")) {
        *target.borrow_mut() = None;
    }
    if value["t"] == "done" {
        let path = value["target"].as_str().unwrap_or_default();
        *target.borrow_mut() = if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        };
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
        "done" => {
            state.show(
                Kind::Done,
                title,
                detail,
                None,
                Some(Duration::from_secs(3)),
            );
            // A small impulse: the pill reacts to the result instead of just changing colour.
            motion.borrow_mut().width.kick(-240.0);
        }
        "error" => {
            state.show(
                Kind::Error,
                title,
                detail,
                None,
                Some(Duration::from_secs(4)),
            );
            motion.borrow_mut().shake.kick(-900.0);
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

fn draw(
    _area: &gtk::DrawingArea,
    cr: &cairo::Context,
    view: &View,
    motion: &Motion,
    pill: &Cell<Option<(i32, i32, i32, i32)>>,
    pointer: (f64, f64),
) {
    let open = motion.open.value;
    let alpha = open.clamp(0.0, 1.0);
    if !view.visible || (view.title.is_empty() && view.kind != Kind::Detect) || alpha <= 0.01 {
        cr.set_operator(cairo::Operator::Source);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        let _ = cr.paint();
        pill.set(None);
        return;
    }

    let now = Instant::now();
    let morph = if view.kind == view.prev_kind {
        1.0
    } else {
        spring(now.duration_since(view.kind_since).as_secs_f64() * 2.6).clamp(0.0, 1.0)
    };

    let hover = motion.hover.value.clamp(0.0, 1.0);
    let scale = 1.0 + 0.012 * hover;
    let width = motion.width.value.max(24.0) * scale;
    let height = (motion.height.value.max(2.0) * open.max(0.0).min(1.2)) * scale;
    let x = (SURFACE_WIDTH as f64 - width) / 2.0 + motion.shake.value;
    let y = PILL_TOP - 1.2 * hover;
    let radius = height / 2.0;
    pill.set(Some((x as i32, y as i32, width as i32, height as i32)));

    let accent = mix(accent_of(view.prev_kind), accent_of(view.kind), morph);
    let pulse = (motion.open.value.max(0.0)).min(1.0);

    // A halo in the state's colour: what makes the pill feel alive on a dark panel.
    for (spread, strength) in [(10.0, 0.012), (6.0, 0.017), (3.0, 0.022)] {
        rounded_rect(
            cr,
            x - spread,
            y - spread,
            width + 2.0 * spread,
            height + 2.0 * spread,
            radius + spread,
        );
        cr.set_source_rgba(
            accent.0,
            accent.1,
            accent.2,
            strength * alpha * pulse * (1.0 + 0.3 * hover),
        );
        let _ = cr.fill();
    }

    // Soft shadow, built from stacked translucent outlines: Cairo has no blur, the eye reads this
    // the same at this size, and it is what lifts the pill off the wallpaper.
    for (spread, strength) in [(7.0, 0.040), (4.0, 0.046), (2.0, 0.052)] {
        rounded_rect(
            cr,
            x - spread,
            y + 2.0 - spread,
            width + 2.0 * spread,
            height + 2.0 * spread,
            radius + spread,
        );
        cr.set_source_rgba(0.0, 0.0, 0.0, strength * alpha);
        let _ = cr.fill();
    }

    // The fill is never flat: a very slow gradient drifts through it, tinted by the state.
    let drift = 0.5 + 0.16 * (elapsed_seconds(view).sin());
    let body = cairo::LinearGradient::new(x, y, x + width, y + height * 0.9);
    body.add_color_stop_rgba(0.0, 0.012, 0.012, 0.016, 0.975 * alpha);
    body.add_color_stop_rgba(
        drift.clamp(0.2, 0.8),
        0.030 + accent.0 * 0.020,
        0.032 + accent.1 * 0.020,
        0.040 + accent.2 * 0.020,
        0.972 * alpha,
    );
    body.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.985 * alpha);
    rounded_rect(cr, x, y, width, height, radius);
    let _ = cr.set_source(&body);
    let _ = cr.fill();

    cr.set_source_rgba(1.0, 1.0, 1.0, (0.05 + 0.06 * hover) * alpha);
    cr.set_line_width(0.6 + 0.4 * hover);
    rounded_rect(cr, x, y, width, height, radius);
    let _ = cr.stroke();

    if height < 22.0 {
        return;
    }

    // A lit hairline along the top edge only: the glass catches the light from above.
    let edge = cairo::LinearGradient::new(x, y, x, y + height * 0.65);
    edge.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.09 * alpha);
    edge.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
    let _ = cr.save();
    rounded_rect(cr, x, y, width, height, radius);
    cr.clip();
    rounded_rect(cr, x + 0.7, y + 0.7, width - 1.4, height - 1.4, radius - 0.7);
    let _ = cr.set_source(&edge);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    let _ = cr.restore();

    // The body sheen: a faint light falling from the top inside the pill.
    let sheen = cairo::LinearGradient::new(x, y, x, y + height * 0.45);
    sheen.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.08 * alpha);
    sheen.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
    let _ = cr.save();
    rounded_rect(cr, x, y, width, height, radius);
    cr.clip();
    let _ = cr.set_source(&sheen);
    let _ = cr.paint();
    let _ = cr.restore();

    // The content lands in a stagger, each part a beat after the one before.
    let stage = |offset: f64| ((open - offset) / (1.0 - offset)).clamp(0.0, 1.0);
    let icon_stage = stage(0.0);
    let title_stage = stage(0.12);
    let detail_stage = stage(0.2);
    let bar_stage = stage(0.3);
    let elapsed = now.duration_since(view.shown_at).as_secs_f64();

    // A sweep of light while a transfer runs, so it reads as alive.
    if view.progress.is_some() && bar_stage > 0.2 {
        let phase = (elapsed % 2.4) / 2.4;
        let band = 120.0;
        let bx = x - band + (width + 2.0 * band) * phase;
        let sweep = cairo::LinearGradient::new(bx, y, bx + band, y);
        sweep.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.0);
        sweep.add_color_stop_rgba(0.5, 1.0, 1.0, 1.0, 0.055 * alpha * bar_stage);
        sweep.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
        let _ = cr.save();
        rounded_rect(cr, x, y, width, height, radius);
        cr.clip();
        let _ = cr.set_source(&sweep);
        let _ = cr.paint();
        let _ = cr.restore();
    }

    let center_y = y + height / 2.0;
    let icon_x = if view.kind == Kind::Detect && view.prev_kind != Kind::Detect {
        x + width / 2.0
    } else {
        x + 24.0
    };
    if morph < 0.999 {
        state_icon(
            cr,
            icon_x,
            center_y,
            view.prev_kind,
            alpha * icon_stage * (1.0 - morph),
            elapsed,
            hover,
            1.0,
            motion.check.value,
        );
    }
    let bob = if view.kind == Kind::Receive && view.progress.is_some() {
        (elapsed * 4.2).sin() * 1.6
    } else {
        0.0
    };
    state_icon(
        cr,
        icon_x,
        center_y + bob,
        view.kind,
        alpha * icon_stage * morph,
        elapsed,
        hover,
        motion.check.value,
        motion.check.value,
    );

    let text_x = x + 46.0;

    // The pointer light: a soft radial highlight under the cursor while hovering.
    if hover > 0.02 {
        let light = cairo::RadialGradient::new(
            pointer.0,
            pointer.1,
            0.0,
            pointer.0,
            pointer.1,
            150.0,
        );
        light.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.07 * hover * alpha);
        light.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
        let _ = cr.save();
        rounded_rect(cr, x, y, width, height, radius);
        cr.clip();
        cr.arc(pointer.0, pointer.1, 150.0, 0.0, std::f64::consts::TAU);
        let _ = cr.set_source(&light);
        let _ = cr.fill();
        let _ = cr.restore();
    }

    if morph < 0.999 {
        if let (Some(title), Some(detail)) = (&view.layouts.prev_title, &view.layouts.prev_detail) {
            let a = alpha * title_stage * (1.0 - morph);
            let (title_y, detail_y) = text_positions(view.prev_kind, y, 6.0);
            draw_layout(cr, title, text_x, title_y, INK, a);
            draw_layout(cr, detail, text_x, detail_y, DIM, a);
        }
    }
    if let (Some(title), Some(detail)) = (&view.layouts.title, &view.layouts.detail) {
        let a = alpha * (title_stage * 0.5 + detail_stage * 0.5) * morph;
        let (title_y, detail_y) = text_positions(view.kind, y, 6.0 * (1.0 - detail_stage));
        draw_layout(cr, title, text_x, title_y, INK, a);
        draw_layout(cr, detail, text_x, detail_y, DIM, a);
    }

    let bar_alpha = if view.progress.is_some() {
        bar_stage
    } else {
        bar_stage * (1.0 - morph)
    };
    if bar_alpha > 0.02 {
        let track_x = text_x;
        let track_y = y + height - 12.5;
        let track_width = (width - 46.0 - 20.0).max(10.0);
        rounded_rect(cr, track_x, track_y, track_width, 3.0, 1.5);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.09 * alpha * bar_alpha);
        let _ = cr.fill();
        // An inner shadow along the top of the track gives it depth.
        rounded_rect(cr, track_x, track_y, track_width, 1.0, 0.5);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.25 * alpha * bar_alpha);
        let _ = cr.fill();

        let filled = (track_width * motion.bar.value.clamp(0.0, 1.0)).max(0.0);
        if filled > 1.0 {
            let fill = cairo::LinearGradient::new(track_x, track_y, track_x, track_y + 3.0);
            fill.add_color_stop_rgba(0.0, accent.0, accent.1, accent.2, alpha * bar_alpha);
            fill.add_color_stop_rgba(
                1.0,
                accent.0 * 0.82,
                accent.1 * 0.82,
                accent.2 * 0.82,
                alpha * bar_alpha,
            );
            rounded_rect(cr, track_x, track_y, filled, 3.0, 1.5);
            let _ = cr.set_source(&fill);
            let _ = cr.fill();

            // A glowing head on the bar, like a needle.
            let head_x = track_x + filled;
            let glow =
                cairo::RadialGradient::new(head_x, track_y + 1.5, 0.0, head_x, track_y + 1.5, 8.0);
            glow.add_color_stop_rgba(0.0, accent.0, accent.1, accent.2, 0.45 * alpha * bar_alpha);
            glow.add_color_stop_rgba(1.0, accent.0, accent.1, accent.2, 0.0);
            cr.arc(head_x, track_y + 1.5, 8.0, 0.0, std::f64::consts::TAU);
            let _ = cr.set_source(&glow);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.9 * alpha * bar_alpha);
            cr.arc(head_x, track_y + 1.5, 2.2, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }
}

/// One slow turn every ten seconds, in radians.
fn elapsed_seconds(view: &View) -> f64 {
    Instant::now().duration_since(view.shown_at).as_secs_f64() * std::f64::consts::TAU / 10.0
}

fn text_positions(kind: Kind, y: f64, slide: f64) -> (f64, f64) {
    match kind {
        // For a code, the big line is the code itself (drawn from `detail`), the small one its source.
        Kind::Code => (y + 44.0 + slide * 0.4, y + 10.0 + slide * 0.4),
        _ => (y + 11.0 + slide * 0.5, y + 33.0 + slide),
    }
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

fn sizes_for(kind: Kind) -> (i32, i32) {
    match kind {
        Kind::Code => (10, 21),
        _ => (13, 11),
    }
}

fn make_layout(
    area: &gtk::DrawingArea,
    value: &str,
    size: i32,
    bold: bool,
    spacing: f64,
    width: f64,
) -> pango::Layout {
    let layout = area.create_pango_layout(Some(value));
    layout.set_attributes(Some(&attrs_for(size, bold, spacing)));
    layout.set_width((width * pango::SCALE as f64) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_single_paragraph_mode(true);
    layout
}

fn measure_line(area: &gtk::DrawingArea, value: &str, size: i32, bold: bool, spacing: f64) -> f64 {
    let layout = area.create_pango_layout(Some(value));
    let attrs = attrs_for(size, bold, spacing);
    layout.set_attributes(Some(&attrs));
    layout.set_single_paragraph_mode(true);
    let (width, _) = layout.pixel_size();
    f64::from(width)
}

fn attrs_for(size: i32, bold: bool, spacing: f64) -> pango::AttrList {
    let attrs = pango::AttrList::new();
    attrs.insert(pango::AttrString::new_family(FONT));
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

#[allow(clippy::too_many_arguments)]
/// The icon of each state: breathing rings while something lands, a check that draws itself when
/// a transfer lands, and the plain glyphs in between.
#[allow(clippy::too_many_arguments)]
fn state_icon(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    kind: Kind,
    alpha: f64,
    elapsed: f64,
    hover: f64,
    sweep: f64,
    check: f64,
) {
    let color = accent_of(kind);
    if kind == Kind::Detect {
        // Two rings, half a beat apart: it reads as a radar sweep, not a blinking dot.
        for offset in [0.0, 0.5] {
            let phase = ((elapsed / 1.5) + offset) % 1.0;
            cr.set_source_rgba(color.0, color.1, color.2, 0.42 * (1.0 - phase) * alpha);
            cr.set_line_width(1.7);
            cr.arc(cx, cy, 5.0 + 15.0 * phase, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
        cr.set_source_rgba(color.0, color.1, color.2, alpha);
        cr.arc(cx, cy, 5.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
        return;
    }

    let radius = 13.0 * alpha.clamp(0.0, 1.0).sqrt() * (1.0 + 0.06 * hover);
    if radius <= 1.0 {
        return;
    }
    if kind == Kind::Done {
        // A single expanding ring, then the check traces itself.
        let ring = check.clamp(0.0, 1.0);
        cr.set_source_rgba(color.0, color.1, color.2, 0.30 * (1.0 - ring) * alpha);
        cr.set_line_width(1.4);
        cr.arc(cx, cy, radius + 6.0 * ring, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
    }
    let disc = cairo::RadialGradient::new(cx, cy - radius * 0.4, 0.0, cx, cy, radius);
    disc.add_color_stop_rgba(0.0, color.0, color.1, color.2, 0.26 * alpha);
    disc.add_color_stop_rgba(1.0, color.0, color.1, color.2, 0.16 * alpha);
    cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
    let _ = cr.set_source(&disc);
    let _ = cr.fill();

    if kind == Kind::Done {
        let p = sweep.clamp(0.0, 1.0);
        let (x1, y1) = (cx - 6.0, cy);
        let (x2, y2) = (cx - 1.5, cy + 4.5);
        let (x3, y3) = (cx + 6.0, cy - 4.5);
        cr.set_source_rgba(color.0, color.1, color.2, alpha);
        cr.set_line_width(1.7);
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
        return;
    }
    draw_glyph(cr, cx, cy, kind, color, alpha);
}

fn accent_of(kind: Kind) -> (f64, f64, f64) {
    match kind {
        Kind::Done => GREEN,
        Kind::Error => RED,
        Kind::Hint => AMBER,
        Kind::Detect | Kind::Receive | Kind::Send | Kind::Code => BLUE,
    }
}

fn height_for(kind: Kind) -> f64 {
    match kind {
        Kind::Detect => 40.0,
        _ => 70.0,
    }
}

fn mix(from: (f64, f64, f64), to: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    let t = t.clamp(0.0, 1.0);
    (
        from.0 + (to.0 - from.0) * t,
        from.1 + (to.1 - from.1) * t,
        from.2 + (to.2 - from.2) * t,
    )
}

/// A damped spring, in seconds: it opens over about half a second, overshoots a little and settles.
fn spring(seconds: f64) -> f64 {
    let (damping, frequency): (f64, f64) = (0.55, 12.0);
    let damped = frequency * (1.0 - damping * damping).sqrt();
    let decay = (-damping * frequency * seconds).exp();
    1.0 - decay
        * ((damped * seconds).cos() + (damping * frequency / damped) * (damped * seconds).sin())
}

fn draw_glyph(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    kind: Kind,
    color: (f64, f64, f64),
    alpha: f64,
) {
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(1.7);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    match kind {
        Kind::Receive | Kind::Send => {
            let direction = if kind == Kind::Receive { 1.0 } else { -1.0 };
            cr.move_to(cx, cy - 7.0 * direction);
            cr.line_to(cx, cy + 5.0 * direction);
            cr.move_to(cx - 5.0, cy + direction);
            cr.line_to(cx, cy + 6.0 * direction);
            cr.line_to(cx + 5.0, cy + direction);
            let _ = cr.stroke();
        }
        Kind::Done => {
            cr.move_to(cx - 6.0, cy);
            cr.line_to(cx - 1.5, cy + 4.5);
            cr.line_to(cx + 6.0, cy - 4.5);
            let _ = cr.stroke();
        }
        Kind::Error | Kind::Hint => {
            cr.move_to(cx, cy - 7.0);
            cr.line_to(cx, cy + 0.5);
            let _ = cr.stroke();
            cr.arc(cx, cy + 5.0, 1.2, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        Kind::Detect => {}
        Kind::Code => {
            cr.arc(cx - 2.0, cy - 2.0, 4.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
            cr.move_to(cx + 1.0, cy + 1.0);
            cr.line_to(cx + 7.0, cy + 7.0);
            cr.move_to(cx + 4.0, cy + 7.0);
            cr.line_to(cx + 7.0, cy + 4.0);
            let _ = cr.stroke();
        }
    }
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let r = radius.min(width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
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
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

fn open_target(target: Option<&std::path::Path>) {
    let path = match target {
        Some(path) => path.to_path_buf(),
        None => dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
    };
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}
