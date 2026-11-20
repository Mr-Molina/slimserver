//! JUSTIFICATION: C-ABI FFI bindings and raw memory buffer handling for MP3
//! streaming.
#![deny(unsafe_op_in_unsafe_fn)]

use md5::{Digest, Md5};
use std::collections::hash_map::RandomState;
use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

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
            ((144 * (bitrate_kbps as u64) * 1000) / (sample_rate as u64)) as usize
                + padding as usize
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
            return 0;
        }
        let raw_samples = (audio_frames as u128) * u128::from(self.samples_per_frame);
        let trim_samples = u128::from(self.enc_delay).saturating_add(u128::from(self.enc_padding));
        let net_samples = raw_samples.saturating_sub(trim_samples);
        let total_ms = (net_samples * 1000) / u128::from(self.sample_rate);
        u64::try_from(total_ms).unwrap_or(u64::MAX)
    }
}

const MLLT_MAGIC: [u8; 4] = *b"MLLT";
const MLLT_VERSION: u32 = 1;
const MLLT_HEADER_SIZE: usize = 32;
const MLLT_ENTRY_SIZE: usize = 12;
const MAX_MLLT_FRAMES: usize = 2_000_000;
const MAX_MLLT_FILE_BYTES: usize = MLLT_HEADER_SIZE + MAX_MLLT_FRAMES * MLLT_ENTRY_SIZE;
const MAX_MLLT_FRAME_LENGTH: u32 = 8192;
const MAX_INITIAL_FRAME_CAPACITY: usize = 65_536;
const MAX_INITIAL_MLLT_BYTES: usize = 1024 * 1024;

/// Serializes frame index into `.mllt` binary cache format.
pub fn encode_mllt_cache(meta: &Mp3Metadata) -> Result<Vec<u8>, Mp3CutError> {
    let frame_count = u32::try_from(meta.frames.len())
        .map_err(|_| Mp3CutError::CacheError("Frame count exceeds u32::MAX limit".into()))?;
    let total_len = meta
        .frames
        .len()
        .checked_mul(MLLT_ENTRY_SIZE)
        .and_then(|v| v.checked_add(MLLT_HEADER_SIZE))
        .ok_or_else(|| Mp3CutError::CacheError("MLLT cache size overflow".into()))?;
    let mut buf = Vec::with_capacity(total_len.min(MAX_INITIAL_MLLT_BYTES));
    buf.extend_from_slice(&MLLT_MAGIC);
    buf.extend_from_slice(&MLLT_VERSION.to_le_bytes());
    buf.extend_from_slice(&meta.sample_rate.to_le_bytes());
    buf.extend_from_slice(&meta.samples_per_frame.to_le_bytes());
    buf.extend_from_slice(&meta.channels.to_le_bytes());
    let flags: u16 = if meta.has_xing { 1 } else { 0 };
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&meta.enc_delay.to_le_bytes());
    buf.extend_from_slice(&meta.enc_padding.to_le_bytes());
    buf.extend_from_slice(&frame_count.to_le_bytes());

    for frame in &meta.frames {
        buf.extend_from_slice(&frame.offset.to_le_bytes());
        buf.extend_from_slice(&frame.length.to_le_bytes());
    }
    Ok(buf)
}

