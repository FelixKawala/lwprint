//! Turns a label description into a bitmap. Elements are placed left to right,
//! each centred across the tape.

use crate::bitmap::Bitmap;
use crate::model::{Model, Tape};
use ab_glyph::{point, Font, FontArc, GlyphId, PxScale, ScaleFont};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("the label has no content")]
    Empty,
    #[error("text needs a font, but none was given")]
    NoFont,
    #[error("icons are not available")]
    NoIcons,
    #[error("unknown icon '{0}'")]
    UnknownIcon(String),
    #[error("QR code: {0}")]
    Qr(String),
    #[error("barcode: {0}")]
    Barcode(String),
    #[error("image '{0}': {1}")]
    Image(String, String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Symbology {
    #[default]
    Code128,
    Ean13,
    Code39,
}

fn default_module() -> usize {
    2
}

/// Horizontal alignment of text lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HAlign {
    Left,
    #[default]
    Center,
    Right,
}

/// Vertical position of an element that is smaller than the tape height.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VAlign {
    Top,
    #[default]
    Center,
    Bottom,
}

/// Frame drawn around the content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Border {
    #[default]
    None,
    Line,
    Double,
    Rounded,
    Dashed,
}

/// One piece of label content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Element {
    /// Text; `\n` starts a new line. Without `size_mm` (line height), lines share the height.
    Text {
        text: String,
        #[serde(default)]
        size_mm: Option<f32>,
        #[serde(default)]
        align: HAlign,
    },
    /// QR code, as large as fits unless `size_mm` is given.
    Qr {
        data: String,
        #[serde(default)]
        size_mm: Option<f32>,
        /// Fill the available height with squares of slightly uneven size (mixing whole
        /// pixel widths) instead of equal squares; larger on average, but scanners must
        /// cope with the unevenness. The tape edge serves as the white border.
        #[serde(default)]
        fill: bool,
    },
    /// Icon from the icon font by name (Material Design Icons, e.g. "screwdriver" or
    /// "mdi:screwdriver"). Fills the height unless `size_mm` is given.
    Icon {
        name: String,
        #[serde(default)]
        size_mm: Option<f32>,
    },
    /// 1D barcode, full height unless `height_mm` is given.
    Barcode {
        data: String,
        #[serde(default)]
        symbology: Symbology,
        /// Width of the narrowest bar in pixels (1 px = 0.14 mm).
        #[serde(default = "default_module")]
        module_px: usize,
        #[serde(default)]
        height_mm: Option<f32>,
    },
    /// Image scaled to the height (or `height_mm`). `path` is resolved by the caller.
    Image {
        path: String,
        #[serde(default)]
        dither: bool,
        #[serde(default)]
        height_mm: Option<f32>,
    },
    /// Empty space.
    Space { mm: f32 },
}

impl Element {
    pub fn text(text: impl Into<String>) -> Self {
        Element::Text { text: text.into(), size_mm: None, align: HAlign::Center }
    }
}

/// An element plus its placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    #[serde(flatten)]
    pub element: Element,
    #[serde(default)]
    pub valign: VAlign,
}

impl From<Element> for Item {
    fn from(element: Element) -> Self {
        Item { element, valign: VAlign::Center }
    }
}

fn default_margin() -> f32 {
    2.0
}
fn default_gap() -> f32 {
    1.5
}

/// A complete label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelSpec {
    pub elements: Vec<Item>,
    /// Blank tape before and after the content.
    #[serde(default = "default_margin")]
    pub margin_mm: f32,
    /// Extra blank space at the top and bottom edge (the printable area already
    /// keeps the minimum edge the printer needs).
    #[serde(default)]
    pub padding_mm: f32,
    /// Space between elements.
    #[serde(default = "default_gap")]
    pub gap_mm: f32,
    /// Minimum label length; content is centred if the label is longer.
    #[serde(default)]
    pub min_length_mm: Option<f32>,
    #[serde(default)]
    pub border: Border,
}

impl LabelSpec {
    pub fn new(elements: Vec<Element>) -> Self {
        Self {
            elements: elements.into_iter().map(Item::from).collect(),
            margin_mm: default_margin(),
            padding_mm: 0.0,
            gap_mm: default_gap(),
            min_length_mm: None,
            border: Border::None,
        }
    }
}

