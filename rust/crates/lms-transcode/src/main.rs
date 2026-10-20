#![deny(unsafe_op_in_unsafe_fn)]

use std::io::{Read, Seek, Write};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::formats::SeekTo;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::probe::Hint;
use thiserror::Error;

struct SourceWrapper<R>(R);

impl<R: Read + Seek + Send + Sync> Read for SourceWrapper<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R: Read + Seek + Send + Sync> Seek for SourceWrapper<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for SourceWrapper<R> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[derive(Error, Debug)]
pub enum TranscodeError {
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Symphonia error: {0}")]
    Symphonia(#[from] symphonia::core::errors::Error),
    #[error("Resample error: {0}")]
    Resample(String),
    #[error("Transcode error: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputContainer {
    Wav,
    RawPcmLe,
    RawPcmBe,
    Flac,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeJob {
    pub input_path: Option<String>,
    pub output_format: OutputContainer,
    pub start_sec: Option<f64>,
    pub end_sec: Option<f64>,
    pub target_sample_rate: Option<u32>,
    pub target_channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
}

pub fn parse_time_offset(val: &str) -> Result<f64, TranscodeError> {
    let s = val.trim();
    if s.is_empty() {
        return Err(TranscodeError::Parse("empty time offset".into()));
    }
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => {
            let sec = parts[0]
                .parse::<f64>()
                .map_err(|e| TranscodeError::Parse(format!("invalid seconds '{}': {}", parts[0], e)))?;
            if sec < 0.0 {
                return Err(TranscodeError::Parse("seconds cannot be negative".into()));
            }
            Ok(sec)
        }
        2 => {
            let m = parts[0]
                .parse::<f64>()
                .map_err(|e| TranscodeError::Parse(format!("invalid minutes '{}': {}", parts[0], e)))?;
            let sec = parts[1]
                .parse::<f64>()
                .map_err(|e| TranscodeError::Parse(format!("invalid seconds '{}': {}", parts[1], e)))?;
            if m < 0.0 || sec < 0.0 {
                return Err(TranscodeError::Parse("time offset components cannot be negative".into()));
            }
            Ok(m * 60.0 + sec)
        }
        3 => {
            let h = parts[0]
                .parse::<f64>()
                .map_err(|e| TranscodeError::Parse(format!("invalid hours '{}': {}", parts[0], e)))?;
            let m = parts[1]
                .parse::<f64>()
                .map_err(|e| TranscodeError::Parse(format!("invalid minutes '{}': {}", parts[1], e)))?;
            let sec = parts[2]
                .parse::<f64>()
                .map_err(|e| TranscodeError::Parse(format!("invalid seconds '{}': {}", parts[2], e)))?;
            if h < 0.0 || m < 0.0 || sec < 0.0 {
                return Err(TranscodeError::Parse("time offset components cannot be negative".into()));
            }
            Ok(h * 3600.0 + m * 60.0 + sec)
        }
        _ => Err(TranscodeError::Parse(format!("unrecognized time format '{}'", s))),
    }
}

pub fn parse_cli_invocation(argv0: &str, args: &[String]) -> Result<TranscodeJob, TranscodeError> {
    let binary_name = std::path::Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);

    let mut mode_override: Option<String> = None;
    let mut filtered_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--mode" && i + 1 < args.len() {
            mode_override = Some(args[i + 1].clone());
            i += 2;
        } else if let Some(val) = args[i].strip_prefix("--mode=") {
            mode_override = Some(val.to_string());
            i += 1;
        } else {
            filtered_args.push(args[i].clone());
            i += 1;
        }
    }

    let mode = if let Some(m) = mode_override {
        m
    } else if binary_name.contains("faad") {
        "faad".to_string()
    } else if binary_name.contains("flac") {
        "flac".to_string()
    } else if binary_name.contains("wvunpack") {
        "wvunpack".to_string()
    } else if binary_name.contains("sox") {
        "sox".to_string()
    } else if filtered_args
        .iter()
        .any(|a| a == "-j" || a == "-e" || (a == "-w" && filtered_args.iter().any(|b| b == "-f")))
    {
        "faad".to_string()
    } else if filtered_args.iter().any(|a| {
        a.starts_with("--skip") || a.starts_with("--until") || a.contains("force-raw-format")
    }) {
        "flac".to_string()
    } else if filtered_args
        .iter()
        .any(|a| a == "-wq" || a == "-rq" || a == "--dsf" || a == "--dff")
    {
        "wvunpack".to_string()
    } else {
        "sox".to_string()
    };

    match mode.as_str() {
        "faad" => parse_faad(&filtered_args),
        "flac" => parse_flac(&filtered_args),
        "wvunpack" => parse_wvunpack(&filtered_args),
        "sox" => parse_sox(&filtered_args),
        other => Err(TranscodeError::Parse(format!("unrecognized transcoder mode '{}'", other))),
    }
}

