#![deny(unsafe_op_in_unsafe_fn)]

use image::{DynamicImage, ImageReader, Limits};
use lms_cache::{ArtworkRecord, DbArtworkStore};
use lofty::config::{apply_global_options, GlobalOptions, ParseOptions};
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use std::borrow::Cow;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const MAX_ARTWORK_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 64_000_000;
const MAX_DECODE_DIMENSION: u32 = 16_384;
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FLAC_METADATA_BLOCKS: usize = 128;
const MAX_ID3_FRAMES: usize = 512;

/// Embedded artwork bytes and container metadata extracted from a media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedArtwork {
    /// Raw encoded image payload.
    pub data: Vec<u8>,
    /// Picture type identifier (for example, 3 = Front Cover).
    pub picture_type: u8,
    /// Optional MIME type reported by the tag.
    pub mime_type: Option<String>,
}

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

#[derive(Debug)]
pub enum InputSource<'a> {
    Path(&'a std::path::Path),
    Bytes(&'a [u8]),
    OpenedFile {
        file: std::fs::File,
        original_path: PathBuf,
        mtime: i64,
    },
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
    #[error("Invalid arguments")]
    InvalidArguments,
}

fn is_valid_bmp_header(bytes: &[u8]) -> bool {
    if bytes.len() < 14 || !bytes.starts_with(b"BM") {
        return false;
    }
    if bytes[6..10] != [0, 0, 0, 0] {
        return false;
    }
    let file_size = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let off_bits = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
    file_size >= 26
        && (file_size as usize) <= MAX_INPUT_BYTES
        && off_bits >= 26
        && off_bits <= file_size
}

pub fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some(ImageFormat::Jpg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if is_valid_bmp_header(bytes) {
        Some(ImageFormat::Bmp)
    } else {
        None
    }
}

fn parse_dim_component(dim: &str) -> Result<Option<u32>, ImageError> {
    if dim.is_empty() || dim == "X" || dim == "x" {
        return Ok(None);
    }
    if dim.chars().all(|c| c.is_ascii_digit()) {
        let value = dim
            .parse::<u32>()
            .map_err(|_| ImageError::InvalidSpec(format!("dimension overflow: {dim}")))?;
        Ok((value > 0).then_some(value))
    } else {
        Err(ImageError::InvalidSpec(format!(
            "invalid dimension component: {dim}"
        )))
    }
}

/// Parse and strictly validate a resize specification string.
pub fn try_parse_single_spec(spec: &str) -> Result<ResizeSpec, ImageError> {
    if spec.is_empty() {
        return Err(ImageError::InvalidSpec("empty resize spec".to_string()));
    }
    let mut remainder = spec;

    let mut explicit_format = None;
    if let Some((prefix, ext)) = remainder.rsplit_once('.') {
        let lower_ext = ext.to_ascii_lowercase();
        let parsed_fmt = ImageFormat::from_ext(&lower_ext)
            .ok_or_else(|| ImageError::InvalidSpec(format!("unsupported output format: {ext}")))?;
        explicit_format = Some(parsed_fmt);
        remainder = prefix;
    }

    let parts: Vec<&str> = remainder.split('_').collect();
    let first = parts
        .first()
        .copied()
        .ok_or_else(|| ImageError::InvalidSpec(format!("missing dimensions: {spec}")))?;
    let (w_str, h_str) = first
        .split_once('x')
        .or_else(|| first.split_once('X'))
        .ok_or_else(|| ImageError::InvalidSpec(format!("missing WxH separator: {spec}")))?;
    let width = parse_dim_component(w_str)?;
    let height = parse_dim_component(h_str)?;

    let mut mode = ResizeMode::Max;
    let mut bgcolor = None;
    let mut part_idx = 1;

    if let Some(&p) = parts.get(part_idx) {
        if p.len() == 1 {
            let ch = p.chars().next().ok_or_else(|| {
                ImageError::InvalidSpec(format!("invalid resize mode in spec: {spec}"))
            })?;
            mode = match ch {
                'm' | 'M' => ResizeMode::Max,
                'p' | 'P' => ResizeMode::Pad,
                'o' | 'O' => ResizeMode::Original,
                'f' | 'F' => ResizeMode::FitOrKeep,
                _ => {
                    return Err(ImageError::InvalidSpec(format!("invalid resize mode: {p}")));
                }
            };
            part_idx += 1;
        }
    }

    if let Some(&p) = parts.get(part_idx) {
        if (p.len() == 6 || p.len() == 8) && p.chars().all(|c| c.is_ascii_hexdigit()) {
            let parsed = u32::from_str_radix(p, 16).map_err(|_| {
                ImageError::InvalidSpec(format!("invalid background color hex: {p}"))
            })?;
            bgcolor = Some(if p.len() == 6 {
                0xFF00_0000 | parsed
            } else {
                parsed
            });
            part_idx += 1;
        } else {
            return Err(ImageError::InvalidSpec(format!(
                "invalid background color hex: {p}"
            )));
        }
    }

    if parts.len() > part_idx {
        return Err(ImageError::InvalidSpec(format!(
            "unexpected trailing segment in spec: {spec}"
        )));
    }

    Ok(ResizeSpec {
        width,
        height,
        mode,
        bgcolor,
        explicit_format,
    })
}

pub fn parse_single_spec(spec: &str) -> ResizeSpec {
    try_parse_single_spec(spec).unwrap_or(ResizeSpec {
        width: None,
        height: None,
        mode: ResizeMode::Max,
        bgcolor: None,
        explicit_format: None,
    })
}

struct BoundedReader<'a, R> {
    inner: &'a mut R,
    offset: u64,
    pos: u64,
    len: u64,
}

impl<'a, R: Seek> BoundedReader<'a, R> {
    fn new(inner: &'a mut R, offset: u64, len: u64) -> std::io::Result<Self> {
        offset.checked_add(len).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bounded stream range overflow",
            )
        })?;
        inner.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            inner,
            offset,
            pos: 0,
            len,
        })
    }
}

impl<R: Read> Read for BoundedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos);
        if remaining == 0 {
            return Ok(0);
        }
        let max_len = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let read_bytes = self.inner.read(&mut buf[..max_len])?;
        self.pos = self.pos.saturating_add(read_bytes as u64);
        Ok(read_bytes)
    }
}

impl<R: Seek> Seek for BoundedReader<'_, R> {
    fn seek(&mut self, style: SeekFrom) -> std::io::Result<u64> {
        let next = match style {
            SeekFrom::Start(delta) => Some(delta),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        }
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start of bounded stream",
            )
        })?;
        if next > self.len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek past end of bounded stream",
            ));
        }
        let abs = self.offset + next;
        self.inner.seek(SeekFrom::Start(abs))?;
        self.pos = next;
        Ok(next)
    }
}

fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize, ImageError> {
    let mut total = 0;
    while total < buf.len() {
        let n = reader.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn decode_syncsafe_u32(bytes: [u8; 4]) -> Option<u32> {
    if bytes.iter().any(|&b| (b & 0x80) != 0) {
        return None;
    }
    Some(
        ((bytes[0] as u32) << 21)
            | ((bytes[1] as u32) << 14)
            | ((bytes[2] as u32) << 7)
            | (bytes[3] as u32),
    )
}

fn read_be_u32_at(buf: &[u8], offset: usize) -> Option<(u32, usize)> {
    let end = offset.checked_add(4)?;
    let slice = buf.get(offset..end)?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some((u32::from_be_bytes(arr), end))
}

fn parse_flac_picture_block(buf: &[u8]) -> Result<Option<ExtractedArtwork>, ImageError> {
    let malformed = || ImageError::Lofty("malformed FLAC PICTURE block".to_string());

    let (pic_type_raw, offset) = read_be_u32_at(buf, 0).ok_or_else(malformed)?;
    let (mime_len_u32, offset) = read_be_u32_at(buf, offset).ok_or_else(malformed)?;
    let mime_len = usize::try_from(mime_len_u32).map_err(|_| malformed())?;
    let mime_end = offset.checked_add(mime_len).ok_or_else(malformed)?;
    let mime_bytes = buf.get(offset..mime_end).ok_or_else(malformed)?;
    let mime_type = std::str::from_utf8(mime_bytes)
        .ok()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if mime_type.as_deref() == Some("-->") {
        return Err(ImageError::Lofty(
            "FLAC URI picture blocks are not supported".to_string(),
        ));
    }

    let (desc_len_u32, offset) = read_be_u32_at(buf, mime_end).ok_or_else(malformed)?;
    let desc_len = usize::try_from(desc_len_u32).map_err(|_| malformed())?;
    let desc_end = offset.checked_add(desc_len).ok_or_else(malformed)?;

    // Skip width, height, color depth, and indexed colors (16 bytes).
    let data_len_offset = desc_end.checked_add(16).ok_or_else(malformed)?;
    let (data_len_u32, data_start) = read_be_u32_at(buf, data_len_offset).ok_or_else(malformed)?;
    let data_len = usize::try_from(data_len_u32).map_err(|_| malformed())?;
    if data_len > MAX_ARTWORK_BYTES {
        return Err(ImageError::Lofty(
            "FLAC embedded artwork exceeds 32 MiB limit".to_string(),
        ));
    }
    let data_end = data_start.checked_add(data_len).ok_or_else(malformed)?;
    let data = buf.get(data_start..data_end).ok_or_else(malformed)?;
    if data.is_empty() {
        return Ok(None);
    }

    Ok(Some(ExtractedArtwork {
        data: data.to_vec(),
        picture_type: u8::try_from(pic_type_raw).unwrap_or(u8::MAX),
        mime_type,
    }))
}

fn extract_flac_artwork<R: Read + Seek>(
    reader: &mut R,
    len: u64,
) -> Result<Option<ExtractedArtwork>, ImageError> {
    let mut cursor: u64 = 4;
    let mut best_pic: Option<ExtractedArtwork> = None;

    for _ in 0..MAX_FLAC_METADATA_BLOCKS {
        if cursor.checked_add(4).is_none_or(|end| end > len) {
            return Err(ImageError::Lofty(
                "truncated FLAC metadata block header".to_string(),
            ));
        }
        let mut hdr = [0u8; 4];
        reader.read_exact(&mut hdr)?;
        cursor += 4;

        let is_last = (hdr[0] & 0x80) != 0;
        let block_type = hdr[0] & 0x7f;
        let block_len = u32::from_be_bytes([0, hdr[1], hdr[2], hdr[3]]) as u64;

        let next_cursor = cursor
            .checked_add(block_len)
            .filter(|&end| end <= len)
            .ok_or_else(|| {
                ImageError::Lofty("FLAC metadata block exceeds file length".to_string())
            })?;

        if block_type == 6 {
            if block_len > MAX_ARTWORK_BYTES as u64 {
                return Err(ImageError::Lofty(
                    "FLAC PICTURE block exceeds 32 MiB limit".to_string(),
                ));
            }
            let block_usize = usize::try_from(block_len)
                .map_err(|_| ImageError::Lofty("FLAC PICTURE block length overflow".to_string()))?;
            let mut buf = vec![0u8; block_usize];
            reader.read_exact(&mut buf)?;
            if let Some(pic) = parse_flac_picture_block(&buf)? {
                match &best_pic {
                    Some(best) if pic.picture_type < best.picture_type => {
                        best_pic = Some(pic);
                    }
                    None => {
                        best_pic = Some(pic);
                    }
                    _ => {}
                }
            }
        } else if block_len > 0 {
            // Seek past VORBIS_COMMENTS (type 4) and other blocks directly.
            let skip = i64::try_from(block_len)
                .map_err(|_| ImageError::Lofty("FLAC block skip overflow".to_string()))?;
            reader.seek(SeekFrom::Current(skip))?;
        }

        cursor = next_cursor;
        if is_last {
            break;
        }
    }

    Ok(best_pic)
}

fn validate_id3v2_preflight<R: Read + Seek>(reader: &mut R, len: u64) -> Result<(), ImageError> {
    if len < 10 {
        return Err(ImageError::Lofty("truncated ID3v2 header".to_string()));
    }
    reader.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; 10];
    reader.read_exact(&mut header)?;
    if &header[0..3] != b"ID3" {
        return Err(ImageError::Lofty("invalid ID3v2 magic".to_string()));
    }

    let major = header[3];
    if !(2..=4).contains(&major) {
        return Err(ImageError::Lofty(format!(
            "unsupported ID3v2 version: 2.{major}"
        )));
    }
    let flags = header[5];
    if major == 2 && (flags & 0x40) != 0 {
        return Err(ImageError::Lofty(
            "compressed ID3v2.2 tags are not supported".to_string(),
        ));
    }

    let tag_size = decode_syncsafe_u32([header[6], header[7], header[8], header[9]])
        .ok_or_else(|| ImageError::Lofty("invalid ID3v2 syncsafe tag size".to_string()))?
        as u64;
    if tag_size > MAX_ARTWORK_BYTES as u64 {
        return Err(ImageError::Lofty(
            "ID3v2 tag size exceeds 32 MiB limit".to_string(),
        ));
    }
    if 10u64.checked_add(tag_size).is_none_or(|end| end > len) {
        return Err(ImageError::Lofty(
            "ID3v2 tag size exceeds file length".to_string(),
        ));
    }

    let mut remaining = tag_size;
    if (major == 3 || major == 4) && (flags & 0x40) != 0 {
        if remaining < 4 {
            return Err(ImageError::Lofty(
                "truncated ID3v2 extended header".to_string(),
            ));
        }
        let mut ext_hdr = [0u8; 4];
        reader.read_exact(&mut ext_hdr)?;
        remaining -= 4;
        let skip = if major == 3 {
            u32::from_be_bytes(ext_hdr) as u64
        } else {
            let total = decode_syncsafe_u32(ext_hdr).ok_or_else(|| {
                ImageError::Lofty("invalid ID3v2.4 extended header size".to_string())
            })? as u64;
            total.checked_sub(4).ok_or_else(|| {
                ImageError::Lofty("invalid ID3v2.4 extended header size".to_string())
            })?
        };
        if skip > remaining {
            return Err(ImageError::Lofty(
                "ID3v2 extended header exceeds tag size".to_string(),
            ));
        }
        if skip > 0 {
            let skip_i64 = i64::try_from(skip)
                .map_err(|_| ImageError::Lofty("ID3v2 skip overflow".to_string()))?;
            reader.seek(SeekFrom::Current(skip_i64))?;
            remaining -= skip;
        }
    }

    let frame_hdr_len: u64 = if major == 2 { 6 } else { 10 };
    let mut total_pic_bytes: u64 = 0;

    for _ in 0..MAX_ID3_FRAMES {
        if remaining < frame_hdr_len {
            return Ok(());
        }
        let (frame_len, is_pic, is_protected) = if major == 2 {
            let mut fh = [0u8; 6];
            reader.read_exact(&mut fh)?;
            if fh[0] == 0 {
                return Ok(());
            }
            let size = u32::from_be_bytes([0, fh[3], fh[4], fh[5]]) as u64;
            (size, &fh[0..3] == b"PIC", false)
        } else {
            let mut fh = [0u8; 10];
            reader.read_exact(&mut fh)?;
            if fh[0] == 0 {
                return Ok(());
            }
            let size = if major == 3 {
                u32::from_be_bytes([fh[4], fh[5], fh[6], fh[7]]) as u64
            } else {
                decode_syncsafe_u32([fh[4], fh[5], fh[6], fh[7]]).ok_or_else(|| {
                    ImageError::Lofty("invalid ID3v2.4 syncsafe frame size".to_string())
                })? as u64
            };
            let fmt_flags = fh[9];
            let protected = if major == 3 {
                (fmt_flags & 0xC0) != 0
            } else {
                (fmt_flags & 0x0C) != 0
            };
            (size, &fh[0..4] == b"APIC", protected)
        };

        remaining -= frame_hdr_len;
        if frame_len > MAX_ARTWORK_BYTES as u64 || frame_len > remaining {
            return Err(ImageError::Lofty(
                "ID3v2 frame size exceeds limit or remaining tag bytes".to_string(),
            ));
        }

        if is_pic {
            if is_protected {
                return Err(ImageError::Lofty(
                    "compressed or encrypted APIC frame is not supported".to_string(),
                ));
            }
            total_pic_bytes = total_pic_bytes
                .checked_add(frame_len)
                .filter(|&sum| sum <= MAX_ARTWORK_BYTES as u64)
                .ok_or_else(|| {
                    ImageError::Lofty("cumulative ID3v2 picture bytes exceed 32 MiB".to_string())
                })?;
        }

        if frame_len > 0 {
            let skip = i64::try_from(frame_len)
                .map_err(|_| ImageError::Lofty("ID3v2 frame skip overflow".to_string()))?;
            reader.seek(SeekFrom::Current(skip))?;
            remaining -= frame_len;
        }
    }

    if remaining >= frame_hdr_len {
        let mut peek = [0u8; 1];
        reader.read_exact(&mut peek)?;
        if peek[0] != 0 {
            return Err(ImageError::Lofty(
                "ID3v2 frame count exceeds 512-frame limit".to_string(),
            ));
        }
    }

    Ok(())
}

fn extract_with_lofty<R: Read + Seek>(
    reader: &mut R,
    len: u64,
) -> Result<Option<ExtractedArtwork>, ImageError> {
    reader.seek(SeekFrom::Start(0))?;
    let cap = len.min(MAX_INPUT_BYTES as u64);
    let bounded = BoundedReader::new(reader, 0, cap)?;

    apply_global_options(GlobalOptions::new().allocation_limit(MAX_ARTWORK_BYTES));
    let parse_options = ParseOptions::new().read_properties(false);
    let tagged_file = Probe::new(bounded)
        .guess_file_type()
        .map_err(|e| ImageError::Lofty(e.to_string()))?
        .options(parse_options)
        .read()
        .map_err(|e| ImageError::Lofty(e.to_string()))?;

    let mut best_pic: Option<(&lofty::picture::Picture, u8)> = None;
    let mut pic_count = 0usize;
    for tag in tagged_file.tags() {
        for pic in tag.pictures() {
            if pic_count >= MAX_FLAC_METADATA_BLOCKS {
                break;
            }
            pic_count += 1;
            if pic.mime_type().is_some_and(|m| m.as_str() == "-->") {
                return Err(ImageError::Lofty(
                    "URI picture blocks are not supported".to_string(),
                ));
            }
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
        if pic_count >= MAX_FLAC_METADATA_BLOCKS {
            break;
        }
    }

    if let Some((pic, pic_type)) = best_pic {
        let data = pic.data();
        if data.len() > MAX_ARTWORK_BYTES {
            return Err(ImageError::Lofty(
                "embedded artwork exceeds 32 MiB limit".to_string(),
            ));
        }
        Ok(Some(ExtractedArtwork {
            data: data.to_vec(),
            picture_type: pic_type,
            mime_type: pic.mime_type().map(|m| m.as_str().to_string()),
        }))
    } else {
        Ok(None)
    }
}

/// Extract embedded artwork from a seekable reader with pre-flight bounds.
pub fn extract_embedded_artwork_from_reader<R: Read + Seek>(
    reader: &mut R,
    len: u64,
) -> Result<Option<ExtractedArtwork>, ImageError> {
    if len > MAX_INPUT_BYTES as u64 {
        return Err(ImageError::InvalidArguments);
    }

    reader.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    let n = read_up_to(reader, &mut magic)?;

    if n >= 4 && &magic == b"fLaC" {
        return extract_flac_artwork(reader, len);
    }
    if n >= 3 && &magic[..3] == b"ID3" {
        validate_id3v2_preflight(reader, len)?;
    }

    extract_with_lofty(reader, len)
}

#[cfg(unix)]
fn unix_open_flags() -> i32 {
    libc::O_NONBLOCK | libc::O_NOFOLLOW
}

fn validate_regular_file_metadata(meta: &Metadata) -> Result<(), ImageError> {
    if !meta.is_file() || meta.len() > MAX_INPUT_BYTES as u64 {
        return Err(ImageError::InvalidArguments);
    }
    Ok(())
}

fn open_regular_file(path: &Path) -> Result<(File, Metadata), ImageError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(unix_open_flags());
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    validate_regular_file_metadata(&meta)?;
    Ok((file, meta))
}

/// Extract embedded artwork from a regular audio file path.
pub fn extract_embedded_artwork(path: &Path) -> Result<Vec<u8>, ImageError> {
    let (mut file, meta) = open_regular_file(path)?;

    match extract_embedded_artwork_from_reader(&mut file, meta.len())? {
        Some(art) => Ok(art.data),
        None => Err(ImageError::NoArtworkFound),
    }
}

fn resolve_bg_rgba(bgcolor: Option<u32>, format: ImageFormat) -> image::Rgba<u8> {
    match bgcolor {
        Some(bg) => {
            let a = ((bg >> 24) & 0xFF) as u8;
            let r = ((bg >> 16) & 0xFF) as u8;
            let g = ((bg >> 8) & 0xFF) as u8;
            let b = (bg & 0xFF) as u8;
            image::Rgba([r, g, b, a])
        }
        None => {
            if format == ImageFormat::Png {
                image::Rgba([0, 0, 0, 0])
            } else {
                image::Rgba([0, 0, 0, 255])
            }
        }
    }
}

fn compose_on_canvas(
    scaled: DynamicImage,
    canvas_w: u32,
    canvas_h: u32,
    spec: &ResizeSpec,
    format: ImageFormat,
) -> DynamicImage {
    let scaled_w = scaled.width();
    let scaled_h = scaled.height();
    let needs_pad = scaled_w != canvas_w || scaled_h != canvas_h;
    let needs_jpg_alpha = format == ImageFormat::Jpg && scaled.color().has_alpha();

    if !needs_pad && !needs_jpg_alpha {
        return scaled;
    }

    let mut bg_rgba = resolve_bg_rgba(spec.bgcolor, format);
    if format == ImageFormat::Jpg {
        // JPEG has no alpha channel; composite onto an opaque background.
        bg_rgba.0[3] = 255;
    }
    let offset_x = i64::from(canvas_w.saturating_sub(scaled_w) / 2);
    let offset_y = i64::from(canvas_h.saturating_sub(scaled_h) / 2);

    if format == ImageFormat::Jpg && !scaled.color().has_alpha() {
        let [r, g, b, _] = bg_rgba.0;
        let mut canvas = image::RgbImage::from_pixel(canvas_w, canvas_h, image::Rgb([r, g, b]));
        image::imageops::overlay(&mut canvas, &scaled.to_rgb8(), offset_x, offset_y);
        DynamicImage::ImageRgb8(canvas)
    } else {
        let mut canvas = image::RgbaImage::from_pixel(canvas_w, canvas_h, bg_rgba);
        let scaled_rgba = scaled.to_rgba8();
        image::imageops::overlay(&mut canvas, &scaled_rgba, offset_x, offset_y);
        if format == ImageFormat::Jpg {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(canvas).to_rgb8())
        } else {
            DynamicImage::ImageRgba8(canvas)
        }
    }
}

