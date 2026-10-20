#![deny(unsafe_op_in_unsafe_fn)]

use md5::{Digest, Md5};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum Mp3CutError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid MP3 file: {0}")]
    InvalidFile(String),
    #[error("No MP3 frames found: {0}")]
    NoFramesFound(String),
    #[error("Cache error: {0}")]
    CacheError(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpegVersion {
    Mpeg25,
    Reserved,
    Mpeg2,
    Mpeg1,
}

impl MpegVersion {
    pub fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Mpeg25,
            1 => Self::Reserved,
            2 => Self::Mpeg2,
            3 => Self::Mpeg1,
            _ => Self::Reserved,
        }
    }
}

// MPEG-1 Layer III bitrates (kbps)
const MPEG1_LAYER3_BITRATES: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];

// MPEG-2 / MPEG-2.5 Layer III bitrates (kbps)
const MPEG2_LAYER3_BITRATES: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

// Sampling rates (Hz)
const MPEG1_SAMPLE_RATES: [u32; 4] = [44100, 48000, 32000, 0];
const MPEG2_SAMPLE_RATES: [u32; 4] = [22050, 24000, 16000, 0];
const MPEG25_SAMPLE_RATES: [u32; 4] = [11025, 12000, 8000, 0];

#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub version: MpegVersion,
    pub has_crc: bool,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub padding: u32,
    pub channels: u16,
    pub samples_per_frame: u32,
    pub frame_size: usize,
}

pub fn parse_frame_header(bytes: &[u8]) -> Option<FrameHeader> {
    if bytes.len() < 4 {
        return None;
    }
    let b0 = bytes[0];
    let b1 = bytes[1];
    let b2 = bytes[2];
    let b3 = bytes[3];

    // Frame sync: 11 bits set (0xFF followed by top 3 bits of b1 == 0xE0)
    if b0 != 0xFF || (b1 & 0xE0) != 0xE0 {
        return None;
    }

    let version = MpegVersion::from_bits((b1 >> 3) & 0x03);
    if version == MpegVersion::Reserved {
        return None;
    }

    let layer_bits = (b1 >> 1) & 0x03;
    if layer_bits != 1 {
        // Only Layer III is supported
        return None;
    }

    let has_crc = (b1 & 0x01) == 0;

    let bitrate_idx = ((b2 >> 4) & 0x0F) as usize;
    if bitrate_idx == 0 || bitrate_idx == 15 {
        return None;
    }

    let bitrate_kbps = match version {
        MpegVersion::Mpeg1 => MPEG1_LAYER3_BITRATES[bitrate_idx],
        MpegVersion::Mpeg2 | MpegVersion::Mpeg25 => MPEG2_LAYER3_BITRATES[bitrate_idx],
        MpegVersion::Reserved => return None,
    };
    if bitrate_kbps == 0 {
        return None;
    }

    let sample_rate_idx = ((b2 >> 2) & 0x03) as usize;
    if sample_rate_idx == 3 {
        return None;
    }

    let sample_rate = match version {
        MpegVersion::Mpeg1 => MPEG1_SAMPLE_RATES[sample_rate_idx],
        MpegVersion::Mpeg2 => MPEG2_SAMPLE_RATES[sample_rate_idx],
        MpegVersion::Mpeg25 => MPEG25_SAMPLE_RATES[sample_rate_idx],
        MpegVersion::Reserved => return None,
    };
    if sample_rate == 0 {
        return None;
    }

    let padding = ((b2 >> 1) & 0x01) as u32;

    let channel_mode = (b3 >> 6) & 0x03;
    let channels = if channel_mode == 3 { 1 } else { 2 };

    let samples_per_frame = match version {
        MpegVersion::Mpeg1 => 1152,
        MpegVersion::Mpeg2 | MpegVersion::Mpeg25 => 576,
        MpegVersion::Reserved => return None,
    };

    let frame_size = match version {
        MpegVersion::Mpeg1 => {
            ((144 * (bitrate_kbps as u64) * 1000) / (sample_rate as u64)) as usize + padding as usize
        }
        MpegVersion::Mpeg2 | MpegVersion::Mpeg25 => {
            ((72 * (bitrate_kbps as u64) * 1000) / (sample_rate as u64)) as usize + padding as usize
        }
        MpegVersion::Reserved => return None,
    };

    if frame_size < 4 {
        return None;
    }

    Some(FrameHeader {
        version,
        has_crc,
        bitrate_kbps,
        sample_rate,
        padding,
        channels,
        samples_per_frame,
        frame_size,
    })
}

