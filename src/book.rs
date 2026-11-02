//! Carga de un EPUB y conversión a una lista plana de frases.
//!
//! Toda la aplicación trabaja con el "índice de frase" como posición:
//! es independiente del tamaño de la ventana o de la letra, así que la
//! posición guardada sigue siendo válida aunque cambie la paginación.

use scraper::{ElementRef, Html, Selector};
use std::path::Path;

pub struct Sentence {
    pub text: String,
    /// Es la primera frase de un párrafo (se dibuja con sangría).
    pub new_para: bool,
    /// Es un título (h1..h6).
    pub heading: bool,
}

pub struct Book {
    pub title: String,
    pub sentences: Vec<Sentence>,
}

const BLOCKS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6", "p", "li"];

impl Book {
    pub fn open(path: &Path) -> Result<Book, String> {
        let mut doc = epub::doc::EpubDoc::new(path)
            .map_err(|e| format!("No se pudo abrir el EPUB: {e}"))?;

        let title = doc
            .mdata("title")
            .map(|m| m.value.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Libro".to_string())
            });

        let mut sentences = Vec::new();
        loop {
            if let Some((html, _mime)) = doc.get_current_str() {
                add_chapter(&html, &mut sentences);
            }
            if !doc.go_next() {
                break;
            }
        }

        if sentences.is_empty() {
            return Err("El libro no contiene texto legible.".to_string());
        }
        Ok(Book { title, sentences })
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn add_chapter(html: &str, out: &mut Vec<Sentence>) {
    let html = html
        .replace("<br/>", " ")
        .replace("<br />", " ")
        .replace("<br>", " ");
    let dom = Html::parse_document(&html);
    let sel = Selector::parse("h1, h2, h3, h4, h5, h6, p, li").unwrap();

    let mut blocks: Vec<(String, bool)> = Vec::new();
    for el in dom.select(&sel) {
        // Evitar duplicados cuando un bloque está dentro de otro (li > p).
        let nested = el
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|a| BLOCKS.contains(&a.value().name()));
        if nested {
            continue;
        }
        let text = norm(&el.text().collect::<String>());
        if !text.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }
        let name = el.value().name();
        let heading = name.len() == 2 && name.starts_with('h');
        blocks.push((text, heading));
    }

    // Plan B: EPUB sin etiquetas de párrafo (solo <div> con texto).
    if blocks.is_empty() {
        let body = Selector::parse("body").unwrap();
        if let Some(b) = dom.select(&body).next() {
            let raw = b.text().collect::<Vec<_>>().join("\n");
            for line in raw.lines() {
                let t = norm(line);
                if t.chars().any(|c| c.is_alphanumeric()) {
                    blocks.push((t, false));
                }
            }
        }
    }

    for (text, heading) in blocks {
        if heading {
            out.push(Sentence { text, new_para: true, heading: true });
        } else {
            for (i, s) in split_sentences(&text).into_iter().enumerate() {
                out.push(Sentence { text: s, new_para: i == 0, heading: false });
            }
        }
    }
}

/// Divide un párrafo en frases (con cuidado con abreviaturas españolas).
pub fn split_sentences(p: &str) -> Vec<String> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        cur.push(c);
        if matches!(c, '.' | '!' | '?' | '…') {
            while i + 1 < chars.len()
                && matches!(chars[i + 1], '.' | '!' | '?' | '…' | '"' | '”' | '»' | '’' | ')' | '\'')
            {
                i += 1;
                cur.push(chars[i]);
            }
            let at_end = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
            if at_end && !is_abbreviation(&cur) {
                push_chunks(&mut out, &cur);
                cur.clear();
            }
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        push_chunks(&mut out, &cur);
    }
    out
}

fn is_abbreviation(s: &str) -> bool {
    let t = s.trim_end();
    if !t.ends_with('.') || t.ends_with("..") {
        return false;
    }
    let last = t.rsplit(char::is_whitespace).next().unwrap_or("");
    let w = last
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    const ABBR: &[&str] = &[
        "sr.", "sra.", "srta.", "dr.", "dra.", "lic.", "ing.", "prof.", "pág.", "págs.",
        "etc.", "ud.", "uds.", "av.", "núm.", "vs.", "cap.", "art.", "tel.", "dpto.",
        "sto.", "sta.", "gral.", "mr.", "mrs.", "ms.",
    ];
    if ABBR.contains(&w.as_str()) {
        return true;
    }
    // Inicial de nombre: "J."
    let letters: Vec<char> = w.chars().collect();
    letters.len() == 2 && letters[0].is_alphabetic()
}

/// Parte frases larguísimas en trozos manejables para las voces.
fn push_chunks(out: &mut Vec<String>, s: &str) {
    const MAX: usize = 320;
    let s = s.trim();
    if s.is_empty() {
        return;
    }
    let mut rest: Vec<char> = s.chars().collect();
    while rest.len() > MAX {
        let window = &rest[..MAX];
        let cut = window
            .iter()
            .rposition(|c| matches!(c, ',' | ';' | ':'))
            .or_else(|| window.iter().rposition(|c| c.is_whitespace()))
            .unwrap_or(MAX - 1);
        let head: String = rest[..=cut].iter().collect();
        let head = head.trim().to_string();
        if !head.is_empty() {
            out.push(head);
        }
        rest = rest[cut + 1..].to_vec();
    }
    let tail: String = rest.iter().collect();
    let tail = tail.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
}