fn resize_with_downsample(
    img: &image::DynamicImage,
    scaled_w: u32,
    scaled_h: u32,
    filter: image::imageops::FilterType,
) -> image::DynamicImage {
    let in_width = img.width();
    let in_height = img.height();
    if filter == image::imageops::FilterType::CatmullRom
        && in_width >= scaled_w.saturating_mul(4)
        && in_height >= scaled_h.saturating_mul(4)
    {
        let intermediate = img.resize_exact(
            scaled_w.saturating_mul(2),
            scaled_h.saturating_mul(2),
            image::imageops::FilterType::Triangle,
        );
        intermediate.resize_exact(scaled_w, scaled_h, image::imageops::FilterType::CatmullRom)
    } else {
        img.resize_exact(scaled_w, scaled_h, filter)
    }
}

pub fn decode_checked(bytes: &[u8]) -> Result<(DynamicImage, ImageFormat), ImageError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ImageError::InvalidArguments);
    }

    let in_format = sniff_format(bytes).ok_or(ImageError::UnsupportedFormat)?;

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);

    let mut dim_reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::ImageDecode(e.to_string()))?;
    dim_reader.limits(limits.clone());
    let (dim_w, dim_h) = dim_reader
        .into_dimensions()
        .map_err(|e| ImageError::ImageDecode(e.to_string()))?;
    if dim_w == 0 || dim_h == 0 || dim_w > MAX_DECODE_DIMENSION || dim_h > MAX_DECODE_DIMENSION {
        return Err(ImageError::InvalidArguments);
    }
    if (dim_w as u64).saturating_mul(dim_h as u64) > MAX_PIXELS {
        return Err(ImageError::InvalidArguments);
    }

    let mut decode_reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::ImageDecode(e.to_string()))?;
    decode_reader.limits(limits);
    let img = decode_reader
        .decode()
        .map_err(|e| ImageError::ImageDecode(e.to_string()))?;
    Ok((img, in_format))
}