#[derive(Debug, Clone)]
pub struct FrameEntry {
    pub offset: u64,
    pub length: u32,
}

#[derive(Debug, Clone)]
pub struct Mp3Metadata {
    pub sample_rate: u32,
    pub samples_per_frame: u32,
    pub channels: u16,
    pub enc_delay: u32,
    pub enc_padding: u32,
    pub has_xing: bool,
    pub frames: Vec<FrameEntry>,
}

impl Mp3Metadata {
    pub fn duration_ms(&self) -> u64 {
        let audio_frames = if self.has_xing {
            self.frames.len().saturating_sub(1)
        } else {
            self.frames.len()
        };
        if self.sample_rate == 0 {
            0
        } else {
            (audio_frames as u64 * self.samples_per_frame as u64 * 1000) / self.sample_rate as u64
        }
    }
}

const MLLT_MAGIC: [u8; 4] = *b"MLLT";
const MLLT_VERSION: u32 = 1;

/// Serializes frame index into `.mllt` binary cache format.
pub fn encode_mllt_cache(meta: &Mp3Metadata) -> Vec<u8> {
    let header_size = 32;
    let mut buf = Vec::with_capacity(header_size + meta.frames.len() * 12);
    buf.extend_from_slice(&MLLT_MAGIC);
    buf.extend_from_slice(&MLLT_VERSION.to_le_bytes());
    buf.extend_from_slice(&meta.sample_rate.to_le_bytes());
    buf.extend_from_slice(&meta.samples_per_frame.to_le_bytes());
    buf.extend_from_slice(&meta.channels.to_le_bytes());
    let flags: u16 = if meta.has_xing { 1 } else { 0 };
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&meta.enc_delay.to_le_bytes());
    buf.extend_from_slice(&meta.enc_padding.to_le_bytes());
    buf.extend_from_slice(&(meta.frames.len() as u32).to_le_bytes());

    for frame in &meta.frames {
        buf.extend_from_slice(&frame.offset.to_le_bytes());
        buf.extend_from_slice(&frame.length.to_le_bytes());
    }
    buf
}

