//! Guarda ajustes y la última posición de cada libro en un JSON
//! dentro de la carpeta de configuración del usuario.

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Serialize, Deserialize, Clone)]
pub struct Progress {
    /// Índice de la frase donde se quedó la lectura.
    pub sentence: usize,
    pub total: usize,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Store {
    pub last_book: Option<String>,
    /// "sys:<id>" para voces del sistema, "web:<nombre>" para voces en línea.
    pub voice: String,
    pub rate: f32,
    pub volume: f32,
    pub font_size: f32,
    pub progress: HashMap<String, Progress>,
}

impl Default for Store {
    fn default() -> Self {
        Store {
            last_book: None,
            voice: String::new(),
            rate: 1.0,
            volume: 1.0,
            font_size: 20.0,
            progress: HashMap::new(),
        }
    }
}

impl Store {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("lector_epub").join("estado.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            // Escritura atómica: primero a un temporal y luego se renombra.
            let tmp = p.with_extension("tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, &p);
            }
        }
    }
}
