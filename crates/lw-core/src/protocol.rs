//! Wire protocol of Epson LabelWorks printers (verified on an LW-C410).
//!
//! Control commands are framed as `ESC { <len> <cmd> <params..> <checksum> }`, where
//! `len` counts the bytes from `cmd` through the closing `}` and `checksum` is the low
//! byte of the sum of `cmd` and `params`. The label itself is sent as raster lines.

use crate::bitmap::Bitmap;
use std::collections::BTreeMap;

/// Number of raster lines Epson's library sends per burst.
pub const BAND_LINES: usize = 1024;

/// Build one framed control command.
pub fn frame(cmd: u8, params: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(params.len() + 6);
    out.extend_from_slice(&[0x1B, 0x7B, (params.len() + 3) as u8, cmd]);
    out.extend_from_slice(params);
    let sum = params.iter().fold(cmd as u32, |a, &b| a + b as u32);
    out.push((sum & 0xFF) as u8);
    out.push(0x7D);
    out
}

/// Ask the printer for one status report.
pub fn status_request() -> Vec<u8> {
    frame(0x51, &[0x00, 0x00])
}

/// Ask the printer to report status by itself while printing (it reports once the job ends).
pub fn auto_status_request() -> Vec<u8> {
    frame(0x51, &[0x05, 0x00])
}

/// When the printer cuts the tape during a job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CutMode {
    /// Cut after every label.
    #[default]
    Each,
    /// Cut only after the last label of the job (one strip).
    End,
    /// Never cut.
    None,
}

/// Job-level settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobOptions {
    /// Print density from -5 (light) to +5 (dark); 0 is the printer default.
    pub density: i8,
    pub cut: CutMode,
}

/// Commands that start a job (byte-identical to Epson's library for the LW-C410).
pub fn job_setup(opts: &JobOptions) -> Vec<u8> {
    let cut: [u8; 4] = match opts.cut {
        CutMode::Each => [1, 1, 1, 1],
        CutMode::End => [1, 0, 1, 1],
        CutMode::None => [0, 0, 0, 0],
    };
    let density = (opts.density.clamp(-5, 5) + 5) as u8;
    let mut out = frame(0x40, &[]);
    out.extend(frame(0x7B, &[0x00, 0x00, b'S', b'T']));
    out.extend(frame(0x43, &cut));
    out.extend(frame(0x44, &[density]));
    out.extend(frame(0x47, &[]));
    out
}

/// Commands that start a page: its length in raster lines and a margin.
pub fn page_setup(lines: u32, margin: u16) -> Vec<u8> {
    let mut out = frame(0x4C, &lines.to_le_bytes());
    out.extend(frame(0x54, &margin.to_le_bytes()));
    out
}

/// End of page.
pub fn page_end() -> Vec<u8> {
    vec![0x0C]
}

/// End of job.
pub fn job_end() -> Vec<u8> {
    frame(0x40, &[])
}

/// How the bitmap is mapped onto the tape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Orientation {
    /// Readable when the tape is held as in the first calibration print.
    #[default]
    Normal,
    /// Rotated by 180 degrees.
    Rotated,
}

/// Encode a label bitmap (x = along the tape, y = across it) into raster lines,
/// grouped into bands of [`BAND_LINES`] lines. `bitmap.height()` must equal the
/// number of printable pixels across the tape.
///
/// The printer prints line 0 at the far end of the label, with the first pixel of
/// each line at the top edge, so without correction an image comes out mirrored.
pub fn raster_bands(bitmap: &Bitmap, orientation: Orientation) -> Vec<Vec<u8>> {
    let (w, h) = (bitmap.width(), bitmap.height());
    let bytes_per_line = h.div_ceil(8);
    let header = {
        let mut v = vec![0x1B, 0x2E, 0x00, 0x00, 0x00, 0x01];
        v.extend_from_slice(&(h as u16).to_le_bytes());
        v
    };
    let mut bands = Vec::new();
    let mut band = Vec::new();
    for line in 0..w {
        band.extend_from_slice(&header);
        let mut packed = vec![0u8; bytes_per_line];
        for px in 0..h {
            let (x, y) = match orientation {
                Orientation::Normal => (w - 1 - line, px),
                Orientation::Rotated => (line, h - 1 - px),
            };
            if bitmap.get(x, y) {
                packed[px / 8] |= 0x80 >> (px % 8);
            }
        }
        band.extend_from_slice(&packed);
        if (line + 1) % BAND_LINES == 0 {
            bands.push(std::mem::take(&mut band));
        }
    }
    if !band.is_empty() {
        bands.push(band);
    }
    bands
}