/// An icon font plus the names of its icons.
pub struct IconFont {
    pub font: FontArc,
    pub glyphs: HashMap<String, char>,
}

impl IconFont {
    /// Look up an icon; accepts "name" or "mdi:name".
    pub fn get(&self, name: &str) -> Option<char> {
        let n = name.trim();
        let n = n.strip_prefix("mdi:").or_else(|| n.strip_prefix("mdi-")).unwrap_or(n);
        self.glyphs.get(&n.to_lowercase()).copied()
    }
}

/// Everything rendering needs from the outside world.
pub struct RenderContext<'a> {
    pub model: Model,
    pub tape: Tape,
    /// Fonts for text: the main font first, then fallbacks (e.g. emoji) for missing characters.
    pub fonts: &'a [FontArc],
    pub icons: Option<&'a IconFont>,
    /// Loads image bytes for an `Image` element's `path`.
    pub load_image: &'a dyn Fn(&str) -> Result<Vec<u8>, String>,
    /// If set, collects hints about the label (e.g. QR codes that may be hard to scan).
    pub notes: Option<&'a std::cell::RefCell<Vec<String>>>,
}

/// Border line thickness in pixels.
fn border_px(b: Border) -> usize {
    match b {
        Border::None => 0,
        Border::Double => 4,
        _ => 2,
    }
}

/// Render a label. The result is `tape.pixels` high (across the tape) and as long as needed.
pub fn render(spec: &LabelSpec, ctx: &RenderContext) -> Result<Bitmap, RenderError> {
    let height = ctx.tape.pixels;
    let pad = ctx.model.mm_to_px(spec.padding_mm.max(0.0));
    let bw = border_px(spec.border);
    // Space between border and content: across the tape and along it.
    let (inner_y, inner_x) = if bw > 0 { (bw + 3, bw + ctx.model.mm_to_px(1.0)) } else { (0, 0) };
    let content_h = height.saturating_sub(2 * (pad + inner_y)).max(8).min(height);
    let top = (height - content_h) / 2;

    let parts = spec
        .elements
        .iter()
        .map(|it| render_element(&it.element, content_h, ctx).map(|b| (b, it.valign)))
        .collect::<Result<Vec<_>, _>>()?;
    if parts.iter().all(|(p, _)| p.width() == 0) {
        return Err(RenderError::Empty);
    }
    let margin = ctx.model.mm_to_px(spec.margin_mm.max(0.0));
    let gap = ctx.model.mm_to_px(spec.gap_mm.max(0.0));
    let content: usize =
        parts.iter().map(|(p, _)| p.width()).sum::<usize>() + gap * parts.len().saturating_sub(1);
    let min_len = spec.min_length_mm.map(|mm| ctx.model.mm_to_px(mm)).unwrap_or(0);
    let total = (content + 2 * (margin + inner_x)).max(min_len);
    let mut out = Bitmap::new(total, height);
    let mut x = (total - content) / 2;
    for (p, valign) in &parts {
        let free = content_h.saturating_sub(p.height());
        let dy = match valign {
            VAlign::Top => 0,
            VAlign::Center => free / 2,
            VAlign::Bottom => free,
        };
        out.blit(p, x as isize, (top + dy) as isize);
        x += p.width() + gap;
    }
    if bw > 0 && total > 2 * margin + 2 * bw && height > 2 * pad + 2 * bw {
        draw_border(&mut out, spec.border, margin, pad, total - margin, height - pad, bw);
    }
    Ok(out)
}

