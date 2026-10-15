//! Printer models and tapes. Values come from Epson's Label Editor app.

/// A tape size the printer can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tape {
    /// Nominal width in millimetres.
    pub mm: u8,
    /// `TW` code the printer reports when this tape is inserted.
    pub code: u8,
    /// Printable pixels across the tape.
    pub pixels: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub name: &'static str,
    /// Resolution in dots per inch (same along and across the tape).
    pub dpi: u32,
    pub tapes: &'static [Tape],
}

pub const LW_C410: Model = Model {
    name: "LW-C410",
    dpi: 180,
    tapes: &[
        Tape { mm: 4, code: 0x0B, pixels: 22 },
        Tape { mm: 6, code: 0x01, pixels: 36 },
        Tape { mm: 9, code: 0x02, pixels: 54 },
        Tape { mm: 12, code: 0x03, pixels: 72 },
        Tape { mm: 18, code: 0x04, pixels: 96 },
    ],
};

impl Model {
    pub fn tape_by_code(&self, code: u8) -> Option<Tape> {
        // The printer may also report codes with 0x50 added (e.g. 0x53 for 12 mm).
        let code = if (0x51..=0x5B).contains(&code) { code - 0x50 } else { code };
        self.tapes.iter().copied().find(|t| t.code == code)
    }

    pub fn tape_by_mm(&self, mm: u8) -> Option<Tape> {
        self.tapes.iter().copied().find(|t| t.mm == mm)
    }

    pub fn mm_to_px(&self, mm: f32) -> usize {
        (mm * self.dpi as f32 / 25.4).round().max(0.0) as usize
    }
}
