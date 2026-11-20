#![deny(unsafe_op_in_unsafe_fn)]

use base64::Engine;
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::file::FileType;
use lofty::file::TaggedFile;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::TagType;
use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

const ASF_HEADER_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_DATA_GUID: [u8; 16] = [
    0x36, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_FILE_PROPERTIES_GUID: [u8; 16] = [
    0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
const ASF_STREAM_PROPERTIES_GUID: [u8; 16] = [
    0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
const ASF_AUDIO_MEDIA_GUID: [u8; 16] = [
    0x40, 0x9E, 0x69, 0xF8, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B,
];
const ASF_VIDEO_MEDIA_GUID: [u8; 16] = [
    0xC0, 0xEF, 0x19, 0xBC, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B,
];
const ASF_CONTENT_DESC_GUID: [u8; 16] = [
    0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_EXT_CONTENT_DESC_GUID: [u8; 16] = [
    0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50,
];

/// Open a regular file safely in one open syscall without following symlinks
/// or blocking on special files such as FIFOs.
pub fn open_regular_file(path: &Path) -> Result<std::fs::File, AudioScanError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(target_os = "linux")]
    {
        // Linux asm-generic/fcntl.h O_NONBLOCK = 00004000 avoids FIFO hang.
        const O_NONBLOCK_FLAG: i32 = 0o4000;
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        const O_NOFOLLOW_FLAG: i32 = 0o100_000;
        #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
        const O_NOFOLLOW_FLAG: i32 = 0o400_000;
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(O_NOFOLLOW_FLAG | O_NONBLOCK_FLAG);
    }

    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    {
        const O_NONBLOCK_FLAG: i32 = 0x0004;
        const O_NOFOLLOW_FLAG: i32 = 0x0100;
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(O_NOFOLLOW_FLAG | O_NONBLOCK_FLAG);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    {
        let link_meta = std::fs::symlink_metadata(path)?;
        if link_meta.file_type().is_symlink() || !link_meta.is_file() {
            return Err(AudioScanError::Scan(format!(
                "Not a regular file: {}",
                path.display()
            )));
        }
    }

    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(AudioScanError::Scan(format!(
            "Not a regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub info: serde_json::Map<String, serde_json::Value>,
    pub tags: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameSeekResult {
    pub byte_offset: i64,
    pub seek_header: Option<String>,
    pub samplerate: Option<u32>,
    pub channels: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioScanError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Lofty error: {0}")]
    Lofty(#[from] lofty::error::LoftyError),
    #[error("Symphonia error: {0}")]
    Symphonia(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Scan error: {0}")]
    Scan(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Mp3FrameHeader {
    pub version: u8,
    pub layer: u8,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub padding: bool,
    pub channels: u8,
    pub frame_length: usize,
}

pub fn parse_mp3_header(bytes: &[u8]) -> Option<Mp3FrameHeader> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes[0] != 0xFF || (bytes[1] & 0xE0) != 0xE0 {
        return None;
    }

    let version_bits = (bytes[1] >> 3) & 0x03;
    let version = match version_bits {
        0 => 3, // MPEG 2.5
        2 => 2, // MPEG 2
        3 => 1, // MPEG 1
        _ => return None,
    };

    let layer_bits = (bytes[1] >> 1) & 0x03;
    let layer = match layer_bits {
        1 => 3, // Layer III
        2 => 2, // Layer II
        3 => 1, // Layer I
        _ => return None,
    };

    let bitrate_index = (bytes[2] >> 4) & 0x0F;
    if bitrate_index == 0 || bitrate_index == 0x0F {
        return None;
    }

    let sample_rate_index = (bytes[2] >> 2) & 0x03;
    if sample_rate_index == 3 {
        return None;
    }

    let padding = (bytes[2] & 0x02) != 0;
    let channel_mode = (bytes[3] >> 6) & 0x03;
    let channels = if channel_mode == 3 { 1 } else { 2 };

    let sample_rate: u32 = match (version, sample_rate_index) {
        (1, 0) => 44100,
        (1, 1) => 48000,
        (1, 2) => 32000,
        (2, 0) => 22050,
        (2, 1) => 24000,
        (2, 2) => 16000,
        (3, 0) => 11025,
        (3, 1) => 12000,
        (3, 2) => 8000,
        _ => return None,
    };

    let bitrate_kbps: u32 = match (version, layer, bitrate_index) {
        (1, 3, idx) => {
            const RATES: [u32; 15] = [
                0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
            ];
            RATES[usize::from(idx)]
        }
        (1, 2, idx) => {
            const RATES: [u32; 15] = [
                0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
            ];
            RATES[usize::from(idx)]
        }
        (1, 1, idx) => {
            const RATES: [u32; 15] = [
                0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
            ];
            RATES[usize::from(idx)]
        }
        (_, 1, idx) => {
            const RATES: [u32; 15] = [
                0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
            ];
            RATES[usize::from(idx)]
        }
        (_, _, idx) => {
            const RATES: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
            RATES[usize::from(idx)]
        }
    };

    let padding_slot: u32 = if padding { 1 } else { 0 };
    let frame_len_u32 = match (version, layer) {
        (_, 1) => ((12 * bitrate_kbps * 1000 / sample_rate) + padding_slot) * 4,
        (1, _) | (_, 2) => (144 * bitrate_kbps * 1000 / sample_rate) + padding_slot,
        (_, _) => (72 * bitrate_kbps * 1000 / sample_rate) + padding_slot,
    };
    let frame_length = usize::try_from(frame_len_u32).ok()?;

    if frame_length < 4 {
        return None;
    }

    Some(Mp3FrameHeader {
        version,
        layer,
        bitrate_kbps,
        sample_rate,
        padding,
        channels,
        frame_length,
    })
}

fn has_consecutive_mp3_frame(bytes: &[u8], pos: usize, hdr: Mp3FrameHeader) -> bool {
    let Some(next_pos) = pos.checked_add(hdr.frame_length) else {
        return false;
    };
    if bytes.len().saturating_sub(next_pos) < 4 {
        return false;
    }
    let Some(next_hdr) = parse_mp3_header(&bytes[next_pos..next_pos + 4]) else {
        return false;
    };
    next_hdr.version == hdr.version
        && next_hdr.layer == hdr.layer
        && next_hdr.sample_rate == hdr.sample_rate
}

fn find_mp3_frame(bytes: &[u8], start: usize) -> Option<(usize, Mp3FrameHeader)> {
    let mut pos = start;
    while bytes.len().saturating_sub(pos) >= 4 {
        if bytes[pos] == 0xFF && (bytes[pos + 1] & 0xE0) == 0xE0 {
            if let Some(hdr) = parse_mp3_header(&bytes[pos..pos + 4]) {
                if has_consecutive_mp3_frame(bytes, pos, hdr) {
                    return Some((pos, hdr));
                }
            }
        }
        pos = pos.checked_add(1)?;
    }
    None
}

fn find_first_mp3_audio_offset(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while let Some((found, hdr)) = find_mp3_frame(bytes, pos) {
        let frame_len = hdr.frame_length.max(4);
        let frame_end = found.saturating_add(frame_len).min(bytes.len());
        let payload = &bytes[found..frame_end];
        if payload
            .windows(4)
            .any(|w| w == b"Xing" || w == b"Info" || w == b"VBRI")
        {
            pos = found.checked_add(frame_len)?;
            continue;
        }
        return Some(found);
    }
    None
}

fn find_flac_frame(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while bytes.len().saturating_sub(pos) >= 2 {
        if bytes[pos] == 0xFF && (bytes[pos + 1] & 0xFC) == 0xF8 {
            return Some(pos);
        }
        pos = match pos.checked_add(1) {
            Some(p) => p,
            None => break,
        };
    }
    None
}

fn find_ogg_page(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while bytes.len().saturating_sub(pos) >= 4 {
        if &bytes[pos..pos + 4] == b"OggS" {
            return Some(pos);
        }
        pos = match pos.checked_add(1) {
            Some(p) => p,
            None => break,
        };
    }
    None
}

fn max_suffix(needle: &[u8], reverse: bool) -> (usize, usize) {
    let mut ms: usize = 0;
    let mut j: usize = 1;
    let mut k: usize = 1;
    let mut p: usize = 1;
    while let Some(jk) = j.checked_add(k) {
        if jk > needle.len() {
            break;
        }
        let a = needle[jk - 1];
        let b = needle[ms + k - 1];
        let less = if reverse { a > b } else { a < b };
        if less {
            j += k;
            k = 1;
            p = j - ms;
        } else if a == b {
            if k != p {
                k += 1;
            } else {
                j += p;
                k = 1;
            }
        } else {
            ms = j;
            j = ms + 1;
            k = 1;
            p = 1;
        }
    }
    (ms, p)
}

/// Linear-time O(N + M) Two-Way byte-slice search.
fn find_slice_offset(haystack: &[u8], needle: &[u8]) -> Option<u64> {
    let m = needle.len();
    let n = haystack.len();
    if m == 0 || n < m {
        return None;
    }
    if m == 1 {
        let target = needle[0];
        return haystack
            .iter()
            .position(|&b| b == target)
            .and_then(|i| u64::try_from(i).ok());
    }
    let (ell1, p1) = max_suffix(needle, false);
    let (ell2, p2) = max_suffix(needle, true);
    let (ell, per) = if ell1 >= ell2 { (ell1, p1) } else { (ell2, p2) };

    let max_pos = n - m;
    let mut pos: usize = 0;
    if per.checked_add(ell).is_some_and(|s| s <= m) && needle[..ell] == needle[per..per + ell] {
        let mut memory: usize = 0;
        while pos <= max_pos {
            let mut i = ell.max(memory);
            while i < m && needle[i] == haystack[pos + i] {
                i += 1;
            }
            if i < m {
                pos += i - ell + 1;
                memory = 0;
            } else {
                let mut j = ell;
                while j > memory && needle[j - 1] == haystack[pos + j - 1] {
                    j -= 1;
                }
                if j <= memory {
                    return u64::try_from(pos).ok();
                }
                pos += per;
                memory = m - per;
            }
        }
    } else {
        let shift = ell.max(m - ell) + 1;
        while pos <= max_pos {
            let mut i = ell;
            while i < m && needle[i] == haystack[pos + i] {
                i += 1;
            }
            if i < m {
                pos += i - ell + 1;
            } else {
                let mut j = ell;
                while j > 0 && needle[j - 1] == haystack[pos + j - 1] {
                    j -= 1;
                }
                if j == 0 {
                    return u64::try_from(pos).ok();
                }
                pos += shift;
            }
        }
    }
    None
}

fn parse_flac_audio_offset(file_bytes: &[u8], flac_start: usize) -> u64 {
    let mut pos = match flac_start.checked_add(4) {
        Some(p) => p,
        None => return 0,
    };
    while file_bytes.len().saturating_sub(pos) >= 4 {
        let header_byte = file_bytes[pos];
        let is_last = (header_byte & 0x80) != 0;
        let block_len = (usize::from(file_bytes[pos + 1]) << 16)
            | (usize::from(file_bytes[pos + 2]) << 8)
            | usize::from(file_bytes[pos + 3]);
        let next_pos = match pos.checked_add(4).and_then(|p| p.checked_add(block_len)) {
            Some(p) => p,
            None => break,
        };
        if is_last {
            return u64::try_from(next_pos).unwrap_or(u64::MAX);
        }
        if next_pos > file_bytes.len() {
            break;
        }
        pos = next_pos;
    }
    u64::try_from(pos.min(file_bytes.len())).unwrap_or(u64::MAX)
}

fn is_non_mp3_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "flac"
            | "flc"
            | "fla"
            | "ogg"
            | "oga"
            | "ogf"
            | "opus"
            | "spx"
            | "wav"
            | "wave"
            | "aif"
            | "aiff"
            | "aifc"
            | "mp4"
            | "m4a"
            | "m4b"
            | "m4p"
            | "m4v"
            | "m4r"
            | "3gp"
            | "3g2"
            | "mov"
            | "aac"
            | "wma"
            | "wmv"
            | "asf"
            | "ape"
            | "apl"
            | "wv"
            | "mpc"
            | "mpp"
            | "dsf"
            | "dff"
            | "caf"
    )
}

fn compute_audio_offset(file_bytes: &[u8], path: &Path) -> u64 {
    if file_bytes.len() >= 10
        && &file_bytes[0..3] == b"ID3"
        && file_bytes[6..10].iter().all(|&b| (b & 0x80) == 0)
    {
        let size = (u64::from(file_bytes[6] & 0x7F) << 21)
            | (u64::from(file_bytes[7] & 0x7F) << 14)
            | (u64::from(file_bytes[8] & 0x7F) << 7)
            | u64::from(file_bytes[9] & 0x7F);
        let footer = (file_bytes[5] & 0x10) != 0;
        let total_tag = 10u64
            .saturating_add(size)
            .saturating_add(if footer { 10 } else { 0 });
        let tag_end = usize::try_from(total_tag).unwrap_or(file_bytes.len());
        let pos = tag_end.min(file_bytes.len());
        if file_bytes.get(pos..pos.saturating_add(4)) == Some(b"fLaC") {
            return parse_flac_audio_offset(file_bytes, pos);
        }
        if is_non_mp3_extension(path) {
            return u64::try_from(pos).unwrap_or(u64::MAX);
        }
        let audio_pos = find_first_mp3_audio_offset(file_bytes, pos).unwrap_or(pos);
        return u64::try_from(audio_pos).unwrap_or(u64::MAX);
    }

    if file_bytes.len() >= 4 && &file_bytes[0..4] == b"fLaC" {
        return parse_flac_audio_offset(file_bytes, 0);
    }

    if file_bytes.len() >= 12 && &file_bytes[0..4] == b"RIFF" && &file_bytes[8..12] == b"WAVE" {
        let mut pos: usize = 12;
        while file_bytes.len().saturating_sub(pos) >= 8 {
            let chunk_id = &file_bytes[pos..pos + 4];
            let chunk_size_u32 = u32::from_le_bytes([
                file_bytes[pos + 4],
                file_bytes[pos + 5],
                file_bytes[pos + 6],
                file_bytes[pos + 7],
            ]);
            let chunk_size = match usize::try_from(chunk_size_u32) {
                Ok(len) => len,
                Err(_) => break,
            };
            let data_start = match pos.checked_add(8) {
                Some(p) => p,
                None => break,
            };
            if chunk_id == b"data" {
                return u64::try_from(data_start).unwrap_or(u64::MAX);
            }
            let padded_size = match chunk_size.checked_add(chunk_size & 1) {
                Some(len) => len,
                None => break,
            };
            let next_pos = match data_start.checked_add(padded_size) {
                Some(p) if p <= file_bytes.len() => p,
                _ => break,
            };
            pos = next_pos;
        }
        return 0;
    }

    if file_bytes.len() >= 12
        && &file_bytes[0..4] == b"FORM"
        && (&file_bytes[8..12] == b"AIFF" || &file_bytes[8..12] == b"AIFC")
    {
        let mut pos: usize = 12;
        while file_bytes.len().saturating_sub(pos) >= 8 {
            let chunk_id = &file_bytes[pos..pos + 4];
            let chunk_size_u32 = u32::from_be_bytes([
                file_bytes[pos + 4],
                file_bytes[pos + 5],
                file_bytes[pos + 6],
                file_bytes[pos + 7],
            ]);
            let chunk_size = match usize::try_from(chunk_size_u32) {
                Ok(len) => len,
                Err(_) => break,
            };
            if chunk_id == b"SSND" {
                let ssnd_offset = if file_bytes.len().saturating_sub(pos) >= 12 {
                    let raw_offset = u32::from_be_bytes([
                        file_bytes[pos + 8],
                        file_bytes[pos + 9],
                        file_bytes[pos + 10],
                        file_bytes[pos + 11],
                    ]);
                    usize::try_from(raw_offset).unwrap_or(usize::MAX)
                } else {
                    0
                };
                let audio_pos = pos
                    .saturating_add(16)
                    .saturating_add(ssnd_offset)
                    .min(file_bytes.len());
                return u64::try_from(audio_pos).unwrap_or(u64::MAX);
            }
            let data_start = match pos.checked_add(8) {
                Some(p) => p,
                None => break,
            };
            let padded_size = match chunk_size.checked_add(chunk_size & 1) {
                Some(len) => len,
                None => break,
            };
            let next_pos = match data_start.checked_add(padded_size) {
                Some(p) if p <= file_bytes.len() => p,
                _ => break,
            };
            pos = next_pos;
        }
        return 0;
    }

    // Known non-MP3 containers must not fall through to MP4 or MP3 sync.
    if file_bytes.starts_with(&ASF_HEADER_GUID) {
        if file_bytes.len() >= 24 {
            let hdr_size = u64::from_le_bytes([
                file_bytes[16],
                file_bytes[17],
                file_bytes[18],
                file_bytes[19],
                file_bytes[20],
                file_bytes[21],
                file_bytes[22],
                file_bytes[23],
            ]);
            if let Ok(hdr_end) = usize::try_from(hdr_size) {
                if hdr_end >= 30 && file_bytes.get(hdr_end..hdr_end + 16) == Some(&ASF_DATA_GUID) {
                    return hdr_size
                        .saturating_add(50)
                        .min(u64::try_from(file_bytes.len()).unwrap_or(u64::MAX));
                }
                if hdr_end >= 30 && hdr_end <= file_bytes.len() {
                    return hdr_size;
                }
            }
        }
        return 0;
    }
    if file_bytes.starts_with(b"OggS")
        || file_bytes.starts_with(b"MAC ")
        || file_bytes.starts_with(b"wvpk")
        || file_bytes.starts_with(b"MP+")
        || file_bytes.starts_with(b"MPCK")
        || file_bytes.starts_with(b"caff")
        || file_bytes.starts_with(b"DSD ")
        || file_bytes.starts_with(b"FRM8")
        || file_bytes.starts_with(b"RIFF")
        || file_bytes.starts_with(b"\x1A\x45\xDF\xA3")
    {
        return 0;
    }

    // MP4 / ISO BMFF: find mdat box and do not fall through to MP3 sync.
    let mut is_mp4_container = false;
    let mut pos: usize = 0;
    while file_bytes.len().saturating_sub(pos) >= 8 {
        let box_len = u32::from_be_bytes([
            file_bytes[pos],
            file_bytes[pos + 1],
            file_bytes[pos + 2],
            file_bytes[pos + 3],
        ]);
        let box_type = &file_bytes[pos + 4..pos + 8];
        if !box_type.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
            break;
        }
        let (actual_len, header_len) = if box_len == 1 {
            if file_bytes.len().saturating_sub(pos) >= 16 {
                let size64 = u64::from_be_bytes([
                    file_bytes[pos + 8],
                    file_bytes[pos + 9],
                    file_bytes[pos + 10],
                    file_bytes[pos + 11],
                    file_bytes[pos + 12],
                    file_bytes[pos + 13],
                    file_bytes[pos + 14],
                    file_bytes[pos + 15],
                ]);
                let largesize = match usize::try_from(size64) {
                    Ok(len) => len,
                    Err(_) => break,
                };
                (largesize, 16usize)
            } else {
                break;
            }
        } else {
            let normal_len = match usize::try_from(box_len) {
                Ok(len) => len,
                Err(_) => break,
            };
            (normal_len, 8usize)
        };
        if actual_len < header_len {
            break;
        }
        if matches!(
            box_type,
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"uuid"
        ) {
            is_mp4_container = true;
        } else if pos == 0 {
            break;
        }
        if box_type == b"mdat" {
            let mdat_data = pos.saturating_add(header_len).min(file_bytes.len());
            return u64::try_from(mdat_data).unwrap_or(u64::MAX);
        }
        pos = match pos.checked_add(actual_len) {
            Some(p) if p <= file_bytes.len() => p,
            _ => break,
        };
    }
    if is_mp4_container || is_non_mp3_extension(path) {
        return 0;
    }

    // MP3 without ID3 header
    if let Some(offset) = find_first_mp3_audio_offset(file_bytes, 0) {
        return u64::try_from(offset).unwrap_or(u64::MAX);
    }

    0
}

fn pic_type_to_u8(pt: lofty::picture::PictureType) -> u8 {
    match pt {
        lofty::picture::PictureType::Other => 0,
        lofty::picture::PictureType::Icon => 1,
        lofty::picture::PictureType::OtherIcon => 2,
        lofty::picture::PictureType::CoverFront => 3,
        lofty::picture::PictureType::CoverBack => 4,
        lofty::picture::PictureType::Leaflet => 5,
        lofty::picture::PictureType::Media => 6,
        lofty::picture::PictureType::LeadArtist => 7,
        lofty::picture::PictureType::Artist => 8,
        lofty::picture::PictureType::Conductor => 9,
        lofty::picture::PictureType::Band => 10,
        lofty::picture::PictureType::Composer => 11,
        lofty::picture::PictureType::Lyricist => 12,
        lofty::picture::PictureType::RecordingLocation => 13,
        lofty::picture::PictureType::DuringRecording => 14,
        lofty::picture::PictureType::DuringPerformance => 15,
        lofty::picture::PictureType::ScreenCapture => 16,
        lofty::picture::PictureType::BrightFish => 17,
        lofty::picture::PictureType::Illustration => 18,
        lofty::picture::PictureType::BandLogo => 19,
        lofty::picture::PictureType::PublisherLogo => 20,
        _ => 0,
    }
}

const INITIAL_HEADER_READ: usize = 256 * 1024;
const MAX_HEADER_READ: usize = 16 * 1024 * 1024;
const SEEK_WINDOW_BYTES: usize = 256 * 1024;
const SEEK_HALF_WINDOW: u64 = 128 * 1024;
const MAX_ARTWORK_INLINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_ARTWORK_INLINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOFTY_JUNK_BYTES: usize = 1024 * 1024;

fn clamp_u64_to_i64(val: u64) -> i64 {
    i64::try_from(val).unwrap_or(i64::MAX)
}

fn read_window_at(
    file: &mut std::fs::File,
    file_size: u64,
    start: u64,
    max_len: usize,
) -> Result<Vec<u8>, AudioScanError> {
    if start >= file_size || max_len == 0 {
        return Ok(Vec::new());
    }
    let available = file_size.saturating_sub(start);
    let max_len_u64 = u64::try_from(max_len).unwrap_or(u64::MAX);
    let to_read_u64 = available.min(max_len_u64);
    let to_read = usize::try_from(to_read_u64).unwrap_or(max_len);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(to_read);
    file.take(to_read_u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn extend_header_bytes(
    file: &mut std::fs::File,
    file_size: u64,
    header_bytes: &mut Vec<u8>,
    target_cap: usize,
) -> Result<(), AudioScanError> {
    let capped_target = u64::try_from(target_cap).unwrap_or(u64::MAX).min(file_size);
    let current = u64::try_from(header_bytes.len()).unwrap_or(u64::MAX);
    if current >= capped_target {
        return Ok(());
    }
    let extra = capped_target.saturating_sub(current);
    file.seek(SeekFrom::Start(current))?;
    file.take(extra).read_to_end(header_bytes)?;
    Ok(())
}

fn id3v2_tag_end(header_bytes: &[u8]) -> Option<u64> {
    if header_bytes.len() < 10 || &header_bytes[0..3] != b"ID3" {
        return None;
    }
    for &b in &header_bytes[6..10] {
        if (b & 0x80) != 0 {
            return None;
        }
    }
    let size = (u64::from(header_bytes[6] & 0x7F) << 21)
        | (u64::from(header_bytes[7] & 0x7F) << 14)
        | (u64::from(header_bytes[8] & 0x7F) << 7)
        | u64::from(header_bytes[9] & 0x7F);
    let footer: u64 = if (header_bytes[5] & 0x10) != 0 { 10 } else { 0 };
    Some(10u64.saturating_add(size).saturating_add(footer))
}

const MAX_CONTAINER_SCAN_STEPS: usize = 65_536;

fn flac_metadata_incomplete(header_bytes: &[u8]) -> bool {
    if header_bytes.len() < 4 || &header_bytes[0..4] != b"fLaC" {
        return false;
    }
    let mut pos: usize = 4;
    while header_bytes.len().saturating_sub(pos) >= 4 {
        let is_last = (header_bytes[pos] & 0x80) != 0;
        let block_len = (usize::from(header_bytes[pos + 1]) << 16)
            | (usize::from(header_bytes[pos + 2]) << 8)
            | usize::from(header_bytes[pos + 3]);
        let next_pos = match pos.checked_add(4).and_then(|p| p.checked_add(block_len)) {
            Some(p) => p,
            None => return false,
        };
        if next_pos > header_bytes.len() {
            return true;
        }
        if is_last {
            return false;
        }
        pos = next_pos;
    }
    true
}

fn is_riff_wave(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

fn is_aiff_header(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[0..4] == b"FORM"
        && (&bytes[8..12] == b"AIFF" || &bytes[8..12] == b"AIFC")
}

fn is_mp4_header(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let box_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let box_type = &bytes[4..8];
    let valid_len = box_len == 1 || box_len >= 8;
    valid_len
        && matches!(
            box_type,
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"uuid"
        )
}

fn find_flac_audio_offset_seek(
    file: &mut std::fs::File,
    file_size: u64,
    flac_start: u64,
) -> Result<u64, AudioScanError> {
    let mut pos = flac_start.saturating_add(4);
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        if file_size.saturating_sub(pos) < 4 {
            break;
        }
        let hdr = read_window_at(file, file_size, pos, 4)?;
        if hdr.len() < 4 {
            break;
        }
        let is_last = (hdr[0] & 0x80) != 0;
        let block_len = (u64::from(hdr[1]) << 16) | (u64::from(hdr[2]) << 8) | u64::from(hdr[3]);
        let next_pos = pos.saturating_add(4).saturating_add(block_len);
        if is_last {
            return Ok(next_pos.min(file_size));
        }
        if next_pos > file_size {
            break;
        }
        pos = next_pos;
    }
    Ok(pos.min(file_size))
}

fn find_riff_data_offset_seek(
    file: &mut std::fs::File,
    file_size: u64,
    mut pos: u64,
) -> Result<Option<u64>, AudioScanError> {
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        if file_size.saturating_sub(pos) < 8 {
            break;
        }
        let hdr = read_window_at(file, file_size, pos, 8)?;
        if hdr.len() < 8 {
            break;
        }
        let chunk_size = u64::from(u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]));
        let data_start = pos.saturating_add(8);
        if &hdr[0..4] == b"data" {
            return Ok(Some(data_start.min(file_size)));
        }
        let padded = chunk_size.saturating_add(chunk_size & 1);
        let Some(next_pos) = data_start.checked_add(padded) else {
            break;
        };
        if next_pos > file_size {
            break;
        }
        pos = next_pos;
    }
    Ok(None)
}

fn extend_riff_wave_and_find_offset(
    file: &mut std::fs::File,
    file_size: u64,
    header_bytes: &mut Vec<u8>,
) -> Result<Option<u64>, AudioScanError> {
    let max_cap_u64 = u64::try_from(MAX_HEADER_READ).unwrap_or(u64::MAX);
    let mut pos: usize = 12;
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        let pos_u64 = u64::try_from(pos).unwrap_or(u64::MAX);
        if file_size.saturating_sub(pos_u64) < 8 {
            break;
        }
        if header_bytes.len().saturating_sub(pos) < 8 {
            if pos_u64.saturating_add(8) <= max_cap_u64 {
                let target = pos.saturating_add(8192).min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            } else {
                return find_riff_data_offset_seek(file, file_size, pos_u64);
            }
        }
        if header_bytes.len().saturating_sub(pos) < 8 {
            break;
        }
        let chunk_id = &header_bytes[pos..pos + 4];
        let chunk_size = u64::from(u32::from_le_bytes([
            header_bytes[pos + 4],
            header_bytes[pos + 5],
            header_bytes[pos + 6],
            header_bytes[pos + 7],
        ]));
        let data_start = pos_u64.saturating_add(8);
        if chunk_id == b"data" {
            return Ok(Some(data_start.min(file_size)));
        }
        let padded = chunk_size.saturating_add(chunk_size & 1);
        let Some(next_pos_u64) = data_start.checked_add(padded) else {
            break;
        };
        if next_pos_u64 > file_size {
            break;
        }
        if next_pos_u64 > u64::try_from(header_bytes.len()).unwrap_or(u64::MAX) {
            if next_pos_u64.saturating_add(8) <= max_cap_u64 {
                let target = usize::try_from(next_pos_u64)
                    .unwrap_or(MAX_HEADER_READ)
                    .saturating_add(8192)
                    .min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            } else {
                return find_riff_data_offset_seek(file, file_size, next_pos_u64);
            }
        }
        pos = match usize::try_from(next_pos_u64) {
            Ok(p) => p,
            Err(_) => return find_riff_data_offset_seek(file, file_size, next_pos_u64),
        };
    }
    Ok(None)
}

fn find_aiff_ssnd_offset_seek(
    file: &mut std::fs::File,
    file_size: u64,
    mut pos: u64,
) -> Result<Option<u64>, AudioScanError> {
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        if file_size.saturating_sub(pos) < 8 {
            break;
        }
        let hdr = read_window_at(file, file_size, pos, 16)?;
        if hdr.len() < 8 {
            break;
        }
        let chunk_size = u64::from(u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]));
        if &hdr[0..4] == b"SSND" {
            let ssnd_offset = if hdr.len() >= 12 && chunk_size >= 4 {
                u64::from(u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]))
            } else {
                0
            };
            let audio_pos = pos.saturating_add(16).saturating_add(ssnd_offset);
            return Ok(Some(audio_pos.min(file_size)));
        }
        let padded = chunk_size.saturating_add(chunk_size & 1);
        let Some(next_pos) = pos.saturating_add(8).checked_add(padded) else {
            break;
        };
        if next_pos > file_size {
            break;
        }
        pos = next_pos;
    }
    Ok(None)
}

