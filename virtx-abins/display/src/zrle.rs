//! ZRLE (RFC 6143 §7.7.6): a rectangle cut into 64×64 tiles, each the smallest of a solid
//! colour, a palette of up to 16, a run-length list or its pixels, and all of it through one
//! zlib stream that lasts as long as the connection.

use flate2::{Compress, Compression, FlushCompress};

use crate::rfb::PixelFormat;

const TILE: usize = 64;

pub struct Zrle {
    zlib: Compress,
    tiles: Vec<u8>,
}

impl Zrle {
    pub fn new() -> Self {
        Self {
            zlib: Compress::new(Compression::fast(), true),
            tiles: Vec::new(),
        }
    }

    /// Encode the `w`×`h` rectangle at `x`,`y` of `screen` (`stride` pixels a row) in
    /// `format`, appending its length and zlib data to `out`.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &mut self,
        screen: &[u32],
        stride: usize,
        (x, y, w, h): (usize, usize, usize, usize),
        format: &PixelFormat,
        out: &mut Vec<u8>,
    ) {
        self.tiles.clear();
        let mut tile = Vec::with_capacity(TILE * TILE);
        for ty in (y..y + h).step_by(TILE) {
            let th = TILE.min(y + h - ty);
            for tx in (x..x + w).step_by(TILE) {
                let tw = TILE.min(x + w - tx);
                tile.clear();
                for row in ty..ty + th {
                    let start = row * stride + tx;
                    tile.extend(screen[start..start + tw].iter().map(|&v| format.pixel(v)));
                }
                encode_tile(&tile, tw, format, &mut self.tiles);
            }
        }

        let length_at = out.len();
        out.extend_from_slice(&[0; 4]);
        let mut input = &self.tiles[..];
        loop {
            out.reserve(input.len() / 2 + 64);
            let before = self.zlib.total_in();
            self.zlib
                .compress_vec(input, out, FlushCompress::Sync)
                .expect("zlib compression of a buffer in memory");
            input = &input[(self.zlib.total_in() - before) as usize..];
            if input.is_empty() && out.len() < out.capacity() {
                break;
            }
        }
        let length = (out.len() - length_at - 4) as u32;
        out[length_at..length_at + 4].copy_from_slice(&length.to_be_bytes());
    }
}

/// One tile, `pixels` already in the client's format, `w` a row.
fn encode_tile(pixels: &[u32], w: usize, format: &PixelFormat, out: &mut Vec<u8>) {
    let mut palette: Vec<u32> = Vec::with_capacity(17);
    for &p in pixels {
        if !palette.contains(&p) {
            palette.push(p);
            if palette.len() > 16 {
                break;
            }
        }
    }
    let cpixel = format.cpixel_len();

    if palette.len() == 1 {
        out.push(1);
        format.write_cpixel(palette[0], out);
        return;
    }
    if palette.len() <= 16 {
        out.push(palette.len() as u8);
        for &p in &palette {
            format.write_cpixel(p, out);
        }
        let bits = match palette.len() {
            2 => 1,
            3..=4 => 2,
            _ => 4,
        };
        for row in pixels.chunks(w) {
            let (mut byte, mut used) = (0u8, 0);
            for p in row {
                let index = palette.iter().position(|q| q == p).unwrap() as u8;
                byte = byte << bits | index;
                used += bits;
                if used == 8 {
                    out.push(byte);
                    (byte, used) = (0, 0);
                }
            }
            if used > 0 {
                out.push(byte << (8 - used));
            }
        }
        return;
    }

    // Plain RLE when the runs come to less than the pixels themselves.
    let runs = runs(pixels);
    let rle_len: usize = runs.iter().map(|&(_, n)| cpixel + (n - 1) / 255 + 1).sum();
    if rle_len < pixels.len() * cpixel {
        out.push(128);
        for (p, n) in runs {
            format.write_cpixel(p, out);
            let mut rest = n - 1;
            while rest >= 255 {
                out.push(255);
                rest -= 255;
            }
            out.push(rest as u8);
        }
    } else {
        out.push(0);
        for &p in pixels {
            format.write_cpixel(p, out);
        }
    }
}

fn runs(pixels: &[u32]) -> Vec<(u32, usize)> {
    let mut runs: Vec<(u32, usize)> = Vec::new();
    for &p in pixels {
        match runs.last_mut() {
            Some((q, n)) if *q == p => *n += 1,
            _ => runs.push((p, 1)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_of_one_colour_is_its_colour() {
        let format = PixelFormat::default();
        let mut out = Vec::new();
        encode_tile(&[0x123456; 16], 4, &format, &mut out);
        assert_eq!(out, [1, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn two_colours_pack_a_bit_a_pixel_padded_to_the_row() {
        let format = PixelFormat::default();
        let mut out = Vec::new();
        encode_tile(&[0, 1, 1, 0, 1, 0], 3, &format, &mut out);
        assert_eq!(out, [2, 0, 0, 0, 1, 0, 0, 0b0110_0000, 0b0100_0000]);
    }

    #[test]
    fn runs_beyond_255_continue_in_255s() {
        let format = PixelFormat::default();
        let mut pixels = vec![0; 300];
        pixels.extend((1..=17).collect::<Vec<u32>>());
        let mut out = Vec::new();
        encode_tile(&pixels, 317, &format, &mut out);
        assert_eq!(out[0], 128);
        assert_eq!(&out[1..6], [0, 0, 0, 255, 44]);
    }
}
