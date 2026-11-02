//! Voces neuronales en línea (servicio de lectura de Microsoft Edge).
//!
//! Un hilo de trabajo recibe órdenes, sintetiza las frases por adelantado
//! (hasta 3 frases de búfer, para que no haya silencios entre frases) y las
//! reproduce con `rodio`. Avisa a la interfaz cuando empieza cada frase, de
//! modo que el resaltado sigue exactamente al audio.

use msedge_tts::tts::client::connect;
use msedge_tts::tts::SpeechConfig;
use rodio::{Decoder, OutputStream, Sink};
use std::io::Cursor;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

/// (nombre técnico, etiqueta para mostrar)
pub const ONLINE_VOICES: &[(&str, &str)] = &[
    ("es-ES-ElviraNeural", "Elvira · España (mujer)"),
    ("es-ES-AlvaroNeural", "Álvaro · España (hombre)"),
    ("es-CO-SalomeNeural", "Salomé · Colombia (mujer)"),
    ("es-CO-GonzaloNeural", "Gonzalo · Colombia (hombre)"),
    ("es-MX-DaliaNeural", "Dalia · México (mujer)"),
    ("es-MX-JorgeNeural", "Jorge · México (hombre)"),
    ("es-AR-ElenaNeural", "Elena · Argentina (mujer)"),
    ("es-AR-TomasNeural", "Tomás · Argentina (hombre)"),
    ("es-CL-CatalinaNeural", "Catalina · Chile (mujer)"),
    ("es-CL-LorenzoNeural", "Lorenzo · Chile (hombre)"),
    ("es-PE-CamilaNeural", "Camila · Perú (mujer)"),
    ("es-PE-AlexNeural", "Alex · Perú (hombre)"),
    ("es-VE-PaolaNeural", "Paola · Venezuela (mujer)"),
    ("es-VE-SebastianNeural", "Sebastián · Venezuela (hombre)"),
    ("es-US-PalomaNeural", "Paloma · EE. UU. (mujer)"),
    ("es-US-AlonsoNeural", "Alonso · EE. UU. (hombre)"),
];

pub enum Cmd {
    Start {
        gen: u64,
        from: usize,
        texts: Arc<Vec<String>>,
        voice: String,
        /// Porcentaje de velocidad respecto a la normal (-50..=100).
        rate: i32,
    },
    Pause,
    Resume,
    Stop,
    Volume(f32),
}

pub enum Event {
    /// Empezó a sonar la frase `idx`.
    Started { gen: u64, idx: usize },
    /// Se leyeron todas las frases.
    Finished { gen: u64 },
    Error { gen: u64, msg: String },
}

pub struct OnlineEngine {
    tx: Sender<Cmd>,
    pub rx: Receiver<Event>,
}

