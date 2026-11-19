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
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

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

fn find_mp3_frame(bytes: &[u8], start: usize) -> Option<(usize, Mp3FrameHeader)> {
    let mut pos = start;
    while bytes.len().saturating_sub(pos) >= 4 {
        if bytes[pos] == 0xFF && (bytes[pos + 1] & 0xE0) == 0xE0 {
            if let Some(hdr) = parse_mp3_header(&bytes[pos..pos + 4]) {
                let next_pos = match pos.checked_add(hdr.frame_length) {
                    Some(p) => p,
                    None => break,
                };
                if bytes.len().saturating_sub(next_pos) >= 4 {
                    if let Some(next_hdr) = parse_mp3_header(&bytes[next_pos..next_pos + 4]) {
                        if next_hdr.version == hdr.version
                            && next_hdr.layer == hdr.layer
                            && next_hdr.sample_rate == hdr.sample_rate
                        {
                            return Some((pos, hdr));
                        }
                    }
                } else {
                    return Some((pos, hdr));
                }
            }
        }
        pos = match pos.checked_add(1) {
            Some(p) => p,
            None => break,
        };
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

fn find_slice_offset(haystack: &[u8], needle: &[u8]) -> Option<u64> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let prefix_len = needle.len().min(16);
    let prefix = &needle[..prefix_len];
    let max_pos = haystack.len() - needle.len();
    for i in 0..=max_pos {
        if &haystack[i..i + prefix_len] == prefix && &haystack[i..i + needle.len()] == needle {
            return u64::try_from(i).ok();
        }
    }
    None
}

fn compute_audio_offset(file_bytes: &[u8], _path: &Path) -> u64 {
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
        let mut pos = tag_end.min(file_bytes.len());
        while file_bytes.len().saturating_sub(pos) >= 4 {
            if file_bytes[pos] == 0xFF && (file_bytes[pos + 1] & 0xE0) == 0xE0 {
                if let Some(hdr) = parse_mp3_header(&file_bytes[pos..pos + 4]) {
                    let frame_len = hdr.frame_length.max(4);
                    let frame_end = pos.saturating_add(frame_len).min(file_bytes.len());
                    let payload = &file_bytes[pos..frame_end];
                    if payload
                        .windows(4)
                        .any(|w| w == b"Xing" || w == b"Info" || w == b"VBRI")
                    {
                        pos = match pos.checked_add(frame_len) {
                            Some(next) => next,
                            None => break,
                        };
                        continue;
                    }
                    return u64::try_from(pos).unwrap_or(u64::MAX);
                }
            }
            pos = match pos.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }
        return u64::try_from(pos.min(file_bytes.len())).unwrap_or(u64::MAX);
    }

    if file_bytes.len() >= 4 && &file_bytes[0..4] == b"fLaC" {
        let mut pos: usize = 4;
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
        return u64::try_from(pos.min(file_bytes.len())).unwrap_or(u64::MAX);
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
    }

    // MP4 container: find mdat box
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
        if box_type == b"mdat" {
            let mdat_data = pos.saturating_add(header_len).min(file_bytes.len());
            return u64::try_from(mdat_data).unwrap_or(u64::MAX);
        }
        pos = match pos.checked_add(actual_len) {
            Some(p) if p <= file_bytes.len() => p,
            _ => break,
        };
    }

    // MP3 without ID3 header
    if let Some((offset, _)) = find_mp3_frame(file_bytes, 0) {
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
const MAX_ARTWORK_INLINE_BYTES: usize = 16 * 1024 * 1024;

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
        if is_last {
            return false;
        }
        if next_pos > header_bytes.len() {
            return true;
        }
        pos = next_pos;
    }
    true
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
        } else if tag_end < file_size {
            // ID3 tag exceeds MAX_HEADER_READ; jump straight past tag to
            // locate the first audio frame after Xing/Info/VBRI headers.
            let tail = read_window_at(file, file_size, tag_end, 16 * 1024)?;
            let mut offset = tag_end;
            let mut pos: usize = 0;
            while tail.len().saturating_sub(pos) >= 4 {
                if tail[pos] == 0xFF && (tail[pos + 1] & 0xE0) == 0xE0 {
                    if let Some(hdr) = parse_mp3_header(&tail[pos..pos + 4]) {
                        let frame_len = hdr.frame_length.max(4);
                        let frame_end = pos.saturating_add(frame_len).min(tail.len());
                        let payload = &tail[pos..frame_end];
                        if payload
                            .windows(4)
                            .any(|w| w == b"Xing" || w == b"Info" || w == b"VBRI")
                        {
                            pos = match pos.checked_add(frame_len) {
                                Some(next) => next,
                                None => break,
                            };
                            continue;
                        }
                        if let Ok(rel) = u64::try_from(pos) {
                            offset = tag_end.saturating_add(rel);
                        }
                        break;
                    }
                }
                pos = match pos.checked_add(1) {
                    Some(next) => next,
                    None => break,
                };
            }
            return Ok((header_bytes, offset.min(file_size)));
        }
    } else if flac_metadata_incomplete(&header_bytes) {
        extend_header_bytes(file, file_size, &mut header_bytes, MAX_HEADER_READ)?;
    }

    let audio_offset = compute_audio_offset(&header_bytes, path).min(file_size);
    Ok((header_bytes, audio_offset))
}