fn extend_aiff_and_find_offset(
    file: &mut std::fs::File,
    file_size: u64,
    header_bytes: &mut Vec<u8>,
) -> Result<Option<u64>, AudioScanError> {
    let max_cap_u64 = u64::try_from(MAX_HEADER_READ).unwrap_or(u64::MAX);
    let mut pos: usize = 12;
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        let pos_u64 = u64::try_from(pos).unwrap_or(u64::MAX);
        if file_size.saturating_sub(pos_u64) < 8 {
            break;
        }
        if header_bytes.len().saturating_sub(pos) < 8 {
            if pos_u64.saturating_add(16) <= max_cap_u64 {
                let target = pos.saturating_add(8192).min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            } else {
                return find_aiff_ssnd_offset_seek(file, file_size, pos_u64);
            }
        }
        if header_bytes.len().saturating_sub(pos) < 8 {
            break;
        }
        let chunk_id = [
            header_bytes[pos],
            header_bytes[pos + 1],
            header_bytes[pos + 2],
            header_bytes[pos + 3],
        ];
        let chunk_size = u64::from(u32::from_be_bytes([
            header_bytes[pos + 4],
            header_bytes[pos + 5],
            header_bytes[pos + 6],
            header_bytes[pos + 7],
        ]));
        if &chunk_id == b"SSND" {
            if header_bytes.len().saturating_sub(pos) < 16
                && pos_u64.saturating_add(16) <= file_size
                && pos_u64.saturating_add(16) <= max_cap_u64
            {
                let target = pos.saturating_add(16).min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            }
            let ssnd_offset = if header_bytes.len().saturating_sub(pos) >= 12 && chunk_size >= 4 {
                u64::from(u32::from_be_bytes([
                    header_bytes[pos + 8],
                    header_bytes[pos + 9],
                    header_bytes[pos + 10],
                    header_bytes[pos + 11],
                ]))
            } else if header_bytes.len().saturating_sub(pos) < 12 && chunk_size >= 4 {
                let hdr = read_window_at(file, file_size, pos_u64, 16)?;
                if hdr.len() >= 12 {
                    u64::from(u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]))
                } else {
                    0
                }
            } else {
                0
            };
            let audio_pos = pos_u64.saturating_add(16).saturating_add(ssnd_offset);
            return Ok(Some(audio_pos.min(file_size)));
        }
        let padded = chunk_size.saturating_add(chunk_size & 1);
        let Some(next_pos_u64) = pos_u64.saturating_add(8).checked_add(padded) else {
            break;
        };
        if next_pos_u64 > file_size {
            break;
        }
        if next_pos_u64 > u64::try_from(header_bytes.len()).unwrap_or(u64::MAX) {
            if next_pos_u64.saturating_add(16) <= max_cap_u64 {
                let target = usize::try_from(next_pos_u64)
                    .unwrap_or(MAX_HEADER_READ)
                    .saturating_add(8192)
                    .min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            } else {
                return find_aiff_ssnd_offset_seek(file, file_size, next_pos_u64);
            }
        }
        pos = match usize::try_from(next_pos_u64) {
            Ok(p) => p,
            Err(_) => return find_aiff_ssnd_offset_seek(file, file_size, next_pos_u64),
        };
    }
    Ok(None)
}

fn parse_mp4_box_header(buf: &[u8]) -> Option<(u64, u64, [u8; 4])> {
    if buf.len() < 8 {
        return None;
    }
    let box_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let box_type = [buf[4], buf[5], buf[6], buf[7]];
    if !box_type.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
        return None;
    }
    let (actual_len, header_len) = if box_len == 1 {
        if buf.len() < 16 {
            return None;
        }
        let size64 = u64::from_be_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        (size64, 16u64)
    } else {
        (u64::from(box_len), 8u64)
    };
    if actual_len < header_len {
        return None;
    }
    Some((actual_len, header_len, box_type))
}

fn find_mp4_mdat_offset_seek(
    file: &mut std::fs::File,
    file_size: u64,
    mut pos: u64,
) -> Result<Option<u64>, AudioScanError> {
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        if file_size.saturating_sub(pos) < 8 {
            break;
        }
        let hdr = read_window_at(file, file_size, pos, 16)?;
        let Some((actual_len, header_len, box_type)) = parse_mp4_box_header(&hdr) else {
            break;
        };
        if &box_type == b"mdat" {
            return Ok(Some(pos.saturating_add(header_len).min(file_size)));
        }
        let Some(next_pos) = pos.checked_add(actual_len) else {
            break;
        };
        if next_pos > file_size {
            break;
        }
        pos = next_pos;
    }
    Ok(None)
}

fn extend_mp4_and_find_offset(
    file: &mut std::fs::File,
    file_size: u64,
    header_bytes: &mut Vec<u8>,
) -> Result<Option<u64>, AudioScanError> {
    let max_cap_u64 = u64::try_from(MAX_HEADER_READ).unwrap_or(u64::MAX);
    let mut pos: usize = 0;
    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        let pos_u64 = u64::try_from(pos).unwrap_or(u64::MAX);
        if file_size.saturating_sub(pos_u64) < 8 {
            break;
        }
        if header_bytes.len().saturating_sub(pos) < 16 {
            if pos_u64.saturating_add(16) <= max_cap_u64 {
                let target = pos.saturating_add(8192).min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            } else {
                return find_mp4_mdat_offset_seek(file, file_size, pos_u64);
            }
        }
        let Some((actual_len, header_len, box_type)) = parse_mp4_box_header(&header_bytes[pos..])
        else {
            break;
        };
        if pos == 0
            && !matches!(
                &box_type,
                b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"uuid"
            )
        {
            break;
        }
        if &box_type == b"mdat" {
            return Ok(Some(pos_u64.saturating_add(header_len).min(file_size)));
        }
        let Some(next_pos_u64) = pos_u64.checked_add(actual_len) else {
            break;
        };
        if next_pos_u64 > file_size {
            break;
        }
        if next_pos_u64 > u64::try_from(header_bytes.len()).unwrap_or(u64::MAX) {
            if next_pos_u64.saturating_add(16) <= max_cap_u64 {
                let target = usize::try_from(next_pos_u64)
                    .unwrap_or(MAX_HEADER_READ)
                    .saturating_add(8192)
                    .min(MAX_HEADER_READ);
                extend_header_bytes(file, file_size, header_bytes, target)?;
            } else {
                return find_mp4_mdat_offset_seek(file, file_size, next_pos_u64);
            }
        }
        pos = match usize::try_from(next_pos_u64) {
            Ok(p) => p,
            Err(_) => return find_mp4_mdat_offset_seek(file, file_size, next_pos_u64),
        };
    }
    Ok(None)
}

fn read_header_prefix(
    file: &mut std::fs::File,
    file_size: u64,
    path: &Path,
) -> Result<(Vec<u8>, u64), AudioScanError> {
    let mut header_bytes = read_window_at(file, file_size, 0, INITIAL_HEADER_READ)?;

    if let Some(tag_end) = id3v2_tag_end(&header_bytes) {
        let needed = tag_end.saturating_add(8192);
        let max_cap_u64 = u64::try_from(MAX_HEADER_READ).unwrap_or(u64::MAX);
        if needed <= max_cap_u64 {
            let target = usize::try_from(needed).unwrap_or(MAX_HEADER_READ);
            extend_header_bytes(file, file_size, &mut header_bytes, target)?;
            if let Ok(tag_end_usize) = usize::try_from(tag_end) {
                if let Some(rest) = header_bytes.get(tag_end_usize..) {
                    if flac_metadata_incomplete(rest) {
                        extend_header_bytes(file, file_size, &mut header_bytes, MAX_HEADER_READ)?;
                    }
                }
            }
        } else if tag_end < file_size {
            // ID3 tag exceeds MAX_HEADER_READ; jump past tag to locate audio.
            let tail = read_window_at(file, file_size, tag_end, 16 * 1024)?;
            let offset = if tail.starts_with(b"fLaC") {
                if !flac_metadata_incomplete(&tail) {
                    tag_end.saturating_add(parse_flac_audio_offset(&tail, 0))
                } else {
                    find_flac_audio_offset_seek(file, file_size, tag_end)?
                }
            } else if is_non_mp3_extension(path) {
                tag_end
            } else {
                find_first_mp3_audio_offset(&tail, 0)
                    .and_then(|rel| u64::try_from(rel).ok())
                    .map_or(tag_end, |rel| tag_end.saturating_add(rel))
            };
            return Ok((header_bytes, offset.min(file_size)));
        }
    } else if flac_metadata_incomplete(&header_bytes) {
        extend_header_bytes(file, file_size, &mut header_bytes, MAX_HEADER_READ)?;
    } else if is_riff_wave(&header_bytes) {
        if let Some(offset) = extend_riff_wave_and_find_offset(file, file_size, &mut header_bytes)?
        {
            return Ok((header_bytes, offset.min(file_size)));
        }
    } else if is_aiff_header(&header_bytes) {
        if let Some(offset) = extend_aiff_and_find_offset(file, file_size, &mut header_bytes)? {
            return Ok((header_bytes, offset.min(file_size)));
        }
    } else if is_mp4_header(&header_bytes) {
        if let Some(offset) = extend_mp4_and_find_offset(file, file_size, &mut header_bytes)? {
            return Ok((header_bytes, offset.min(file_size)));
        }
    } else if header_bytes.starts_with(&ASF_HEADER_GUID) && header_bytes.len() >= 24 {
        let hdr_size = u64::from_le_bytes([
            header_bytes[16],
            header_bytes[17],
            header_bytes[18],
            header_bytes[19],
            header_bytes[20],
            header_bytes[21],
            header_bytes[22],
            header_bytes[23],
        ]);
        let needed = hdr_size.saturating_add(50);
        let max_cap_u64 = u64::try_from(MAX_HEADER_READ).unwrap_or(u64::MAX);
        if needed <= max_cap_u64 {
            let target = usize::try_from(needed).unwrap_or(MAX_HEADER_READ);
            extend_header_bytes(file, file_size, &mut header_bytes, target)?;
        }
    }

    let audio_offset = compute_audio_offset(&header_bytes, path).min(file_size);
    Ok((header_bytes, audio_offset))
}

fn read_tagged_file(
    file: &mut std::fs::File,
    path: &Path,
    format_hint: Option<&str>,
    read_tags: bool,
    read_cover_art: bool,
) -> Result<TaggedFile, AudioScanError> {
    file.seek(SeekFrom::Start(0))?;
    let reader = BufReader::new(&mut *file);
    let options = ParseOptions::new()
        .read_tags(read_tags)
        .read_cover_art(read_cover_art)
        .max_junk_bytes(MAX_LOFTY_JUNK_BYTES);
    let mut probe = Probe::new(reader).options(options);
    if let Some(hint) = format_hint {
        if let Some(ft) = FileType::from_ext(hint) {
            probe = probe.set_file_type(ft);
        }
    }
    let probe = if probe.file_type().is_some() {
        probe
    } else {
        probe.guess_file_type()?
    };
    if probe.file_type().is_some() {
        return Ok(probe.read()?);
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(ft) = FileType::from_ext(ext) {
            return Ok(probe.set_file_type(ft).read()?);
        }
    }
    Err(AudioScanError::UnsupportedFormat(
        path.display().to_string(),
    ))
}

fn locate_artwork_offset(header_bytes: &[u8], audio_offset: u64, data: &[u8]) -> Option<u64> {
    if data.is_empty() {
        return None;
    }
    let boundary = usize::try_from(audio_offset)
        .unwrap_or(header_bytes.len())
        .min(header_bytes.len());
    if let Some(off) = find_slice_offset(&header_bytes[..boundary], data) {
        return Some(off);
    }
    if boundary < header_bytes.len() {
        let overlap_start = boundary.saturating_sub(data.len().saturating_sub(1));
        if let Some(rel) = find_slice_offset(&header_bytes[overlap_start..], data) {
            return u64::try_from(overlap_start)
                .ok()
                .and_then(|s| s.checked_add(rel));
        }
    }
    None
}

fn insert_tag_value(
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    multi_value_keys: &mut HashSet<String>,
    key: String,
    val_json: serde_json::Value,
) {
    if let Some(existing) = tags_map.get_mut(&key) {
        if multi_value_keys.contains(&key) {
            if let Some(arr) = existing.as_array_mut() {
                arr.push(val_json);
                return;
            }
        }
        let old = existing.take();
        *existing = serde_json::json!([old, val_json]);
        multi_value_keys.insert(key);
    } else {
        tags_map.insert(key, val_json);
    }
}

fn append_apic_entries(
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    multi_value_keys: &mut HashSet<String>,
    mut entries: Vec<serde_json::Value>,
) {
    if entries.is_empty() {
        return;
    }
    if let Some(existing) = tags_map.get_mut("APIC") {
        if let Some(arr) = existing.as_array_mut() {
            let is_multi =
                multi_value_keys.contains("APIC") || arr.iter().any(serde_json::Value::is_array);
            if is_multi {
                arr.append(&mut entries);
                multi_value_keys.insert("APIC".to_string());
                return;
            }
        }
        let first = existing.take();
        let mut combined = Vec::with_capacity(entries.len() + 1);
        combined.push(first);
        combined.append(&mut entries);
        *existing = serde_json::Value::Array(combined);
        multi_value_keys.insert("APIC".to_string());
    } else if entries.len() == 1 {
        if let Some(single) = entries.pop() {
            tags_map.insert("APIC".to_string(), single);
        }
    } else {
        tags_map.insert("APIC".to_string(), serde_json::Value::Array(entries));
        multi_value_keys.insert("APIC".to_string());
    }
}

fn append_allpictures(
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    mut objects: Vec<serde_json::Value>,
) {
    if objects.is_empty() {
        return;
    }
    if let Some(existing) = tags_map.get_mut("ALLPICTURES") {
        if let Some(arr) = existing.as_array_mut() {
            arr.append(&mut objects);
            return;
        }
    }
    tags_map.insert("ALLPICTURES".to_string(), serde_json::Value::Array(objects));
}

