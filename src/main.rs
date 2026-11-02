#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod book;
mod online;
mod storage;
mod system;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, pos2, text::LayoutJob, vec2, Align2, Color32, FontId, Key, Rect, RichText, TextFormat,
    ViewportCommand,
};

use book::Book;
use online::{Cmd, Event, OnlineEngine, ONLINE_VOICES};
use storage::{Progress, Store};
use system::{SystemEngine, VoiceInfo};

const PAPER: Color32 = Color32::from_rgb(250, 243, 224);
const INK: Color32 = Color32::from_rgb(45, 38, 32);
const HEADING: Color32 = Color32::from_rgb(120, 52, 28);
const HL_PLAY: Color32 = Color32::from_rgb(255, 214, 90);
const HL_IDLE: Color32 = Color32::from_rgb(236, 224, 188);
const ANIM_SECS: f64 = 0.55;

#[derive(Clone, Copy, PartialEq)]
enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy)]
enum Action {
    Prev,
    Next,
    Play,
    Pause,
    Stop,
    GoStart,
    GoSaved,
}

#[derive(Clone)]
struct Anim {
    old_start: usize,
    old_end: usize,
    started: f64,
    /// +1 = página siguiente, -1 = página anterior
    dir: f32,
}

struct App {
    store: Store,
    book: Option<Book>,
    book_key: String,
    texts: Arc<Vec<String>>,

    /// Primera frase de la página visible.
    page_start: usize,
    /// Frase actual (la que se está leyendo o donde se quedó la lectura).
    cursor: usize,
    state: PlayState,

    sys: Option<SystemEngine>,
    sys_err: Option<String>,
    sys_voices: Vec<VoiceInfo>,
    online: OnlineEngine,
    gen: u64,
    fallback_deadline: Option<Instant>,

    /// Posición guardada pendiente de confirmar por el usuario.
    pending_resume: Option<usize>,
    status: String,
    want_open: bool,

    anim: Option<Anim>,
    end_cache: Option<((usize, u32, u32, u32), usize)>,
    cur_end: usize,
    layout: (f32, f32),
    last_save: Instant,
}