/// Deserializes frame index from `.mllt` binary cache format.
pub fn decode_mllt_cache(data: &[u8]) -> Result<Mp3Metadata, Mp3CutError> {
    if data.len() < MLLT_HEADER_SIZE {
        return Err(Mp3CutError::CacheError("Truncated .mllt header".into()));
    }
    if data[0..4] != MLLT_MAGIC {
        return Err(Mp3CutError::CacheError("Invalid .mllt magic bytes".into()));
    }
    let version = u32::from_le_bytes(
        data[4..8]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    if version != MLLT_VERSION {
        return Err(Mp3CutError::CacheError(format!(
            "Unsupported .mllt version {version}"
        )));
    }
    let sample_rate = u32::from_le_bytes(
        data[8..12]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    if sample_rate == 0 {
        return Err(Mp3CutError::CacheError(
            "Invalid .mllt sample_rate: must be > 0".into(),
        ));
    }
    let samples_per_frame = u32::from_le_bytes(
        data[12..16]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    if samples_per_frame != 576 && samples_per_frame != 1152 {
        return Err(Mp3CutError::CacheError(format!(
            "Invalid .mllt samples_per_frame {samples_per_frame}"
        )));
    }
    let channels = u16::from_le_bytes(
        data[16..18]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    if channels != 1 && channels != 2 {
        return Err(Mp3CutError::CacheError(format!(
            "Invalid .mllt channel count {channels}"
        )));
    }
    let flags = u16::from_le_bytes(
        data[18..20]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    let has_xing = (flags & 1) != 0;
    let enc_delay = u32::from_le_bytes(
        data[20..24]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    let enc_padding = u32::from_le_bytes(
        data[24..28]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    let frame_count_u32 = u32::from_le_bytes(
        data[28..32]
            .try_into()
            .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
    );
    let frame_count = usize::try_from(frame_count_u32)
        .map_err(|_| Mp3CutError::CacheError("Frame count exceeds addressable range".into()))?;
    if frame_count == 0 {
        return Err(Mp3CutError::CacheError("Empty .mllt frame table".into()));
    }
    if frame_count > MAX_MLLT_FRAMES {
        return Err(Mp3CutError::CacheError(format!(
            "Frame count {frame_count} exceeds maximum {MAX_MLLT_FRAMES}"
        )));
    }

    let expected_len = frame_count
        .checked_mul(MLLT_ENTRY_SIZE)
        .and_then(|v| v.checked_add(MLLT_HEADER_SIZE))
        .ok_or_else(|| Mp3CutError::CacheError("Invalid frame count overflow".into()))?;
    if data.len() != expected_len {
        return Err(Mp3CutError::CacheError(format!(
            "Invalid .mllt length: expected {expected_len}, got {}",
            data.len()
        )));
    }

    let mut frames = Vec::with_capacity(frame_count.min(MAX_INITIAL_FRAME_CAPACITY));
    let mut offset = MLLT_HEADER_SIZE;
    let mut prev_end: u64 = 0;
    for _ in 0..frame_count {
        let f_offset = u64::from_le_bytes(
            data[offset..offset + 8]
                .try_into()
                .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
        );
        let f_len = u32::from_le_bytes(
            data[offset + 8..offset + 12]
                .try_into()
                .map_err(|_| Mp3CutError::CacheError("Read error".into()))?,
        );
        if !(4..=MAX_MLLT_FRAME_LENGTH).contains(&f_len) {
            return Err(Mp3CutError::CacheError(format!(
                "Invalid .mllt frame length {f_len}"
            )));
        }
        let f_end = f_offset
            .checked_add(u64::from(f_len))
            .ok_or_else(|| Mp3CutError::CacheError("Frame offset overflow in .mllt".into()))?;
        if f_offset < prev_end {
            return Err(Mp3CutError::CacheError(
                "Non-monotonic .mllt frame offsets".into(),
            ));
        }
        prev_end = f_end;
        offset += MLLT_ENTRY_SIZE;
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

const STREAM_SCAN_WINDOW_BYTES: usize = 128 * 1024;
const STREAM_SCAN_LOOKAHEAD_BYTES: usize = 4096;

fn find_next_frame_header_confirmed(
    bytes: &[u8],
    at_eof: bool,
    expected: Option<(MpegVersion, u32)>,
) -> Option<(usize, FrameHeader)> {
    let limit = if at_eof {
        bytes.len().saturating_sub(3)
    } else {
        bytes.len().saturating_sub(STREAM_SCAN_LOOKAHEAD_BYTES)
    };
    let mut i = 0;
    while i < limit {
        let rel = bytes[i..limit].iter().position(|&b| b == 0xFF)?;
        i += rel;
        if (bytes[i + 1] & 0xE0) == 0xE0 {
            if let Some(header) = parse_frame_header(&bytes[i..]) {
                let matches_stream = match expected {
                    Some((ver, sr)) => header.version == ver && header.sample_rate == sr,
                    None => true,
                };
                if matches_stream {
                    if let Some(next_i) = i.checked_add(header.frame_size) {
                        if next_i == bytes.len() && at_eof {
                            return Some((i, header));
                        }
                        if next_i + 4 <= bytes.len() {
                            if let Some(next_header) = parse_frame_header(&bytes[next_i..]) {
                                if next_header.version == header.version
                                    && next_header.sample_rate == header.sample_rate
                                {
                                    return Some((i, header));
                                }
                            }
                        }
                        // Accept contiguous frame in locked stream at EOF/tag boundary.
                        if expected.is_some() && i == 0 && next_i <= bytes.len() {
                            return Some((i, header));
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Finds the next valid MPEG Layer III frame header in `bytes`.
///
/// Confirms candidates via multi-frame sync lock (`next_i` parses as a valid
/// `FrameHeader` with matching MPEG version, layer, and sample rate) or exact
/// terminal frame alignment (`next_i == bytes.len()`).
pub fn find_next_frame_header(bytes: &[u8]) -> Option<(usize, FrameHeader)> {
    find_next_frame_header_confirmed(bytes, true, None)
}

fn parse_first_frame_metadata(frame_bytes: &[u8], header: &FrameHeader) -> (bool, u32, u32) {
    let mut enc_delay = 0;
    let mut enc_padding = 0;
    let mut has_xing = false;

    let frame_end = header.frame_size.min(frame_bytes.len());
    let side_info_len = match (header.version, header.channels) {
        (MpegVersion::Mpeg1, 1) => 17,
        (MpegVersion::Mpeg1, _) => 32,
        (_, 1) => 9,
        (_, _) => 17,
    };
    let xing_rel = 4 + usize::from(header.has_crc) * 2 + side_info_len;
    if xing_rel + 4 <= frame_end {
        let tag = &frame_bytes[xing_rel..xing_rel + 4];
        if tag == b"Xing" || tag == b"Info" {
            has_xing = true;
            let mut x_ptr = xing_rel + 4;
            if x_ptr + 4 <= frame_end {
                let x_flags = u32::from_be_bytes([
                    frame_bytes[x_ptr],
                    frame_bytes[x_ptr + 1],
                    frame_bytes[x_ptr + 2],
                    frame_bytes[x_ptr + 3],
                ]);
                x_ptr += 4;
                let mut flags_valid = true;
                let xing_fields: [(u32, usize); 4] = [
                    (0x01, 4),   // frames
                    (0x02, 4),   // bytes
                    (0x04, 100), // TOC
                    (0x08, 4),   // quality
                ];
                for (flag_bit, field_len) in xing_fields {
                    if (x_flags & flag_bit) != 0 {
                        if x_ptr + field_len <= frame_end {
                            x_ptr += field_len;
                        } else {
                            flags_valid = false;
                            break;
                        }
                    }
                }
                if flags_valid
                    && x_ptr + 24 <= frame_end
                    && &frame_bytes[x_ptr..x_ptr + 4] == b"LAME"
                {
                    let delay_bytes = &frame_bytes[x_ptr + 21..x_ptr + 24];
                    let b_d0 = u32::from(delay_bytes[0]);
                    let b_d1 = u32::from(delay_bytes[1]);
                    let b_d2 = u32::from(delay_bytes[2]);
                    enc_delay = (b_d0 << 4) | (b_d1 >> 4);
                    enc_padding = ((b_d1 & 0x0F) << 8) | b_d2;
                }
            }
        }
    }

    let vbri_rel = 4 + 32;
    if !has_xing && vbri_rel + 26 <= frame_end && &frame_bytes[vbri_rel..vbri_rel + 4] == b"VBRI" {
        has_xing = true;
    }

    (has_xing, enc_delay, enc_padding)
}

fn read_exact_eintr<R: Read>(reader: &mut R, mut buf: &mut [u8]) -> Result<usize, Mp3CutError> {
    let mut total = 0usize;
    while !buf.is_empty() {
        match reader.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                buf = &mut buf[n..];
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Mp3CutError::Io(err)),
        }
    }
    Ok(total)
}

/// Scans an MP3 stream of known byte length and builds its frame index.
pub fn scan_mp3_frames_reader<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
) -> Result<Mp3Metadata, Mp3CutError> {
    if file_len < 4 {
        return Err(Mp3CutError::NoFramesFound(
            "File is too small to contain MP3 audio".into(),
        ));
    }

    reader.seek(SeekFrom::Start(0))?;
    let mut id3v2_hdr = [0u8; 10];
    let hdr_read = if file_len >= 10 {
        read_exact_eintr(reader, &mut id3v2_hdr)?
    } else {
        0
    };

    let mut scan_start: u64 = 0;
    if hdr_read == 10
        && id3v2_hdr.starts_with(b"ID3")
        && id3v2_hdr[6..10].iter().all(|&b| (b & 0x80) == 0)
    {
        let flags = id3v2_hdr[5];
        let tag_size = (u64::from(id3v2_hdr[6] & 0x7F) << 21)
            | (u64::from(id3v2_hdr[7] & 0x7F) << 14)
            | (u64::from(id3v2_hdr[8] & 0x7F) << 7)
            | u64::from(id3v2_hdr[9] & 0x7F);
        let footer_size: u64 = if (flags & 0x10) != 0 { 10 } else { 0 };
        scan_start = 10u64.saturating_add(tag_size).saturating_add(footer_size);
    }

    let mut scan_end = file_len;
    if file_len >= 128 && file_len - 128 >= scan_start {
        reader.seek(SeekFrom::Start(file_len - 128))?;
        let mut id3v1_tag = [0u8; 3];
        if read_exact_eintr(reader, &mut id3v1_tag)? == 3 && &id3v1_tag == b"TAG" {
            scan_end = file_len - 128;
        }
    }

    if scan_start.saturating_add(4) > scan_end {
        return Err(Mp3CutError::NoFramesFound(
            "No valid MPEG Layer III audio frames identified".into(),
        ));
    }

    reader.seek(SeekFrom::Start(scan_start))?;
    let audio_span = scan_end.saturating_sub(scan_start);
    let est_frames = usize::try_from(audio_span / 417)
        .unwrap_or(MAX_INITIAL_FRAME_CAPACITY)
        .min(MAX_INITIAL_FRAME_CAPACITY);
    let mut frames = Vec::with_capacity(est_frames);
    let mut expected_stream: Option<(MpegVersion, u32)> = None;
    let mut sample_rate = 0;
    let mut samples_per_frame = 0;
    let mut channels = 0;
    let mut enc_delay = 0;
    let mut enc_padding = 0;
    let mut has_xing = false;

    let mut buf = Vec::with_capacity(STREAM_SCAN_WINDOW_BYTES);
    let mut window_offset = scan_start;
    let mut file_cursor = scan_start;
    let mut pos = 0usize;

    loop {
        if pos > 0 && (buf.len() - pos <= STREAM_SCAN_LOOKAHEAD_BYTES && file_cursor < scan_end) {
            buf.drain(..pos);
            window_offset = window_offset.saturating_add(pos as u64);
            pos = 0;
        }
        while buf.len() < STREAM_SCAN_WINDOW_BYTES && file_cursor < scan_end {
            let max_chunk = (STREAM_SCAN_WINDOW_BYTES - buf.len())
                .min(usize::try_from(scan_end - file_cursor).unwrap_or(usize::MAX));
            if max_chunk == 0 {
                break;
            }
            let old_len = buf.len();
            buf.resize(old_len + max_chunk, 0);
            let n = read_exact_eintr(reader, &mut buf[old_len..old_len + max_chunk])?;
            buf.truncate(old_len + n);
            file_cursor = file_cursor.saturating_add(n as u64);
            if n == 0 {
                file_cursor = scan_end;
                break;
            }
        }

        let at_eof = file_cursor >= scan_end;
        if pos + 4 > buf.len() {
            break;
        }

        if let Some((rel_offset, header)) =
            find_next_frame_header_confirmed(&buf[pos..], at_eof, expected_stream)
        {
            let frame_pos = pos + rel_offset;
            let frame_offset = window_offset.saturating_add(frame_pos as u64);
            if frames.is_empty() {
                sample_rate = header.sample_rate;
                samples_per_frame = header.samples_per_frame;
                channels = header.channels;
                expected_stream = Some((header.version, header.sample_rate));
                (has_xing, enc_delay, enc_padding) =
                    parse_first_frame_metadata(&buf[frame_pos..], &header);
            }
            frames.push(FrameEntry {
                offset: frame_offset,
                length: header.frame_size as u32,
            });
            pos = frame_pos + header.frame_size;
            continue;
        }

        if at_eof {
            break;
        }
        let discard = buf.len().saturating_sub(STREAM_SCAN_LOOKAHEAD_BYTES);
        if discard <= pos {
            break;
        }
        buf.drain(..discard);
        window_offset = window_offset.saturating_add(discard as u64);
        pos = 0;
    }

    if frames.is_empty() {
        return Err(Mp3CutError::NoFramesFound(
            "No valid MPEG Layer III audio frames identified".into(),
        ));
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
    let mut cursor = std::io::Cursor::new(bytes);
    scan_mp3_frames_reader(&mut cursor, bytes.len() as u64)
}

static TEMP_CACHE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Computes the `.mllt` cache path from an already-opened MP3 file's metadata.
pub fn mllt_cache_path_from_metadata(
    file: &Path,
    cache_dir: &Path,
    metadata: &Metadata,
) -> Result<PathBuf, Mp3CutError> {
    let size = metadata.len();
    let mtime = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut hasher = Md5::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        hasher.update(file.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        let file_str = file
            .to_str()
            .ok_or_else(|| Mp3CutError::InvalidFile("Path contains invalid UTF-8".into()))?;
        hasher.update(file_str.as_bytes());
    }
    hasher.update(format!("{size}{mtime}").as_bytes());
    let digest = hasher.finalize();
    let filename = format!("{digest:x}.mllt");
    Ok(cache_dir.join(filename))
}

/// Computes the `.mllt` cache path matching `CPAN/MP3/Cut/Gapless.pm:43-47`.
///
/// Key formula: `md5_hex(format!("{file_str}{size}{mtime_secs}")) + ".mllt"`,
/// matching `Digest::MD5::md5_hex($self->{file} . $size . $mtime) . '.mllt'`.
/// Note: shared with `CPAN/MP3/Cut/Gapless.pm` (no delimiter separation and
/// whole-second mtime granularity); any delimiter/sub-second change must stay
/// synchronized with `CPAN/MP3/Cut/Gapless.pm`.
pub fn get_mllt_cache_path(file: &Path, cache_dir: &Path) -> Result<PathBuf, Mp3CutError> {
    let metadata = std::fs::metadata(file)?;
    mllt_cache_path_from_metadata(file, cache_dir, &metadata)
}

struct TempFileGuard(Option<PathBuf>);

impl TempFileGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn reject_symlink_cache_target(path: &Path) -> Result<(), Mp3CutError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Mp3CutError::CacheError(
            "Refusing symlinked .mllt cache target".into(),
        )),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Mp3CutError::Io(err)),
    }
}

#[cfg(unix)]
const fn unix_o_nonblock_flag() -> i32 {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        0x0004
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        0o4000
    }
}

#[cfg(unix)]
const fn unix_o_nofollow_flag() -> i32 {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        0x0100
    }
    #[cfg(target_os = "netbsd")]
    {
        0x0200
    }
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm")))]
    {
        0o100_000
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm"))
    )))]
    {
        0o400_000
    }
}

#[cfg(unix)]
fn is_eloop_error(err: &std::io::Error) -> bool {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    const ELOOP_CODE: i32 = 62;
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    const ELOOP_CODE: i32 = 40;
    err.raw_os_error() == Some(ELOOP_CODE)
}

/// Opens an MP3 source file non-blockingly and verifies it is a regular file.
pub fn open_source_mp3(path: &Path) -> Result<(File, Metadata), Mp3CutError> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        opts.custom_flags(unix_o_nonblock_flag());
    }
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(Mp3CutError::InvalidFile(
            "MP3 input path is not a regular file".into(),
        ));
    }
    Ok((file, meta))
}

fn random_temp_token() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let state = RandomState::new();
    let mut hasher = state.build_hasher();
    let seq = TEMP_CACHE_COUNTER.fetch_add(1, Ordering::Relaxed);
    hasher.write_u64(seq);
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// Atomically writes encoded `.mllt` bytes using a CSPRNG temporary file
/// and symlink checks.
pub fn write_mllt_cache_atomic(path: &Path, data: &[u8]) -> Result<(), Mp3CutError> {
    reject_symlink_cache_target(path)?;

    let parent = path
        .parent()
        .ok_or_else(|| Mp3CutError::CacheError("Cache path has no parent directory".into()))?;
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Mp3CutError::CacheError("Invalid .mllt cache filename".into()))?;

    let mut guard = TempFileGuard(None);
    let mut tmp_path_opt = None;
    let mut tmp_file_opt = None;

    for _ in 0..16 {
        let token = random_temp_token();
        let candidate = parent.join(format!(".{file_name}.tmp.{token:016x}"));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            opts.mode(0o600);
            opts.custom_flags(unix_o_nofollow_flag());
        }
        match opts.open(&candidate) {
            Ok(file) => {
                guard.0 = Some(candidate.clone());
                tmp_path_opt = Some(candidate);
                tmp_file_opt = Some(file);
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(Mp3CutError::Io(err)),
        }
    }

    let (Some(tmp_path), Some(mut tmp_file)) = (tmp_path_opt, tmp_file_opt) else {
        return Err(Mp3CutError::CacheError(
            "Unable to allocate unique .mllt temporary file".into(),
        ));
    };

    tmp_file.write_all(data)?;
    tmp_file.sync_all()?;
    drop(tmp_file);

    reject_symlink_cache_target(path)?;
    std::fs::rename(&tmp_path, path)?;
    guard.disarm();
    Ok(())
}

/// Safely reads an `.mllt` cache file using `O_NOFOLLOW | O_NONBLOCK`,
/// regular-file validation, and `MAX_MLLT_FILE_BYTES` bounds.
pub fn read_mllt_cache_safe(path: &Path) -> Result<Option<Vec<u8>>, Mp3CutError> {
    reject_symlink_cache_target(path)?;

    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        opts.custom_flags(unix_o_nofollow_flag() | unix_o_nonblock_flag());
    }

    let file = match opts.open(path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        #[cfg(unix)]
        Err(err) if is_eloop_error(&err) => {
            return Err(Mp3CutError::CacheError(
                "Refusing symlinked .mllt cache target".into(),
            ));
        }
        Err(_) => return Ok(None),
    };

    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(Mp3CutError::CacheError(
            "Refusing non-regular .mllt cache target".into(),
        ));
    }

    let max_bytes_u64 = MAX_MLLT_FILE_BYTES as u64;
    if meta.len() < MLLT_HEADER_SIZE as u64 || meta.len() > max_bytes_u64 {
        return Ok(None);
    }

    let cap = usize::try_from(meta.len()).unwrap_or(MAX_INITIAL_MLLT_BYTES);
    let mut data = Vec::with_capacity(cap.min(MAX_INITIAL_MLLT_BYTES));
    let mut limited = file.take(max_bytes_u64.saturating_add(1));
    if limited.read_to_end(&mut data).is_err() {
        return Ok(None);
    }
    if data.len() > MAX_MLLT_FILE_BYTES {
        return Ok(None);
    }
    Ok(Some(data))
}