fn resize_decoded_image(
    img: &DynamicImage,
    in_format: ImageFormat,
    spec: &ResizeSpec,
    faster: bool,
) -> Result<(DynamicImage, Vec<u8>, ImageFormat), ImageError> {
    if let Some(w) = spec.width {
        if w > 8_192 {
            return Err(ImageError::InvalidArguments);
        }
    }
    if let Some(h) = spec.height {
        if h > 8_192 {
            return Err(ImageError::InvalidArguments);
        }
    }

    let in_width = img.width();
    let in_height = img.height();
    if in_width == 0 || in_height == 0 {
        return Err(ImageError::InvalidArguments);
    }

    // Determine target format.
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

    let req_w = spec.width.filter(|&w| w > 0);
    let req_h = spec.height.filter(|&h| h > 0);
    let (mut width, mut height) = match (req_w, req_h) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let scaled_h = ((in_height as f64) * (w as f64 / in_width as f64))
                .round()
                .max(1.0) as u32;
            (w, scaled_h)
        }
        (None, Some(h)) => {
            let scaled_w = ((in_width as f64) * (h as f64 / in_height as f64))
                .round()
                .max(1.0) as u32;
            (scaled_w, h)
        }
        (None, None) => (in_width, in_height),
    };

    let filter = if faster {
        image::imageops::FilterType::Triangle
    } else {
        image::imageops::FilterType::CatmullRom
    };

    let output_image: DynamicImage = match spec.mode {
        ResizeMode::Max | ResizeMode::Pad => {
            // Do not upscale if requested size exceeds original on both axes.
            if width > in_width && height > in_height {
                if (in_height as f64 / in_width as f64) > 1.0 {
                    height = ((height as f64) * (in_width as f64 / width as f64))
                        .round()
                        .max(1.0) as u32;
                    width = in_width;
                } else {
                    width = ((width as f64) * (in_height as f64 / height as f64))
                        .round()
                        .max(1.0) as u32;
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

            // Bug 17140: switch to png if image will contain padded space,
            // unless a format was explicitly requested.
            if spec.explicit_format.is_none()
                && format != ImageFormat::Png
                && (scaled_w != canvas_w || scaled_h != canvas_h)
            {
                format = ImageFormat::Png;
            }

            let scaled = resize_with_downsample(img, scaled_w, scaled_h, filter);
            compose_on_canvas(scaled, canvas_w, canvas_h, spec, format)
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

            let scaled = resize_with_downsample(img, scaled_w, scaled_h, filter);
            compose_on_canvas(scaled, canvas_w, canvas_h, spec, format)
        }
        ResizeMode::Original => {
            // mode 'o': only use width
            if width > in_width && height > in_height {
                if (in_height as f64 / in_width as f64) > 1.0 {
                    let denom = height.max(1) as f64;
                    width = ((width as f64) * (in_height as f64 / denom))
                        .round()
                        .max(1.0) as u32;
                } else {
                    width = in_width;
                }
            }

            let mut target_w = width.max(1);
            let scale = target_w as f64 / in_width as f64;
            let mut target_h = ((in_height as f64 * scale).round() as u32).max(1);

            if target_w > 8_192 || target_h > 8_192 {
                let cap_scale = f64::min(8_192.0 / target_w as f64, 8_192.0 / target_h as f64);
                target_w = ((target_w as f64 * cap_scale).round() as u32).clamp(1, 8_192);
                target_h = ((target_h as f64 * cap_scale).round() as u32).clamp(1, 8_192);
            }

            let scaled = resize_with_downsample(img, target_w, target_h, filter);
            compose_on_canvas(scaled, target_w, target_h, spec, format)
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

    Ok((output_image, out_bytes, format))
}

pub fn resize_image(
    input_bytes: &[u8],
    spec: &ResizeSpec,
    faster: bool,
) -> Result<(Vec<u8>, ImageFormat), ImageError> {
    if input_bytes.len() > MAX_INPUT_BYTES {
        return Err(ImageError::InvalidArguments);
    }
    if let Some(w) = spec.width {
        if w > 8_192 {
            return Err(ImageError::InvalidArguments);
        }
    }
    if let Some(h) = spec.height {
        if h > 8_192 {
            return Err(ImageError::InvalidArguments);
        }
    }

    let in_format = sniff_format(input_bytes).ok_or(ImageError::UnsupportedFormat)?;

    // Short-circuit if no width/height specified and formats match;
    // return original image.
    if spec.width.is_none()
        && spec.height.is_none()
        && (spec.explicit_format.is_none() || spec.explicit_format == Some(in_format))
    {
        return Ok((input_bytes.to_vec(), in_format));
    }

    let (img, in_fmt) = decode_checked(input_bytes)?;
    let (_out_img, out_bytes, out_format) = resize_decoded_image(&img, in_fmt, spec, faster)?;
    Ok((out_bytes, out_format))
}

fn format_series_cachekey(cachekey: &str, spec: &str) -> String {
    if let Some((stem, ext)) = cachekey.rsplit_once('.') {
        if (ext.len() == 3 || ext.len() == 4)
            && ext.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return format!("{stem}_{spec}.{ext}");
        }
    }
    format!("{cachekey}{spec}")
}

fn format_series_spec_str(spec: &ResizeSpec) -> String {
    let w_str = match spec.width {
        Some(w) => w.to_string(),
        None => "X".to_string(),
    };
    let h_str = match spec.height {
        Some(h) => h.to_string(),
        None => "X".to_string(),
    };
    let mut out = format!("{w_str}x{h_str}_{}", spec.mode.as_char());
    if let Some(bg) = spec.bgcolor {
        if (bg >> 24) == 0xFF {
            out.push_str(&format!("_{:06x}", bg & 0x00FF_FFFF));
        } else {
            out.push_str(&format!("_{bg:08x}"));
        }
    }
    if let Some(fmt) = spec.explicit_format {
        out.push('.');
        out.push_str(fmt.as_str());
    }
    out
}

fn read_image_or_embedded_from_file(
    file: &mut File,
    meta: &Metadata,
) -> Result<Vec<u8>, ImageError> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = Vec::with_capacity(16);
    (&mut *file).take(16).read_to_end(&mut header)?;

    if sniff_format(&header).is_some() {
        let remaining_cap = (MAX_INPUT_BYTES + 1).saturating_sub(header.len()) as u64;
        let mut full = header;
        file.take(remaining_cap).read_to_end(&mut full)?;
        if full.len() > MAX_INPUT_BYTES {
            return Err(ImageError::InvalidArguments);
        }
        Ok(full)
    } else {
        match extract_embedded_artwork_from_reader(file, meta.len())? {
            Some(art) => Ok(art.data),
            None => Err(ImageError::NoArtworkFound),
        }
    }
}

fn clamp_mtime_to_u32(mtime: i64) -> u32 {
    if mtime <= 0 {
        0
    } else {
        u32::try_from(mtime).unwrap_or(u32::MAX)
    }
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

    if specs.len() > 1 && cachekey.is_empty() {
        return Err(ImageError::InvalidArguments);
    }

    let (src_bytes, original_path, mtime) = match file_or_data {
        InputSource::Path(p) => {
            let (mut file, meta) = open_regular_file(p)?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
                .unwrap_or(0);
            let path_str = p.to_string_lossy().to_string();
            let raw = read_image_or_embedded_from_file(&mut file, &meta)?;
            (Cow::Owned(raw), path_str, mtime)
        }
        InputSource::Bytes(b) => (Cow::Borrowed(b), String::new(), 0),
        InputSource::OpenedFile {
            mut file,
            original_path,
            mtime,
        } => {
            let meta = file.metadata()?;
            validate_regular_file_metadata(&meta)?;
            let raw = read_image_or_embedded_from_file(&mut file, &meta)?;
            let path_str = original_path.to_string_lossy().to_string();
            (Cow::Owned(raw), path_str, clamp_mtime_to_u32(mtime))
        }
    };

    if src_bytes.len() > MAX_INPUT_BYTES {
        return Err(ImageError::InvalidArguments);
    }

    if let [single_spec] = specs {
        let spec_parsed = try_parse_single_spec(single_spec)?;
        let (out_bytes, out_format) = resize_image(src_bytes.as_ref(), &spec_parsed, faster)?;

        if !cachekey.is_empty() {
            let record = ArtworkRecord {
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
            sort_w: u32,
            sort_h: u32,
        }

        let mut parsed_specs: Vec<ResizeSpec> = Vec::with_capacity(specs.len());
        for s in specs {
            let parsed = try_parse_single_spec(s)?;
            if let Some(w) = parsed.width {
                if w > 8_192 {
                    return Err(ImageError::InvalidArguments);
                }
            }
            if let Some(h) = parsed.height {
                if h > 8_192 {
                    return Err(ImageError::InvalidArguments);
                }
            }
            parsed_specs.push(parsed);
        }

        let (src_img, src_fmt) = decode_checked(&src_bytes)?;
        let src_w = src_img.width();
        let src_h = src_img.height();
        if src_w == 0 || src_h == 0 {
            return Err(ImageError::InvalidArguments);
        }

        let mut series: Vec<SeriesItem> = Vec::with_capacity(parsed_specs.len());
        for parsed in parsed_specs {
            let (sort_w, sort_h) = match (parsed.width, parsed.height) {
                (Some(w), Some(h)) => (w, h),
                (Some(w), None) => {
                    let h = ((src_h as f64) * (w as f64 / src_w as f64))
                        .round()
                        .max(1.0) as u32;
                    (w, h)
                }
                (None, Some(h)) => {
                    let w = ((src_w as f64) * (h as f64 / src_h as f64))
                        .round()
                        .max(1.0) as u32;
                    (w, h)
                }
                (None, None) => (src_w, src_h),
            };
            series.push(SeriesItem {
                parsed,
                sort_w,
                sort_h,
            });
        }

        series.sort_by_key(|a| std::cmp::Reverse((a.sort_w, a.sort_h)));

        let mut staged: Vec<(String, ArtworkRecord)> = Vec::with_capacity(series.len());
        let mut first_result: Option<(Vec<u8>, ImageFormat)> = None;

        for item in &series {
            let (next_img, resized_bytes, format) =
                resize_decoded_image(&src_img, src_fmt, &item.parsed, faster)?;
            if next_img.width() > 8_192 || next_img.height() > 8_192 {
                return Err(ImageError::InvalidArguments);
            }

            let spec_str = format_series_spec_str(&item.parsed);
            let key = format_series_cachekey(cachekey, &spec_str);

            let record_data = if first_result.is_none() {
                let data = resized_bytes.clone();
                first_result = Some((resized_bytes, format));
                data
            } else {
                resized_bytes
            };

            let record = ArtworkRecord {
                content_type: format.as_str().to_string(),
                mtime,
                original_path: original_path.clone(),
                data: record_data,
            };
            staged.push((key, record));
        }

        store.set_batch(&staged)?;

        first_result.ok_or_else(|| ImageError::InvalidSpec("no series processed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;
    use image::ImageFormat as ImgFmt;
    use image::RgbImage;
    use lofty::config::WriteOptions;
    use lofty::picture::MimeType;
    use lofty::picture::Picture;
    use lofty::picture::PictureType;
    use lofty::tag::Tag;
    use lofty::tag::TagExt;
    use lofty::tag::TagType;
    use std::io::Cursor;
    use std::io::Write;

    fn make_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |_, _| image::Rgb([200, 100, 50])));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg)
            .expect("encode test jpeg");
        buf
    }

    #[test]
    fn test_resize_mode_m_switches_to_png_unless_explicit_jpg() {
        // Non-square 200x100 JPEG resized to 100x100 in mode 'm':
        // Bug 17140: without explicit format, switches to PNG for padding.
        let src = make_jpeg(200, 100);
        let spec_implicit = parse_single_spec("100x100_m");
        let (bytes_png, fmt_png) =
            resize_image(&src, &spec_implicit, false).expect("resize implicit png");
        assert_eq!(fmt_png, ImageFormat::Png);
        assert!(bytes_png.starts_with(b"\x89PNG"));

        // With explicit .jpg format (t/gdresizer_explicit_format.t), honors jpg.
        let spec_explicit = parse_single_spec("100x100_m.jpg");
        let (bytes_jpg, fmt_jpg) =
            resize_image(&src, &spec_explicit, false).expect("resize explicit jpg");
        assert_eq!(fmt_jpg, ImageFormat::Jpg);
        assert!(bytes_jpg.starts_with(b"\xFF\xD8"));
    }

    #[test]
    fn test_resize_series_descending_and_cache_key_formatting() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
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
        .expect("series resize");
        assert!(store
            .get("music/deadbeef/cover_200x200_o")
            .expect("get 200x200_o")
            .is_some());
        assert!(store
            .get("music/deadbeef/cover_100x100_o")
            .expect("get 100x100_o")
            .is_some());
        assert!(store
            .get("music/deadbeef/cover_50x50_m")
            .expect("get 50x50_m")
            .is_some());
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
        assert_eq!(s2.bgcolor, Some(0xFFFF0000));
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

        let s6 = parse_single_spec("Xx100");
        assert_eq!(s6.width, None);
        assert_eq!(s6.height, Some(100));
        assert_eq!(s6.mode, ResizeMode::Max);

        let s7 = parse_single_spec("0x200");
        assert_eq!(s7.width, None);
        assert_eq!(s7.height, Some(200));
        assert_eq!(s7.mode, ResizeMode::Max);

        let s8 = parse_single_spec("150x200_p_00112233.png");
        assert_eq!(s8.bgcolor, Some(0x00112233));
        assert_eq!(s8.explicit_format, Some(ImageFormat::Png));
    }

    #[test]
    fn test_missing_dimensions_proportional_resolution() {
        let src = make_jpeg(400, 200);

        let spec_x100 = parse_single_spec("Xx100_m");
        let (out_x100, _) = resize_image(&src, &spec_x100, false).expect("resize Xx100");
        let dec_x100 = image::load_from_memory(&out_x100).expect("decode Xx100");
        assert_eq!(dec_x100.width(), 200);
        assert_eq!(dec_x100.height(), 100);

        let spec_100x = parse_single_spec("100xX_m");
        let (out_100x, _) = resize_image(&src, &spec_100x, false).expect("resize 100xX");
        let dec_100x = image::load_from_memory(&out_100x).expect("decode 100xX");
        assert_eq!(dec_100x.width(), 100);
        assert_eq!(dec_100x.height(), 50);

        let src_tall = make_jpeg(800, 400);
        let spec_0x200 = parse_single_spec("0x200_m");
        let (out_0x200, _) = resize_image(&src_tall, &spec_0x200, false).expect("resize 0x200");
        let dec_0x200 = image::load_from_memory(&out_0x200).expect("decode 0x200");
        assert_eq!(dec_0x200.width(), 400);
        assert_eq!(dec_0x200.height(), 200);
    }

    #[test]
    fn test_series_preserves_x_dimensions_and_dot_extensions() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        let src = make_jpeg(400, 200);
        let specs = vec![
            "XxX_m".to_string(),
            "100xX_o".to_string(),
            "Xx50_m".to_string(),
        ];
        gdresize_to_cache(
            InputSource::Bytes(&src),
            &specs,
            &store,
            "music/deadbeef/cover_",
            false,
        )
        .expect("series with X dims");

        let rec_xxx = store
            .get("music/deadbeef/cover_XxX_m")
            .expect("get XxX_m")
            .expect("XxX_m record present");
        let dec_xxx = image::load_from_memory(&rec_xxx.data).expect("decode XxX_m");
        assert_eq!(dec_xxx.width(), 400);
        assert_eq!(dec_xxx.height(), 200);

        let rec_100x = store
            .get("music/deadbeef/cover_100xX_o")
            .expect("get 100xX_o")
            .expect("100xX_o record present");
        let dec_100x = image::load_from_memory(&rec_100x.data).expect("decode 100xX_o");
        assert_eq!(dec_100x.width(), 100);
        assert_eq!(dec_100x.height(), 50);

        let rec_x50 = store
            .get("music/deadbeef/cover_Xx50_m")
            .expect("get Xx50_m")
            .expect("Xx50_m record present");
        let dec_x50 = image::load_from_memory(&rec_x50.data).expect("decode Xx50_m");
        assert_eq!(dec_x50.width(), 100);
        assert_eq!(dec_x50.height(), 50);

        let ext_specs = vec!["200x200_o".to_string(), "100x100_o".to_string()];
        gdresize_to_cache(
            InputSource::Bytes(&src),
            &ext_specs,
            &store,
            "music/deadbeef/cover.jpg",
            false,
        )
        .expect("series .jpg cachekey");
        assert!(store
            .get("music/deadbeef/cover_200x200_o.jpg")
            .expect("get .jpg 200")
            .is_some());
        assert!(store
            .get("music/deadbeef/cover_100x100_o.jpg")
            .expect("get .jpg 100")
            .is_some());

        gdresize_to_cache(
            InputSource::Bytes(&src),
            &ext_specs,
            &store,
            "music/deadbeef/cover_.gd",
            false,
        )
        .expect("series .gd cachekey");
        assert!(store
            .get("music/deadbeef/cover_.gd200x200_o")
            .expect("get .gd 200")
            .is_some());
        assert!(store
            .get("music/deadbeef/cover_.gd100x100_o")
            .expect("get .gd 100")
            .is_some());
    }

    #[test]
    fn test_transparent_8digit_bgcolor_preserves_alpha() {
        let src = make_jpeg(200, 100);
        let spec = parse_single_spec("100x100_p_00112233.png");
        let (out, fmt) = resize_image(&src, &spec, false).expect("render transparent pad");
        assert_eq!(fmt, ImageFormat::Png);
        let decoded = image::load_from_memory(&out)
            .expect("decode transparent png")
            .to_rgba8();
        assert_eq!(decoded.width(), 100);
        assert_eq!(decoded.height(), 100);
        assert_eq!(
            *decoded.get_pixel(0, 0),
            image::Rgba([0x11, 0x22, 0x33, 0x00])
        );
    }

    #[test]
    fn test_sniff_format_all_types() {
        assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\n"), Some(ImageFormat::Png));
        assert_eq!(sniff_format(b"\x89PNG"), None);
        assert_eq!(sniff_format(b"\xFF\xD8\xFF\xE0"), Some(ImageFormat::Jpg));
        assert_eq!(sniff_format(b"\xFF\xD8"), None);
        assert_eq!(sniff_format(b"GIF87a"), Some(ImageFormat::Gif));
        assert_eq!(sniff_format(b"GIF89a"), Some(ImageFormat::Gif));
        assert_eq!(sniff_format(b"GIF"), None);
        assert_eq!(sniff_format(b"GIF99a"), None);
        let valid_bmp = b"BM\x36\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00";
        assert_eq!(sniff_format(valid_bmp), Some(ImageFormat::Bmp));
        assert_eq!(sniff_format(b"BM"), None);
        let bad_bmp_off = b"BM\x36\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(sniff_format(bad_bmp_off), None);
        assert_eq!(sniff_format(b"UNKNOWN_MAGIC"), None);
    }

    #[test]
    fn test_jpg_rgba_compositing_when_scaled_equals_canvas() {
        let mut rgba = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 0]));
        for y in 25..75 {
            for x in 25..75 {
                rgba.put_pixel(x, y, image::Rgba([10, 200, 30, 255]));
            }
        }
        let mut png_bytes = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png_bytes), ImgFmt::Png)
            .expect("encode rgba png");

        let cases = [
            ("100x100_m_ff0000.jpg", (230u8, 25u8, 25u8), true),
            ("100x100_p_00ff00.jpg", (25u8, 230u8, 25u8), false),
            ("100x100_F_0000ff.jpg", (25u8, 25u8, 230u8), false),
        ];
        for (spec_str, expected, is_red) in cases {
            let spec = parse_single_spec(spec_str);
            let (out, fmt) = resize_image(&png_bytes, &spec, false).expect("resize rgba to jpg");
            assert_eq!(fmt, ImageFormat::Jpg);
            let dec = image::load_from_memory(&out).expect("decode jpg").to_rgb8();
            let corner = dec.get_pixel(0, 0);
            if is_red {
                assert!(corner[0] > expected.0 && corner[1] < expected.1 && corner[2] < expected.2);
            } else if spec_str.contains("_p_") {
                assert!(corner[0] < expected.0 && corner[1] > expected.1 && corner[2] < expected.2);
            } else {
                assert!(corner[0] < expected.0 && corner[1] < expected.1 && corner[2] > expected.2);
            }
        }
    }

    #[test]
    fn test_series_renders_independently_from_original_source() {
        let mut rgba = image::RgbaImage::from_pixel(200, 100, image::Rgba([0, 0, 0, 0]));
        for y in 0..100 {
            for x in 100..200 {
                rgba.put_pixel(x, y, image::Rgba([20, 180, 40, 255]));
            }
        }
        let mut png_bytes = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png_bytes), ImgFmt::Png)
            .expect("encode source png");

        let dir = tempfile::tempdir().expect("create tempdir");
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        let specs = vec!["150x150_p_ff0000.jpg".to_string(), "100x100_o".to_string()];
        gdresize_to_cache(
            InputSource::Bytes(&png_bytes),
            &specs,
            &store,
            "music/series_orig/cover_",
            false,
        )
        .expect("series resize");

        let rec = store
            .get("music/series_orig/cover_100x100_o")
            .expect("get 100x100_o")
            .expect("100x100_o present");
        assert_eq!(rec.content_type, "png");
        let dec = image::load_from_memory(&rec.data)
            .expect("decode 100x100_o")
            .to_rgba8();
        assert_eq!(dec.width(), 100);
        assert_eq!(dec.height(), 50);
        assert_eq!(dec.get_pixel(0, 0)[3], 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_rejects_symlinks_and_non_regular_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let img_path = dir.path().join("real.jpg");
        std::fs::write(&img_path, make_jpeg(100, 100)).expect("write real.jpg");

        let sym_path = dir.path().join("link.jpg");
        std::os::unix::fs::symlink(&img_path, &sym_path).expect("create symlink");

        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        assert!(extract_embedded_artwork(&sym_path).is_err());
        assert!(gdresize_to_cache(
            InputSource::Path(&sym_path),
            &["50x50_m".to_string()],
            &store,
            "cached/sym",
            false,
        )
        .is_err());

        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).expect("create subdir");
        assert!(matches!(
            extract_embedded_artwork(&subdir),
            Err(ImageError::InvalidArguments)
        ));
        assert!(matches!(
            gdresize_to_cache(
                InputSource::Path(&subdir),
                &["50x50_m".to_string()],
                &store,
                "cached/dir",
                false,
            ),
            Err(ImageError::InvalidArguments)
        ));
    }

    #[test]
    fn test_short_circuit_on_xxx_matching_format() {
        let src = make_jpeg(50, 50);
        let spec = parse_single_spec("XxX");
        let (out, fmt) = resize_image(&src, &spec, false).expect("short-circuit XxX");
        assert_eq!(fmt, ImageFormat::Jpg);
        assert_eq!(out, src);
    }

    #[test]
    fn test_resize_mode_o_preserves_aspect_without_padding() {
        let src = make_jpeg(200, 100);
        let spec = parse_single_spec("100x100_o");
        let (out, fmt) = resize_image(&src, &spec, false).expect("resize mode o");
        assert_eq!(fmt, ImageFormat::Jpg);
        let decoded = image::load_from_memory(&out).expect("decode mode o");
        assert_eq!(decoded.width(), 100);
        assert_eq!(decoded.height(), 50);
    }

    #[test]
    fn test_upscale_avoidance_and_original_dimension_cap() {
        let src = make_jpeg(100, 80);

        let spec_f = parse_single_spec("200x200_F");
        let (out_f, _) = resize_image(&src, &spec_f, false).expect("resize mode F");
        let dec_f = image::load_from_memory(&out_f).expect("decode mode F");
        assert_eq!(dec_f.width(), 100);
        assert_eq!(dec_f.height(), 80);

        let spec_m = parse_single_spec("200x200_m");
        let (out_m, _) = resize_image(&src, &spec_m, false).expect("resize mode m no upscale");
        let dec_m = image::load_from_memory(&out_m).expect("decode mode m");
        assert_eq!(dec_m.width(), 80);
        assert_eq!(dec_m.height(), 80);

        let spec_o = parse_single_spec("200x200_o");
        let (out_o, _) = resize_image(&src, &spec_o, false).expect("resize mode o no upscale");
        let dec_o = image::load_from_memory(&out_o).expect("decode mode o");
        assert_eq!(dec_o.width(), 100);
        assert_eq!(dec_o.height(), 80);

        let tall = make_jpeg(10, 1_000);
        let spec_amp = parse_single_spec("2000x500_o");
        let (out_amp, _) = resize_image(&tall, &spec_amp, true).expect("cap mode o amplification");
        let dec_amp = image::load_from_memory(&out_amp).expect("decode capped mode o");
        assert!(dec_amp.width() <= 8_192);
        assert!(dec_amp.height() <= 8_192);
        assert!(dec_amp.width() >= 1 && dec_amp.height() >= 1);
    }

    fn silence_mp3_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../HTML/EN/html/silence.mp3")
    }

    #[test]
    fn test_extract_embedded_artwork_from_tagged_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mp3_path = dir.path().join("tagged.mp3");
        std::fs::copy(silence_mp3_path(), &mp3_path).expect("copy silence.mp3");

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
            .expect("save mp3 tag");

        let extracted = extract_embedded_artwork(&mp3_path).expect("extract artwork");
        assert_eq!(extracted, fake_art);
    }

    #[test]
    fn test_extract_embedded_artwork_selects_lowest_picture_type() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mp3_path = dir.path().join("tagged_multi.mp3");
        std::fs::copy(silence_mp3_path(), &mp3_path).expect("copy silence.mp3");

        let mut tag = Tag::new(TagType::Id3v2);
        let pic_media = Picture::new_unchecked(
            PictureType::Media,
            Some(MimeType::Jpeg),
            None,
            vec![6, 6, 6],
        );
        let pic_icon =
            Picture::new_unchecked(PictureType::Icon, Some(MimeType::Jpeg), None, vec![1, 1, 1]);
        // Push in reverse order
        tag.push_picture(pic_media);
        tag.push_picture(pic_icon);
        tag.save_to_path(&mp3_path, WriteOptions::default())
            .expect("save multi-pic mp3 tag");

        let extracted = extract_embedded_artwork(&mp3_path).expect("extract lowest type");
        assert_eq!(extracted, vec![1, 1, 1]);
    }

    #[test]
    fn test_gdresize_to_cache_single_spec_with_path_and_artwork_record() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let img_path = dir.path().join("source.jpg");
        let src = make_jpeg(200, 200);
        std::fs::write(&img_path, &src).expect("write source.jpg");

        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        let (out, fmt) = gdresize_to_cache(
            InputSource::Path(&img_path),
            &["100x100_m".to_string()],
            &store,
            "cached/test_key",
            false,
        )
        .expect("gdresize single path");
        assert_eq!(fmt, ImageFormat::Jpg);
        assert!(!out.is_empty());
        let cached = store
            .get("cached/test_key")
            .expect("get cached record")
            .expect("cached record present");
        assert_eq!(cached.data, out);
        assert_eq!(cached.content_type, "jpg");
        assert!(cached.mtime > 0);
        assert_eq!(cached.original_path, img_path.to_string_lossy().to_string());
    }

    #[test]
    fn test_dimension_bounds_and_series_guard_rejections() {
        let src = make_jpeg(100, 100);
        let spec_w = parse_single_spec("8193x100_m");
        assert!(matches!(
            resize_image(&src, &spec_w, false),
            Err(ImageError::InvalidArguments)
        ));

        let spec_h = parse_single_spec("100x8193_m");
        assert!(matches!(
            resize_image(&src, &spec_h, false),
            Err(ImageError::InvalidArguments)
        ));

        let dir = tempfile::tempdir().expect("create tempdir");
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");

        let bad_series = vec!["8193x100_m".to_string(), "100x100_m".to_string()];
        assert!(matches!(
            gdresize_to_cache(
                InputSource::Bytes(&src),
                &bad_series,
                &store,
                "music/deadbeef/cover_",
                false
            ),
            Err(ImageError::InvalidArguments)
        ));

        let ok_series = vec!["200x200_m".to_string(), "100x100_m".to_string()];
        assert!(matches!(
            gdresize_to_cache(InputSource::Bytes(&src), &ok_series, &store, "", false),
            Err(ImageError::InvalidArguments)
        ));

        let oversize_path = dir.path().join("oversize.jpg");
        {
            let mut file = std::fs::File::create(&oversize_path).expect("create oversize file");
            file.write_all(b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01")
                .expect("write jpeg header");
            file.set_len((MAX_INPUT_BYTES + 1) as u64)
                .expect("extend oversize file");
        }
        assert!(matches!(
            gdresize_to_cache(
                InputSource::Path(&oversize_path),
                &["100x100_m".to_string()],
                &store,
                "cached/oversize",
                false
            ),
            Err(ImageError::InvalidArguments)
        ));
    }

    fn build_flac_block(is_last: bool, block_type: u8, payload: &[u8]) -> Vec<u8> {
        let len = payload.len() as u32;
        let len_be = len.to_be_bytes();
        let mut out = Vec::with_capacity(4 + payload.len());
        let first = (block_type & 0x7f) | if is_last { 0x80 } else { 0 };
        out.push(first);
        out.extend_from_slice(&len_be[1..4]);
        out.extend_from_slice(payload);
        out
    }

    fn build_flac_picture_payload(pic_type: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&pic_type.to_be_bytes());
        out.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        out.extend_from_slice(mime.as_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // description length
        out.extend_from_slice(&[0u8; 16]); // width, height, depth, colors
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    fn encode_syncsafe_u32(val: u32) -> [u8; 4] {
        [
            ((val >> 21) & 0x7f) as u8,
            ((val >> 14) & 0x7f) as u8,
            ((val >> 7) & 0x7f) as u8,
            (val & 0x7f) as u8,
        ]
    }

    #[test]
    fn test_flac_vorbis_comments_u32_max_skipped_and_picture_extracted() {
        let mut flac = Vec::new();
        flac.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0)
        flac.extend_from_slice(&build_flac_block(false, 0, &[0u8; 34]));

        // VORBIS_COMMENTS (type 4) claiming u32::MAX comment items
        let mut vorbis = Vec::new();
        vorbis.extend_from_slice(&0u32.to_le_bytes()); // vendor_length = 0
        vorbis.extend_from_slice(&u32::MAX.to_le_bytes()); // list_length = MAX
        flac.extend_from_slice(&build_flac_block(false, 4, &vorbis));

        let fake_art = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x11, 0x22, 0x33, 0x44];
        let pic_payload = build_flac_picture_payload(3, "image/jpeg", &fake_art);
        flac.extend_from_slice(&build_flac_block(true, 6, &pic_payload));

        let len = flac.len() as u64;
        let mut cursor = Cursor::new(flac);
        let extracted = extract_embedded_artwork_from_reader(&mut cursor, len)
            .expect("flac extraction succeeds")
            .expect("artwork present");
        assert_eq!(extracted.data, fake_art);
        assert_eq!(extracted.picture_type, 3);
        assert_eq!(extracted.mime_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn test_id3v2_rejects_compressed_and_oversized_apic_headers() {
        // 1. Compressed v2.3 APIC (flags = 0x0080)
        let mut compressed = Vec::new();
        compressed.extend_from_slice(b"ID3");
        compressed.extend_from_slice(&[3, 0, 0]);
        compressed.extend_from_slice(&encode_syncsafe_u32(20));
        compressed.extend_from_slice(b"APIC");
        compressed.extend_from_slice(&10u32.to_be_bytes());
        compressed.extend_from_slice(&0x0080u16.to_be_bytes());
        compressed.extend_from_slice(&[0u8; 10]);
        let len = compressed.len() as u64;
        assert!(matches!(
            extract_embedded_artwork_from_reader(&mut Cursor::new(compressed), len),
            Err(ImageError::Lofty(_))
        ));

        // 2. Oversized APIC frame (> MAX_ARTWORK_BYTES)
        let mut oversized_frame = Vec::new();
        oversized_frame.extend_from_slice(b"ID3");
        oversized_frame.extend_from_slice(&[3, 0, 0]);
        oversized_frame.extend_from_slice(&encode_syncsafe_u32(20));
        oversized_frame.extend_from_slice(b"APIC");
        oversized_frame.extend_from_slice(&((MAX_ARTWORK_BYTES as u32) + 1).to_be_bytes());
        oversized_frame.extend_from_slice(&0u16.to_be_bytes());
        oversized_frame.extend_from_slice(&[0u8; 10]);
        let len = oversized_frame.len() as u64;
        assert!(matches!(
            extract_embedded_artwork_from_reader(&mut Cursor::new(oversized_frame), len),
            Err(ImageError::Lofty(_))
        ));

        // 3. Oversized ID3v2 header syncsafe tag_size (> MAX_ARTWORK_BYTES)
        let mut oversized_tag = Vec::new();
        oversized_tag.extend_from_slice(b"ID3");
        oversized_tag.extend_from_slice(&[3, 0, 0]);
        oversized_tag.extend_from_slice(&encode_syncsafe_u32((MAX_ARTWORK_BYTES as u32) + 1));
        let len = oversized_tag.len() as u64;
        assert!(matches!(
            extract_embedded_artwork_from_reader(&mut Cursor::new(oversized_tag), len),
            Err(ImageError::Lofty(_))
        ));
    }

    #[test]
    fn test_resize_image_rejects_oversize_input_in_passthrough_mode() {
        let mut data = vec![0u8; MAX_INPUT_BYTES + 1];
        data[..4].copy_from_slice(b"\xFF\xD8\xFF\xE0");
        let spec = parse_single_spec("XxX_o");
        assert_eq!(spec.mode, ResizeMode::Original);
        assert!(matches!(
            resize_image(&data, &spec, false),
            Err(ImageError::InvalidArguments)
        ));
    }

    #[test]
    fn test_try_parse_single_spec_rejects_malformed_and_overflow_specs() {
        assert!(matches!(
            try_parse_single_spec("4294967296x100_m"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x4294967296_m"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x100_z"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x100_p_123"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x100_p_zzzzzz"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x100_m.webp"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x100_m.gd"),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(matches!(
            try_parse_single_spec("100x100_m_ff0000_extra"),
            Err(ImageError::InvalidSpec(_))
        ));
    }

    #[test]
    fn test_bug_17140_aspect_rounding_without_padding_stays_jpg() {
        let src_3x2 = make_jpeg(300, 200);
        let spec_x = parse_single_spec("100xX_m");
        let (out_bytes, fmt) = resize_image(&src_3x2, &spec_x, false).expect("resize 300x200");
        assert_eq!(fmt, ImageFormat::Jpg);
        let dec = image::load_from_memory(&out_bytes).expect("decode 100x67 jpg");
        assert_eq!(dec.width(), 100);
        assert_eq!(dec.height(), 67);

        let src_100x101 = make_jpeg(100, 101);
        let spec_sq = parse_single_spec("50x50_m");
        let (_, fmt_sq) = resize_image(&src_100x101, &spec_sq, false).expect("resize 100x101");
        assert_eq!(fmt_sq, ImageFormat::Jpg);
    }

    #[test]
    fn test_gif_and_bmp_decode_resize_and_encode() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(80, 40, |x, _| {
            if x < 40 {
                image::Rgb([220, 40, 40])
            } else {
                image::Rgb([40, 40, 220])
            }
        }));

        let mut gif_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut gif_bytes), ImgFmt::Gif)
            .expect("encode gif");
        assert_eq!(sniff_format(&gif_bytes), Some(ImageFormat::Gif));
        let spec_gif = parse_single_spec("40x20_m.gif");
        let (out_gif, fmt_gif) = resize_image(&gif_bytes, &spec_gif, false).expect("resize gif");
        assert_eq!(fmt_gif, ImageFormat::Gif);
        assert_eq!(sniff_format(&out_gif), Some(ImageFormat::Gif));
        let dec_gif = image::load_from_memory(&out_gif).expect("decode resized gif");
        assert_eq!(dec_gif.width(), 40);
        assert_eq!(dec_gif.height(), 20);

        let mut bmp_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bmp_bytes), ImgFmt::Bmp)
            .expect("encode bmp");
        assert_eq!(sniff_format(&bmp_bytes), Some(ImageFormat::Bmp));
        let spec_bmp = parse_single_spec("40x20_m.bmp");
        let (out_bmp, fmt_bmp) = resize_image(&bmp_bytes, &spec_bmp, false).expect("resize bmp");
        assert_eq!(fmt_bmp, ImageFormat::Bmp);
        assert_eq!(sniff_format(&out_bmp), Some(ImageFormat::Bmp));
        let dec_bmp = image::load_from_memory(&out_bmp).expect("decode resized bmp");
        assert_eq!(dec_bmp.width(), 40);
        assert_eq!(dec_bmp.height(), 20);
    }

    #[test]
    fn test_rgba_to_jpg_default_black_and_alpha_blend() {
        let mut rgba = image::RgbaImage::from_pixel(60, 60, image::Rgba([0, 0, 0, 0]));
        for y in 20..40 {
            for x in 20..40 {
                rgba.put_pixel(x, y, image::Rgba([200, 0, 0, 128]));
            }
        }
        let mut png_bytes = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png_bytes), ImgFmt::Png)
            .expect("encode rgba png");

        let spec_default = parse_single_spec("60x60_m.jpg");
        let (out_default, fmt_default) =
            resize_image(&png_bytes, &spec_default, false).expect("rgba to default jpg");
        assert_eq!(fmt_default, ImageFormat::Jpg);
        let dec_default = image::load_from_memory(&out_default)
            .expect("decode default jpg")
            .to_rgb8();
        assert!(dec_default.get_pixel(0, 0)[0] < 10);
        assert!(dec_default.get_pixel(0, 0)[1] < 10);
        assert!(dec_default.get_pixel(0, 0)[2] < 10);

        let spec_white = parse_single_spec("60x60_m_ffffff.jpg");
        let (out_white, _) =
            resize_image(&png_bytes, &spec_white, false).expect("rgba over white jpg");
        let dec_white = image::load_from_memory(&out_white)
            .expect("decode white jpg")
            .to_rgb8();
        let center = dec_white.get_pixel(30, 30);
        assert!(center[0] > 200 && center[1] > 90 && center[1] < 165);
    }

    #[test]
    fn test_series_set_batch_transactional_and_distinct_specs() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        let src = make_jpeg(200, 200);

        // Failing second spec must not commit the first spec.
        let failing_specs = vec!["100x100_o".to_string(), "150x150_p_badhex".to_string()];
        assert!(matches!(
            gdresize_to_cache(
                InputSource::Bytes(&src),
                &failing_specs,
                &store,
                "music/tx_fail/cover_",
                false
            ),
            Err(ImageError::InvalidSpec(_))
        ));
        assert!(store
            .get("music/tx_fail/cover_100x100_o")
            .expect("query tx_fail")
            .is_none());

        // Distinct bgcolor + format specs in a series must not collide on cache keys.
        let distinct_specs = vec![
            "100x100_p_ff0000.jpg".to_string(),
            "100x100_p_00ff00.png".to_string(),
        ];
        gdresize_to_cache(
            InputSource::Bytes(&src),
            &distinct_specs,
            &store,
            "music/tx_ok/cover_",
            false,
        )
        .expect("series with distinct bgcolor and format");
        let rec_jpg = store
            .get("music/tx_ok/cover_100x100_p_ff0000.jpg")
            .expect("get red jpg")
            .expect("red jpg present");
        assert_eq!(rec_jpg.content_type, "jpg");
        let rec_png = store
            .get("music/tx_ok/cover_100x100_p_00ff00.png")
            .expect("get green png")
            .expect("green png present");
        assert_eq!(rec_png.content_type, "png");
    }

    #[test]
    fn test_flac_rejects_uri_picture_and_preserves_lowest_type_priority() {
        let mut uri_flac = Vec::new();
        uri_flac.extend_from_slice(b"fLaC");
        uri_flac.extend_from_slice(&build_flac_block(false, 0, &[0u8; 34]));
        let uri_pic = build_flac_picture_payload(3, "-->", b"https://example.com/cover.jpg");
        uri_flac.extend_from_slice(&build_flac_block(true, 6, &uri_pic));
        let uri_len = uri_flac.len() as u64;
        assert!(matches!(
            extract_embedded_artwork_from_reader(&mut Cursor::new(uri_flac), uri_len),
            Err(ImageError::Lofty(_))
        ));

        let mut prio_flac = Vec::new();
        prio_flac.extend_from_slice(b"fLaC");
        prio_flac.extend_from_slice(&build_flac_block(false, 0, &[0u8; 34]));
        let pic_front = build_flac_picture_payload(3, "image/jpeg", b"FRONT");
        let pic_icon = build_flac_picture_payload(1, "image/png", b"ICON");
        prio_flac.extend_from_slice(&build_flac_block(false, 6, &pic_front));
        prio_flac.extend_from_slice(&build_flac_block(true, 6, &pic_icon));
        let prio_len = prio_flac.len() as u64;
        let extracted = extract_embedded_artwork_from_reader(&mut Cursor::new(prio_flac), prio_len)
            .expect("prio flac succeeds")
            .expect("artwork present");
        assert_eq!(extracted.picture_type, 1);
        assert_eq!(extracted.data, b"ICON");
    }

    #[test]
    fn test_bounded_reader_seek_within_offset_range() {
        let backing: Vec<u8> = (0u8..30).collect();
        let mut cursor = Cursor::new(backing);
        let mut br = BoundedReader::new(&mut cursor, 10, 10).expect("create bounded reader");

        assert_eq!(br.seek(SeekFrom::End(0)).expect("seek end 0"), 10);
        assert_eq!(br.seek(SeekFrom::End(-3)).expect("seek end -3"), 7);
        let mut buf = [0u8; 4];
        let n = br.read(&mut buf).expect("read bounded");
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], &[17, 18, 19]);

        assert!(br.seek(SeekFrom::End(-11)).is_err());
        assert!(br.seek(SeekFrom::End(1)).is_err());
        assert!(br.seek(SeekFrom::Start(11)).is_err());
        assert!(BoundedReader::new(&mut cursor, u64::MAX - 5, 10).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_open_flags_rejects_symlinks() {
        use std::os::unix::fs::OpenOptionsExt as _;

        let flags = unix_open_flags();
        assert_ne!(flags & libc::O_NOFOLLOW, 0);
        assert_ne!(flags & libc::O_NONBLOCK, 0);

        let dir = tempfile::tempdir().expect("create tempdir");
        let real_path = dir.path().join("real.jpg");
        std::fs::write(&real_path, make_jpeg(64, 64)).expect("write real.jpg");
        let sym_path = dir.path().join("sym.jpg");
        std::os::unix::fs::symlink(&real_path, &sym_path).expect("create symlink");

        let sym_open = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(&sym_path);
        assert!(sym_open.is_err());
        assert!(open_regular_file(&sym_path).is_err());
        assert!(open_regular_file(&real_path).is_ok());
    }

    #[test]
    fn test_opened_file_rewinds_and_preserves_metadata_without_reopening() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let img_path = dir.path().join("source.jpg");
        std::fs::write(&img_path, make_jpeg(200, 200)).expect("write source.jpg");

        let mut file = File::open(&img_path).expect("open source.jpg");
        file.seek(SeekFrom::Start(12)).expect("seek forward");

        let virtual_path = PathBuf::from("/virtual/nonexistent/album/cover.jpg");
        let expected_mtime: i64 = 1_700_000_123;
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        let (out, fmt) = gdresize_to_cache(
            InputSource::OpenedFile {
                file,
                original_path: virtual_path.clone(),
                mtime: expected_mtime,
            },
            &["100x100_m".to_string()],
            &store,
            "cached/opened_single",
            false,
        )
        .expect("gdresize opened file");
        assert_eq!(fmt, ImageFormat::Jpg);
        assert!(!out.is_empty());

        let cached = store
            .get("cached/opened_single")
            .expect("get cached record")
            .expect("cached record present");
        assert_eq!(cached.data, out);
        assert_eq!(cached.content_type, "jpg");
        assert_eq!(cached.mtime, 1_700_000_123);
        assert_eq!(cached.original_path, virtual_path.to_string_lossy());

        let mut series_file = File::open(&img_path).expect("reopen for series");
        series_file.seek(SeekFrom::End(0)).expect("seek to end");
        gdresize_to_cache(
            InputSource::OpenedFile {
                file: series_file,
                original_path: virtual_path.clone(),
                mtime: expected_mtime,
            },
            &["120x120_o".to_string(), "60x60_m".to_string()],
            &store,
            "cached/opened_series_",
            false,
        )
        .expect("gdresize opened file series");

        for key in [
            "cached/opened_series_120x120_o",
            "cached/opened_series_60x60_m",
        ] {
            let rec = store
                .get(key)
                .expect("query series record")
                .expect("series record present");
            assert_eq!(rec.mtime, 1_700_000_123);
            assert_eq!(rec.original_path, virtual_path.to_string_lossy());
        }
    }

    #[test]
    fn test_opened_file_embedded_artwork_rejections_and_mtime_clamp() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mp3_path = dir.path().join("tagged.mp3");
        std::fs::copy(silence_mp3_path(), &mp3_path).expect("copy silence.mp3");

        let mut tag = Tag::new(TagType::Id3v2);
        let pic = Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Jpeg),
            None,
            make_jpeg(120, 120),
        );
        tag.push_picture(pic);
        tag.save_to_path(&mp3_path, WriteOptions::default())
            .expect("save mp3 tag");

        let mut mp3_file = File::open(&mp3_path).expect("open tagged.mp3");
        mp3_file.seek(SeekFrom::Start(5)).expect("seek forward");

        let store = lms_cache::DbArtworkStore::open(dir.path(), false).expect("open store");
        let virtual_mp3 = PathBuf::from("/virtual/nonexistent/track.mp3");
        gdresize_to_cache(
            InputSource::OpenedFile {
                file: mp3_file,
                original_path: virtual_mp3.clone(),
                mtime: -5,
            },
            &["60x60_m".to_string()],
            &store,
            "cached/opened_mp3",
            false,
        )
        .expect("gdresize opened mp3");

        let rec = store
            .get("cached/opened_mp3")
            .expect("query mp3 record")
            .expect("mp3 record present");
        assert_eq!(rec.original_path, virtual_mp3.to_string_lossy());
        assert_eq!(rec.mtime, 0);

        let img_path = dir.path().join("clamp.jpg");
        std::fs::write(&img_path, make_jpeg(80, 80)).expect("write clamp.jpg");
        let clamp_file = File::open(&img_path).expect("open clamp.jpg");
        gdresize_to_cache(
            InputSource::OpenedFile {
                file: clamp_file,
                original_path: PathBuf::from("/virtual/clamp.jpg"),
                mtime: i64::MAX,
            },
            &["40x40_m".to_string()],
            &store,
            "cached/opened_clamp",
            false,
        )
        .expect("gdresize opened clamp");
        let clamp_rec = store
            .get("cached/opened_clamp")
            .expect("query clamp record")
            .expect("clamp record present");
        assert_eq!(clamp_rec.mtime, u32::MAX);

        let oversize_path = dir.path().join("oversize_opened.jpg");
        {
            let mut file = File::create(&oversize_path).expect("create oversize file");
            file.write_all(b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01")
                .expect("write jpeg header");
            file.set_len((MAX_INPUT_BYTES + 1) as u64)
                .expect("extend oversize file");
        }
        let oversize_file = File::open(&oversize_path).expect("open oversize file");
        assert!(matches!(
            gdresize_to_cache(
                InputSource::OpenedFile {
                    file: oversize_file,
                    original_path: oversize_path,
                    mtime: 100,
                },
                &["50x50_m".to_string()],
                &store,
                "cached/opened_oversize",
                false,
            ),
            Err(ImageError::InvalidArguments)
        ));

        #[cfg(unix)]
        {
            let subdir = dir.path().join("opened_subdir");
            std::fs::create_dir(&subdir).expect("create subdir");
            let dir_file = File::open(&subdir).expect("open subdir");
            assert!(matches!(
                gdresize_to_cache(
                    InputSource::OpenedFile {
                        file: dir_file,
                        original_path: subdir,
                        mtime: 100,
                    },
                    &["50x50_m".to_string()],
                    &store,
                    "cached/opened_dir",
                    false,
                ),
                Err(ImageError::InvalidArguments)
            ));
        }
    }
}
