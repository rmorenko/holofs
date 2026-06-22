//! Demo utilities: synthetic "mandala", loading images into WxH, RGB conversion,
//! PNG writing.
//!
//! Input formats: PNG, JPEG, WebP, GIF, BMP, TIFF (via the `image` crate).
//! Output format is always PNG: after inverse DWT we render the detail level
//! as 8-bit RGB, and PNG is the most honest container for the client.

use std::fs::File;
use std::io::BufWriter;

pub fn synth(w: usize, h: usize) -> [Vec<f32>; 3] {
    let mut r = vec![0f32; w * h];
    let mut g = vec![0f32; w * h];
    let mut b = vec![0f32; w * h];
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    // Scale the mandala radius to the frame size; otherwise on a 1024×1024 the
    // pattern shrinks to a tiny dot in the centre.
    let scale = (w.min(h) as f32) / 256.0;
    let rad_norm = 180.0 * scale;
    let ring_freq = 0.6 / scale;
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 - cx;
            let fy = y as f32 - cy;
            let rad = (fx * fx + fy * fy).sqrt();
            let ang = fy.atan2(fx);
            let coarse = (1.0 - rad / rad_norm).clamp(0.0, 1.0);
            let rings = ((rad * ring_freq).sin() * 0.5 + 0.5) * (rad / rad_norm).clamp(0.0, 1.0);
            let spokes = (ang * 8.0).sin() * 0.5 + 0.5;
            let bsz = (8.0 * scale).max(1.0) as usize;
            let checker = (((x / bsz) + (y / bsz)) % 2) as f32;
            let i = y * w + x;
            r[i] = (60.0 + 150.0 * coarse + 45.0 * rings + 15.0 * spokes).clamp(0.0, 255.0);
            g[i] = (40.0 + 120.0 * coarse * (0.5 + 0.5 * spokes) + 50.0 * rings).clamp(0.0, 255.0);
            b[i] = (80.0
                + 90.0 * (0.5 + 0.5 * ang.sin())
                + 55.0 * checker * (rad / (rad_norm * 0.67)).clamp(0.0, 1.0))
            .clamp(0.0, 255.0);
        }
    }
    [r, g, b]
}

/// Decode → centre-crop to a square → box-downscale/stretch to w×h.
/// If the photo is larger than w×h, blocks are averaged; if smaller, the
/// nearest pixel is taken.
///
/// Accepts any format the `image` crate supports: PNG, JPEG, WebP, GIF,
/// BMP, TIFF. All colour spaces are converted to RGB8.
pub fn load_photo(path: &str, w: usize, h: usize) -> [Vec<f32>; 3] {
    let img = image::open(path).expect("could not open photo");
    rgb8_into_channels(img, w, h)
}

/// Same logic but from bytes in memory. Returns `Result` — suitable for HTTP input.
pub fn load_photo_from_bytes(bytes: &[u8], w: usize, h: usize) -> Result<[Vec<f32>; 3], String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("decode failed: {e}"))?;
    Ok(rgb8_into_channels(img, w, h))
}

fn rgb8_into_channels(img: image::DynamicImage, w: usize, h: usize) -> [Vec<f32>; 3] {
    // Convert any colour space to RGB8 (8-bit RGB without alpha).
    let rgb = img.to_rgb8();
    let iw = rgb.width() as usize;
    let ih = rgb.height() as usize;
    let buf = rgb.as_raw(); // linear buffer of length iw*ih*3
    let at = |x: usize, y: usize, c: usize| -> f32 { buf[(y * iw + x) * 3 + c] as f32 };
    let side = iw.min(ih);
    let ox = (iw - side) / 2;
    let oy = (ih - side) / 2;
    let mut out = [vec![0f32; w * h], vec![0f32; w * h], vec![0f32; w * h]];
    for oyp in 0..h {
        for oxp in 0..w {
            let sx0 = ox + oxp * side / w;
            let sx1 = (ox + (oxp + 1) * side / w).max(sx0 + 1);
            let sy0 = oy + oyp * side / h;
            let sy1 = (oy + (oyp + 1) * side / h).max(sy0 + 1);
            for c in 0..3 {
                let mut acc = 0f32;
                let mut cnt = 0f32;
                for yy in sy0..sy1.min(ih) {
                    for xx in sx0..sx1.min(iw) {
                        acc += at(xx, yy, c);
                        cnt += 1.0;
                    }
                }
                out[c][oyp * w + oxp] = acc / cnt.max(1.0);
            }
        }
    }
    out
}

pub fn to_rgb(ch: &[Vec<f32>; 3], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    for i in 0..w * h {
        for c in 0..3 {
            out[i * 3 + c] = ch[c][i].round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

pub fn save_png(path: &str, w: u32, h: u32, rgb: &[u8]) {
    let file = File::create(path).unwrap();
    let bw = BufWriter::new(file);
    let mut enc = png::Encoder::new(bw, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgb).unwrap();
}