/// Returns `true` when all indexed frames fall within `mp3_size`.
pub fn is_metadata_valid_for_file(meta: &Mp3Metadata, mp3_size: u64) -> bool {
    let Some(last) = meta.frames.last() else {
        return false;
    };
    let Some(last_end) = last.offset.checked_add(u64::from(last.length)) else {
        return false;
    };
    last_end <= mp3_size
}

fn load_or_build_metadata(
    mp3_file: &mut File,
    mp3_size: u64,
    cache_path: Option<&Path>,
) -> Result<Mp3Metadata, Mp3CutError> {
    if let Some(path) = cache_path {
        if let Some(data) = read_mllt_cache_safe(path)? {
            if let Ok(meta) = decode_mllt_cache(&data) {
                if is_metadata_valid_for_file(&meta, mp3_size) {
                    return Ok(meta);
                }
            }
        }
    }

    let meta = scan_mp3_frames_reader(mp3_file, mp3_size)?;
    if let Some(path) = cache_path {
        let encoded = encode_mllt_cache(&meta)?;
        if let Err(err) = write_mllt_cache_atomic(path, &encoded) {
            if matches!(err, Mp3CutError::CacheError(_)) {
                return Err(err);
            }
        }
    }
    Ok(meta)
}

pub struct GaplessCutStream {
    file: BufReader<File>,
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
        if let (Some(start), Some(end)) = (start_ms, end_ms) {
            if start > end {
                return Err(Mp3CutError::InvalidFile(format!(
                    "start_ms ({start}) must not exceed end_ms ({end})"
                )));
            }
        }