fn parse_faad(args: &[String]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut bits_per_sample = None;
    let mut input_path = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-q" => i += 1,
            "-w" => {
                if output_format != OutputContainer::RawPcmLe {
                    output_format = OutputContainer::Wav;
                }
                i += 1;
            }
            "-f" => {
                if i + 1 < args.len() {
                    match args[i + 1].as_str() {
                        "1" => output_format = OutputContainer::Wav,
                        "2" => output_format = OutputContainer::RawPcmLe,
                        _ => {}
                    }
                    i += 2;
                } else {
                    return Err(TranscodeError::Parse("missing argument to -f".into()));
                }
            }
            "-b" => {
                if i + 1 < args.len() {
                    bits_per_sample = match args[i + 1].as_str() {
                        "1" => Some(16),
                        "2" => Some(24),
                        "3" => Some(32),
                        other => other.parse::<u16>().ok(),
                    };
                    i += 2;
                } else {
                    return Err(TranscodeError::Parse("missing argument to -b".into()));
                }
            }
            "-j" => {
                if i + 1 < args.len() {
                    start_sec = Some(parse_time_offset(&args[i + 1])?);
                    i += 2;
                } else {
                    return Err(TranscodeError::Parse("missing argument to -j".into()));
                }
            }
            "-e" => {
                if i + 1 < args.len() {
                    end_sec = Some(parse_time_offset(&args[i + 1])?);
                    i += 2;
                } else {
                    return Err(TranscodeError::Parse("missing argument to -e".into()));
                }
            }
            _ if arg.starts_with("-j") => {
                start_sec = Some(parse_time_offset(&arg[2..])?);
                i += 1;
            }
            _ if arg.starts_with("-e") => {
                end_sec = Some(parse_time_offset(&arg[2..])?);
                i += 1;
            }
            _ if !arg.starts_with('-') || arg == "-" => {
                if arg == "-" {
                    input_path = None;
                } else {
                    input_path = Some(arg.clone());
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    Ok(TranscodeJob {
        input_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate: None,
        target_channels: None,
        bits_per_sample,
    })
}

fn parse_flac(args: &[String]) -> Result<TranscodeJob, TranscodeError> {
    let mut is_encode = false;
    let mut force_raw = false;
    let mut endian_big = false;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut bits_per_sample = None;
    let mut target_sample_rate = None;
    let mut target_channels = None;
    let mut input_path = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            if i < args.len() {
                if args[i] != "-" {
                    input_path = Some(args[i].clone());
                }
                i += 1;
            }
            continue;
        }

        if let Some(val) = arg.strip_prefix("--skip=") {
            start_sec = Some(parse_time_offset(val)?);
            i += 1;
            continue;
        }
        if arg == "--skip" && i + 1 < args.len() {
            start_sec = Some(parse_time_offset(&args[i + 1])?);
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--until=") {
            end_sec = Some(parse_time_offset(val)?);
            i += 1;
            continue;
        }
        if arg == "--until" && i + 1 < args.len() {
            end_sec = Some(parse_time_offset(&args[i + 1])?);
            i += 2;
            continue;
        }
        if arg == "--force-raw-format" {
            force_raw = true;
            i += 1;
            continue;
        }
        if arg == "--endian=little"
            || (arg == "--endian" && i + 1 < args.len() && args[i + 1] == "little")
        {
            endian_big = false;
            i += if arg == "--endian" { 2 } else { 1 };
            continue;
        }
        if arg == "--endian=big"
            || (arg == "--endian" && i + 1 < args.len() && args[i + 1] == "big")
        {
            endian_big = true;
            i += if arg == "--endian" { 2 } else { 1 };
            continue;
        }
        if let Some(val) = arg.strip_prefix("--bps=") {
            bits_per_sample = val.parse().ok();
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--sample-rate=") {
            target_sample_rate = val.parse().ok();
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--channels=") {
            target_channels = val.parse().ok();
            i += 1;
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") {
            if arg.contains('c') && !arg.contains('d') {
                is_encode = true;
            }
            i += 1;
            continue;
        }
        if arg.starts_with("--compression-level") {
            is_encode = true;
            i += 1;
            continue;
        }
        if !arg.starts_with('-') || arg == "-" {
            if arg == "-" {
                input_path = None;
            } else {
                input_path = Some(arg.clone());
            }
            i += 1;
            continue;
        }
        i += 1;
    }

    let output_format = if force_raw {
        if endian_big {
            OutputContainer::RawPcmBe
        } else {
            OutputContainer::RawPcmLe
        }
    } else if is_encode {
        OutputContainer::Flac
    } else {
        OutputContainer::Wav
    };

    Ok(TranscodeJob {
        input_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate,
        target_channels,
        bits_per_sample,
    })
}

