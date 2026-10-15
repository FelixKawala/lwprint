//! Bundled icon font (Material Design Icons) and monochrome emoji font (Noto Emoji),
//! with search over names, aliases and tags. Licenses are in `assets/`.

use ab_glyph::FontArc;
use lw_core::IconFont;
use serde::Serialize;
use std::collections::HashMap;

pub static MDI_TTF: &[u8] = include_bytes!("../assets/materialdesignicons-webfont.ttf");
static MDI_INDEX: &str = include_str!("../assets/mdi.tsv");
static EMOJI_TTF: &[u8] = include_bytes!("../assets/NotoEmoji.ttf");
static EMOJI_INDEX: &str = include_str!("../assets/emoji.tsv");

/// Shown before anything is typed into the icon search.
const SUGGESTED: &[&str] = &[
    "home", "tools", "screwdriver", "hammer-wrench", "wrench", "toolbox", "power-plug", "power-socket-eu",
    "cable-data", "usb", "battery", "lightbulb", "fuse", "flash", "water", "fire", "fridge", "washing-machine",
    "silverware-fork-knife", "coffee", "food-apple", "bottle-wine", "medical-bag", "pill", "flower", "sprout",
    "dog", "cat", "car", "bicycle", "book-open-variant", "file-document", "folder", "archive",
    "package-variant-closed", "gift", "tshirt-crew", "shoe-formal", "baby-carriage", "gamepad-variant",
    "music", "camera", "laptop", "router-wireless", "printer", "key", "lock", "star", "heart", "alert",
    "information", "check-circle", "close-circle", "arrow-up-bold", "recycle", "snowflake", "sun-thermometer",
];

#[derive(Debug, Clone, Serialize)]
pub struct IconHit {
    pub name: String,
    /// Code point in the icon font, as hex (for showing the icon in a browser).
    pub codepoint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmojiHit {
    pub emoji: String,
    pub name: String,
    pub group: String,
}

struct IconEntry {
    name: &'static str,
    codepoint: &'static str,
    aliases: &'static str,
    tags: &'static str,
}

fn icon_entries() -> impl Iterator<Item = IconEntry> {
    MDI_INDEX.lines().filter_map(|l| {
        let mut f = l.split('\t');
        Some(IconEntry { name: f.next()?, codepoint: f.next()?, aliases: f.next()?, tags: f.next().unwrap_or("") })
    })
}

pub fn icon_font() -> IconFont {
    let font = FontArc::try_from_slice(MDI_TTF).expect("bundled icon font is valid");
    let glyphs: HashMap<String, char> = icon_entries()
        .filter_map(|e| {
            let ch = char::from_u32(u32::from_str_radix(e.codepoint, 16).ok()?)?;
            Some((e.name.to_string(), ch))
        })
        .collect();
    IconFont { font, glyphs }
}

pub fn emoji_font() -> FontArc {
    FontArc::try_from_slice(EMOJI_TTF).expect("bundled emoji font is valid")
}

/// Score how well `field` matches `term` (lower is better).
fn score(term: &str, name: &str, aliases: &str, tags: &str) -> Option<u32> {
    if name == term {
        Some(0)
    } else if name.starts_with(term) {
        Some(1)
    } else if name.split('-').any(|w| w.starts_with(term)) {
        Some(2)
    } else if name.contains(term) {
        Some(3)
    } else if aliases.split(' ').any(|a| a.contains(term)) {
        Some(4)
    } else if tags.contains(term) {
        Some(5)
    } else {
        None
    }
}

/// Search icons. All words must match; better matches and shorter names come first.
pub fn search_icons(query: &str, limit: usize) -> Vec<IconHit> {
    let q = query.trim().to_lowercase();
    let q = q.strip_prefix("mdi:").unwrap_or(&q).to_string();
    let terms: Vec<&str> = q.split_whitespace().collect();
    if terms.is_empty() {
        let index: HashMap<&str, &str> = icon_entries().map(|e| (e.name, e.codepoint)).collect();
        return SUGGESTED
            .iter()
            .filter_map(|n| index.get(n).map(|cp| IconHit { name: n.to_string(), codepoint: cp.to_string() }))
            .take(limit)
            .collect();
    }
    let mut hits: Vec<(u32, usize, IconHit)> = icon_entries()
        .filter_map(|e| {
            let total = terms
                .iter()
                .map(|t| score(t, e.name, e.aliases, e.tags))
                .try_fold(0u32, |acc, s| s.map(|s| acc + s))?;
            Some((total, e.name.len(), IconHit { name: e.name.into(), codepoint: e.codepoint.into() }))
        })
        .collect();
    hits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    hits.into_iter().take(limit).map(|h| h.2).collect()
}

/// Search emoji by name or group. Only emoji the bundled font can print are listed.
pub fn search_emoji(query: &str, limit: usize) -> Vec<EmojiHit> {
    let q = query.trim().to_lowercase();
    let terms: Vec<&str> = q.split_whitespace().collect();
    let mut hits: Vec<(u32, usize, EmojiHit)> = EMOJI_INDEX
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let mut f = l.split('\t');
            let (emoji, name, group) = (f.next()?, f.next()?, f.next().unwrap_or(""));
            let group_l = group.to_lowercase();
            let total = terms.iter().try_fold(0u32, |acc, t| {
                let s = if name == *t {
                    0
                } else if name.split(' ').any(|w| w.starts_with(t)) {
                    1
                } else if name.contains(t) {
                    2
                } else if group_l.contains(t) {
                    3
                } else {
                    return None;
                };
                Some(acc + s)
            })?;
            Some((total, i, EmojiHit { emoji: emoji.into(), name: name.into(), group: group.into() }))
        })
        .collect();
    hits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    hits.into_iter().take(limit).map(|h| h.2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_icons() {
        let hits = search_icons("screwdriver", 5);
        assert_eq!(hits[0].name, "screwdriver");
        assert!(!search_icons("", 10).is_empty());
        assert!(search_icons("light bulb", 10).iter().any(|h| h.name.contains("lightbulb") || h.name.contains("bulb")));
        let font = icon_font();
        assert!(font.get("mdi:screwdriver").is_some());
        assert!(SUGGESTED.iter().all(|n| font.get(n).is_some()), "all suggested icons exist");
    }

    #[test]
    fn finds_emoji() {
        let hits = search_emoji("smile", 5);
        assert!(!hits.is_empty());
        assert!(search_emoji("", 3).len() == 3);
        let f = emoji_font();
        use ab_glyph::Font;
        assert_ne!(f.glyph_id('😀').0, 0);
    }
}