        let (mut mp3_file, mp3_meta) = open_source_mp3(file)?;
        let mp3_size = mp3_meta.len();
        let cache_path = match cache_dir {
            Some(dir) => Some(mllt_cache_path_from_metadata(file, dir, &mp3_meta)?),
            None => None,
        };

        let metadata = load_or_build_metadata(&mut mp3_file, mp3_size, cache_path.as_deref())?;

        let audio_start_frame = if metadata.has_xing { 1 } else { 0 };
        let total_frames = metadata.frames.len();
        let is_empty_slice = matches!((start_ms, end_ms), (Some(s), Some(e)) if s == e);

        let (byte_start, byte_end) = if total_frames <= audio_start_frame || is_empty_slice {
            (0, 0)
        } else {
            let meta = &metadata;
            let audio_frames = &metadata.frames[audio_start_frame..];
            let frame_count = audio_frames.len();

            let (start_idx, end_idx) = if meta.sample_rate > 0 && meta.samples_per_frame > 0 {
                let sample_rate = u128::from(meta.sample_rate);
                let spf = u128::from(meta.samples_per_frame);
                let raw_total_samples = (frame_count as u128) * spf;
                let delay_samples = u128::from(meta.enc_delay);
                let max_audio_sample =
                    raw_total_samples.saturating_sub(u128::from(meta.enc_padding));
                let max_end_idx = usize::try_from(max_audio_sample.div_ceil(spf))
                    .unwrap_or(usize::MAX)
                    .min(frame_count);

                let start_idx = match start_ms {
                    None => usize::try_from(delay_samples / spf)
                        .unwrap_or(usize::MAX)
                        .min(max_end_idx),
                    Some(ms) => {
                        let start_sample =
                            delay_samples.saturating_add((u128::from(ms) * sample_rate) / 1000);
                        usize::try_from(start_sample / spf)
                            .unwrap_or(usize::MAX)
                            .min(max_end_idx)
                    }
                };
                let end_idx = match end_ms {
                    None => max_end_idx,
                    Some(ms) => {
                        let end_sample = delay_samples
                            .saturating_add((u128::from(ms) * sample_rate).div_ceil(1000))
                            .min(max_audio_sample);
                        usize::try_from(end_sample.div_ceil(spf))
                            .unwrap_or(usize::MAX)
                            .min(max_end_idx)
                    }
                };
                (start_idx, end_idx)
            } else {
                (0, frame_count)
            };

            if start_idx < end_idx {
                let start_off = audio_frames[start_idx].offset;
                let last_frame = &audio_frames[end_idx - 1];
                let end_off = last_frame
                    .offset
                    .checked_add(u64::from(last_frame.length))
                    .ok_or_else(|| Mp3CutError::CacheError("Frame end offset overflow".into()))?;
                if start_off > end_off {
                    return Err(Mp3CutError::CacheError(
                        "Invalid non-monotonic frame offsets".into(),
                    ));
                }
                (start_off, end_off)
            } else {
                (0, 0)
            }
        };

        mp3_file.seek(SeekFrom::Start(byte_start))?;

        Ok(Self {
            file: BufReader::with_capacity(64 * 1024, mp3_file),
            current_offset: byte_start,
            end_offset: byte_end,
            metadata,
        })
    }

    pub fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, Mp3CutError> {
        if self.current_offset >= self.end_offset || buf.is_empty() {
            return Ok(0);
        }

        let Some(remaining) = self.end_offset.checked_sub(self.current_offset) else {
            return Ok(0);
        };
        let max_read = usize::try_from(remaining).unwrap_or(usize::MAX);
        let to_read = buf.len().min(max_read);
        let n = loop {
            match self.file.read(&mut buf[..to_read]) {
                Ok(bytes_read) => break bytes_read,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(Mp3CutError::Io(err)),
            }
        };
        let n_u64 =
            u64::try_from(n).map_err(|_| Mp3CutError::InvalidFile("Read count overflow".into()))?;
        self.current_offset = self
            .current_offset
            .checked_add(n_u64)
            .ok_or_else(|| Mp3CutError::InvalidFile("Stream offset overflow".into()))?;
        Ok(n)
    }

    pub fn metadata(&self) -> &Mp3Metadata {
        &self.metadata
    }
}

/// Opaque C-ABI handle wrapping a heap-allocated `GaplessCutStream`.
#[repr(C)]
pub struct LmsMp3CutStream {
    _private: [u8; 0],
}

const FFI_READ_SCRATCH_CAP: usize = 64 * 1024;
static LIVE_HANDLES: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();

fn live_handles() -> &'static Mutex<HashSet<usize>> {
    LIVE_HANDLES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_valid_stream_ptr(stream: *mut LmsMp3CutStream) -> bool {
    !stream.is_null() && (stream as usize).is_multiple_of(std::mem::align_of::<GaplessCutStream>())
}

fn is_registered_stream_ptr(stream: *mut LmsMp3CutStream) -> bool {
    if !is_valid_stream_ptr(stream) {
        return false;
    }
    let Ok(guard) = live_handles().lock() else {
        return false;
    };
    guard.contains(&(stream as usize))
}

fn cstr_to_pathbuf(cstr: &std::ffi::CStr) -> Result<PathBuf, ()> {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = cstr.to_bytes();
        if bytes.is_empty() {
            return Err(());
        }
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        let s = cstr.to_str().map_err(|_| ())?;
        if s.is_empty() {
            return Err(());
        }
        Ok(PathBuf::from(s))
    }
}