fn clean_tag_string(s: &str) -> String {
    let trimmed = s.trim_end_matches(|c: char| c == '\0' || c.is_control());
    if trimmed.contains('\0') {
        trimmed.replace('\0', "")
    } else {
        trimmed.to_string()
    }
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

fn normalize_item_key_name(raw: &str, target_tag_type: TagType) -> Option<String> {
    let cleaned = clean_tag_string(raw);
    let mut trimmed = cleaned.trim();
    loop {
        if let Some(rest) = strip_prefix_ignore_ascii_case(trimmed, "TXXX:") {
            trimmed = rest.trim_start();
        } else if let Some(rest) = strip_prefix_ignore_ascii_case(trimmed, "----:com.apple.iTunes:")
        {
            trimmed = rest.trim_start();
        } else {
            break;
        }
    }
    if target_tag_type == TagType::Mp4Ilst {
        trimmed = trimmed.strip_prefix('\u{a9}').unwrap_or(trimmed);
    }
    let upper = trimmed.to_uppercase();
    (!upper.is_empty()).then_some(upper)
}

fn fallback_item_key_name(key: &lofty::tag::ItemKey) -> Option<&'static str> {
    use lofty::tag::ItemKey;

    match key {
        ItemKey::AlbumTitle => Some("ALBUM"),
        ItemKey::SetSubtitle => Some("DISCSUBTITLE"),
        ItemKey::ShowName => Some("SHOWNAME"),
        ItemKey::ContentGroup => Some("GROUPING"),
        ItemKey::TrackTitle => Some("TITLE"),
        ItemKey::TrackSubtitle => Some("SUBTITLE"),
        ItemKey::OriginalAlbumTitle => Some("ORIGINALALBUM"),
        ItemKey::OriginalArtist => Some("ORIGINALARTIST"),
        ItemKey::OriginalLyricist => Some("ORIGINALLYRICIST"),
        ItemKey::AlbumTitleSortOrder => Some("ALBUMSORT"),
        ItemKey::AlbumArtistSortOrder => Some("ALBUMARTISTSORT"),
        ItemKey::TrackTitleSortOrder => Some("TITLESORT"),
        ItemKey::TrackArtistSortOrder => Some("ARTISTSORT"),
        ItemKey::ShowNameSortOrder => Some("SHOWNAMESORT"),
        ItemKey::ComposerSortOrder => Some("COMPOSERSORT"),
        ItemKey::AlbumArtist => Some("ALBUMARTIST"),
        ItemKey::TrackArtist => Some("ARTIST"),
        ItemKey::Arranger => Some("ARRANGER"),
        ItemKey::Writer => Some("WRITER"),
        ItemKey::Composer => Some("COMPOSER"),
        ItemKey::Conductor => Some("CONDUCTOR"),
        ItemKey::Director => Some("DIRECTOR"),
        ItemKey::Engineer => Some("ENGINEER"),
        ItemKey::Lyricist => Some("LYRICIST"),
        ItemKey::MixDj => Some("DJMIXER"),
        ItemKey::MixEngineer => Some("MIXER"),
        ItemKey::MusicianCredits => Some("MUSICIANCREDITS"),
        ItemKey::Performer => Some("PERFORMER"),
        ItemKey::Producer => Some("PRODUCER"),
        ItemKey::Publisher => Some("PUBLISHER"),
        ItemKey::Label => Some("LABEL"),
        ItemKey::InternetRadioStationName => Some("INTERNETRADIOSTATIONNAME"),
        ItemKey::InternetRadioStationOwner => Some("INTERNETRADIOSTATIONOWNER"),
        ItemKey::Remixer => Some("REMIXER"),
        ItemKey::DiscNumber => Some("DISCNUMBER"),
        ItemKey::DiscTotal => Some("DISCTOTAL"),
        ItemKey::TrackNumber => Some("TRACKNUMBER"),
        ItemKey::TrackTotal => Some("TRACKTOTAL"),
        ItemKey::Popularimeter => Some("RATING"),
        ItemKey::ParentalAdvisory => Some("ITUNESADVISORY"),
        ItemKey::RecordingDate => Some("DATE"),
        ItemKey::Year => Some("YEAR"),
        ItemKey::ReleaseDate => Some("RELEASEDATE"),
        ItemKey::OriginalReleaseDate => Some("ORIGINALDATE"),
        ItemKey::Isrc => Some("ISRC"),
        ItemKey::Barcode => Some("BARCODE"),
        ItemKey::CatalogNumber => Some("CATALOGNUMBER"),
        ItemKey::Work => Some("WORK"),
        ItemKey::Movement => Some("MOVEMENTNAME"),
        ItemKey::MovementNumber => Some("MOVEMENT"),
        ItemKey::MovementTotal => Some("MOVEMENTTOTAL"),
        ItemKey::MusicBrainzRecordingId => Some("MUSICBRAINZ_TRACKID"),
        ItemKey::MusicBrainzTrackId => Some("MUSICBRAINZ_RELEASETRACKID"),
        ItemKey::MusicBrainzReleaseId => Some("MUSICBRAINZ_ALBUMID"),
        ItemKey::MusicBrainzReleaseGroupId => Some("MUSICBRAINZ_RELEASEGROUPID"),
        ItemKey::MusicBrainzArtistId => Some("MUSICBRAINZ_ARTISTID"),
        ItemKey::MusicBrainzReleaseArtistId => Some("MUSICBRAINZ_ALBUMARTISTID"),
        ItemKey::MusicBrainzWorkId => Some("MUSICBRAINZ_WORKID"),
        ItemKey::FlagCompilation => Some("COMPILATION"),
        ItemKey::FlagPodcast => Some("PODCAST"),
        ItemKey::FileType => Some("FILETYPE"),
        ItemKey::FileOwner => Some("FILEOWNER"),
        ItemKey::TaggingTime => Some("TAGGINGTIME"),
        ItemKey::Length => Some("LENGTH"),
        ItemKey::OriginalFileName => Some("ORIGINALFILENAME"),
        ItemKey::OriginalMediaType => Some("MEDIA"),
        ItemKey::EncodedBy => Some("ENCODEDBY"),
        ItemKey::EncoderSoftware => Some("ENCODER"),
        ItemKey::EncoderSettings => Some("ENCODING"),
        ItemKey::EncodingTime => Some("ENCODINGTIME"),
        ItemKey::ReplayGainAlbumGain => Some("REPLAYGAIN_ALBUM_GAIN"),
        ItemKey::ReplayGainAlbumPeak => Some("REPLAYGAIN_ALBUM_PEAK"),
        ItemKey::ReplayGainTrackGain => Some("REPLAYGAIN_TRACK_GAIN"),
        ItemKey::ReplayGainTrackPeak => Some("REPLAYGAIN_TRACK_PEAK"),
        ItemKey::AudioFileUrl => Some("AUDIOFILEURL"),
        ItemKey::AudioSourceUrl => Some("AUDIOSOURCEURL"),
        ItemKey::CommercialInformationUrl => Some("COMMERCIALINFORMATIONURL"),
        ItemKey::CopyrightUrl => Some("COPYRIGHTURL"),
        ItemKey::TrackArtistUrl => Some("TRACKARTISTURL"),
        ItemKey::RadioStationUrl => Some("RADIOSTATIONURL"),
        ItemKey::PaymentUrl => Some("PAYMENTURL"),
        ItemKey::PublisherUrl => Some("PUBLISHERURL"),
        ItemKey::Genre => Some("GENRE"),
        ItemKey::InitialKey => Some("INITIALKEY"),
        ItemKey::Color => Some("COLOR"),
        ItemKey::Mood => Some("MOOD"),
        ItemKey::Bpm | ItemKey::IntegerBpm => Some("BPM"),
        ItemKey::CopyrightMessage => Some("COPYRIGHT"),
        ItemKey::License => Some("LICENSE"),
        ItemKey::PodcastDescription => Some("PODCASTDESCRIPTION"),
        ItemKey::PodcastSeriesCategory => Some("PODCASTCATEGORY"),
        ItemKey::PodcastUrl => Some("PODCASTURL"),
        ItemKey::PodcastGlobalUniqueId => Some("PODCASTID"),
        ItemKey::PodcastKeywords => Some("PODCASTKEYWORDS"),
        ItemKey::Comment => Some("COMMENT"),
        ItemKey::Description => Some("DESCRIPTION"),
        ItemKey::Language => Some("LANGUAGE"),
        ItemKey::Script => Some("SCRIPT"),
        ItemKey::Lyrics => Some("LYRICS"),
        ItemKey::AppleXid => Some("XID"),
        ItemKey::AppleId3v2ContentGroup => Some("GRP1"),
        ItemKey::Unknown(_) => None,
        _ => None,
    }
}

fn map_item_key(key: &lofty::tag::ItemKey, target_tag_type: TagType) -> Option<String> {
    if let lofty::tag::ItemKey::Unknown(raw) = key {
        return normalize_item_key_name(raw, target_tag_type);
    }
    if matches!(key, lofty::tag::ItemKey::MusicBrainzRecordingId)
        && target_tag_type == TagType::Id3v2
    {
        return Some("MUSICBRAINZ_ID".to_string());
    }
    if let Some(mapped) = key
        .map_key(target_tag_type, false)
        .or_else(|| key.map_key(target_tag_type, true))
    {
        if let Some(norm) = normalize_item_key_name(mapped, target_tag_type) {
            return Some(norm);
        }
    }
    if let Some(mapped) = key
        .map_key(TagType::VorbisComments, false)
        .or_else(|| key.map_key(TagType::VorbisComments, true))
    {
        if let Some(norm) = normalize_item_key_name(mapped, target_tag_type) {
            return Some(norm);
        }
    }
    fallback_item_key_name(key).map(str::to_string)
}

fn base64_encoded_len(raw_len: usize) -> Option<usize> {
    raw_len.div_ceil(3).checked_mul(4)
}

fn can_inline_artwork(data_len: usize, no_artwork: bool, total_inline_b64: &mut usize) -> bool {
    if no_artwork {
        return false;
    }
    let Some(b64_len) = base64_encoded_len(data_len) else {
        return false;
    };
    if b64_len > MAX_ARTWORK_INLINE_BYTES {
        return false;
    }
    match total_inline_b64.checked_add(b64_len) {
        Some(next) if next <= MAX_TOTAL_ARTWORK_INLINE_BYTES => {
            *total_inline_b64 = next;
            true
        }
        _ => false,
    }
}

fn find_id3_nul_end(bytes: &[u8], start: usize, two_byte: bool) -> Option<(usize, usize)> {
    if !two_byte {
        let rel = bytes.get(start..)?.iter().position(|&b| b == 0)?;
        let end = start + rel;
        return Some((end, end + 1));
    }
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 {
            return Some((i, i + 2));
        }
        i += 2;
    }
    None
}

fn decode_id3_text(encoding: u8, raw: &[u8]) -> String {
    let text = match encoding {
        0 => raw.iter().map(|&b| char::from(b)).collect::<String>(),
        1 => {
            let (little_endian, body) = if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
                (true, &raw[2..])
            } else if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
                (false, &raw[2..])
            } else {
                (true, raw)
            };
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| {
                    if little_endian {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
        2 => {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(raw).into_owned(),
    };
    clean_tag_string(&text)
}

fn visit_id3v2_frames(header_bytes: &[u8], mut on_frame: impl FnMut(u8, &[u8], &[u8], usize)) {
    let Some(tag_end_u64) = id3v2_tag_end(header_bytes) else {
        return;
    };
    let major = header_bytes[3];
    let tag_flags = header_bytes[5];
    if (tag_flags & 0x80) != 0 {
        return;
    }
    let footer_len: u64 = if (tag_flags & 0x10) != 0 { 10 } else { 0 };
    let tag_end = usize::try_from(tag_end_u64.saturating_sub(footer_len))
        .unwrap_or(header_bytes.len())
        .min(header_bytes.len());
    let mut pos: usize = 10;

    if (tag_flags & 0x40) != 0 {
        if major == 3 && tag_end.saturating_sub(pos) >= 4 {
            let ext_len = u32::from_be_bytes([
                header_bytes[pos],
                header_bytes[pos + 1],
                header_bytes[pos + 2],
                header_bytes[pos + 3],
            ]);
            let Some(skip) = usize::try_from(ext_len).ok().and_then(|n| n.checked_add(4)) else {
                return;
            };
            pos = pos.saturating_add(skip);
        } else if major == 4
            && tag_end.saturating_sub(pos) >= 4
            && header_bytes[pos..pos + 4].iter().all(|&b| (b & 0x80) == 0)
        {
            let ext_len = (usize::from(header_bytes[pos] & 0x7F) << 21)
                | (usize::from(header_bytes[pos + 1] & 0x7F) << 14)
                | (usize::from(header_bytes[pos + 2] & 0x7F) << 7)
                | usize::from(header_bytes[pos + 3] & 0x7F);
            if ext_len < 4 {
                return;
            }
            pos = pos.saturating_add(ext_len);
        } else {
            return;
        }
    }

    while pos < tag_end {
        if header_bytes[pos] == 0 {
            break;
        }
        let (frame_id, frame_size, hdr_len, skip_payload_prefix, unsupported_flags) = if major == 2
        {
            if tag_end.saturating_sub(pos) < 6 {
                break;
            }
            let sz = (usize::from(header_bytes[pos + 3]) << 16)
                | (usize::from(header_bytes[pos + 4]) << 8)
                | usize::from(header_bytes[pos + 5]);
            (&header_bytes[pos..pos + 3], sz, 6usize, 0usize, false)
        } else {
            if tag_end.saturating_sub(pos) < 10 {
                break;
            }
            let sz = if major == 4
                && header_bytes[pos + 4..pos + 8]
                    .iter()
                    .all(|&b| (b & 0x80) == 0)
            {
                (usize::from(header_bytes[pos + 4] & 0x7F) << 21)
                    | (usize::from(header_bytes[pos + 5] & 0x7F) << 14)
                    | (usize::from(header_bytes[pos + 6] & 0x7F) << 7)
                    | usize::from(header_bytes[pos + 7] & 0x7F)
            } else {
                let raw_sz = u32::from_be_bytes([
                    header_bytes[pos + 4],
                    header_bytes[pos + 5],
                    header_bytes[pos + 6],
                    header_bytes[pos + 7],
                ]);
                match usize::try_from(raw_sz) {
                    Ok(n) => n,
                    Err(_) => break,
                }
            };
            let flags_lo = header_bytes[pos + 9];
            let bad = (flags_lo & 0x0E) != 0;
            let data_len_ind = if major == 4 && (flags_lo & 0x01) != 0 {
                4usize
            } else {
                0usize
            };
            (&header_bytes[pos..pos + 4], sz, 10usize, data_len_ind, bad)
        };

        let Some(payload_start) = pos.checked_add(hdr_len) else {
            break;
        };
        let Some(next_pos) = payload_start.checked_add(frame_size) else {
            break;
        };
        if next_pos > tag_end || frame_size == 0 {
            break;
        }
        let payload = &header_bytes[payload_start..next_pos];
        if !unsupported_flags && payload.len() > skip_payload_prefix {
            let p = &payload[skip_payload_prefix..];
            let base_off = payload_start + skip_payload_prefix;
            on_frame(major, frame_id, p, base_off);
        }
        pos = next_pos;
    }
}

fn parse_id3_lang_desc_text(p: &[u8]) -> Option<(String, String, String)> {
    if p.len() < 5 {
        return None;
    }
    let encoding = p[0];
    let lang_raw: String = p[1..4]
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| char::from(b))
        .collect();
    let lang = clean_tag_string(&lang_raw);
    let two_byte = encoding == 1 || encoding == 2;
    let (desc_end, text_rel) = find_id3_nul_end(p, 4, two_byte)?;
    let desc = decode_id3_text(encoding, &p[4..desc_end]);
    let text = decode_id3_text(encoding, &p[text_rel..]);
    if desc.is_empty() && text.is_empty() {
        return None;
    }
    Some((lang, desc, text))
}

fn extract_id3v2_comm_uslt(
    header_bytes: &[u8],
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let mut comm_entries = Vec::new();
    let mut uslt_entries = Vec::new();
    visit_id3v2_frames(header_bytes, |major, frame_id, p, _| {
        let is_comm = (major == 2 && frame_id == b"COM") || (major >= 3 && frame_id == b"COMM");
        let is_uslt = (major == 2 && frame_id == b"ULT") || (major >= 3 && frame_id == b"USLT");
        if is_comm {
            if let Some((lang, desc, text)) = parse_id3_lang_desc_text(p) {
                comm_entries.push(serde_json::json!([lang, desc, text]));
            }
        } else if is_uslt {
            if let Some((lang, desc, text)) = parse_id3_lang_desc_text(p) {
                uslt_entries.push(serde_json::json!([lang, desc, text]));
            }
        }
    });
    (comm_entries, uslt_entries)
}

fn extract_id3v2_no_artwork(
    header_bytes: &[u8],
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    multi_value_keys: &mut HashSet<String>,
) {
    let mut entries = Vec::new();
    visit_id3v2_frames(header_bytes, |major, frame_id, p, base_off| {
        if major == 2 && frame_id == b"PIC" && p.len() >= 6 {
            let encoding = p[0];
            let mime = match &p[1..4] {
                b"JPG" => "image/jpeg".to_string(),
                b"PNG" => "image/png".to_string(),
                other => {
                    let s: String = other.iter().map(|&b| char::from(b)).collect();
                    s.to_ascii_lowercase()
                }
            };
            let pic_type = p[4];
            let two_byte = encoding == 1 || encoding == 2;
            if let Some((desc_end, data_rel)) = find_id3_nul_end(p, 5, two_byte) {
                let desc = decode_id3_text(encoding, &p[5..desc_end]);
                let img_len = p.len().saturating_sub(data_rel);
                if img_len > 0 {
                    let abs_off = u64::try_from(base_off + data_rel).ok();
                    entries.push(serde_json::json!([mime, pic_type, desc, img_len, abs_off]));
                }
            }
        } else if major >= 3 && frame_id == b"APIC" && p.len() >= 4 {
            let encoding = p[0];
            if let Some((mime_end, after_mime)) = find_id3_nul_end(p, 1, false) {
                if after_mime < p.len() {
                    let mime_raw = String::from_utf8_lossy(&p[1..mime_end]);
                    let mime = if mime_raw.is_empty() {
                        "image/jpeg".to_string()
                    } else {
                        clean_tag_string(&mime_raw)
                    };
                    let pic_type = p[after_mime];
                    let two_byte = encoding == 1 || encoding == 2;
                    if let Some((desc_end, data_rel)) =
                        find_id3_nul_end(p, after_mime + 1, two_byte)
                    {
                        let desc = decode_id3_text(encoding, &p[after_mime + 1..desc_end]);
                        let img_len = p.len().saturating_sub(data_rel);
                        if img_len > 0 {
                            let abs_off = u64::try_from(base_off + data_rel).ok();
                            entries
                                .push(serde_json::json!([mime, pic_type, desc, img_len, abs_off]));
                        }
                    }
                }
            }
        }
    });
    append_apic_entries(tags_map, multi_value_keys, entries);
}

fn extract_flac_no_artwork(
    header_bytes: &[u8],
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
) {
    if header_bytes.len() < 4 || &header_bytes[0..4] != b"fLaC" {
        return;
    }
    let mut pic_objects = Vec::new();
    let mut pos: usize = 4;
    while header_bytes.len().saturating_sub(pos) >= 4 {
        let header_byte = header_bytes[pos];
        let is_last = (header_byte & 0x80) != 0;
        let block_type = header_byte & 0x7F;
        let block_len = (usize::from(header_bytes[pos + 1]) << 16)
            | (usize::from(header_bytes[pos + 2]) << 8)
            | usize::from(header_bytes[pos + 3]);
        let Some(data_start) = pos.checked_add(4) else {
            break;
        };
        let Some(next_pos) = data_start.checked_add(block_len) else {
            break;
        };
        if next_pos > header_bytes.len() {
            break;
        }
        if block_type == 6 && block_len >= 32 {
            let b = &header_bytes[data_start..next_pos];
            let pic_type = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            let mime_len =
                usize::try_from(u32::from_be_bytes([b[4], b[5], b[6], b[7]])).unwrap_or(usize::MAX);
            if let Some(mime_end) = 8usize.checked_add(mime_len) {
                if mime_end.saturating_add(4) <= b.len() {
                    let mime_raw = String::from_utf8_lossy(&b[8..mime_end]);
                    let mime = if mime_raw.is_empty() {
                        "image/jpeg".to_string()
                    } else {
                        clean_tag_string(&mime_raw)
                    };
                    let desc_len = usize::try_from(u32::from_be_bytes([
                        b[mime_end],
                        b[mime_end + 1],
                        b[mime_end + 2],
                        b[mime_end + 3],
                    ]))
                    .unwrap_or(usize::MAX);
                    if let Some(desc_end) = mime_end
                        .checked_add(4)
                        .and_then(|s| s.checked_add(desc_len))
                    {
                        if desc_end.saturating_add(20) <= b.len() {
                            let desc = clean_tag_string(&String::from_utf8_lossy(
                                &b[mime_end + 4..desc_end],
                            ));
                            let m = desc_end;
                            let width = u32::from_be_bytes([b[m], b[m + 1], b[m + 2], b[m + 3]]);
                            let height =
                                u32::from_be_bytes([b[m + 4], b[m + 5], b[m + 6], b[m + 7]]);
                            let depth =
                                u32::from_be_bytes([b[m + 8], b[m + 9], b[m + 10], b[m + 11]]);
                            let color_index =
                                u32::from_be_bytes([b[m + 12], b[m + 13], b[m + 14], b[m + 15]]);
                            let img_len = usize::try_from(u32::from_be_bytes([
                                b[m + 16],
                                b[m + 17],
                                b[m + 18],
                                b[m + 19],
                            ]))
                            .unwrap_or(0);
                            let img_rel = m + 20;
                            if img_len > 0 && img_rel.saturating_add(img_len) <= b.len() {
                                let abs_off = u64::try_from(data_start + img_rel).ok();
                                pic_objects.push(serde_json::json!({
                                    "mime_type": mime,
                                    "picture_type": u8::try_from(pic_type).unwrap_or(0),
                                    "description": desc,
                                    "width": width,
                                    "height": height,
                                    "depth": depth,
                                    "color_index": color_index,
                                    "image_data": img_len,
                                    "offset": abs_off,
                                }));
                            }
                        }
                    }
                }
            }
        }
        if is_last {
            break;
        }
        pos = next_pos;
    }
    append_allpictures(tags_map, pic_objects);
}

fn find_mp4_subbox(bytes: &[u8], base_offset: usize, target: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos: usize = 0;
    while bytes.len().saturating_sub(pos) >= 8 {
        let box_len =
            u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        let box_type = &bytes[pos + 4..pos + 8];
        let (actual_len, header_len) = if box_len == 1 {
            if bytes.len().saturating_sub(pos) < 16 {
                break;
            }
            let sz64 = u64::from_be_bytes([
                bytes[pos + 8],
                bytes[pos + 9],
                bytes[pos + 10],
                bytes[pos + 11],
                bytes[pos + 12],
                bytes[pos + 13],
                bytes[pos + 14],
                bytes[pos + 15],
            ]);
            (usize::try_from(sz64).ok()?, 16usize)
        } else {
            (usize::try_from(box_len).ok()?, 8usize)
        };
        if actual_len < header_len {
            break;
        }
        let next_pos = pos.checked_add(actual_len)?;
        if next_pos > bytes.len() {
            break;
        }
        if box_type == target {
            return Some((base_offset + pos + header_len, base_offset + next_pos));
        }
        pos = next_pos;
    }
    None
}

fn extract_mp4_no_artwork(
    header_bytes: &[u8],
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
) {
    if tags_map.contains_key("COVR") {
        return;
    }
    let Some((moov_s, moov_e)) = find_mp4_subbox(header_bytes, 0, b"moov") else {
        return;
    };
    let Some((udta_s, udta_e)) = find_mp4_subbox(&header_bytes[moov_s..moov_e], moov_s, b"udta")
    else {
        return;
    };
    let Some((meta_s, meta_e)) = find_mp4_subbox(&header_bytes[udta_s..udta_e], udta_s, b"meta")
    else {
        return;
    };
    if meta_e.saturating_sub(meta_s) < 4 {
        return;
    }
    let meta_body = meta_s + 4;
    let Some((ilst_s, ilst_e)) =
        find_mp4_subbox(&header_bytes[meta_body..meta_e], meta_body, b"ilst")
    else {
        return;
    };
    let Some((covr_s, covr_e)) = find_mp4_subbox(&header_bytes[ilst_s..ilst_e], ilst_s, b"covr")
    else {
        return;
    };
    let Some((data_s, data_e)) = find_mp4_subbox(&header_bytes[covr_s..covr_e], covr_s, b"data")
    else {
        return;
    };
    let img_start = data_s.saturating_add(8);
    if data_e > img_start {
        let img_len = data_e - img_start;
        tags_map.insert("COVR".to_string(), serde_json::json!(img_len));
        tags_map.insert(
            "COVR_offset".to_string(),
            serde_json::json!(u64::try_from(img_start).ok()),
        );
    }
}

fn find_mp4_subbox_range(bytes: &[u8], target: &[u8; 4]) -> Option<(usize, usize)> {
    find_mp4_subbox(bytes, 0, target)
}

fn parse_mp4_esds_bitrates(esds_body: &[u8]) -> (Option<u32>, Option<u32>) {
    if esds_body.len() < 4 {
        return (None, None);
    }
    let mut pos: usize = 4;
    while pos < esds_body.len() {
        let tag = esds_body[pos];
        pos += 1;
        let mut size: usize = 0;
        let mut count = 0;
        while pos < esds_body.len() && count < 4 {
            let b = esds_body[pos];
            pos += 1;
            count += 1;
            size = (size << 7) | usize::from(b & 0x7F);
            if (b & 0x80) == 0 {
                break;
            }
        }
        let Some(end) = pos.checked_add(size) else {
            break;
        };
        if end > esds_body.len() {
            break;
        }
        if tag == 0x03 {
            if size < 3 {
                break;
            }
            let flags = esds_body[pos + 2];
            let mut inner = pos + 3;
            if (flags & 0x80) != 0 {
                inner = inner.saturating_add(2);
            }
            if (flags & 0x40) != 0 {
                if inner < end {
                    let url_len = usize::from(esds_body[inner]);
                    inner = inner.saturating_add(1).saturating_add(url_len);
                } else {
                    break;
                }
            }
            if (flags & 0x20) != 0 {
                inner = inner.saturating_add(2);
            }
            if inner < end {
                pos = inner;
                continue;
            }
            break;
        } else if tag == 0x04 {
            if size >= 13 {
                let max_br = u32::from_be_bytes([
                    esds_body[pos + 5],
                    esds_body[pos + 6],
                    esds_body[pos + 7],
                    esds_body[pos + 8],
                ]);
                let avg_br = u32::from_be_bytes([
                    esds_body[pos + 9],
                    esds_body[pos + 10],
                    esds_body[pos + 11],
                    esds_body[pos + 12],
                ]);
                return (
                    (avg_br > 0).then_some(avg_br),
                    (max_br > 0).then_some(max_br),
                );
            }
            break;
        }
        pos = end;
    }
    (None, None)
}

struct Mp4MoovScan<'a> {
    moov: Option<Cow<'a, [u8]>>,
    leading_mdat: bool,
}

