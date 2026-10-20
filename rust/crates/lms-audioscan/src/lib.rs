#![deny(unsafe_op_in_unsafe_fn)]

use std::path::Path;
use base64::Engine;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::TagType;
use serde::{Deserialize, Serialize};

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
            const RATES: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
            RATES[idx as usize]
        }
        (1, 2, idx) => {
            const RATES: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
            RATES[idx as usize]
        }
        (1, 1, idx) => {
            const RATES: [u32; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
            RATES[idx as usize]
        }
        (_, 1, idx) => {
            const RATES: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
            RATES[idx as usize]
        }
        (_, _, idx) => {
            const RATES: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
            RATES[idx as usize]
        }
    };

    let padding_slot = if padding { 1 } else { 0 };
    let frame_length = match (version, layer) {
        (1, 1) => ((12 * bitrate_kbps * 1000 / sample_rate) + padding_slot * 4) as usize,
        (1, _) => ((144 * bitrate_kbps * 1000 / sample_rate) + padding_slot) as usize,
        (_, 1) => ((12 * bitrate_kbps * 1000 / sample_rate) + padding_slot * 4) as usize,
        (_, _) => ((72 * bitrate_kbps * 1000 / sample_rate) + padding_slot) as usize,
    };

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
    while pos + 4 <= bytes.len() {
        if bytes[pos] == 0xFF && (bytes[pos + 1] & 0xE0) == 0xE0 {
            if let Some(hdr) = parse_mp3_header(&bytes[pos..pos + 4]) {
                let next_pos = pos + hdr.frame_length;
                if next_pos + 4 <= bytes.len() {
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
        pos += 1;
    }
    None
}

fn find_flac_frame(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while pos + 2 <= bytes.len() {
        if bytes[pos] == 0xFF && (bytes[pos + 1] & 0xFC) == 0xF8 {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

fn find_ogg_page(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    while pos + 4 <= bytes.len() {
        if &bytes[pos..pos + 4] == b"OggS" {
            return Some(pos);
        }
        pos += 1;
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
            return Some(i as u64);
        }
    }
    None
}

fn compute_audio_offset(file_bytes: &[u8], _path: &Path) -> u64 {
    if file_bytes.len() >= 10 && &file_bytes[0..3] == b"ID3" {
        let size = ((file_bytes[6] as u64 & 0x7F) << 21)
            | ((file_bytes[7] as u64 & 0x7F) << 14)
            | ((file_bytes[8] as u64 & 0x7F) << 7)
            | (file_bytes[9] as u64 & 0x7F);
        let footer = (file_bytes[5] & 0x10) != 0;
        let tag_end = (10 + size + if footer { 10 } else { 0 }) as usize;
        let mut pos = tag_end;
        while pos + 4 <= file_bytes.len() {
            if file_bytes[pos] == 0xFF
                && (file_bytes[pos + 1] & 0xE0) == 0xE0
                && parse_mp3_header(&file_bytes[pos..pos + 4]).is_some()
            {
                return pos as u64;
            }
            pos += 1;
        }
        return tag_end as u64;
    }

    if file_bytes.len() >= 4 && &file_bytes[0..4] == b"fLaC" {
        let mut pos = 4;
        while pos + 4 <= file_bytes.len() {
            let header_byte = file_bytes[pos];
            let is_last = (header_byte & 0x80) != 0;
            let block_len = ((file_bytes[pos + 1] as usize) << 16)
                | ((file_bytes[pos + 2] as usize) << 8)
                | (file_bytes[pos + 3] as usize);
            pos += 4 + block_len;
            if is_last {
                return pos as u64;
            }
        }
        return pos as u64;
    }

    if file_bytes.len() >= 12 && &file_bytes[0..4] == b"RIFF" && &file_bytes[8..12] == b"WAVE" {
        let mut pos = 12;
        while pos + 8 <= file_bytes.len() {
            let chunk_id = &file_bytes[pos..pos + 4];
            let chunk_len = u32::from_le_bytes([
                file_bytes[pos + 4],
                file_bytes[pos + 5],
                file_bytes[pos + 6],
                file_bytes[pos + 7],
            ]) as usize;
            if chunk_id == b"data" {
                return (pos + 8) as u64;
            }
            pos += 8 + chunk_len;
            if chunk_len % 2 == 1 {
                pos += 1;
            }
        }
    }

    if file_bytes.len() >= 12
        && &file_bytes[0..4] == b"FORM"
        && (&file_bytes[8..12] == b"AIFF" || &file_bytes[8..12] == b"AIFC")
    {
        let mut pos = 12;
        while pos + 8 <= file_bytes.len() {
            let chunk_id = &file_bytes[pos..pos + 4];
            let chunk_len = u32::from_be_bytes([
                file_bytes[pos + 4],
                file_bytes[pos + 5],
                file_bytes[pos + 6],
                file_bytes[pos + 7],
            ]) as usize;
            if chunk_id == b"SSND" {
                let ssnd_offset = if pos + 12 <= file_bytes.len() {
                    u32::from_be_bytes([
                        file_bytes[pos + 8],
                        file_bytes[pos + 9],
                        file_bytes[pos + 10],
                        file_bytes[pos + 11],
                    ]) as usize
                } else {
                    0
                };
                return (pos + 16 + ssnd_offset) as u64;
            }
            pos += 8 + chunk_len;
            if chunk_len % 2 == 1 {
                pos += 1;
            }
        }
    }

    // MP4 container: find mdat box
    let mut pos = 0;
    while pos + 8 <= file_bytes.len() {
        let box_len = u32::from_be_bytes([
            file_bytes[pos],
            file_bytes[pos + 1],
            file_bytes[pos + 2],
            file_bytes[pos + 3],
        ]) as usize;
        let box_type = &file_bytes[pos + 4..pos + 8];
        if box_type == b"mdat" {
            return (pos + 8) as u64;
        }
        if box_len == 0 || box_len == 1 {
            break;
        }
        pos += box_len;
    }

    // MP3 without ID3 header
    if let Some((offset, _)) = find_mp3_frame(file_bytes, 0) {
        return offset as u64;
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

pub fn scan_file(
    path: &Path,
    include_info: bool,
    include_tags: bool,
    no_artwork: bool,
) -> Result<ScanResult, AudioScanError> {
    let file_bytes = std::fs::read(path)?;
    let file_size = file_bytes.len() as u64;

    let mut info_map = serde_json::Map::new();
    let mut tags_map = serde_json::Map::new();

    let tagged_file = Probe::open(path)?.read()?;

    if include_info {
        let props = tagged_file.properties();
        let song_length_ms = props.duration().as_millis() as u64;
        let audio_offset = compute_audio_offset(&file_bytes, path);
        let audio_size = file_size.saturating_sub(audio_offset);
        let bitrate_bps = props
            .audio_bitrate()
            .map(|kbps| (kbps as u64) * 1000)
            .unwrap_or(0);
        let samplerate = props.sample_rate().unwrap_or(44100);
        let channels = props.channels().unwrap_or(2) as u64;
        let bits_per_sample = props.bit_depth().unwrap_or(16) as u64;

        info_map.insert("song_length_ms".to_string(), serde_json::json!(song_length_ms));
        info_map.insert("file_size".to_string(), serde_json::json!(file_size));
        info_map.insert("audio_offset".to_string(), serde_json::json!(audio_offset));
        info_map.insert("audio_size".to_string(), serde_json::json!(audio_size));
        info_map.insert("bitrate".to_string(), serde_json::json!(bitrate_bps));
        info_map.insert("samplerate".to_string(), serde_json::json!(samplerate));
        info_map.insert("channels".to_string(), serde_json::json!(channels));
        info_map.insert("stereo".to_string(), serde_json::json!(if channels == 2 { 1 } else { 0 }));
        info_map.insert("bits_per_sample".to_string(), serde_json::json!(bits_per_sample));

        if file_bytes.len() >= 3 && &file_bytes[0..3] == b"ID3" {
            let major = file_bytes.get(3).copied().unwrap_or(4);
            let revision = file_bytes.get(4).copied().unwrap_or(0);
            info_map.insert("id3_version".to_string(), serde_json::json!(format!("ID3v2.{major}.{revision}")));
            info_map.insert("layer".to_string(), serde_json::json!(3));
        } else if let Some((_, hdr)) = find_mp3_frame(&file_bytes, 0) {
            info_map.insert("layer".to_string(), serde_json::json!(hdr.layer));
        }
    }

    if include_tags {
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

                if let Some(existing) = tags_map.get_mut(&key_name) {
                    if let Some(arr) = existing.as_array_mut() {
                        arr.push(val_json);
                    } else {
                        let old = existing.take();
                        *existing = serde_json::json!([old, val_json]);
                    }
                } else {
                    tags_map.insert(key_name, val_json);
                }
            }

            let pics = tag.pictures();
            if !pics.is_empty() {
                match tag.tag_type() {
                    TagType::Id3v2 | TagType::Id3v1 => {
                        let mut apic_entries = Vec::new();
                        for pic in pics {
                            let mime = pic
                                .mime_type()
                                .map(|m| m.as_str())
                                .unwrap_or("image/jpeg");
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = pic.description().unwrap_or("");
                            let data = pic.data();
                            let offset = find_slice_offset(&file_bytes, data).unwrap_or(0);
                            let entry = if no_artwork {
                                serde_json::json!([mime, type_u8, desc, data.len(), offset])
                            } else {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                                serde_json::json!([mime, type_u8, desc, b64])
                            };
                            apic_entries.push(entry);
                        }
                        if apic_entries.len() == 1 {
                            tags_map.insert("APIC".to_string(), apic_entries.remove(0));
                        } else if !apic_entries.is_empty() {
                            tags_map.insert("APIC".to_string(), serde_json::json!(apic_entries));
                        }
                    }
                    TagType::VorbisComments => {
                        let mut pic_objects = Vec::new();
                        for pic in pics {
                            let mime = pic
                                .mime_type()
                                .map(|m| m.as_str())
                                .unwrap_or("image/jpeg");
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = pic.description().unwrap_or("");
                            let data = pic.data();
                            let offset = find_slice_offset(&file_bytes, data).unwrap_or(0);
                            let img_data_val = if no_artwork {
                                serde_json::json!(data.len())
                            } else {
                                serde_json::json!(base64::engine::general_purpose::STANDARD.encode(data))
                            };
                            let mut obj = serde_json::json!({
                                "mime_type": mime,
                                "picture_type": type_u8,
                                "description": desc,
                                "width": 0,
                                "height": 0,
                                "depth": 24,
                                "color_index": 0,
                                "image_data": img_data_val,
                            });
                            if let Some(map) = obj.as_object_mut() {
                                if offset > 0 {
                                    map.insert("offset".to_string(), serde_json::json!(offset));
                                }
                            }
                            pic_objects.push(obj);
                        }
                        tags_map.insert("ALLPICTURES".to_string(), serde_json::json!(pic_objects));
                    }
                    TagType::Mp4Ilst => {
                        if let Some(pic) = pics.first() {
                            let data = pic.data();
                            let offset = find_slice_offset(&file_bytes, data).unwrap_or(0);
                            if no_artwork {
                                tags_map.insert("COVR".to_string(), serde_json::json!(data.len()));
                            } else {
                                tags_map.insert(
                                    "COVR".to_string(),
                                    serde_json::json!(base64::engine::general_purpose::STANDARD.encode(data)),
                                );
                            }
                            tags_map.insert("COVR_offset".to_string(), serde_json::json!(offset));
                        }
                    }
                    TagType::Ape => {
                        if let Some(pic) = pics.first() {
                            let data = pic.data();
                            let offset = find_slice_offset(&file_bytes, data).unwrap_or(0);
                            if no_artwork {
                                tags_map.insert("COVER ART (FRONT)".to_string(), serde_json::json!(data.len()));
                            } else {
                                tags_map.insert(
                                    "COVER ART (FRONT)".to_string(),
                                    serde_json::json!(base64::engine::general_purpose::STANDARD.encode(data)),
                                );
                            }
                            tags_map.insert(
                                "COVER ART (FRONT)_offset".to_string(),
                                serde_json::json!(offset),
                            );
                        }
                    }
                    _ => {
                        let mut apic_entries = Vec::new();
                        for pic in pics {
                            let mime = pic
                                .mime_type()
                                .map(|m| m.as_str())
                                .unwrap_or("image/jpeg");
                            let type_u8 = pic_type_to_u8(pic.pic_type());
                            let desc = pic.description().unwrap_or("");
                            let data = pic.data();
                            let offset = find_slice_offset(&file_bytes, data).unwrap_or(0);
                            let entry = if no_artwork {
                                serde_json::json!([mime, type_u8, desc, data.len(), offset])
                            } else {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(data);
                                serde_json::json!([mime, type_u8, desc, b64])
                            };
                            apic_entries.push(entry);
                        }
                        if apic_entries.len() == 1 {
                            tags_map.insert("APIC".to_string(), apic_entries.remove(0));
                        } else if !apic_entries.is_empty() {
                            tags_map.insert("APIC".to_string(), serde_json::json!(apic_entries));
                        }
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

fn probe_audio_stream_props(path: &Path) -> (Option<u32>, Option<u32>) {
    if let Ok(tagged_file) = Probe::open(path).and_then(|p| p.read()) {
        let props = tagged_file.properties();
        return (props.sample_rate(), props.channels().map(|c| c as u32));
    }
    (None, None)
}

fn probe_duration_ms(path: &Path) -> u64 {
    if let Ok(tagged_file) = Probe::open(path).and_then(|p| p.read()) {
        return tagged_file.properties().duration().as_millis() as u64;
    }
    0
}

pub fn find_frame_offset(
    path: &Path,
    format_hint: Option<&str>,
    target_ms_or_neg_byte_offset: i64,
) -> Result<FrameSeekResult, AudioScanError> {
    let file_bytes = std::fs::read(path)?;
    let ext_str = format_hint
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()))
        .unwrap_or_else(|| "mp3".to_string());
    let ext = ext_str.as_str();

    let (sample_rate, channels) = probe_audio_stream_props(path);

    if target_ms_or_neg_byte_offset < 0 {
        let target_byte = target_ms_or_neg_byte_offset.unsigned_abs() as usize;
        match ext {
            "mp3" | "mp2" => {
                if let Some((pos, hdr)) = find_mp3_frame(&file_bytes, target_byte) {
                    return Ok(FrameSeekResult {
                        byte_offset: pos as i64,
                        seek_header: None,
                        samplerate: Some(hdr.sample_rate),
                        channels: Some(hdr.channels as u32),
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
                if let Some(pos) = find_flac_frame(&file_bytes, target_byte) {
                    return Ok(FrameSeekResult {
                        byte_offset: pos as i64,
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
            "ogg" | "oga" | "ogf" => {
                if let Some(pos) = find_ogg_page(&file_bytes, target_byte) {
                    return Ok(FrameSeekResult {
                        byte_offset: pos as i64,
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
                byte_offset: target_byte as i64,
                seek_header: None,
                samplerate: sample_rate,
                channels,
            }),
        }
    } else {
        let target_ms = target_ms_or_neg_byte_offset as u64;
        let audio_offset = compute_audio_offset(&file_bytes, path) as usize;
        if target_ms == 0 {
            return Ok(FrameSeekResult {
                byte_offset: audio_offset as i64,
                seek_header: None,
                samplerate: sample_rate,
                channels,
            });
        }

        let total_duration_ms = probe_duration_ms(path);
        let audio_size = file_bytes.len().saturating_sub(audio_offset);
        let est_byte = (target_ms * audio_size as u64)
            .checked_div(total_duration_ms)
            .map(|offset| audio_offset + offset as usize)
            .unwrap_or(audio_offset);
        let clamped_est = est_byte.min(file_bytes.len());

        match ext {
            "mp3" | "mp2" => {
                if let Some((pos, hdr)) = find_mp3_frame(&file_bytes, clamped_est) {
                    return Ok(FrameSeekResult {
                        byte_offset: pos as i64,
                        seek_header: None,
                        samplerate: Some(hdr.sample_rate),
                        channels: Some(hdr.channels as u32),
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: audio_offset as i64,
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            "flac" | "flc" | "fla" => {
                if let Some(pos) = find_flac_frame(&file_bytes, clamped_est) {
                    return Ok(FrameSeekResult {
                        byte_offset: pos as i64,
                        seek_header: None,
                        samplerate: sample_rate,
                        channels,
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: audio_offset as i64,
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            "ogg" | "oga" | "ogf" => {
                if let Some(pos) = find_ogg_page(&file_bytes, clamped_est) {
                    return Ok(FrameSeekResult {
                        byte_offset: pos as i64,
                        seek_header: None,
                        samplerate: sample_rate,
                        channels,
                    });
                }
                Ok(FrameSeekResult {
                    byte_offset: audio_offset as i64,
                    seek_header: None,
                    samplerate: sample_rate,
                    channels,
                })
            }
            _ => Ok(FrameSeekResult {
                byte_offset: clamped_est as i64,
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

    fn write_id3v2_mp3_with_apic(path: &std::path::Path, title: &str, artist: &str, jpeg: &[u8]) {
        use lofty::config::WriteOptions;
        use lofty::id3::v2::Id3v2Tag;
        use lofty::picture::{MimeType, Picture, PictureType};
        use lofty::prelude::*;

        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        std::fs::write(path, frame.repeat(20)).unwrap();

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
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn test_audioscan_no_artwork_env_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("tagged.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        write_id3v2_mp3_with_apic(&mp3_path, "Test Title", "Test Artist", &fake_jpeg);

        let res_no_art = scan_file(&mp3_path, true, true, true).unwrap();
        assert_eq!(res_no_art.tags.get("TIT2").unwrap().as_str().unwrap(), "Test Title");
        let apic = res_no_art.tags.get("APIC").unwrap().as_array().unwrap();
        assert_eq!(apic[3].as_u64().unwrap(), 10);
        assert!(apic[4].as_u64().unwrap() > 0);

        let res_with_art = scan_file(&mp3_path, true, true, false).unwrap();
        let apic_art = res_with_art.tags.get("APIC").unwrap().as_array().unwrap();
        assert!(apic_art[3].is_string());
    }

    #[test]
    fn test_audioscan_info_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("tagged_info.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        write_id3v2_mp3_with_apic(&mp3_path, "Info Test", "Artist Test", &fake_jpeg);

        let res = scan_file(&mp3_path, true, false, true).unwrap();
        assert!(res.tags.is_empty());
        assert_eq!(res.info.get("samplerate").unwrap().as_u64().unwrap(), 44100);
        assert_eq!(res.info.get("channels").unwrap().as_u64().unwrap(), 2);
        assert!(res.info.get("song_length_ms").unwrap().as_u64().unwrap() > 0);
        assert!(res.info.get("audio_offset").unwrap().as_u64().unwrap() > 0);
        assert!(res.info.get("audio_size").unwrap().as_u64().unwrap() > 0);
    }

    #[test]
    fn test_find_frame_offset_mp3() {
        let dir = tempfile::tempdir().unwrap();
        let mp3_path = dir.path().join("seek.mp3");
        let fake_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6];
        write_id3v2_mp3_with_apic(&mp3_path, "Seek Test", "Seek Artist", &fake_jpeg);

        let scan = scan_file(&mp3_path, true, false, true).unwrap();
        let audio_offset = scan.info.get("audio_offset").unwrap().as_i64().unwrap();

        // 1. Time 0 seek should point to first audio frame
        let seek_0 = find_frame_offset(&mp3_path, Some("mp3"), 0).unwrap();
        assert_eq!(seek_0.byte_offset, audio_offset);
        assert_eq!(seek_0.samplerate, Some(44100));

        // 2. Negative offset byte-seeking past first frame (audio_offset + 1)
        let seek_neg = find_frame_offset(&mp3_path, Some("mp3"), -(audio_offset + 1)).unwrap();
        assert_eq!(seek_neg.byte_offset, audio_offset + 417);
    }
}