/// Opens a gapless MP3 cut stream across the C-ABI boundary.
///
/// # Arguments
/// - `file_path`: null-terminated C string path to the MP3 file.
/// - `cache_dir`: null-terminated C string path to cache directory, or NULL.
/// - `start_ms`: start time in milliseconds, or negative (< 0) for stream
///   start.
/// - `end_ms`: end time in milliseconds, or negative (< 0) for stream end.
///
/// # Returns
/// Opaque pointer to `LmsMp3CutStream`, or null on error.
///
/// # Safety
/// Caller must ensure `file_path` is non-null and points to a valid
/// null-terminated C string readable for this call, and `cache_dir` (if
/// non-null) likewise points to a valid null-terminated C string.
// JUSTIFICATION: C-ABI entrypoint returning an opaque stream handle for
// external FFI callers without exposing Rust struct layout.
// SAFETY:
// 1. Validity: `file_path` and non-null `cache_dir` are checked non-null
//    before `CStr::from_ptr` and assumed valid C strings per caller contract.
// 2. Alignment: `c_char` requires 1-byte alignment; `GaplessCutStream` is
//    boxed on the Rust heap before casting to `*mut LmsMp3CutStream`.
// 3. Bounds/Lifetime: `CStr::from_ptr` borrows caller strings only during this
//    call and does not retain raw pointers afterward.
// 4. Aliasing: Returned handle uniquely owns the heap-allocated stream and is
//    tracked in `LIVE_HANDLES`.
// 5. Thread/FFI safety: Panic-free `Result`-driven path; no unwinds cross ABI.
// 6. Ownership: Caller receives ownership and must free once with
//    `lms_mp3cut_close`.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_open(
    file_path: *const std::ffi::c_char,
    cache_dir: *const std::ffi::c_char,
    start_ms: i64,
    end_ms: i64,
) -> *mut LmsMp3CutStream {
    if file_path.is_null() {
        return std::ptr::null_mut();
    }

    // JUSTIFICATION: Convert C-ABI path string argument into Rust `PathBuf`.
    // SAFETY:
    // 1. Validity: `file_path` is verified non-null above.
    // 2. Alignment: `c_char` has 1-byte alignment.
    // 3. Bounds/Lifetime: Caller guarantees a null-terminated allocation for
    //    the duration of this call.
    // 4. Aliasing: Shared read-only borrow does not outlive this function.
    // 5. Thread/FFI safety: No mutation or unwinding occurs during inspection.
    // 6. Ownership: Borrowed C string remains owned by the caller.
    let c_file = unsafe { std::ffi::CStr::from_ptr(file_path) };
    let Ok(file_p) = cstr_to_pathbuf(c_file) else {
        return std::ptr::null_mut();
    };

    let cache_p = if !cache_dir.is_null() {
        // JUSTIFICATION: Convert optional C-ABI cache directory path string.
        // SAFETY:
        // 1. Validity: `cache_dir` is verified non-null above.
        // 2. Alignment: `c_char` has 1-byte alignment.
        // 3. Bounds/Lifetime: Caller guarantees a null-terminated string for
        //    this call's duration.
        // 4. Aliasing: Read-only borrow is dropped before returning.
        // 5. Thread/FFI safety: Panic-free path conversion only.
        // 6. Ownership: Caller retains ownership of the C string buffer.
        let c_cache = unsafe { std::ffi::CStr::from_ptr(cache_dir) };
        match cstr_to_pathbuf(c_cache) {
            Ok(p) => Some(p),
            Err(()) => return std::ptr::null_mut(),
        }
    } else {
        None
    };

    let s_ms = if start_ms >= 0 {
        Some(start_ms as u64)
    } else {
        None
    };
    let e_ms = if end_ms >= 0 {
        Some(end_ms as u64)
    } else {
        None
    };

    if let (Some(s), Some(e)) = (s_ms, e_ms) {
        if s > e {
            return std::ptr::null_mut();
        }
    }

    let Ok(stream) = GaplessCutStream::open(&file_p, cache_p.as_deref(), s_ms, e_ms) else {
        return std::ptr::null_mut();
    };

    let raw = Box::into_raw(Box::new(stream));
    let Ok(mut guard) = live_handles().lock() else {
        // JUSTIFICATION: Reclaim newly boxed stream if handle registry lock fails.
        // SAFETY:
        // 1. Validity: `raw` was just produced by `Box::into_raw`.
        // 2. Alignment: `raw` is properly aligned for `GaplessCutStream`.
        // 3. Bounds/Lifetime: Single ownership before pointer exposure to C.
        // 4. Aliasing: Unexposed pointer; no aliases exist.
        // 5. Thread/FFI safety: Panic-free drop before returning null.
        // 6. Ownership: Reclaims and frees local allocation cleanly.
        unsafe {
            drop(Box::from_raw(raw));
        }
        return std::ptr::null_mut();
    };
    guard.insert(raw as usize);
    raw.cast::<LmsMp3CutStream>()
}

/// Reads a chunk from `LmsMp3CutStream` into `buf`.
///
/// # Returns
/// Number of bytes read (0 on EOF or `buf_len == 0`), or -1 on error.
///
/// # Safety
/// Caller must ensure `stream` is a live handle returned by
/// `lms_mp3cut_open` and not yet closed, `buf` (when `buf_len > 0`) points to
/// at least `buf_len` writable bytes, `buf_len <= isize::MAX as usize`, and
/// no concurrent calls access `stream`.
// JUSTIFICATION: C-ABI stream read entrypoint copying initialized audio bytes
// into a caller-owned output buffer.
// SAFETY:
// 1. Validity: `stream` is verified non-null, aligned, and registered in
//    `LIVE_HANDLES`; `buf` is checked non-null whenever `buf_len > 0`.
// 2. Alignment: `stream` matches `align_of::<GaplessCutStream>()`; `buf` is
//    `*mut u8` with 1-byte alignment.
// 3. Bounds/Lifetime: `buf_len` is bounded to `<= isize::MAX as usize`; reads
//    go into an initialized Rust scratch buffer first, then `n <= buf_len`
//    bytes are copied via `copy_nonoverlapping`.
// 4. Aliasing: Caller contract guarantees exclusive access to `stream` and
//    `buf`; scratch buffer is disjoint from `buf`.
// 5. Thread/FFI safety: Panic-free `Result`-driven path with checked `isize`
//    conversion on the returned byte count.
// 6. Ownership: Stream handle and output buffer remain caller-owned.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_read(
    stream: *mut LmsMp3CutStream,
    buf: *mut u8,
    buf_len: usize,
) -> isize {
    if !is_registered_stream_ptr(stream) {
        return -1;
    }
    if buf_len == 0 {
        return 0;
    }
    if buf.is_null() || buf_len > isize::MAX as usize {
        return -1;
    }

    // JUSTIFICATION: Borrow boxed `GaplessCutStream` behind opaque C handle.
    // SAFETY:
    // 1. Validity: `stream` is non-null and registered in `LIVE_HANDLES`.
    // 2. Alignment: `stream` was verified aligned to `GaplessCutStream`.
    // 3. Bounds/Lifetime: Live `Box::into_raw` object; borrow ends before
    //    function return.
    // 4. Aliasing: Caller contract ensures exclusive access during read.
    // 5. Thread/FFI safety: No panic can occur inside `read_chunk`.
    // 6. Ownership: Borrowed mutably without reclaiming `Box`.
    let stream_ref = unsafe { &mut *stream.cast::<GaplessCutStream>() };

    let mut scratch = vec![0u8; buf_len.min(FFI_READ_SCRATCH_CAP)];
    let n = match stream_ref.read_chunk(&mut scratch) {
        Ok(bytes_read) => bytes_read,
        Err(_) => return -1,
    };
    if n > scratch.len() || n > buf_len {
        return -1;
    }
    if n > 0 {
        // JUSTIFICATION: Copy initialized scratch bytes into caller-owned C
        // buffer without creating `&mut [u8]` over uninitialized C memory.
        // SAFETY:
        // 1. Validity: `scratch.as_ptr()` points to `n` initialized bytes;
        //    `buf` was verified non-null above.
        // 2. Alignment: `u8` has 1-byte alignment for source and destination.
        // 3. Bounds/Lifetime: `n <= scratch.len() <= buf_len <= isize::MAX`;
        //    caller guarantees `buf` is writable for `buf_len` bytes.
        // 4. Aliasing: `scratch` is a fresh Rust heap allocation disjoint from
        //    caller-owned `buf`.
        // 5. Thread/FFI safety: Pure byte copy with no panics or unwinds.
        // 6. Ownership: Caller retains ownership of `buf`; `scratch` drops here.
        unsafe {
            std::ptr::copy_nonoverlapping(scratch.as_ptr(), buf, n);
        }
    }

    isize::try_from(n).unwrap_or(-1)
}