/// Locate the MP4 `moov` box body and check whether `mdat` precedes `moov`.
fn find_mp4_moov_and_leading_mdat<'a>(
    file: &mut std::fs::File,
    file_size: u64,
    header_bytes: &'a [u8],
) -> Result<Mp4MoovScan<'a>, AudioScanError> {
    let mut pos: u64 = 0;
    let mut seen_mdat = false;
    let header_len_u64 = u64::try_from(header_bytes.len()).unwrap_or(u64::MAX);
    let max_cap_u64 = u64::try_from(MAX_HEADER_READ).unwrap_or(u64::MAX);

    for _ in 0..MAX_CONTAINER_SCAN_STEPS {
        if file_size.saturating_sub(pos) < 8 {
            break;
        }
        let parsed = if pos < header_len_u64 {
            let pos_usize = usize::try_from(pos).unwrap_or(usize::MAX);
            if pos_usize < header_bytes.len() && header_bytes.len().saturating_sub(pos_usize) >= 16
            {
                parse_mp4_box_header(&header_bytes[pos_usize..])
            } else {
                let hdr = read_window_at(file, file_size, pos, 16)?;
                parse_mp4_box_header(&hdr)
            }
        } else {
            let hdr = read_window_at(file, file_size, pos, 16)?;
            parse_mp4_box_header(&hdr)
        };
        let Some((actual_len, box_hdr_len, box_type)) = parsed else {
            break;
        };
        if &box_type == b"mdat" {
            seen_mdat = true;
        } else if &box_type == b"moov" {
            let body_start = pos.saturating_add(box_hdr_len);
            let body_len = actual_len.saturating_sub(box_hdr_len);
            let body_end = body_start.saturating_add(body_len);
            if body_end <= header_len_u64 {
                if let (Ok(s), Ok(e)) = (usize::try_from(body_start), usize::try_from(body_end)) {
                    if let Some(slice) = header_bytes.get(s..e) {
                        return Ok(Mp4MoovScan {
                            moov: Some(Cow::Borrowed(slice)),
                            leading_mdat: seen_mdat,
                        });
                    }
                }
            }
            if body_len <= max_cap_u64 {
                let len = usize::try_from(body_len).unwrap_or(MAX_HEADER_READ);
                let buf = read_window_at(file, file_size, body_start, len)?;
                return Ok(Mp4MoovScan {
                    moov: Some(Cow::Owned(buf)),
                    leading_mdat: seen_mdat,
                });
            }
            return Ok(Mp4MoovScan {
                moov: None,
                leading_mdat: seen_mdat,
            });
        }
        let Some(next_pos) = pos.checked_add(actual_len) else {
            break;
        };
        if next_pos > file_size {
            break;
        }
        pos = next_pos;
    }
    Ok(Mp4MoovScan {
        moov: None,
        leading_mdat: false,
    })
}

fn parse_mp4_trak_box(trak: &[u8]) -> Option<(serde_json::Value, Option<u64>, bool)> {
    let mut track_id: Option<u32> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    if let Some((tkhd_s, tkhd_e)) = find_mp4_subbox_range(trak, b"tkhd") {
        let tkhd = &trak[tkhd_s..tkhd_e];
        if !tkhd.is_empty() {
            if tkhd[0] == 0 && tkhd.len() >= 16 {
                track_id = Some(u32::from_be_bytes([tkhd[12], tkhd[13], tkhd[14], tkhd[15]]));
                if tkhd.len() >= 84 {
                    let w = u32::from_be_bytes([tkhd[76], tkhd[77], tkhd[78], tkhd[79]]) >> 16;
                    let h = u32::from_be_bytes([tkhd[80], tkhd[81], tkhd[82], tkhd[83]]) >> 16;
                    if w > 0 {
                        width = Some(w);
                    }
                    if h > 0 {
                        height = Some(h);
                    }
                }
            } else if tkhd[0] == 1 && tkhd.len() >= 24 {
                track_id = Some(u32::from_be_bytes([tkhd[20], tkhd[21], tkhd[22], tkhd[23]]));
                if tkhd.len() >= 96 {
                    let w = u32::from_be_bytes([tkhd[88], tkhd[89], tkhd[90], tkhd[91]]) >> 16;
                    let h = u32::from_be_bytes([tkhd[92], tkhd[93], tkhd[94], tkhd[95]]) >> 16;
                    if w > 0 {
                        width = Some(w);
                    }
                    if h > 0 {
                        height = Some(h);
                    }
                }
            }
        }
    }

    let (mdia_s, mdia_e) = find_mp4_subbox_range(trak, b"mdia")?;
    let mdia = &trak[mdia_s..mdia_e];

    let mut duration_ms: Option<u64> = None;
    if let Some((mdhd_s, mdhd_e)) = find_mp4_subbox_range(mdia, b"mdhd") {
        let mdhd = &mdia[mdhd_s..mdhd_e];
        if !mdhd.is_empty() {
            let (timescale, dur) = if mdhd[0] == 0 && mdhd.len() >= 20 {
                let ts = u32::from_be_bytes([mdhd[12], mdhd[13], mdhd[14], mdhd[15]]);
                let d = u64::from(u32::from_be_bytes([mdhd[16], mdhd[17], mdhd[18], mdhd[19]]));
                (ts, d)
            } else if mdhd[0] == 1 && mdhd.len() >= 32 {
                let ts = u32::from_be_bytes([mdhd[20], mdhd[21], mdhd[22], mdhd[23]]);
                let d = u64::from_be_bytes([
                    mdhd[24], mdhd[25], mdhd[26], mdhd[27], mdhd[28], mdhd[29], mdhd[30], mdhd[31],
                ]);
                (ts, d)
            } else {
                (0, 0)
            };
            if timescale > 0 {
                let ms = u128::from(dur).saturating_mul(1000) / u128::from(timescale);
                duration_ms = u64::try_from(ms).ok();
            }
        }
    }

    let mut handler_4cc = [0u8; 4];
    let mut track_type = "unknown".to_string();
    if let Some((hdlr_s, hdlr_e)) = find_mp4_subbox_range(mdia, b"hdlr") {
        let hdlr = &mdia[hdlr_s..hdlr_e];
        if hdlr.len() >= 12 {
            handler_4cc.copy_from_slice(&hdlr[8..12]);
            track_type = match &handler_4cc {
                b"soun" => "audio".to_string(),
                b"vide" => "video".to_string(),
                other => String::from_utf8_lossy(other).into_owned(),
            };
        }
    }

    let mut encoding: Option<String> = None;
    let mut samplerate: Option<u32> = None;
    let mut channels: Option<u16> = None;
    let mut bits_per_sample: Option<u16> = None;
    let mut avg_bitrate: Option<u64> = None;
    let mut max_bitrate: Option<u64> = None;

    if let Some((minf_s, minf_e)) = find_mp4_subbox_range(mdia, b"minf") {
        let minf = &mdia[minf_s..minf_e];
        if let Some((stbl_s, stbl_e)) = find_mp4_subbox_range(minf, b"stbl") {
            let stbl = &minf[stbl_s..stbl_e];
            if let Some((stsd_s, stsd_e)) = find_mp4_subbox_range(stbl, b"stsd") {
                let stsd = &stbl[stsd_s..stsd_e];
                if stsd.len() >= 16 {
                    let entry = &stsd[8..];
                    if let Some((entry_len, entry_hdr, coding)) = parse_mp4_box_header(entry) {
                        let coding_str = String::from_utf8_lossy(&coding).into_owned();
                        encoding = Some(coding_str);
                        let end = usize::try_from(entry_len)
                            .unwrap_or(entry.len())
                            .min(entry.len());
                        let sample_entry = &entry[..end];
                        let is_audio_coding = matches!(
                            &coding,
                            b"mp4a"
                                | b"alac"
                                | b"ac-3"
                                | b"ec-3"
                                | b"fLaC"
                                | b"Opus"
                                | b"lpcm"
                                | b"twos"
                                | b"sowt"
                        );
                        if (&handler_4cc == b"soun" || is_audio_coding)
                            && sample_entry.len() >= 36
                            && entry_hdr == 8
                        {
                            let version = u16::from_be_bytes([sample_entry[16], sample_entry[17]]);
                            let ch = u16::from_be_bytes([sample_entry[24], sample_entry[25]]);
                            let bps = u16::from_be_bytes([sample_entry[26], sample_entry[27]]);
                            let sr = u32::from_be_bytes([
                                sample_entry[32],
                                sample_entry[33],
                                sample_entry[34],
                                sample_entry[35],
                            ]) >> 16;
                            if ch > 0 {
                                channels = Some(ch);
                            }
                            if bps > 0 {
                                bits_per_sample = Some(bps);
                            }
                            if sr > 0 {
                                samplerate = Some(sr);
                            }
                            let sub_off = match version {
                                1 => 52usize,
                                2 => 72usize,
                                _ => 36usize,
                            };
                            if sub_off < sample_entry.len() {
                                let sub = &sample_entry[sub_off..];
                                if let Some((esds_s, esds_e)) = find_mp4_subbox_range(sub, b"esds")
                                {
                                    let (avg, max) = parse_mp4_esds_bitrates(&sub[esds_s..esds_e]);
                                    avg_bitrate = avg.map(u64::from);
                                    max_bitrate = max.map(u64::from);
                                }
                                if let Some((alac_s, alac_e)) = find_mp4_subbox_range(sub, b"alac")
                                {
                                    let alac = &sub[alac_s..alac_e];
                                    if alac.len() >= 28 && alac[0..4] == [0, 0, 0, 0] {
                                        let depth = u16::from(alac[9]);
                                        let alac_ch = u16::from(alac[13]);
                                        let alac_avg = u32::from_be_bytes([
                                            alac[20], alac[21], alac[22], alac[23],
                                        ]);
                                        let alac_sr = u32::from_be_bytes([
                                            alac[24], alac[25], alac[26], alac[27],
                                        ]);
                                        if depth > 0 {
                                            bits_per_sample = Some(depth);
                                        }
                                        if alac_ch > 0 {
                                            channels = Some(alac_ch);
                                        }
                                        if alac_avg > 0 {
                                            avg_bitrate = Some(u64::from(alac_avg));
                                        }
                                        if alac_sr > 0 {
                                            samplerate = Some(alac_sr);
                                        }
                                    }
                                }
                            }
                        } else if (&handler_4cc == b"vide"
                            || matches!(
                                &coding,
                                b"avc1"
                                    | b"avc3"
                                    | b"hvc1"
                                    | b"hev1"
                                    | b"mp4v"
                                    | b"vp08"
                                    | b"vp09"
                                    | b"av01"
                                    | b"s263"
                                    | b"encv"
                            ))
                            && sample_entry.len() >= 36
                            && entry_hdr == 8
                        {
                            let stsd_w =
                                u32::from(u16::from_be_bytes([sample_entry[32], sample_entry[33]]));
                            let stsd_h =
                                u32::from(u16::from_be_bytes([sample_entry[34], sample_entry[35]]));
                            if stsd_w > 0 {
                                width = Some(stsd_w);
                            }
                            if stsd_h > 0 {
                                height = Some(stsd_h);
                            }
                        }
                    }
                }
            }
        }
    }

    let mut obj = serde_json::Map::new();
    if let Some(id) = track_id {
        obj.insert("id".to_string(), serde_json::json!(id));
    }
    obj.insert("type".to_string(), serde_json::json!(track_type));
    if let Some(enc) = encoding {
        obj.insert("encoding".to_string(), serde_json::json!(enc));
    }
    let is_video = &handler_4cc == b"vide" || track_type == "video";
    if is_video {
        if let Some(w) = width {
            obj.insert("width".to_string(), serde_json::json!(w));
        }
        if let Some(h) = height {
            obj.insert("height".to_string(), serde_json::json!(h));
        }
    }
    if let Some(sr) = samplerate {
        obj.insert("samplerate".to_string(), serde_json::json!(sr));
    }
    if let Some(ch) = channels {
        obj.insert("channels".to_string(), serde_json::json!(ch));
    }
    if let Some(bps) = bits_per_sample {
        obj.insert("bits_per_sample".to_string(), serde_json::json!(bps));
    }
    if let Some(abr) = avg_bitrate {
        obj.insert("avg_bitrate".to_string(), serde_json::json!(abr));
    }
    if let Some(mbr) = max_bitrate {
        obj.insert("max_bitrate".to_string(), serde_json::json!(mbr));
    }
    if let Some(dur) = duration_ms {
        obj.insert("duration_ms".to_string(), serde_json::json!(dur));
    }
    let is_audio = &handler_4cc == b"soun" || track_type == "audio";
    Some((serde_json::Value::Object(obj), avg_bitrate, is_audio))
}

fn populate_mp4_info(
    file: &mut std::fs::File,
    file_size: u64,
    header_bytes: &[u8],
    audio_size: u64,
    song_length_ms: u64,
    props_bitrate_bps: Option<u64>,
    info_map: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), AudioScanError> {
    let mut tracks = Vec::new();
    let mut audio_avg_bitrate: Option<u64> = None;

    let moov_scan = find_mp4_moov_and_leading_mdat(file, file_size, header_bytes)?;
    if moov_scan.leading_mdat {
        info_map.insert("leading_mdat".to_string(), serde_json::json!(1));
    }
    if let Some(moov) = moov_scan.moov {
        let mut pos: usize = 0;
        while moov.len().saturating_sub(pos) >= 8 {
            let Some((box_len, header_len, box_type)) = parse_mp4_box_header(&moov[pos..]) else {
                break;
            };
            let Ok(actual_len) = usize::try_from(box_len) else {
                break;
            };
            let Ok(hdr_len) = usize::try_from(header_len) else {
                break;
            };
            let Some(next_pos) = pos.checked_add(actual_len) else {
                break;
            };
            if next_pos > moov.len() {
                break;
            }
            if &box_type == b"trak" {
                if let Some((track_val, track_abr, is_audio)) =
                    parse_mp4_trak_box(&moov[pos + hdr_len..next_pos])
                {
                    if is_audio && audio_avg_bitrate.is_none() {
                        audio_avg_bitrate = track_abr;
                    }
                    tracks.push(track_val);
                }
            }
            pos = next_pos;
        }
    }

    if !tracks.is_empty() {
        info_map.insert("tracks".to_string(), serde_json::Value::Array(tracks));
    }

    let computed_bps = if song_length_ms > 0 && audio_size > 0 {
        u64::try_from(u128::from(audio_size).saturating_mul(8000) / u128::from(song_length_ms))
            .ok()
            .filter(|&v| v > 0)
    } else {
        None
    };

    if let Some(abr) = audio_avg_bitrate.or(props_bitrate_bps).or(computed_bps) {
        info_map.insert("avg_bitrate".to_string(), serde_json::json!(abr));
        if !info_map.contains_key("bitrate") {
            info_map.insert("bitrate".to_string(), serde_json::json!(abr));
        }
    }
    Ok(())
}

fn populate_ogg_info(
    header_bytes: &[u8],
    audio_size: u64,
    song_length_ms: u64,
    props_bitrate_bps: Option<u64>,
    info_map: &mut serde_json::Map<String, serde_json::Value>,
) {
    if header_bytes.len() < 27 || &header_bytes[0..4] != b"OggS" {
        return;
    }
    let num_segments = usize::from(header_bytes[26]);
    let body_start = 27usize.saturating_add(num_segments);
    if header_bytes.len() <= body_start {
        return;
    }
    let mut pkt_len: usize = 0;
    for &lace in &header_bytes[27..body_start] {
        pkt_len = pkt_len.saturating_add(usize::from(lace));
        if lace < 255 {
            break;
        }
    }
    let pkt_end = body_start.saturating_add(pkt_len).min(header_bytes.len());
    let pkt = &header_bytes[body_start..pkt_end];

    let mut nominal: Option<u64> = None;
    let mut upper: Option<u64> = None;
    let mut lower: Option<u64> = None;
    let mut ogg_type: Option<&'static str> = None;

    if pkt.starts_with(b"\x01vorbis") && pkt.len() >= 30 {
        ogg_type = Some("vorbis");
        let max_br = i32::from_le_bytes([pkt[16], pkt[17], pkt[18], pkt[19]]);
        let nom_br = i32::from_le_bytes([pkt[20], pkt[21], pkt[22], pkt[23]]);
        let min_br = i32::from_le_bytes([pkt[24], pkt[25], pkt[26], pkt[27]]);
        if max_br > 0 {
            upper = u64::try_from(max_br).ok();
        }
        if nom_br > 0 {
            nominal = u64::try_from(nom_br).ok();
        }
        if min_br > 0 {
            lower = u64::try_from(min_br).ok();
        }
    } else if pkt.starts_with(b"OpusHead") {
        ogg_type = Some("opus");
    } else if pkt.starts_with(b"Speex   ") {
        ogg_type = Some("speex");
        if pkt.len() >= 52 {
            let br = i32::from_le_bytes([pkt[48], pkt[49], pkt[50], pkt[51]]);
            if br > 0 {
                nominal = u64::try_from(br).ok();
            }
        }
    } else if pkt.starts_with(b"\x7fFLAC") {
        ogg_type = Some("flac");
    }

    if let Some(kind) = ogg_type {
        info_map.insert("ogg_type".to_string(), serde_json::json!(kind));
    }
    if let Some(u) = upper {
        info_map.insert("bitrate_upper".to_string(), serde_json::json!(u));
    }
    if let Some(n) = nominal {
        info_map.insert("bitrate_nominal".to_string(), serde_json::json!(n));
    }
    if let Some(l) = lower {
        info_map.insert("bitrate_lower".to_string(), serde_json::json!(l));
    }

    let computed_avg = if song_length_ms > 0 && audio_size > 0 {
        u64::try_from(u128::from(audio_size).saturating_mul(8000) / u128::from(song_length_ms))
            .ok()
            .filter(|&v| v > 0)
    } else {
        None
    };

    if let Some(avg) = props_bitrate_bps.or(computed_avg).or(nominal) {
        info_map.insert("bitrate_average".to_string(), serde_json::json!(avg));
    }
}

fn decode_utf16le_trimmed(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    clean_tag_string(&String::from_utf16_lossy(&units))
}

fn find_utf16le_nul_end(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut pos = start;
    while pos + 1 < bytes.len() {
        if bytes[pos] == 0 && bytes[pos + 1] == 0 {
            return Some((pos, pos + 2));
        }
        pos += 2;
    }
    None
}

