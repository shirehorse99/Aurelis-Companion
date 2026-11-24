//! Native logo/poster decode + downscale for Aurelis.
//! See `docs/performance/NATIVE_SPEED_WINS_PLAN.md` (WS-IMG).
//!
//! Ports `LogoCacheStore.downscale` (Kotlin `BitmapFactory.decodeByteArray` +
//! `Bitmap.createScaledBitmap` + PNG re-encode) to a pure-Rust path: decode
//! with the `image` crate's pure-Rust codecs (png/jpeg/webp/gif/bmp — no
//! libjpeg/libwebp C dependency), downscale with `fast_image_resize`'s
//! aarch64-NEON SIMD convolution kernels, and re-encode to PNG (preserving
//! alpha, matching the Kotlin behaviour).
//!
//! Behaviour parity with the Kotlin original: decode → if the long edge exceeds
//! `max_px`, scale by `max_px / longEdge` (bilinear) → always re-encode as PNG.
//! Exact pixels differ slightly (different resamplers), which is irrelevant for
//! a desaturated ~38dp channel-rail thumbnail.
//!
//! Owns no long-lived state. Not the app's default decode path — wired behind
//! `ImageDecodeNativeBridge.isAvailable` with the Kotlin path as fallback,
//! matching every other Aurelis native engine's dormant-by-default status.

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ImageFormat, RgbaImage};
use jni::{EnvUnowned};
use jni::objects::{JByteArray, JClass};
use jni::sys::{jbyteArray, jint};
use jni::Env;

/// Bump when the FFI surface changes so Kotlin can refuse a stale .so.
pub const ENGINE_VERSION: i32 = 1;

/// Decode `bytes`, downscale so the long edge is at most `max_px` (bilinear,
/// aspect preserved), and re-encode as PNG. Returns `None` for undecodable
/// input or any encode failure.
pub fn downscale_to_png(bytes: &[u8], max_px: u32) -> Option<Vec<u8>> {
    if bytes.is_empty() || max_px == 0 {
        return None;
    }
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }

    let max_dim = w.max(h);
    let out_img: DynamicImage = if max_dim > max_px {
        let ratio = max_px as f32 / max_dim as f32;
        let nw = ((w as f32 * ratio) as u32).max(1);
        let nh = ((h as f32 * ratio) as u32).max(1);

        let rgba = img.to_rgba8();
        let src = Image::from_vec_u8(w, h, rgba.into_raw(), PixelType::U8x4).ok()?;
        let mut dst = Image::new(nw, nh, PixelType::U8x4);
        let mut resizer = Resizer::new();
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
        resizer.resize(&src, &mut dst, &opts).ok()?;

        let out = RgbaImage::from_raw(nw, nh, dst.into_vec())?;
        DynamicImage::ImageRgba8(out)
    } else {
        img
    };

    let mut out_bytes: Vec<u8> = Vec::new();
    out_img
        .write_to(&mut std::io::Cursor::new(&mut out_bytes), ImageFormat::Png)
        .ok()?;
    Some(out_bytes)
}

// ── JNI exports ──────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_ImageDecodeNativeBridge_nativeDownscaleToPng(
    mut env: EnvUnowned,
    _class: JClass,
    input: JByteArray,
    max_px: jint,
) -> jbyteArray {
    env.with_env(|env| -> std::result::Result<jbyteArray, jni::errors::Error> {
        Ok({
        let bytes = match env.convert_byte_array(&input) {
            Ok(b) => b,
            Err(_) => return Ok(std::ptr::null_mut()),
        };
        match downscale_to_png(&bytes, max_px.max(0) as u32) {
            Some(out) => env
                .byte_array_from_slice(&out)
                .map(|a| a.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => std::ptr::null_mut(),
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_parser_ImageDecodeNativeBridge_nativeEngineVersion(
    _env: EnvUnowned,
    _class: JClass,
) -> jint {
    ENGINE_VERSION
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    // A 4x2 red RGBA PNG, encoded so decode+downscale can round-trip in-process
    // without any Android/JNI dependency.
    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 40, 40, 255]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn downscales_when_long_edge_exceeds_max() {
        let png = tiny_png(400, 200);
        let out = downscale_to_png(&png, 160).expect("should produce png");
        let decoded = image::load_from_memory(&out).expect("valid png out");
        assert_eq!(decoded.width(), 160);
        assert_eq!(decoded.height(), 80);
    }

    #[test]
    fn keeps_dimensions_when_within_max() {
        let png = tiny_png(100, 50);
        let out = downscale_to_png(&png, 160).expect("should produce png");
        let decoded = image::load_from_memory(&out).expect("valid png out");
        assert_eq!(decoded.width(), 100);
        assert_eq!(decoded.height(), 50);
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(downscale_to_png(&[0, 1, 2, 3, 4], 160).is_none());
        assert!(downscale_to_png(&[], 160).is_none());
    }

    #[test]
    fn engine_version_is_stable_constant() {
        assert_eq!(ENGINE_VERSION, 1);
    }
}