/// Closes and frees an `LmsMp3CutStream`.
///
/// # Safety
/// Caller must ensure `stream` is either null or a live handle returned by
/// `lms_mp3cut_open` that has not already been closed, and no other thread
/// accesses `stream` concurrently.
// JUSTIFICATION: C-ABI destructor freeing heap-allocated `GaplessCutStream`.
// SAFETY:
// 1. Validity: Null, misaligned, and unregistered/already-closed pointers are
//    ignored safely via `LIVE_HANDLES` membership removal.
// 2. Alignment: `is_valid_stream_ptr` verifies `GaplessCutStream` alignment.
// 3. Bounds/Lifetime: Single reclamation of `Box::into_raw` allocation.
// 4. Aliasing: Caller contract guarantees unique ownership and no active
//    borrows; atomic removal from `LIVE_HANDLES` blocks duplicate closes.
// 5. Thread/FFI safety: Panic-free drop path; no unwinds cross the C ABI.
// 6. Ownership: Reclaims ownership via `Box::from_raw` and drops immediately.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_close(stream: *mut LmsMp3CutStream) {
    if !is_valid_stream_ptr(stream) {
        return;
    }
    let removed = match live_handles().lock() {
        Ok(mut guard) => guard.remove(&(stream as usize)),
        Err(_) => false,
    };
    if removed {
        // JUSTIFICATION: Reclaim boxed `GaplessCutStream` created by
        // `lms_mp3cut_open` when the C caller closes the handle.
        // SAFETY:
        // 1. Validity: `stream` was removed from `LIVE_HANDLES`, proving it is
        //    a live, unclosed allocation from `lms_mp3cut_open`.
        // 2. Alignment: `stream` alignment for `GaplessCutStream` is verified.
        // 3. Bounds/Lifetime: Allocated by `Box::into_raw` in `lms_mp3cut_open`.
        // 4. Aliasing: Caller guarantees no concurrent `lms_mp3cut_read`.
        // 5. Thread/FFI safety: `Drop` for `GaplessCutStream` is panic-free.
        // 6. Ownership: Reconstructs `Box<GaplessCutStream>` once to drop it.
        unsafe {
            drop(Box::from_raw(stream.cast::<GaplessCutStream>()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_mp3_bytes(count: usize) -> Vec<u8> {
        // MPEG-1 Layer III, 128 kbps, 44100 Hz, Stereo:
        // header = 0xFF 0xFB 0x90 0x00, frame size = 417 bytes
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        let mut data = Vec::with_capacity(417 * count);
        for _ in 0..count {
            data.extend_from_slice(&frame);
        }
        data
    }

    fn write_synthetic_mp3_frames(path: &Path, count: usize) {
        std::fs::write(path, synthetic_mp3_bytes(count)).unwrap();
    }

    fn read_all_stream(stream: &mut GaplessCutStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read_chunk(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn test_mp3cut_mllt_cache_creation_and_time_slice() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("synthetic.mp3");
        write_synthetic_mp3_frames(&mp3_path, 100);

        let mut cutter =
            GaplessCutStream::open(&mp3_path, Some(dir.path()), Some(500), Some(1500)).unwrap();

        let out = read_all_stream(&mut cutter);
        assert!(out.len() >= 37 * 417 && out.len() <= 41 * 417);
        let mut mllt_count = 0usize;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) == Some("mllt") {
                mllt_count += 1;
            }
        }
        assert_eq!(mllt_count, 1);

        // Verify re-opening uses the cached .mllt file and identical output.
        let mut cached_cutter =
            GaplessCutStream::open(&mp3_path, Some(dir.path()), Some(500), Some(1500)).unwrap();
        let cached_out = read_all_stream(&mut cached_cutter);
        assert_eq!(out, cached_out);
    }

    #[test]
    fn test_c_abi_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("c_abi_test.mp3");
        write_synthetic_mp3_frames(&mp3_path, 50);

        let c_file = std::ffi::CString::new(mp3_path.to_str().unwrap()).unwrap();
        let c_cache = std::ffi::CString::new(dir.path().to_str().unwrap()).unwrap();

        // JUSTIFICATION: Exercise C-ABI stream creation in unit test.
        // SAFETY: `c_file` and `c_cache` are valid null-terminated UTF-8.
        let stream = unsafe { lms_mp3cut_open(c_file.as_ptr(), c_cache.as_ptr(), 0, 500) };
        assert!(!stream.is_null());

        let mut buf = [0u8; 1024];
        let mut total_read = 0;
        loop {
            // JUSTIFICATION: Exercise C-ABI stream read loop in unit test.
            // SAFETY: `stream` is live; `buf` is valid for `buf.len()` bytes.
            let n = unsafe { lms_mp3cut_read(stream, buf.as_mut_ptr(), buf.len()) };
            assert!(n >= 0);
            if n == 0 {
                break;
            }
            total_read += n as usize;
        }
        assert!(total_read > 0);

        // JUSTIFICATION: Exercise C-ABI stream cleanup in unit test.
        // SAFETY: `stream` was returned by `lms_mp3cut_open` and not closed.
        unsafe {
            lms_mp3cut_close(stream);
        }
    }

    #[test]
    fn test_c_abi_null_pointers_and_unaligned_handle() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("c_abi_null_test.mp3");
        write_synthetic_mp3_frames(&mp3_path, 10);
        let c_file = std::ffi::CString::new(mp3_path.to_str().unwrap()).unwrap();

        // JUSTIFICATION: Verify `lms_mp3cut_open` rejects NULL `file_path`.
        // SAFETY: Passing NULL triggers immediate guard return without deref.
        let null_open = unsafe { lms_mp3cut_open(std::ptr::null(), std::ptr::null(), 0, 500) };
        assert!(null_open.is_null());

        // JUSTIFICATION: Verify `lms_mp3cut_open` allows NULL `cache_dir`.
        // SAFETY: `c_file` is valid null-terminated UTF-8.
        let stream = unsafe { lms_mp3cut_open(c_file.as_ptr(), std::ptr::null(), 0, 500) };
        assert!(!stream.is_null());

        let mut buf = [0u8; 64];
        let null_stream: *mut LmsMp3CutStream = std::ptr::null_mut();
        // JUSTIFICATION: Verify `lms_mp3cut_read` rejects NULL `stream`.
        // SAFETY: Null stream returns -1 before dereference.
        let null_stream_read = unsafe { lms_mp3cut_read(null_stream, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(null_stream_read, -1);

        // JUSTIFICATION: Verify `lms_mp3cut_read` rejects NULL `buf` when len > 0.
        // SAFETY: Null buffer returns -1 before `from_raw_parts_mut`.
        let null_buf_read = unsafe { lms_mp3cut_read(stream, std::ptr::null_mut(), 16) };
        assert_eq!(null_buf_read, -1);

        // JUSTIFICATION: Verify `lms_mp3cut_read` handles zero `buf_len` safely.
        // SAFETY: Zero length returns 0 without constructing slice.
        let zero_len_read = unsafe { lms_mp3cut_read(stream, std::ptr::null_mut(), 0) };
        assert_eq!(zero_len_read, 0);

        if std::mem::align_of::<GaplessCutStream>() > 1 {
            let mut raw = [0u64; 4];
            let base = raw.as_mut_ptr().cast::<u8>();
            // JUSTIFICATION: Derive an intentionally misaligned pointer in bounds.
            // SAFETY: Offset +1 stays inside the 32-byte `raw` stack buffer.
            let unaligned = unsafe { base.add(1) }.cast::<LmsMp3CutStream>();
            // JUSTIFICATION: Verify alignment guard rejects misaligned handle.
            // SAFETY: Unaligned pointer is rejected before dereference.
            let unaligned_read = unsafe { lms_mp3cut_read(unaligned, buf.as_mut_ptr(), buf.len()) };
            assert_eq!(unaligned_read, -1);
            // JUSTIFICATION: Verify close ignores misaligned handle safely.
            // SAFETY: Unaligned pointer is rejected before `Box::from_raw`.
            unsafe {
                lms_mp3cut_close(unaligned);
            }
        }

        // JUSTIFICATION: Verify NULL close is a safe no-op and free live stream.
        // SAFETY: Null is ignored; `stream` is live and closed once.
        unsafe {
            lms_mp3cut_close(std::ptr::null_mut());
            lms_mp3cut_close(stream);
        }
    }

    #[test]
    fn test_c_abi_rejects_oversized_buf_len() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("c_abi_bounds.mp3");
        write_synthetic_mp3_frames(&mp3_path, 10);
        let c_file = std::ffi::CString::new(mp3_path.to_str().unwrap()).unwrap();

        // JUSTIFICATION: Open test stream for bounds validation check.
        // SAFETY: `c_file` is valid null-terminated UTF-8.
        let stream = unsafe { lms_mp3cut_open(c_file.as_ptr(), std::ptr::null(), -1, -1) };
        assert!(!stream.is_null());

        let mut buf = [0u8; 512];
        // JUSTIFICATION: Verify `buf_len = usize::MAX` is rejected cleanly.
        // SAFETY: Oversized `buf_len` is rejected before slice creation.
        let max_len_read = unsafe { lms_mp3cut_read(stream, buf.as_mut_ptr(), usize::MAX) };
        assert_eq!(max_len_read, -1);

        let just_over = (isize::MAX as usize) + 1;
        // JUSTIFICATION: Verify `buf_len > isize::MAX` boundary is rejected.
        // SAFETY: Oversized `buf_len` is rejected before slice creation.
        let over_isize_read = unsafe { lms_mp3cut_read(stream, buf.as_mut_ptr(), just_over) };
        assert_eq!(over_isize_read, -1);

        // Stream must remain usable after rejected reads.
        // JUSTIFICATION: Verify normal read succeeds after rejected calls.
        // SAFETY: `stream` is live and `buf` has 512 writable bytes.
        let n = unsafe { lms_mp3cut_read(stream, buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);

        // JUSTIFICATION: Clean up test stream handle.
        // SAFETY: `stream` was allocated by `lms_mp3cut_open` and not closed.
        unsafe {
            lms_mp3cut_close(stream);
        }
    }

    #[test]
    fn test_corrupt_mllt_cache_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("corrupt_cache.mp3");
        write_synthetic_mp3_frames(&mp3_path, 20);

        let cache_path = get_mllt_cache_path(&mp3_path, dir.path()).unwrap();
        std::fs::write(&cache_path, b"CORRUPT_MLLT_HEADER_BYTES").unwrap();

        let mut stream = GaplessCutStream::open(&mp3_path, Some(dir.path()), None, None).unwrap();
        let out = read_all_stream(&mut stream);
        assert_eq!(out.len(), 20 * 417);

        let healed = std::fs::read(&cache_path).unwrap();
        let decoded = decode_mllt_cache(&healed).unwrap();
        assert_eq!(decoded.frames.len(), 20);

        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".tmp."), "Unexpected temp file: {name}");
        }
    }

    #[test]
    fn test_terminal_frame_retention() {
        let dir = tempfile::tempdir().unwrap();

        // Single-frame MP3 ending right at EOF (`next_i == bytes.len()`).
        let single = synthetic_mp3_bytes(1);
        let single_meta = scan_mp3_frames(&single).unwrap();
        assert_eq!(single_meta.frames.len(), 1);
        assert_eq!(single_meta.frames[0].offset, 0);
        assert_eq!(single_meta.frames[0].length, 417);

        // Multi-frame MP3 followed by 128-byte ID3v1 TAG footer.
        let mut with_id3v1 = synthetic_mp3_bytes(4);
        let mut id3v1 = vec![0u8; 128];
        id3v1[0..3].copy_from_slice(b"TAG");
        with_id3v1.extend_from_slice(&id3v1);

        let mp3_path = dir.path().join("terminal.mp3");
        std::fs::write(&mp3_path, &with_id3v1).unwrap();

        let mut stream = GaplessCutStream::open(&mp3_path, None, None, None).unwrap();
        assert_eq!(stream.metadata().frames.len(), 4);
        let out = read_all_stream(&mut stream);
        assert_eq!(out.len(), 4 * 417);
    }

    #[test]
    fn test_concurrent_mllt_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("concurrent.mp3");
        write_synthetic_mp3_frames(&mp3_path, 50);

        let thread_count = 8usize;
        let barrier = std::sync::Barrier::new(thread_count);
        let outputs: Vec<Vec<u8>> = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(thread_count);
            for _ in 0..thread_count {
                handles.push(scope.spawn(|| {
                    barrier.wait();
                    let mut stream =
                        GaplessCutStream::open(&mp3_path, Some(dir.path()), Some(0), Some(1000))
                            .unwrap();
                    read_all_stream(&mut stream)
                }));
            }
            let mut results = Vec::with_capacity(thread_count);
            for handle in handles {
                results.push(handle.join().unwrap());
            }
            results
        });

        assert!(!outputs[0].is_empty());
        for out in &outputs[1..] {
            assert_eq!(out, &outputs[0]);
        }

        let cache_path = get_mllt_cache_path(&mp3_path, dir.path()).unwrap();
        let decoded = decode_mllt_cache(&std::fs::read(&cache_path).unwrap()).unwrap();
        assert_eq!(decoded.frames.len(), 50);

        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".tmp."), "Leftover temp file: {name}");
        }
    }

    #[test]
    fn test_u128_ms_to_frame_seek_math() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("seek_math.mp3");
        write_synthetic_mp3_frames(&mp3_path, 10);

        // Zero-length slice when start_ms == end_ms == 0.
        let mut empty_slice = GaplessCutStream::open(&mp3_path, None, Some(0), Some(0)).unwrap();
        assert_eq!(read_all_stream(&mut empty_slice).len(), 0);

        // At 44100 Hz, 1152 spf: 1 ms => ceil(44100 / 1_152_000) = 1 frame.
        let mut one_frame = GaplessCutStream::open(&mp3_path, None, Some(0), Some(1)).unwrap();
        assert_eq!(read_all_stream(&mut one_frame).len(), 417);
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_mllt_cache_target_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("symlink_test.mp3");
        write_synthetic_mp3_frames(&mp3_path, 10);

        let target_path = dir.path().join("protected.txt");
        std::fs::write(&target_path, b"do_not_overwrite").unwrap();

        let cache_path = get_mllt_cache_path(&mp3_path, dir.path()).unwrap();
        std::os::unix::fs::symlink(&target_path, &cache_path).unwrap();

        let res = GaplessCutStream::open(&mp3_path, Some(dir.path()), None, None);
        assert!(matches!(res, Err(Mp3CutError::CacheError(_))));
        assert_eq!(std::fs::read(&target_path).unwrap(), b"do_not_overwrite");
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

    #[test]
    fn test_c_abi_uninit_buf_inverted_range_and_double_close() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("c_abi_edge.mp3");
        write_synthetic_mp3_frames(&mp3_path, 10);
        let c_file = std::ffi::CString::new(mp3_path.to_str().unwrap()).unwrap();

        // JUSTIFICATION: Verify `lms_mp3cut_open` rejects `start_ms > end_ms`.
        // SAFETY: `c_file` is a valid null-terminated UTF-8 string.
        let inverted = unsafe { lms_mp3cut_open(c_file.as_ptr(), std::ptr::null(), 500, 100) };
        assert!(inverted.is_null());

        // JUSTIFICATION: Open valid C-ABI stream handle for uninit/close tests.
        // SAFETY: `c_file` is a valid null-terminated UTF-8 string.
        let stream = unsafe { lms_mp3cut_open(c_file.as_ptr(), std::ptr::null(), 0, 500) };
        assert!(!stream.is_null());

        let mut uninit_buf = std::mem::MaybeUninit::<[u8; 512]>::uninit();
        // JUSTIFICATION: Exercise `lms_mp3cut_read` writing into uninitialized
        // stack storage.
        // SAFETY: `stream` is live; `uninit_buf` provides 512 writable bytes.
        let n = unsafe { lms_mp3cut_read(stream, uninit_buf.as_mut_ptr().cast::<u8>(), 512) };
        assert!(n > 0 && (n as usize) <= 512);

        // JUSTIFICATION: Close live stream handle once, then verify duplicate
        // close and read-after-close are rejected cleanly.
        // SAFETY: First close frees `stream`; `LIVE_HANDLES` blocks UAF/double
        // free on subsequent calls.
        unsafe {
            lms_mp3cut_close(stream);
            lms_mp3cut_close(stream);
        }
        let after_close = {
            // JUSTIFICATION: Verify read after close returns error cleanly.
            // SAFETY: `LIVE_HANDLES` rejects closed pointer without deref.
            unsafe { lms_mp3cut_read(stream, uninit_buf.as_mut_ptr().cast::<u8>(), 64) }
        };
        assert_eq!(after_close, -1);
    }

    #[cfg(unix)]
    #[test]
    fn test_c_abi_non_utf8_unix_path() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = tempfile::tempdir().unwrap();
        let non_utf8_name = std::ffi::OsStr::from_bytes(b"track_\xff.mp3");
        let mp3_path = dir.path().join(non_utf8_name);
        write_synthetic_mp3_frames(&mp3_path, 4);

        let c_file = std::ffi::CString::new(mp3_path.as_os_str().as_bytes()).unwrap();
        let c_cache = std::ffi::CString::new(dir.path().as_os_str().as_bytes()).unwrap();
        // JUSTIFICATION: Exercise C-ABI stream open with non-UTF-8 Unix path.
        // SAFETY: `c_file` and `c_cache` are valid null-terminated C strings.
        let stream = unsafe { lms_mp3cut_open(c_file.as_ptr(), c_cache.as_ptr(), 0, 100) };
        assert!(!stream.is_null());

        let mut buf = [0u8; 1024];
        // JUSTIFICATION: Verify non-UTF-8 opened stream reads valid bytes.
        // SAFETY: `stream` is live and `buf` has 1024 writable bytes.
        let n = unsafe { lms_mp3cut_read(stream, buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);
        // JUSTIFICATION: Free C-ABI stream handle created in test.
        // SAFETY: `stream` is live and not previously closed.
        unsafe {
            lms_mp3cut_close(stream);
        }
    }

    #[test]
    fn test_mllt_rejects_empty_non_monotonic_and_rebuilds() {
        let empty_meta = Mp3Metadata {
            sample_rate: 44100,
            samples_per_frame: 1152,
            channels: 2,
            enc_delay: 0,
            enc_padding: 0,
            has_xing: false,
            frames: Vec::new(),
        };
        let empty_bytes = encode_mllt_cache(&empty_meta).unwrap();
        assert!(matches!(
            decode_mllt_cache(&empty_bytes),
            Err(Mp3CutError::CacheError(_))
        ));

        let bad_order_meta = Mp3Metadata {
            sample_rate: 44100,
            samples_per_frame: 1152,
            channels: 2,
            enc_delay: 0,
            enc_padding: 0,
            has_xing: false,
            frames: vec![
                FrameEntry {
                    offset: 417,
                    length: 417,
                },
                FrameEntry {
                    offset: 100,
                    length: 417,
                },
            ],
        };
        let bad_order_bytes = encode_mllt_cache(&bad_order_meta).unwrap();
        assert!(matches!(
            decode_mllt_cache(&bad_order_bytes),
            Err(Mp3CutError::CacheError(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("rebuild_empty.mp3");
        write_synthetic_mp3_frames(&mp3_path, 8);
        let cache_path = get_mllt_cache_path(&mp3_path, dir.path()).unwrap();
        std::fs::write(&cache_path, &empty_bytes).unwrap();

        let mut stream = GaplessCutStream::open(&mp3_path, Some(dir.path()), None, None).unwrap();
        assert_eq!(stream.metadata().frames.len(), 8);
        assert_eq!(read_all_stream(&mut stream).len(), 8 * 417);
    }

    #[test]
    fn test_rejects_false_sync_and_inconsistent_sample_rate() {
        // Construct garbage buffer with false sync followed by valid stream.
        let mut data = vec![0x11u8; 256];
        // False sync header at offset 16 (417-byte frame pointing to 0xAA).
        data[16] = 0xFF;
        data[17] = 0xFB;
        data[18] = 0x90;
        data[19] = 0x00;
        let valid = synthetic_mp3_bytes(3);
        let valid_start = data.len();
        data.extend_from_slice(&valid);
        // Append an inconsistent 48kHz MPEG-1 frame header at the tail.
        data.extend_from_slice(&[0xFF, 0xFB, 0x94, 0x00]);
        data.resize(data.len() + 380, 0);

        let meta = scan_mp3_frames(&data).unwrap();
        assert_eq!(meta.frames.len(), 3);
        assert_eq!(meta.frames[0].offset, valid_start as u64);
        assert_eq!(meta.sample_rate, 44100);

        // Pure false sync stream with zero confirmed frames must fail.
        let mut only_false = vec![0x22u8; 1024];
        only_false[10] = 0xFF;
        only_false[11] = 0xFB;
        only_false[12] = 0x90;
        only_false[13] = 0x00;
        assert!(matches!(
            scan_mp3_frames(&only_false),
            Err(Mp3CutError::NoFramesFound(_))
        ));
    }

    #[test]
    fn test_streaming_scan_exceeding_window_and_bufreader() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("large_stream.mp3");
        // 360 * 417 = 150,120 bytes (> 128 KiB scan window & 64 KiB BufReader).
        write_synthetic_mp3_frames(&mp3_path, 360);

        let mut stream = GaplessCutStream::open(&mp3_path, Some(dir.path()), None, None).unwrap();
        assert_eq!(stream.metadata().frames.len(), 360);
        let out = read_all_stream(&mut stream);
        assert_eq!(out.len(), 360 * 417);
    }

    #[test]
    fn test_streaming_scan_exact_lookahead_boundary_refill() {
        // Align frame 304's start to `pos == 127_072` (128 KiB - 4096 B lookahead):
        // 304 leading non-sync bytes + 303 * 417 = 126_655, plus 417 = 127_072.
        let mut data = vec![0u8; 304];
        data.extend_from_slice(&synthetic_mp3_bytes(320));
        assert!(data.len() > STREAM_SCAN_WINDOW_BYTES);
        assert_eq!(data.len(), 304 + 320 * 417);

        let meta = scan_mp3_frames(&data).unwrap();
        assert_eq!(meta.frames.len(), 320);
        assert_eq!(meta.frames[0].offset, 304);
        assert_eq!(
            meta.frames.last().unwrap().offset + u64::from(meta.frames.last().unwrap().length),
            data.len() as u64
        );
    }

    #[test]
    fn test_enc_delay_padding_and_start_ge_end_parity() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("gapless_trim.mp3");

        // Frame 0: Xing/LAME header with enc_delay = 1152 (1 full frame),
        // enc_padding = 1152 (1 full frame). Followed by 5 audio frames.
        let mut bytes = vec![0u8; 417];
        bytes[0..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        bytes[36..40].copy_from_slice(b"Xing");
        bytes[40..44].copy_from_slice(&1u32.to_be_bytes());
        bytes[44..48].copy_from_slice(&5u32.to_be_bytes());
        bytes[48..57].copy_from_slice(b"LAME3.99r");
        // 1152 = 0x480: d0 = 0x48, d1 = 0x04, d2 = 0x80
        bytes[69] = 0x48;
        bytes[70] = 0x04;
        bytes[71] = 0x80;
        bytes.extend_from_slice(&synthetic_mp3_bytes(5));
        std::fs::write(&mp3_path, &bytes).unwrap();

        let meta = scan_mp3_frames(&bytes).unwrap();
        // 5 audio frames * 1152 = 5760 raw samples; minus 2304 trim = 3456 samples.
        // 3456 * 1000 / 44100 = 78 ms.
        assert_eq!(meta.duration_ms(), 78);

        // Full cut skips 1 pure-delay leading frame and 1 pure-padding trailing frame.
        let mut trimmed = GaplessCutStream::open(&mp3_path, None, None, None).unwrap();
        assert_eq!(read_all_stream(&mut trimmed).len(), 3 * 417);

        // Non-zero equal start_ms == end_ms returns empty slice (0 bytes).
        let mut equal_non_zero =
            GaplessCutStream::open(&mp3_path, None, Some(25), Some(25)).unwrap();
        assert_eq!(read_all_stream(&mut equal_non_zero).len(), 0);

        // Inverted range start_ms > end_ms returns error.
        let inverted = GaplessCutStream::open(&mp3_path, None, Some(50), Some(10));
        assert!(matches!(inverted, Err(Mp3CutError::InvalidFile(_))));
    }
}