fn scan_asf_header(
    header_bytes: &[u8],
    file_size: u64,
    audio_offset: u64,
    include_info: bool,
    include_tags: bool,
    no_artwork: bool,
) -> Result<ScanResult, AudioScanError> {
    if header_bytes.len() < 30 || !header_bytes.starts_with(&ASF_HEADER_GUID) {
        return Err(AudioScanError::Scan("Invalid ASF header".to_string()));
    }
    let hdr_size = u64::from_le_bytes([
        header_bytes[16],
        header_bytes[17],
        header_bytes[18],
        header_bytes[19],
        header_bytes[20],
        header_bytes[21],
        header_bytes[22],
        header_bytes[23],
    ]);
    let num_objects = u32::from_le_bytes([
        header_bytes[24],
        header_bytes[25],
        header_bytes[26],
        header_bytes[27],
    ]);
    let hdr_end = usize::try_from(hdr_size)
        .unwrap_or(header_bytes.len())
        .min(header_bytes.len());

    let mut info_map = serde_json::Map::new();
    let mut tags_map = serde_json::Map::new();
    let mut multi_value_keys = HashSet::new();
    let mut total_inline_artwork_bytes: usize = 0;

    let mut streams = Vec::new();
    let mut song_length_ms: Option<u64> = None;
    let mut max_bitrate: Option<u32> = None;
    let mut first_audio_bitrate: Option<u64> = None;
    let mut first_audio_samplerate: Option<u32> = None;
    let mut first_audio_channels: Option<u16> = None;
    let mut first_audio_bits: Option<u16> = None;
    let mut any_lossless = false;

    let mut pos: usize = 30;
    let capped_objects = usize::try_from(num_objects)
        .unwrap_or(MAX_CONTAINER_SCAN_STEPS)
        .min(MAX_CONTAINER_SCAN_STEPS);
    for _ in 0..capped_objects {
        if hdr_end.saturating_sub(pos) < 24 {
            break;
        }
        let guid = &header_bytes[pos..pos + 16];
        let obj_size_u64 = u64::from_le_bytes([
            header_bytes[pos + 16],
            header_bytes[pos + 17],
            header_bytes[pos + 18],
            header_bytes[pos + 19],
            header_bytes[pos + 20],
            header_bytes[pos + 21],
            header_bytes[pos + 22],
            header_bytes[pos + 23],
        ]);
        let Ok(obj_size) = usize::try_from(obj_size_u64) else {
            break;
        };
        if obj_size < 24 {
            break;
        }
        let Some(next_pos) = pos.checked_add(obj_size) else {
            break;
        };
        if next_pos > hdr_end {
            break;
        }
        let body_start = pos + 24;
        let body = &header_bytes[body_start..next_pos];

        if guid == ASF_FILE_PROPERTIES_GUID && body.len() >= 80 {
            let play_dur_100ns = u64::from_le_bytes([
                body[40], body[41], body[42], body[43], body[44], body[45], body[46], body[47],
            ]);
            let preroll_ms = u64::from_le_bytes([
                body[56], body[57], body[58], body[59], body[60], body[61], body[62], body[63],
            ]);
            let max_br = u32::from_le_bytes([body[76], body[77], body[78], body[79]]);
            let dur_ms = (play_dur_100ns / 10_000).saturating_sub(preroll_ms);
            song_length_ms = Some(dur_ms);
            if max_br > 0 {
                max_bitrate = Some(max_br);
            }
        } else if guid == ASF_STREAM_PROPERTIES_GUID && body.len() >= 54 {
            let stream_type_guid = &body[0..16];
            let type_spec_len =
                usize::try_from(u32::from_le_bytes([body[40], body[41], body[42], body[43]]))
                    .unwrap_or(0);
            let flags = u16::from_le_bytes([body[48], body[49]]);
            let stream_number = flags & 0x007F;
            let encrypted = (flags & 0x8000) != 0;
            let spec_end = 54usize.saturating_add(type_spec_len).min(body.len());
            let spec = &body[54..spec_end];

            if stream_type_guid == ASF_AUDIO_MEDIA_GUID && spec.len() >= 16 {
                let format_tag = u16::from_le_bytes([spec[0], spec[1]]);
                let ch = u16::from_le_bytes([spec[2], spec[3]]);
                let sr = u32::from_le_bytes([spec[4], spec[5], spec[6], spec[7]]);
                let avg_bytes_per_sec = u32::from_le_bytes([spec[8], spec[9], spec[10], spec[11]]);
                let bps = u16::from_le_bytes([spec[14], spec[15]]);
                let br_bps = u64::from(avg_bytes_per_sec).saturating_mul(8);
                let is_lossless = format_tag == 0x0163 || format_tag == 0x0001;
                if is_lossless {
                    any_lossless = true;
                }
                if first_audio_samplerate.is_none() && sr > 0 {
                    first_audio_samplerate = Some(sr);
                }
                if first_audio_channels.is_none() && ch > 0 {
                    first_audio_channels = Some(ch);
                }
                if first_audio_bitrate.is_none() && br_bps > 0 {
                    first_audio_bitrate = Some(br_bps);
                }
                if first_audio_bits.is_none() && bps > 0 {
                    first_audio_bits = Some(bps);
                }
                let mut stream_obj = serde_json::Map::new();
                stream_obj.insert(
                    "stream_number".to_string(),
                    serde_json::json!(stream_number),
                );
                stream_obj.insert("stream_type".to_string(), serde_json::json!("audio"));
                stream_obj.insert("codec_id".to_string(), serde_json::json!(format_tag));
                stream_obj.insert("channels".to_string(), serde_json::json!(ch));
                stream_obj.insert("samplerate".to_string(), serde_json::json!(sr));
                stream_obj.insert("bitrate".to_string(), serde_json::json!(br_bps));
                if bps > 0 {
                    stream_obj.insert("bits_per_sample".to_string(), serde_json::json!(bps));
                }
                stream_obj.insert(
                    "lossless".to_string(),
                    serde_json::json!(if is_lossless { 1 } else { 0 }),
                );
                stream_obj.insert(
                    "encrypted".to_string(),
                    serde_json::json!(if encrypted { 1 } else { 0 }),
                );
                streams.push(serde_json::Value::Object(stream_obj));
            } else {
                let kind = if stream_type_guid == ASF_VIDEO_MEDIA_GUID {
                    "video"
                } else {
                    "other"
                };
                streams.push(serde_json::json!({
                    "stream_number": stream_number,
                    "stream_type": kind,
                    "encrypted": if encrypted { 1 } else { 0 },
                }));
            }
        } else if include_tags && guid == ASF_CONTENT_DESC_GUID && body.len() >= 10 {
            let lens = [
                usize::from(u16::from_le_bytes([body[0], body[1]])),
                usize::from(u16::from_le_bytes([body[2], body[3]])),
                usize::from(u16::from_le_bytes([body[4], body[5]])),
                usize::from(u16::from_le_bytes([body[6], body[7]])),
                usize::from(u16::from_le_bytes([body[8], body[9]])),
            ];
            let names = ["Title", "Author", "Copyright", "Description", "Rating"];
            let mut cur = 10usize;
            for (name, len) in names.iter().zip(lens.iter()) {
                let Some(end) = cur.checked_add(*len) else {
                    break;
                };
                if end > body.len() {
                    break;
                }
                let text = decode_utf16le_trimmed(&body[cur..end]);
                if !text.is_empty() {
                    insert_tag_value(
                        &mut tags_map,
                        &mut multi_value_keys,
                        (*name).to_string(),
                        serde_json::json!(text),
                    );
                }
                cur = end;
            }
        } else if include_tags && guid == ASF_EXT_CONTENT_DESC_GUID && body.len() >= 2 {
            let count = usize::from(u16::from_le_bytes([body[0], body[1]]));
            let mut cur = 2usize;
            for _ in 0..count {
                if body.len().saturating_sub(cur) < 2 {
                    break;
                }
                let name_len = usize::from(u16::from_le_bytes([body[cur], body[cur + 1]]));
                cur += 2;
                let Some(name_end) = cur.checked_add(name_len) else {
                    break;
                };
                if name_end.saturating_add(4) > body.len() {
                    break;
                }
                let name = decode_utf16le_trimmed(&body[cur..name_end]);
                cur = name_end;
                let data_type = u16::from_le_bytes([body[cur], body[cur + 1]]);
                let val_len = usize::from(u16::from_le_bytes([body[cur + 2], body[cur + 3]]));
                cur += 4;
                let Some(val_end) = cur.checked_add(val_len) else {
                    break;
                };
                if val_end > body.len() {
                    break;
                }
                let val_bytes = &body[cur..val_end];
                let val_abs_offset = body_start + cur;
                cur = val_end;

                if name.is_empty() {
                    continue;
                }
                if name.eq_ignore_ascii_case("WM/Picture") && data_type == 1 && val_bytes.len() >= 9
                {
                    let pic_type = val_bytes[0];
                    let data_len = usize::try_from(u32::from_le_bytes([
                        val_bytes[1],
                        val_bytes[2],
                        val_bytes[3],
                        val_bytes[4],
                    ]))
                    .unwrap_or(0);
                    if let Some((mime_end, after_mime)) = find_utf16le_nul_end(val_bytes, 5) {
                        if let Some((desc_end, img_rel)) =
                            find_utf16le_nul_end(val_bytes, after_mime)
                        {
                            if data_len > 0 && img_rel.saturating_add(data_len) <= val_bytes.len() {
                                let mime_raw = decode_utf16le_trimmed(&val_bytes[5..mime_end]);
                                let mime = if mime_raw.is_empty() {
                                    "image/jpeg".to_string()
                                } else {
                                    mime_raw
                                };
                                let desc = decode_utf16le_trimmed(&val_bytes[after_mime..desc_end]);
                                let img_data = &val_bytes[img_rel..img_rel + data_len];
                                let abs_off = u64::try_from(val_abs_offset + img_rel).ok();
                                let img_val = if can_inline_artwork(
                                    img_data.len(),
                                    no_artwork,
                                    &mut total_inline_artwork_bytes,
                                ) {
                                    serde_json::json!(
                                        base64::engine::general_purpose::STANDARD.encode(img_data)
                                    )
                                } else {
                                    serde_json::json!(img_data.len())
                                };
                                let pic_obj = serde_json::json!({
                                    "mime_type": mime,
                                    "image_type": pic_type,
                                    "description": desc,
                                    "image": img_val,
                                    "offset": abs_off,
                                });
                                insert_tag_value(
                                    &mut tags_map,
                                    &mut multi_value_keys,
                                    "WM/Picture".to_string(),
                                    pic_obj,
                                );
                            }
                        }
                    }
                    continue;
                }

                let val_json = match data_type {
                    0 => {
                        let s = decode_utf16le_trimmed(val_bytes);
                        if s.is_empty() {
                            continue;
                        }
                        serde_json::json!(s)
                    }
                    1 => serde_json::json!(
                        base64::engine::general_purpose::STANDARD.encode(val_bytes)
                    ),
                    2 if val_bytes.len() >= 4 => {
                        let b = u32::from_le_bytes([
                            val_bytes[0],
                            val_bytes[1],
                            val_bytes[2],
                            val_bytes[3],
                        ]);
                        serde_json::json!(if b != 0 { 1 } else { 0 })
                    }
                    3 if val_bytes.len() >= 4 => {
                        let d = u32::from_le_bytes([
                            val_bytes[0],
                            val_bytes[1],
                            val_bytes[2],
                            val_bytes[3],
                        ]);
                        serde_json::json!(d)
                    }
                    4 if val_bytes.len() >= 8 => {
                        let q = u64::from_le_bytes([
                            val_bytes[0],
                            val_bytes[1],
                            val_bytes[2],
                            val_bytes[3],
                            val_bytes[4],
                            val_bytes[5],
                            val_bytes[6],
                            val_bytes[7],
                        ]);
                        serde_json::json!(q)
                    }
                    5 if val_bytes.len() >= 2 => {
                        let w = u16::from_le_bytes([val_bytes[0], val_bytes[1]]);
                        serde_json::json!(w)
                    }
                    _ => continue,
                };
                insert_tag_value(&mut tags_map, &mut multi_value_keys, name, val_json);
            }
        }

        pos = next_pos;
    }

    if include_info {
        let audio_size = file_size.saturating_sub(audio_offset);
        info_map.insert(
            "song_length_ms".to_string(),
            serde_json::json!(song_length_ms.unwrap_or(0)),
        );
        info_map.insert("file_size".to_string(), serde_json::json!(file_size));
        info_map.insert("audio_offset".to_string(), serde_json::json!(audio_offset));
        info_map.insert("audio_size".to_string(), serde_json::json!(audio_size));
        if let Some(br) = first_audio_bitrate {
            info_map.insert("bitrate".to_string(), serde_json::json!(br));
        }
        if let Some(sr) = first_audio_samplerate {
            info_map.insert("samplerate".to_string(), serde_json::json!(sr));
        }
        if let Some(ch) = first_audio_channels {
            info_map.insert("channels".to_string(), serde_json::json!(ch));
            info_map.insert(
                "stereo".to_string(),
                serde_json::json!(if ch == 2 { 1 } else { 0 }),
            );
        }
        if let Some(bps) = first_audio_bits {
            info_map.insert("bits_per_sample".to_string(), serde_json::json!(bps));
        }
        if let Some(mbr) = max_bitrate {
            info_map.insert("max_bitrate".to_string(), serde_json::json!(mbr));
        }
        if !streams.is_empty() {
            info_map.insert("streams".to_string(), serde_json::Value::Array(streams));
        }
        info_map.insert(
            "lossless".to_string(),
            serde_json::json!(if any_lossless { 1 } else { 0 }),
        );
    }

    Ok(ScanResult {
        info: info_map,
        tags: tags_map,
    })
}

fn extract_no_artwork_metadata(
    header_bytes: &[u8],
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    multi_value_keys: &mut HashSet<String>,
) {
    if header_bytes.starts_with(b"ID3") {
        extract_id3v2_no_artwork(header_bytes, tags_map, multi_value_keys);
    } else if header_bytes.starts_with(b"fLaC") {
        extract_flac_no_artwork(header_bytes, tags_map);
    } else if header_bytes.len() >= 8 && &header_bytes[4..8] == b"ftyp" {
        extract_mp4_no_artwork(header_bytes, tags_map);
    }
}

pub fn scan_file(
    path: &Path,
    include_info: bool,
    include_tags: bool,
    no_artwork: bool,
) -> Result<ScanResult, AudioScanError> {
    let mut file = open_regular_file(path)?;
    let file_size = file.metadata()?.len();
    let (mut header_bytes, audio_offset) = read_header_prefix(&mut file, file_size, path)?;

    if header_bytes.starts_with(&ASF_HEADER_GUID) {
        return scan_asf_header(
            &header_bytes,
            file_size,
            audio_offset,
            include_info,
            include_tags,
            no_artwork,
        );
    }

    let mut info_map = serde_json::Map::new();
    let mut tags_map = serde_json::Map::new();

    let tagged_file = read_tagged_file(
        &mut file,
        path,
        None,
        include_tags,
        include_tags && !no_artwork,
    )?;

    if include_info {
        let props = tagged_file.properties();
        let song_length_ms = u64::try_from(props.duration().as_millis()).unwrap_or(u64::MAX);
        let audio_size = file_size.saturating_sub(audio_offset);
        let mp3_hdr = if tagged_file.file_type() == FileType::Mpeg {
            let header_len_u64 = u64::try_from(header_bytes.len()).unwrap_or(u64::MAX);
            if audio_offset < header_len_u64 {
                let start = usize::try_from(audio_offset)
                    .unwrap_or(0)
                    .min(header_bytes.len());
                find_mp3_frame(&header_bytes, start)
                    .or_else(|| find_mp3_frame(&header_bytes, 0))
                    .map(|(_, hdr)| hdr)
            } else {
                let audio_win = read_window_at(&mut file, file_size, audio_offset, 16 * 1024)?;
                find_mp3_frame(&audio_win, 0).map(|(_, hdr)| hdr)
            }
        } else {
            None
        };

        info_map.insert(
            "song_length_ms".to_string(),
            serde_json::json!(song_length_ms),
        );
        info_map.insert("file_size".to_string(), serde_json::json!(file_size));
        info_map.insert("audio_offset".to_string(), serde_json::json!(audio_offset));
        info_map.insert("audio_size".to_string(), serde_json::json!(audio_size));

        let props_bitrate_bps = props
            .audio_bitrate()
            .map(|kbps| u64::from(kbps).saturating_mul(1000))
            .or_else(|| mp3_hdr.map(|h| u64::from(h.bitrate_kbps).saturating_mul(1000)));
        if let Some(bps) = props_bitrate_bps {
            info_map.insert("bitrate".to_string(), serde_json::json!(bps));
        }
        if let Some(samplerate) = props
            .sample_rate()
            .or_else(|| mp3_hdr.map(|h| h.sample_rate))
        {
            info_map.insert("samplerate".to_string(), serde_json::json!(samplerate));
        }
        if let Some(channels) = props
            .channels()
            .map(u64::from)
            .or_else(|| mp3_hdr.map(|h| u64::from(h.channels)))
        {
            info_map.insert("channels".to_string(), serde_json::json!(channels));
            info_map.insert(
                "stereo".to_string(),
                serde_json::json!(if channels == 2 { 1 } else { 0 }),
            );
        }
        if let Some(bits_per_sample) = props.bit_depth() {
            info_map.insert(
                "bits_per_sample".to_string(),
                serde_json::json!(u64::from(bits_per_sample)),
            );
        }

        if header_bytes.len() >= 5 && &header_bytes[0..3] == b"ID3" {
            let major = header_bytes[3];
            let revision = header_bytes[4];
            info_map.insert(
                "id3_version".to_string(),
                serde_json::json!(format!("ID3v2.{major}.{revision}")),
            );
        }
        if let Some(hdr) = mp3_hdr {
            info_map.insert("layer".to_string(), serde_json::json!(hdr.layer));
        }

        if tagged_file.file_type() == FileType::Mp4 || is_mp4_header(&header_bytes) {
            populate_mp4_info(
                &mut file,
                file_size,
                &header_bytes,
                audio_size,
                song_length_ms,
                props_bitrate_bps,
                &mut info_map,
            )?;
        } else if header_bytes.starts_with(b"OggS")
            || matches!(
                tagged_file.file_type(),
                FileType::Vorbis | FileType::Opus | FileType::Speex
            )
        {
            populate_ogg_info(
                &header_bytes,
                audio_size,
                song_length_ms,
                props_bitrate_bps,
                &mut info_map,
            );
        }
    }

    if include_tags {
        let has_id3v2_items = tagged_file
            .tags()
            .iter()
            .any(|t| t.tag_type() == TagType::Id3v2 && t.item_count() > 0);
        let mut multi_value_keys: HashSet<String> = HashSet::new();
        let mut extended_for_art = false;
        let mut total_inline_artwork_bytes: usize = 0;

        let (id3_comm_frames, id3_uslt_frames) = if header_bytes.starts_with(b"ID3") {
            extract_id3v2_comm_uslt(&header_bytes)
        } else {
            (Vec::new(), Vec::new())
        };
        let has_raw_comm = !id3_comm_frames.is_empty();
        let has_raw_uslt = !id3_uslt_frames.is_empty();
        for entry in id3_comm_frames {
            insert_tag_value(
                &mut tags_map,
                &mut multi_value_keys,
                "COMM".to_string(),
                entry,
            );
        }
        for entry in id3_uslt_frames {
            insert_tag_value(
                &mut tags_map,
                &mut multi_value_keys,
                "USLT".to_string(),
                entry,
            );
        }

        for tag in tagged_file.tags() {
            if tag.tag_type() == TagType::Id3v1 && has_id3v2_items {
                continue;
            }
            let target_tag_type = match tag.tag_type() {
                TagType::Id3v1 => TagType::Id3v2,
                other => other,
            };

            for item in tag.items() {
                let Some(key_name) = map_item_key(item.key(), target_tag_type) else {
                    continue;
                };
                if tag.tag_type() == TagType::Id3v2
                    && ((key_name == "COMM" && has_raw_comm)
                        || (key_name == "USLT" && has_raw_uslt))
                {
                    continue;
                }

                let val_json = match item.value() {
                    lofty::tag::ItemValue::Text(s) | lofty::tag::ItemValue::Locator(s) => {
                        let cleaned = clean_tag_string(s);
                        if cleaned.is_empty() {
                            continue;
                        }
                        serde_json::json!(cleaned)
                    }
                    lofty::tag::ItemValue::Binary(b) => {
                        serde_json::json!(base64::engine::general_purpose::STANDARD.encode(b))
                    }
                };

                insert_tag_value(&mut tags_map, &mut multi_value_keys, key_name, val_json);
            }

            if no_artwork {
                continue;
            }

            let pics = tag.pictures();
            if !pics.is_empty() && !extended_for_art {
                let needs_extend = pics.iter().any(|p| {
                    let data = p.data();
                    !data.is_empty()
                        && locate_artwork_offset(&header_bytes, audio_offset, data).is_none()
                });
                if needs_extend {
                    extend_header_bytes(&mut file, file_size, &mut header_bytes, MAX_HEADER_READ)?;
                    extended_for_art = true;
                }
            }

            if !pics.is_empty() {
                match tag.tag_type() {
                    TagType::VorbisComments => {
                        let mut pic_objects = Vec::new();
                        for pic in pics {
                            let data = pic.data();
                            if data.is_empty() {
                                continue;
                            }
                            let mime = pic
                                .mime_type()
                                .map(|m| clean_tag_string(m.as_str()))
                                .filter(|m| !m.is_empty())
                                .unwrap_or_else(|| "image/jpeg".to_string());
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = clean_tag_string(pic.description().unwrap_or(""));
                            let offset = locate_artwork_offset(&header_bytes, audio_offset, data);
                            let img_data_val = if can_inline_artwork(
                                data.len(),
                                no_artwork,
                                &mut total_inline_artwork_bytes,
                            ) {
                                serde_json::json!(
                                    base64::engine::general_purpose::STANDARD.encode(data)
                                )
                            } else {
                                serde_json::json!(data.len())
                            };
                            let obj = serde_json::json!({
                                "mime_type": mime,
                                "picture_type": type_u8,
                                "description": desc,
                                "width": 0,
                                "height": 0,
                                "depth": 24,
                                "color_index": 0,
                                "image_data": img_data_val,
                                "offset": offset,
                            });
                            pic_objects.push(obj);
                        }
                        append_allpictures(&mut tags_map, pic_objects);
                    }
                    TagType::Mp4Ilst => {
                        for pic in pics {
                            let data = pic.data();
                            if data.is_empty() {
                                continue;
                            }
                            let offset = locate_artwork_offset(&header_bytes, audio_offset, data);
                            if !tags_map.contains_key("COVR") {
                                if can_inline_artwork(
                                    data.len(),
                                    no_artwork,
                                    &mut total_inline_artwork_bytes,
                                ) {
                                    let b64 =
                                        base64::engine::general_purpose::STANDARD.encode(data);
                                    tags_map.insert("COVR".to_string(), serde_json::json!(b64));
                                } else {
                                    tags_map
                                        .insert("COVR".to_string(), serde_json::json!(data.len()));
                                }
                                tags_map
                                    .insert("COVR_offset".to_string(), serde_json::json!(offset));
                            }
                            break;
                        }
                    }
                    TagType::Ape => {
                        for pic in pics {
                            let data = pic.data();
                            if data.is_empty() {
                                continue;
                            }
                            let offset = locate_artwork_offset(&header_bytes, audio_offset, data);
                            if !tags_map.contains_key("COVER ART (FRONT)") {
                                if can_inline_artwork(
                                    data.len(),
                                    no_artwork,
                                    &mut total_inline_artwork_bytes,
                                ) {
                                    let b64 =
                                        base64::engine::general_purpose::STANDARD.encode(data);
                                    tags_map.insert(
                                        "COVER ART (FRONT)".to_string(),
                                        serde_json::json!(b64),
                                    );
                                } else {
                                    tags_map.insert(
                                        "COVER ART (FRONT)".to_string(),
                                        serde_json::json!(data.len()),
                                    );
                                }
                                tags_map.insert(
                                    "COVER ART (FRONT)_offset".to_string(),
                                    serde_json::json!(offset),
                                );
                            }
                            break;
                        }
                    }
                    _ => {
                        let mut apic_entries = Vec::new();
                        for pic in pics {
                            let data = pic.data();
                            if data.is_empty() {
                                continue;
                            }
                            let mime = pic
                                .mime_type()
                                .map(|m| clean_tag_string(m.as_str()))
                                .filter(|m| !m.is_empty())
                                .unwrap_or_else(|| "image/jpeg".to_string());
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = clean_tag_string(pic.description().unwrap_or(""));
                            let offset = locate_artwork_offset(&header_bytes, audio_offset, data);
                            let entry = if can_inline_artwork(
                                data.len(),
                                no_artwork,
                                &mut total_inline_artwork_bytes,
                            ) {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                                serde_json::json!([mime, type_u8, desc, b64])
                            } else {
                                serde_json::json!([mime, type_u8, desc, data.len(), offset])
                            };
                            apic_entries.push(entry);
                        }
                        append_apic_entries(&mut tags_map, &mut multi_value_keys, apic_entries);
                    }
                }
            }
        }

        if no_artwork {
            extract_no_artwork_metadata(&header_bytes, &mut tags_map, &mut multi_value_keys);
        }
    }

    Ok(ScanResult {
        info: info_map,
        tags: tags_map,
    })
}

fn is_supported_seek_format(ext: &str) -> bool {
    matches!(
        ext,
        "mp3"
            | "mp2"
            | "mp1"
            | "flac"
            | "flc"
            | "fla"
            | "ogg"
            | "oga"
            | "ogf"
            | "opus"
            | "wav"
            | "wave"
            | "aif"
            | "aiff"
            | "aifc"
            | "mp4"
            | "m4a"
            | "m4b"
            | "aac"
            | "ape"
            | "wv"
            | "mpc"
            | "spx"
            | "wma"
            | "asf"
    )
}

fn file_type_to_ext(ft: FileType) -> Option<&'static str> {
    match ft {
        FileType::Mpeg => Some("mp3"),
        FileType::Flac => Some("flac"),
        FileType::Vorbis => Some("ogg"),
        FileType::Opus => Some("opus"),
        FileType::Speex => Some("spx"),
        FileType::Wav => Some("wav"),
        FileType::Aiff => Some("aiff"),
        FileType::Mp4 => Some("mp4"),
        FileType::Aac => Some("aac"),
        FileType::Ape => Some("ape"),
        FileType::WavPack => Some("wv"),
        FileType::Mpc => Some("mpc"),
        FileType::Custom(name) => {
            if name.eq_ignore_ascii_case("wma") {
                Some("wma")
            } else if name.eq_ignore_ascii_case("asf") {
                Some("asf")
            } else {
                None
            }
        }
        _ => None,
    }
}

fn clamp_seek_offset(offset: u64, audio_offset: u64, file_size: u64) -> u64 {
    let min_off = audio_offset.min(file_size);
    let max_i64_u64 = u64::try_from(i64::MAX).unwrap_or(u64::MAX);
    let max_off = file_size.min(max_i64_u64);
    if min_off > max_off {
        return max_off;
    }
    offset.clamp(min_off, max_off)
}

fn estimate_byte_offset(
    target_ms: u64,
    total_duration_ms: u64,
    audio_offset: u64,
    audio_size: u64,
    file_size: u64,
) -> u64 {
    if total_duration_ms == 0 {
        return clamp_seek_offset(audio_offset, audio_offset, file_size);
    }
    let scaled = u128::from(target_ms).saturating_mul(u128::from(audio_size));
    let rel = scaled / u128::from(total_duration_ms);
    let abs = u128::from(audio_offset).saturating_add(rel);
    let min_off = u128::from(audio_offset.min(file_size));
    let max_i64 = u128::from(u64::try_from(i64::MAX).unwrap_or(u64::MAX));
    let max_off = u128::from(file_size).min(max_i64);
    let clamped = if min_off > max_off {
        max_off
    } else {
        abs.clamp(min_off, max_off)
    };
    u64::try_from(clamped).unwrap_or_else(|_| clamp_seek_offset(file_size, audio_offset, file_size))
}