fn read_tagged_file(
    file: &mut std::fs::File,
    path: &Path,
    format_hint: Option<&str>,
) -> Result<TaggedFile, AudioScanError> {
    file.seek(SeekFrom::Start(0))?;
    let reader = BufReader::new(&mut *file);
    let mut probe = Probe::new(reader).options(ParseOptions::new());
    if let Some(hint) = format_hint {
        if let Some(ft) = FileType::from_ext(hint) {
            probe = probe.set_file_type(ft);
        }
    }
    let probe = probe.guess_file_type()?;
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
        if let Some(rel) = find_slice_offset(&header_bytes[boundary..], data) {
            return u64::try_from(boundary)
                .ok()
                .and_then(|b| b.checked_add(rel));
        }
    }
    None
}

fn insert_tag_value(
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    key: String,
    val_json: serde_json::Value,
) {
    if let Some(existing) = tags_map.get_mut(&key) {
        if let Some(arr) = existing.as_array_mut() {
            arr.push(val_json);
        } else {
            let old = existing.take();
            *existing = serde_json::json!([old, val_json]);
        }
    } else {
        tags_map.insert(key, val_json);
    }
}

fn append_apic_entries(
    tags_map: &mut serde_json::Map<String, serde_json::Value>,
    mut entries: Vec<serde_json::Value>,
) {
    if entries.is_empty() {
        return;
    }
    if let Some(existing) = tags_map.get_mut("APIC") {
        if let Some(arr) = existing.as_array_mut() {
            let is_nested = arr.first().map(|v| v.is_array()).unwrap_or(false);
            if is_nested {
                arr.append(&mut entries);
            } else {
                let first = existing.take();
                let mut combined = Vec::with_capacity(entries.len() + 1);
                combined.push(first);
                combined.append(&mut entries);
                *existing = serde_json::Value::Array(combined);
            }
        }
    } else if entries.len() == 1 {
        if let Some(single) = entries.pop() {
            tags_map.insert("APIC".to_string(), single);
        }
    } else {
        tags_map.insert("APIC".to_string(), serde_json::Value::Array(entries));
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

pub fn scan_file(
    path: &Path,
    include_info: bool,
    include_tags: bool,
    no_artwork: bool,
) -> Result<ScanResult, AudioScanError> {
    let mut file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    let (mut header_bytes, audio_offset) = read_header_prefix(&mut file, file_size, path)?;

    let mut info_map = serde_json::Map::new();
    let mut tags_map = serde_json::Map::new();

    let tagged_file = read_tagged_file(&mut file, path, None)?;

    if include_info {
        let props = tagged_file.properties();
        let song_length_ms = u64::try_from(props.duration().as_millis()).unwrap_or(u64::MAX);
        let audio_size = file_size.saturating_sub(audio_offset);
        let bitrate_bps = props
            .audio_bitrate()
            .map(|kbps| u64::from(kbps).saturating_mul(1000))
            .unwrap_or(0);
        let samplerate = props.sample_rate().unwrap_or(44100);
        let channels = u64::from(props.channels().unwrap_or(2));
        let bits_per_sample = u64::from(props.bit_depth().unwrap_or(16));

        info_map.insert(
            "song_length_ms".to_string(),
            serde_json::json!(song_length_ms),
        );
        info_map.insert("file_size".to_string(), serde_json::json!(file_size));
        info_map.insert("audio_offset".to_string(), serde_json::json!(audio_offset));
        info_map.insert("audio_size".to_string(), serde_json::json!(audio_size));
        info_map.insert("bitrate".to_string(), serde_json::json!(bitrate_bps));
        info_map.insert("samplerate".to_string(), serde_json::json!(samplerate));
        info_map.insert("channels".to_string(), serde_json::json!(channels));
        info_map.insert(
            "stereo".to_string(),
            serde_json::json!(if channels == 2 { 1 } else { 0 }),
        );
        info_map.insert(
            "bits_per_sample".to_string(),
            serde_json::json!(bits_per_sample),
        );

        if header_bytes.len() >= 3 && &header_bytes[0..3] == b"ID3" {
            let major = header_bytes.get(3).copied().unwrap_or(4);
            let revision = header_bytes.get(4).copied().unwrap_or(0);
            info_map.insert(
                "id3_version".to_string(),
                serde_json::json!(format!("ID3v2.{major}.{revision}")),
            );
            info_map.insert("layer".to_string(), serde_json::json!(3));
        } else if let Some((_, hdr)) = find_mp3_frame(&header_bytes, 0) {
            info_map.insert("layer".to_string(), serde_json::json!(hdr.layer));
        }
    }

    if include_tags {
        let mut extended_for_art = false;
        for tag in tagged_file.tags() {
            let target_tag_type = match tag.tag_type() {
                TagType::Id3v1 => TagType::Id3v2,
                other => other,
            };

            for item in tag.items() {
                let key_name = if let Some(mapped) = item.key().map_key(target_tag_type, false) {
                    mapped.to_uppercase()
                } else {
                    match item.key() {
                        lofty::tag::ItemKey::Unknown(ref s) => s.to_uppercase(),
                        _ => format!("{:?}", item.key()).to_uppercase(),
                    }
                };

                let val_json = match item.value() {
                    lofty::tag::ItemValue::Text(s) => serde_json::json!(s),
                    lofty::tag::ItemValue::Locator(s) => serde_json::json!(s),
                    lofty::tag::ItemValue::Binary(b) => {
                        serde_json::json!(base64::engine::general_purpose::STANDARD.encode(b))
                    }
                };

                insert_tag_value(&mut tags_map, key_name, val_json);
            }

            let pics = tag.pictures();
            if !pics.is_empty() && !extended_for_art {
                let first_data = pics[0].data();
                if !first_data.is_empty()
                    && locate_artwork_offset(&header_bytes, audio_offset, first_data).is_none()
                {
                    extend_header_bytes(&mut file, file_size, &mut header_bytes, MAX_HEADER_READ)?;
                }
                extended_for_art = true;
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
                            let mime = pic.mime_type().map(|m| m.as_str()).unwrap_or("image/jpeg");
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = pic.description().unwrap_or("");
                            let offset = locate_artwork_offset(&header_bytes, audio_offset, data);
                            let img_data_val =
                                if no_artwork || data.len() > MAX_ARTWORK_INLINE_BYTES {
                                    serde_json::json!(data.len())
                                } else {
                                    serde_json::json!(
                                        base64::engine::general_purpose::STANDARD.encode(data)
                                    )
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
                                if no_artwork || data.len() > MAX_ARTWORK_INLINE_BYTES {
                                    tags_map
                                        .insert("COVR".to_string(), serde_json::json!(data.len()));
                                } else {
                                    let b64 =
                                        base64::engine::general_purpose::STANDARD.encode(data);
                                    tags_map.insert("COVR".to_string(), serde_json::json!(b64));
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
                                if no_artwork || data.len() > MAX_ARTWORK_INLINE_BYTES {
                                    tags_map.insert(
                                        "COVER ART (FRONT)".to_string(),
                                        serde_json::json!(data.len()),
                                    );
                                } else {
                                    let b64 =
                                        base64::engine::general_purpose::STANDARD.encode(data);
                                    tags_map.insert(
                                        "COVER ART (FRONT)".to_string(),
                                        serde_json::json!(b64),
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
                            let mime = pic.mime_type().map(|m| m.as_str()).unwrap_or("image/jpeg");
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = pic.description().unwrap_or("");
                            let offset = locate_artwork_offset(&header_bytes, audio_offset, data);
                            let entry = if no_artwork || data.len() > MAX_ARTWORK_INLINE_BYTES {
                                serde_json::json!([mime, type_u8, desc, data.len(), offset])
                            } else {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                                serde_json::json!([mime, type_u8, desc, b64])
                            };
                            apic_entries.push(entry);
                        }
                        append_apic_entries(&mut tags_map, apic_entries);
                    }
                }
            }
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

fn estimate_mp3_toc_or_vbri(
    header_bytes: &[u8],
    audio_offset: u64,
    audio_size: u64,
    target_ms: u64,
    total_duration_ms: u64,
    file_size: u64,
) -> Option<u64> {
    if total_duration_ms == 0 {
        return None;
    }
    let tag_start = id3v2_tag_end(header_bytes).unwrap_or(0);
    let scan_start = usize::try_from(tag_start).ok()?.min(header_bytes.len());
    let (frame_pos, hdr) = find_mp3_frame(header_bytes, scan_start)?;
    let frame_end = frame_pos
        .checked_add(hdr.frame_length)?
        .min(header_bytes.len());
    let frame = &header_bytes[frame_pos..frame_end];

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
            if (1..=4).contains(&entry_bytes) && num_entries > 0 {
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
                        acc = acc.saturating_add(val.saturating_mul(scale.max(1)));
                    }
                    let abs = audio_offset.saturating_add(acc);
                    return Some(clamp_seek_offset(abs, audio_offset, file_size));
                }
            }
        }
    }

    None
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
    let mut file = std::fs::File::open(path)?;
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

    let tagged_file = read_tagged_file(&mut file, path, hinted_ext.as_deref())?;
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
                byte_offset: clamp_u64_to_i64(target_byte.min(file_size)),
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
                    &header_bytes,
                    audio_offset,
                    audio_size,
                    target_ms,
                    total_duration_ms,
                    file_size,
                )
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
}