fn speakable(s: &str) -> bool {
    s.chars().any(|c| c.is_alphanumeric())
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        ctx.set_visuals(egui::Visuals::dark());

        let store = Store::load();
        let (sys, sys_err) = match SystemEngine::new(&ctx) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };
        let sys_voices = sys.as_ref().map(|s| s.voice_list()).unwrap_or_default();

        let mut app = App {
            store,
            book: None,
            book_key: String::new(),
            texts: Arc::new(Vec::new()),
            page_start: 0,
            cursor: 0,
            state: PlayState::Stopped,
            sys,
            sys_err,
            sys_voices,
            online: OnlineEngine::new(ctx.clone()),
            gen: 0,
            fallback_deadline: None,
            pending_resume: None,
            status: String::new(),
            want_open: false,
            anim: None,
            end_cache: None,
            cur_end: 0,
            layout: (600.0, 400.0),
            last_save: Instant::now(),
        };
        app.normalize_voice();

        // Reabrir el último libro: si tiene posición guardada, se pregunta.
        if let Some(last) = app.store.last_book.clone() {
            let p = PathBuf::from(&last);
            if p.exists() {
                app.open_book(&ctx, p);
            }
        }
        app
    }

    // ───────────────────────── voces ─────────────────────────

    fn use_online(&self) -> bool {
        self.store.voice.starts_with("web:")
    }

    fn online_voice(&self) -> String {
        self.store.voice.trim_start_matches("web:").to_string()
    }

    fn normalize_voice(&mut self) {
        let valid = if let Some(id) = self.store.voice.strip_prefix("sys:") {
            self.sys_voices.iter().any(|v| v.id == id)
        } else if let Some(n) = self.store.voice.strip_prefix("web:") {
            ONLINE_VOICES.iter().any(|(k, _)| *k == n)
        } else {
            false
        };
        if !valid {
            self.store.voice = match self.sys_voices.iter().find(|v| v.spanish) {
                Some(v) => format!("sys:{}", v.id),
                None => format!("web:{}", ONLINE_VOICES[0].0),
            };
        }
        self.apply_system_voice();
    }

    fn apply_system_voice(&mut self) {
        if let Some(id) = self.store.voice.strip_prefix("sys:").map(|s| s.to_string()) {
            if let Some(s) = self.sys.as_mut() {
                if let Err(e) = s.set_voice(&id) {
                    self.status = format!("No se pudo seleccionar la voz: {e}");
                }
            }
        }
    }

    fn voice_label(&self, key: &str) -> String {
        if let Some(id) = key.strip_prefix("sys:") {
            self.sys_voices
                .iter()
                .find(|v| v.id == id)
                .map(|v| format!("Sistema · {}", v.label))
                .unwrap_or_else(|| "Voz desconocida".into())
        } else if let Some(n) = key.strip_prefix("web:") {
            ONLINE_VOICES
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, l)| format!("En línea · {l}"))
                .unwrap_or_else(|| "Voz desconocida".into())
        } else {
            "Sin voz".into()
        }
    }

    fn set_voice(&mut self, ctx: &egui::Context, key: String) {
        self.store.voice = key;
        self.apply_system_voice();
        self.store.save();
        if self.state == PlayState::Playing {
            let c = self.cursor;
            self.start_reading(ctx, c);
        }
    }

    // ───────────────────────── libro ─────────────────────────

    fn open_book(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.halt_engines();
        self.gen += 1;
        self.state = PlayState::Stopped;
        match Book::open(&path) {
            Ok(book) => {
                let key = path
                    .canonicalize()
                    .unwrap_or_else(|_| path.clone())
                    .to_string_lossy()
                    .to_string();
                let n = book.sentences.len();
                self.texts = Arc::new(book.sentences.iter().map(|s| s.text.clone()).collect());
                ctx.send_viewport_cmd(ViewportCommand::Title(format!(
                    "{} — Lector EPUB",
                    book.title
                )));
                self.book = Some(book);
                self.page_start = 0;
                self.cursor = 0;
                self.anim = None;
                self.end_cache = None;
                self.cur_end = 0;
                // ¿Hay una posición guardada de este libro? Se pregunta al usuario.
                self.pending_resume = self
                    .store
                    .progress
                    .get(&key)
                    .and_then(|p| (p.sentence > 0 && p.sentence < n).then_some(p.sentence));
                self.store.last_book = Some(key.clone());
                self.book_key = key;
                self.store.save();
                self.status.clear();
            }
            Err(e) => self.status = e,
        }
    }

    fn save_progress(&mut self) {
        if self.pending_resume.is_some() {
            return; // no pisar la posición guardada mientras se decide
        }
        if let Some(b) = &self.book {
            self.store.progress.insert(
                self.book_key.clone(),
                Progress { sentence: self.cursor, total: b.sentences.len() },
            );
            self.store.last_book = Some(self.book_key.clone());
            self.store.save();
            self.last_save = Instant::now();
        }
    }

    // ───────────────────── maquetación / páginas ─────────────────────

    fn build_job(&self, start: usize, end: usize, hl: Option<(usize, Color32)>, wrap: f32) -> LayoutJob {
        let book = self.book.as_ref().expect("libro abierto");
        let fs = self.store.font_size;
        let mut job = LayoutJob::default();
        job.wrap.max_width = wrap;

        let plain = TextFormat {
            font_id: FontId::proportional(fs),
            color: INK,
            line_height: Some(fs * 1.5),
            ..Default::default()
        };

        for i in start..end.min(book.sentences.len()) {
            let s = &book.sentences[i];
            if i > start && s.new_para {
                job.append(if s.heading { "\n\n" } else { "\n" }, 0.0, plain.clone());
            }
            if s.new_para && !s.heading {
                job.append("    ", 0.0, plain.clone());
            }
            let size = if s.heading { fs * 1.45 } else { fs };
            let mut fmt = TextFormat {
                font_id: FontId::proportional(size),
                color: if s.heading { HEADING } else { INK },
                line_height: Some(size * 1.5),
                ..Default::default()
            };
            if let Some((h, c)) = hl {
                if h == i {
                    fmt.background = c;
                }
            }
            job.append(&s.text, 0.0, fmt);
            job.append(" ", 0.0, plain.clone());
        }
        job
    }

    fn measure(&self, ctx: &egui::Context, start: usize, end: usize, w: f32) -> f32 {
        let job = self.build_job(start, end, None, w);
        ctx.fonts(|f| f.layout_job(job).size().y)
    }

    /// Última frase (exclusive) que cabe en la página que empieza en `start`.
    fn page_end(&mut self, ctx: &egui::Context, start: usize, w: f32, h: f32) -> usize {
        let key = (start, w as u32, h as u32, (self.store.font_size * 10.0) as u32);
        if let Some((k, e)) = self.end_cache {
            if k == key {
                return e;
            }
        }
        let n = self.texts.len();
        let mut lo = start + 1;
        let mut hi = (start + 150).min(n);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if self.measure(ctx, start, mid, w) <= h {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        self.end_cache = Some((key, lo));
        lo
    }

    /// Inicio de la página anterior a la que termina en `end`.
    fn prev_start(&self, ctx: &egui::Context, end: usize) -> usize {
        let (w, h) = self.layout;
        let mut lo = end.saturating_sub(150);
        let mut hi = end.saturating_sub(1);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.measure(ctx, mid, end, w) <= h {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    fn turn_to(&mut self, ctx: &egui::Context, new_start: usize, dir: f32) {
        if new_start == self.page_start {
            return;
        }
        self.anim = Some(Anim {
            old_start: self.page_start,
            old_end: self.cur_end.max(self.page_start + 1),
            started: ctx.input(|i| i.time),
            dir,
        });
        self.page_start = new_start;
        ctx.request_repaint();
    }

    fn go_page(&mut self, ctx: &egui::Context, start: usize, dir: f32) {
        self.turn_to(ctx, start, dir);
        if self.state == PlayState::Playing {
            self.start_reading(ctx, start);
        } else {
            self.cursor = start;
        }
        self.save_progress();
    }

    fn next_page(&mut self, ctx: &egui::Context) {
        if self.cur_end < self.texts.len() {
            let s = self.cur_end;
            self.go_page(ctx, s, 1.0);
        }
    }

    fn prev_page(&mut self, ctx: &egui::Context) {
        if self.page_start > 0 {
            let s = self.prev_start(ctx, self.page_start);
            self.go_page(ctx, s, -1.0);
        }
    }

    /// Si la frase actual sale de la página, se pasa de página con efecto.
    fn follow_cursor(&mut self, ctx: &egui::Context) {
        if self.cur_end > self.page_start
            && (self.cursor >= self.cur_end || self.cursor < self.page_start)
        {
            let dir = if self.cursor >= self.cur_end { 1.0 } else { -1.0 };
            let c = self.cursor;
            self.turn_to(ctx, c, dir);
        }
        if self.state == PlayState::Playing && self.last_save.elapsed() > Duration::from_secs(3) {
            self.save_progress();
        }
    }

    // ───────────────────── control de la lectura ─────────────────────

    fn halt_engines(&mut self) {
        self.online.send(Cmd::Stop);
        if let Some(s) = self.sys.as_mut() {
            s.stop();
        }
        self.fallback_deadline = None;
    }

    fn start_reading(&mut self, ctx: &egui::Context, from: usize) {
        if self.book.is_none() {
            return;
        }
        self.halt_engines();
        self.gen += 1;
        self.cursor = from.min(self.texts.len().saturating_sub(1));
        self.state = PlayState::Playing;
        self.status.clear();

        if self.use_online() {
            let rate = ((self.store.rate - 1.0) * 100.0).round().clamp(-50.0, 100.0) as i32;
            self.online.send(Cmd::Volume(self.store.volume));
            self.online.send(Cmd::Start {
                gen: self.gen,
                from: self.cursor,
                texts: self.texts.clone(),
                voice: self.online_voice(),
                rate,
            });
            self.follow_cursor(ctx);
        } else {
            self.speak_current(ctx);
        }
    }

    fn speak_current(&mut self, ctx: &egui::Context) {
        let n = self.texts.len();
        while self.cursor < n && !speakable(&self.texts[self.cursor]) {
            self.cursor += 1;
        }
        if self.cursor >= n {
            self.finish(ctx);
            return;
        }
        self.follow_cursor(ctx);
        let text = self.texts[self.cursor].clone();
        let (rate, vol) = (self.store.rate, self.store.volume);
        let res = match self.sys.as_mut() {
            Some(s) => s.speak(&text, rate, vol),
            None => Err("No hay voces del sistema disponibles; elige una voz en línea.".to_string()),
        };
        match res {
            Ok(hint) => self.fallback_deadline = hint.map(|d| Instant::now() + d),
            Err(e) => {
                self.status = format!("Error de voz: {e}");
                self.state = PlayState::Stopped;
            }
        }
    }

    fn pause(&mut self) {
        if self.state != PlayState::Playing {
            return;
        }
        self.state = PlayState::Paused;
        if self.use_online() {
            self.online.send(Cmd::Pause);
        } else {
            // Las voces del sistema no permiten pausar de forma fiable:
            // se detiene y se retoma desde el principio de la frase actual.
            if let Some(s) = self.sys.as_mut() {
                s.stop();
            }
            self.fallback_deadline = None;
        }
        self.save_progress();
    }

    fn resume(&mut self, ctx: &egui::Context) {
        if self.state != PlayState::Paused {
            return;
        }
        self.state = PlayState::Playing;
        if self.use_online() {
            self.online.send(Cmd::Resume);
        } else {
            self.speak_current(ctx);
        }
    }

    fn stop(&mut self) {
        self.halt_engines();
        self.gen += 1; // ignora eventos atrasados
        self.state = PlayState::Stopped;
        self.save_progress();
    }

    fn finish(&mut self, ctx: &egui::Context) {
        self.halt_engines();
        self.gen += 1;
        self.state = PlayState::Stopped;
        self.status = "Has llegado al final del libro.".into();
        self.store.progress.remove(&self.book_key);
        self.cursor = 0;
        self.turn_to(ctx, 0, -1.0);
        self.store.save();
    }

    fn apply_action(&mut self, ctx: &egui::Context, a: Action) {
        match a {
            Action::Prev => self.prev_page(ctx),
            Action::Next => self.next_page(ctx),
            Action::Play => match self.state {
                PlayState::Paused => self.resume(ctx),
                PlayState::Stopped => {
                    let c = if self.cursor >= self.texts.len() { 0 } else { self.cursor };
                    self.start_reading(ctx, c);
                }
                PlayState::Playing => {}
            },
            Action::Pause => self.pause(),
            Action::Stop => self.stop(),
            Action::GoStart => {
                self.stop();
                self.cursor = 0;
                self.turn_to(ctx, 0, -1.0);
                self.save_progress();
            }
            Action::GoSaved => {
                let saved = self.store.progress.get(&self.book_key).map(|p| p.sentence);
                if let Some(s) = saved {
                    self.stop();
                    let dir = if s >= self.page_start { 1.0 } else { -1.0 };
                    self.cursor = s;
                    self.turn_to(ctx, s, dir);
                }
            }
        }
    }

    // ───────────────────── eventos de audio ─────────────────────

    fn process_events(&mut self, ctx: &egui::Context) {
        let mut evs = Vec::new();
        while let Ok(e) = self.online.rx.try_recv() {
            evs.push(e);
        }
        for e in evs {
            match e {
                Event::Started { gen, idx } if gen == self.gen && self.state != PlayState::Stopped => {
                    self.cursor = idx;
                    self.follow_cursor(ctx);
                }
                Event::Finished { gen } if gen == self.gen && self.state == PlayState::Playing => {
                    self.finish(ctx);
                }
                Event::Error { gen, msg } if gen == self.gen => {
                    self.status = msg;
                    self.halt_engines();
                    self.state = PlayState::Stopped;
                    self.save_progress();
                }
                _ => {}
            }
        }
        self.system_tick(ctx);
    }

    fn system_tick(&mut self, ctx: &egui::Context) {
        if self.state != PlayState::Playing || self.use_online() {
            if let Some(s) = self.sys.as_mut() {
                s.drain();
            }
            return;
        }
        let mut ended = self.sys.as_mut().map(|s| s.poll_ended()).unwrap_or(false);
        if let Some(d) = self.fallback_deadline {
            let now = Instant::now();
            if now >= d {
                ended = true;
                self.fallback_deadline = None;
            } else {
                ctx.request_repaint_after(d - now);
            }
        }
        if ended {
            self.cursor += 1;
            self.speak_current(ctx);
        }
    }

    // ───────────────────────── interfaz ─────────────────────────

    fn menu_ui(&mut self, ctx: &egui::Context) {
        let mut act: Option<Action> = None;
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Archivo", |ui| {
                    if ui.button("Abrir EPUB…").clicked() {
                        self.want_open = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Salir").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                });
                ui.menu_button("Lectura", |ui| {
                    let has = self.book.is_some() && self.pending_resume.is_none();
                    if ui.add_enabled(has, egui::Button::new("Ir al inicio del libro")).clicked() {
                        act = Some(Action::GoStart);
                        ui.close_menu();
                    }
                    let saved = has && self.store.progress.contains_key(&self.book_key);
                    if ui
                        .add_enabled(saved, egui::Button::new("Ir a la última posición guardada"))
                        .clicked()
                    {
                        act = Some(Action::GoSaved);
                        ui.close_menu();
                    }
                });
                ui.menu_button("Ayuda", |ui| {
                    ui.label("Espacio: reproducir / pausa");
                    ui.label("Flechas ← →: cambiar de página");
                    ui.label("Clic en los bordes de la página: pasar de página");
                    ui.label("Arrastra un archivo .epub a la ventana para abrirlo");
                });
            });
        });
        if let Some(a) = act {
            self.apply_action(ctx, a);
        }
    }

    fn controls_ui(&mut self, ctx: &egui::Context) {
        let mut act: Option<Action> = None;
        let mut new_voice: Option<String> = None;
        let mut rate = self.store.rate;
        let mut vol = self.store.volume;
        let mut font = self.store.font_size;
        let mut rate_done = false;
        let state = self.state;
        let enabled = self.book.is_some() && self.pending_resume.is_none();
        let sel_label = self.voice_label(&self.store.voice);
        let sys_voices = self.sys_voices.clone();
        let current_voice = self.store.voice.clone();

        egui::TopBottomPanel::top("controles").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let big = |t: &str| {
                    egui::Button::new(RichText::new(t).size(16.0)).min_size(vec2(100.0, 32.0))
                };
                ui.add_enabled_ui(enabled, |ui| {
                    if ui.add(big("« Anterior")).clicked() {
                        act = Some(Action::Prev);
                    }
                    if ui.add_enabled(state != PlayState::Playing, big("Reproducir")).clicked() {
                        act = Some(Action::Play);
                    }
                    if ui.add_enabled(state == PlayState::Playing, big("Pausa")).clicked() {
                        act = Some(Action::Pause);
                    }
                    if ui.add_enabled(state != PlayState::Stopped, big("Parar")).clicked() {
                        act = Some(Action::Stop);
                    }
                    if ui.add(big("Siguiente »")).clicked() {
                        act = Some(Action::Next);
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Voz:");
                egui::ComboBox::from_id_salt("voz")
                    .width(300.0)
                    .selected_text(sel_label)
                    .show_ui(ui, |ui| {
                        if !sys_voices.is_empty() {
                            ui.label(RichText::new("Voces instaladas en el sistema").strong());
                            for v in &sys_voices {
                                let key = format!("sys:{}", v.id);
                                if ui.selectable_label(current_voice == key, &v.label).clicked() {
                                    new_voice = Some(key);
                                }
                            }
                            ui.separator();
                        }
                        ui.label(RichText::new("Voces en línea (requieren internet)").strong());
                        for (name, label) in ONLINE_VOICES {
                            let key = format!("web:{name}");
                            if ui.selectable_label(current_voice == key, *label).clicked() {
                                new_voice = Some(key);
                            }
                        }
                    });
                ui.separator();
                let r = ui.add(
                    egui::Slider::new(&mut rate, 0.5..=2.0)
                        .text("Velocidad")
                        .step_by(0.05)
                        .fixed_decimals(2),
                );
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    rate_done = true;
                }
                ui.separator();
                let v = ui.add(egui::Slider::new(&mut vol, 0.0..=1.0).text("Volumen").show_value(false));
                if v.changed() {
                    self.online.send(Cmd::Volume(vol));
                }
                ui.separator();
                ui.label("Letra:");
                if ui.button("A−").clicked() {
                    font = (font - 1.0).max(12.0);
                }
                if ui.button("A+").clicked() {
                    font = (font + 1.0).min(40.0);
                }
            });
            ui.add_space(4.0);
        });

        self.store.volume = vol;
        if (font - self.store.font_size).abs() > f32::EPSILON {
            self.store.font_size = font;
            self.end_cache = None;
            self.store.save();
        }
        if rate_done && (rate - self.store.rate).abs() > f32::EPSILON {
            self.store.rate = rate;
            self.store.save();
            if self.state == PlayState::Playing {
                let c = self.cursor;
                self.start_reading(ctx, c);
            }
        } else {
            self.store.rate = rate;
        }
        if let Some(k) = new_voice {
            self.set_voice(ctx, k);
        }
        if let Some(a) = act {
            self.apply_action(ctx, a);
        }
    }

    fn status_ui(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("estado").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(b) = &self.book {
                    let n = b.sentences.len().max(1);
                    let pct = self.cursor as f32 / n as f32 * 100.0;
                    ui.label(RichText::new(&b.title).strong());
                    ui.separator();
                    ui.label(format!("Avance: {pct:.1} %"));
                    ui.separator();
                    ui.label(match self.state {
                        PlayState::Playing => "Leyendo…",
                        PlayState::Paused => "En pausa",
                        PlayState::Stopped => "Detenido",
                    });
                } else {
                    ui.label("Ningún libro abierto");
                }
                if !self.status.is_empty() {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(255, 150, 130), &self.status);
                } else if let Some(e) = &self.sys_err {
                    ui.separator();
                    ui.colored_label(
                        Color32::from_rgb(255, 200, 120),
                        format!("Voces del sistema no disponibles ({e}). Usa las voces en línea."),
                    );
                }
            });
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_layer(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        page_rect: Rect,
        text_pos: egui::Pos2,
        w: f32,
        start: usize,
        end: usize,
        hl: Option<(usize, Color32)>,
    ) {
        painter.rect_filled(page_rect, 4.0, PAPER);
        let job = self.build_job(start, end, hl, w);
        let galley = ctx.fonts(|f| f.layout_job(job));
        painter.galley(text_pos, galley, INK);
        let n = self.texts.len().max(1);
        painter.text(
            pos2(page_rect.center().x, page_rect.bottom() - 24.0),
            Align2::CENTER_CENTER,
            format!("{:.0} %", start as f32 / n as f32 * 100.0),
            FontId::proportional(13.0),
            Color32::from_gray(130),
        );
    }

    fn page_ui(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(44, 40, 38)).inner_margin(12.0))
            .show(ctx, |ui| {
                if self.book.is_none() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.35);
                        ui.label(RichText::new("Lector EPUB").size(34.0).strong());
                        ui.add_space(8.0);
                        ui.label("Abre un libro desde Archivo ▸ Abrir EPUB… o arrastra un archivo .epub aquí.");
                        ui.add_space(12.0);
                        if ui.add(egui::Button::new(RichText::new("Abrir EPUB…").size(18.0))).clicked() {
                            self.want_open = true;
                        }
                    });
                    return;
                }

                let avail = ui.available_rect_before_wrap();
                let page_rect = Rect::from_center_size(avail.center(), vec2(avail.width().min(860.0), avail.height()));
                let text_rect = Rect::from_min_max(
                    page_rect.min + vec2(48.0, 34.0),
                    page_rect.max - vec2(48.0, 50.0),
                );
                let (w, h) = (text_rect.width().max(100.0), text_rect.height().max(60.0));
                self.layout = (w, h);

                let n = self.texts.len();
                self.page_start = self.page_start.min(n.saturating_sub(1));
                let end = self.page_end(ctx, self.page_start, w, h);
                self.cur_end = end;
                if self.state == PlayState::Playing {
                    self.follow_cursor(ctx);
                }

                let hl_color = if self.state == PlayState::Playing { HL_PLAY } else { HL_IDLE };
                let hl = Some((self.cursor, hl_color));
                let now = ctx.input(|i| i.time);
                let anim = self.anim.clone().filter(|a| now - a.started < ANIM_SECS);
                if anim.is_none() {
                    self.anim = None;
                }

                let painter = ui.painter_at(page_rect.expand(10.0));
                painter.rect_filled(page_rect.translate(vec2(5.0, 6.0)), 6.0, Color32::from_black_alpha(90));
                let page_painter = painter.with_clip_rect(page_rect);
                let text_pos = text_rect.min;

                if let Some(a) = anim {
                    // Efecto de pasar página: el borde de la hoja avanza con sombra.
                    let t = ((now - a.started) / ANIM_SECS) as f32;
                    let e = t * t * (3.0 - 2.0 * t);
                    let old = (a.old_start, a.old_end, None);
                    let new = (self.page_start, end, hl);
                    let (top, bottom) = if a.dir > 0.0 { (old, new) } else { (new, old) };

                    self.paint_layer(ctx, &page_painter, page_rect, text_pos, w, bottom.0, bottom.1, bottom.2);

                    let edge = if a.dir > 0.0 {
                        page_rect.right() - e * page_rect.width()
                    } else {
                        page_rect.left() + e * page_rect.width()
                    };
                    let clip = Rect::from_min_max(page_rect.min, pos2(edge, page_rect.max.y));
                    let top_painter = painter.with_clip_rect(clip.intersect(page_rect));
                    self.paint_layer(ctx, &top_painter, page_rect, text_pos, w, top.0, top.1, top.2);

                    for k in 0..16 {
                        let x = edge + k as f32 * 3.0;
                        let f = 1.0 - k as f32 / 16.0;
                        let strip = Rect::from_min_max(pos2(x, page_rect.top()), pos2(x + 3.0, page_rect.bottom()));
                        page_painter.rect_filled(strip, 0.0, Color32::from_black_alpha((90.0 * f * f) as u8));
                    }
                    page_painter.line_segment(
                        [pos2(edge, page_rect.top()), pos2(edge, page_rect.bottom())],
                        egui::Stroke::new(1.0_f32, Color32::from_black_alpha(80)),
                    );
                    ctx.request_repaint();
                } else {
                    self.paint_layer(ctx, &page_painter, page_rect, text_pos, w, self.page_start, end, hl);
                }

                // Clic en los bordes de la página = pasar de página.
                let resp = ui.interact(page_rect, ui.id().with("pagina"), egui::Sense::click());
                if resp.clicked() {
                    if let Some(p) = resp.interact_pointer_pos() {
                        let rel = (p.x - page_rect.left()) / page_rect.width();
                        if rel < 0.18 {
                            self.prev_page(ctx);
                        } else if rel > 0.82 {
                            self.next_page(ctx);
                        }
                    }
                }
            });
    }

    fn resume_dialog(&mut self, ctx: &egui::Context) {
        let Some(pos) = self.pending_resume else { return };
        let n = self.texts.len().max(1);
        let pct = pos as f32 / n as f32 * 100.0;
        let snippet: String = self
            .texts
            .get(pos)
            .map(|t| t.chars().take(90).collect())
            .unwrap_or_default();
        let mut choice: Option<bool> = None;

        egui::Window::new("Retomar lectura")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.label(format!("Este libro tiene una posición guardada ({pct:.0} % del libro)."));
                if !snippet.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("«…{snippet}…»")).italics());
                }
                ui.add_space(6.0);
                ui.label("¿Quieres continuar desde donde lo dejaste o empezar de nuevo desde el principio?");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Continuar donde lo dejé").clicked() {
                        choice = Some(true);
                    }
                    if ui.button("Empezar desde el principio").clicked() {
                        choice = Some(false);
                    }
                });
            });

        match choice {
            Some(true) => {
                self.pending_resume = None;
                self.cursor = pos;
                self.page_start = pos;
                self.anim = None;
                self.end_cache = None;
            }
            Some(false) => {
                self.pending_resume = None;
                self.cursor = 0;
                self.page_start = 0;
                self.anim = None;
                self.end_cache = None;
                self.store.progress.remove(&self.book_key);
                self.store.save();
            }
            None => {}
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.pending_resume.is_some() || self.book.is_none() || ctx.wants_keyboard_input() {
            return;
        }
        let (left, right, space) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Space),
            )
        });
        if left {
            self.apply_action(ctx, Action::Prev);
        }
        if right {
            self.apply_action(ctx, Action::Next);
        }
        if space {
            let a = if self.state == PlayState::Playing { Action::Pause } else { Action::Play };
            self.apply_action(ctx, a);
        }
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Abrir libro EPUB")
            .add_filter("Libros EPUB", &["epub"])
            .pick_file()
        {
            self.open_book(ctx, path);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events(ctx);

        // Arrastrar y soltar un .epub
        let dropped: Option<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .find(|p| p.extension().map(|e| e.eq_ignore_ascii_case("epub")).unwrap_or(false))
        });
        if let Some(p) = dropped {
            self.open_book(ctx, p);
        }

        self.handle_keys(ctx);
        self.menu_ui(ctx);
        self.controls_ui(ctx);
        self.status_ui(ctx);
        self.page_ui(ctx);
        self.resume_dialog(ctx);

        if self.want_open {
            self.want_open = false;
            self.pick_file(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.halt_engines();
        self.save_progress();
        self.store.save();
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Lector EPUB")
            .with_inner_size([1040.0, 780.0])
            .with_min_inner_size([680.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Lector EPUB",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