fn parse_wvunpack(args: &[String]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut input_path = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-w" | "-wq" => {
                output_format = OutputContainer::Wav;
                i += 1;
            }
            "-r" | "-rq" => {
                output_format = OutputContainer::RawPcmLe;
                i += 1;
            }
            "-q" | "--dsf" | "--dff" => i += 1,
            "-o" => {
                i += 2;
            }
            _ if arg.starts_with("-o") => i += 1,
            _ if arg.starts_with("--skip=") => {
                start_sec = Some(parse_time_offset(&arg["--skip=".len()..])?);
                i += 1;
            }
            _ if arg.starts_with("--until=") => {
                end_sec = Some(parse_time_offset(&arg["--until=".len()..])?);
                i += 1;
            }
            "-s" | "-ss" => {
                if i + 1 < args.len() {
                    start_sec = Some(parse_time_offset(&args[i + 1])?);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "-u" => {
                if i + 1 < args.len() {
                    end_sec = Some(parse_time_offset(&args[i + 1])?);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ if !arg.starts_with('-') || arg == "-" => {
                if input_path.is_none() {
                    if arg == "-" {
                        input_path = None;
                    } else {
                        input_path = Some(arg.clone());
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    Ok(TranscodeJob {
        input_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate: None,
        target_channels: None,
        bits_per_sample: None,
    })
}

fn parse_sox(args: &[String]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut target_sample_rate = None;
    let mut target_channels = None;
    let mut bits_per_sample = None;
    let mut input_path = None;
    let mut endian_big = false;

    let mut i = 0;
    let mut non_option_args = Vec::new();
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-t" | "-r" | "-c" | "-b" | "-C" | "--encoding" | "-e" | "--endian" => {
                i += 2;
            }
            _ if arg.starts_with('-') => {
                i += 1;
            }
            _ => {
                non_option_args.push((i, arg.clone()));
                i += 1;
            }
        }
    }

    let in_idx = non_option_args.first().map(|p| p.0);
    if let Some(idx) = in_idx {
        let s = &args[idx];
        if s != "-" {
            input_path = Some(s.clone());
        }
    }

    let search_start = in_idx.map(|idx| idx + 1).unwrap_or(0);
    let mut j = search_start;
    while j < args.len() {
        let arg = &args[j];
        match arg.as_str() {
            "-t" => {
                if j + 1 < args.len() {
                    match args[j + 1].as_str() {
                        "wav" => output_format = OutputContainer::Wav,
                        "flac" => output_format = OutputContainer::Flac,
                        "raw" => {
                            output_format = if endian_big {
                                OutputContainer::RawPcmBe
                            } else {
                                OutputContainer::RawPcmLe
                            };
                        }
                        _ => {}
                    }
                    j += 2;
                } else {
                    j += 1;
                }
            }
            "-r" => {
                if j + 1 < args.len() {
                    target_sample_rate = args[j + 1].parse().ok();
                    j += 2;
                } else {
                    j += 1;
                }
            }
            "-c" => {
                if j + 1 < args.len() {
                    target_channels = args[j + 1].parse().ok();
                    j += 2;
                } else {
                    j += 1;
                }
            }
            "-b" => {
                if j + 1 < args.len() {
                    bits_per_sample = args[j + 1].parse().ok();
                    j += 2;
                } else {
                    j += 1;
                }
            }
            "-1" => {
                bits_per_sample = Some(8);
                j += 1;
            }
            "-2" => {
                bits_per_sample = Some(16);
                j += 1;
            }
            "-3" => {
                bits_per_sample = Some(24);
                j += 1;
            }
            "-4" => {
                bits_per_sample = Some(32);
                j += 1;
            }
            "--endian" => {
                if j + 1 < args.len() {
                    endian_big = args[j + 1] == "big";
                    if output_format == OutputContainer::RawPcmLe
                        || output_format == OutputContainer::RawPcmBe
                    {
                        output_format = if endian_big {
                            OutputContainer::RawPcmBe
                        } else {
                            OutputContainer::RawPcmLe
                        };
                    }
                    j += 2;
                } else {
                    j += 1;
                }
            }
            "-x" | "$-x$" => {
                endian_big = true;
                if output_format == OutputContainer::RawPcmLe
                    || output_format == OutputContainer::RawPcmBe
                {
                    output_format = OutputContainer::RawPcmBe;
                }
                j += 1;
            }
            "trim" => {
                if j + 1 < args.len() {
                    let s = parse_time_offset(&args[j + 1])?;
                    start_sec = Some(s);
                    if j + 2 < args.len() && !args[j + 2].starts_with('-') {
                        let end_str = &args[j + 2];
                        if let Some(val) = end_str.strip_prefix('=') {
                            end_sec = Some(parse_time_offset(val)?);
                        } else if let Ok(dur) = parse_time_offset(end_str) {
                            end_sec = Some(s + dur);
                        }
                        j += 3;
                    } else {
                        j += 2;
                    }
                } else {
                    j += 1;
                }
            }
            "rate" => {
                if j + 1 < args.len() {
                    target_sample_rate = args[j + 1].parse().ok();
                    j += 2;
                } else {
                    j += 1;
                }
            }
            _ => j += 1,
        }
    }

    Ok(TranscodeJob {
        input_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate,
        target_channels,
        bits_per_sample,
    })
}

fn flac_crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            if (crc & 0x80) != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn flac_crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x8005;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn write_flac_header<W: Write>(
    out: &mut W,
    sample_rate: u32,
    channels: u16,
    bps: u16,
    total_samples: u64,
) -> Result<(), TranscodeError> {
    out.write_all(b"fLaC")?;
    let mut block = Vec::with_capacity(38);
    block.push(0x80);
    block.push(0x00);
    block.push(0x00);
    block.push(0x22);
    block.extend_from_slice(&4096u16.to_be_bytes());
    block.extend_from_slice(&4096u16.to_be_bytes());
    block.extend_from_slice(&[0, 0, 0]);
    block.extend_from_slice(&[0, 0, 0]);
    let sr = sample_rate as u64;
    let ch = ((channels - 1) & 0x07) as u64;
    let b = ((bps - 1) & 0x1F) as u64;
    let tot = total_samples & 0x0000_000F_FFFF_FFFF;
    let packed = (sr << 44) | (ch << 41) | (b << 36) | tot;
    block.extend_from_slice(&packed.to_be_bytes());
    block.extend_from_slice(&[0u8; 16]);
    out.write_all(&block)?;
    Ok(())
}

fn write_flac_frame<W: Write>(
    out: &mut W,
    frame_idx: u32,
    block_len: usize,
    channels: u16,
    bps: u16,
    channel_samples: &[Vec<i32>],
) -> Result<(), TranscodeError> {
    let mut header = Vec::with_capacity(16);
    header.push(0xFF);
    header.push(0xF8);
    let bs_code = if block_len == 4096 { 0x0Cu8 } else { 0x07u8 };
    header.push(bs_code << 4);
    let ch_code = ((channels - 1) as u8) & 0x0F;
    header.push(ch_code << 4);
    if frame_idx < 0x80 {
        header.push(frame_idx as u8);
    } else if frame_idx < 0x800 {
        header.push(0xC0 | ((frame_idx >> 6) as u8));
        header.push(0x80 | ((frame_idx & 0x3F) as u8));
    } else if frame_idx < 0x10000 {
        header.push(0xE0 | ((frame_idx >> 12) as u8));
        header.push(0x80 | (((frame_idx >> 6) & 0x3F) as u8));
        header.push(0x80 | ((frame_idx & 0x3F) as u8));
    } else {
        header.push(0xF0 | ((frame_idx >> 18) as u8));
        header.push(0x80 | (((frame_idx >> 12) & 0x3F) as u8));
        header.push(0x80 | (((frame_idx >> 6) & 0x3F) as u8));
        header.push(0x80 | ((frame_idx & 0x3F) as u8));
    }
    if bs_code == 0x07 {
        header.extend_from_slice(&((block_len - 1) as u16).to_be_bytes());
    }
    let crc8 = flac_crc8(&header);
    header.push(crc8);

    let mut frame_bytes = header;
    for samples in channel_samples.iter().take(channels as usize) {
        frame_bytes.push(0x02);
        for &s in &samples[..block_len] {
            match bps {
                16 => {
                    frame_bytes.extend_from_slice(&(s as i16).to_be_bytes());
                }
                24 => {
                    let b = s.to_be_bytes();
                    frame_bytes.push(b[1]);
                    frame_bytes.push(b[2]);
                    frame_bytes.push(b[3]);
                }
                32 => {
                    frame_bytes.extend_from_slice(&s.to_be_bytes());
                }
                8 => {
                    frame_bytes.push(s as u8);
                }
                _ => {
                    frame_bytes.extend_from_slice(&(s as i16).to_be_bytes());
                }
            }
        }
    }

    let crc16 = flac_crc16(&frame_bytes);
    frame_bytes.extend_from_slice(&crc16.to_be_bytes());
    out.write_all(&frame_bytes)?;
    Ok(())
}

fn write_wav_header<W: Write>(
    out: &mut W,
    sample_rate: u32,
    channels: u16,
    bps: u16,
    total_frames: Option<u64>,
) -> Result<(), TranscodeError> {
    let (riff_size, data_size) = match total_frames {
        Some(frames) => {
            let data_bytes = frames * channels as u64 * (bps as u64 / 8);
            if data_bytes > u32::MAX as u64 - 36 {
                (0xFFFF_FFFFu32, 0xFFFF_FFFFu32)
            } else {
                ((data_bytes + 36) as u32, data_bytes as u32)
            }
        }
        None => (0xFFFF_FFFFu32, 0xFFFF_FFFFu32),
    };

    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size.to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * channels as u32 * (bps as u32 / 8);
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    let block_align = channels * (bps / 8);
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bps.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
    out.write_all(&header)?;
    Ok(())
}

struct ContainerWriter<'a, W: Write> {
    out: &'a mut W,
    format: OutputContainer,
    channels: u16,
    bps: u16,
    flac_frame_idx: &'a mut u32,
    flac_pending: &'a mut [Vec<i32>],
}

impl<'a, W: Write> ContainerWriter<'a, W> {
    fn write_samples(
        &mut self,
        samples_planar: &[Vec<i32>],
        num_frames: usize,
    ) -> Result<(), TranscodeError> {
        match self.format {
            OutputContainer::RawPcmLe | OutputContainer::Wav => {
                let mut buf =
                    Vec::with_capacity(num_frames * self.channels as usize * (self.bps as usize / 8));
                for f in 0..num_frames {
                    for ch_samples in samples_planar.iter().take(self.channels as usize) {
                        let s = ch_samples[f];
                        match self.bps {
                            16 => buf.extend_from_slice(&(s as i16).to_le_bytes()),
                            24 => {
                                let b = s.to_le_bytes();
                                buf.extend_from_slice(&b[0..3]);
                            }
                            32 => buf.extend_from_slice(&s.to_le_bytes()),
                            8 => buf.push(s as u8),
                            _ => buf.extend_from_slice(&(s as i16).to_le_bytes()),
                        }
                    }
                }
                self.out.write_all(&buf)?;
            }
            OutputContainer::RawPcmBe => {
                let mut buf =
                    Vec::with_capacity(num_frames * self.channels as usize * (self.bps as usize / 8));
                for f in 0..num_frames {
                    for ch_samples in samples_planar.iter().take(self.channels as usize) {
                        let s = ch_samples[f];
                        match self.bps {
                            16 => buf.extend_from_slice(&(s as i16).to_be_bytes()),
                            24 => {
                                let b = s.to_be_bytes();
                                buf.extend_from_slice(&b[1..4]);
                            }
                            32 => buf.extend_from_slice(&s.to_be_bytes()),
                            8 => buf.push(s as u8),
                            _ => buf.extend_from_slice(&(s as i16).to_be_bytes()),
                        }
                    }
                }
                self.out.write_all(&buf)?;
            }
            OutputContainer::Flac => {
                for (ch, ch_samples) in samples_planar.iter().enumerate().take(self.channels as usize) {
                    self.flac_pending[ch].extend_from_slice(&ch_samples[..num_frames]);
                }
                while self.flac_pending[0].len() >= 4096 {
                    write_flac_frame(
                        self.out,
                        *self.flac_frame_idx,
                        4096,
                        self.channels,
                        self.bps,
                        self.flac_pending,
                    )?;
                    *self.flac_frame_idx += 1;
                    for ch_buf in self.flac_pending.iter_mut().take(self.channels as usize) {
                        ch_buf.drain(0..4096);
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn execute_transcode<R: Read + Seek + Send + Sync + 'static, W: Write>(
    job: &TranscodeJob,
    input: R,
    mut output: W,
) -> Result<(), TranscodeError> {
    let media_source = Box::new(SourceWrapper(input));
    let mss = MediaSourceStream::new(media_source, Default::default());
    let hint = Hint::new();
    let meta_opts = Default::default();
    let fmt_opts = Default::default();
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .map_err(TranscodeError::Symphonia)?;
    let mut format = probed.format;

    let (track_id, src_sample_rate, src_channels, codec_params, track_n_frames) = {
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| TranscodeError::Other("no supported audio track found".into()))?;
        let track_id = track.id;
        let src_sample_rate = track
            .codec_params
            .sample_rate
            .ok_or_else(|| TranscodeError::Other("unknown sample rate".into()))?;
        let src_channels = track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2);
        let codec_params = track.codec_params.clone();
        let n_frames = track.codec_params.n_frames;
        (track_id, src_sample_rate, src_channels, codec_params, n_frames)
    };

    let out_channels = job.target_channels.unwrap_or(src_channels);
    let out_sample_rate = job.target_sample_rate.unwrap_or(src_sample_rate);
    let out_bps = job.bits_per_sample.unwrap_or(16);

    let dec_opts = Default::default();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &dec_opts)
        .map_err(TranscodeError::Symphonia)?;

    let start_frame: u64 = job
        .start_sec
        .map(|s| (s.max(0.0) * src_sample_rate as f64).round() as u64)
        .unwrap_or(0);
    let end_frame: u64 = job
        .end_sec
        .map(|s| (s.max(0.0) * src_sample_rate as f64).round() as u64)
        .unwrap_or(u64::MAX);

    if end_frame <= start_frame {
        if job.output_format == OutputContainer::Wav {
            write_wav_header(&mut output, out_sample_rate, out_channels, out_bps, Some(0))?;
        } else if job.output_format == OutputContainer::Flac {
            write_flac_header(&mut output, out_sample_rate, out_channels, out_bps, 0)?;
        }
        return Ok(());
    }

    let mut current_frame_pos: u64 = 0;
    if start_frame > 0 {
        if let Ok(seeked_to) = format.seek(
            symphonia::core::formats::SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: start_frame,
                track_id,
            },
        ) {
            current_frame_pos = seeked_to.actual_ts;
            decoder.reset();
        }
    }

    let total_frames_hint = if end_frame != u64::MAX {
        Some(end_frame - start_frame)
    } else {
        track_n_frames.map(|n| n.saturating_sub(start_frame))
    };
    let total_out_frames_hint = total_frames_hint.map(|f| {
        if out_sample_rate != src_sample_rate {
            (f as f64 * (out_sample_rate as f64 / src_sample_rate as f64)).round() as u64
        } else {
            f
        }
    });

    if job.output_format == OutputContainer::Wav {
        write_wav_header(
            &mut output,
            out_sample_rate,
            out_channels,
            out_bps,
            total_out_frames_hint,
        )?;
    } else if job.output_format == OutputContainer::Flac {
        write_flac_header(
            &mut output,
            out_sample_rate,
            out_channels,
            out_bps,
            total_out_frames_hint.unwrap_or(0),
        )?;
    }

    let needs_resample = out_sample_rate != src_sample_rate;
    let resample_chunk_size = 1024usize;
    let mut resampler = if needs_resample {
        use rubato::{FastFixedIn, PolynomialDegree};
        let ratio = out_sample_rate as f64 / src_sample_rate as f64;
        Some(
            FastFixedIn::<f32>::new(
                ratio,
                1.0,
                PolynomialDegree::Linear,
                resample_chunk_size,
                out_channels as usize,
            )
            .map_err(|e| TranscodeError::Resample(e.to_string()))?,
        )
    } else {
        None
    };

    let mut resample_pending_in = vec![Vec::<f32>::new(); out_channels as usize];
    let mut flac_frame_idx = 0u32;
    let mut flac_pending = vec![Vec::<i32>::new(); out_channels as usize];

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut sample_buf_i16: Option<SampleBuffer<i16>> = None;

    let direct_i16 = !needs_resample
        && src_channels == out_channels
        && out_bps == 16
        && matches!(
            job.output_format,
            OutputContainer::RawPcmLe | OutputContainer::RawPcmBe
        );

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(TranscodeError::Symphonia(e)),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(TranscodeError::Symphonia(e)),
        };

        let n_frames = decoded.frames();
        if n_frames == 0 {
            continue;
        }

        let pkt_start = if packet.ts() > 0 {
            packet.ts()
        } else {
            current_frame_pos
        };
        let pkt_end = pkt_start + n_frames as u64;
        current_frame_pos = pkt_end;

        if pkt_end <= start_frame {
            continue;
        }
        if pkt_start >= end_frame {
            break;
        }

        let slice_start = if start_frame > pkt_start {
            (start_frame - pkt_start) as usize
        } else {
            0
        };
        let slice_end = if end_frame < pkt_end {
            (end_frame - pkt_start) as usize
        } else {
            n_frames
        };

        if slice_start >= slice_end {
            continue;
        }
        let sliced_frames = slice_end - slice_start;

        if direct_i16 {
            let buf = sample_buf_i16.get_or_insert_with(|| {
                SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec())
            });
            buf.copy_interleaved_ref(decoded);
            let samples = buf.samples();
            let c = src_channels as usize;
            let mut out_bytes = Vec::with_capacity(sliced_frames * c * 2);
            for f in slice_start..slice_end {
                for ch in 0..c {
                    let s = samples[f * c + ch];
                    if job.output_format == OutputContainer::RawPcmLe {
                        out_bytes.extend_from_slice(&s.to_le_bytes());
                    } else {
                        out_bytes.extend_from_slice(&s.to_be_bytes());
                    }
                }
            }
            output.write_all(&out_bytes)?;
        } else {
            let buf = sample_buf.get_or_insert_with(|| {
                SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec())
            });
            buf.copy_interleaved_ref(decoded);
            let samples = buf.samples();
            let sc = src_channels as usize;
            let tc = out_channels as usize;

            let mut mapped_planar = vec![Vec::with_capacity(sliced_frames); tc];
            for f in slice_start..slice_end {
                if sc == tc {
                    for ch in 0..tc {
                        mapped_planar[ch].push(samples[f * sc + ch]);
                    }
                } else if sc == 2 && tc == 1 {
                    let m = (samples[f * 2] + samples[f * 2 + 1]) * 0.5;
                    mapped_planar[0].push(m);
                } else if sc == 1 && tc == 2 {
                    let m = samples[f];
                    mapped_planar[0].push(m);
                    mapped_planar[1].push(m);
                } else {
                    for ch in 0..tc {
                        let s = if ch < sc { samples[f * sc + ch] } else { 0.0 };
                        mapped_planar[ch].push(s);
                    }
                }
            }

            if let Some(ref mut res) = resampler {
                use rubato::Resampler;
                for ch in 0..tc {
                    resample_pending_in[ch].extend_from_slice(&mapped_planar[ch]);
                }
                while resample_pending_in[0].len() >= resample_chunk_size {
                    let mut chunk_in = vec![Vec::with_capacity(resample_chunk_size); tc];
                    for ch in 0..tc {
                        chunk_in[ch].extend(resample_pending_in[ch].drain(0..resample_chunk_size));
                    }
                    let chunk_out = res
                        .process(&chunk_in, None)
                        .map_err(|e| TranscodeError::Resample(e.to_string()))?;
                    let out_len = chunk_out[0].len();
                    let int_samples = convert_f32_to_int(&chunk_out, out_bps);
                    let mut writer = ContainerWriter {
                        out: &mut output,
                        format: job.output_format,
                        channels: out_channels,
                        bps: out_bps,
                        flac_frame_idx: &mut flac_frame_idx,
                        flac_pending: &mut flac_pending,
                    };
                    writer.write_samples(&int_samples, out_len)?;
                }
            } else {
                let int_samples = convert_f32_to_int(&mapped_planar, out_bps);
                let mut writer = ContainerWriter {
                    out: &mut output,
                    format: job.output_format,
                    channels: out_channels,
                    bps: out_bps,
                    flac_frame_idx: &mut flac_frame_idx,
                    flac_pending: &mut flac_pending,
                };
                writer.write_samples(&int_samples, sliced_frames)?;
            }
        }
    }

    if let Some(ref mut res) = resampler {
        use rubato::Resampler;
        let rem = resample_pending_in[0].len();
        if rem > 0 {
            let tc = out_channels as usize;
            let mut padded = resample_pending_in;
            for ch_buf in padded.iter_mut().take(tc) {
                ch_buf.resize(resample_chunk_size, 0.0);
            }
            let chunk_out = res
                .process(&padded, None)
                .map_err(|e| TranscodeError::Resample(e.to_string()))?;
            let ratio = out_sample_rate as f64 / src_sample_rate as f64;
            let valid_out = ((rem as f64 * ratio).round() as usize).min(chunk_out[0].len());
            let int_samples = convert_f32_to_int(&chunk_out, out_bps);
            let mut writer = ContainerWriter {
                out: &mut output,
                format: job.output_format,
                channels: out_channels,
                bps: out_bps,
                flac_frame_idx: &mut flac_frame_idx,
                flac_pending: &mut flac_pending,
            };
            writer.write_samples(&int_samples, valid_out)?;
        }
    }

    if job.output_format == OutputContainer::Flac && !flac_pending[0].is_empty() {
        let rem = flac_pending[0].len();
        write_flac_frame(
            &mut output,
            flac_frame_idx,
            rem,
            out_channels,
            out_bps,
            &flac_pending,
        )?;
    }

    Ok(())
}

