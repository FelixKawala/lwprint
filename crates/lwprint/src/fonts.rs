//! Font loading and other small helpers shared by the CLI and the service.

use ab_glyph::{FontArc, FontVec};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use std::path::Path;

/// Decode a `data:` URL (e.g. `data:image/png;base64,....`) into bytes.
pub fn decode_data_url(url: &str) -> Option<Result<Vec<u8>, String>> {
    let rest = url.strip_prefix("data:")?;
    let Some((meta, data)) = rest.split_once(',') else {
        return Some(Err("malformed data URL".into()));
    };
    Some(if meta.ends_with(";base64") {
        base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|e| format!("invalid base64 in data URL: {e}"))
    } else {
        Ok(data.as_bytes().to_vec())
    })
}

/// Load a font from a file path, or find an installed font by family name.
pub fn load_font(name: Option<&str>) -> Result<FontArc> {
    if let Some(n) = name {
        if Path::new(n).is_file() {
            let data = std::fs::read(n).with_context(|| format!("could not read font {n}"))?;
            let font = FontVec::try_from_vec(data).map_err(|_| anyhow!("{n} is not a usable font"))?;
            return Ok(FontArc::new(font));
        }
    }
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let families: Vec<fontdb::Family> = match name {
        Some(n) => vec![fontdb::Family::Name(n)],
        None => vec![
            fontdb::Family::Name("DejaVu Sans"),
            fontdb::Family::Name("Liberation Sans"),
            fontdb::Family::Name("Noto Sans"),
            fontdb::Family::SansSerif,
        ],
    };
    let id = db
        .query(&fontdb::Query { families: &families, ..Default::default() })
        .ok_or_else(|| match name {
            Some(n) => anyhow!("font '{n}' not found"),
            None => anyhow!("no font found; install one (e.g. fonts-dejavu-core) or use --font"),
        })?;
    db.with_face_data(id, |data, index| FontVec::try_from_vec_and_index(data.to_vec(), index))
        .ok_or_else(|| anyhow!("could not load font"))?
        .map(FontArc::new)
        .map_err(|_| anyhow!("could not parse font"))
}