fn estimate_mp3_toc_or_vbri_in_slice(
    bytes: &[u8],
    scan_start: usize,
    audio_offset: u64,
    audio_size: u64,
    target_ms: u64,
    total_duration_ms: u64,
    file_size: u64,
) -> Option<u64> {
    if total_duration_ms == 0 {
        return None;
    }
    let (frame_pos, hdr) = find_mp3_frame(bytes, scan_start)?;
    let frame_end = frame_pos.checked_add(hdr.frame_length)?.min(bytes.len());
    let frame = &bytes[frame_pos..frame_end];

    let xing_offset = frame.windows(4).position(|w| w == b"Xing" || w == b"Info");
    if let Some(xoff) = xing_offset {
        let flags_pos = xoff.checked_add(4)?;
        if frame.len().saturating_sub(flags_pos) >= 4 {
            let flags = u32::from_be_bytes([
                frame[flags_pos],
                frame[flags_pos + 1],
                frame[flags_pos + 2],
                frame[flags_pos + 3],
            ]);
            let mut cursor = flags_pos.checked_add(4)?;
            if (flags & 0x01) != 0 {
                cursor = cursor.checked_add(4)?;
            }
            let total_bytes: u64 = if (flags & 0x02) != 0 {
                if frame.len().saturating_sub(cursor) < 4 {
                    return None;
                }
                let b = u32::from_be_bytes([
                    frame[cursor],
                    frame[cursor + 1],
                    frame[cursor + 2],
                    frame[cursor + 3],
                ]);
                cursor = cursor.checked_add(4)?;
                u64::from(b).min(audio_size)
            } else {
                audio_size
            };
            if (flags & 0x04) != 0 && frame.len().saturating_sub(cursor) >= 100 {
                let toc = &frame[cursor..cursor + 100];
                let pct_fp =
                    (u128::from(target_ms).saturating_mul(100)) / u128::from(total_duration_ms);
                let pct = usize::try_from(pct_fp.min(99)).unwrap_or(99);
                let fa = u128::from(toc[pct]);
                let fb = if pct + 1 < 100 {
                    u128::from(toc[pct + 1])
                } else {
                    256u128
                };
                let frac_num = u128::from(target_ms)
                    .saturating_mul(100)
                    .saturating_sub(u128::from(total_duration_ms).saturating_mul(pct as u128));
                let interp_fp = fa
                    .saturating_mul(u128::from(total_duration_ms))
                    .saturating_add(fb.saturating_sub(fa).saturating_mul(frac_num));
                let denom = 256u128.saturating_mul(u128::from(total_duration_ms));
                let rel = interp_fp.saturating_mul(u128::from(total_bytes)) / denom.max(1);
                let abs = u128::from(audio_offset).saturating_add(rel);
                if let Ok(abs_u64) = u64::try_from(abs) {
                    return Some(clamp_seek_offset(abs_u64, audio_offset, file_size));
                }
            }
        }
    }

    if let Some(voff) = frame.windows(4).position(|w| w == b"VBRI") {
        let hdr_end = voff.checked_add(26)?;
        if frame.len() >= hdr_end {
            let num_entries = usize::from(u16::from_be_bytes([frame[voff + 18], frame[voff + 19]]));
            let scale = u64::from(u16::from_be_bytes([frame[voff + 20], frame[voff + 21]]));
            let entry_bytes = usize::from(u16::from_be_bytes([frame[voff + 22], frame[voff + 23]]));
            if scale > 0 && (1..=4).contains(&entry_bytes) && num_entries > 0 {
                let table_bytes = num_entries.checked_mul(entry_bytes)?;
                if frame.len().saturating_sub(hdr_end) >= table_bytes {
                    let target_idx_u128 = (u128::from(target_ms)
                        .saturating_mul(num_entries as u128))
                        / u128::from(total_duration_ms);
                    let target_idx = usize::try_from(target_idx_u128)
                        .unwrap_or(num_entries)
                        .min(num_entries);
                    let mut acc: u64 = 0;
                    for i in 0..target_idx {
                        let start = hdr_end + i * entry_bytes;
                        let mut val: u64 = 0;
                        for j in 0..entry_bytes {
                            val = (val << 8) | u64::from(frame[start + j]);
                        }
                        acc = acc.saturating_add(val.saturating_mul(scale));
                    }
                    let abs = audio_offset.saturating_add(acc);
                    return Some(clamp_seek_offset(abs, audio_offset, file_size));
                }
            }
        }
    }

    None
}

fn estimate_mp3_toc_or_vbri(
    file: &mut std::fs::File,
    header_bytes: &[u8],
    audio_offset: u64,
    audio_size: u64,
    target_ms: u64,
    total_duration_ms: u64,
    file_size: u64,
) -> Result<Option<u64>, AudioScanError> {
    if total_duration_ms == 0 {
        return Ok(None);
    }
    let tag_start = id3v2_tag_end(header_bytes).unwrap_or(0);
    let header_len_u64 = u64::try_from(header_bytes.len()).unwrap_or(u64::MAX);
    if tag_start.saturating_add(8192) <= header_len_u64 || header_len_u64 >= file_size {
        let scan_start = usize::try_from(tag_start)
            .unwrap_or(header_bytes.len())
            .min(header_bytes.len());
        return Ok(estimate_mp3_toc_or_vbri_in_slice(
            header_bytes,
            scan_start,
            audio_offset,
            audio_size,
            target_ms,
            total_duration_ms,
            file_size,
        ));
    }
    let post_tag = read_window_at(file, file_size, tag_start, 16 * 1024)?;
    Ok(estimate_mp3_toc_or_vbri_in_slice(
        &post_tag,
        0,
        audio_offset,
        audio_size,
        target_ms,
        total_duration_ms,
        file_size,
    ))
}

fn estimate_flac_seektable(
    header_bytes: &[u8],
    audio_offset: u64,
    target_ms: u64,
    sample_rate: u32,
    file_size: u64,
) -> Option<u64> {
    if header_bytes.len() < 4 || &header_bytes[0..4] != b"fLaC" || sample_rate == 0 {
        return None;
    }
    let target_sample_u128 =
        (u128::from(target_ms).saturating_mul(u128::from(sample_rate))) / 1000u128;
    let target_sample = u64::try_from(target_sample_u128).unwrap_or(u64::MAX);

    let mut pos: usize = 4;
    while header_bytes.len().saturating_sub(pos) >= 4 {
        let header_byte = header_bytes[pos];
        let is_last = (header_byte & 0x80) != 0;
        let block_type = header_byte & 0x7F;
        let block_len = (usize::from(header_bytes[pos + 1]) << 16)
            | (usize::from(header_bytes[pos + 2]) << 8)
            | usize::from(header_bytes[pos + 3]);
        let data_start = pos.checked_add(4)?;
        let next_pos = data_start.checked_add(block_len)?;
        if next_pos > header_bytes.len() {
            break;
        }
        if block_type == 3 {
            let num_points = block_len / 18;
            let mut best_offset: Option<u64> = None;
            for i in 0..num_points {
                let p = data_start + i * 18;
                let sample_num = u64::from_be_bytes([
                    header_bytes[p],
                    header_bytes[p + 1],
                    header_bytes[p + 2],
                    header_bytes[p + 3],
                    header_bytes[p + 4],
                    header_bytes[p + 5],
                    header_bytes[p + 6],
                    header_bytes[p + 7],
                ]);
                if sample_num == u64::MAX {
                    continue;
                }
                if sample_num <= target_sample {
                    let stream_off = u64::from_be_bytes([
                        header_bytes[p + 8],
                        header_bytes[p + 9],
                        header_bytes[p + 10],
                        header_bytes[p + 11],
                        header_bytes[p + 12],
                        header_bytes[p + 13],
                        header_bytes[p + 14],
                        header_bytes[p + 15],
                    ]);
                    best_offset = Some(stream_off);
                } else {
                    break;
                }
            }
            if let Some(off) = best_offset {
                let abs = audio_offset.saturating_add(off);
                return Some(clamp_seek_offset(abs, audio_offset, file_size));
            }
        }
        if is_last {
            break;
        }
        pos = next_pos;
    }
    None
}

fn read_ogg_page_granule(window: &[u8], page_start: usize) -> Option<u64> {
    if window.len().saturating_sub(page_start) < 27 {
        return None;
    }
    if &window[page_start..page_start + 4] != b"OggS" {
        return None;
    }
    let granule = u64::from_le_bytes([
        window[page_start + 6],
        window[page_start + 7],
        window[page_start + 8],
        window[page_start + 9],
        window[page_start + 10],
        window[page_start + 11],
        window[page_start + 12],
        window[page_start + 13],
    ]);
    Some(granule)
}

fn find_ogg_page_for_ms(
    file: &mut std::fs::File,
    file_size: u64,
    audio_offset: u64,
    est_byte: u64,
    target_ms: u64,
    granule_rate: u32,
) -> Result<Option<u64>, AudioScanError> {
    let mut probe_byte = est_byte;
    if granule_rate > 0 && file_size > u64::try_from(SEEK_WINDOW_BYTES).unwrap_or(u64::MAX) {
        let target_granule = u64::try_from(
            (u128::from(target_ms).saturating_mul(u128::from(granule_rate))) / 1000u128,
        )
        .unwrap_or(u64::MAX);
        let mut lo = audio_offset;
        let mut hi = file_size;
        for _ in 0..8 {
            if hi.saturating_sub(lo) <= SEEK_HALF_WINDOW {
                break;
            }
            let mid = lo + (hi - lo) / 2;
            let win = read_window_at(file, file_size, mid, 64 * 1024)?;
            let mut found_granule: Option<u64> = None;
            let mut search = 0;
            while let Some(pos) = find_ogg_page(&win, search) {
                if let Some(g) = read_ogg_page_granule(&win, pos) {
                    if g != u64::MAX && g > 0 {
                        found_granule = Some(g);
                        probe_byte = mid.saturating_add(u64::try_from(pos).unwrap_or(0));
                        break;
                    }
                }
                search = match pos.checked_add(4) {
                    Some(next) => next,
                    None => break,
                };
            }
            match found_granule {
                Some(g) if g < target_granule => lo = mid,
                Some(_) => hi = mid,
                None => break,
            }
        }
    }

    let window_start = probe_byte
        .saturating_sub(SEEK_HALF_WINDOW)
        .max(audio_offset);
    let window_end = probe_byte.saturating_add(SEEK_HALF_WINDOW).min(file_size);
    let win_len = usize::try_from(window_end.saturating_sub(window_start))
        .unwrap_or(SEEK_WINDOW_BYTES)
        .min(SEEK_WINDOW_BYTES);
    let window = read_window_at(file, file_size, window_start, win_len)?;
    let rel_est = usize::try_from(probe_byte.saturating_sub(window_start))
        .unwrap_or(0)
        .min(window.len());
    if let Some(rel) = find_ogg_page(&window, rel_est) {
        let abs = window_start.saturating_add(u64::try_from(rel).unwrap_or(0));
        return Ok(Some(clamp_seek_offset(abs, audio_offset, file_size)));
    }
    if let Some(rel) = find_ogg_page(&window, 0) {
        let abs = window_start.saturating_add(u64::try_from(rel).unwrap_or(0));
        return Ok(Some(clamp_seek_offset(abs, audio_offset, file_size)));
    }
    Ok(None)
}

fn find_last_mp3_frame(bytes: &[u8], limit: usize) -> Option<(usize, Mp3FrameHeader)> {
    let end = limit.min(bytes.len());
    let slice = &bytes[..end];
    let mut pos = 0;
    let mut last: Option<(usize, Mp3FrameHeader)> = None;
    while let Some((found, hdr)) = find_mp3_frame(slice, pos) {
        last = Some((found, hdr));
        pos = match found.checked_add(hdr.frame_length.max(1)) {
            Some(next) => next,
            None => break,
        };
    }
    last
}

fn find_last_flac_frame(bytes: &[u8], limit: usize) -> Option<usize> {
    let end = limit.min(bytes.len());
    let slice = &bytes[..end];
    let mut pos = 0;
    let mut last: Option<usize> = None;
    while let Some(found) = find_flac_frame(slice, pos) {
        last = Some(found);
        pos = match found.checked_add(2) {
            Some(next) => next,
            None => break,
        };
    }
    last
}