/// Draw a frame inside the rectangle [x0, x1) × [y0, y1).
fn draw_border(out: &mut Bitmap, style: Border, x0: usize, y0: usize, x1: usize, y1: usize, bw: usize) {
    let (w, h) = ((x1 - x0) as f32, (y1 - y0) as f32);
    let radius = if style == Border::Rounded { (h.min(w) * 0.22).min(18.0) } else { 0.0 };
    let (cx, cy) = (x0 as f32 + w / 2.0, y0 as f32 + h / 2.0);
    let (hx, hy) = (w / 2.0 - radius, h / 2.0 - radius);
    for y in y0..y1 {
        for x in x0..x1 {
            // Signed distance to the (rounded) rectangle edge; negative inside.
            let (px, py) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let (qx, qy) = (px.abs() - hx, py.abs() - hy);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let d = -(outside + qx.max(qy).min(0.0) - radius);
            let on = match style {
                Border::Double => d < 1.0 || (d >= 3.0 && d < 4.0),
                Border::Dashed => d < bw as f32 && ((x + y) / 8) % 2 == 0,
                _ => d < bw as f32,
            };
            if on && d >= 0.0 {
                out.set(x, y, true);
            }
        }
    }
}

fn render_element(e: &Element, height: usize, ctx: &RenderContext) -> Result<Bitmap, RenderError> {
    let px = |mm: &Option<f32>| mm.map(|mm| ctx.model.mm_to_px(mm).clamp(1, height));
    match e {
        Element::Text { text, size_mm, align } => {
            if ctx.fonts.is_empty() {
                return Err(RenderError::NoFont);
            }
            Ok(render_text(text, ctx.fonts, height, px(size_mm), *align))
        }
        Element::Icon { name, size_mm } => {
            let icons = ctx.icons.ok_or(RenderError::NoIcons)?;
            let ch = icons.get(name).ok_or_else(|| RenderError::UnknownIcon(name.clone()))?;
            Ok(render_icon(&icons.font, ch, px(size_mm).unwrap_or(height)))
        }
        Element::Qr { data, size_mm, fill: true } => {
            let target = px(size_mm).unwrap_or(height);
            // The tape edge is a white border only if nothing (border, padding) is between.
            let edge_is_quiet = target >= ctx.tape.pixels;
            render_qr_fill(data, target, !edge_is_quiet)
        }
        Element::Qr { data, size_mm, fill: false } => {
            let qr = render_qr(data, px(size_mm).unwrap_or(height))?;
            if let Some(notes) = ctx.notes {
                let module = qr_module_px(data, px(size_mm).unwrap_or(height)).unwrap_or(0);
                if module <= 2 {
                    notes.borrow_mut().push(format!(
                        "QR code squares are only {:.2} mm, which some phones struggle to read. \
                         Up to about 17 characters of content, or wider tape, gives larger squares.",
                        module as f32 * 25.4 / 180.0
                    ));
                }
            }
            Ok(qr)
        }
        Element::Barcode { data, symbology, module_px, height_mm } => {
            render_barcode(data, *symbology, (*module_px).max(1), px(height_mm).unwrap_or(height))
        }
        Element::Image { path, dither, height_mm } => {
            let bytes = (ctx.load_image)(path).map_err(|e| RenderError::Image(path.clone(), e))?;
            render_image(&bytes, px(height_mm).unwrap_or(height), *dither)
                .map_err(|e| RenderError::Image(path.clone(), e))
        }
        Element::Space { mm } => Ok(Bitmap::new(ctx.model.mm_to_px(*mm), 1)),
    }
}