pub const ERROR_LOW_VOLTAGE: u8 = 0x22;

/// One decoded status report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// All `XX:hh` fields of the report.
    pub fields: BTreeMap<String, u8>,
}

impl Status {
    fn field(&self, key: &str) -> u8 {
        self.fields.get(key).copied().unwrap_or(0)
    }
    /// `ST`: 0 means idle.
    pub fn state(&self) -> u8 {
        self.field("ST")
    }
    /// `ER`: 0 means no error.
    pub fn error(&self) -> u8 {
        self.field("ER")
    }
    /// `TW`: tape width code (0 = no tape).
    pub fn tape_code(&self) -> u8 {
        self.field("TW")
    }
    /// Ready for a new job: idle or finished with the previous one, and no error.
    pub fn is_ready(&self) -> bool {
        matches!(self.state(), STATE_IDLE | STATE_PRINT_END) && self.error() == 0
    }

    /// Feeding, receiving data or printing.
    pub fn is_busy(&self) -> bool {
        !matches!(self.state(), STATE_IDLE | STATE_PRINT_END | STATE_FEED_END)
    }

    /// Parse one 64-byte report like `@BDC ST\r\nST:00;ER:00;TW:03;...;\xFF`.
    pub fn parse(block: &[u8]) -> Option<Status> {
        if block.len() < 2 || block[0] != b'@' || *block.last()? != 0xFF {
            return None;
        }
        let body = &block[..block.len() - 1];
        let start = body.windows(2).position(|w| w == b"\r\n").map(|p| p + 2).unwrap_or(0);
        let text = String::from_utf8_lossy(&body[start..]);
        let mut fields = BTreeMap::new();
        for item in text.split(';') {
            if let Some((k, v)) = item.trim().split_once(':') {
                if let Ok(v) = u8::from_str_radix(v.trim(), 16) {
                    fields.insert(k.trim().to_string(), v);
                }
            }
        }
        if fields.is_empty() {
            None
        } else {
            Some(Status { fields })
        }
    }
}

pub const STATE_IDLE: u8 = 0x00;
pub const STATE_FEED_END: u8 = 0x04;
pub const STATE_PRINT_END: u8 = 0x05;

/// Name of an `ST` value (from Epson's library).
pub fn state_name(code: u8) -> &'static str {
    match code {
        0x00 => "idle",
        0x01 => "feeding",
        0x02 => "printing",
        0x03 => "receiving data",
        0x04 => "feed finished",
        0x05 => "print finished",
        0x13 => "updating firmware",
        _ => "unknown state",
    }
}

/// Name of an `ER` value (from Epson's library; values are hexadecimal).
pub fn error_name(code: u8) -> &'static str {
    match code {
        0x00 => "no error",
        0x01 => "cutter error",
        0x06 => "no tape cartridge",
        0x15 => "print head overheated",
        0x20 => "cancelled on the printer",
        0x21 => "cover open",
        0x22 => "low voltage (replace the batteries or use the AC adapter)",
        0x23 => "cancelled by switching off",
        0x24 => "tape eject error",
        0x30 => "tape feed error (tape jammed?)",
        0x40 => "ink ribbon slack",
        0x41 => "ink ribbon too short",
        0x42 => "end of tape",
        0x43 => "label cut error",
        0x44 => "temperature error",
        0x5C => "stopped by auto power-off",
        0x5D => "stopped by power supply change",
        _ => "unknown error",
    }
}

/// Status reports are 64 bytes long, start with `@` and end with `0xFF`.
pub const STATUS_LEN: usize = 64;

/// Collects incoming bytes and yields complete status reports.
#[derive(Debug, Default)]
pub struct StatusReader {
    buf: Vec<u8>,
}

