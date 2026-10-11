//! macOS media decoding: HEIC and friends through ImageIO, video frames through AVFoundation.
//! Nothing is bundled; all of it ships with the OS.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFData, CFNumber, CFRetained, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, CGContext, CGImage};
use objc2_image_io::CGImageSource;

unsafe extern "C-unwind" {
    // not exposed by objc2-core-graphics
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const CGColorSpace,
        bitmap_info: u32,
    ) -> *mut CGContext;
}

/// kCGImageAlphaNoneSkipLast
const NONE_SKIP_LAST: u32 = 5;

/// Draws a `CGImage` into an RGB bitmap (transparent areas become white).
pub fn cgimage_to_rgb(img: &CGImage) -> Result<image::RgbImage, String> {
    let (w, h) = (CGImage::width(Some(img)), CGImage::height(Some(img)));
    if w == 0 || h == 0 || w > 32_768 || h > 32_768 {
        return Err(format!("unsupported image size {w}x{h}"));
    }
    let space = CGColorSpace::new_device_rgb().ok_or("no colour space")?;
    let mut buf = vec![0u8; w * h * 4];
    // SAFETY: `buf` outlives the context, which is released before this function returns.
    let ctx = unsafe {
        let raw = CGBitmapContextCreate(buf.as_mut_ptr().cast(), w, h, 8, w * 4, &*space, NONE_SKIP_LAST);
        CFRetained::from_raw(NonNull::new(raw).ok_or("cannot create a bitmap context")?)
    };
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as CGFloat, h as CGFloat));
    CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&ctx), rect);
    CGContext::draw_image(Some(&ctx), rect, Some(img));
    drop(ctx);
    let mut rgb = image::RgbImage::new(w as u32, h as u32);
    for (px, src) in rgb.pixels_mut().zip(buf.chunks_exact(4)) {
        *px = image::Rgb([src[0], src[1], src[2]]);
    }
    Ok(rgb)
}

/// Decodes any format ImageIO knows (HEIC/HEIF in particular), applying the EXIF orientation and
/// limiting the longer side to `max_side`.
pub fn decode_image(bytes: &[u8], max_side: u32) -> Result<image::DynamicImage, String> {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};
    let data = CFData::from_bytes(bytes);
    // SAFETY: plain ImageIO calls on retained objects; the option keys are the documented constants.
    unsafe {
        let src = CGImageSource::with_data(&data, None).ok_or("not a decodable image")?;
        let opts: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[
                objc2_image_io::kCGImageSourceCreateThumbnailFromImageAlways,
                objc2_image_io::kCGImageSourceCreateThumbnailWithTransform,
                objc2_image_io::kCGImageSourceThumbnailMaxPixelSize,
            ],
            &[
                CFBoolean::new(true).as_ref(),
                CFBoolean::new(true).as_ref(),
                CFNumber::new_i64(max_side as i64).as_ref(),
            ],
        );
        let img = src.thumbnail_at_index(0, Some(opts.as_opaque())).ok_or("ImageIO could not decode the image")?;
        Ok(image::DynamicImage::ImageRgb8(cgimage_to_rgb(&img)?))
    }
}