/// Characters that only modify how an emoji looks; skipped because there is no shaping.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{200D}' | '\u{20E3}') || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// Render text lines. Without `line_px`, all lines together fill `height`.
/// Characters missing from the first font are taken from the following fonts.
pub fn render_text(
    text: &str,
    fonts: &[FontArc],
    height: usize,
    line_px: Option<usize>,
    align: HAlign,
) -> Bitmap {
    let lines: Vec<&str> = text.split('\n').collect();
    let line_h = line_px.unwrap_or(height / lines.len().max(1)).clamp(1, height) as f32;
    // Per font: scale so that ascent + descent equals the line height.
    let scales: Vec<PxScale> = fonts
        .iter()
        .map(|f| {
            let probe = f.as_scaled(PxScale::from(line_h));
            let natural = probe.ascent() - probe.descent();
            PxScale::from(line_h * line_h / natural.max(1.0))
        })
        .collect();
    let pick = |c: char| -> (usize, GlyphId) {
        for (i, f) in fonts.iter().enumerate() {
            let id = f.glyph_id(c);
            if id.0 != 0 {
                return (i, id);
            }
        }
        (0, fonts[0].glyph_id(c))
    };

    // Lay out each line: (font index, glyph) pairs and the line width.
    let laid: Vec<(Vec<(usize, ab_glyph::Glyph)>, f32)> = lines
        .iter()
        .map(|line| {
            let mut caret = 0.0f32;
            let mut prev: Option<(usize, GlyphId)> = None;
            let mut glyphs = Vec::new();
            for c in line.chars().filter(|&c| !is_invisible(c)) {
                let (fi, id) = pick(c);
                let sf = fonts[fi].as_scaled(scales[fi]);
                if let Some((pf, pid)) = prev {
                    if pf == fi {
                        caret += sf.kern(pid, id);
                    }
                }
                glyphs.push((fi, id.with_scale_and_position(scales[fi], point(caret, 0.0))));
                caret += sf.h_advance(id);
                prev = Some((fi, id));
            }
            (glyphs, caret)
        })
        .collect();
    let width = laid.iter().map(|(_, w)| w.ceil() as usize).max().unwrap_or(0);
    let block_h = (line_h * lines.len() as f32).ceil() as usize;
    let mut out = Bitmap::new(width, block_h.min(height).max(1));
    for (i, (glyphs, w)) in laid.iter().enumerate() {
        let x_off = match align {
            HAlign::Left => 0.0,
            HAlign::Center => (width as f32 - w) / 2.0,
            HAlign::Right => width as f32 - w,
        };
        let line_top = i as f32 * line_h;
        for (fi, g) in glyphs {
            let sf = fonts[*fi].as_scaled(scales[*fi]);
            let mut g = g.clone();
            g.position = point(g.position.x + x_off, line_top + sf.ascent());
            if let Some(outline) = fonts[*fi].outline_glyph(g) {
                let b = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    if cov > 0.5 {
                        let x = b.min.x as isize + gx as isize;
                        let y = b.min.y as isize + gy as isize;
                        if x >= 0 && y >= 0 {
                            out.set(x as usize, y as usize, true);
                        }
                    }
                });
            }
        }
    }
    out
}

/// Render one icon glyph at a size where its em square is `px` high, centred in a
/// `px` × `px` bitmap.
pub fn render_icon(font: &FontArc, ch: char, px: usize) -> Bitmap {
    let units = font.units_per_em().unwrap_or(1000.0);
    // PxScale is relative to ascent - descent; convert so that one em equals `px`.
    let scale = PxScale::from(px as f32 * font.height_unscaled() / units);
    let mut out = Bitmap::new(px, px);
    let glyph = font.glyph_id(ch).with_scale_and_position(scale, point(0.0, 0.0));
    if let Some(outline) = font.outline_glyph(glyph) {
        let b = outline.px_bounds();
        let dx = ((px as f32 - b.width()) / 2.0).round() as isize;
        let dy = ((px as f32 - b.height()) / 2.0).round() as isize;
        outline.draw(|gx, gy, cov| {
            if cov > 0.5 {
                let (x, y) = (dx + gx as isize, dy + gy as isize);
                if x >= 0 && y >= 0 {
                    out.set(x as usize, y as usize, true);
                }
            }
        });
    }
    out
}

