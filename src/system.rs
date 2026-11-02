//! Voces instaladas en el sistema (SAPI/WinRT en Windows, AVFoundation en
//! macOS, speech-dispatcher en Linux) mediante la crate `tts`.

use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use tts::{Tts, UtteranceId, Voice};

#[derive(Clone)]
pub struct VoiceInfo {
    pub id: String,
    pub label: String,
    pub spanish: bool,
}

pub struct SystemEngine {
    tts: Tts,
    voices: Vec<Voice>,
    current: Option<UtteranceId>,
    rx: Receiver<UtteranceId>,
    callbacks: bool,
}

impl SystemEngine {
    pub fn new(ctx: &eframe::egui::Context) -> Result<Self, String> {
        #[allow(unused_mut)] // en Windows no hace falta, en Linux/macOS sí
        let mut tts = Tts::default().map_err(|e| e.to_string())?;
        let features = tts.supported_features();
        let (tx, rx) = channel::<UtteranceId>();
        let callbacks = features.utterance_callbacks;
        if callbacks {
            let ctx = ctx.clone();
            let _ = tts.on_utterance_end(Some(Box::new(move |id| {
                let _ = tx.send(id);
                ctx.request_repaint();
            })));
        }
        let mut voices = if features.voice {
            tts.voices().unwrap_or_default()
        } else {
            Vec::new()
        };
        // Primero las voces en español.
        voices.sort_by_key(|v| !is_spanish(&v.language().to_string()));
        Ok(SystemEngine { tts, voices, current: None, rx, callbacks })
    }

    pub fn voice_list(&self) -> Vec<VoiceInfo> {
        self.voices
            .iter()
            .map(|v| {
                let lang = v.language().to_string();
                VoiceInfo {
                    id: v.id(),
                    label: format!("{} ({})", v.name(), lang),
                    spanish: is_spanish(&lang),
                }
            })
            .collect()
    }

    pub fn set_voice(&mut self, id: &str) -> Result<(), String> {
        if let Some(v) = self.voices.iter().find(|v| v.id() == id).cloned() {
            self.tts.set_voice(&v).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Habla un texto. Devuelve `Some(duración estimada)` si el motor no
    /// avisa del final de la locución (entonces la app usa un temporizador).
    pub fn speak(&mut self, text: &str, rate: f32, volume: f32) -> Result<Option<Duration>, String> {
        let f = self.tts.supported_features();
        if f.rate {
            let (min, max, normal) = (self.tts.min_rate(), self.tts.max_rate(), self.tts.normal_rate());
            let r = if rate >= 1.0 {
                normal + (rate - 1.0) * (max - normal) * 0.4
            } else {
                min + ((rate - 0.5) / 0.5) * (normal - min)
            };
            let _ = self.tts.set_rate(r.clamp(min, max));
        }
        if f.volume {
            let (min, max) = (self.tts.min_volume(), self.tts.max_volume());
            let _ = self.tts.set_volume(min + volume.clamp(0.0, 1.0) * (max - min));
        }
        let id = self.tts.speak(text, true).map_err(|e| e.to_string())?;
        self.current = id.clone();
        if self.callbacks && id.is_some() {
            Ok(None)
        } else {
            let secs = text.chars().count() as f32 / (14.0 * rate.max(0.3)) + 0.4;
            Ok(Some(Duration::from_secs_f32(secs)))
        }
    }

    pub fn stop(&mut self) {
        self.current = None;
        let _ = self.tts.stop();
        while self.rx.try_recv().is_ok() {}
    }

    /// `true` si terminó la locución actual (ignora finales de locuciones viejas).
    pub fn poll_ended(&mut self) -> bool {
        let mut ended = false;
        while let Ok(id) = self.rx.try_recv() {
            if Some(&id) == self.current.as_ref() {
                ended = true;
                self.current = None;
            }
        }
        ended
    }

    pub fn drain(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }
}

fn is_spanish(lang: &str) -> bool {
    let l = lang.to_lowercase();
    l.starts_with("es") || l.starts_with("spa")
}
