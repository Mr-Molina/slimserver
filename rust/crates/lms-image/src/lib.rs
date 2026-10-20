#![deny(unsafe_op_in_unsafe_fn)]

use lms_cache::DbArtworkStore;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpg,
    Png,
    Gif,
    Bmp,
}

impl ImageFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jpg => "jpg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
        }
    }

    pub fn from_ext(ext: &str) -> Option<Self> {
        let lower = ext.to_ascii_lowercase();
        match lower.as_str() {
            "jpg" | "jpeg" => Some(Self::Jpg),
            "png" => Some(Self::Png),
            "gif" => Some(Self::Gif),
            "bmp" => Some(Self::Bmp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeMode {
    Max,
    Pad,
    Original,
    FitOrKeep,
}

impl ResizeMode {
    pub fn as_char(&self) -> char {
        match self {
            Self::Max => 'm',
            Self::Pad => 'p',
            Self::Original => 'o',
            Self::FitOrKeep => 'F',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeSpec {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub mode: ResizeMode,
    pub bgcolor: Option<u32>,
    pub explicit_format: Option<ImageFormat>,
}

#[derive(Debug, Clone, Copy)]
pub enum InputSource<'a> {
    Path(&'a std::path::Path),
    Bytes(&'a [u8]),
}

#[derive(thiserror::Error, Debug)]
pub enum ImageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Cache error: {0}")]
    Cache(#[from] lms_cache::CacheError),
    #[error("Image decode error: {0}")]
    ImageDecode(String),
    #[error("Image encode error: {0}")]
    ImageEncode(String),
    #[error("Lofty metadata error: {0}")]
    Lofty(String),
    #[error("No embedded artwork found")]
    NoArtworkFound,
    #[error("Unsupported image format")]
    UnsupportedFormat,
    #[error("Invalid resize spec: {0}")]
    InvalidSpec(String),
}

pub fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 4 && &bytes[0..4] == b"\x89PNG" {
        Some(ImageFormat::Png)
    } else if bytes.len() >= 2 && &bytes[0..2] == b"\xFF\xD8" {
        Some(ImageFormat::Jpg)
    } else if bytes.len() >= 3 && &bytes[0..3] == b"GIF" {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 2 && &bytes[0..2] == b"BM" {
        Some(ImageFormat::Bmp)
    } else {
        None
    }
}

pub fn parse_single_spec(spec: &str) -> ResizeSpec {
    let mut remainder = spec;

    let mut explicit_format = None;
    if let Some(dot_idx) = remainder.rfind('.') {
        let ext = &remainder[dot_idx + 1..];
        explicit_format = ImageFormat::from_ext(ext);
        remainder = &remainder[..dot_idx];
    }

    let parts: Vec<&str> = remainder.split('_').collect();
    let mut width = None;
    let mut height = None;
    let mut mode = ResizeMode::Max;
    let mut bgcolor = None;

    if !parts.is_empty() {
        let first = parts[0];
        if let Some(x_idx) = first.find(['x', 'X']) {
            let w_str = &first[..x_idx];
            let h_str = &first[x_idx + 1..];
            if !w_str.is_empty()
                && (w_str == "X" || w_str == "x" || w_str.chars().all(|c| c.is_ascii_digit()))
                && w_str != "X"
                && w_str != "x"
            {
                width = w_str.parse::<u32>().ok();
            }
            if !h_str.is_empty()
                && (h_str == "X" || h_str == "x" || h_str.chars().all(|c| c.is_ascii_digit()))
                && h_str != "X"
                && h_str != "x"
            {
                height = h_str.parse::<u32>().ok();
            }
        }

        let mut part_idx = 1;
        if part_idx < parts.len() {
            let p = parts[part_idx];
            if p.len() == 1 && p.chars().all(|c| c.is_ascii_alphabetic()) {
                if let Some(ch) = p.chars().next() {
                    mode = match ch {
                        'm' | 'M' => ResizeMode::Max,
                        'p' | 'P' => ResizeMode::Pad,
                        'o' | 'O' => ResizeMode::Original,
                        'f' | 'F' => ResizeMode::FitOrKeep,
                        _ => ResizeMode::Max,
                    };
                    part_idx += 1;
                }
            }
        }

        if part_idx < parts.len() {
            let p = parts[part_idx];
            if !p.is_empty() && p.chars().all(|c| c.is_ascii_hexdigit()) {
                if p.len() == 6 || p.len() == 8 {
                    bgcolor = u32::from_str_radix(p, 16).ok().or(Some(0xFFFFFF));
                } else {
                    bgcolor = Some(0xFFFFFF);
                }
            }
        }
    }

    ResizeSpec {
        width,
        height,
        mode,
        bgcolor,
        explicit_format,
    }
}

pub fn extract_embedded_artwork(path: &Path) -> Result<Vec<u8>, ImageError> {
    use lofty::file::TaggedFileExt;
    let tagged_file = lofty::read_from_path(path).map_err(|e| ImageError::Lofty(e.to_string()))?;

    let mut best_pic: Option<(&lofty::picture::Picture, u8)> = None;
    for tag in tagged_file.tags() {
        for pic in tag.pictures() {
            let pic_type = pic.pic_type().as_u8();
            match best_pic {
                Some((_, best_type)) if pic_type < best_type => {
                    best_pic = Some((pic, pic_type));
                }
                None => {
                    best_pic = Some((pic, pic_type));
                }
                _ => {}
            }
        }
    }

    if let Some((pic, _)) = best_pic {
        Ok(pic.data().to_vec())
    } else {
        Err(ImageError::NoArtworkFound)
    }
}

pub fn resize_image(
    input_bytes: &[u8],
    spec: &ResizeSpec,
    faster: bool,
) -> Result<(Vec<u8>, ImageFormat), ImageError> {
    let in_format = sniff_format(input_bytes).ok_or(ImageError::UnsupportedFormat)?;

    // Short-circuit if no width/height specified, and formats match, return original image
    if spec.width.is_none()
        && spec.height.is_none()
        && (spec.explicit_format.is_none() || spec.explicit_format == Some(in_format))
    {
        return Ok((input_bytes.to_vec(), in_format));
    }

    let img = image::load_from_memory(input_bytes)
        .map_err(|e| ImageError::ImageDecode(e.to_string()))?;
    let in_width = img.width();
    let in_height = img.height();

    if in_width == 0 || in_height == 0 {
        return Err(ImageError::ImageDecode("image has 0 dimensions".to_string()));
    }

    // Determine target format
    let mut format = match spec.explicit_format {
        Some(f) => f,
        None => {
            if in_format == ImageFormat::Jpg {
                ImageFormat::Jpg
            } else {
                ImageFormat::Png
            }
        }
    };

    let mut width = spec.width.unwrap_or(in_width);
    let mut height = spec.height.unwrap_or(in_height);

    let filter = if faster {
        image::imageops::FilterType::Triangle
    } else {
        image::imageops::FilterType::Lanczos3
    };

    let output_image: image::DynamicImage = match spec.mode {
        ResizeMode::Max | ResizeMode::Pad => {
            // Bug 17140: switch to png if image will contain any padded space,
            // unless a format was explicitly requested.
            if spec.explicit_format.is_none() && format != ImageFormat::Png {
                let in_aspect = in_height as f64 / in_width as f64;
                let target_aspect = height as f64 / width as f64;
                if (in_aspect - target_aspect).abs() > 1e-4 {
                    format = ImageFormat::Png;
                }
            }

            // Don't upscale if requested size is larger than original in both dimensions
            if width > in_width && height > in_height {
                if (in_height as f64 / in_width as f64) > 1.0 {
                    height = ((height as f64) * (in_width as f64 / width as f64)).round() as u32;
                    width = in_width;
                } else {
                    width = ((width as f64) * (in_height as f64 / height as f64)).round() as u32;
                    height = in_height;
                }
            }

            let canvas_w = width.max(1);
            let canvas_h = height.max(1);

            let scale = f64::min(
                canvas_w as f64 / in_width as f64,
                canvas_h as f64 / in_height as f64,
            );
            let scaled_w = ((in_width as f64 * scale).round() as u32).clamp(1, canvas_w);
            let scaled_h = ((in_height as f64 * scale).round() as u32).clamp(1, canvas_h);

            let scaled = img.resize_exact(scaled_w, scaled_h, filter);

            if scaled_w == canvas_w && scaled_h == canvas_h {
                scaled
            } else {
                let bg_rgba = match spec.bgcolor {
                    Some(bg) => {
                        let r = ((bg >> 16) & 0xFF) as u8;
                        let g = ((bg >> 8) & 0xFF) as u8;
                        let b = (bg & 0xFF) as u8;
                        let a = if bg > 0xFFFFFF {
                            ((bg >> 24) & 0xFF) as u8
                        } else {
                            255
                        };
                        image::Rgba([r, g, b, a])
                    }
                    None => {
                        if format == ImageFormat::Png {
                            image::Rgba([0, 0, 0, 0])
                        } else {
                            image::Rgba([0, 0, 0, 255])
                        }
                    }
                };

                let mut canvas = image::RgbaImage::from_pixel(canvas_w, canvas_h, bg_rgba);
                let offset_x = (canvas_w - scaled_w) / 2;
                let offset_y = (canvas_h - scaled_h) / 2;
                let scaled_rgba = scaled.to_rgba8();
                image::imageops::overlay(&mut canvas, &scaled_rgba, offset_x as i64, offset_y as i64);
                image::DynamicImage::ImageRgba8(canvas)
            }
        }
        ResizeMode::FitOrKeep => {
            // Requested size is bigger than original -> return original size
            let (target_w, target_h) = if width >= in_width && height >= in_height {
                (in_width, in_height)
            } else {
                (width, height)
            };

            let canvas_w = target_w.max(1);
            let canvas_h = target_h.max(1);

            let scale = f64::min(
                canvas_w as f64 / in_width as f64,
                canvas_h as f64 / in_height as f64,
            );
            let scaled_w = ((in_width as f64 * scale).round() as u32).clamp(1, canvas_w);
            let scaled_h = ((in_height as f64 * scale).round() as u32).clamp(1, canvas_h);

            let scaled = img.resize_exact(scaled_w, scaled_h, filter);
            if scaled_w == canvas_w && scaled_h == canvas_h {
                scaled
            } else {
                let bg_rgba = match spec.bgcolor {
                    Some(bg) => {
                        let r = ((bg >> 16) & 0xFF) as u8;
                        let g = ((bg >> 8) & 0xFF) as u8;
                        let b = (bg & 0xFF) as u8;
                        let a = if bg > 0xFFFFFF {
                            ((bg >> 24) & 0xFF) as u8
                        } else {
                            255
                        };
                        image::Rgba([r, g, b, a])
                    }
                    None => {
                        if format == ImageFormat::Png {
                            image::Rgba([0, 0, 0, 0])
                        } else {
                            image::Rgba([0, 0, 0, 255])
                        }
                    }
                };

                let mut canvas = image::RgbaImage::from_pixel(canvas_w, canvas_h, bg_rgba);
                let offset_x = (canvas_w - scaled_w) / 2;
                let offset_y = (canvas_h - scaled_h) / 2;
                let scaled_rgba = scaled.to_rgba8();
                image::imageops::overlay(&mut canvas, &scaled_rgba, offset_x as i64, offset_y as i64);
                image::DynamicImage::ImageRgba8(canvas)
            }
        }
        ResizeMode::Original => {
            // mode 'o': only use width
            if width > in_width && height > in_height {
                if (in_height as f64 / in_width as f64) > 1.0 {
                    width = ((width as f64) * (in_height as f64 / height as f64)).round() as u32;
                } else {
                    width = in_width;
                }
            }

            let target_w = width.max(1);
            let scale = target_w as f64 / in_width as f64;
            let target_h = ((in_height as f64 * scale).round() as u32).max(1);

            img.resize_exact(target_w, target_h, filter)
        }
    };

    let mut out_bytes = Vec::new();
    match format {
        ImageFormat::Jpg => {
            let rgb = output_image.to_rgb8();
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out_bytes, 90);
            encoder
                .encode_image(&rgb)
                .map_err(|e| ImageError::ImageEncode(e.to_string()))?;
        }
        ImageFormat::Png => {
            output_image
                .write_to(
                    &mut std::io::Cursor::new(&mut out_bytes),
                    image::ImageFormat::Png,
                )
                .map_err(|e| ImageError::ImageEncode(e.to_string()))?;
        }
        ImageFormat::Gif => {
            output_image
                .write_to(
                    &mut std::io::Cursor::new(&mut out_bytes),
                    image::ImageFormat::Gif,
                )
                .map_err(|e| ImageError::ImageEncode(e.to_string()))?;
        }
        ImageFormat::Bmp => {
            output_image
                .write_to(
                    &mut std::io::Cursor::new(&mut out_bytes),
                    image::ImageFormat::Bmp,
                )
                .map_err(|e| ImageError::ImageEncode(e.to_string()))?;
        }
    }

    Ok((out_bytes, format))
}

fn format_series_cachekey(cachekey: &str, spec: &str) -> String {
    if let Some(dot_idx) = cachekey.rfind('.') {
        let ext = &cachekey[dot_idx + 1..];
        if (ext.len() == 3 || ext.len() == 4)
            && ext.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return format!("{}_{spec}.{ext}", &cachekey[..dot_idx]);
        }
    }
    format!("{cachekey}{spec}")
}

pub fn gdresize_to_cache(
    file_or_data: InputSource<'_>,
    specs: &[String],
    store: &DbArtworkStore,
    cachekey: &str,
    faster: bool,
) -> Result<(Vec<u8>, ImageFormat), ImageError> {
    if specs.is_empty() {
        return Err(ImageError::InvalidSpec("no specs provided".to_string()));
    }

    let (src_bytes, original_path, mtime) = match file_or_data {
        InputSource::Path(p) => {
            let metadata = std::fs::metadata(p).ok();
            let mtime = metadata
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            let path_str = p.to_string_lossy().to_string();

            let raw = std::fs::read(p)?;
            if sniff_format(&raw).is_some() {
                (raw, path_str, mtime)
            } else {
                match extract_embedded_artwork(p) {
                    Ok(artwork) => (artwork, path_str, mtime),
                    Err(_) => (raw, path_str, mtime),
                }
            }
        }
        InputSource::Bytes(b) => (b.to_vec(), String::new(), 0),
    };

    if specs.len() == 1 {
        let spec_parsed = parse_single_spec(&specs[0]);
        let (out_bytes, out_format) = resize_image(&src_bytes, &spec_parsed, faster)?;

        if !cachekey.is_empty() {
            let record = lms_cache::ArtworkRecord {
                content_type: out_format.as_str().to_string(),
                mtime,
                original_path,
                data: out_bytes.clone(),
            };
            store.set(cachekey, &record)?;
        }
        Ok((out_bytes, out_format))
    } else {
        struct SeriesItem {
            parsed: ResizeSpec,
            width: u32,
            height: u32,
            mode_char: char,
        }

        let mut series: Vec<SeriesItem> = Vec::with_capacity(specs.len());
        for s in specs {
            let parsed = parse_single_spec(s);
            let w = parsed.width.unwrap_or(0);
            let h = parsed.height.unwrap_or(w);
            let m = parsed.mode.as_char();
            series.push(SeriesItem {
                parsed,
                width: w,
                height: h,
                mode_char: m,
            });
        }

        series.sort_by_key(|a| std::cmp::Reverse(a.width));

        let mut current_bytes = src_bytes;
        let mut first_result: Option<(Vec<u8>, ImageFormat)> = None;

        for item in &series {
            let mut spec_to_run = item.parsed.clone();
            if spec_to_run.width.is_none() {
                spec_to_run.width = Some(item.width);
            }
            if spec_to_run.height.is_none() {
                spec_to_run.height = Some(item.height);
            }

            let (resized_bytes, format) = resize_image(&current_bytes, &spec_to_run, faster)?;

            let spec_str = format!("{}x{}_{}", item.width, item.height, item.mode_char);
            let key = format_series_cachekey(cachekey, &spec_str);

            let record = lms_cache::ArtworkRecord {
                content_type: format.as_str().to_string(),
                mtime,
                original_path: original_path.clone(),
                data: resized_bytes.clone(),
            };
            store.set(&key, &record)?;

            if item.width >= 100 && item.height >= 100 {
                current_bytes = resized_bytes.clone();
            }

            if first_result.is_none() {
                first_result = Some((resized_bytes, format));
            }
        }

        first_result.ok_or_else(|| ImageError::InvalidSpec("no series processed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat as ImgFmt, RgbImage};
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Tag, TagExt, TagType};
    use std::io::Cursor;

    fn make_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |_, _| image::Rgb([200, 100, 50])));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg)
            .unwrap();
        buf
    }

    #[test]
    fn test_resize_mode_m_switches_to_png_unless_explicit_jpg() {
        // Non-square 200x100 JPEG resized to 100x100 in mode 'm':
        // Bug 17140: without explicit format, switches to PNG for transparent padding.
        let src = make_jpeg(200, 100);
        let spec_implicit = parse_single_spec("100x100_m");
        let (bytes_png, fmt_png) = resize_image(&src, &spec_implicit, false).unwrap();
        assert_eq!(fmt_png, ImageFormat::Png);
        assert_eq!(&bytes_png[0..4], b"\x89PNG");

        // With explicit .jpg format (t/gdresizer_explicit_format.t), honors jpg!
        let spec_explicit = parse_single_spec("100x100_m.jpg");
        let (bytes_jpg, fmt_jpg) = resize_image(&src, &spec_explicit, false).unwrap();
        assert_eq!(fmt_jpg, ImageFormat::Jpg);
        assert_eq!(&bytes_jpg[0..2], b"\xFF\xD8");
    }

    #[test]
    fn test_resize_series_descending_and_cache_key_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).unwrap();
        let src = make_jpeg(400, 400);
        let specs = vec![
            "100x100_o".to_string(),
            "200x200_o".to_string(),
            "50x50_m".to_string(),
        ];
        gdresize_to_cache(
            InputSource::Bytes(&src),
            &specs,
            &store,
            "music/deadbeef/cover_",
            false,
        )
        .unwrap();
        assert!(store
            .get("music/deadbeef/cover_200x200_o")
            .unwrap()
            .is_some());
        assert!(store
            .get("music/deadbeef/cover_100x100_o")
            .unwrap()
            .is_some());
        assert!(store.get("music/deadbeef/cover_50x50_m").unwrap().is_some());
    }

    #[test]
    fn test_spec_parser_variations() {
        let s1 = parse_single_spec("300x300_m");
        assert_eq!(s1.width, Some(300));
        assert_eq!(s1.height, Some(300));
        assert_eq!(s1.mode, ResizeMode::Max);
        assert_eq!(s1.bgcolor, None);
        assert_eq!(s1.explicit_format, None);

        let s2 = parse_single_spec("150x200_p_ff0000.png");
        assert_eq!(s2.width, Some(150));
        assert_eq!(s2.height, Some(200));
        assert_eq!(s2.mode, ResizeMode::Pad);
        assert_eq!(s2.bgcolor, Some(0xFF0000));
        assert_eq!(s2.explicit_format, Some(ImageFormat::Png));

        let s3 = parse_single_spec("XxX");
        assert_eq!(s3.width, None);
        assert_eq!(s3.height, None);
        assert_eq!(s3.mode, ResizeMode::Max);

        let s4 = parse_single_spec("100xX_o");
        assert_eq!(s4.width, Some(100));
        assert_eq!(s4.height, None);
        assert_eq!(s4.mode, ResizeMode::Original);

        let s5 = parse_single_spec("50x50_F.jpg");
        assert_eq!(s5.width, Some(50));
        assert_eq!(s5.height, Some(50));
        assert_eq!(s5.mode, ResizeMode::FitOrKeep);
        assert_eq!(s5.explicit_format, Some(ImageFormat::Jpg));
    }

    #[test]
    fn test_sniff_format_all_types() {
        assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\n"), Some(ImageFormat::Png));
        assert_eq!(sniff_format(b"\xFF\xD8\xFF\xE0"), Some(ImageFormat::Jpg));
        assert_eq!(sniff_format(b"GIF89a"), Some(ImageFormat::Gif));
        assert_eq!(sniff_format(b"BM"), Some(ImageFormat::Bmp));
        assert_eq!(sniff_format(b"UNKNOWN_MAGIC"), None);
    }

    #[test]
    fn test_short_circuit_on_xxx_matching_format() {
        let src = make_jpeg(50, 50);
        let spec = parse_single_spec("XxX");
        let (out, fmt) = resize_image(&src, &spec, false).unwrap();
        assert_eq!(fmt, ImageFormat::Jpg);
        assert_eq!(out, src);
    }

    #[test]
    fn test_resize_mode_o_preserves_aspect_without_padding() {
        let src = make_jpeg(200, 100);
        let spec = parse_single_spec("100x100_o");
        let (out, fmt) = resize_image(&src, &spec, false).unwrap();
        assert_eq!(fmt, ImageFormat::Jpg);
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.width(), 100);
        assert_eq!(decoded.height(), 50);
    }

    #[test]
    fn test_resize_mode_f_no_upscale() {
        let src = make_jpeg(100, 80);
        let spec = parse_single_spec("200x200_F");
        let (out, _) = resize_image(&src, &spec, false).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.width(), 100);
        assert_eq!(decoded.height(), 80);
    }

    #[test]
    fn test_extract_embedded_artwork_from_tagged_file() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("tagged.mp3");
        std::fs::copy(
            "/home/console-admin/Github/slimserver/HTML/EN/html/silence.mp3",
            &mp3_path,
        )
        .unwrap();

        let mut tag = Tag::new(TagType::Id3v2);
        let fake_art = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        let pic = Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Jpeg),
            None,
            fake_art.clone(),
        );
        tag.push_picture(pic);
        tag.save_to_path(&mp3_path, WriteOptions::default())
            .unwrap();

        let extracted = extract_embedded_artwork(&mp3_path).unwrap();
        assert_eq!(extracted, fake_art);
    }

    #[test]
    fn test_extract_embedded_artwork_selects_lowest_picture_type() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("tagged_multi.mp3");
        std::fs::copy(
            "/home/console-admin/Github/slimserver/HTML/EN/html/silence.mp3",
            &mp3_path,
        )
        .unwrap();

        let mut tag = Tag::new(TagType::Id3v2);
        let pic_media = Picture::new_unchecked(
            PictureType::Media,
            Some(MimeType::Jpeg),
            None,
            vec![6, 6, 6],
        );
        let pic_icon = Picture::new_unchecked(
            PictureType::Icon,
            Some(MimeType::Jpeg),
            None,
            vec![1, 1, 1],
        );
        // Push in reverse order
        tag.push_picture(pic_media);
        tag.push_picture(pic_icon);
        tag.save_to_path(&mp3_path, WriteOptions::default())
            .unwrap();

        let extracted = extract_embedded_artwork(&mp3_path).unwrap();
        assert_eq!(extracted, vec![1, 1, 1]);
    }

    #[test]
    fn test_gdresize_to_cache_single_spec_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("source.jpg");
        let src = make_jpeg(200, 200);
        std::fs::write(&img_path, &src).unwrap();

        let store = lms_cache::DbArtworkStore::open(dir.path(), false).unwrap();
        let (out, fmt) = gdresize_to_cache(
            InputSource::Path(&img_path),
            &["100x100_m".to_string()],
            &store,
            "cached/test_key",
            false,
        )
        .unwrap();
        assert_eq!(fmt, ImageFormat::Jpg);
        assert!(!out.is_empty());
        let cached = store.get("cached/test_key").unwrap().unwrap();
        assert_eq!(cached.data, out);
        assert_eq!(cached.content_type, "jpg");
        assert_eq!(cached.original_path, img_path.to_string_lossy().to_string());
    }
}