/// Render a QR code that fills `height` exactly, with squares of uneven whole-pixel
/// sizes. `quiet_v` keeps a one-square white border at top and bottom; horizontally
/// there is always a one-square border.
pub fn render_qr_fill(data: &str, height: usize, quiet_v: bool) -> Result<Bitmap, RenderError> {
    use qrcode::{Color, EcLevel, QrCode};
    let qv = if quiet_v { 1 } else { 0 };
    // The smaller code (fewer squares) gives larger squares; medium correction on a tie.
    let code = [EcLevel::M, EcLevel::L]
        .iter()
        .filter_map(|l| QrCode::with_error_correction_level(data.as_bytes(), *l).ok())
        .min_by_key(|c| c.width())
        .ok_or_else(|| RenderError::Qr("too much data for a QR code".into()))?;
    let w = code.width();
    let step = height as f32 / (w + 2 * qv) as f32;
    if step < 2.0 {
        return Err(RenderError::Qr(format!(
            "too much data for {} pixels ({:.1} mm); use shorter data or wider tape",
            height,
            height as f32 * 25.4 / 180.0
        )));
    }
    // Square i spans [edge(i), edge(i + 1)); edges are rounded to whole pixels.
    let edge = |i: usize| (i as f32 * step).round() as usize;
    let width = edge(w + 2);
    let colors = code.to_colors();
    let mut out = Bitmap::new(width, height);
    for my in 0..w {
        let (y0, y1) = (edge(my + qv), edge(my + qv + 1).min(height));
        for mx in 0..w {
            if colors[my * w + mx] == Color::Dark {
                for y in y0..y1 {
                    for x in edge(mx + 1)..edge(mx + 2) {
                        out.set(x, y, true);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Size of one QR square in pixels when `data` is rendered into `height` pixels.
pub fn qr_module_px(data: &str, height: usize) -> Option<usize> {
    use qrcode::{EcLevel, QrCode};
    [EcLevel::M, EcLevel::L]
        .iter()
        .filter_map(|l| QrCode::with_error_correction_level(data.as_bytes(), *l).ok())
        .map(|c| height / (c.width() + 2))
        .filter(|s| *s >= 2)
        .max()
}

/// Render a QR code with a 1-module quiet zone, as large as fits.
pub fn render_qr(data: &str, height: usize) -> Result<Bitmap, RenderError> {
    use qrcode::{Color, EcLevel, QrCode};
    // Use whichever error correction level gives the larger squares (medium on a tie):
    // on narrow tape the size of the squares matters more for scanning than redundancy.
    let mut best: Option<(QrCode, usize)> = None;
    for level in [EcLevel::M, EcLevel::L] {
        let Ok(code) = QrCode::with_error_correction_level(data.as_bytes(), level) else { continue };
        let scale = height / (code.width() + 2);
        if scale >= 2 && best.as_ref().is_none_or(|(_, s)| scale > *s) {
            best = Some((code, scale));
        }
    }
    let (code, scale) = best.ok_or_else(|| {
        RenderError::Qr(format!(
            "too much data for {} pixels ({:.1} mm); use shorter data, a larger size or wider tape",
            height,
            height as f32 * 25.4 / 180.0
        ))
    })?;
    let w = code.width();
    let colors = code.to_colors();
    let size = (w + 2) * scale;
    let mut out = Bitmap::new(size, size);
    for my in 0..w {
        for mx in 0..w {
            if colors[my * w + mx] == Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        out.set((mx + 1) * scale + dx, (my + 1) * scale + dy, true);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Render a 1D barcode over the full height, with a quiet zone of 5 modules per side.
pub fn render_barcode(
    data: &str,
    symbology: Symbology,
    module: usize,
    height: usize,
) -> Result<Bitmap, RenderError> {
    use barcoders::sym::{code128::Code128, code39::Code39, ean13::EAN13};
    let err = |e: barcoders::error::Error| RenderError::Barcode(e.to_string());
    let bars: Vec<u8> = match symbology {
        // 'Ɓ' selects Code 128 character set B (letters, digits, punctuation).
        Symbology::Code128 => Code128::new(format!("\u{0181}{data}")).map_err(err)?.encode(),
        Symbology::Ean13 => EAN13::new(data).map_err(err)?.encode(),
        Symbology::Code39 => Code39::new(data.to_uppercase()).map_err(err)?.encode(),
    };
    let quiet = 5 * module;
    let mut out = Bitmap::new(bars.len() * module + 2 * quiet, height);
    for (i, &bar) in bars.iter().enumerate() {
        if bar == 1 {
            for dx in 0..module {
                for y in 0..height {
                    out.set(quiet + i * module + dx, y, true);
                }
            }
        }
    }
    Ok(out)
}

/// Decode an image (PNG/JPEG), flatten transparency onto white and scale it to `height`.
pub fn render_image(bytes: &[u8], height: usize, dither: bool) -> Result<Bitmap, String> {
    use image::imageops::FilterType;
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    let gray = image::GrayImage::from_fn(w, h, |x, y| {
        let p = img.get_pixel(x, y).0;
        let a = p[3] as f32 / 255.0;
        let l = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
        image::Luma([(l * a + 255.0 * (1.0 - a)).round() as u8])
    });
    let new_w = ((w as f32 * height as f32 / h as f32).round() as u32).max(1);
    let scaled = image::imageops::resize(&gray, new_w, height as u32, FilterType::Triangle);
    Ok(Bitmap::from_gray(new_w as usize, height, scaled.as_raw(), dither))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LW_C410;

    fn ctx(fonts: &[FontArc]) -> RenderContext<'_> {
        static NO_IMAGES: fn(&str) -> Result<Vec<u8>, String> = |_| Err("no images".into());
        RenderContext {
            model: LW_C410,
            tape: LW_C410.tape_by_mm(12).unwrap(),
            fonts,
            icons: None,
            load_image: &NO_IMAGES,
            notes: None,
        }
    }

    #[test]
    fn qr_prefers_larger_squares() {
        // 16 characters: medium correction needs version 2 (2 px squares), low fits version 1 (3 px).
        assert_eq!(qr_module_px("http://s.lan/123", 72), Some(3));
        assert_eq!(qr_module_px("https://inventory.lan/box/12", 72), Some(2));
        assert_eq!(qr_module_px("https://inventory.lan/box/12", 96), Some(3));
    }

    #[test]
    fn qr_fill_uses_full_height() {
        let b = render_qr_fill("https://inventory.lan/box/12", 72, false).unwrap();
        assert_eq!(b.height(), 72);
        // Dark pixels reach the first and the last row (finder patterns at the edges).
        assert!((0..b.width()).any(|x| b.get(x, 0)));
        assert!((0..b.width()).any(|x| b.get(x, 71)));
        assert!(render_qr_fill(&"x".repeat(200), 72, false).is_err());
    }

    #[test]
    fn qr_fits_12mm() {
        let b = render_qr("https://example.com/box/42", 72).unwrap();
        assert!(b.height() <= 72);
        assert!(render_qr(&"x".repeat(500), 72).is_err());
    }

    #[test]
    fn barcode_and_layout() {
        let spec = LabelSpec::new(vec![
            Element::Barcode { data: "BOX-42".into(), symbology: Symbology::Code128, module_px: 2, height_mm: None },
            Element::Qr { data: "42".into(), size_mm: None, fill: false },
        ]);
        let b = render(&spec, &ctx(&[])).unwrap();
        assert_eq!(b.height(), 72);
        assert!(b.width() > 100);
    }

    #[test]
    fn border_and_padding() {
        let mut spec = LabelSpec::new(vec![Element::Qr { data: "42".into(), size_mm: None, fill: false }]);
        spec.border = Border::Line;
        spec.padding_mm = 1.0;
        let framed = render(&spec, &ctx(&[])).unwrap();
        assert_eq!(framed.height(), 72);
        // Top-left corner area of the frame is drawn, the padding above it is blank.
        let m = LW_C410.mm_to_px(2.0);
        let pad = LW_C410.mm_to_px(1.0);
        assert!(framed.get(m + 20, pad));
        assert!(!framed.get(m + 20, pad - 1));
        spec.padding_mm = 0.0;
        for style in [Border::Double, Border::Rounded, Border::Dashed] {
            spec.border = style;
            render(&spec, &ctx(&[])).unwrap();
        }
    }

    #[test]
    fn text_without_font_fails() {
        let spec = LabelSpec::new(vec![Element::text("hi")]);
        assert!(matches!(render(&spec, &ctx(&[])), Err(RenderError::NoFont)));
    }

    #[test]
    fn spec_from_json() {
        let json = r#"{"elements":[{"type":"text","text":"Screws\nM4"},{"type":"qr","data":"x"}]}"#;
        let spec: LabelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.elements.len(), 2);
        assert_eq!(spec.margin_mm, 2.0);
        let json = r#"{"elements":[{"type":"icon","name":"oil","valign":"top","size_mm":5}],"border":"rounded","padding_mm":1}"#;
        let spec: LabelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.elements[0].valign, VAlign::Top);
        assert_eq!(spec.border, Border::Rounded);
    }
}
