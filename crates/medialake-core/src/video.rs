//! Video frame extraction with Windows Media Foundation (nothing to bundle).

use std::path::Path;

pub const MAX_FRAMES: usize = 8;

pub struct PreparedVideo {
    /// JPEG frames (evenly spaced), downscaled.
    pub frames: Vec<Vec<u8>>,
    /// Hash over the (downscaled) decoded frames.
    pub hash: Vec<u8>,
}

#[cfg(windows)]
pub fn prepare_video(path: &Path) -> Result<PreparedVideo, String> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::core::{GUID, HSTRING};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("MFStartup: {e}"))?;
        let result = (|| -> Result<PreparedVideo, String> {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2).map_err(|e| e.to_string())?;
            let attrs = attrs.ok_or("no attributes")?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(|e| e.to_string())?;
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs).map_err(|e| format!("cannot open video: {e}"))?;
            let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

            // ask for decoded RGB32 frames
            let mt = MFCreateMediaType().map_err(|e| e.to_string())?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(|e| e.to_string())?;
            reader.SetCurrentMediaType(video, None, &mt).map_err(|e| format!("no RGB output: {e}"))?;
            let cur = reader.GetCurrentMediaType(video).map_err(|e| e.to_string())?;
            let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| e.to_string())?;
            let (w, h) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
            let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|v| v as i32).unwrap_or((w * 4) as i32);

            let dur = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .and_then(|v| i64::try_from(&v).ok().or_else(|| u64::try_from(&v).ok().map(|x| x as i64)))
                .unwrap_or(0); // 100 ns units

            let n = MAX_FRAMES;
            let mut frames = Vec::new();
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            for i in 0..n {
                let t = if dur > 0 { dur * (2 * i as i64 + 1) / (2 * n as i64) } else { 0 };
                if dur > 0 {
                    let pv = PROPVARIANT::from(t);
                    let _ = reader.SetCurrentPosition(&GUID::zeroed(), &pv);
                }
                // after a seek, the first sample may be a few decoder frames before the target; take the first one
                let mut got = None;
                for _ in 0..30 {
                    let (mut flags, mut _ts) = (0u32, 0i64);
                    let mut sample: Option<IMFSample> = None;
                    reader
                        .ReadSample(video, 0, None, Some(&mut flags), Some(&mut _ts), Some(&mut sample))
                        .map_err(|e| e.to_string())?;
                    if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                        break;
                    }
                    if let Some(s) = sample {
                        got = Some(s);
                        break;
                    }
                }
                let Some(sample) = got else { continue };
                let buf = sample.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
                let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
                buf.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| e.to_string())?;
                let data = std::slice::from_raw_parts(ptr, len as usize);
                let mut rgb = image::RgbImage::new(w, h);
                let abs = stride.unsigned_abs() as usize;
                for y in 0..h as usize {
                    let row = if stride < 0 { h as usize - 1 - y } else { y };
                    let line = &data[row * abs..];
                    for x in 0..w as usize {
                        let p = &line[x * 4..x * 4 + 4]; // BGRA
                        rgb.put_pixel(x as u32, y as u32, image::Rgb([p[2], p[1], p[0]]));
                    }
                }
                let _ = buf.Unlock();
                let prepared = crate::extract::prepare_decoded(image::DynamicImage::ImageRgb8(rgb))?;
                sha2::Digest::update(&mut hasher, &prepared.pixel_hash);
                frames.push(prepared.jpeg);
            }
            if frames.is_empty() {
                return Err("no frames could be decoded".into());
            }
            Ok(PreparedVideo { frames, hash: sha2::Digest::finalize(hasher).to_vec() })
        })();
        let _ = MFShutdown();
        result
    }
}

#[cfg(not(windows))]
pub fn prepare_video(_path: &Path) -> Result<PreparedVideo, String> {
    Err("video frame extraction needs Windows".into())
}