impl StatusReader {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Return the next complete report, if one has arrived.
    pub fn next_status(&mut self) -> Option<Status> {
        loop {
            let start = self.buf.iter().position(|&b| b == b'@')?;
            if self.buf.len() - start < STATUS_LEN {
                self.buf.drain(..start);
                return None;
            }
            let block: Vec<u8> = self.buf[start..start + STATUS_LEN].to_vec();
            if block[STATUS_LEN - 1] == 0xFF {
                self.buf.drain(..start + STATUS_LEN);
                if let Some(s) = Status::parse(&block) {
                    return Some(s);
                }
            } else {
                // Not a report: skip this '@' and keep looking.
                self.buf.drain(..start + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn commands_match_epson_library() {
        assert_eq!(hex(&status_request()), "1b 7b 05 51 00 00 51 7d");
        assert_eq!(hex(&auto_status_request()), "1b 7b 05 51 05 00 56 7d");
        assert_eq!(
            hex(&job_setup(&JobOptions::default())),
            "1b 7b 03 40 40 7d 1b 7b 07 7b 00 00 53 54 22 7d 1b 7b 07 43 01 01 01 01 47 7d \
             1b 7b 04 44 05 49 7d 1b 7b 03 47 47 7d"
        );
        let no_cut = job_setup(&JobOptions { density: -5, cut: CutMode::None });
        assert!(hex(&no_cut).contains("1b 7b 07 43 00 00 00 00 43 7d 1b 7b 04 44 00 44 7d"));
        let at_end = job_setup(&JobOptions { density: 0, cut: CutMode::End });
        assert!(hex(&at_end).contains("1b 7b 07 43 01 00 01 01 46 7d"));
        assert_eq!(hex(&page_setup(500, 0)), "1b 7b 07 4c f4 01 00 00 41 7d 1b 7b 05 54 00 00 54 7d");
        assert_eq!(hex(&page_setup(300, 20)), "1b 7b 07 4c 2c 01 00 00 79 7d 1b 7b 05 54 14 00 68 7d");
        assert_eq!(hex(&job_end()), "1b 7b 03 40 40 7d");
    }

    #[test]
    fn parses_real_status() {
        let raw = b"@BDC ST\r\nST:00;ER:00;WR:00;RV:00;TW:03;TR:00;CT:00;OP:00;EI:00;\xFF";
        assert_eq!(raw.len(), 64);
        let mut r = StatusReader::default();
        r.push(&raw[..20]);
        assert!(r.next_status().is_none());
        r.push(&raw[20..]);
        let s = r.next_status().unwrap();
        assert!(s.is_ready());
        assert!(!s.is_busy());
        assert!(!s.is_busy());
        assert_eq!(s.tape_code(), 3);
        assert_eq!(s.fields.len(), 9);
    }

    #[test]
    fn status_states() {
        let parse = |t: &str| {
            let mut raw = format!("@BDC ST\r\n{t}").into_bytes();
            raw.resize(63, b' ');
            raw.push(0xFF);
            Status::parse(&raw).unwrap()
        };
        let printing = parse("ST:02;ER:00;TW:03;");
        assert!(printing.is_busy() && !printing.is_ready());
        let done = parse("ST:05;ER:00;TW:03;");
        assert!(done.is_ready() && !done.is_busy());
        let low = parse("ST:05;ER:22;EI:08;TW:03;");
        assert!(!low.is_ready());
        assert_eq!(error_name(low.error()), "low voltage (replace the batteries or use the AC adapter)");
    }

    #[test]
    fn raster_orientation() {
        // 2 lines long, 9 pixels across; mark pixel (x=0, y=0).
        let mut b = Bitmap::new(2, 9);
        b.set(0, 0, true);
        let bands = raster_bands(&b, Orientation::Normal);
        assert_eq!(bands.len(), 1);
        let line_len = 8 + 2;
        // Normal: x=0 is sent last, y=0 is the first (highest) bit.
        assert_eq!(&bands[0][..8], &[0x1B, 0x2E, 0, 0, 0, 1, 9, 0]);
        assert_eq!(&bands[0][8..10], &[0, 0]);
        assert_eq!(&bands[0][line_len + 8..], &[0x80, 0]);
        // Rotated: x=0 is sent first, y=0 is the last pixel of the line.
        let r = raster_bands(&b, Orientation::Rotated);
        assert_eq!(&r[0][8..10], &[0x00, 0x80]);
    }
}
