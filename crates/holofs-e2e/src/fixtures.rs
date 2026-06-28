//! Small deterministic byte fixtures used by the harness when seeding
//! test corpora. Everything here is generated procedurally — no
//! committed binary blobs — so re-running the suite always produces
//! byte-identical PUT bodies.

/// A 32×32 solid-blue PNG. Enough bytes to exercise the image
/// ingest pipeline without paying the cost of a real photo. The
/// gateway upscales to its native 512×512 working resolution at
/// PUT time.
pub fn tiny_image_png() -> &'static [u8] {
    static PNG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PNG.get_or_init(|| solid_png(32, 32, [40, 80, 200]))
}

/// A 128×128 deterministic-noise PNG. Solid-colour PNGs are too
/// degenerate for the streaming-hologram tests — every DWT layer
/// past the coarsest is all-zero, so the multipart stream sends a
/// single frame and the browser viewport never changes. This
/// fixture has per-pixel variation at every Haar scale, so each
/// layer in the progressive reveal produces visibly different
/// output.
pub fn textured_image_png() -> &'static [u8] {
    static PNG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PNG.get_or_init(|| textured_png(128, 128, 0xC0FFEE))
}

fn textured_png(w: u32, h: u32, seed: u32) -> Vec<u8> {
    // Cheap LCG so the test stays free of `rand` dependencies. The
    // exact noise distribution doesn't matter; we only need enough
    // variation at every spatial scale to make Haar layer 3 ≠ 0.
    let mut state: u32 = seed.wrapping_mul(0x9E37_79B1);
    let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for _ in 0..(w * h) {
        // LCG step.
        state = state
            .wrapping_mul(1_103_515_245)
            .wrapping_add(12345);
        let r = (state >> 16) as u8;
        let g = (state >> 8) as u8;
        let b = state as u8;
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    rgb_png(w, h, &rgb)
}

fn rgb_png(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + rgb.len());
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_chunk(&mut out, *b"IHDR", &ihdr);
    let mut raw = Vec::with_capacity((1 + 3 * w as usize) * h as usize);
    for row in 0..h as usize {
        raw.push(0);
        let start = row * 3 * w as usize;
        raw.extend_from_slice(&rgb[start..start + 3 * w as usize]);
    }
    write_chunk(&mut out, *b"IDAT", &zlib_store(&raw));
    write_chunk(&mut out, *b"IEND", &[]);
    out
}

/// Six 64×64 PNGs with deliberately different palettes / patterns so
/// CLIP ranks them apart. Returned as `(filename, bytes)`. The names
/// match what the search-band tests expect.
pub fn search_corpus() -> Vec<(String, Vec<u8>)> {
    vec![
        ("mountain.png".into(), solid_png(64, 64, [80, 130, 90])),
        ("ocean.png".into(), solid_png(64, 64, [30, 90, 180])),
        ("sunset.png".into(), solid_png(64, 64, [220, 110, 60])),
        ("forest.png".into(), solid_png(64, 64, [40, 110, 50])),
        ("snow.png".into(), solid_png(64, 64, [230, 235, 240])),
        ("desert.png".into(), solid_png(64, 64, [210, 180, 110])),
    ]
}

/// Build a solid-colour PNG of the given dimensions. Uses the
/// minimal-but-valid PNG layout: IHDR + IDAT (uncompressed, stored
/// blocks) + IEND. Output is deterministic.
fn solid_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + (w as usize) * (h as usize) * 4);
    // PNG signature.
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    // IHDR.
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type = RGB (no alpha)
    ihdr.push(0); // compression = deflate
    ihdr.push(0); // filter
    ihdr.push(0); // interlace
    write_chunk(&mut out, *b"IHDR", &ihdr);
    // IDAT: filter byte 0x00 per row, then RGB triplets, wrapped in a
    // zlib stream with stored (uncompressed) blocks.
    let mut raw = Vec::with_capacity((1 + 3 * w as usize) * h as usize);
    for _ in 0..h {
        raw.push(0); // filter = none
        for _ in 0..w {
            raw.extend_from_slice(&rgb);
        }
    }
    let zlib = zlib_store(&raw);
    write_chunk(&mut out, *b"IDAT", &zlib);
    // IEND.
    write_chunk(&mut out, *b"IEND", &[]);
    out
}

fn write_chunk(buf: &mut Vec<u8>, tag: [u8; 4], data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let crc_start = buf.len();
    buf.extend_from_slice(&tag);
    buf.extend_from_slice(data);
    let crc = crc32_ieee(&buf[crc_start..]);
    buf.extend_from_slice(&crc.to_be_bytes());
}

/// Zlib-wrap `raw` using "stored" (uncompressed) deflate blocks. The
/// decoder side doesn't care about compression ratio for tiny test
/// fixtures, and a stored stream is trivially deterministic.
fn zlib_store(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    // zlib header: deflate, 32K window, no preset dict, default level.
    out.push(0x78);
    out.push(0x01);
    // Split into deflate stored blocks of ≤ 65535 bytes each.
    let mut i = 0;
    while i < raw.len() {
        let chunk = std::cmp::min(0xFFFF, raw.len() - i);
        let bfinal = if i + chunk == raw.len() { 1 } else { 0 };
        // First byte: BFINAL bit + BTYPE=00 (stored).
        out.push(bfinal);
        out.extend_from_slice(&(chunk as u16).to_le_bytes());
        let nlen = !(chunk as u16);
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&raw[i..i + chunk]);
        i += chunk;
    }
    // Adler-32 checksum of the uncompressed data.
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &c in data {
        a = (a + c as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
