//! The screen, read from the file Xvfb keeps it in.
//!
//! With `-fbdir`, Xvfb maps each screen to a file it draws into: an XWD image whose header is
//! written most significant byte first, followed by the window name, the colormap and then the
//! pixels in the server's own byte order. Reading the file is reading the screen as it is now.

use std::{fs::File, io, os::unix::fs::FileExt as _, path::Path};

const HEADER_LEN: usize = 100;
const XWD_COLOR_LEN: u64 = 12;
const MSB_FIRST: u32 = 1;

pub struct Framebuffer {
    file: File,
    offset: u64,
    pub width: u16,
    pub height: u16,
    stride: usize,
    big_endian: bool,
    shifts: [u32; 3],
}

impl Framebuffer {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact_at(&mut header, 0)?;
        let field = |i: usize| u32::from_be_bytes(header[i * 4..i * 4 + 4].try_into().unwrap());
        let (header_size, width, height) = (field(0), field(4), field(5));
        let (byte_order, bits_per_pixel, bytes_per_line) = (field(7), field(11), field(12));
        let masks = [field(14), field(15), field(16)];
        let ncolors = field(19);

        if bits_per_pixel != 32 {
            return Err(io::Error::other(format!(
                "a screen of {bits_per_pixel} bits a pixel; this reads 32"
            )));
        }
        // The visual's masks are written when its colormap is installed; until then, the ones
        // a 24-bit TrueColor screen has.
        let masks = if masks.contains(&0) {
            [0xff0000, 0xff00, 0xff]
        } else {
            masks
        };
        Ok(Self {
            file,
            offset: u64::from(header_size) + u64::from(ncolors) * XWD_COLOR_LEN,
            width: width as u16,
            height: height as u16,
            stride: bytes_per_line as usize,
            big_endian: byte_order == MSB_FIRST,
            shifts: masks.map(u32::trailing_zeros),
        })
    }

    /// Read the whole screen into `pixels`, as `0x00RRGGBB` a pixel, row after row.
    pub fn read(&mut self, pixels: &mut Vec<u32>) -> io::Result<()> {
        let mut raw = vec![0u8; self.stride * usize::from(self.height)];
        self.file.read_exact_at(&mut raw, self.offset)?;
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        pixels.clear();
        pixels.reserve(w * h);
        let [rs, gs, bs] = self.shifts;
        for row in raw.chunks_exact(self.stride) {
            for &px in row[..w * 4].as_chunks::<4>().0 {
                let v = if self.big_endian {
                    u32::from_be_bytes(px)
                } else {
                    u32::from_le_bytes(px)
                };
                let (r, g, b) = ((v >> rs) & 0xff, (v >> gs) & 0xff, (v >> bs) & 0xff);
                pixels.push(r << 16 | g << 8 | b);
            }
        }
        debug_assert_eq!(pixels.len(), w * h);
        Ok(())
    }
}

/// Write `pixels`, as [`Framebuffer::read`] gives them, to `path` as an RGB PNG.
pub fn write_png(screen: &Framebuffer, pixels: &[u32], path: &Path) -> io::Result<()> {
    let file = io::BufWriter::new(File::create(path)?);
    let mut encoder = png::Encoder::new(file, u32::from(screen.width), u32::from(screen.height));
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    let rgb: Vec<u8> = pixels
        .iter()
        .flat_map(|v| [(v >> 16) as u8, (v >> 8) as u8, *v as u8])
        .collect();
    writer.write_image_data(&rgb).map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)
}