pub fn find_frame_offset(
    path: &Path,
    format_hint: Option<&str>,
    target_ms_or_neg_byte_offset: i64,
) -> Result<FrameSeekResult, AudioScanError> {
    let mut file = open_regular_file(path)?;
    let file_size = file.metadata()?.len();

    let hinted_ext = format_hint.map(|s| s.to_ascii_lowercase()).or_else(|| {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
    });
    if let Some(ref ext) = hinted_ext {
        if !is_supported_seek_format(ext) {
            return Err(AudioScanError::UnsupportedFormat(format!(
                "Unsupported seek format: {ext}"
            )));
        }
    }

    let tagged_file = read_tagged_file(&mut file, path, hinted_ext.as_deref(), false, false)?;
    let ext_str = match hinted_ext {
        Some(ext) => ext,
        None => file_type_to_ext(tagged_file.file_type())
            .ok_or_else(|| {
                AudioScanError::UnsupportedFormat("Unrecognized audio format".to_string())
            })?
            .to_string(),
    };
    let ext = ext_str.as_str();
    if !is_supported_seek_format(ext) {
        return Err(AudioScanError::UnsupportedFormat(format!(
            "Unsupported seek format: {ext}"
        )));
    }

    let props = tagged_file.properties();
    let sample_rate = props.sample_rate();
    let channels = props.channels().map(u32::from);
    let total_duration_ms = u64::try_from(props.duration().as_millis()).unwrap_or(u64::MAX);

    if target_ms_or_neg_byte_offset < 0 {
        let target_byte = target_ms_or_neg_byte_offset.unsigned_abs();
        if target_byte >= file_size {
            return Ok(FrameSeekResult {
                byte_offset: -1,
                seek_header: None,
                samplerate: sample_rate,
                channels,
            });
        }
        let window = read_window_at(&mut file, file_size, target_byte, SEEK_WINDOW_BYTES)?;
        match ext {
            "mp3" | "mp2" | "mp1" => {
                if let Some((rel, hdr)) = find_mp3_frame(&window, 0) {
                    let abs = target_byte.saturating_add(u64::try_from(rel).unwrap_or(0));
                    return Ok(FrameSeekResult {
                        byte_offset: clamp_u64_to_i64(abs.min(file_size)),
                        seek_header: None,
                        samplerate: Some(hdr.sample_rate),
                        channels: Some(u32::from(hdr.channels)),
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: -1,
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            "flac" | "flc" | "fla" => {
                if let Some(rel) = find_flac_frame(&window, 0) {
                    let abs = target_byte.saturating_add(u64::try_from(rel).unwrap_or(0));
                    return Ok(FrameSeekResult {
                        byte_offset: clamp_u64_to_i64(abs.min(file_size)),
                        seek_header: None,
                        samplerate: sample_rate,
                        channels,
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: -1,
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            "ogg" | "oga" | "ogf" | "opus" => {
                if let Some(rel) = find_ogg_page(&window, 0) {
                    let abs = target_byte.saturating_add(u64::try_from(rel).unwrap_or(0));
                    return Ok(FrameSeekResult {
                        byte_offset: clamp_u64_to_i64(abs.min(file_size)),
                        seek_header: None,
                        samplerate: sample_rate,
                        channels,
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: -1,
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            _ => Ok(FrameSeekResult {
                byte_offset: clamp_u64_to_i64(target_byte),
                seek_header: None,
                samplerate: sample_rate,
                channels,
            }),
        }
    } else {
        let target_ms = u64::try_from(target_ms_or_neg_byte_offset).unwrap_or(0);
        let (header_bytes, audio_offset) = read_header_prefix(&mut file, file_size, path)?;
        if target_ms == 0 {
            return Ok(FrameSeekResult {
                byte_offset: clamp_u64_to_i64(audio_offset),
                seek_header: None,
                samplerate: sample_rate,
                channels,
            });
        }

        let audio_size = file_size.saturating_sub(audio_offset);
        let linear_est = estimate_byte_offset(
            target_ms,
            total_duration_ms,
            audio_offset,
            audio_size,
            file_size,
        );

        match ext {
            "mp3" | "mp2" | "mp1" => {
                let est_byte = estimate_mp3_toc_or_vbri(
                    &mut file,
                    &header_bytes,
                    audio_offset,
                    audio_size,
                    target_ms,
                    total_duration_ms,
                    file_size,
                )?
                .unwrap_or(linear_est);
                let window_start = est_byte.saturating_sub(SEEK_HALF_WINDOW).max(audio_offset);
                let window_end = est_byte.saturating_add(SEEK_HALF_WINDOW).min(file_size);
                let win_len = usize::try_from(window_end.saturating_sub(window_start))
                    .unwrap_or(SEEK_WINDOW_BYTES)
                    .min(SEEK_WINDOW_BYTES);
                let window = read_window_at(&mut file, file_size, window_start, win_len)?;
                let rel_est = usize::try_from(est_byte.saturating_sub(window_start))
                    .unwrap_or(0)
                    .min(window.len());
                if let Some((rel, hdr)) = find_mp3_frame(&window, rel_est)
                    .or_else(|| find_last_mp3_frame(&window, rel_est))
                {
                    let abs = window_start.saturating_add(u64::try_from(rel).unwrap_or(0));
                    return Ok(FrameSeekResult {
                        byte_offset: clamp_u64_to_i64(clamp_seek_offset(
                            abs,
                            audio_offset,
                            file_size,
                        )),
                        seek_header: None,
                        samplerate: Some(hdr.sample_rate),
                        channels: Some(u32::from(hdr.channels)),
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: clamp_u64_to_i64(audio_offset),
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            "flac" | "flc" | "fla" => {
                let est_byte = sample_rate
                    .and_then(|sr| {
                        estimate_flac_seektable(
                            &header_bytes,
                            audio_offset,
                            target_ms,
                            sr,
                            file_size,
                        )
                    })
                    .unwrap_or(linear_est);
                let window_start = est_byte.saturating_sub(SEEK_HALF_WINDOW).max(audio_offset);
                let window_end = est_byte.saturating_add(SEEK_HALF_WINDOW).min(file_size);
                let win_len = usize::try_from(window_end.saturating_sub(window_start))
                    .unwrap_or(SEEK_WINDOW_BYTES)
                    .min(SEEK_WINDOW_BYTES);
                let window = read_window_at(&mut file, file_size, window_start, win_len)?;
                let rel_est = usize::try_from(est_byte.saturating_sub(window_start))
                    .unwrap_or(0)
                    .min(window.len());
                if let Some(rel) = find_flac_frame(&window, rel_est)
                    .or_else(|| find_last_flac_frame(&window, rel_est))
                {
                    let abs = window_start.saturating_add(u64::try_from(rel).unwrap_or(0));
                    return Ok(FrameSeekResult {
                        byte_offset: clamp_u64_to_i64(clamp_seek_offset(
                            abs,
                            audio_offset,
                            file_size,
                        )),
                        seek_header: None,
                        samplerate: sample_rate,
                        channels,
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: clamp_u64_to_i64(audio_offset),
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            "ogg" | "oga" | "ogf" | "opus" => {
                let granule_rate =
                    if ext == "opus" || header_bytes.windows(8).any(|w| w == b"OpusHead") {
                        48000
                    } else {
                        sample_rate.unwrap_or(0)
                    };
                if let Some(pos) = find_ogg_page_for_ms(
                    &mut file,
                    file_size,
                    audio_offset,
                    linear_est,
                    target_ms,
                    granule_rate,
                )? {
                    return Ok(FrameSeekResult {
                        byte_offset: clamp_u64_to_i64(pos),
                        seek_header: None,
                        samplerate: sample_rate,
                        channels,
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: clamp_u64_to_i64(audio_offset),
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            _ => Ok(FrameSeekResult {
                byte_offset: clamp_u64_to_i64(linear_est),
                seek_header: None,
                samplerate: sample_rate,
                channels,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_id3v2_mp3_with_apic(
        path: &Path,
        title: &str,
        artist: &str,
        jpeg: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        use lofty::config::WriteOptions;
        use lofty::id3::v2::Id3v2Tag;
        use lofty::picture::MimeType;
        use lofty::picture::Picture;
        use lofty::picture::PictureType;
        use lofty::tag::Accessor;
        use lofty::tag::TagExt;

        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        std::fs::write(path, frame.repeat(20))?;

        let mut tag = Id3v2Tag::default();
        tag.set_title(title.to_string());
        tag.set_artist(artist.to_string());
        let pic = Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Jpeg),
            Some("cover".to_string()),
            jpeg.to_vec(),
        );
        tag.insert_picture(pic);
        tag.save_to_path(path, WriteOptions::default())?;
        Ok(())
    }

    fn build_minimal_wav(pcm_samples: &[i16], sample_rate: u32) -> Vec<u8> {
        let data_bytes_len = pcm_samples.len() * 2;
        let riff_size = u32::try_from(36 + data_bytes_len).expect("wav size");
        let byte_rate = sample_rate * 2;
        let mut out = Vec::with_capacity(44 + data_bytes_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(u32::try_from(data_bytes_len).expect("data len")).to_le_bytes());
        for &s in pcm_samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn build_minimal_flac(total_samples: u64, sample_rate: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        // STREAMINFO block (type 0, last=0, len=34)
        out.push(0x00);
        out.extend_from_slice(&34u32.to_be_bytes()[1..4]);
        let mut streaminfo = [0u8; 34];
        streaminfo[0..2].copy_from_slice(&4096u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&4096u16.to_be_bytes());
        // Pack sample_rate (20b), channels-1 (3b), bps-1 (5b), total_samples.
        let packed: u64 = (u64::from(sample_rate) << 44)
            | (1u64 << 41)
            | (15u64 << 36)
            | (total_samples & 0x000F_FFFF_FFFF);
        streaminfo[10..18].copy_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&streaminfo);
        // Trailing PADDING block (type 1, last=1, len=4)
        out.extend_from_slice(&[0x81, 0x00, 0x00, 0x04, 0, 0, 0, 0]);
        // Minimal FLAC audio frame sync header + padding for lofty properties.
        out.extend_from_slice(&[0xFF, 0xF8, 0x69, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0u8; 128]);
        out
    }

    #[test]
    fn test_audioscan_no_artwork_env_toggle() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mp3_path = dir.path().join("tagged.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        write_id3v2_mp3_with_apic(&mp3_path, "Test Title", "Test Artist", &fake_jpeg)?;

        let res_no_art = scan_file(&mp3_path, true, true, true)?;
        assert_eq!(
            res_no_art
                .tags
                .get("TIT2")
                .and_then(|v| v.as_str())
                .expect("TIT2"),
            "Test Title"
        );
        let apic = res_no_art
            .tags
            .get("APIC")
            .and_then(|v| v.as_array())
            .expect("APIC array");
        assert_eq!(apic[3].as_u64().expect("apic len"), 10);
        assert!(apic[4].as_u64().expect("apic offset") > 0);

        let res_with_art = scan_file(&mp3_path, true, true, false)?;
        let apic_art = res_with_art
            .tags
            .get("APIC")
            .and_then(|v| v.as_array())
            .expect("APIC array with art");
        assert!(apic_art[3].is_string());
        Ok(())
    }

    #[test]
    fn test_audioscan_info_extraction() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mp3_path = dir.path().join("tagged_info.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        write_id3v2_mp3_with_apic(&mp3_path, "Info Test", "Artist Test", &fake_jpeg)?;

        let res = scan_file(&mp3_path, true, false, true)?;
        assert!(res.tags.is_empty());
        assert_eq!(
            res.info.get("samplerate").and_then(|v| v.as_u64()),
            Some(44100)
        );
        assert_eq!(res.info.get("channels").and_then(|v| v.as_u64()), Some(2));
        assert!(
            res.info
                .get("song_length_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
        );
        assert!(
            res.info
                .get("audio_offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
        );
        assert!(
            res.info
                .get("audio_size")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
        );
        Ok(())
    }

    #[test]
    fn test_find_frame_offset_mp3() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mp3_path = dir.path().join("seek.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        write_id3v2_mp3_with_apic(&mp3_path, "Seek Test", "Seek Artist", &fake_jpeg)?;

        let scan = scan_file(&mp3_path, true, false, true)?;
        let audio_offset = scan
            .info
            .get("audio_offset")
            .and_then(|v| v.as_i64())
            .expect("audio_offset");

        let seek_0 = find_frame_offset(&mp3_path, Some("mp3"), 0)?;
        assert_eq!(seek_0.byte_offset, audio_offset);
        assert_eq!(seek_0.samplerate, Some(44100));

        let seek_neg = find_frame_offset(&mp3_path, Some("mp3"), -(audio_offset + 1))?;
        assert_eq!(seek_neg.byte_offset, audio_offset + 417);

        let seek_min = find_frame_offset(&mp3_path, Some("mp3"), i64::MIN)?;
        assert_eq!(seek_min.byte_offset, -1);
        Ok(())
    }

    #[test]
    fn test_mpeg_layer_i_and_mpeg2_frame_lengths() {
        // MPEG-1 Layer I, 128 kbps (idx=4), 44100 Hz (idx=0), no pad:
        // ((12 * 128000 / 44100) + 0) * 4 = 34 * 4 = 136 bytes.
        let hdr_m1_l1 = parse_mp3_header(&[0xFF, 0xFE, 0x40, 0x00]).expect("MPEG1 Layer I");
        assert_eq!(hdr_m1_l1.version, 1);
        assert_eq!(hdr_m1_l1.layer, 1);
        assert_eq!(hdr_m1_l1.bitrate_kbps, 128);
        assert_eq!(hdr_m1_l1.sample_rate, 44100);
        assert_eq!(hdr_m1_l1.frame_length, 136);

        // MPEG-2 Layer II, 64 kbps (idx=8), 22050 Hz (idx=0), no pad:
        // (144 * 64000 / 22050) + 0 = 417 bytes.
        let hdr_m2_l2 = parse_mp3_header(&[0xFF, 0xF4, 0x80, 0x00]).expect("MPEG2 Layer II");
        assert_eq!(hdr_m2_l2.version, 2);
        assert_eq!(hdr_m2_l2.layer, 2);
        assert_eq!(hdr_m2_l2.bitrate_kbps, 64);
        assert_eq!(hdr_m2_l2.sample_rate, 22050);
        assert_eq!(hdr_m2_l2.frame_length, 417);

        // MPEG-2 Layer III, 64 kbps (idx=8), 22050 Hz (idx=0), no pad:
        // (72 * 64000 / 22050) + 0 = 208 bytes.
        let hdr_m2_l3 = parse_mp3_header(&[0xFF, 0xF2, 0x80, 0x00]).expect("MPEG2 Layer III");
        assert_eq!(hdr_m2_l3.version, 2);
        assert_eq!(hdr_m2_l3.layer, 3);
        assert_eq!(hdr_m2_l3.bitrate_kbps, 64);
        assert_eq!(hdr_m2_l3.sample_rate, 22050);
        assert_eq!(hdr_m2_l3.frame_length, 208);
    }

    #[test]
    fn test_malformed_riff_and_aiff_u32_max_chunks_no_hang() {
        let mut riff = Vec::new();
        riff.extend_from_slice(b"RIFF");
        riff.extend_from_slice(&100u32.to_le_bytes());
        riff.extend_from_slice(b"WAVE");
        riff.extend_from_slice(b"JUNK");
        riff.extend_from_slice(&u32::MAX.to_le_bytes());
        riff.extend_from_slice(&[0u8; 16]);
        let off_riff = compute_audio_offset(&riff, Path::new("bad.wav"));
        assert_eq!(off_riff, 0);

        let mut aiff = Vec::new();
        aiff.extend_from_slice(b"FORM");
        aiff.extend_from_slice(&100u32.to_be_bytes());
        aiff.extend_from_slice(b"AIFF");
        aiff.extend_from_slice(b"JUNK");
        aiff.extend_from_slice(&u32::MAX.to_be_bytes());
        aiff.extend_from_slice(&[0u8; 16]);
        let off_aiff = compute_audio_offset(&aiff, Path::new("bad.aiff"));
        assert_eq!(off_aiff, 0);
    }

    #[test]
    fn test_large_sparse_mp3_seek_beyond_16mib() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sparse_20m.mp3");
        let total_len: u64 = 20 * 1024 * 1024;
        let frame = [0xFFu8, 0xFB, 0x90, 0x00];
        let mut f = std::fs::File::create(&path)?;
        f.set_len(total_len)?;
        f.seek(SeekFrom::Start(0))?;
        use std::io::Write;
        f.write_all(&frame)?;
        f.seek(SeekFrom::Start(417))?;
        f.write_all(&frame)?;
        let far_pos: u64 = 18 * 1024 * 1024;
        f.seek(SeekFrom::Start(far_pos))?;
        f.write_all(&frame)?;
        f.seek(SeekFrom::Start(far_pos + 417))?;
        f.write_all(&frame)?;
        f.sync_all()?;
        drop(f);

        let scan = scan_file(&path, true, false, true)?;
        let dur_ms = scan
            .info
            .get("song_length_ms")
            .and_then(|v| v.as_u64())
            .expect("song_length_ms");
        assert!(dur_ms > 0);

        let target_ms = i64::try_from((u128::from(dur_ms) * 90) / 100)?;
        let seek_res = find_frame_offset(&path, Some("mp3"), target_ms)?;
        assert!(
            seek_res.byte_offset >= 16 * 1024 * 1024,
            "expected seek offset >= 16 MiB, got {}",
            seek_res.byte_offset
        );
        assert_eq!(seek_res.byte_offset, i64::try_from(far_pos)?);

        let neg_seek = find_frame_offset(&path, Some("mp3"), -i64::try_from(far_pos - 10)?)?;
        assert_eq!(neg_seek.byte_offset, i64::try_from(far_pos)?);
        Ok(())
    }

    #[test]
    fn test_wav_and_flac_metadata_scan_parity() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;

        let wav_path = dir.path().join("tone.wav");
        let pcm = vec![0i16; 4410];
        std::fs::write(&wav_path, build_minimal_wav(&pcm, 44100))?;
        let wav_res = scan_file(&wav_path, true, true, true)?;
        assert_eq!(
            wav_res.info.get("samplerate").and_then(|v| v.as_u64()),
            Some(44100)
        );
        assert_eq!(
            wav_res.info.get("channels").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            wav_res.info.get("audio_offset").and_then(|v| v.as_u64()),
            Some(44)
        );

        let flac_path = dir.path().join("tagged.flac");
        std::fs::write(&flac_path, build_minimal_flac(88200, 44100))?;
        {
            use lofty::config::WriteOptions;
            use lofty::picture::MimeType;
            use lofty::picture::Picture;
            use lofty::picture::PictureType;
            use lofty::tag::Accessor;
            use lofty::tag::Tag;
            use lofty::tag::TagExt;

            let mut tag = Tag::new(TagType::VorbisComments);
            tag.set_title("FLAC Title".to_string());
            let pic = Picture::new_unchecked(
                PictureType::CoverFront,
                Some(MimeType::Jpeg),
                Some("front".to_string()),
                vec![0xFF, 0xD8, 0xFF, 0xE0, 9, 8, 7, 6],
            );
            tag.push_picture(pic);
            tag.save_to_path(&flac_path, WriteOptions::default())?;
        }

        let flac_res = scan_file(&flac_path, true, true, true)?;
        assert_eq!(
            flac_res.tags.get("TITLE").and_then(|v| v.as_str()),
            Some("FLAC Title"),
            "unexpected flac tags: {:?}",
            flac_res.tags
        );
        let pics = flac_res
            .tags
            .get("ALLPICTURES")
            .and_then(|v| v.as_array())
            .expect("ALLPICTURES");
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].get("image_data").and_then(|v| v.as_u64()), Some(8));
        assert!(pics[0].get("offset").and_then(|v| v.as_u64()).unwrap_or(0) > 0);

        let seek_flac = find_frame_offset(&flac_path, Some("flac"), 0)?;
        assert_eq!(
            seek_flac.byte_offset,
            flac_res
                .info
                .get("audio_offset")
                .and_then(|v| v.as_i64())
                .expect("flac audio_offset")
        );
        Ok(())
    }

    #[test]
    fn test_corrupt_empty_and_unsupported_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let empty_path = dir.path().join("empty.mp3");
        std::fs::write(&empty_path, b"")?;
        assert!(scan_file(&empty_path, true, true, true).is_err());

        let corrupt_path = dir.path().join("corrupt.flac");
        std::fs::write(&corrupt_path, b"not a valid audio file")?;
        assert!(scan_file(&corrupt_path, true, true, true).is_err());

        let unknown_seek = find_frame_offset(&empty_path, Some("xyz"), 0);
        assert!(matches!(
            unknown_seek,
            Err(AudioScanError::UnsupportedFormat(_))
        ));
        Ok(())
    }

    #[test]
    fn test_two_way_search_and_boundary_overlap() {
        // Adversarial repetitive input (would be quadratic in naive scan).
        let mut haystack = vec![b'A'; 100_000];
        let needle_pos = 95_000;
        let mut needle = vec![b'A'; 5_000];
        needle[4_999] = b'B';
        haystack[needle_pos + 4_999] = b'B';
        assert_eq!(find_slice_offset(&haystack, &needle), Some(95_000));
        assert_eq!(find_slice_offset(&haystack, b"ABAAB"), None);

        // Boundary-straddling artwork must be located via overlap tail.
        let mut hdr = vec![0u8; 64];
        let art = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x11, 0x22, 0x33, 0x44];
        hdr[28..36].copy_from_slice(&art);
        assert_eq!(locate_artwork_offset(&hdr, 32, &art), Some(28));
    }

    #[test]
    fn test_container_boundaries_and_consecutive_mp3_sync() {
        // Non-MP3 containers with embedded 0xFF 0xFB must not fall through.
        let mut ogg = b"OggS\x00\x02".to_vec();
        ogg.extend_from_slice(&[0u8; 20]);
        ogg.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        ogg.extend_from_slice(&[0u8; 413]);
        ogg.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(compute_audio_offset(&ogg, Path::new("stream.ogg")), 0);

        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&24u32.to_be_bytes());
        mp4.extend_from_slice(b"ftyp");
        mp4.extend_from_slice(&[0u8; 16]);
        mp4.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        mp4.extend_from_slice(&[0u8; 413]);
        mp4.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(compute_audio_offset(&mp4, Path::new("track.m4a")), 0);

        // Single orphan MP3 header without consecutive frame must not lock sync.
        let mut orphan = vec![0u8; 32];
        orphan[8..12].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert!(find_mp3_frame(&orphan, 0).is_none());
    }

    #[test]
    fn test_tag_sanitization_and_id3_dedup() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(clean_tag_string("Rock\0\0\x03\n"), "Rock");
        assert_eq!(clean_tag_string("\0\x01"), "");

        // File with both ID3v2 and ID3v1 tags must not duplicate ID3v2 fields.
        let dir = tempfile::tempdir()?;
        let mp3_path = dir.path().join("dual_id3.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        write_id3v2_mp3_with_apic(&mp3_path, "Full ID3v2 Title", "Full Artist", &fake_jpeg)?;
        {
            use lofty::config::WriteOptions;
            use lofty::id3::v1::Id3v1Tag;
            use lofty::tag::Accessor;
            use lofty::tag::TagExt;

            let mut v1 = Id3v1Tag::default();
            v1.set_title("Truncated v1 Title\0\0".to_string());
            v1.set_artist("v1 Artist".to_string());
            v1.save_to_path(&mp3_path, WriteOptions::default())?;
        }

        let res = scan_file(&mp3_path, true, true, true)?;
        assert_eq!(
            res.tags.get("TIT2").and_then(|v| v.as_str()),
            Some("Full ID3v2 Title")
        );
        // Lossy MP3 must not fabricate bits_per_sample=16.
        assert!(res.info.get("bits_per_sample").is_none());
        Ok(())
    }

    #[test]
    fn test_flac_metadata_incomplete_checks_bounds_before_is_last() {
        // Last block header present, but payload truncated past buffer end.
        let mut truncated = Vec::new();
        truncated.extend_from_slice(b"fLaC");
        truncated.push(0x86); // is_last = 1, PICTURE block
        truncated.extend_from_slice(&1000u32.to_be_bytes()[1..4]);
        truncated.extend_from_slice(&[0u8; 32]);
        assert!(flac_metadata_incomplete(&truncated));

        // Complete last block fits inside buffer.
        let mut complete = Vec::new();
        complete.extend_from_slice(b"fLaC");
        complete.push(0x81); // is_last = 1, PADDING block
        complete.extend_from_slice(&4u32.to_be_bytes()[1..4]);
        complete.extend_from_slice(&[0u8; 4]);
        assert!(!flac_metadata_incomplete(&complete));
    }

    fn build_mp4_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let len = u32::try_from(8 + payload.len()).expect("mp4 box size");
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn test_read_header_prefix_extends_mp4_riff_aiff_and_ssnd_past_256k(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;

        // RIFF/WAVE with 300 KiB JUNK chunk before data.
        let wav_path = dir.path().join("junk_300k.wav");
        let junk_len = 300 * 1024usize;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&u32::try_from(4 + 8 + junk_len + 8 + 4)?.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"JUNK");
        wav.extend_from_slice(&u32::try_from(junk_len)?.to_le_bytes());
        wav.resize(wav.len() + junk_len, 0);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[1, 2, 3, 4]);
        std::fs::write(&wav_path, &wav)?;
        let mut f = std::fs::File::open(&wav_path)?;
        let (hdr_wav, off_wav) = read_header_prefix(&mut f, u64::try_from(wav.len())?, &wav_path)?;
        assert_eq!(off_wav, u64::try_from(12 + 8 + junk_len + 8)?);
        assert!(hdr_wav.len() > INITIAL_HEADER_READ);

        // AIFF with 300 KiB APPL chunk before SSND (with ssnd_offset=16).
        let aiff_path = dir.path().join("appl_300k.aiff");
        let appl_len = 300 * 1024usize;
        let mut aiff = Vec::new();
        aiff.extend_from_slice(b"FORM");
        aiff.extend_from_slice(&u32::try_from(4 + 8 + appl_len + 8 + 24)?.to_be_bytes());
        aiff.extend_from_slice(b"AIFF");
        aiff.extend_from_slice(b"APPL");
        aiff.extend_from_slice(&u32::try_from(appl_len)?.to_be_bytes());
        aiff.resize(aiff.len() + appl_len, 0);
        let ssnd_pos = aiff.len();
        aiff.extend_from_slice(b"SSND");
        aiff.extend_from_slice(&24u32.to_be_bytes());
        aiff.extend_from_slice(&16u32.to_be_bytes()); // offset = 16
        aiff.extend_from_slice(&0u32.to_be_bytes()); // blockSize = 0
        aiff.extend_from_slice(&[0u8; 16]);
        std::fs::write(&aiff_path, &aiff)?;
        let mut f_aiff = std::fs::File::open(&aiff_path)?;
        let (_, off_aiff) =
            read_header_prefix(&mut f_aiff, u64::try_from(aiff.len())?, &aiff_path)?;
        assert_eq!(off_aiff, u64::try_from(ssnd_pos + 16 + 16)?);

        // Sparse AIFF with 17 MiB APPL chunk (SSND past MAX_HEADER_READ).
        let sparse_aiff = dir.path().join("sparse_17m.aiff");
        let big_appl: u32 = 17 * 1024 * 1024;
        let ssnd_far: u64 = 12 + 8 + u64::from(big_appl);
        let total_aiff: u64 = ssnd_far + 8 + 16;
        {
            use std::io::Write;
            let mut sf = std::fs::File::create(&sparse_aiff)?;
            sf.set_len(total_aiff)?;
            sf.seek(SeekFrom::Start(0))?;
            sf.write_all(b"FORM")?;
            sf.write_all(&u32::try_from(total_aiff - 8)?.to_be_bytes())?;
            sf.write_all(b"AIFF")?;
            sf.write_all(b"APPL")?;
            sf.write_all(&big_appl.to_be_bytes())?;
            sf.seek(SeekFrom::Start(ssnd_far))?;
            sf.write_all(b"SSND")?;
            sf.write_all(&16u32.to_be_bytes())?;
            sf.write_all(&8u32.to_be_bytes())?; // offset = 8
            sf.write_all(&0u32.to_be_bytes())?;
            sf.sync_all()?;
        }
        let mut f_sparse = std::fs::File::open(&sparse_aiff)?;
        let (_, off_sparse) = read_header_prefix(&mut f_sparse, total_aiff, &sparse_aiff)?;
        assert_eq!(off_sparse, ssnd_far + 16 + 8);

        // MP4 with >256 KiB moov containing covr before mdat.
        let mp4_path = dir.path().join("moov_300k.m4a");
        let ftyp = build_mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A mp42");
        let mut data_payload = vec![0u8; 8];
        data_payload.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]);
        let data_box = build_mp4_box(b"data", &data_payload);
        let covr_box = build_mp4_box(b"covr", &data_box);
        let ilst_box = build_mp4_box(b"ilst", &covr_box);
        let mut meta_payload = vec![0u8; 4];
        meta_payload.extend_from_slice(&ilst_box);
        let meta_box = build_mp4_box(b"meta", &meta_payload);
        let udta_box = build_mp4_box(b"udta", &meta_box);
        let mut moov_payload = udta_box;
        moov_payload.extend_from_slice(&build_mp4_box(b"free", &vec![0u8; 300 * 1024]));
        let moov_box = build_mp4_box(b"moov", &moov_payload);
        let mdat_box = build_mp4_box(b"mdat", &[0xAA; 64]);
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&ftyp);
        mp4.extend_from_slice(&moov_box);
        let mdat_start = mp4.len();
        mp4.extend_from_slice(&mdat_box);
        std::fs::write(&mp4_path, &mp4)?;

        let mut f_mp4 = std::fs::File::open(&mp4_path)?;
        let (hdr_mp4, off_mp4) =
            read_header_prefix(&mut f_mp4, u64::try_from(mp4.len())?, &mp4_path)?;
        assert_eq!(off_mp4, u64::try_from(mdat_start + 8)?);
        let mut mp4_tags = serde_json::Map::new();
        let mut mv = HashSet::new();
        extract_no_artwork_metadata(&hdr_mp4, &mut mp4_tags, &mut mv);
        assert_eq!(mp4_tags.get("COVR").and_then(|v| v.as_u64()), Some(8));
        Ok(())
    }

    #[test]
    fn test_read_tagged_file_respects_format_hint() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mp3_src = dir.path().join("src.mp3");
        write_id3v2_mp3_with_apic(&mp3_src, "Hint Title", "Hint Artist", &[0xFF, 0xD8])?;
        // Copy valid MP3 bytes to a file with an unrelated extension.
        let disguised = dir.path().join("track.bin");
        let bytes = std::fs::read(&mp3_src)?;
        std::fs::write(&disguised, bytes)?;

        let mut f = std::fs::File::open(&disguised)?;
        let tagged = read_tagged_file(&mut f, &disguised, Some("mp3"), true, false)?;
        assert_eq!(tagged.file_type(), FileType::Mpeg);
        Ok(())
    }

    #[test]
    fn test_apic_collision_and_insert_tag_value_tuple_preservation() {
        let mut map = serde_json::Map::new();
        let mut mv = HashSet::new();

        // Non-array APIC collision must not drop picture entries.
        insert_tag_value(
            &mut map,
            &mut mv,
            "APIC".to_string(),
            serde_json::json!("text"),
        );
        append_apic_entries(
            &mut map,
            &mut mv,
            vec![serde_json::json!(["image/jpeg", 3, "cover", 10, 100])],
        );
        append_apic_entries(
            &mut map,
            &mut mv,
            vec![serde_json::json!(["image/png", 4, "back", 12, 200])],
        );
        let apic = map.get("APIC").and_then(|v| v.as_array()).expect("APIC");
        assert_eq!(apic.len(), 3);
        assert_eq!(apic[0], serde_json::json!("text"));
        assert!(apic[1].is_array());
        assert!(apic[2].is_array());

        // Array tag tuples inserted via insert_tag_value must not flatten.
        insert_tag_value(
            &mut map,
            &mut mv,
            "TUP".to_string(),
            serde_json::json!(["a", 1]),
        );
        insert_tag_value(
            &mut map,
            &mut mv,
            "TUP".to_string(),
            serde_json::json!(["b", 2]),
        );
        insert_tag_value(
            &mut map,
            &mut mv,
            "TUP".to_string(),
            serde_json::json!(["c", 3]),
        );
        let tup = map.get("TUP").and_then(|v| v.as_array()).expect("TUP");
        assert_eq!(tup.len(), 3);
        assert_eq!(tup[0], serde_json::json!(["a", 1]));
        assert_eq!(tup[1], serde_json::json!(["b", 2]));
        assert_eq!(tup[2], serde_json::json!(["c", 3]));
    }

    #[test]
    fn test_extended_for_art_not_latched_prematurely() -> Result<(), Box<dyn std::error::Error>> {
        use lofty::config::WriteOptions;
        use lofty::id3::v2::Id3v2Tag;
        use lofty::picture::MimeType;
        use lofty::picture::Picture;
        use lofty::picture::PictureType;
        use lofty::tag::TagExt;

        let dir = tempfile::tempdir()?;
        let mp3_path = dir.path().join("dual_art.mp3");
        let small_art = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        write_id3v2_mp3_with_apic(&mp3_path, "Dual", "Art", &small_art)?;

        // Pad file past 256 KiB and append a second ID3v2-less tag? Use APE at EOF.
        let mut raw = std::fs::read(&mp3_path)?;
        raw.resize(300 * 1024, 0x55);
        std::fs::write(&mp3_path, raw)?;

        let mut id3 = Id3v2Tag::default();
        id3.insert_picture(Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Jpeg),
            Some("front".to_string()),
            small_art,
        ));
        id3.save_to_path(&mp3_path, WriteOptions::default())?;

        // Append APE tag with a distinct picture at the tail (> 256 KiB).
        {
            use lofty::tag::Tag;
            let mut ape = Tag::new(TagType::Ape);
            let ape_art = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 7, 8];
            ape.push_picture(Picture::new_unchecked(
                PictureType::CoverFront,
                Some(MimeType::Png),
                Some("ape".to_string()),
                ape_art,
            ));
            ape.save_to_path(&mp3_path, WriteOptions::default())?;
        }

        let res = scan_file(&mp3_path, true, true, false)?;
        assert!(res.tags.contains_key("APIC"));
        Ok(())
    }

    #[test]
    fn test_cumulative_inline_base64_limit() {
        let mut total_b64 = 0usize;
        // >6 MiB raw encodes to >8 MiB Base64 -> rejected per-picture limit.
        assert!(!can_inline_artwork(
            6 * 1024 * 1024 + 1,
            false,
            &mut total_b64
        ));
        assert_eq!(total_b64, 0);

        // Three 5 MiB raw images: raw sum is 15 MiB (<16 MiB), but Base64 is
        // ~6.67 MiB each, so 1st & 2nd pass (~13.33 MiB) and 3rd exceeds 16 MiB.
        let five_mib = 5 * 1024 * 1024;
        assert!(can_inline_artwork(five_mib, false, &mut total_b64));
        assert!(can_inline_artwork(five_mib, false, &mut total_b64));
        assert!(!can_inline_artwork(five_mib, false, &mut total_b64));
    }

    #[test]
    fn test_vbri_zero_scale_and_16mib_id3v2_seek_header() -> Result<(), Box<dyn std::error::Error>>
    {
        // VBRI header with scale=0 must be rejected rather than coerced to 1.
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        let voff = 36usize;
        frame[voff..voff + 4].copy_from_slice(b"VBRI");
        frame[voff + 18..voff + 20].copy_from_slice(&2u16.to_be_bytes());
        frame[voff + 20..voff + 22].copy_from_slice(&0u16.to_be_bytes()); // scale=0
        frame[voff + 22..voff + 24].copy_from_slice(&2u16.to_be_bytes());
        frame[voff + 26..voff + 30].copy_from_slice(&[0x00, 0x64, 0x00, 0x64]);
        let mut two_frames = frame.clone();
        two_frames.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(
            estimate_mp3_toc_or_vbri_in_slice(&two_frames, 0, 0, 1000, 500, 1000, 1000),
            None
        );

        // >16 MiB ID3v2 tag followed by Xing TOC seek header must be parsed.
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("id3_17m_xing.mp3");
        let tag_payload_len: u32 = 17 * 1024 * 1024;
        let tag_end = 10u64 + u64::from(tag_payload_len);
        let audio_start = tag_end + 417;
        let audio_len: u64 = 1024 * 1024;
        let total_len = audio_start + audio_len;

        let mut xing_frame = vec![0u8; 417];
        xing_frame[0] = 0xFF;
        xing_frame[1] = 0xFB;
        xing_frame[2] = 0x90;
        xing_frame[3] = 0x00;
        let xoff = 36usize;
        xing_frame[xoff..xoff + 4].copy_from_slice(b"Xing");
        xing_frame[xoff + 4..xoff + 8].copy_from_slice(&0x07u32.to_be_bytes());
        xing_frame[xoff + 8..xoff + 12].copy_from_slice(&1000u32.to_be_bytes());
        xing_frame[xoff + 12..xoff + 16].copy_from_slice(&u32::try_from(audio_len)?.to_be_bytes());
        // Skew TOC at index 50 (50% duration) to 230/256 (~90% of audio).
        xing_frame[xoff + 16 + 50] = 230;
        xing_frame[xoff + 16 + 51] = 230;

        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path)?;
            f.set_len(total_len)?;
            f.seek(SeekFrom::Start(0))?;
            let ss0 = u8::try_from((tag_payload_len >> 21) & 0x7F)?;
            let ss1 = u8::try_from((tag_payload_len >> 14) & 0x7F)?;
            let ss2 = u8::try_from((tag_payload_len >> 7) & 0x7F)?;
            let ss3 = u8::try_from(tag_payload_len & 0x7F)?;
            f.write_all(&[b'I', b'D', b'3', 4, 0, 0, ss0, ss1, ss2, ss3])?;
            f.seek(SeekFrom::Start(tag_end))?;
            f.write_all(&xing_frame)?;
            // Write two consecutive audio frames right at audio_start and at 90%.
            let hdr = [0xFFu8, 0xFB, 0x90, 0x00];
            f.write_all(&hdr)?;
            f.seek(SeekFrom::Start(audio_start + 417))?;
            f.write_all(&hdr)?;
            let target_90 = audio_start + (audio_len * 230) / 256;
            f.seek(SeekFrom::Start(target_90))?;
            f.write_all(&hdr)?;
            f.seek(SeekFrom::Start(target_90 + 417))?;
            f.write_all(&hdr)?;
            f.sync_all()?;
        }

        let scan = scan_file(&path, true, false, true)?;
        let dur_ms = scan
            .info
            .get("song_length_ms")
            .and_then(|v| v.as_u64())
            .expect("song_length_ms");
        let half_ms = i64::try_from(dur_ms / 2)?;
        let seek_half = find_frame_offset(&path, Some("mp3"), half_ms)?;
        let expected_near = i64::try_from(audio_start + (audio_len * 230) / 256)?;
        assert_eq!(seek_half.byte_offset, expected_near);
        Ok(())
    }

    #[test]
    fn test_negative_offset_out_of_range_and_wma_asf_ext() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let wav_path = dir.path().join("seek.wav");
        let pcm = vec![0i16; 4410];
        let wav = build_minimal_wav(&pcm, 44100);
        let wav_len = i64::try_from(wav.len())?;
        std::fs::write(&wav_path, &wav)?;

        let out_of_range = find_frame_offset(&wav_path, Some("wav"), -(wav_len + 100))?;
        assert_eq!(out_of_range.byte_offset, -1);

        let at_eof = find_frame_offset(&wav_path, Some("wav"), -wav_len)?;
        assert_eq!(at_eof.byte_offset, -1);

        let in_range = find_frame_offset(&wav_path, Some("wav"), -100)?;
        assert_eq!(in_range.byte_offset, 100);

        assert_eq!(file_type_to_ext(FileType::Custom("wma")), Some("wma"));
        assert_eq!(file_type_to_ext(FileType::Custom("asf")), Some("asf"));
        Ok(())
    }

    #[test]
    fn test_map_item_key_prefixes_mp4_and_missing_variants() {
        use lofty::tag::ItemKey;

        assert_eq!(
            map_item_key(
                &ItemKey::Unknown("TXXX:replaygain_track_gain".to_string()),
                TagType::Id3v2
            ),
            Some("REPLAYGAIN_TRACK_GAIN".to_string())
        );
        assert_eq!(
            map_item_key(
                &ItemKey::Unknown("----:com.apple.iTunes:MusicBrainz Album Id".to_string()),
                TagType::Mp4Ilst
            ),
            Some("MUSICBRAINZ ALBUM ID".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::TrackTitle, TagType::Mp4Ilst),
            Some("NAM".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::TrackArtist, TagType::Mp4Ilst),
            Some("ART".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::AlbumTitle, TagType::Mp4Ilst),
            Some("ALB".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::RecordingDate, TagType::Mp4Ilst),
            Some("DAY".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::Unknown("\u{a9}gen".to_string()), TagType::Mp4Ilst),
            Some("GEN".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::MusicBrainzRecordingId, TagType::Id3v2),
            Some("MUSICBRAINZ_ID".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::MusicBrainzRecordingId, TagType::VorbisComments),
            Some("MUSICBRAINZ_TRACKID".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::MusicBrainzRecordingId, TagType::Ape),
            Some("MUSICBRAINZ_TRACKID".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::Producer, TagType::Id3v2),
            Some("PRODUCER".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::Performer, TagType::Id3v2),
            Some("PERFORMER".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::Producer, TagType::Mp4Ilst),
            Some("PRODUCER".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::Performer, TagType::Mp4Ilst),
            Some("PERFORMER".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::ShowName, TagType::VorbisComments),
            Some("SHOWNAME".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::ShowName, TagType::Mp4Ilst),
            Some("TVSH".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::IntegerBpm, TagType::Id3v2),
            Some("TBPM".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::IntegerBpm, TagType::VorbisComments),
            Some("BPM".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::Description, TagType::VorbisComments),
            Some("DESCRIPTION".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::MusicianCredits, TagType::Id3v2),
            Some("TMCL".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::MusicianCredits, TagType::VorbisComments),
            Some("MUSICIANCREDITS".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::PodcastUrl, TagType::VorbisComments),
            Some("PODCASTURL".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::AudioFileUrl, TagType::VorbisComments),
            Some("AUDIOFILEURL".to_string())
        );
        assert_eq!(
            map_item_key(&ItemKey::AppleXid, TagType::VorbisComments),
            Some("XID".to_string())
        );
    }

    #[test]
    fn test_is_non_mp3_extension_extended_list() {
        for ext in [
            "aac", "m4v", "m4r", "3gp", "3g2", "mov", "wmv", "mpp", "apl", "AAC", "MOV", "WMV",
        ] {
            let name = format!("sample.{ext}");
            assert!(
                is_non_mp3_extension(Path::new(&name)),
                "expected non-mp3 extension for {name}"
            );
        }
        assert!(!is_non_mp3_extension(Path::new("sample.mp3")));
    }

    #[test]
    fn test_id3v2_comm_and_uslt_emit_lang_desc_text_arrays(
    ) -> Result<(), Box<dyn std::error::Error>> {
        fn build_id3v2_frame(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(10 + payload.len());
            out.extend_from_slice(id);
            let sz = u32::try_from(payload.len()).expect("frame len");
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(payload);
            out
        }

        let mut frames = Vec::new();
        // TIT2: "Song"
        frames.extend_from_slice(&build_id3v2_frame(b"TIT2", b"\x03Song"));
        // COMM 1: eng, desc="iTunNORM", text="00001234"
        frames.extend_from_slice(&build_id3v2_frame(
            b"COMM",
            b"\x03eng\x69TunNORM\x0000001234",
        ));
        // COMM 2: fra, desc="", text="Bonjour"
        frames.extend_from_slice(&build_id3v2_frame(b"COMM", b"\x03fra\x00Bonjour"));
        // USLT: eng, desc="lyrics", text="La la la"
        frames.extend_from_slice(&build_id3v2_frame(b"USLT", b"\x03englyrics\x00La la la"));

        let tag_len = u32::try_from(frames.len())?;
        let ss = [
            u8::try_from((tag_len >> 21) & 0x7F)?,
            u8::try_from((tag_len >> 14) & 0x7F)?,
            u8::try_from((tag_len >> 7) & 0x7F)?,
            u8::try_from(tag_len & 0x7F)?,
        ];
        let mut mp3 = Vec::new();
        mp3.extend_from_slice(b"ID3");
        mp3.extend_from_slice(&[3, 0, 0]);
        mp3.extend_from_slice(&ss);
        mp3.extend_from_slice(&frames);
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        for _ in 0..10 {
            mp3.extend_from_slice(&frame);
        }

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("comm_uslt.mp3");
        std::fs::write(&path, mp3)?;

        let res = scan_file(&path, true, true, true)?;
        assert_eq!(res.tags.get("TIT2").and_then(|v| v.as_str()), Some("Song"));
        assert_eq!(
            res.tags.get("COMM"),
            Some(&serde_json::json!([
                ["eng", "iTunNORM", "00001234"],
                ["fra", "", "Bonjour"]
            ]))
        );
        assert_eq!(
            res.tags.get("USLT"),
            Some(&serde_json::json!(["eng", "lyrics", "La la la"]))
        );
        Ok(())
    }

    #[test]
    fn test_mp4_and_ogg_info_parity_fields() -> Result<(), Box<dyn std::error::Error>> {
        // Synthetic MP4 moov -> trak -> tkhd + mdia -> stsd -> mp4a -> esds.
        let mut tkhd = vec![0u8; 84];
        tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&44100u32.to_be_bytes());
        mdhd[16..20].copy_from_slice(&88200u32.to_be_bytes()); // 2000 ms
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"soun");

        // esds: 4-byte fullbox + tag 0x03 (len 20) + 3 bytes + tag 0x04 (len 13) + body
        let mut esds_body = vec![0u8; 4];
        esds_body.extend_from_slice(&[0x03, 18, 0, 1, 0]);
        esds_body.extend_from_slice(&[0x04, 13, 0x40, 0x15, 0, 0, 0]);
        esds_body.extend_from_slice(&256_000u32.to_be_bytes()); // maxBitrate
        esds_body.extend_from_slice(&128_000u32.to_be_bytes()); // avgBitrate
        let esds_box = build_mp4_box(b"esds", &esds_body);

        let mut mp4a_payload = vec![0u8; 28];
        mp4a_payload[16..18].copy_from_slice(&2u16.to_be_bytes()); // channels
        mp4a_payload[18..20].copy_from_slice(&16u16.to_be_bytes()); // bits
        mp4a_payload[24..28].copy_from_slice(&(44100u32 << 16).to_be_bytes());
        mp4a_payload.extend_from_slice(&esds_box);
        let mp4a_box = build_mp4_box(b"mp4a", &mp4a_payload);

        let mut stsd_payload = vec![0u8; 4];
        stsd_payload.extend_from_slice(&1u32.to_be_bytes());
        stsd_payload.extend_from_slice(&mp4a_box);
        let stsd_box = build_mp4_box(b"stsd", &stsd_payload);
        let stbl_box = build_mp4_box(b"stbl", &stsd_box);
        let minf_box = build_mp4_box(b"minf", &stbl_box);
        let mut mdia_payload = build_mp4_box(b"mdhd", &mdhd);
        mdia_payload.extend_from_slice(&build_mp4_box(b"hdlr", &hdlr));
        mdia_payload.extend_from_slice(&minf_box);
        let mdia_box = build_mp4_box(b"mdia", &mdia_payload);
        let mut trak_payload = build_mp4_box(b"tkhd", &tkhd);
        trak_payload.extend_from_slice(&mdia_box);
        let trak_box = build_mp4_box(b"trak", &trak_payload);

        // Video track with tkhd (1920x1080) and avc1 VisualSampleEntry (1920x1080).
        let mut v_tkhd = vec![0u8; 84];
        v_tkhd[12..16].copy_from_slice(&2u32.to_be_bytes());
        v_tkhd[76..80].copy_from_slice(&(1920u32 << 16).to_be_bytes());
        v_tkhd[80..84].copy_from_slice(&(1080u32 << 16).to_be_bytes());
        let mut v_hdlr = vec![0u8; 24];
        v_hdlr[8..12].copy_from_slice(b"vide");
        let mut avc1_payload = vec![0u8; 78];
        avc1_payload[24..26].copy_from_slice(&1920u16.to_be_bytes());
        avc1_payload[26..28].copy_from_slice(&1080u16.to_be_bytes());
        let avc1_box = build_mp4_box(b"avc1", &avc1_payload);
        let mut v_stsd_payload = vec![0u8; 4];
        v_stsd_payload.extend_from_slice(&1u32.to_be_bytes());
        v_stsd_payload.extend_from_slice(&avc1_box);
        let v_stsd = build_mp4_box(b"stsd", &v_stsd_payload);
        let v_stbl = build_mp4_box(b"stbl", &v_stsd);
        let v_minf = build_mp4_box(b"minf", &v_stbl);
        let mut v_mdia_payload = build_mp4_box(b"mdhd", &mdhd);
        v_mdia_payload.extend_from_slice(&build_mp4_box(b"hdlr", &v_hdlr));
        v_mdia_payload.extend_from_slice(&v_minf);
        let v_mdia = build_mp4_box(b"mdia", &v_mdia_payload);
        let mut v_trak_payload = build_mp4_box(b"tkhd", &v_tkhd);
        v_trak_payload.extend_from_slice(&v_mdia);
        let v_trak = build_mp4_box(b"trak", &v_trak_payload);

        let mut moov_payload = trak_box;
        moov_payload.extend_from_slice(&v_trak);
        let moov_box = build_mp4_box(b"moov", &moov_payload);
        let ftyp_box = build_mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A mp42");
        let mdat_box = build_mp4_box(b"mdat", b"\x00\x00\x00\x00");
        let mut mp4_bytes = ftyp_box;
        mp4_bytes.extend_from_slice(&mdat_box);
        mp4_bytes.extend_from_slice(&moov_box);

        let dir = tempfile::tempdir()?;
        let mp4_path = dir.path().join("dummy.m4a");
        std::fs::write(&mp4_path, &mp4_bytes)?;
        let mut f = std::fs::File::open(&mp4_path)?;
        let mut mp4_info = serde_json::Map::new();
        populate_mp4_info(
            &mut f,
            u64::try_from(mp4_bytes.len())?,
            &mp4_bytes,
            0,
            2000,
            None,
            &mut mp4_info,
        )?;
        assert_eq!(
            mp4_info.get("avg_bitrate").and_then(|v| v.as_u64()),
            Some(128_000)
        );
        assert_eq!(
            mp4_info.get("leading_mdat").and_then(|v| v.as_u64()),
            Some(1)
        );
        let tracks = mp4_info
            .get("tracks")
            .and_then(|v| v.as_array())
            .expect("tracks");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0]["id"].as_u64(), Some(1));
        assert_eq!(tracks[0]["encoding"].as_str(), Some("mp4a"));
        assert_eq!(tracks[0]["samplerate"].as_u64(), Some(44100));
        assert_eq!(tracks[0]["channels"].as_u64(), Some(2));
        assert_eq!(tracks[0]["avg_bitrate"].as_u64(), Some(128_000));
        assert!(tracks[0].get("width").is_none());
        assert_eq!(tracks[1]["id"].as_u64(), Some(2));
        assert_eq!(tracks[1]["type"].as_str(), Some("video"));
        assert_eq!(tracks[1]["encoding"].as_str(), Some("avc1"));
        assert_eq!(tracks[1]["width"].as_u64(), Some(1920));
        assert_eq!(tracks[1]["height"].as_u64(), Some(1080));

        // Synthetic Ogg Vorbis page 0.
        let mut vorbis_pkt = vec![0u8; 30];
        vorbis_pkt[0..7].copy_from_slice(b"\x01vorbis");
        vorbis_pkt[11] = 2;
        vorbis_pkt[12..16].copy_from_slice(&44100u32.to_le_bytes());
        vorbis_pkt[16..20].copy_from_slice(&320_000i32.to_le_bytes());
        vorbis_pkt[20..24].copy_from_slice(&192_000i32.to_le_bytes());
        vorbis_pkt[24..28].copy_from_slice(&128_000i32.to_le_bytes());
        let mut ogg_hdr = vec![0u8; 28];
        ogg_hdr[0..4].copy_from_slice(b"OggS");
        ogg_hdr[26] = 1;
        ogg_hdr[27] = 30;
        ogg_hdr.extend_from_slice(&vorbis_pkt);
        let mut ogg_info = serde_json::Map::new();
        populate_ogg_info(&ogg_hdr, 24_000, 1000, None, &mut ogg_info);
        assert_eq!(
            ogg_info.get("ogg_type").and_then(|v| v.as_str()),
            Some("vorbis")
        );
        assert_eq!(
            ogg_info.get("bitrate_upper").and_then(|v| v.as_u64()),
            Some(320_000)
        );
        assert_eq!(
            ogg_info.get("bitrate_nominal").and_then(|v| v.as_u64()),
            Some(192_000)
        );
        assert_eq!(
            ogg_info.get("bitrate_lower").and_then(|v| v.as_u64()),
            Some(128_000)
        );
        assert_eq!(
            ogg_info.get("bitrate_average").and_then(|v| v.as_u64()),
            Some(192_000)
        );

        let mut opus_hdr = vec![0u8; 28];
        opus_hdr[0..4].copy_from_slice(b"OggS");
        opus_hdr[26] = 1;
        opus_hdr[27] = 19;
        opus_hdr.extend_from_slice(b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00");
        let mut opus_info = serde_json::Map::new();
        populate_ogg_info(&opus_hdr, 16_000, 1000, None, &mut opus_info);
        assert_eq!(
            opus_info.get("ogg_type").and_then(|v| v.as_str()),
            Some("opus")
        );
        assert_eq!(
            opus_info.get("bitrate_average").and_then(|v| v.as_u64()),
            Some(128_000)
        );
        Ok(())
    }

    #[test]
    fn test_asf_wma_scan_streams_lossless_and_wm_picture() -> Result<(), Box<dyn std::error::Error>>
    {
        fn asf_obj(guid: &[u8; 16], body: &[u8]) -> Vec<u8> {
            let total = u64::try_from(24 + body.len()).expect("asf obj len");
            let mut out = Vec::with_capacity(24 + body.len());
            out.extend_from_slice(guid);
            out.extend_from_slice(&total.to_le_bytes());
            out.extend_from_slice(body);
            out
        }
        fn utf16le_nul(s: &str) -> Vec<u8> {
            let mut out = Vec::new();
            for u in s.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&[0, 0]);
            out
        }

        // File Properties Object (80-byte body)
        let mut fp = vec![0u8; 80];
        fp[40..48].copy_from_slice(&30_000_000u64.to_le_bytes()); // 3000 ms
        fp[56..64].copy_from_slice(&1000u64.to_le_bytes()); // 1000 ms preroll -> 2000 ms
        fp[76..80].copy_from_slice(&1_411_200u32.to_le_bytes()); // max_bitrate

        // Stream Properties Object: WMA Lossless (0x0163)
        let mut sp = vec![0u8; 54 + 18];
        sp[0..16].copy_from_slice(&ASF_AUDIO_MEDIA_GUID);
        sp[40..44].copy_from_slice(&18u32.to_le_bytes());
        sp[48..50].copy_from_slice(&1u16.to_le_bytes()); // stream 1
        let spec = &mut sp[54..];
        spec[0..2].copy_from_slice(&0x0163u16.to_le_bytes());
        spec[2..4].copy_from_slice(&2u16.to_le_bytes());
        spec[4..8].copy_from_slice(&44100u32.to_le_bytes());
        spec[8..12].copy_from_slice(&110_250u32.to_le_bytes()); // 882_000 bps
        spec[14..16].copy_from_slice(&16u16.to_le_bytes());

        // Content Description Object
        let title_u16 = utf16le_nul("WMA Lossless Track");
        let author_u16 = utf16le_nul("WMA Artist");
        let mut cd = Vec::new();
        cd.extend_from_slice(&u16::try_from(title_u16.len())?.to_le_bytes());
        cd.extend_from_slice(&u16::try_from(author_u16.len())?.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&title_u16);
        cd.extend_from_slice(&author_u16);

        // Extended Content Description Object: WM/AlbumTitle + WM/Picture
        let pic_bytes = [0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        let mime_u16 = utf16le_nul("image/jpeg");
        let desc_u16 = utf16le_nul("front");
        let mut wm_pic = Vec::new();
        wm_pic.push(3); // CoverFront
        wm_pic.extend_from_slice(&u32::try_from(pic_bytes.len())?.to_le_bytes());
        wm_pic.extend_from_slice(&mime_u16);
        wm_pic.extend_from_slice(&desc_u16);
        wm_pic.extend_from_slice(&pic_bytes);

        let alb_name = utf16le_nul("WM/AlbumTitle");
        let alb_val = utf16le_nul("WMA Album");
        let pic_name = utf16le_nul("WM/Picture");
        let mut ecd = Vec::new();
        ecd.extend_from_slice(&2u16.to_le_bytes());
        ecd.extend_from_slice(&u16::try_from(alb_name.len())?.to_le_bytes());
        ecd.extend_from_slice(&alb_name);
        ecd.extend_from_slice(&0u16.to_le_bytes());
        ecd.extend_from_slice(&u16::try_from(alb_val.len())?.to_le_bytes());
        ecd.extend_from_slice(&alb_val);
        ecd.extend_from_slice(&u16::try_from(pic_name.len())?.to_le_bytes());
        ecd.extend_from_slice(&pic_name);
        ecd.extend_from_slice(&1u16.to_le_bytes());
        ecd.extend_from_slice(&u16::try_from(wm_pic.len())?.to_le_bytes());
        ecd.extend_from_slice(&wm_pic);

        let mut hdr_body = Vec::new();
        hdr_body.extend_from_slice(&4u32.to_le_bytes());
        hdr_body.push(1);
        hdr_body.push(2);
        hdr_body.extend_from_slice(&asf_obj(&ASF_FILE_PROPERTIES_GUID, &fp));
        hdr_body.extend_from_slice(&asf_obj(&ASF_STREAM_PROPERTIES_GUID, &sp));
        hdr_body.extend_from_slice(&asf_obj(&ASF_CONTENT_DESC_GUID, &cd));
        hdr_body.extend_from_slice(&asf_obj(&ASF_EXT_CONTENT_DESC_GUID, &ecd));

        let hdr_total = u64::try_from(24 + hdr_body.len())?;
        let mut asf = Vec::new();
        asf.extend_from_slice(&ASF_HEADER_GUID);
        asf.extend_from_slice(&hdr_total.to_le_bytes());
        asf.extend_from_slice(&hdr_body);
        asf.extend_from_slice(&ASF_DATA_GUID);
        asf.extend_from_slice(&50u64.to_le_bytes());
        asf.extend_from_slice(&[0u8; 26]);

        let dir = tempfile::tempdir()?;
        let wma_path = dir.path().join("lossless.wma");
        std::fs::write(&wma_path, &asf)?;

        let res_no_art = scan_file(&wma_path, true, true, true)?;
        assert_eq!(
            res_no_art.info.get("max_bitrate").and_then(|v| v.as_u64()),
            Some(1_411_200)
        );
        assert_eq!(
            res_no_art.info.get("lossless").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            res_no_art
                .info
                .get("song_length_ms")
                .and_then(|v| v.as_u64()),
            Some(2000)
        );
        assert_eq!(
            res_no_art.info.get("samplerate").and_then(|v| v.as_u64()),
            Some(44100)
        );
        let streams = res_no_art
            .info
            .get("streams")
            .and_then(|v| v.as_array())
            .expect("streams");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["lossless"].as_u64(), Some(1));

        assert_eq!(
            res_no_art.tags.get("Title").and_then(|v| v.as_str()),
            Some("WMA Lossless Track")
        );
        assert_eq!(
            res_no_art
                .tags
                .get("WM/AlbumTitle")
                .and_then(|v| v.as_str()),
            Some("WMA Album")
        );
        let wm_pic_no_art = res_no_art.tags.get("WM/Picture").expect("WM/Picture");
        assert_eq!(wm_pic_no_art["image_type"].as_u64(), Some(3));
        assert_eq!(wm_pic_no_art["image"].as_u64(), Some(8));
        assert!(wm_pic_no_art.get("picture_type").is_none());
        assert!(wm_pic_no_art.get("image_data").is_none());
        assert!(wm_pic_no_art["offset"].as_u64().unwrap_or(0) > 0);

        let res_with_art = scan_file(&wma_path, true, true, false)?;
        let wm_pic_art = res_with_art.tags.get("WM/Picture").expect("WM/Picture");
        assert_eq!(wm_pic_art["image_type"].as_u64(), Some(3));
        assert!(wm_pic_art["image"].is_string());
        Ok(())
    }

    #[test]
    fn test_open_regular_file_rejects_directories_and_symlinks(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        assert!(open_regular_file(dir.path()).is_err());

        #[cfg(unix)]
        {
            let target = dir.path().join("real.mp3");
            std::fs::write(&target, b"123")?;
            let link = dir.path().join("link.mp3");
            std::os::unix::fs::symlink(&target, &link)?;
            assert!(open_regular_file(&link).is_err());
        }
        Ok(())
    }
}
