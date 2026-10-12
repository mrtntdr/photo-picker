//! JPEG decoding into egui images, using the decoder's DCT scaling so we only
//! decode as many pixels as the screen can show (1/2, 1/4, 1/8 decodes are
//! several times faster than a full decode).

use eframe::egui::{Color32, ColorImage};
use jpeg_decoder::{Decoder, PixelFormat};

/// Decodes `data` so the (orientation-corrected) result fits at least
/// `max_w` x `max_h` without decoding more than needed. `max_side` is the GPU
/// texture limit.
pub fn decode_jpeg(
    data: &[u8],
    orientation: u8,
    max_w: u32,
    max_h: u32,
    max_side: u32,
) -> Result<ColorImage, String> {
    let mut dec = Decoder::new(data);
    dec.read_info().map_err(|e| e.to_string())?;
    let info = dec.info().ok_or("missing JPEG info")?;
    let (fw, fh) = (info.width as u32, info.height as u32);
    if fw == 0 || fh == 0 {
        return Err("empty image".into());
    }

    // Target box in the image's stored (unrotated) orientation.
    let rotated = orientation >= 5;
    let (bw, bh) = if rotated { (max_h, max_w) } else { (max_w, max_h) };
    let bw = bw.min(max_side);
    let bh = bh.min(max_side);
    let fit = (bw as f64 / fw as f64).min(bh as f64 / fh as f64).min(1.0);
    let rw = ((fw as f64 * fit).ceil() as u32).clamp(1, u16::MAX as u32) as u16;
    let rh = ((fh as f64 * fit).ceil() as u32).clamp(1, u16::MAX as u32) as u16;
    let (sw, sh) = dec.scale(rw, rh).map_err(|e| e.to_string())?;
    if (sw as u32, sh as u32) == (fw, fh) && info.pixel_format != PixelFormat::CMYK32 {
        // No DCT scaling possible: zune-jpeg is faster at full size.
        return decode_jpeg_full(data, orientation, max_side);
    }

    let pixels = dec.decode().map_err(|e| e.to_string())?;
    let info = dec.info().ok_or("missing JPEG info")?;
    let (w, h) = (info.width as usize, info.height as usize);

    let src: Vec<Color32> = match info.pixel_format {
        PixelFormat::RGB24 => pixels
            .chunks_exact(3)
            .map(|p| Color32::from_rgb(p[0], p[1], p[2]))
            .collect(),
        PixelFormat::L8 => pixels.iter().map(|&l| Color32::from_gray(l)).collect(),
        PixelFormat::L16 => pixels.chunks_exact(2).map(|p| Color32::from_gray(p[0])).collect(),
        PixelFormat::CMYK32 => pixels
            .chunks_exact(4)
            .map(|p| {
                // Adobe CMYK JPEGs are stored inverted.
                let k = p[3] as u32;
                let f = |c: u8| ((c as u32 * k) / 255) as u8;
                Color32::from_rgb(f(p[0]), f(p[1]), f(p[2]))
            })
            .collect(),
    };
    if src.len() < w * h {
        return Err("decoded image is truncated".into());
    }

    let mut img = orient(src, w, h, orientation);
    // If the texture limit still applies (e.g. 8x zoom of huge files), halve.
    while img.size[0] > max_side as usize || img.size[1] > max_side as usize {
        img = halve(&img);
    }
    Ok(img)
}

/// Applies an EXIF orientation (1..=8) to a row-major pixel buffer.
fn orient(src: Vec<Color32>, w: usize, h: usize, o: u8) -> ColorImage {
    if o <= 1 || o > 8 {
        return ColorImage::new([w, h], src);
    }
    let (dw, dh) = if o >= 5 { (h, w) } else { (w, h) };
    let mut dst = vec![Color32::BLACK; dw * dh];
    // Work in 64x64 tiles so rotations stay cache-friendly.
    const T: usize = 64;
    for ty in (0..h).step_by(T) {
        for tx in (0..w).step_by(T) {
            for y in ty..(ty + T).min(h) {
                for x in tx..(tx + T).min(w) {
                    let (dx, dy) = match o {
                        2 => (w - 1 - x, y),
                        3 => (w - 1 - x, h - 1 - y),
                        4 => (x, h - 1 - y),
                        5 => (y, x),
                        6 => (h - 1 - y, x),
                        7 => (h - 1 - y, w - 1 - x),
                        _ => (y, w - 1 - x), // 8
                    };
                    dst[dy * dw + dx] = src[y * w + x];
                }
            }
        }
    }
    ColorImage::new([dw, dh], dst)
}

fn halve(img: &ColorImage) -> ColorImage {
    let [w, h] = img.size;
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        for x in 0..nw {
            out.push(img.pixels[(y * 2).min(h - 1) * w + (x * 2).min(w - 1)]);
        }
    }
    ColorImage::new([nw, nh], out)
}

/// Full-resolution decode (for 100% zoom). zune-jpeg is faster than
/// jpeg-decoder at full size and writes RGBA directly.
pub fn decode_jpeg_full(data: &[u8], orientation: u8, max_side: u32) -> Result<ColorImage, String> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;

    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), opts);
    let decoded = (|| {
        dec.decode_headers().ok()?;
        let (w, h) = dec.dimensions()?;
        if dec.output_colorspace()? != ColorSpace::RGBA {
            return None;
        }
        // Opaque RGBA has the same bytes as Color32: decode straight into it.
        let mut pixels = vec![Color32::BLACK; w * h];
        dec.decode_into(bytemuck::cast_slice_mut(&mut pixels)).ok()?;
        Some((pixels, w, h))
    })();
    let Some((pixels, w, h)) = decoded else {
        // Unusual JPEGs (CMYK, some progressive files): fall back.
        return decode_jpeg(data, orientation, u32::MAX, u32::MAX, max_side);
    };
    let mut img = orient(pixels, w, h, orientation);
    while img.size[0] > max_side as usize || img.size[1] > max_side as usize {
        img = halve(&img);
    }
    Ok(img)
}