/// Deserializes frame index from `.mllt` binary cache format.
pub fn decode_mllt_cache(data: &[u8]) -> Result<Mp3Metadata, Mp3CutError> {
    if data.len() < 32 {
        return Err(Mp3CutError::CacheError("Truncated .mllt header".into()));
    }
    if data[0..4] != MLLT_MAGIC {
        return Err(Mp3CutError::CacheError("Invalid .mllt magic bytes".into()));
    }
    let version = u32::from_le_bytes(data[4..8].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    if version != MLLT_VERSION {
        return Err(Mp3CutError::CacheError(format!("Unsupported .mllt version {version}")));
    }
    let sample_rate = u32::from_le_bytes(data[8..12].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    let samples_per_frame = u32::from_le_bytes(data[12..16].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    let channels = u16::from_le_bytes(data[16..18].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    let flags = u16::from_le_bytes(data[18..20].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    let has_xing = (flags & 1) != 0;
    let enc_delay = u32::from_le_bytes(data[20..24].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    let enc_padding = u32::from_le_bytes(data[24..28].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
    let frame_count = u32::from_le_bytes(data[28..32].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?) as usize;

    let expected_len = 32 + frame_count * 12;
    if data.len() < expected_len {
        return Err(Mp3CutError::CacheError("Incomplete .mllt frame table".into()));
    }

    let mut frames = Vec::with_capacity(frame_count);
    let mut offset = 32;
    for _ in 0..frame_count {
        let f_offset = u64::from_le_bytes(data[offset..offset + 8].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
        let f_len = u32::from_le_bytes(data[offset + 8..offset + 12].try_into().map_err(|_| Mp3CutError::CacheError("Read error".into()))?);
        offset += 12;
        frames.push(FrameEntry {
            offset: f_offset,
            length: f_len,
        });
    }

    Ok(Mp3Metadata {
        sample_rate,
        samples_per_frame,
        channels,
        enc_delay,
        enc_padding,
        has_xing,
        frames,
    })
}

/// Scans an MP3 file and constructs its frame index and Xing/LAME metadata.
pub fn scan_mp3_frames(bytes: &[u8]) -> Result<Mp3Metadata, Mp3CutError> {
    if bytes.len() < 4 {
        return Err(Mp3CutError::NoFramesFound("File is too small to contain MP3 audio".into()));
    }

    // Skip ID3v2 header if present
    let mut scan_start = 0;
    if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let flags = bytes[5];
        let tag_size = ((bytes[6] as usize & 0x7F) << 21)
            | ((bytes[7] as usize & 0x7F) << 14)
            | ((bytes[8] as usize & 0x7F) << 7)
            | (bytes[9] as usize & 0x7F);
        let footer_size = if (flags & 0x10) != 0 { 10 } else { 0 };
        scan_start = 10 + tag_size + footer_size;
    }

    let mut scan_end = bytes.len();
    // Exclude ID3v1 footer if present
    if bytes.len() >= 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG" {
        scan_end = bytes.len() - 128;
    }

    let mut frames = Vec::new();
    let mut sample_rate = 0;
    let mut samples_per_frame = 0;
    let mut channels = 0;
    let mut enc_delay = 0;
    let mut enc_padding = 0;
    let mut has_xing = false;

    let mut offset = scan_start;
    while offset + 4 <= scan_end {
        if let Some(header) = parse_frame_header(&bytes[offset..]) {
            if offset + header.frame_size <= scan_end {
                // If this is the first frame, check for Xing / Info / LAME tag
                if frames.is_empty() {
                    sample_rate = header.sample_rate;
                    samples_per_frame = header.samples_per_frame;
                    channels = header.channels;

                    let side_info_len = match (header.version, header.channels) {
                        (MpegVersion::Mpeg1, 1) => 17,
                        (MpegVersion::Mpeg1, _) => 32,
                        (_, 1) => 9,
                        (_, _) => 17,
                    };
                    let xing_rel = 4 + (if header.has_crc { 2 } else { 0 }) + side_info_len;
                    if xing_rel + 4 <= header.frame_size {
                        let tag = &bytes[offset + xing_rel..offset + xing_rel + 4];
                        if tag == b"Xing" || tag == b"Info" {
                            has_xing = true;
                            // Parse Xing flags & fields
                            let mut x_ptr = offset + xing_rel + 4;
                            if x_ptr + 4 <= offset + header.frame_size {
                                let x_flags = u32::from_be_bytes([
                                    bytes[x_ptr],
                                    bytes[x_ptr + 1],
                                    bytes[x_ptr + 2],
                                    bytes[x_ptr + 3],
                                ]);
                                x_ptr += 4;
                                if (x_flags & 0x01) != 0 && x_ptr + 4 <= offset + header.frame_size {
                                    x_ptr += 4; // frames
                                }
                                if (x_flags & 0x02) != 0 && x_ptr + 4 <= offset + header.frame_size {
                                    x_ptr += 4; // bytes
                                }
                                if (x_flags & 0x04) != 0 && x_ptr + 100 <= offset + header.frame_size {
                                    x_ptr += 100; // TOC
                                }
                                if (x_flags & 0x08) != 0 && x_ptr + 4 <= offset + header.frame_size {
                                    x_ptr += 4; // quality
                                }
                                // Look for LAME tag
                                if x_ptr + 9 <= offset + header.frame_size && &bytes[x_ptr..x_ptr + 4] == b"LAME" {
                                    // 9 bytes version, 1 rev/vbr, 1 lowpass, 8 replaygain, 1 flags, 1 bitrate = 21 bytes
                                    if x_ptr + 24 <= offset + header.frame_size {
                                        let delay_bytes = &bytes[x_ptr + 21..x_ptr + 24];
                                        let b_d0 = delay_bytes[0] as u32;
                                        let b_d1 = delay_bytes[1] as u32;
                                        let b_d2 = delay_bytes[2] as u32;
                                        enc_delay = (b_d0 << 4) | (b_d1 >> 4);
                                        enc_padding = ((b_d1 & 0x0F) << 8) | b_d2;
                                    }
                                }
                            }
                        }
                    }
                }

                frames.push(FrameEntry {
                    offset: offset as u64,
                    length: header.frame_size as u32,
                });
                offset += header.frame_size;
                continue;
            }
        }
        offset += 1;
    }

    if frames.is_empty() {
        return Err(Mp3CutError::NoFramesFound("No valid MPEG Layer III audio frames identified".into()));
    }

    Ok(Mp3Metadata {
        sample_rate,
        samples_per_frame,
        channels,
        enc_delay,
        enc_padding,
        has_xing,
        frames,
    })
}

/// Computes the exact cache file path matching `CPAN/MP3/Cut/Gapless.pm:41`.
pub fn get_mllt_cache_path(file: &Path, cache_dir: &Path) -> Result<PathBuf, Mp3CutError> {
    let metadata = std::fs::metadata(file)?;
    let size = metadata.len();
    let mtime = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let file_str = file
        .to_str()
        .ok_or_else(|| Mp3CutError::InvalidFile("Path contains invalid UTF-8".into()))?;

    let key = format!("{}{}{}", file_str, size, mtime);
    let digest = Md5::digest(key.as_bytes());
    let filename = format!("{:x}.mllt", digest);
    Ok(cache_dir.join(filename))
}

pub struct GaplessCutStream {
    file: File,
    current_offset: u64,
    end_offset: u64,
    metadata: Mp3Metadata,
}

impl GaplessCutStream {
    pub fn open(
        file: &Path,
        cache_dir: Option<&Path>,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> Result<Self, Mp3CutError> {
        let cache_path = match cache_dir {
            Some(dir) => Some(get_mllt_cache_path(file, dir)?),
            None => None,
        };

        let metadata = if let Some(ref path) = cache_path {
            if path.exists() {
                match std::fs::read(path) {
                    Ok(data) => match decode_mllt_cache(&data) {
                        Ok(meta) => meta,
                        Err(_) => {
                            // Cache corrupted, rescan and rewrite
                            let file_bytes = std::fs::read(file)?;
                            let meta = scan_mp3_frames(&file_bytes)?;
                            let encoded = encode_mllt_cache(&meta);
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::write(path, encoded);
                            meta
                        }
                    },
                    Err(_) => {
                        let file_bytes = std::fs::read(file)?;
                        let meta = scan_mp3_frames(&file_bytes)?;
                        let encoded = encode_mllt_cache(&meta);
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(path, encoded);
                        meta
                    }
                }
            } else {
                let file_bytes = std::fs::read(file)?;
                let meta = scan_mp3_frames(&file_bytes)?;
                let encoded = encode_mllt_cache(&meta);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, encoded);
                meta
            }
        } else {
            let file_bytes = std::fs::read(file)?;
            scan_mp3_frames(&file_bytes)?
        };

        let audio_start_frame = if metadata.has_xing { 1 } else { 0 };
        let total_frames = metadata.frames.len();

        let (byte_start, byte_end) = if total_frames <= audio_start_frame {
            (0, 0)
        } else {
            let sample_rate = metadata.sample_rate.max(1) as f64;
            let samples_per_frame = metadata.samples_per_frame.max(1) as f64;

            let start_frame_idx = match start_ms {
                None => audio_start_frame,
                Some(s_ms) => {
                    let mut found = None;
                    for i in audio_start_frame..total_frames {
                        let audio_idx = i - audio_start_frame;
                        let frame_end_ms = ((audio_idx + 1) as f64 * samples_per_frame * 1000.0) / sample_rate;
                        if frame_end_ms > s_ms as f64 {
                            found = Some(i);
                            break;
                        }
                    }
                    found.unwrap_or(total_frames)
                }
            };

            let end_frame_idx = match end_ms {
                None => total_frames.saturating_sub(1),
                Some(e_ms) => {
                    let mut found = None;
                    for i in (audio_start_frame..total_frames).rev() {
                        let audio_idx = i - audio_start_frame;
                        let frame_start_ms = (audio_idx as f64 * samples_per_frame * 1000.0) / sample_rate;
                        if frame_start_ms < e_ms as f64 {
                            found = Some(i);
                            break;
                        }
                    }
                    found.unwrap_or(audio_start_frame.saturating_sub(1))
                }
            };

            if start_frame_idx <= end_frame_idx && end_frame_idx < total_frames {
                let start_off = metadata.frames[start_frame_idx].offset;
                let last_frame = &metadata.frames[end_frame_idx];
                let end_off = last_frame.offset + last_frame.length as u64;
                (start_off, end_off)
            } else {
                (0, 0)
            }
        };

        let mut f = File::open(file)?;
        if byte_start > 0 {
            f.seek(SeekFrom::Start(byte_start))?;
        }

        Ok(Self {
            file: f,
            current_offset: byte_start,
            end_offset: byte_end,
            metadata,
        })
    }

    pub fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, Mp3CutError> {
        if self.current_offset >= self.end_offset || buf.is_empty() {
            return Ok(0);
        }

        let remaining = (self.end_offset - self.current_offset) as usize;
        let to_read = buf.len().min(remaining);
        let n = self.file.read(&mut buf[..to_read])?;
        self.current_offset += n as u64;
        Ok(n)
    }

    pub fn metadata(&self) -> &Mp3Metadata {
        &self.metadata
    }
}

/// Opens a gapless MP3 cut stream across the C-ABI boundary.
///
/// # Arguments
/// - `file_path`: null-terminated UTF-8 C string path to the MP3 file.
/// - `cache_dir`: null-terminated UTF-8 C string path to cache directory, or NULL.
/// - `start_ms`: start time in milliseconds, or negative (< 0) for stream start.
/// - `end_ms`: end time in milliseconds, or negative (< 0) for stream end.
///
/// # Returns
/// Opaque pointer to `GaplessCutStream`, or null on error.
///
/// # Safety
/// Caller must ensure `file_path` points to a valid null-terminated UTF-8 C string,
/// and `cache_dir` (if non-null) points to a valid null-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_open(
    file_path: *const std::ffi::c_char,
    cache_dir: *const std::ffi::c_char,
    start_ms: i64,
    end_ms: i64,
) -> *mut GaplessCutStream {
    if file_path.is_null() {
        return std::ptr::null_mut();
    }

    // SAFETY: file_path is checked non-null and assumed to be a valid null-terminated C string.
    let c_file = unsafe { std::ffi::CStr::from_ptr(file_path) };
    let file_str = match c_file.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let file_p = Path::new(file_str);

    let cache_p = if !cache_dir.is_null() {
        // SAFETY: cache_dir is non-null and assumed to be a valid null-terminated C string.
        let c_cache = unsafe { std::ffi::CStr::from_ptr(cache_dir) };
        match c_cache.to_str() {
            Ok(s) => Some(Path::new(s)),
            Err(_) => return std::ptr::null_mut(),
        }
    } else {
        None
    };

    let s_ms = if start_ms >= 0 { Some(start_ms as u64) } else { None };
    let e_ms = if end_ms >= 0 { Some(end_ms as u64) } else { None };

    match GaplessCutStream::open(file_p, cache_p, s_ms, e_ms) {
        Ok(stream) => Box::into_raw(Box::new(stream)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Reads a chunk from `GaplessCutStream` into `buf`.
///
/// # Returns
/// Number of bytes read (0 on EOF), or -1 on error.
///
/// # Safety
/// Caller must ensure `stream` points to a live `GaplessCutStream` created by `lms_mp3cut_open`,
/// and `buf` points to valid mutable memory with at least `buf_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_read(
    stream: *mut GaplessCutStream,
    buf: *mut u8,
    buf_len: usize,
) -> isize {
    if stream.is_null() || buf.is_null() || buf_len == 0 {
        return -1;
    }

    // SAFETY: stream is verified non-null and references a heap-allocated GaplessCutStream.
    let stream_ref = unsafe { &mut *stream };
    // SAFETY: buf is verified non-null and caller guarantees valid write memory for buf_len bytes.
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };

    match stream_ref.read_chunk(slice) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

/// Closes and frees `GaplessCutStream`.
///
/// # Safety
/// Caller must ensure `stream` was returned by `lms_mp3cut_open` and has not been closed yet.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_close(stream: *mut GaplessCutStream) {
    if !stream.is_null() {
        // SAFETY: stream was allocated via Box::into_raw in lms_mp3cut_open and is reclaimed here for drop.
        unsafe {
            drop(Box::from_raw(stream));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_synthetic_mp3_frames(path: &std::path::Path, count: usize) {
        // MPEG-1 Layer III, 128 kbps, 44100 Hz, Stereo: header = 0xFF 0xFB 0x90 0x00, frame size = 417 bytes
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        let mut data = Vec::with_capacity(417 * count);
        for _ in 0..count {
            data.extend_from_slice(&frame);
        }
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn test_mp3cut_mllt_cache_creation_and_time_slice() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("synthetic.mp3");
        write_synthetic_mp3_frames(&mp3_path, 100);

        let mut cutter = GaplessCutStream::open(
            &mp3_path,
            Some(dir.path()),
            Some(500),
            Some(1500),
        ).unwrap();

        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = cutter.read_chunk(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert!(out.len() >= 37 * 417 && out.len() <= 41 * 417);
        let mllt_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().and_then(|s| s.to_str()) == Some("mllt"))
            .count();
        assert_eq!(mllt_count, 1);

        // Verify re-opening uses the cached .mllt file and produces identical output
        let mut cached_cutter = GaplessCutStream::open(
            &mp3_path,
            Some(dir.path()),
            Some(500),
            Some(1500),
        ).unwrap();
        let mut cached_out = Vec::new();
        loop {
            let n = cached_cutter.read_chunk(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            cached_out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, cached_out);
    }

    #[test]
    fn test_c_abi_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("c_abi_test.mp3");
        write_synthetic_mp3_frames(&mp3_path, 50);

        let c_file = std::ffi::CString::new(mp3_path.to_str().unwrap()).unwrap();
        let c_cache = std::ffi::CString::new(dir.path().to_str().unwrap()).unwrap();

        // SAFETY: C-strings are valid and pointers passed to lms_mp3cut_open
        let stream = unsafe {
            lms_mp3cut_open(c_file.as_ptr(), c_cache.as_ptr(), 0, 500)
        };
        assert!(!stream.is_null());

        let mut buf = [0u8; 1024];
        let mut total_read = 0;
        loop {
            // SAFETY: stream is non-null, buf is valid memory
            let n = unsafe { lms_mp3cut_read(stream, buf.as_mut_ptr(), buf.len()) };
            assert!(n >= 0);
            if n == 0 {
                break;
            }
            total_read += n as usize;
        }
        assert!(total_read > 0);

        // SAFETY: stream pointer was allocated by lms_mp3cut_open
        unsafe {
            lms_mp3cut_close(stream);
        }
    }

    #[test]
    fn test_xing_lame_header_parsing() {
        let mut xing_frame = vec![0u8; 417];
        // Header: MPEG-1 Layer III, 128 kbps, 44100 Hz, Stereo
        xing_frame[0] = 0xFF;
        xing_frame[1] = 0xFB;
        xing_frame[2] = 0x90;
        xing_frame[3] = 0x00;
        // Xing tag at offset 36 (4 header + 32 stereo side info)
        xing_frame[36..40].copy_from_slice(b"Xing");
        // Flags: 0x01 (frames present)
        xing_frame[40..44].copy_from_slice(&1u32.to_be_bytes());
        // Frame count: 10
        xing_frame[44..48].copy_from_slice(&10u32.to_be_bytes());
        // LAME tag at offset 48: "LAME3.99r"
        xing_frame[48..57].copy_from_slice(b"LAME3.99r");
        // Skip 21 bytes after LAME = offset 48 + 21 = 69
        // Delay 576 (0x240), Padding 1000 (0x3E8)
        // b0 = 0x24, b1 = 0x03, b2 = 0xE8
        xing_frame[69] = 0x24;
        xing_frame[70] = 0x03;
        xing_frame[71] = 0xE8;

        let meta = scan_mp3_frames(&xing_frame).unwrap();
        assert!(meta.has_xing);
        assert_eq!(meta.enc_delay, 576);
        assert_eq!(meta.enc_padding, 1000);
    }
}
