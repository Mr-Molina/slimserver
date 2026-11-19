//! JUSTIFICATION: C-ABI FFI bindings and raw memory buffer handling for MP3
//! streaming.
#![deny(unsafe_op_in_unsafe_fn)]

use md5::{Digest, Md5};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
            0
        } else {
            let total_ms = (audio_frames as u128) * (self.samples_per_frame as u128) * 1000
                / (self.sample_rate as u128);
            u64::try_from(total_ms).unwrap_or(u64::MAX)
        }
    }
}

const MLLT_MAGIC: [u8; 4] = *b"MLLT";
const MLLT_VERSION: u32 = 1;
const MLLT_HEADER_SIZE: usize = 32;
const MLLT_ENTRY_SIZE: usize = 12;
const MAX_MLLT_FRAMES: usize = 2_000_000;
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

const MAX_CANDIDATE_RESCAN_STEPS: usize = 64;
const MAX_CANDIDATE_RESCAN_BYTES: usize = 64 * 1024;

/// Finds the next valid MPEG Layer III frame header in `bytes`.
///
/// Confirms candidates when `next_i == bytes.len()` (terminal frame) or when
/// `next_i` parses as a valid `FrameHeader` with matching MPEG version, layer,
/// and sample rate, falling back to the first valid candidate within a bounded
/// rescan window so repeated sync-like streams remain `O(N)`.
pub fn find_next_frame_header(bytes: &[u8]) -> Option<(usize, FrameHeader)> {
    let mut first_candidate: Option<(usize, FrameHeader)> = None;
    let mut first_candidate_idx: Option<usize> = None;
    let mut rescan_steps: usize = 0;
    let limit = bytes.len().saturating_sub(3);
    let mut i = 0;
    while i < limit {
        let scan_limit = match first_candidate_idx {
            Some(start_i) => start_i
                .saturating_add(MAX_CANDIDATE_RESCAN_BYTES)
                .min(limit),
            None => limit,
        };
        if i >= scan_limit {
            return first_candidate;
        }
        let Some(rel) = bytes[i..scan_limit].iter().position(|&b| b == 0xFF) else {
            return first_candidate;
        };
        i += rel;
        if (bytes[i + 1] & 0xE0) == 0xE0 {
            if first_candidate_idx.is_some() {
                rescan_steps += 1;
                if rescan_steps > MAX_CANDIDATE_RESCAN_STEPS {
                    return first_candidate;
                }
            }
            if let Some(header) = parse_frame_header(&bytes[i..]) {
                if let Some(next_i) = i.checked_add(header.frame_size) {
                    if next_i <= bytes.len() {
                        if next_i == bytes.len() {
                            return Some((i, header));
                        }
                        if let Some(next_header) = parse_frame_header(&bytes[next_i..]) {
                            if next_header.version == header.version
                                && next_header.sample_rate == header.sample_rate
                            {
                                return Some((i, header));
                            }
                        }
                        if first_candidate.is_none() {
                            first_candidate = Some((i, header));
                            first_candidate_idx = Some(i);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    first_candidate
}

/// Scans an MP3 file and constructs its frame index and Xing/LAME metadata.
pub fn scan_mp3_frames(bytes: &[u8]) -> Result<Mp3Metadata, Mp3CutError> {
    if bytes.len() < 4 {
        return Err(Mp3CutError::NoFramesFound(
            "File is too small to contain MP3 audio".into(),
        ));
    }

    // Skip ID3v2 header if present and synchsafe size bytes are valid.
    let mut scan_start = 0;
    if bytes.starts_with(b"ID3")
        && bytes.len() >= 10
        && bytes[6..10].iter().all(|&b| (b & 0x80) == 0)
    {
        let flags = bytes[5];
        let tag_size = (usize::from(bytes[6] & 0x7F) << 21)
            | (usize::from(bytes[7] & 0x7F) << 14)
            | (usize::from(bytes[8] & 0x7F) << 7)
            | usize::from(bytes[9] & 0x7F);
        let footer_size = if (flags & 0x10) != 0 { 10 } else { 0 };
        scan_start = 10usize.saturating_add(tag_size).saturating_add(footer_size);
    }

    let mut scan_end = bytes.len();
    // Exclude ID3v1 footer if present
    if bytes.len() >= 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG" {
        scan_end = bytes.len() - 128;
    }

    let audio_span = scan_end.saturating_sub(scan_start);
    let mut frames = Vec::with_capacity((audio_span / 417).min(MAX_INITIAL_FRAME_CAPACITY));
    let mut sample_rate = 0;
    let mut samples_per_frame = 0;
    let mut channels = 0;
    let mut enc_delay = 0;
    let mut enc_padding = 0;
    let mut has_xing = false;

    let mut offset = scan_start;
    while offset + 4 <= scan_end {
        if let Some((rel_offset, header)) = find_next_frame_header(&bytes[offset..scan_end]) {
            let frame_offset = offset + rel_offset;
            // If this is the first frame, check for Xing / Info / LAME / VBRI.
            if frames.is_empty() {
                sample_rate = header.sample_rate;
                samples_per_frame = header.samples_per_frame;
                channels = header.channels;

                let frame_end = frame_offset + header.frame_size;
                let side_info_len = match (header.version, header.channels) {
                    (MpegVersion::Mpeg1, 1) => 17,
                    (MpegVersion::Mpeg1, _) => 32,
                    (_, 1) => 9,
                    (_, _) => 17,
                };
                let xing_rel = 4 + usize::from(header.has_crc) * 2 + side_info_len;
                if xing_rel + 4 <= header.frame_size {
                    let tag = &bytes[frame_offset + xing_rel..frame_offset + xing_rel + 4];
                    if tag == b"Xing" || tag == b"Info" {
                        has_xing = true;
                        // Parse Xing flags sequentially without desyncing.
                        let mut x_ptr = frame_offset + xing_rel + 4;
                        if x_ptr + 4 <= frame_end {
                            let x_flags = u32::from_be_bytes([
                                bytes[x_ptr],
                                bytes[x_ptr + 1],
                                bytes[x_ptr + 2],
                                bytes[x_ptr + 3],
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
                            // Look for LAME tag only if Xing flags stayed in sync.
                            if flags_valid
                                && x_ptr + 24 <= frame_end
                                && &bytes[x_ptr..x_ptr + 4] == b"LAME"
                            {
                                // 9B version, 1B rev/vbr, 1B lowpass, 8B rg,
                                // 1B flags, 1B bitrate = 21 bytes before delay.
                                let delay_bytes = &bytes[x_ptr + 21..x_ptr + 24];
                                let b_d0 = u32::from(delay_bytes[0]);
                                let b_d1 = u32::from(delay_bytes[1]);
                                let b_d2 = u32::from(delay_bytes[2]);
                                enc_delay = (b_d0 << 4) | (b_d1 >> 4);
                                enc_padding = ((b_d1 & 0x0F) << 8) | b_d2;
                            }
                        }
                    }
                }

                // Check for Fraunhofer VBRI header at fixed offset 36 (4 + 32).
                let vbri_rel = 4 + 32;
                if !has_xing
                    && vbri_rel + 26 <= header.frame_size
                    && &bytes[frame_offset + vbri_rel..frame_offset + vbri_rel + 4] == b"VBRI"
                {
                    has_xing = true;
                }
            }

            frames.push(FrameEntry {
                offset: frame_offset as u64,
                length: header.frame_size as u32,
            });
            offset = frame_offset + header.frame_size;
            continue;
        }
        break;
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

static TEMP_CACHE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Computes the `.mllt` cache path matching `CPAN/MP3/Cut/Gapless.pm:43-47`.
///
/// Key formula: `md5_hex(format!("{file_str}{size}{mtime_secs}")) + ".mllt"`,
/// matching `Digest::MD5::md5_hex($self->{file} . $size . $mtime) . '.mllt'`.
/// Note: shared with `CPAN/MP3/Cut/Gapless.pm` (no delimiter separation and
/// whole-second mtime granularity); any delimiter/sub-second change must stay
/// synchronized with `CPAN/MP3/Cut/Gapless.pm`.
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

    let key = format!("{file_str}{size}{mtime}");
    let digest = Md5::digest(key.as_bytes());
    let filename = format!("{digest:x}.mllt");
    Ok(cache_dir.join(filename))
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

fn write_mllt_cache_atomic(path: &Path, data: &[u8]) -> Result<(), Mp3CutError> {
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
    let pid = std::process::id();
    let seq = TEMP_CACHE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{pid}.{seq}"));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }

    let mut guard = TempFileGuard(None);
    {
        let mut tmp_file = opts.open(&tmp_path)?;
        guard.0 = Some(tmp_path.clone());
        tmp_file.write_all(data)?;
        tmp_file.sync_all()?;
    }

    reject_symlink_cache_target(path)?;
    std::fs::rename(&tmp_path, path)?;
    guard.disarm();
    Ok(())
}

fn load_or_build_metadata(
    file: &Path,
    cache_path: Option<&Path>,
) -> Result<Mp3Metadata, Mp3CutError> {
    let Some(path) = cache_path else {
        let file_bytes = std::fs::read(file)?;
        return scan_mp3_frames(&file_bytes);
    };

    reject_symlink_cache_target(path)?;
    if let Ok(data) = std::fs::read(path) {
        if let Ok(meta) = decode_mllt_cache(&data) {
            return Ok(meta);
        }
    }

    let file_bytes = std::fs::read(file)?;
    let meta = scan_mp3_frames(&file_bytes)?;
    let encoded = encode_mllt_cache(&meta)?;
    if let Err(err) = write_mllt_cache_atomic(path, &encoded) {
        if matches!(err, Mp3CutError::CacheError(_)) {
            return Err(err);
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
        let cache_path = match cache_dir {
            Some(dir) => Some(get_mllt_cache_path(file, dir)?),
            None => None,
        };

        let metadata = load_or_build_metadata(file, cache_path.as_deref())?;

        let audio_start_frame = if metadata.has_xing { 1 } else { 0 };
        let total_frames = metadata.frames.len();

        let (byte_start, byte_end) = if total_frames <= audio_start_frame {
            (0, 0)
        } else {
            let meta = &metadata;
            let audio_frames = &metadata.frames[audio_start_frame..];
            let frame_count = audio_frames.len();

            let (start_idx, end_idx) = if meta.sample_rate > 0 && meta.samples_per_frame > 0 {
                let sample_rate = u128::from(meta.sample_rate);
                let denom = 1000u128 * u128::from(meta.samples_per_frame);
                let start_idx = match start_ms {
                    None => 0,
                    Some(ms) => {
                        let frame_idx = (u128::from(ms) * sample_rate) / denom;
                        usize::try_from(frame_idx)
                            .unwrap_or(usize::MAX)
                            .min(frame_count)
                    }
                };
                let end_idx = match end_ms {
                    None => frame_count,
                    Some(ms) => {
                        let numer = u128::from(ms) * sample_rate;
                        let frame_idx = numer.div_ceil(denom);
                        usize::try_from(frame_idx)
                            .unwrap_or(usize::MAX)
                            .min(frame_count)
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

        let mut f = File::open(file)?;
        if byte_start > 0 {
            f.seek(SeekFrom::Start(byte_start))?;
        }

        Ok(Self {
            file: BufReader::with_capacity(64 * 1024, f),
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
        let n = self.file.read(&mut buf[..to_read])?;
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

fn is_valid_stream_ptr(stream: *mut LmsMp3CutStream) -> bool {
    !stream.is_null() && (stream as usize).is_multiple_of(std::mem::align_of::<GaplessCutStream>())
}

/// Opens a gapless MP3 cut stream across the C-ABI boundary.
///
/// # Arguments
/// - `file_path`: null-terminated UTF-8 C string path to the MP3 file.
/// - `cache_dir`: null-terminated UTF-8 C string path to cache directory, or
///   NULL.
/// - `start_ms`: start time in milliseconds, or negative (< 0) for stream
///   start.
/// - `end_ms`: end time in milliseconds, or negative (< 0) for stream end.
///
/// # Returns
/// Opaque pointer to `LmsMp3CutStream`, or null on error.
///
/// # Safety
/// Caller must ensure `file_path` is non-null and points to a valid
/// null-terminated UTF-8 C string readable for this call, and `cache_dir`
/// (if non-null) likewise points to a valid null-terminated UTF-8 C string.
// JUSTIFICATION: C-ABI entrypoint returning an opaque stream handle for
// external FFI callers without exposing Rust struct layout.
// SAFETY:
// 1. Validity: `file_path` and non-null `cache_dir` are checked non-null
//    before `CStr::from_ptr` and assumed valid C strings per caller contract.
// 2. Alignment: `c_char` requires 1-byte alignment; `GaplessCutStream` is
//    boxed on the Rust heap before casting to `*mut LmsMp3CutStream`.
// 3. Bounds/Lifetime: `CStr::from_ptr` borrows caller strings only during this
//    call and does not retain raw pointers afterward.
// 4. Aliasing: Returned handle uniquely owns the heap-allocated stream.
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

    // JUSTIFICATION: Convert C-ABI path string argument into Rust `&str`.
    // SAFETY:
    // 1. Validity: `file_path` is verified non-null above.
    // 2. Alignment: `c_char` has 1-byte alignment.
    // 3. Bounds/Lifetime: Caller guarantees a null-terminated allocation for
    //    the duration of this call.
    // 4. Aliasing: Shared read-only borrow does not outlive this function.
    // 5. Thread/FFI safety: No mutation or unwinding occurs during inspection.
    // 6. Ownership: Borrowed C string remains owned by the caller.
    let c_file = unsafe { std::ffi::CStr::from_ptr(file_path) };
    let Ok(file_str) = c_file.to_str() else {
        return std::ptr::null_mut();
    };
    let file_p = Path::new(file_str);

    let cache_p = if !cache_dir.is_null() {
        // JUSTIFICATION: Convert optional C-ABI cache directory path string.
        // SAFETY:
        // 1. Validity: `cache_dir` is verified non-null above.
        // 2. Alignment: `c_char` has 1-byte alignment.
        // 3. Bounds/Lifetime: Caller guarantees a null-terminated string for
        //    this call's duration.
        // 4. Aliasing: Read-only borrow is dropped before returning.
        // 5. Thread/FFI safety: Panic-free UTF-8 validation only.
        // 6. Ownership: Caller retains ownership of the C string buffer.
        let c_cache = unsafe { std::ffi::CStr::from_ptr(cache_dir) };
        match c_cache.to_str() {
            Ok(s) => Some(Path::new(s)),
            Err(_) => return std::ptr::null_mut(),
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

    match GaplessCutStream::open(file_p, cache_p, s_ms, e_ms) {
        Ok(stream) => Box::into_raw(Box::new(stream)).cast::<LmsMp3CutStream>(),
        Err(_) => std::ptr::null_mut(),
    }
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
// JUSTIFICATION: C-ABI stream read entrypoint writing cut audio bytes into a
// caller-owned output buffer.
// SAFETY:
// 1. Validity: `stream` is checked non-null and aligned; `buf` is checked
//    non-null whenever `buf_len > 0`.
// 2. Alignment: `stream` matches `align_of::<GaplessCutStream>()`; `buf` is
//    `*mut u8` with 1-byte alignment.
// 3. Bounds/Lifetime: `buf_len` is bounded to `<= isize::MAX as usize` before
//    `from_raw_parts_mut`, and the slice does not outlive this call.
// 4. Aliasing: Caller contract guarantees exclusive access to `stream` and
//    `buf`.
// 5. Thread/FFI safety: Panic-free `Result`-driven path with checked `isize`
//    conversion on the returned byte count.
// 6. Ownership: Stream handle and output buffer remain caller-owned.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_read(
    stream: *mut LmsMp3CutStream,
    buf: *mut u8,
    buf_len: usize,
) -> isize {
    if !is_valid_stream_ptr(stream) {
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
    // 1. Validity: `stream` was verified non-null by `is_valid_stream_ptr`.
    // 2. Alignment: `stream` was verified aligned to `GaplessCutStream`.
    // 3. Bounds/Lifetime: Live `Box::into_raw` object; borrow ends before
    //    function return.
    // 4. Aliasing: Caller contract ensures exclusive access during read.
    // 5. Thread/FFI safety: No panic can occur inside `read_chunk`.
    // 6. Ownership: Borrowed mutably without reclaiming `Box`.
    let stream_ref = unsafe { &mut *stream.cast::<GaplessCutStream>() };

    // JUSTIFICATION: Construct bounded mutable slice over FFI output buffer.
    // SAFETY:
    // 1. Validity: `buf` is verified non-null and `buf_len > 0` above.
    // 2. Alignment: `u8` has 1-byte alignment.
    // 3. Bounds/Lifetime: `buf_len <= isize::MAX as usize` verified above;
    //    caller guarantees `buf_len` writable bytes for this call.
    // 4. Aliasing: Caller guarantees `buf` is unaliased for this call.
    // 5. Thread/FFI safety: Slice is dropped before returning to C.
    // 6. Ownership: Buffer remains owned by the caller.
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_len) };

    match stream_ref.read_chunk(slice) {
        Ok(n) => isize::try_from(n).unwrap_or(-1),
        Err(_) => -1,
    }
}

/// Closes and frees an `LmsMp3CutStream`.
///
/// # Safety
/// Caller must ensure `stream` is either null or a live handle returned by
/// `lms_mp3cut_open` that has not already been closed, and no other thread
/// accesses `stream` concurrently.
// JUSTIFICATION: C-ABI destructor freeing heap-allocated `GaplessCutStream`.
// SAFETY:
// 1. Validity: Null pointers are ignored safely; non-null pointers are
//    assumed live handles from `lms_mp3cut_open`.
// 2. Alignment: `is_valid_stream_ptr` verifies `GaplessCutStream` alignment.
// 3. Bounds/Lifetime: Single reclamation of `Box::into_raw` allocation.
// 4. Aliasing: Caller contract guarantees unique ownership and no active
//    borrows.
// 5. Thread/FFI safety: Panic-free drop path; no unwinds cross the C ABI.
// 6. Ownership: Reclaims ownership via `Box::from_raw` and drops immediately.
#[no_mangle]
pub unsafe extern "C" fn lms_mp3cut_close(stream: *mut LmsMp3CutStream) {
    if is_valid_stream_ptr(stream) {
        // JUSTIFICATION: Reclaim boxed `GaplessCutStream` created by
        // `lms_mp3cut_open` when the C caller closes the handle.
        // SAFETY:
        // 1. Validity: `stream` is verified non-null above.
        // 2. Alignment: `stream` alignment for `GaplessCutStream` is verified.
        // 3. Bounds/Lifetime: Allocated by `Box::into_raw` in `lms_mp3cut_open`.
        // 4. Aliasing: Caller guarantees no concurrent or duplicate close.
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
}
