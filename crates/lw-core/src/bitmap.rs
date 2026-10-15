//! 1-bit bitmap. `true` means a printed (black) pixel.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    width: usize,
    height: usize,
    data: Vec<bool>,
}

impl Bitmap {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, data: vec![false; width * height] }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.height && self.data[y * self.width + x]
    }

    pub fn set(&mut self, x: usize, y: usize, v: bool) {
        if x < self.width && y < self.height {
            self.data[y * self.width + x] = v;
        }
    }

    /// Copy `src` into this bitmap with its top-left corner at (`x`, `y`).
    /// Parts outside this bitmap are clipped.
    pub fn blit(&mut self, src: &Bitmap, x: isize, y: isize) {
        for sy in 0..src.height {
            for sx in 0..src.width {
                if src.get(sx, sy) {
                    let (dx, dy) = (x + sx as isize, y + sy as isize);
                    if dx >= 0 && dy >= 0 {
                        self.set(dx as usize, dy as usize, true);
                    }
                }
            }
        }
    }

    /// Convert 8-bit grayscale (0 = black) to 1 bit, either with a fixed threshold
    /// or with Floyd–Steinberg dithering (better for photos).
    pub fn from_gray(width: usize, height: usize, gray: &[u8], dither: bool) -> Self {
        let mut out = Bitmap::new(width, height);
        if !dither {
            for (i, &g) in gray.iter().enumerate().take(width * height) {
                out.data[i] = g < 128;
            }
            return out;
        }
        let mut err: Vec<f32> = gray.iter().map(|&g| g as f32).collect();
        for y in 0..height {
            for x in 0..width {
                let i = y * width + x;
                let old = err[i];
                let black = old < 128.0;
                out.data[i] = black;
                let e = old - if black { 0.0 } else { 255.0 };
                let mut spread = |dx: isize, dy: usize, f: f32| {
                    let nx = x as isize + dx;
                    if nx >= 0 && (nx as usize) < width && y + dy < height {
                        err[(y + dy) * width + nx as usize] += e * f;
                    }
                };
                spread(1, 0, 7.0 / 16.0);
                spread(-1, 1, 3.0 / 16.0);
                spread(0, 1, 5.0 / 16.0);
                spread(1, 1, 1.0 / 16.0);
            }
        }
        out
    }

    /// Grayscale copy (black = 0, white = 255), e.g. for saving a preview.
    pub fn to_gray(&self) -> Vec<u8> {
        self.data.iter().map(|&b| if b { 0 } else { 255 }).collect()
    }
}