impl OnlineEngine {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (tx_cmd, rx_cmd) = channel::<Cmd>();
        let (tx_ev, rx_ev) = channel::<Event>();
        std::thread::spawn(move || worker(rx_cmd, tx_ev, ctx));
        OnlineEngine { tx: tx_cmd, rx: rx_ev }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

type Chunk = Result<(usize, Vec<u8>), String>;

struct Session {
    gen: u64,
    brx: Receiver<Chunk>,
}

fn worker(rx: Receiver<Cmd>, tx: Sender<Event>, ctx: eframe::egui::Context) {
    let mut output: Option<(OutputStream, rodio::OutputStreamHandle)> = None;
    let mut sink: Option<Sink> = None;
    let mut session: Option<Session> = None;
    let mut volume = 1.0f32;

    let notify = |ev: Event| {
        let _ = tx.send(ev);
        ctx.request_repaint();
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(15)) {
            Ok(Cmd::Start { gen, from, texts, voice, rate }) => {
                session = None;
                if let Some(s) = sink.take() {
                    s.stop();
                }
                if output.is_none() {
                    match OutputStream::try_default() {
                        Ok(o) => output = Some(o),
                        Err(e) => {
                            notify(Event::Error {
                                gen,
                                msg: format!("No se pudo abrir la salida de audio: {e}"),
                            });
                            continue;
                        }
                    }
                }
                let handle = &output.as_ref().unwrap().1;
                match Sink::try_new(handle) {
                    Ok(s) => {
                        s.set_volume(volume);
                        sink = Some(s);
                    }
                    Err(e) => {
                        notify(Event::Error { gen, msg: format!("Error de audio: {e}") });
                        continue;
                    }
                }
                let (btx, brx) = sync_channel::<Chunk>(3);
                spawn_synth(from, texts, voice, rate, btx);
                session = Some(Session { gen, brx });
            }
            Ok(Cmd::Pause) => {
                if let Some(s) = &sink {
                    s.pause();
                }
            }
            Ok(Cmd::Resume) => {
                if let Some(s) = &sink {
                    s.play();
                }
            }
            Ok(Cmd::Stop) => {
                session = None;
                if let Some(s) = sink.take() {
                    s.stop();
                }
            }
            Ok(Cmd::Volume(v)) => {
                volume = v;
                if let Some(s) = &sink {
                    s.set_volume(v);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Cuando la frase anterior terminó, se encola la siguiente.
        let mut next: Option<Chunk> = None;
        let mut finished = false;
        if let (Some(sess), Some(sk)) = (&session, &sink) {
            if sk.empty() {
                match sess.brx.try_recv() {
                    Ok(m) => next = Some(m),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => finished = true,
                }
            }
        }
        let gen = session.as_ref().map(|s| s.gen).unwrap_or(0);

        if let Some(m) = next {
            match m {
                Ok((idx, bytes)) => match Decoder::new(Cursor::new(bytes)) {
                    Ok(dec) => {
                        if let Some(sk) = &sink {
                            sk.append(dec);
                        }
                        notify(Event::Started { gen, idx });
                    }
                    Err(e) => {
                        notify(Event::Error { gen, msg: format!("Audio no válido: {e}") });
                        session = None;
                    }
                },
                Err(msg) => {
                    notify(Event::Error { gen, msg });
                    session = None;
                }
            }
        } else if finished {
            notify(Event::Finished { gen });
            session = None;
        }
    }
}

/// Hilo que sintetiza las frases en orden y las deja en un búfer pequeño.
/// Termina solo cuando el receptor se descarta (parar / nueva sesión).
fn spawn_synth(from: usize, texts: Arc<Vec<String>>, voice: String, rate: i32, btx: std::sync::mpsc::SyncSender<Chunk>) {
    std::thread::spawn(move || {
        let cfg = SpeechConfig {
            voice_name: voice,
            audio_format: "audio-24khz-48kbitrate-mono-mp3".to_string(),
            pitch: 0,
            rate,
            volume: 0,
        };
        let mut client = None;
        for idx in from..texts.len() {
            let text = texts[idx].trim();
            if !text.chars().any(|c| c.is_alphanumeric()) {
                continue;
            }
            let mut last_err = String::new();
            let mut audio: Option<Vec<u8>> = None;
            for _ in 0..3 {
                if client.is_none() {
                    match connect() {
                        Ok(c) => client = Some(c),
                        Err(e) => {
                            last_err = e.to_string();
                            std::thread::sleep(Duration::from_millis(300));
                            continue;
                        }
                    }
                }
                match client.as_mut().unwrap().synthesize(text, &cfg) {
                    Ok(a) => {
                        audio = Some(a.audio_bytes);
                        break;
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        client = None; // se reconecta en el siguiente intento
                    }
                }
            }
            match audio {
                Some(bytes) => {
                    if btx.send(Ok((idx, bytes))).is_err() {
                        return;
                    }
                }
                None => {
                    let _ = btx.send(Err(format!(
                        "La voz en línea no está disponible (¿hay conexión a internet?): {last_err}"
                    )));
                    return;
                }
            }
        }
    });
}