fn convert_f32_to_int(planar: &[Vec<f32>], bps: u16) -> Vec<Vec<i32>> {
    let tc = planar.len();
    let num_frames = planar[0].len();
    let mut out = vec![Vec::with_capacity(num_frames); tc];
    for ch in 0..tc {
        for &sample in &planar[ch] {
            let clamped = sample.clamp(-1.0, 1.0);
            let int_val = match bps {
                16 => (clamped * 32767.0).round().clamp(-32768.0, 32767.0) as i32,
                24 => (clamped * 8388607.0).round().clamp(-8388608.0, 8388607.0) as i32,
                32 => (clamped * 2147483647.0)
                    .round()
                    .clamp(-2147483648.0, 2147483647.0) as i32,
                8 => (clamped * 127.0).round().clamp(-128.0, 127.0) as i32,
                _ => (clamped * 32767.0).round().clamp(-32768.0, 32767.0) as i32,
            };
            out[ch].push(int_val);
        }
    }
    out
}

fn run(argv0: &str, args: &[String]) -> Result<(), TranscodeError> {
    let job = parse_cli_invocation(argv0, args)?;
    let stdout = std::io::stdout();
    let mut out_handle = std::io::BufWriter::new(stdout.lock());

    if let Some(ref path) = job.input_path {
        let file = std::fs::File::open(path)?;
        execute_transcode(&job, file, &mut out_handle)?;
    } else {
        let mut stdin = std::io::stdin();
        let mut temp = tempfile::tempfile()?;
        std::io::copy(&mut stdin, &mut temp)?;
        temp.rewind()?;
        execute_transcode(&job, temp, &mut out_handle)?;
    }

    out_handle.flush()?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(|s| s.as_str()).unwrap_or("lms-transcode");
    let cli_args = if args.len() > 1 { &args[1..] } else { &[] };

    if let Err(e) = run(argv0, cli_args) {
        eprintln!("lms-transcode error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_test_wav_44100_stereo(seconds: f64) -> Vec<u8> {
        let frames = (44100.0 * seconds) as u32;
        let data_bytes = frames * 2 * 2;
        let mut buf = Vec::with_capacity(44 + data_bytes as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        buf.extend_from_slice(b"WAVEfmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
        buf.extend_from_slice(&2u16.to_le_bytes()); // 2 channels
        buf.extend_from_slice(&44100u32.to_le_bytes());
        buf.extend_from_slice(&(44100u32 * 4).to_le_bytes());
        buf.extend_from_slice(&4u16.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());
        for i in 0..(frames * 2) {
            let sample = ((i % 100) as i16) * 100;
            buf.extend_from_slice(&sample.to_le_bytes());
        }
        buf
    }

    #[test]
    fn test_parse_time_offset_seconds_and_mm_ss_frac() {
        assert!((parse_time_offset("12.5").unwrap() - 12.5).abs() < 1e-6);
        assert!((parse_time_offset("1:05.50").unwrap() - 65.5).abs() < 1e-6);
        assert!((parse_time_offset("02:03:04.25").unwrap() - 7384.25).abs() < 1e-6);
    }

    #[test]
    fn test_parse_convert_conf_flag_combinations() {
        let job_faad = parse_cli_invocation(
            "faad",
            &[
                "-q", "-w", "-f", "2", "-b", "1", "-j", "10", "-e", "30", "/tmp/in.m4a",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job_faad.output_format, OutputContainer::RawPcmLe);
        assert_eq!(job_faad.start_sec, Some(10.0));
        assert_eq!(job_faad.end_sec, Some(30.0));

        let job_flac = parse_cli_invocation(
            "flac",
            &[
                "-dcs",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
                "--skip=0:15.25",
                "--until=1:00.00",
                "--",
                "/tmp/in.flac",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job_flac.output_format, OutputContainer::RawPcmLe);
        assert_eq!(job_flac.start_sec, Some(15.25));
        assert_eq!(job_flac.end_sec, Some(60.0));
    }

    #[test]
    fn test_wav_to_pcm_and_seek_slicing_roundtrip() {
        let wav_bytes = generate_test_wav_44100_stereo(1.0);
        let job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: Some(0.25),
            end_sec: Some(0.75),
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        assert_eq!(out.len(), 88200);
    }

    #[test]
    fn test_wvunpack_and_sox_parsing() {
        let job_wv = parse_cli_invocation(
            "wvunpack",
            &["/tmp/in.wv", "-rq", "--skip=10", "--until=20", "-o", "-"]
                .map(String::from),
        )
        .unwrap();
        assert_eq!(job_wv.output_format, OutputContainer::RawPcmLe);
        assert_eq!(job_wv.input_path, Some("/tmp/in.wv".to_string()));
        assert_eq!(job_wv.start_sec, Some(10.0));
        assert_eq!(job_wv.end_sec, Some(20.0));

        let job_sox = parse_cli_invocation(
            "sox",
            &[
                "-q",
                "-t",
                "ogg",
                "/tmp/in.ogg",
                "-t",
                "flac",
                "-C",
                "0",
                "-",
                "trim",
                "0:10",
                "=0:40",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job_sox.output_format, OutputContainer::Flac);
        assert_eq!(job_sox.input_path, Some("/tmp/in.ogg".to_string()));
        assert_eq!(job_sox.start_sec, Some(10.0));
        assert_eq!(job_sox.end_sec, Some(40.0));
    }

    #[test]
    fn test_flac_encoding_and_symphonia_roundtrip() {
        let wav_bytes = generate_test_wav_44100_stereo(0.2);
        let job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::Flac,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
        };
        let mut flac_out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut flac_out).unwrap();
        assert!(flac_out.starts_with(b"fLaC"));

        // Roundtrip verify by decoding the generated FLAC with Symphonia
        let decode_job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
        };
        let mut pcm_out = Vec::new();
        execute_transcode(&decode_job, std::io::Cursor::new(flac_out), &mut pcm_out).unwrap();
        // 0.2s * 44100 = 8820 frames * 4 bytes = 35280 bytes
        assert_eq!(pcm_out.len(), 35280);
    }

    #[test]
    fn test_explicit_mode_and_sox_flags() {
        let job = parse_cli_invocation(
            "lms-transcode",
            &[
                "--mode",
                "sox",
                "-q",
                "-t",
                "raw",
                "--endian",
                "big",
                "-b",
                "16",
                "-r",
                "48000",
                "-c",
                "2",
                "/tmp/in.raw",
                "-t",
                "wav",
                "-",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job.output_format, OutputContainer::Wav);
        assert_eq!(job.input_path, Some("/tmp/in.raw".to_string()));

        let job_faad_mode = parse_cli_invocation(
            "lms-transcode",
            &["--mode=faad", "-q", "-w", "-f", "1", "/tmp/in.aac"].map(String::from),
        )
        .unwrap();
        assert_eq!(job_faad_mode.output_format, OutputContainer::Wav);
        assert_eq!(job_faad_mode.input_path, Some("/tmp/in.aac".to_string()));
    }

    #[test]
    fn test_wav_output_container_roundtrip() {
        let wav_bytes = generate_test_wav_44100_stereo(0.5);
        let job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::Wav,
            start_sec: Some(0.1),
            end_sec: Some(0.4),
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // 44-byte WAV header + (0.3s * 44100 = 13230 frames * 4 bytes = 52920 bytes) = 52964 bytes
        assert_eq!(out.len(), 44 + 52920);
        assert!(out.starts_with(b"RIFF"));
    }

    #[test]
    fn test_raw_pcm_be_transcode() {
        let wav_bytes = generate_test_wav_44100_stereo(0.1);
        let job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::RawPcmBe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // 0.1s * 44100 = 4410 frames * 4 bytes = 17640 bytes
        assert_eq!(out.len(), 17640);
    }

    #[test]
    fn test_resampling_44100_to_48000() {
        let wav_bytes = generate_test_wav_44100_stereo(0.5);
        let job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: Some(48000),
            target_channels: Some(2),
            bits_per_sample: Some(16),
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // Expected ~0.5s * 48000 frames = 24000 frames * 4 bytes = 96000 bytes (within small window)
        let out_frames = out.len() / 4;
        let diff = (out_frames as i64 - 24000).abs();
        assert!(diff < 50, "resampled frames {} deviates from expected 24000", out_frames);
    }

    #[test]
    fn test_downmix_stereo_to_mono() {
        let wav_bytes = generate_test_wav_44100_stereo(0.25);
        let job = TranscodeJob {
            input_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(1),
            bits_per_sample: Some(16),
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // 0.25s * 44100 = 11025 frames * 1 channel * 2 bytes = 22050 bytes
        assert_eq!(out.len(), 22050);
    }
}
