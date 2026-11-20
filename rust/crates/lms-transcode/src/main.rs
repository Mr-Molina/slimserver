#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, Write};
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::CODEC_TYPE_NULL;
use symphonia::core::formats::SeekTo;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::probe::Hint;
use thiserror::Error;

struct SourceWrapper<R> {
    inner: R,
    seekable: bool,
    byte_len: Option<u64>,
}

impl<R: Read + Seek + Send + Sync> SourceWrapper<R> {
    fn new(mut inner: R) -> Self {
        let (seekable, byte_len) = match inner.stream_position() {
            Ok(pos) => match inner.seek(std::io::SeekFrom::End(0)) {
                Ok(end_pos) if inner.seek(std::io::SeekFrom::Start(pos)).is_ok() => {
                    (true, Some(end_pos))
                }
                _ => (false, None),
            },
            Err(_) => (false, None),
        };
        Self {
            inner,
            seekable,
            byte_len,
        }
    }
}

impl<R: Read + Seek + Send + Sync> Read for SourceWrapper<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Seek + Send + Sync> Seek for SourceWrapper<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        if !self.seekable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "source is not seekable",
            ));
        }
        self.inner.seek(pos)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for SourceWrapper<R> {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

#[derive(Error, Debug)]
pub enum TranscodeError {
    #[error("Help requested")]
    Help,
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

const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 384_000;
const MAX_CHANNELS: u16 = 8;

/// Input stream descriptor for headerless raw signed PCM input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPcmInputFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub endian_big: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeJob {
    pub input_path: Option<PathBuf>,
    pub output_path: Option<PathBuf>,
    pub output_format: OutputContainer,
    pub start_sec: Option<f64>,
    pub end_sec: Option<f64>,
    pub target_sample_rate: Option<u32>,
    pub target_channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
    pub raw_input: Option<RawPcmInputFormat>,
}

fn parse_non_negative_finite_f64(raw: &str, label: &str) -> Result<f64, TranscodeError> {
    let val = raw
        .parse::<f64>()
        .map_err(|e| TranscodeError::Parse(format!("invalid {label} '{raw}': {e}")))?;
    if !val.is_finite() || val < 0.0 {
        return Err(TranscodeError::Parse(format!(
            "{label} must be finite and non-negative, got '{raw}'"
        )));
    }
    Ok(val)
}

pub fn parse_time_offset(val: &str) -> Result<f64, TranscodeError> {
    let s = val.trim();
    if s.is_empty() {
        return Err(TranscodeError::Parse("empty time offset".into()));
    }
    let parts: Vec<&str> = s.split(':').collect();
    let total = match parts.as_slice() {
        [sec_str] => parse_non_negative_finite_f64(sec_str, "seconds")?,
        [min_str, sec_str] => {
            let m = parse_non_negative_finite_f64(min_str, "minutes")?;
            let sec = parse_non_negative_finite_f64(sec_str, "seconds")?;
            m * 60.0 + sec
        }
        [hr_str, min_str, sec_str] => {
            let h = parse_non_negative_finite_f64(hr_str, "hours")?;
            let m = parse_non_negative_finite_f64(min_str, "minutes")?;
            let sec = parse_non_negative_finite_f64(sec_str, "seconds")?;
            h * 3600.0 + m * 60.0 + sec
        }
        _ => {
            return Err(TranscodeError::Parse(format!(
                "unrecognized time format '{s}'"
            )));
        }
    };
    if !total.is_finite() || total < 0.0 {
        return Err(TranscodeError::Parse(format!(
            "time offset overflowed for '{s}'"
        )));
    }
    Ok(total)
}

fn os_to_str<'a>(arg: &'a OsStr, context: &str) -> Result<&'a str, TranscodeError> {
    arg.to_str()
        .ok_or_else(|| TranscodeError::Parse(format!("non-UTF-8 value for {context}")))
}

fn next_str_arg<'a, I>(iter: &mut I, opt: &str) -> Result<&'a str, TranscodeError>
where
    I: Iterator<Item = &'a OsString>,
{
    let next = iter
        .next()
        .ok_or_else(|| TranscodeError::Parse(format!("missing argument to {opt}")))?;
    os_to_str(next.as_os_str(), opt)
}

fn next_os_arg<'a, I>(iter: &mut I, opt: &str) -> Result<&'a OsString, TranscodeError>
where
    I: Iterator<Item = &'a OsString>,
{
    iter.next()
        .ok_or_else(|| TranscodeError::Parse(format!("missing argument to {opt}")))
}

fn set_positional_input(
    input_path: &mut Option<PathBuf>,
    saw_input: &mut bool,
    arg: &OsStr,
    mode: &str,
) -> Result<(), TranscodeError> {
    if *saw_input {
        return Err(TranscodeError::Parse(format!(
            "unexpected extra positional argument in {mode} mode"
        )));
    }
    *saw_input = true;
    if arg != OsStr::new("-") {
        *input_path = Some(PathBuf::from(arg));
    } else {
        *input_path = None;
    }
    Ok(())
}

/// Parses CLI arguments into a [`TranscodeJob`] from string slices.
pub fn parse_cli_invocation(argv0: &str, args: &[String]) -> Result<TranscodeJob, TranscodeError> {
    let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
    parse_cli_invocation_os(OsStr::new(argv0), &os_args)
}

fn flag_takes_operand_in_prescan(flag: &str) -> bool {
    matches!(
        flag,
        "-o" | "--output-name"
            | "-t"
            | "--type"
            | "-e"
            | "--encoding"
            | "-C"
            | "--compression"
            | "--bits"
            | "--rate"
            | "--bps"
            | "--sample-rate"
            | "--channels"
            | "--skip"
            | "--until"
            | "--endian"
            | "--sign"
            | "-b"
            | "-j"
            | "-ss"
    )
}

/// Parses OS-native CLI arguments (`env::args_os()`) into a [`TranscodeJob`].
pub fn parse_cli_invocation_os(
    argv0: &OsStr,
    args: &[OsString],
) -> Result<TranscodeJob, TranscodeError> {
    let binary_name = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let mut mode_override: Option<String> = None;
    let mut filtered_args: Vec<OsString> = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    let mut after_double_dash = false;

    while let Some(arg) = iter.next() {
        if after_double_dash {
            filtered_args.push(arg.clone());
            continue;
        }
        if arg == OsStr::new("--") {
            after_double_dash = true;
            filtered_args.push(arg.clone());
            continue;
        }
        if arg == OsStr::new("-h") || arg == OsStr::new("--help") {
            return Err(TranscodeError::Help);
        }
        if arg == OsStr::new("--mode") {
            let val = next_str_arg(&mut iter, "--mode")?;
            mode_override = Some(val.to_string());
            continue;
        }
        if let Some(s) = arg.to_str() {
            if let Some(val) = s.strip_prefix("--mode=") {
                if val.is_empty() {
                    return Err(TranscodeError::Parse("missing argument to --mode=".into()));
                }
                mode_override = Some(val.to_string());
                continue;
            }
            if flag_takes_operand_in_prescan(s) {
                filtered_args.push(arg.clone());
                if let Some(operand) = iter.next() {
                    filtered_args.push(operand.clone());
                }
                continue;
            }
        }
        filtered_args.push(arg.clone());
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
    } else {
        detect_mode_from_args(&filtered_args)
    };

    match mode.as_str() {
        "faad" => parse_faad_os(&filtered_args),
        "flac" => parse_flac_os(&filtered_args),
        "wvunpack" => parse_wvunpack_os(&filtered_args),
        "sox" => parse_sox_os(&filtered_args),
        other => Err(TranscodeError::Parse(format!(
            "unrecognized transcoder mode '{other}'"
        ))),
    }
}

fn set_output_path(output_path: &mut Option<PathBuf>, arg: &OsStr) -> Result<(), TranscodeError> {
    if arg.is_empty() {
        return Err(TranscodeError::Parse("output path cannot be empty".into()));
    }
    if arg == OsStr::new("-") {
        *output_path = None;
    } else {
        *output_path = Some(PathBuf::from(arg));
    }
    Ok(())
}

fn parse_sample_rate(val: &str, opt: &str) -> Result<u32, TranscodeError> {
    let rate: u32 = val
        .parse()
        .map_err(|e| TranscodeError::Parse(format!("invalid {opt} '{val}': {e}")))?;
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&rate) {
        return Err(TranscodeError::Parse(format!(
            "{opt} sample rate {rate} out of range ({MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE})"
        )));
    }
    Ok(rate)
}

fn parse_channels(val: &str, opt: &str) -> Result<u16, TranscodeError> {
    let ch: u16 = val
        .parse()
        .map_err(|e| TranscodeError::Parse(format!("invalid {opt} '{val}': {e}")))?;
    if !(1..=MAX_CHANNELS).contains(&ch) {
        return Err(TranscodeError::Parse(format!(
            "{opt} channel count {ch} out of range (1..={MAX_CHANNELS})"
        )));
    }
    Ok(ch)
}

fn parse_bits_per_sample(val: &str, opt: &str) -> Result<u16, TranscodeError> {
    let bps: u16 = val
        .parse()
        .map_err(|e| TranscodeError::Parse(format!("invalid {opt} '{val}': {e}")))?;
    if !matches!(bps, 8 | 16 | 24 | 32) {
        return Err(TranscodeError::Parse(format!(
            "{opt} bits per sample {bps} unsupported: must be 8, 16, 24, or 32"
        )));
    }
    Ok(bps)
}

fn detect_mode_from_args(args: &[OsString]) -> String {
    let mut saw_sox_exclusive = false;
    let mut saw_wvunpack_exclusive = false;
    let mut saw_wvunpack_r = false;
    let mut saw_flac_exclusive = false;
    let mut saw_faad_flag = false;
    let mut saw_faad_f = false;
    let mut saw_w = false;
    let mut saw_skip_or_until = false;

    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == OsStr::new("--") {
            break;
        }
        let Some(s) = arg.to_str() else {
            continue;
        };
        if s == "$-x$" {
            saw_sox_exclusive = true;
            continue;
        }
        if !s.starts_with('-') || s == "-" {
            continue;
        }
        match s {
            "-t" | "--type" | "--encoding" | "-C" | "--compression" => {
                saw_sox_exclusive = true;
                let _ = iter.next();
            }
            "--bits" | "--rate" => {
                saw_sox_exclusive = true;
                let _ = iter.next();
            }
            "-D" | "--no-dither" | "--ignore-length" | "-1" | "-2" | "-3" | "-4" | "-L" | "-B"
            | "-X" => {
                saw_sox_exclusive = true;
            }
            "-wq" | "-rq" | "--dsf" | "--dff" => {
                saw_wvunpack_exclusive = true;
            }
            "-ss" | "-u" => {
                saw_wvunpack_exclusive = true;
                let _ = iter.next();
            }
            "--force-raw-format"
            | "--decode"
            | "--stdout"
            | "--totally-silent"
            | "--silent"
            | "--ogg"
            | "--ignore-chunk-sizes" => {
                saw_flac_exclusive = true;
            }
            "--sign" | "--bps" | "--sample-rate" | "--channels" | "--output-name" => {
                saw_flac_exclusive = true;
                let _ = iter.next();
            }
            "--skip" | "--until" => {
                saw_skip_or_until = true;
                let _ = iter.next();
            }
            "-o" | "--endian" | "-b" => {
                let _ = iter.next();
            }
            "-j" => {
                saw_faad_flag = true;
                let _ = iter.next();
            }
            "-w" => {
                saw_w = true;
            }
            "-f" => {
                if let Some(next) = iter.peek().and_then(|v| v.to_str()) {
                    if matches!(next, "1" | "2") {
                        saw_faad_f = true;
                        let _ = iter.next();
                    }
                }
            }
            "-e" => {
                if let Some(next) = iter.next() {
                    if next.to_str().is_some_and(|v| parse_time_offset(v).is_ok()) {
                        saw_faad_flag = true;
                    } else {
                        saw_sox_exclusive = true;
                    }
                }
            }
            "-r" => {
                if let Some(next) = iter.peek().and_then(|v| v.to_str()) {
                    if next.parse::<u32>().is_ok() {
                        let _ = iter.next();
                    } else {
                        saw_wvunpack_r = true;
                    }
                } else {
                    saw_wvunpack_r = true;
                }
            }
            _ => {
                if s.starts_with("--type=") || s.starts_with("--encoding=") {
                    saw_sox_exclusive = true;
                } else if s.starts_with("--skip=") || s.starts_with("--until=") {
                    saw_skip_or_until = true;
                } else if s.starts_with("--sign=")
                    || s.starts_with("--bps=")
                    || s.starts_with("--sample-rate=")
                    || s.starts_with("--channels=")
                    || s.starts_with("--output-name=")
                    || s.starts_with("--compression-level-")
                {
                    saw_flac_exclusive = true;
                } else if let Some(rest) = s.strip_prefix("-j") {
                    if !rest.is_empty() && parse_time_offset(rest).is_ok() {
                        saw_faad_flag = true;
                    }
                } else if let Some(rest) = s.strip_prefix("-e") {
                    if !rest.is_empty() && parse_time_offset(rest).is_ok() {
                        saw_faad_flag = true;
                    }
                } else if !s.starts_with("--") {
                    let flags = &s[1..];
                    let valid_flac_cluster = !flags.is_empty()
                        && flags
                            .chars()
                            .all(|ch| matches!(ch, 'c' | 'd' | 's' | 'f' | 'w' | '0'..='8'));
                    if valid_flac_cluster
                        && (flags.contains('d')
                            || flags.chars().any(|ch| ch.is_ascii_digit())
                            || (flags.contains('c') && flags.len() > 1))
                    {
                        saw_flac_exclusive = true;
                    }
                }
            }
        }
    }

    if saw_sox_exclusive {
        return "sox".to_string();
    }
    if saw_wvunpack_exclusive
        || (saw_skip_or_until && (saw_wvunpack_r || (saw_w && !saw_flac_exclusive)))
    {
        return "wvunpack".to_string();
    }
    if saw_flac_exclusive || saw_skip_or_until {
        return "flac".to_string();
    }
    if saw_faad_flag || (saw_w && saw_faad_f) {
        return "faad".to_string();
    }
    if saw_wvunpack_r {
        return "wvunpack".to_string();
    }
    "sox".to_string()
}

fn validate_time_range(
    start_sec: Option<f64>,
    end_sec: Option<f64>,
    mode: &str,
) -> Result<(), TranscodeError> {
    if let (Some(start), Some(end)) = (start_sec, end_sec) {
        if end < start {
            return Err(TranscodeError::Parse(format!(
                "{mode} end offset '{end}' precedes start offset '{start}'"
            )));
        }
    }
    Ok(())
}

fn parse_faad_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut bits_per_sample = None;
    let mut input_path = None;
    let mut output_path = None;
    let mut saw_input = false;
    let mut after_double_dash = false;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        if after_double_dash {
            set_positional_input(&mut input_path, &mut saw_input, arg, "faad")?;
            continue;
        }
        if arg == OsStr::new("--") {
            after_double_dash = true;
            continue;
        }
        if arg == OsStr::new("-") {
            set_positional_input(&mut input_path, &mut saw_input, arg, "faad")?;
            continue;
        }
        let Some(s) = arg.to_str() else {
            set_positional_input(&mut input_path, &mut saw_input, arg, "faad")?;
            continue;
        };
        if !s.starts_with('-') {
            set_positional_input(&mut input_path, &mut saw_input, arg, "faad")?;
            continue;
        }
        match s {
            "-h" | "--help" => return Err(TranscodeError::Help),
            "-q" => {}
            "-w" => {
                if output_format != OutputContainer::RawPcmLe {
                    output_format = OutputContainer::Wav;
                }
            }
            "-f" => match next_str_arg(&mut iter, "-f")? {
                "1" => output_format = OutputContainer::Wav,
                "2" => output_format = OutputContainer::RawPcmLe,
                other => {
                    return Err(TranscodeError::Parse(format!(
                        "unsupported faad format '{other}'"
                    )));
                }
            },
            "-b" => {
                let val = next_str_arg(&mut iter, "-b")?;
                bits_per_sample = Some(match val {
                    "1" => 16,
                    "2" => 24,
                    "3" => 32,
                    other => parse_bits_per_sample(other, "-b")?,
                });
            }
            "-o" => {
                let val = next_os_arg(&mut iter, "-o")?;
                set_output_path(&mut output_path, val.as_os_str())?;
            }
            "-j" => {
                let val = next_str_arg(&mut iter, "-j")?;
                start_sec = Some(parse_time_offset(val)?);
            }
            "-e" => {
                let val = next_str_arg(&mut iter, "-e")?;
                end_sec = Some(parse_time_offset(val)?);
            }
            _ => {
                if let Some(rest) = s.strip_prefix("-o") {
                    set_output_path(&mut output_path, OsStr::new(rest))?;
                } else if let Some(rest) = s.strip_prefix("-j") {
                    start_sec = Some(parse_time_offset(rest)?);
                } else if let Some(rest) = s.strip_prefix("-e") {
                    end_sec = Some(parse_time_offset(rest)?);
                } else {
                    return Err(TranscodeError::Parse(format!("unknown faad option '{s}'")));
                }
            }
        }
    }

    validate_time_range(start_sec, end_sec, "faad")?;

    Ok(TranscodeJob {
        input_path,
        output_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate: None,
        target_channels: None,
        bits_per_sample,
        raw_input: None,
    })
}

fn parse_endian_flag(val: &str, opt: &str) -> Result<bool, TranscodeError> {
    match val {
        "little" => Ok(false),
        "big" => Ok(true),
        other => Err(TranscodeError::Parse(format!(
            "invalid {opt} value '{other}'"
        ))),
    }
}

fn validate_flac_sign(val: &str, opt: &str) -> Result<(), TranscodeError> {
    match val {
        "signed" => Ok(()),
        "unsigned" => Err(TranscodeError::Parse(format!(
            "unsupported {opt} value 'unsigned': only signed PCM is supported"
        ))),
        other => Err(TranscodeError::Parse(format!(
            "invalid {opt} value '{other}'"
        ))),
    }
}

fn parse_flac_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut is_encode: Option<bool> = None;
    let mut saw_stdout = false;
    let mut force_raw = false;
    let mut endian_big = false;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut bits_per_sample = None;
    let mut target_sample_rate = None;
    let mut target_channels = None;
    let mut input_path = None;
    let mut output_path = None;
    let mut saw_input = false;
    let mut after_double_dash = false;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        if after_double_dash {
            set_positional_input(&mut input_path, &mut saw_input, arg, "flac")?;
            continue;
        }
        if arg == OsStr::new("--") {
            after_double_dash = true;
            continue;
        }
        if arg == OsStr::new("-") {
            set_positional_input(&mut input_path, &mut saw_input, arg, "flac")?;
            continue;
        }
        let Some(s) = arg.to_str() else {
            set_positional_input(&mut input_path, &mut saw_input, arg, "flac")?;
            continue;
        };
        if !s.starts_with('-') {
            set_positional_input(&mut input_path, &mut saw_input, arg, "flac")?;
            continue;
        }
        match s {
            "-h" | "--help" => return Err(TranscodeError::Help),
            "--force-raw-format" => force_raw = true,
            "--totally-silent" | "--silent" | "--ogg" | "--ignore-chunk-sizes" => {}
            "--decode" => is_encode = Some(false),
            "--stdout" => saw_stdout = true,
            "--skip" => {
                let val = next_str_arg(&mut iter, "--skip")?;
                start_sec = Some(parse_time_offset(val)?);
            }
            "--until" => {
                let val = next_str_arg(&mut iter, "--until")?;
                end_sec = Some(parse_time_offset(val)?);
            }
            "--endian" => {
                let val = next_str_arg(&mut iter, "--endian")?;
                endian_big = parse_endian_flag(val, "--endian")?;
            }
            "--sign" => {
                let val = next_str_arg(&mut iter, "--sign")?;
                validate_flac_sign(val, "--sign")?;
            }
            "--bps" => {
                let val = next_str_arg(&mut iter, "--bps")?;
                bits_per_sample = Some(parse_bits_per_sample(val, "--bps")?);
            }
            "--sample-rate" => {
                let val = next_str_arg(&mut iter, "--sample-rate")?;
                target_sample_rate = Some(parse_sample_rate(val, "--sample-rate")?);
            }
            "--channels" => {
                let val = next_str_arg(&mut iter, "--channels")?;
                target_channels = Some(parse_channels(val, "--channels")?);
            }
            "-o" | "--output-name" => {
                let val = next_os_arg(&mut iter, s)?;
                set_output_path(&mut output_path, val.as_os_str())?;
            }
            _ => {
                if let Some(val) = s.strip_prefix("--skip=") {
                    start_sec = Some(parse_time_offset(val)?);
                } else if let Some(val) = s.strip_prefix("--until=") {
                    end_sec = Some(parse_time_offset(val)?);
                } else if let Some(val) = s.strip_prefix("--endian=") {
                    endian_big = parse_endian_flag(val, "--endian=")?;
                } else if let Some(val) = s.strip_prefix("--sign=") {
                    validate_flac_sign(val, "--sign=")?;
                } else if let Some(val) = s.strip_prefix("--bps=") {
                    bits_per_sample = Some(parse_bits_per_sample(val, "--bps=")?);
                } else if let Some(val) = s.strip_prefix("--sample-rate=") {
                    target_sample_rate = Some(parse_sample_rate(val, "--sample-rate=")?);
                } else if let Some(val) = s.strip_prefix("--channels=") {
                    target_channels = Some(parse_channels(val, "--channels=")?);
                } else if let Some(val) = s.strip_prefix("--output-name=") {
                    set_output_path(&mut output_path, OsStr::new(val))?;
                } else if let Some(rest) = s.strip_prefix("-o") {
                    set_output_path(&mut output_path, OsStr::new(rest))?;
                } else if let Some(lvl) = s.strip_prefix("--compression-level-") {
                    if !matches!(lvl, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8") {
                        return Err(TranscodeError::Parse(format!(
                            "invalid flac compression level '{lvl}'"
                        )));
                    }
                    is_encode = Some(true);
                } else if !s.starts_with("--") {
                    for ch in s.chars().skip(1) {
                        match ch {
                            'c' => saw_stdout = true,
                            'd' => is_encode = Some(false),
                            's' | 'f' | 'w' => {}
                            '0'..='8' => is_encode = Some(true),
                            _ => {
                                return Err(TranscodeError::Parse(format!(
                                    "unknown flac option '-{ch}'"
                                )));
                            }
                        }
                    }
                } else {
                    return Err(TranscodeError::Parse(format!("unknown flac option '{s}'")));
                }
            }
        }
    }

    validate_time_range(start_sec, end_sec, "flac")?;

    let encode_mode = is_encode.unwrap_or(saw_stdout);
    let output_format = if encode_mode {
        OutputContainer::Flac
    } else if force_raw {
        if endian_big {
            OutputContainer::RawPcmBe
        } else {
            OutputContainer::RawPcmLe
        }
    } else {
        OutputContainer::Wav
    };

    let (target_sample_rate, target_channels, bits_per_sample, raw_input) = if encode_mode
        && force_raw
    {
        let sample_rate = target_sample_rate.ok_or_else(|| {
            TranscodeError::Parse("flac --force-raw-format encode requires --sample-rate".into())
        })?;
        let channels = target_channels.ok_or_else(|| {
            TranscodeError::Parse("flac --force-raw-format encode requires --channels".into())
        })?;
        let raw_bps = bits_per_sample.ok_or_else(|| {
            TranscodeError::Parse("flac --force-raw-format encode requires --bps".into())
        })?;
        (
            None,
            None,
            None,
            Some(RawPcmInputFormat {
                sample_rate,
                channels,
                bits_per_sample: raw_bps,
                endian_big,
            }),
        )
    } else {
        (target_sample_rate, target_channels, bits_per_sample, None)
    };

    Ok(TranscodeJob {
        input_path,
        output_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate,
        target_channels,
        bits_per_sample,
        raw_input,
    })
}

fn parse_wvunpack_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut input_path = None;
    let mut output_path = None;
    let mut saw_input = false;
    let mut after_double_dash = false;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        if after_double_dash {
            set_positional_input(&mut input_path, &mut saw_input, arg, "wvunpack")?;
            continue;
        }
        if arg == OsStr::new("--") {
            after_double_dash = true;
            continue;
        }
        if arg == OsStr::new("-") {
            set_positional_input(&mut input_path, &mut saw_input, arg, "wvunpack")?;
            continue;
        }
        let Some(s) = arg.to_str() else {
            set_positional_input(&mut input_path, &mut saw_input, arg, "wvunpack")?;
            continue;
        };
        if !s.starts_with('-') {
            set_positional_input(&mut input_path, &mut saw_input, arg, "wvunpack")?;
            continue;
        }
        match s {
            "-h" | "--help" => return Err(TranscodeError::Help),
            "-w" | "-wq" => output_format = OutputContainer::Wav,
            "-r" | "-rq" => output_format = OutputContainer::RawPcmLe,
            "-q" => {}
            "--dsf" | "--dff" => {
                return Err(TranscodeError::Parse(format!(
                    "unsupported wvunpack output format '{s}': DSD output is not supported"
                )));
            }
            "-o" => {
                let val = next_os_arg(&mut iter, "-o")?;
                set_output_path(&mut output_path, val.as_os_str())?;
            }
            "-s" | "-ss" => {
                let val = next_str_arg(&mut iter, s)?;
                start_sec = Some(parse_time_offset(val)?);
            }
            "-u" => {
                let val = next_str_arg(&mut iter, "-u")?;
                end_sec = Some(parse_time_offset(val)?);
            }
            "--skip" => {
                let val = next_str_arg(&mut iter, "--skip")?;
                start_sec = Some(parse_time_offset(val)?);
            }
            "--until" => {
                let val = next_str_arg(&mut iter, "--until")?;
                end_sec = Some(parse_time_offset(val)?);
            }
            _ => {
                if let Some(rest) = s.strip_prefix("-o") {
                    if rest.is_empty() {
                        return Err(TranscodeError::Parse("missing argument to -o".into()));
                    }
                    set_output_path(&mut output_path, OsStr::new(rest))?;
                } else if let Some(val) = s.strip_prefix("--skip=") {
                    start_sec = Some(parse_time_offset(val)?);
                } else if let Some(val) = s.strip_prefix("--until=") {
                    end_sec = Some(parse_time_offset(val)?);
                } else {
                    return Err(TranscodeError::Parse(format!(
                        "unknown wvunpack option '{s}'"
                    )));
                }
            }
        }
    }

    validate_time_range(start_sec, end_sec, "wvunpack")?;

    Ok(TranscodeJob {
        input_path,
        output_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate: None,
        target_channels: None,
        bits_per_sample: None,
        raw_input: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoxPhase {
    InputOptions,
    OutputOptions,
    Effects,
}

fn apply_sox_output_type(
    val: &str,
    endian_big: bool,
    output_format: &mut OutputContainer,
) -> Result<(), TranscodeError> {
    match val {
        "wav" => *output_format = OutputContainer::Wav,
        "flac" => *output_format = OutputContainer::Flac,
        "raw" => {
            *output_format = if endian_big {
                OutputContainer::RawPcmBe
            } else {
                OutputContainer::RawPcmLe
            };
        }
        other => {
            return Err(TranscodeError::Parse(format!(
                "unsupported sox output format '{other}'"
            )));
        }
    }
    Ok(())
}

fn parse_sox_trim<'a, I>(
    iter: &mut Peekable<I>,
    start_sec: &mut Option<f64>,
    end_sec: &mut Option<f64>,
) -> Result<(), TranscodeError>
where
    I: Iterator<Item = &'a OsString>,
{
    let start_str = next_str_arg(iter, "trim")?;
    let start = parse_time_offset(start_str)?;
    *start_sec = Some(start);

    let Some(peeked) = iter.peek() else {
        return Ok(());
    };
    let peek_str = os_to_str(peeked.as_os_str(), "trim end/duration")?;
    let looks_like_time = peek_str.starts_with('=')
        || peek_str.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '.' | '+' | '-'))
        || peek_str.contains(':')
        || peek_str.eq_ignore_ascii_case("nan")
        || peek_str.eq_ignore_ascii_case("inf")
        || peek_str.eq_ignore_ascii_case("infinity");
    if !looks_like_time {
        return Ok(());
    }
    let _ = iter.next();
    if let Some(end_str) = peek_str.strip_prefix('=') {
        let end = parse_time_offset(end_str)?;
        if end < start {
            return Err(TranscodeError::Parse(format!(
                "sox trim end '{end}' precedes start '{start}'"
            )));
        }
        *end_sec = Some(end);
    } else {
        let dur = parse_time_offset(peek_str)?;
        let end = start + dur;
        if !end.is_finite() {
            return Err(TranscodeError::Parse("sox trim end overflowed".into()));
        }
        *end_sec = Some(end);
    }
    Ok(())
}

fn parse_sox_rate<'a, I>(
    iter: &mut Peekable<I>,
    target_sample_rate: &mut Option<u32>,
) -> Result<(), TranscodeError>
where
    I: Iterator<Item = &'a OsString>,
{
    while let Some(arg) = iter.next() {
        let s = os_to_str(arg.as_os_str(), "rate")?;
        if matches!(
            s,
            "-q" | "-l" | "-m" | "-h" | "-v" | "-I" | "-L" | "-a" | "-s"
        ) {
            continue;
        }
        if s == "-b" {
            let _ = next_str_arg(iter, "rate -b")?;
            continue;
        }
        if s.starts_with('-') {
            return Err(TranscodeError::Parse(format!(
                "unknown sox rate option '{s}'"
            )));
        }
        let rate = parse_sample_rate(s, "rate")?;
        *target_sample_rate = Some(rate);
        return Ok(());
    }
    Err(TranscodeError::Parse(
        "missing sample rate for sox rate".into(),
    ))
}

fn parse_sox_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut output_type: Option<&str> = None;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut target_sample_rate = None;
    let mut target_channels = None;
    let mut bits_per_sample = None;
    let mut input_path = None;
    let mut output_path = None;
    let mut endian_big = false;
    let mut phase = SoxPhase::InputOptions;
    let mut after_double_dash = false;
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if phase == SoxPhase::Effects {
            let eff = os_to_str(arg.as_os_str(), "sox effect")?;
            match eff {
                "trim" => parse_sox_trim(&mut iter, &mut start_sec, &mut end_sec)?,
                "rate" => parse_sox_rate(&mut iter, &mut target_sample_rate)?,
                other => {
                    return Err(TranscodeError::Parse(format!(
                        "unsupported sox effect '{other}'"
                    )));
                }
            }
            continue;
        }

        if !after_double_dash && arg == OsStr::new("--") {
            after_double_dash = true;
            continue;
        }

        let is_option = !after_double_dash
            && arg != OsStr::new("-")
            && arg
                .to_str()
                .map(|s| s.starts_with('-') || s == "$-x$")
                .unwrap_or(false);

        if !is_option {
            match phase {
                SoxPhase::InputOptions => {
                    if arg != OsStr::new("-") {
                        input_path = Some(PathBuf::from(arg));
                    }
                    phase = SoxPhase::OutputOptions;
                }
                SoxPhase::OutputOptions => {
                    set_output_path(&mut output_path, arg.as_os_str())?;
                    phase = SoxPhase::Effects;
                }
                SoxPhase::Effects => {}
            }
            continue;
        }

        let s = os_to_str(arg.as_os_str(), "sox option")?;
        let is_output = phase == SoxPhase::OutputOptions;
        match s {
            "-h" | "--help" => return Err(TranscodeError::Help),
            "-q" | "--ignore-length" | "-D" | "--no-dither" | "-s" | "-X" => {}
            "-u" | "-f" | "-A" | "-U" => {
                if is_output {
                    return Err(TranscodeError::Parse(format!(
                        "unsupported sox output encoding '{s}': only signed-integer is supported"
                    )));
                }
            }
            "-L" => {
                if is_output {
                    endian_big = false;
                }
            }
            "-B" => {
                if is_output {
                    endian_big = true;
                }
            }
            "-x" | "$-x$" => {
                if is_output {
                    endian_big = !endian_big;
                }
            }
            "-1" => {
                if is_output {
                    bits_per_sample = Some(8);
                }
            }
            "-2" => {
                if is_output {
                    bits_per_sample = Some(16);
                }
            }
            "-3" => {
                if is_output {
                    bits_per_sample = Some(24);
                }
            }
            "-4" => {
                if is_output {
                    bits_per_sample = Some(32);
                }
            }
            "-t" | "--type" => {
                let val = next_str_arg(&mut iter, s)?;
                if is_output {
                    if !matches!(val, "wav" | "flac" | "raw") {
                        return Err(TranscodeError::Parse(format!(
                            "unsupported sox output format '{val}'"
                        )));
                    }
                    output_type = Some(val);
                }
            }
            "-r" | "--rate" => {
                let val = next_str_arg(&mut iter, s)?;
                let rate = parse_sample_rate(val, s)?;
                if is_output {
                    target_sample_rate = Some(rate);
                }
            }
            "-c" | "--channels" => {
                let val = next_str_arg(&mut iter, s)?;
                let ch = parse_channels(val, s)?;
                if is_output {
                    target_channels = Some(ch);
                }
            }
            "-b" | "--bits" => {
                let val = next_str_arg(&mut iter, s)?;
                let bps = parse_bits_per_sample(val, s)?;
                if is_output {
                    bits_per_sample = Some(bps);
                }
            }
            "-C" | "--compression" => {
                let _ = next_str_arg(&mut iter, s)?;
            }
            "-e" | "--encoding" => {
                let val = next_str_arg(&mut iter, s)?;
                if val.is_empty() {
                    return Err(TranscodeError::Parse(format!("empty {s} value")));
                }
                if is_output && val != "signed-integer" {
                    return Err(TranscodeError::Parse(format!(
                        "unsupported sox output encoding '{val}': only signed-integer is supported"
                    )));
                }
            }
            "--endian" => {
                let val = next_str_arg(&mut iter, "--endian")?;
                let big = parse_endian_flag(val, "--endian")?;
                if is_output {
                    endian_big = big;
                }
            }
            _ => {
                if let Some(val) = s.strip_prefix("--endian=") {
                    let big = parse_endian_flag(val, "--endian=")?;
                    if is_output {
                        endian_big = big;
                    }
                } else if let Some(val) = s.strip_prefix("--type=") {
                    if is_output {
                        if !matches!(val, "wav" | "flac" | "raw") {
                            return Err(TranscodeError::Parse(format!(
                                "unsupported sox output format '{val}'"
                            )));
                        }
                        output_type = Some(val);
                    }
                } else if let Some(val) = s.strip_prefix("--encoding=") {
                    if val.is_empty() {
                        return Err(TranscodeError::Parse("empty --encoding=".into()));
                    }
                    if is_output && val != "signed-integer" {
                        return Err(TranscodeError::Parse(format!(
                            "unsupported sox output encoding '{val}': \
                             only signed-integer is supported"
                        )));
                    }
                } else {
                    return Err(TranscodeError::Parse(format!("unknown sox option '{s}'")));
                }
            }
        }
    }

    if phase != SoxPhase::Effects {
        return Err(TranscodeError::Parse(
            "sox requires both input and output paths".into(),
        ));
    }

    if let Some(ty) = output_type {
        apply_sox_output_type(ty, endian_big, &mut output_format)?;
    }

    Ok(TranscodeJob {
        input_path,
        output_path,
        output_format,
        start_sec,
        end_sec,
        target_sample_rate,
        target_channels,
        bits_per_sample,
        raw_input: None,
    })
}

const MAX_FLAC_BLOCK_LEN: usize = 65_536;
const MAX_FLAC_FRAME_NUMBER: u64 = 0x7FFF_FFFF;
const MAX_FLAC_CODED_NUMBER: u64 = 0x000F_FFFF_FFFF;
const MAX_FLAC_SAMPLE_RATE: u32 = 0x000F_FFFF;

fn clamp_i16(sample: i32) -> i16 {
    sample.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn clamp_i24(sample: i32) -> i32 {
    sample.clamp(-8_388_608, 8_388_607)
}

fn validate_audio_params(sample_rate: u32, channels: u16, bps: u16) -> Result<(), TranscodeError> {
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
        return Err(TranscodeError::Other(format!(
            "unsupported sample rate {sample_rate}: must be {MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE}"
        )));
    }
    if !(1..=MAX_CHANNELS).contains(&channels) {
        return Err(TranscodeError::Other(format!(
            "unsupported channel count {channels}: must be 1..={MAX_CHANNELS}"
        )));
    }
    if !matches!(bps, 8 | 16 | 24 | 32) {
        return Err(TranscodeError::Other(format!(
            "unsupported bits per sample {bps}: must be 8, 16, 24, or 32"
        )));
    }
    Ok(())
}

const CRC8_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut j = 0;
        while j < 8 {
            if (crc & 0x80) != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn flac_crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        let idx = usize::from(crc ^ b);
        crc = CRC8_TABLE.get(idx).copied().unwrap_or(0);
    }
    crc
}

const CRC16_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x8005;
            } else {
                crc <<= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn flac_crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        let idx = usize::from(((crc >> 8) as u8) ^ b);
        let entry = CRC16_TABLE.get(idx).copied().unwrap_or(0);
        crc = (crc << 8) ^ entry;
    }
    crc
}

fn push_flac_utf8(out: &mut Vec<u8>, val: u64) -> Result<(), TranscodeError> {
    if val < 0x80 {
        out.push(val as u8);
    } else if val < 0x800 {
        out.push(0xC0 | ((val >> 6) as u8));
        out.push(0x80 | ((val & 0x3F) as u8));
    } else if val < 0x1_0000 {
        out.push(0xE0 | ((val >> 12) as u8));
        out.push(0x80 | (((val >> 6) & 0x3F) as u8));
        out.push(0x80 | ((val & 0x3F) as u8));
    } else if val < 0x20_0000 {
        out.push(0xF0 | ((val >> 18) as u8));
        out.push(0x80 | (((val >> 12) & 0x3F) as u8));
        out.push(0x80 | (((val >> 6) & 0x3F) as u8));
        out.push(0x80 | ((val & 0x3F) as u8));
    } else if val < 0x400_0000 {
        out.push(0xF8 | ((val >> 24) as u8));
        out.push(0x80 | (((val >> 18) & 0x3F) as u8));
        out.push(0x80 | (((val >> 12) & 0x3F) as u8));
        out.push(0x80 | (((val >> 6) & 0x3F) as u8));
        out.push(0x80 | ((val & 0x3F) as u8));
    } else if val < 0x8000_0000 {
        out.push(0xFC | ((val >> 30) as u8));
        out.push(0x80 | (((val >> 24) & 0x3F) as u8));
        out.push(0x80 | (((val >> 18) & 0x3F) as u8));
        out.push(0x80 | (((val >> 12) & 0x3F) as u8));
        out.push(0x80 | (((val >> 6) & 0x3F) as u8));
        out.push(0x80 | ((val & 0x3F) as u8));
    } else if val <= MAX_FLAC_CODED_NUMBER {
        out.push(0xFE);
        out.push(0x80 | (((val >> 30) & 0x3F) as u8));
        out.push(0x80 | (((val >> 24) & 0x3F) as u8));
        out.push(0x80 | (((val >> 18) & 0x3F) as u8));
        out.push(0x80 | (((val >> 12) & 0x3F) as u8));
        out.push(0x80 | (((val >> 6) & 0x3F) as u8));
        out.push(0x80 | ((val & 0x3F) as u8));
    } else {
        return Err(TranscodeError::Other(format!(
            "FLAC coded number {val} exceeds 36-bit limit"
        )));
    }
    Ok(())
}

fn write_flac_header<W: Write>(
    out: &mut W,
    sample_rate: u32,
    channels: u16,
    bps: u16,
    total_samples: u64,
) -> Result<(), TranscodeError> {
    validate_audio_params(sample_rate, channels, bps)?;
    if sample_rate > MAX_FLAC_SAMPLE_RATE {
        return Err(TranscodeError::Other(format!(
            "FLAC sample rate {sample_rate} exceeds 20-bit limit"
        )));
    }
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
    let sr = u64::from(sample_rate) & 0x0F_FFFF;
    let ch = u64::from((channels - 1) & 0x07);
    let b = u64::from((bps - 1) & 0x1F);
    let tot = if total_samples <= MAX_FLAC_CODED_NUMBER {
        total_samples
    } else {
        0
    };
    let packed = (sr << 44) | (ch << 41) | (b << 36) | tot;
    block.extend_from_slice(&packed.to_be_bytes());
    block.extend_from_slice(&[0u8; 16]);
    out.write_all(&block)?;
    Ok(())
}

fn write_flac_frame<W: Write>(
    out: &mut W,
    frame_idx: u64,
    block_len: usize,
    channels: u16,
    bps: u16,
    channel_samples: &[Vec<i32>],
) -> Result<(), TranscodeError> {
    if !(1..=8).contains(&channels) {
        return Err(TranscodeError::Other(format!(
            "unsupported FLAC channel count {channels}: must be 1..=8"
        )));
    }
    if !matches!(bps, 8 | 16 | 24 | 32) {
        return Err(TranscodeError::Other(format!(
            "unsupported FLAC bits per sample {bps}: must be 8, 16, 24, or 32"
        )));
    }
    if !(1..=MAX_FLAC_BLOCK_LEN).contains(&block_len) {
        return Err(TranscodeError::Other(format!(
            "invalid FLAC block length {block_len}: must be 1..={MAX_FLAC_BLOCK_LEN}"
        )));
    }
    if frame_idx > MAX_FLAC_FRAME_NUMBER {
        return Err(TranscodeError::Other(format!(
            "FLAC frame number {frame_idx} exceeds 31-bit limit"
        )));
    }
    let ch_count = usize::from(channels);
    if channel_samples.len() < ch_count {
        return Err(TranscodeError::Other(
            "FLAC input has fewer channels than expected".into(),
        ));
    }

    let mut header = Vec::with_capacity(16);
    header.push(0xFF);
    header.push(0xF8);
    let bs_code = if block_len == 4096 { 0x0Cu8 } else { 0x07u8 };
    header.push(bs_code << 4);
    let ch_code = ((channels - 1) as u8) & 0x0F;
    header.push(ch_code << 4);
    push_flac_utf8(&mut header, frame_idx)?;
    if bs_code == 0x07 {
        header.extend_from_slice(&((block_len - 1) as u16).to_be_bytes());
    }
    let crc8 = flac_crc8(&header);
    header.push(crc8);

    let bytes_per_sample = usize::from(bps / 8);
    let payload_cap = block_len
        .saturating_mul(ch_count)
        .saturating_mul(bytes_per_sample)
        .saturating_add(32);
    let mut frame_bytes = Vec::with_capacity(payload_cap);
    frame_bytes.extend_from_slice(&header);
    for ch in 0..ch_count {
        let samples = channel_samples
            .get(ch)
            .and_then(|s| s.get(..block_len))
            .ok_or_else(|| {
                TranscodeError::Other("FLAC channel buffer shorter than block_len".into())
            })?;
        frame_bytes.push(0x02);
        for &s in samples {
            match bps {
                16 => {
                    frame_bytes.extend_from_slice(&clamp_i16(s).to_be_bytes());
                }
                24 => {
                    let [_, b1, b2, b3] = clamp_i24(s).to_be_bytes();
                    frame_bytes.extend_from_slice(&[b1, b2, b3]);
                }
                32 => {
                    frame_bytes.extend_from_slice(&s.to_be_bytes());
                }
                8 => {
                    frame_bytes.push((s.clamp(-128, 127) as i8) as u8);
                }
                _ => {
                    return Err(TranscodeError::Other(format!(
                        "unsupported FLAC bits per sample: {bps}"
                    )));
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
    validate_audio_params(sample_rate, channels, bps)?;
    let bytes_per_sample = u64::from(bps / 8);
    let (riff_size, data_size) = match total_frames {
        Some(frames) => {
            let data_bytes = frames
                .checked_mul(u64::from(channels))
                .and_then(|v| v.checked_mul(bytes_per_sample));
            match data_bytes {
                Some(bytes) if bytes <= u64::from(u32::MAX) - 36 => {
                    ((bytes + 36) as u32, bytes as u32)
                }
                _ => (0xFFFF_FFFFu32, 0xFFFF_FFFFu32),
            }
        }
        None => (0xFFFF_FFFFu32, 0xFFFF_FFFFu32),
    };

    let bytes_per_sample_u16 = bps / 8;
    let block_align = channels
        .checked_mul(bytes_per_sample_u16)
        .ok_or_else(|| TranscodeError::Other("WAV block_align overflow".into()))?;
    let byte_rate = sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| TranscodeError::Other("WAV byte_rate overflow".into()))?;

    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&bps.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_size.to_le_bytes());
    out.write_all(&header)?;
    Ok(())
}

struct ContainerWriter<'a, W: Write> {
    out: &'a mut W,
    format: OutputContainer,
    channels: u16,
    bps: u16,
    flac_frame_idx: &'a mut u64,
    flac_pending: &'a mut [Vec<i32>],
}

impl<'a, W: Write> ContainerWriter<'a, W> {
    fn write_samples(
        &mut self,
        samples_planar: &[Vec<i32>],
        num_frames: usize,
    ) -> Result<(), TranscodeError> {
        if !matches!(self.bps, 8 | 16 | 24 | 32) {
            return Err(TranscodeError::Other(format!(
                "unsupported bits per sample: {}",
                self.bps
            )));
        }
        if !(1..=MAX_CHANNELS).contains(&self.channels) {
            return Err(TranscodeError::Other(format!(
                "unsupported channel count {}: must be 1..={MAX_CHANNELS}",
                self.channels
            )));
        }
        if num_frames == 0 {
            return Ok(());
        }
        let ch_count = usize::from(self.channels);
        for ch in 0..ch_count {
            let valid = samples_planar
                .get(ch)
                .and_then(|c| c.get(..num_frames))
                .is_some();
            if !valid {
                return Err(TranscodeError::Other(
                    "planar sample buffer shorter than expected".into(),
                ));
            }
        }

        let bytes_per_sample = usize::from(self.bps / 8);
        let buf_cap = num_frames
            .checked_mul(ch_count)
            .and_then(|n| n.checked_mul(bytes_per_sample))
            .filter(|&b| b <= isize::MAX as usize)
            .ok_or_else(|| TranscodeError::Other("output sample buffer size overflow".into()))?;

        match self.format {
            OutputContainer::RawPcmLe | OutputContainer::Wav => {
                let mut buf = Vec::with_capacity(buf_cap);
                for f in 0..num_frames {
                    for ch in 0..ch_count {
                        let s = samples_planar
                            .get(ch)
                            .and_then(|c| c.get(f))
                            .copied()
                            .unwrap_or(0);
                        match self.bps {
                            16 => buf.extend_from_slice(&clamp_i16(s).to_le_bytes()),
                            24 => {
                                let [b0, b1, b2, _] = clamp_i24(s).to_le_bytes();
                                buf.extend_from_slice(&[b0, b1, b2]);
                            }
                            32 => buf.extend_from_slice(&s.to_le_bytes()),
                            8 => {
                                if self.format == OutputContainer::Wav {
                                    // RIFF/WAV 8-bit PCM is unsigned [0..=255].
                                    let u = (s.clamp(-128, 127) + 128) as u8;
                                    buf.push(u);
                                } else {
                                    buf.push((s.clamp(-128, 127) as i8) as u8);
                                }
                            }
                            _ => {
                                return Err(TranscodeError::Other(format!(
                                    "unsupported bits per sample: {}",
                                    self.bps
                                )));
                            }
                        }
                    }
                }
                self.out.write_all(&buf)?;
            }
            OutputContainer::RawPcmBe => {
                let mut buf = Vec::with_capacity(buf_cap);
                for f in 0..num_frames {
                    for ch in 0..ch_count {
                        let s = samples_planar
                            .get(ch)
                            .and_then(|c| c.get(f))
                            .copied()
                            .unwrap_or(0);
                        match self.bps {
                            16 => buf.extend_from_slice(&clamp_i16(s).to_be_bytes()),
                            24 => {
                                let [_, b1, b2, b3] = clamp_i24(s).to_be_bytes();
                                buf.extend_from_slice(&[b1, b2, b3]);
                            }
                            32 => buf.extend_from_slice(&s.to_be_bytes()),
                            8 => buf.push((s.clamp(-128, 127) as i8) as u8),
                            _ => {
                                return Err(TranscodeError::Other(format!(
                                    "unsupported bits per sample: {}",
                                    self.bps
                                )));
                            }
                        }
                    }
                }
                self.out.write_all(&buf)?;
            }
            OutputContainer::Flac => {
                if self.flac_pending.len() < ch_count {
                    return Err(TranscodeError::Other(
                        "FLAC pending buffer has fewer channels than expected".into(),
                    ));
                }
                for ch in 0..ch_count {
                    if let (Some(src), Some(dst)) = (
                        samples_planar.get(ch).and_then(|c| c.get(..num_frames)),
                        self.flac_pending.get_mut(ch),
                    ) {
                        dst.extend_from_slice(src);
                    }
                }
                while self.flac_pending.first().map_or(0, Vec::len) >= 4096 {
                    write_flac_frame(
                        self.out,
                        *self.flac_frame_idx,
                        4096,
                        self.channels,
                        self.bps,
                        self.flac_pending,
                    )?;
                    *self.flac_frame_idx += 1;
                    for ch in 0..ch_count {
                        if let Some(ch_buf) = self.flac_pending.get_mut(ch) {
                            ch_buf.drain(0..4096);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_flac_tail(&mut self) -> Result<(), TranscodeError> {
        if self.format == OutputContainer::Flac {
            let rem = self.flac_pending.first().map_or(0, Vec::len);
            if rem > 0 {
                write_flac_frame(
                    self.out,
                    *self.flac_frame_idx,
                    rem,
                    self.channels,
                    self.bps,
                    self.flac_pending,
                )?;
                *self.flac_frame_idx += 1;
                for ch_buf in self.flac_pending.iter_mut() {
                    ch_buf.clear();
                }
            }
        }
        Ok(())
    }
}

struct ManagedSampleBuffer<T: symphonia::core::sample::Sample> {
    buf: SampleBuffer<T>,
    frame_cap: usize,
    spec: symphonia::core::audio::SignalSpec,
}

fn copy_decoded_interleaved<'a, T>(
    holder: &'a mut Option<ManagedSampleBuffer<T>>,
    decoded: symphonia::core::audio::AudioBufferRef<'_>,
) -> Result<&'a [T], TranscodeError>
where
    T: symphonia::core::sample::Sample + symphonia::core::conv::ConvertibleSample,
{
    let n_frames = decoded.frames();
    let spec = *decoded.spec();
    let sc = spec.channels.count();
    if sc == 0 {
        return Err(TranscodeError::Other(
            "decoded audio frame has zero channels".into(),
        ));
    }
    let needed_frames = n_frames.max(decoded.capacity());
    let needed_samples = needed_frames
        .checked_mul(sc)
        .ok_or_else(|| TranscodeError::Other("decoded sample buffer size overflow".into()))?;
    let _needed_bytes = needed_samples
        .checked_mul(std::mem::size_of::<T>())
        .filter(|&b| b <= (isize::MAX as usize))
        .ok_or_else(|| TranscodeError::Other("decoded sample buffer byte size overflow".into()))?;
    let needed_frames_u64 = u64::try_from(needed_frames)
        .map_err(|_| TranscodeError::Other("decoded frame count overflow".into()))?;
    let needs_new = match holder {
        Some(existing) => {
            existing.frame_cap < needed_frames
                || existing.buf.capacity() < needed_samples
                || existing.spec != spec
        }
        None => true,
    };
    if needs_new {
        let buf = SampleBuffer::<T>::new(needed_frames_u64, spec);
        *holder = Some(ManagedSampleBuffer {
            buf,
            frame_cap: needed_frames,
            spec,
        });
    }
    let Some(managed) = holder.as_mut() else {
        return Err(TranscodeError::Other(
            "sample buffer initialization failed".into(),
        ));
    };
    managed.buf.copy_interleaved_ref(decoded);
    let expected = n_frames
        .checked_mul(sc)
        .ok_or_else(|| TranscodeError::Other("decoded sample count overflow".into()))?;
    managed
        .buf
        .samples()
        .get(..expected)
        .ok_or_else(|| TranscodeError::Other("decoded sample buffer shorter than expected".into()))
}

fn validate_time_base(
    time_base: Option<symphonia::core::units::TimeBase>,
) -> Result<(), TranscodeError> {
    if let Some(tb) = time_base {
        if tb.numer == 0 || tb.denom == 0 {
            return Err(TranscodeError::Other(
                "invalid zero time base in audio track".into(),
            ));
        }
    }
    Ok(())
}

fn ts_to_frames(
    ts: u64,
    time_base: Option<symphonia::core::units::TimeBase>,
    sample_rate: u32,
) -> Result<u64, TranscodeError> {
    if let Some(tb) = time_base {
        if tb.numer == 0 || tb.denom == 0 {
            return Err(TranscodeError::Other(
                "invalid zero time base in audio track".into(),
            ));
        }
        let t = tb.calc_time(ts);
        let secs = t.seconds as f64 + t.frac;
        let scaled = (secs * f64::from(sample_rate)).round();
        if !scaled.is_finite() || scaled < 0.0 || scaled > (u64::MAX as f64) {
            return Err(TranscodeError::Other(
                "timestamp frame count overflow".into(),
            ));
        }
        Ok(scaled as u64)
    } else {
        Ok(ts)
    }
}

pub fn execute_transcode<R: Read + Seek + Send + Sync + 'static, W: Write>(
    job: &TranscodeJob,
    input: R,
    output: W,
) -> Result<(), TranscodeError> {
    execute_transcode_media_source(job, Box::new(SourceWrapper::new(input)), output)
}

const MAX_CONSECUTIVE_RESETS: usize = 8;
const MAX_CONSECUTIVE_DECODE_ERRORS: usize = 64;
const MAX_TOTAL_DECODE_ERRORS: usize = 1024;
const RESAMPLE_CHUNK_SIZE: usize = 1024;

fn secs_to_frame(secs: f64, sample_rate: u32) -> Result<u64, TranscodeError> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(TranscodeError::Other(
            "time offset must be finite and non-negative".into(),
        ));
    }
    let scaled = (secs * f64::from(sample_rate)).round();
    if !scaled.is_finite() || scaled > (u64::MAX as f64) {
        return Err(TranscodeError::Other(
            "time offset frame count overflow".into(),
        ));
    }
    Ok(scaled as u64)
}

fn compute_total_out_frames_hint(
    track_n_frames: Option<u64>,
    start_frame: u64,
    end_frame: u64,
    has_end_sec: bool,
    src_sample_rate: u32,
    out_sample_rate: u32,
) -> Option<u64> {
    let total_frames_hint = track_n_frames.map(|n| {
        let capped_end = if has_end_sec { end_frame.min(n) } else { n };
        capped_end.saturating_sub(start_frame)
    });
    total_frames_hint.and_then(|frames| {
        if out_sample_rate != src_sample_rate {
            u128::from(frames)
                .checked_mul(u128::from(out_sample_rate))
                .map(|num| {
                    let den = u128::from(src_sample_rate);
                    (num + den / 2) / den
                })
                .and_then(|scaled| u64::try_from(scaled).ok())
        } else {
            Some(frames)
        }
    })
}

struct ResamplePipeline {
    resampler: Option<rubato::FastFixedIn<f32>>,
    pending_in: Vec<Vec<f32>>,
    in_cursor: usize,
    chunk_in: Vec<Vec<f32>>,
    chunk_out: Option<Vec<Vec<f32>>>,
    initial_delay_remaining: usize,
    total_in_frames: u64,
    total_out_emitted: u64,
}

impl ResamplePipeline {
    fn new(
        src_sample_rate: u32,
        out_sample_rate: u32,
        channels: usize,
    ) -> Result<Self, TranscodeError> {
        use rubato::Resampler;
        let needs_resample = out_sample_rate != src_sample_rate;
        let resampler = if needs_resample {
            let ratio = f64::from(out_sample_rate) / f64::from(src_sample_rate);
            Some(
                rubato::FastFixedIn::<f32>::new(
                    ratio,
                    1.0,
                    rubato::PolynomialDegree::Cubic,
                    RESAMPLE_CHUNK_SIZE,
                    channels,
                )
                .map_err(|e| TranscodeError::Resample(e.to_string()))?,
            )
        } else {
            None
        };
        let chunk_in = if needs_resample {
            vec![vec![0.0f32; RESAMPLE_CHUNK_SIZE]; channels]
        } else {
            Vec::new()
        };
        let initial_delay_remaining = resampler.as_ref().map_or(0, Resampler::output_delay);
        let chunk_out = resampler.as_ref().map(|r| r.output_buffer_allocate(true));
        Ok(Self {
            resampler,
            pending_in: vec![Vec::new(); channels],
            in_cursor: 0,
            chunk_in,
            chunk_out,
            initial_delay_remaining,
            total_in_frames: 0,
            total_out_emitted: 0,
        })
    }

    fn is_active(&self) -> bool {
        self.resampler.is_some()
    }

    fn process_chunk_and_emit<W: Write>(
        &mut self,
        out_bps: u16,
        max_emit: Option<usize>,
        int_samples: &mut [Vec<i32>],
        writer: &mut ContainerWriter<'_, W>,
    ) -> Result<usize, TranscodeError> {
        use rubato::Resampler;
        let Some(ref mut res) = self.resampler else {
            return Ok(0);
        };
        let Some(ref mut chunk_out) = self.chunk_out else {
            return Err(TranscodeError::Resample(
                "missing resampler output buffer".into(),
            ));
        };
        let (_in_frames, out_len) = res
            .process_into_buffer(&self.chunk_in, chunk_out, None)
            .map_err(|e| TranscodeError::Resample(e.to_string()))?;
        let skip = self.initial_delay_remaining.min(out_len);
        self.initial_delay_remaining -= skip;
        let avail = out_len - skip;
        let write_len = match max_emit {
            Some(limit) => avail.min(limit),
            None => avail,
        };
        if write_len > 0 {
            convert_f32_range_to_int_into(chunk_out, skip, write_len, out_bps, int_samples);
            writer.write_samples(int_samples, write_len)?;
            self.total_out_emitted = self.total_out_emitted.saturating_add(write_len as u64);
        }
        Ok(out_len)
    }

    fn push_and_process<W: Write>(
        &mut self,
        mapped_planar: &[Vec<f32>],
        num_frames: usize,
        out_channels: u16,
        out_bps: u16,
        int_samples: &mut [Vec<i32>],
        writer: &mut ContainerWriter<'_, W>,
    ) -> Result<(), TranscodeError> {
        if self.resampler.is_none() || num_frames == 0 {
            return Ok(());
        }
        let tc = usize::from(out_channels);
        for ch in 0..tc {
            if let (Some(src), Some(dst)) = (
                mapped_planar.get(ch).and_then(|v| v.get(..num_frames)),
                self.pending_in.get_mut(ch),
            ) {
                dst.extend_from_slice(src);
            }
        }
        self.total_in_frames = self.total_in_frames.saturating_add(num_frames as u64);

        loop {
            let available = self
                .pending_in
                .first()
                .map_or(0, |v| v.len().saturating_sub(self.in_cursor));
            if available < RESAMPLE_CHUNK_SIZE {
                break;
            }
            let chunk_end = self.in_cursor + RESAMPLE_CHUNK_SIZE;
            for ch in 0..tc {
                if let (Some(src), Some(dst)) = (
                    self.pending_in
                        .get(ch)
                        .and_then(|v| v.get(self.in_cursor..chunk_end)),
                    self.chunk_in.get_mut(ch),
                ) {
                    dst.copy_from_slice(src);
                }
            }
            self.in_cursor = chunk_end;
            self.process_chunk_and_emit(out_bps, None, int_samples, writer)?;
        }

        let pending_len = self.pending_in.first().map_or(0, Vec::len);
        if self.in_cursor == pending_len {
            for ch in 0..tc {
                if let Some(buf) = self.pending_in.get_mut(ch) {
                    buf.clear();
                }
            }
            self.in_cursor = 0;
        } else if self.in_cursor >= RESAMPLE_CHUNK_SIZE.saturating_mul(4) {
            let rem = pending_len.saturating_sub(self.in_cursor);
            for ch in 0..tc {
                if let Some(buf) = self.pending_in.get_mut(ch) {
                    buf.copy_within(self.in_cursor.., 0);
                    buf.truncate(rem);
                }
            }
            self.in_cursor = 0;
        }
        Ok(())
    }

    fn flush<W: Write>(
        &mut self,
        src_sample_rate: u32,
        out_sample_rate: u32,
        out_channels: u16,
        out_bps: u16,
        int_samples: &mut [Vec<i32>],
        writer: &mut ContainerWriter<'_, W>,
    ) -> Result<(), TranscodeError> {
        use rubato::Resampler;
        if self.resampler.is_none() {
            return Ok(());
        }
        let tc = usize::from(out_channels);
        let pending_len = self.pending_in.first().map_or(0, Vec::len);
        let rem = pending_len.saturating_sub(self.in_cursor);
        let expected_total_out = u64::try_from(
            (u128::from(self.total_in_frames) * u128::from(out_sample_rate)
                + u128::from(src_sample_rate) / 2)
                / u128::from(src_sample_rate),
        )
        .unwrap_or(u64::MAX);

        if rem > 0 {
            for ch in 0..tc {
                if let Some(dst) = self.chunk_in.get_mut(ch) {
                    dst.fill(0.0);
                    if let Some(src) = self
                        .pending_in
                        .get(ch)
                        .and_then(|v| v.get(self.in_cursor..))
                    {
                        let copy_len = src.len().min(dst.len());
                        if let (Some(s_part), Some(d_part)) =
                            (src.get(..copy_len), dst.get_mut(..copy_len))
                        {
                            d_part.copy_from_slice(s_part);
                        }
                    }
                }
            }
            let remaining_needed =
                usize::try_from(expected_total_out.saturating_sub(self.total_out_emitted))
                    .unwrap_or(usize::MAX);
            self.process_chunk_and_emit(out_bps, Some(remaining_needed), int_samples, writer)?;
        }

        // Drain any frames still buffered inside the resampler's output_delay(),
        // including when rem == 0 or zero-padding in the final chunk was shorter
        // than output_delay().
        while self.total_out_emitted < expected_total_out {
            for ch in 0..tc {
                if let Some(dst) = self.chunk_in.get_mut(ch) {
                    dst.fill(0.0);
                }
            }
            let remaining_needed =
                usize::try_from(expected_total_out.saturating_sub(self.total_out_emitted))
                    .unwrap_or(usize::MAX);
            let out_len =
                self.process_chunk_and_emit(out_bps, Some(remaining_needed), int_samples, writer)?;
            if out_len == 0 {
                break;
            }
        }

        for buf in &mut self.pending_in {
            buf.clear();
        }
        self.in_cursor = 0;
        self.total_in_frames = 0;
        self.total_out_emitted = 0;
        if let Some(ref mut res) = self.resampler {
            res.reset();
            self.initial_delay_remaining = res.output_delay();
        }
        Ok(())
    }
}

fn write_container_header<W: Write>(
    output: &mut W,
    output_format: OutputContainer,
    out_sample_rate: u32,
    out_channels: u16,
    out_bps: u16,
    total_out_frames_hint: Option<u64>,
) -> Result<(), TranscodeError> {
    if output_format == OutputContainer::Wav {
        write_wav_header(
            output,
            out_sample_rate,
            out_channels,
            out_bps,
            total_out_frames_hint,
        )?;
        output.flush()?;
    } else if output_format == OutputContainer::Flac {
        let flac_total = total_out_frames_hint
            .filter(|&n| n <= MAX_FLAC_CODED_NUMBER)
            .unwrap_or(0);
        write_flac_header(output, out_sample_rate, out_channels, out_bps, flac_total)?;
        output.flush()?;
    }
    Ok(())
}

fn map_interleaved_f32_to_planar(
    samples: &[f32],
    slice_start: usize,
    slice_end: usize,
    sc: usize,
    mapped_planar: &mut [Vec<f32>],
) {
    let tc = mapped_planar.len();
    let sliced_frames = slice_end.saturating_sub(slice_start);
    for ch_vec in mapped_planar.iter_mut() {
        ch_vec.clear();
        ch_vec.reserve(sliced_frames);
    }

    for f in slice_start..slice_end {
        let frame_base = f.saturating_mul(sc);
        if sc == tc {
            for (ch, ch_vec) in mapped_planar.iter_mut().enumerate() {
                let s = samples.get(frame_base + ch).copied().unwrap_or(0.0);
                ch_vec.push(s);
            }
        } else if sc == 2 && tc == 1 {
            let left = samples.get(frame_base).copied().unwrap_or(0.0);
            let right = samples.get(frame_base + 1).copied().unwrap_or(0.0);
            if let Some(ch_vec) = mapped_planar.first_mut() {
                ch_vec.push((left + right) * 0.5);
            }
        } else if sc == 1 && tc == 2 {
            let mono = samples.get(frame_base).copied().unwrap_or(0.0);
            if let Some(ch0) = mapped_planar.first_mut() {
                ch0.push(mono);
            }
            if let Some(ch1) = mapped_planar.get_mut(1) {
                ch1.push(mono);
            }
        } else {
            for (ch, ch_vec) in mapped_planar.iter_mut().enumerate() {
                let s = if ch < sc {
                    samples.get(frame_base + ch).copied().unwrap_or(0.0)
                } else {
                    0.0
                };
                ch_vec.push(s);
            }
        }
    }
}

fn map_interleaved_i32_to_planar(
    samples: &[i32],
    num_frames: usize,
    sc: usize,
    int_samples: &mut [Vec<i32>],
) {
    let tc = int_samples.len();
    for ch_vec in int_samples.iter_mut() {
        ch_vec.clear();
        ch_vec.reserve(num_frames);
    }

    for f in 0..num_frames {
        let frame_base = f.saturating_mul(sc);
        if sc == tc {
            for (ch, ch_vec) in int_samples.iter_mut().enumerate() {
                let s = samples.get(frame_base + ch).copied().unwrap_or(0);
                ch_vec.push(s);
            }
        } else if sc == 2 && tc == 1 {
            let left = samples.get(frame_base).copied().unwrap_or(0);
            let right = samples.get(frame_base + 1).copied().unwrap_or(0);
            let avg = ((i64::from(left) + i64::from(right)) / 2) as i32;
            if let Some(ch_vec) = int_samples.first_mut() {
                ch_vec.push(avg);
            }
        } else if sc == 1 && tc == 2 {
            let mono = samples.get(frame_base).copied().unwrap_or(0);
            if let Some(ch0) = int_samples.first_mut() {
                ch0.push(mono);
            }
            if let Some(ch1) = int_samples.get_mut(1) {
                ch1.push(mono);
            }
        } else {
            for (ch, ch_vec) in int_samples.iter_mut().enumerate() {
                let s = if ch < sc {
                    samples.get(frame_base + ch).copied().unwrap_or(0)
                } else {
                    0
                };
                ch_vec.push(s);
            }
        }
    }
}

fn decode_raw_pcm_sample_i32(
    bytes: &[u8],
    bps: u16,
    endian_big: bool,
) -> Result<i32, TranscodeError> {
    match bps {
        8 => {
            let b = bytes
                .first()
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 8-bit raw PCM sample".into()))?;
            Ok(i32::from(b as i8))
        }
        16 => {
            let b0 = bytes
                .first()
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 16-bit raw PCM sample".into()))?;
            let b1 = bytes
                .get(1)
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 16-bit raw PCM sample".into()))?;
            let v = if endian_big {
                i16::from_be_bytes([b0, b1])
            } else {
                i16::from_le_bytes([b0, b1])
            };
            Ok(i32::from(v))
        }
        24 => {
            let b0 = bytes
                .first()
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 24-bit raw PCM sample".into()))?;
            let b1 = bytes
                .get(1)
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 24-bit raw PCM sample".into()))?;
            let b2 = bytes
                .get(2)
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 24-bit raw PCM sample".into()))?;
            let packed = if endian_big {
                i32::from_be_bytes([b0, b1, b2, 0])
            } else {
                i32::from_le_bytes([0, b0, b1, b2])
            };
            Ok(packed >> 8)
        }
        32 => {
            let b0 = bytes
                .first()
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 32-bit raw PCM sample".into()))?;
            let b1 = bytes
                .get(1)
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 32-bit raw PCM sample".into()))?;
            let b2 = bytes
                .get(2)
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 32-bit raw PCM sample".into()))?;
            let b3 = bytes
                .get(3)
                .copied()
                .ok_or_else(|| TranscodeError::Other("truncated 32-bit raw PCM sample".into()))?;
            Ok(if endian_big {
                i32::from_be_bytes([b0, b1, b2, b3])
            } else {
                i32::from_le_bytes([b0, b1, b2, b3])
            })
        }
        _ => Err(TranscodeError::Other(format!(
            "unsupported raw PCM bits per sample {bps}"
        ))),
    }
}

fn raw_i32_to_unit_f32(sample: i32, bps: u16) -> f32 {
    match bps {
        8 => (sample as f32) / 128.0_f32,
        16 => (sample as f32) / 32_768.0_f32,
        24 => (sample as f32) / 8_388_608.0_f32,
        32 => ((sample as f64) / 2_147_483_648.0_f64) as f32,
        _ => 0.0,
    }
}

struct RawPcmScratch {
    raw_i32: Vec<i32>,
    raw_f32: Vec<f32>,
    mapped_planar: Vec<Vec<f32>>,
    int_samples: Vec<Vec<i32>>,
}

impl RawPcmScratch {
    fn new(tc: usize) -> Self {
        Self {
            raw_i32: Vec::new(),
            raw_f32: Vec::new(),
            mapped_planar: vec![Vec::new(); tc],
            int_samples: vec![Vec::new(); tc],
        }
    }
}

fn emit_raw_pcm_slice<W: Write>(
    raw_fmt: RawPcmInputFormat,
    frame_slice: &[u8],
    sliced_frames: usize,
    resample_state: &mut ResamplePipeline,
    scratch: &mut RawPcmScratch,
    writer: &mut ContainerWriter<'_, W>,
) -> Result<(), TranscodeError> {
    let sc = usize::from(raw_fmt.channels);
    let src_bps = raw_fmt.bits_per_sample;
    let bytes_per_sample = usize::from(src_bps / 8);
    let out_channels = writer.channels;
    let out_bps = writer.bps;
    let use_direct_int = !resample_state.is_active() && out_bps == src_bps;

    if use_direct_int {
        scratch.raw_i32.clear();
        scratch.raw_i32.reserve(sliced_frames.saturating_mul(sc));
        for sample_bytes in frame_slice.chunks_exact(bytes_per_sample) {
            scratch.raw_i32.push(decode_raw_pcm_sample_i32(
                sample_bytes,
                src_bps,
                raw_fmt.endian_big,
            )?);
        }
        map_interleaved_i32_to_planar(
            &scratch.raw_i32,
            sliced_frames,
            sc,
            &mut scratch.int_samples,
        );
        writer.write_samples(&scratch.int_samples, sliced_frames)?;
        return Ok(());
    }

    scratch.raw_f32.clear();
    scratch.raw_f32.reserve(sliced_frames.saturating_mul(sc));
    for sample_bytes in frame_slice.chunks_exact(bytes_per_sample) {
        let sample_i32 = decode_raw_pcm_sample_i32(sample_bytes, src_bps, raw_fmt.endian_big)?;
        scratch
            .raw_f32
            .push(raw_i32_to_unit_f32(sample_i32, src_bps));
    }
    map_interleaved_f32_to_planar(
        &scratch.raw_f32,
        0,
        sliced_frames,
        sc,
        &mut scratch.mapped_planar,
    );
    if resample_state.is_active() {
        resample_state.push_and_process(
            &scratch.mapped_planar,
            sliced_frames,
            out_channels,
            out_bps,
            &mut scratch.int_samples,
            writer,
        )?;
    } else {
        convert_f32_to_int_into(
            &scratch.mapped_planar,
            sliced_frames,
            out_bps,
            &mut scratch.int_samples,
        );
        writer.write_samples(&scratch.int_samples, sliced_frames)?;
    }
    Ok(())
}

fn execute_raw_pcm_transcode<W: Write>(
    job: &TranscodeJob,
    raw_fmt: RawPcmInputFormat,
    mut media_source: Box<dyn MediaSource>,
    mut output: W,
) -> Result<(), TranscodeError> {
    let src_sample_rate = raw_fmt.sample_rate;
    let src_channels = raw_fmt.channels;
    let src_bps = raw_fmt.bits_per_sample;
    validate_audio_params(src_sample_rate, src_channels, src_bps)?;

    let out_channels = job.target_channels.unwrap_or(src_channels);
    let out_sample_rate = job.target_sample_rate.unwrap_or(src_sample_rate);
    let out_bps = job.bits_per_sample.unwrap_or(src_bps);
    validate_audio_params(out_sample_rate, out_channels, out_bps)?;
    if job.output_format == OutputContainer::Flac && out_sample_rate > MAX_FLAC_SAMPLE_RATE {
        return Err(TranscodeError::Other(format!(
            "FLAC sample rate {out_sample_rate} exceeds 20-bit limit"
        )));
    }

    let start_frame = match job.start_sec {
        Some(s) => secs_to_frame(s, src_sample_rate)?,
        None => 0,
    };
    let end_frame = match job.end_sec {
        Some(s) => secs_to_frame(s, src_sample_rate)?,
        None => u64::MAX,
    };
    if end_frame <= start_frame {
        write_container_header(
            &mut output,
            job.output_format,
            out_sample_rate,
            out_channels,
            out_bps,
            Some(0),
        )?;
        return Ok(());
    }

    let bytes_per_sample = usize::from(src_bps / 8);
    let frame_bytes = usize::from(src_channels)
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| TranscodeError::Other("raw PCM frame size overflow".into()))?;
    let track_n_frames = media_source
        .byte_len()
        .map(|len| len / (frame_bytes as u64));

    let mut current_frame_pos: u64 = 0;
    if start_frame > 0 && media_source.is_seekable() {
        let byte_offset = start_frame
            .checked_mul(frame_bytes as u64)
            .ok_or_else(|| TranscodeError::Other("raw PCM seek byte offset overflow".into()))?;
        media_source.seek(std::io::SeekFrom::Start(byte_offset))?;
        current_frame_pos = start_frame;
    }

    let total_out_frames_hint = compute_total_out_frames_hint(
        track_n_frames,
        start_frame,
        end_frame,
        job.end_sec.is_some(),
        src_sample_rate,
        out_sample_rate,
    );
    write_container_header(
        &mut output,
        job.output_format,
        out_sample_rate,
        out_channels,
        out_bps,
        total_out_frames_hint,
    )?;

    let tc = usize::from(out_channels);
    let mut resample_state = ResamplePipeline::new(src_sample_rate, out_sample_rate, tc)?;
    let mut scratch = RawPcmScratch::new(tc);
    let mut flac_frame_idx = 0u64;
    let mut flac_pending = vec![Vec::<i32>::new(); tc];

    const RAW_CHUNK_FRAMES: usize = 4096;
    let buf_bytes = RAW_CHUNK_FRAMES
        .checked_mul(frame_bytes)
        .ok_or_else(|| TranscodeError::Other("raw PCM buffer size overflow".into()))?;
    let mut raw_buf = vec![0u8; buf_bytes];
    let mut valid_len = 0usize;

    loop {
        let read_target = raw_buf
            .get_mut(valid_len..)
            .ok_or_else(|| TranscodeError::Other("raw PCM buffer cursor out of range".into()))?;
        let n = match media_source.read(read_target) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(TranscodeError::Io(e)),
        };
        valid_len += n;
        let complete_frames = valid_len / frame_bytes;
        if complete_frames == 0 {
            continue;
        }
        let used_bytes = complete_frames * frame_bytes;

        let pkt_start = current_frame_pos;
        let pkt_end = pkt_start.saturating_add(complete_frames as u64);
        current_frame_pos = pkt_end;

        if pkt_end > start_frame && pkt_start < end_frame {
            let slice_start = if start_frame > pkt_start {
                (start_frame - pkt_start) as usize
            } else {
                0
            };
            let slice_end = if end_frame < pkt_end {
                (end_frame - pkt_start) as usize
            } else {
                complete_frames
            };
            if slice_start < slice_end {
                let sliced_frames = slice_end - slice_start;
                let byte_start = slice_start * frame_bytes;
                let byte_end = slice_end * frame_bytes;
                let frame_slice = raw_buf
                    .get(byte_start..byte_end)
                    .ok_or_else(|| TranscodeError::Other("raw PCM slice out of bounds".into()))?;
                let mut writer = ContainerWriter {
                    out: &mut output,
                    format: job.output_format,
                    channels: out_channels,
                    bps: out_bps,
                    flac_frame_idx: &mut flac_frame_idx,
                    flac_pending: &mut flac_pending,
                };
                emit_raw_pcm_slice(
                    raw_fmt,
                    frame_slice,
                    sliced_frames,
                    &mut resample_state,
                    &mut scratch,
                    &mut writer,
                )?;
            }
        }

        let rem_bytes = valid_len - used_bytes;
        if rem_bytes > 0 {
            raw_buf.copy_within(used_bytes..valid_len, 0);
        }
        valid_len = rem_bytes;

        if pkt_end >= end_frame {
            break;
        }
    }

    {
        let mut writer = ContainerWriter {
            out: &mut output,
            format: job.output_format,
            channels: out_channels,
            bps: out_bps,
            flac_frame_idx: &mut flac_frame_idx,
            flac_pending: &mut flac_pending,
        };
        resample_state.flush(
            src_sample_rate,
            out_sample_rate,
            out_channels,
            out_bps,
            &mut scratch.int_samples,
            &mut writer,
        )?;
        writer.flush_flac_tail()?;
    }

    output.flush()?;
    Ok(())
}

fn execute_transcode_media_source<W: Write>(
    job: &TranscodeJob,
    media_source: Box<dyn MediaSource>,
    mut output: W,
) -> Result<(), TranscodeError> {
    if let Some(raw_fmt) = job.raw_input {
        return execute_raw_pcm_transcode(job, raw_fmt, media_source, output);
    }
    let is_seekable = media_source.is_seekable();
    let mss = MediaSourceStream::new(media_source, Default::default());
    let hint = Hint::new();
    let meta_opts = Default::default();
    let fmt_opts = Default::default();
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .map_err(TranscodeError::Symphonia)?;
    let mut format = probed.format;

    let (
        mut track_id,
        mut src_sample_rate,
        src_channels,
        codec_params,
        track_n_frames,
        mut time_base,
    ) = {
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
        let src_channels = match track.codec_params.channels {
            Some(c) => u16::try_from(c.count())
                .map_err(|_| TranscodeError::Other("source channel count overflow".into()))?,
            None => 2,
        };
        let codec_params = track.codec_params.clone();
        let n_frames = track.codec_params.n_frames;
        let time_base = track.codec_params.time_base;
        validate_time_base(time_base)?;
        (
            track_id,
            src_sample_rate,
            src_channels,
            codec_params,
            n_frames,
            time_base,
        )
    };

    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&src_sample_rate) {
        return Err(TranscodeError::Other(format!(
            "unsupported source sample rate {src_sample_rate}: \
             must be {MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE}"
        )));
    }
    if !(1..=MAX_CHANNELS).contains(&src_channels) {
        return Err(TranscodeError::Other(format!(
            "unsupported source channel count {src_channels}: must be 1..={MAX_CHANNELS}"
        )));
    }

    let out_channels = job.target_channels.unwrap_or(src_channels);
    let out_sample_rate = job.target_sample_rate.unwrap_or(src_sample_rate);
    let out_bps = job.bits_per_sample.unwrap_or(16);
    validate_audio_params(out_sample_rate, out_channels, out_bps)?;
    if job.output_format == OutputContainer::Flac && out_sample_rate > MAX_FLAC_SAMPLE_RATE {
        return Err(TranscodeError::Other(format!(
            "FLAC sample rate {out_sample_rate} exceeds 20-bit limit"
        )));
    }

    let dec_opts = Default::default();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &dec_opts)
        .map_err(TranscodeError::Symphonia)?;

    let start_frame = match job.start_sec {
        Some(s) => secs_to_frame(s, src_sample_rate)?,
        None => 0,
    };
    let end_frame = match job.end_sec {
        Some(s) => secs_to_frame(s, src_sample_rate)?,
        None => u64::MAX,
    };

    if end_frame <= start_frame {
        write_container_header(
            &mut output,
            job.output_format,
            out_sample_rate,
            out_channels,
            out_bps,
            Some(0),
        )?;
        return Ok(());
    }

    let mut current_frame_pos: u64 = 0;
    if start_frame > 0 && is_seekable {
        let start_sec = job.start_sec.unwrap_or(0.0);
        if !start_sec.is_finite() || start_sec < 0.0 {
            return Err(TranscodeError::Other(
                "start offset must be finite and non-negative".into(),
            ));
        }
        let whole = start_sec.trunc();
        let frac = start_sec.fract();
        if !whole.is_finite() || whole > (u64::MAX as f64) || !frac.is_finite() || frac < 0.0 {
            return Err(TranscodeError::Other(
                "start offset out of range for seek".into(),
            ));
        }
        let (secs_u64, frac_f64) = if frac >= 1.0 {
            ((whole as u64).saturating_add(1), 0.0)
        } else {
            (whole as u64, frac)
        };
        let seek_time = symphonia::core::units::Time::new(secs_u64, frac_f64);
        let seeked_to = format
            .seek(
                symphonia::core::formats::SeekMode::Accurate,
                SeekTo::Time {
                    time: seek_time,
                    track_id: Some(track_id),
                },
            )
            .map_err(TranscodeError::Symphonia)?;
        current_frame_pos = ts_to_frames(seeked_to.actual_ts, time_base, src_sample_rate)?;
        decoder.reset();
    }

    let total_out_frames_hint = compute_total_out_frames_hint(
        track_n_frames,
        start_frame,
        end_frame,
        job.end_sec.is_some(),
        src_sample_rate,
        out_sample_rate,
    );
    write_container_header(
        &mut output,
        job.output_format,
        out_sample_rate,
        out_channels,
        out_bps,
        total_out_frames_hint,
    )?;

    let tc = usize::from(out_channels);
    let mut resample_state = ResamplePipeline::new(src_sample_rate, out_sample_rate, tc)?;

    let mut mapped_planar = vec![Vec::<f32>::new(); tc];
    let mut int_samples = vec![Vec::<i32>::new(); tc];
    let mut flac_frame_idx = 0u64;
    let mut flac_pending = vec![Vec::<i32>::new(); tc];

    let mut sample_buf: Option<ManagedSampleBuffer<f32>> = None;
    let mut sample_buf_i16: Option<ManagedSampleBuffer<i16>> = None;
    let mut out_bytes = Vec::<u8>::new();

    let mut direct_i16 = !resample_state.is_active()
        && out_bps == 16
        && matches!(
            job.output_format,
            OutputContainer::RawPcmLe | OutputContainer::RawPcmBe
        );

    let mut consecutive_resets = 0usize;
    let mut consecutive_decode_errors = 0usize;
    let mut total_decode_errors = 0usize;
    let mut last_decode_error: Option<String> = None;
    let mut saw_decoded_audio = false;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                consecutive_resets += 1;
                if consecutive_resets > MAX_CONSECUTIVE_RESETS {
                    return Err(TranscodeError::Other(
                        "exceeded maximum consecutive stream resets".into(),
                    ));
                }
                {
                    let mut writer = ContainerWriter {
                        out: &mut output,
                        format: job.output_format,
                        channels: out_channels,
                        bps: out_bps,
                        flac_frame_idx: &mut flac_frame_idx,
                        flac_pending: &mut flac_pending,
                    };
                    resample_state.flush(
                        src_sample_rate,
                        out_sample_rate,
                        out_channels,
                        out_bps,
                        &mut int_samples,
                        &mut writer,
                    )?;
                }
                let track = format
                    .tracks()
                    .iter()
                    .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
                    .ok_or_else(|| {
                        TranscodeError::Other("no supported audio track after stream reset".into())
                    })?;
                let new_track_id = track.id;
                let new_src_sr = track.codec_params.sample_rate.ok_or_else(|| {
                    TranscodeError::Other("unknown sample rate after stream reset".into())
                })?;
                if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&new_src_sr) {
                    return Err(TranscodeError::Other(format!(
                        "unsupported source sample rate {new_src_sr} after stream reset"
                    )));
                }
                let new_src_ch = match track.codec_params.channels {
                    Some(c) => u16::try_from(c.count()).map_err(|_| {
                        TranscodeError::Other("channel count overflow after stream reset".into())
                    })?,
                    None => 2,
                };
                if !(1..=MAX_CHANNELS).contains(&new_src_ch) {
                    return Err(TranscodeError::Other(format!(
                        "unsupported source channel count {new_src_ch} after stream reset"
                    )));
                }
                let new_codec_params = track.codec_params.clone();
                let new_time_base = track.codec_params.time_base;
                validate_time_base(new_time_base)?;
                decoder = symphonia::default::get_codecs()
                    .make(&new_codec_params, &dec_opts)
                    .map_err(TranscodeError::Symphonia)?;
                track_id = new_track_id;
                time_base = new_time_base;
                if new_src_sr != src_sample_rate {
                    src_sample_rate = new_src_sr;
                    resample_state = ResamplePipeline::new(src_sample_rate, out_sample_rate, tc)?;
                    direct_i16 = !resample_state.is_active()
                        && out_bps == 16
                        && matches!(
                            job.output_format,
                            OutputContainer::RawPcmLe | OutputContainer::RawPcmBe
                        );
                }
                continue;
            }
            Err(e) => return Err(TranscodeError::Symphonia(e)),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => {
                consecutive_decode_errors = 0;
                d
            }
            Err(symphonia::core::errors::Error::DecodeError(msg)) => {
                consecutive_decode_errors += 1;
                total_decode_errors += 1;
                last_decode_error = Some(msg.to_string());
                if consecutive_decode_errors >= MAX_CONSECUTIVE_DECODE_ERRORS
                    || total_decode_errors >= MAX_TOTAL_DECODE_ERRORS
                {
                    return Err(TranscodeError::Symphonia(
                        symphonia::core::errors::Error::DecodeError(msg),
                    ));
                }
                continue;
            }
            Err(e) => return Err(TranscodeError::Symphonia(e)),
        };

        let n_frames = decoded.frames();
        if n_frames == 0 {
            continue;
        }
        saw_decoded_audio = true;
        consecutive_resets = 0;

        let pkt_start = if packet.ts() > 0 {
            ts_to_frames(packet.ts(), time_base, src_sample_rate)?
        } else {
            current_frame_pos
        };
        let pkt_end = pkt_start.saturating_add(n_frames as u64);
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
        let sc = decoded.spec().channels.count();
        if sc == 0 {
            return Err(TranscodeError::Other(
                "decoded audio frame has zero channels".into(),
            ));
        }

        if direct_i16 && sc == tc {
            let samples = copy_decoded_interleaved(&mut sample_buf_i16, decoded)?;
            out_bytes.clear();
            out_bytes.reserve(sliced_frames.saturating_mul(sc).saturating_mul(2));
            for f in slice_start..slice_end {
                let frame_base = f.saturating_mul(sc);
                for ch in 0..sc {
                    let s = samples.get(frame_base + ch).copied().unwrap_or(0);
                    if job.output_format == OutputContainer::RawPcmLe {
                        out_bytes.extend_from_slice(&s.to_le_bytes());
                    } else {
                        out_bytes.extend_from_slice(&s.to_be_bytes());
                    }
                }
            }
            output.write_all(&out_bytes)?;
        } else {
            let samples = copy_decoded_interleaved(&mut sample_buf, decoded)?;
            map_interleaved_f32_to_planar(samples, slice_start, slice_end, sc, &mut mapped_planar);

            if resample_state.is_active() {
                let mut writer = ContainerWriter {
                    out: &mut output,
                    format: job.output_format,
                    channels: out_channels,
                    bps: out_bps,
                    flac_frame_idx: &mut flac_frame_idx,
                    flac_pending: &mut flac_pending,
                };
                resample_state.push_and_process(
                    &mapped_planar,
                    sliced_frames,
                    out_channels,
                    out_bps,
                    &mut int_samples,
                    &mut writer,
                )?;
            } else {
                convert_f32_to_int_into(&mapped_planar, sliced_frames, out_bps, &mut int_samples);
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

    if !saw_decoded_audio {
        if let Some(msg) = last_decode_error {
            return Err(TranscodeError::Other(format!(
                "stream contained only corrupt audio frames: {msg}"
            )));
        }
    }

    {
        let mut writer = ContainerWriter {
            out: &mut output,
            format: job.output_format,
            channels: out_channels,
            bps: out_bps,
            flac_frame_idx: &mut flac_frame_idx,
            flac_pending: &mut flac_pending,
        };
        resample_state.flush(
            src_sample_rate,
            out_sample_rate,
            out_channels,
            out_bps,
            &mut int_samples,
            &mut writer,
        )?;
        writer.flush_flac_tail()?;
    }

    output.flush()?;
    Ok(())
}

fn sanitize_unit_sample(sample: f32) -> f64 {
    if sample.is_finite() {
        f64::from(sample).clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn convert_f32_range_to_int_into(
    planar: &[Vec<f32>],
    start_frame: usize,
    num_frames: usize,
    bps: u16,
    out: &mut [Vec<i32>],
) {
    let end_frame = start_frame.saturating_add(num_frames);
    let tc = planar.len().min(out.len());
    for ch in 0..tc {
        let Some(src) = planar.get(ch).and_then(|v| v.get(start_frame..end_frame)) else {
            continue;
        };
        let Some(dst) = out.get_mut(ch) else {
            continue;
        };
        dst.clear();
        dst.reserve(num_frames);
        match bps {
            24 => {
                for &sample in src {
                    let v = (sanitize_unit_sample(sample) * 8_388_607.0_f64)
                        .round()
                        .clamp(-8_388_608.0_f64, 8_388_607.0_f64)
                        as i32;
                    dst.push(v);
                }
            }
            32 => {
                for &sample in src {
                    let v = (sanitize_unit_sample(sample) * 2_147_483_647.0_f64)
                        .round()
                        .clamp(-2_147_483_648.0_f64, 2_147_483_647.0_f64)
                        as i32;
                    dst.push(v);
                }
            }
            8 => {
                for &sample in src {
                    let v = (sanitize_unit_sample(sample) * 127.0_f64)
                        .round()
                        .clamp(-128.0_f64, 127.0_f64) as i32;
                    dst.push(v);
                }
            }
            _ => {
                for &sample in src {
                    let v = (sanitize_unit_sample(sample) * 32_767.0_f64)
                        .round()
                        .clamp(-32_768.0_f64, 32_767.0_f64) as i32;
                    dst.push(v);
                }
            }
        }
    }
}

fn convert_f32_to_int_into(planar: &[Vec<f32>], num_frames: usize, bps: u16, out: &mut [Vec<i32>]) {
    convert_f32_range_to_int_into(planar, 0, num_frames, bps, out);
}

fn is_broken_pipe(err: &TranscodeError) -> bool {
    let mut curr: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = curr {
        if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
            if io_err.kind() == std::io::ErrorKind::BrokenPipe {
                return true;
            }
        }
        curr = e.source();
    }
    match err {
        TranscodeError::Io(io_err) if io_err.kind() == std::io::ErrorKind::BrokenPipe => true,
        TranscodeError::Symphonia(symphonia::core::errors::Error::IoError(io_err))
            if io_err.kind() == std::io::ErrorKind::BrokenPipe =>
        {
            true
        }
        _ => false,
    }
}

fn execute_job_to_writer<W: Write>(job: &TranscodeJob, out: &mut W) -> Result<(), TranscodeError> {
    if let Some(ref path) = job.input_path {
        let file = std::fs::File::open(path)?;
        if file.metadata().map(|m| m.is_file()).unwrap_or(false) {
            execute_transcode(job, file, out)?;
        } else {
            let ro_file = symphonia::core::io::ReadOnlySource::new(file);
            execute_transcode_media_source(job, Box::new(ro_file), out)?;
        }
    } else {
        // Stdin is unseekable: stream unbounded; execute_transcode_media_source
        // forward-decode-skips any start_sec offset without truncating or spooling.
        let ro_stdin = symphonia::core::io::ReadOnlySource::new(std::io::stdin());
        execute_transcode_media_source(job, Box::new(ro_stdin), out)?;
    }
    Ok(())
}

fn run(argv0: &OsStr, args: &[OsString]) -> Result<(), TranscodeError> {
    let job = parse_cli_invocation_os(argv0, args)?;
    if let Some(ref out_path) = job.output_path {
        let file = std::fs::File::create(out_path)?;
        let mut out_handle = std::io::BufWriter::with_capacity(65536, file);
        execute_job_to_writer(&job, &mut out_handle)?;
        out_handle.flush()?;
    } else {
        let stdout = std::io::stdout();
        let mut out_handle = std::io::BufWriter::with_capacity(65536, stdout.lock());
        execute_job_to_writer(&job, &mut out_handle)?;
        out_handle.flush()?;
    }
    Ok(())
}

fn write_usage<W: Write>(out: &mut W) -> std::io::Result<()> {
    writeln!(
        out,
        "Usage: lms-transcode [--mode <faad|flac|wvunpack|sox>] [options] <input>"
    )?;
    writeln!(out, "Options:")?;
    writeln!(
        out,
        "  --mode <mode>      Emulate faad, flac, wvunpack, or sox CLI mode"
    )?;
    writeln!(out, "  -h, --help         Display this help message")?;
    out.flush()
}

fn main() -> std::process::ExitCode {
    let mut args_os = std::env::args_os();
    let argv0 = args_os
        .next()
        .unwrap_or_else(|| OsString::from("lms-transcode"));
    let cli_args: Vec<OsString> = args_os.collect();

    match run(&argv0, &cli_args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(TranscodeError::Help) => {
            let stdout = std::io::stdout();
            let mut out_handle = stdout.lock();
            match write_usage(&mut out_handle) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(io_err) if io_err.kind() == std::io::ErrorKind::BrokenPipe => {
                    std::process::ExitCode::SUCCESS
                }
                Err(io_err) => {
                    let stderr = std::io::stderr();
                    let mut err_handle = stderr.lock();
                    let _ = writeln!(err_handle, "lms-transcode error: {io_err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            if is_broken_pipe(&e) {
                return std::process::ExitCode::SUCCESS;
            }
            let stderr = std::io::stderr();
            let mut err_handle = stderr.lock();
            let _ = writeln!(err_handle, "lms-transcode error: {e}");
            std::process::ExitCode::FAILURE
        }
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
                "-q",
                "-w",
                "-f",
                "2",
                "-b",
                "1",
                "-j",
                "10",
                "-e",
                "30",
                "/tmp/in.m4a",
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
            output_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: Some(0.25),
            end_sec: Some(0.75),
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        assert_eq!(out.len(), 88200);
    }

    #[test]
    fn test_wvunpack_and_sox_parsing() {
        let job_wv = parse_cli_invocation(
            "wvunpack",
            &["/tmp/in.wv", "-rq", "--skip=10", "--until=20", "-o", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(job_wv.output_format, OutputContainer::RawPcmLe);
        assert_eq!(job_wv.input_path, Some(PathBuf::from("/tmp/in.wv")));
        assert_eq!(job_wv.output_path, None);
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
        assert_eq!(job_sox.input_path, Some(PathBuf::from("/tmp/in.ogg")));
        assert_eq!(job_sox.output_path, None);
        assert_eq!(job_sox.start_sec, Some(10.0));
        assert_eq!(job_sox.end_sec, Some(40.0));
    }

    #[test]
    fn test_flac_encoding_and_symphonia_roundtrip() {
        let wav_bytes = generate_test_wav_44100_stereo(0.2);
        let job = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::Flac,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
        };
        let mut flac_out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut flac_out).unwrap();
        assert!(flac_out.starts_with(b"fLaC"));

        // Roundtrip verify by decoding the generated FLAC with Symphonia
        let decode_job = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
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
        assert_eq!(job.input_path, Some(PathBuf::from("/tmp/in.raw")));

        let job_faad_mode = parse_cli_invocation(
            "lms-transcode",
            &["--mode=faad", "-q", "-w", "-f", "1", "/tmp/in.aac"].map(String::from),
        )
        .unwrap();
        assert_eq!(job_faad_mode.output_format, OutputContainer::Wav);
        assert_eq!(job_faad_mode.input_path, Some(PathBuf::from("/tmp/in.aac")));
    }

    #[test]
    fn test_wav_output_container_roundtrip() {
        let wav_bytes = generate_test_wav_44100_stereo(0.5);
        let job = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::Wav,
            start_sec: Some(0.1),
            end_sec: Some(0.4),
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // 44-byte WAV header +
        // (0.3s * 44100 = 13230 frames * 4 bytes = 52920 bytes) = 52964 bytes
        assert_eq!(out.len(), 44 + 52920);
        assert!(out.starts_with(b"RIFF"));
    }

    #[test]
    fn test_raw_pcm_be_transcode() {
        let wav_bytes = generate_test_wav_44100_stereo(0.1);
        let job = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::RawPcmBe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
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
            output_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: Some(48000),
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // Expected ~0.5s * 48000 frames = 24000 frames * 4 bytes = 96000 bytes
        // (within small window)
        let out_frames = out.len() / 4;
        let diff = (out_frames as i64 - 24000).abs();
        assert!(
            diff < 50,
            "resampled frames {} deviates from expected 24000",
            out_frames
        );
    }

    #[test]
    fn test_downmix_stereo_to_mono() {
        let wav_bytes = generate_test_wav_44100_stereo(0.25);
        let job = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(1),
            bits_per_sample: Some(16),
            raw_input: None,
        };
        let mut out = Vec::new();
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut out).unwrap();
        // 0.25s * 44100 = 11025 frames * 1 channel * 2 bytes = 22050 bytes
        assert_eq!(out.len(), 22050);
    }

    #[test]
    fn test_broken_pipe_detection() {
        let err_io = TranscodeError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ));
        assert!(is_broken_pipe(&err_io));

        let err_other = TranscodeError::Other("random error".to_string());
        assert!(!is_broken_pipe(&err_other));
    }

    fn generate_test_wav_f32(sample_rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
        let data_bytes = (samples.len() * 4) as u32;
        let block_align = channels * 4;
        let byte_rate = sample_rate * u32::from(block_align);
        let mut buf = Vec::with_capacity(44 + data_bytes as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        buf.extend_from_slice(b"WAVEfmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(&channels.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&byte_rate.to_le_bytes());
        buf.extend_from_slice(&block_align.to_le_bytes());
        buf.extend_from_slice(&32u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        buf
    }

    fn generate_test_wav_i16_mono(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_bytes = (samples.len() * 2) as u32;
        let mut buf = Vec::with_capacity(44 + data_bytes as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        buf.extend_from_slice(b"WAVEfmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_bytes.to_le_bytes());
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        buf
    }

    fn make_job(
        output_format: OutputContainer,
        bits_per_sample: Option<u16>,
        target_channels: Option<u16>,
    ) -> TranscodeJob {
        TranscodeJob {
            input_path: None,
            output_path: None,
            output_format,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels,
            bits_per_sample,
            raw_input: None,
        }
    }

    #[test]
    fn test_8bit_wav_unsigned_vs_signed_raw_and_flac() {
        let samples = [-1.0_f32, -0.5, 0.0, 0.5, 1.0];
        let wav_in = generate_test_wav_f32(44100, 1, &samples);

        // WAV 8-bit PCM must be unsigned [0..=255] centered at 128.
        let mut wav_out = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::Wav, Some(8), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut wav_out,
        )
        .unwrap();
        assert_eq!(&wav_out[44..], &[1u8, 64, 128, 192, 255]);

        // Raw 8-bit PCM (LE and BE) must be signed two's complement.
        let expected_signed = [(-127i8) as u8, (-64i8) as u8, 0u8, 64u8, 127u8];
        let mut raw_le = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(8), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut raw_le,
        )
        .unwrap();
        assert_eq!(raw_le, expected_signed);

        let mut raw_be = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmBe, Some(8), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut raw_be,
        )
        .unwrap();
        assert_eq!(raw_be, expected_signed);

        // Direct boundary check for -128..=127 in ContainerWriter.
        let planar = vec![vec![-128, 0, 127]];
        let mut wav_boundary = Vec::new();
        let mut idx = 0u64;
        let mut pending = vec![Vec::new()];
        let mut writer = ContainerWriter {
            out: &mut wav_boundary,
            format: OutputContainer::Wav,
            channels: 1,
            bps: 8,
            flac_frame_idx: &mut idx,
            flac_pending: &mut pending,
        };
        writer.write_samples(&planar, 3).unwrap();
        assert_eq!(wav_boundary, vec![0u8, 128u8, 255u8]);

        // 8-bit FLAC encoding and Symphonia decode roundtrip.
        // Symphonia normalizes i8 by /128.0 on decode, shifting +/-127 by 1 LSB.
        let mut flac_out = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::Flac, Some(8), Some(1)),
            std::io::Cursor::new(wav_in),
            &mut flac_out,
        )
        .unwrap();
        assert!(flac_out.starts_with(b"fLaC"));
        let mut flac_roundtrip = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(8), Some(1)),
            std::io::Cursor::new(flac_out),
            &mut flac_roundtrip,
        )
        .unwrap();
        assert_eq!(
            flac_roundtrip,
            [(-126i8) as u8, (-64i8) as u8, 0u8, 64u8, 126u8]
        );
    }

    #[test]
    fn test_16_24_32bit_pcm_flac_and_float_nan_sanitization() {
        let raw_samples = vec![
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            2.5,
            -2.5,
            1.0,
            0.0,
            -1.0,
        ];

        // Direct sanitization check: NaN, +inf, -inf -> 0; out-of-range clamped.
        let mut int_out = vec![Vec::new()];
        convert_f32_to_int_into(std::slice::from_ref(&raw_samples), 8, 16, &mut int_out);
        assert_eq!(int_out[0], vec![0, 0, 0, 32767, -32767, 32767, 0, -32767]);
        convert_f32_to_int_into(std::slice::from_ref(&raw_samples), 8, 24, &mut int_out);
        assert_eq!(
            int_out[0],
            vec![0, 0, 0, 8_388_607, -8_388_607, 8_388_607, 0, -8_388_607,]
        );
        convert_f32_to_int_into(std::slice::from_ref(&raw_samples), 8, 32, &mut int_out);
        assert_eq!(
            int_out[0],
            vec![
                0,
                0,
                0,
                2_147_483_647,
                -2_147_483_647,
                2_147_483_647,
                0,
                -2_147_483_647,
            ]
        );

        let wav_samples = [f32::NAN, 2.5, -2.5, 1.0, 0.0, -1.0];
        let wav_in = generate_test_wav_f32(44100, 1, &wav_samples);

        // 16-bit PCM LE (direct_i16 path): NaN -> 0, -1.0 -> -32768, +1.0 -> 32767.
        let mut pcm16 = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(16), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut pcm16,
        )
        .unwrap();
        let expected16: [i16; 6] = [0, 32767, -32768, 32767, 0, -32768];
        assert_eq!(pcm16.len(), expected16.len() * 2);
        for (idx, &exp) in expected16.iter().enumerate() {
            let got = i16::from_le_bytes([pcm16[idx * 2], pcm16[idx * 2 + 1]]);
            assert_eq!(got, exp, "16-bit mismatch at sample {idx}");
        }

        // 24-bit PCM LE and FLAC roundtrip.
        let mut pcm24 = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(24), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut pcm24,
        )
        .unwrap();
        let expected24: [i32; 6] = [0, 8_388_607, -8_388_607, 8_388_607, 0, -8_388_607];
        assert_eq!(pcm24.len(), expected24.len() * 3);
        for (idx, &exp) in expected24.iter().enumerate() {
            let b = &pcm24[idx * 3..idx * 3 + 3];
            let sign = if (b[2] & 0x80) != 0 { 0xFF } else { 0x00 };
            let got = i32::from_le_bytes([b[0], b[1], b[2], sign]);
            assert_eq!(got, exp, "24-bit mismatch at sample {idx}");
        }

        let mut flac24 = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::Flac, Some(24), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut flac24,
        )
        .unwrap();
        assert!(flac24.starts_with(b"fLaC"));
        let mut flac24_pcm = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(24), Some(1)),
            std::io::Cursor::new(flac24),
            &mut flac24_pcm,
        )
        .unwrap();
        assert_eq!(flac24_pcm.len(), pcm24.len());

        // 32-bit PCM LE and 32-bit FLAC header/frame generation.
        let mut pcm32 = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(32), Some(1)),
            std::io::Cursor::new(wav_in.clone()),
            &mut pcm32,
        )
        .unwrap();
        let expected32: [i32; 6] = [
            0,
            2_147_483_647,
            -2_147_483_647,
            2_147_483_647,
            0,
            -2_147_483_647,
        ];
        assert_eq!(pcm32.len(), expected32.len() * 4);
        for (idx, &exp) in expected32.iter().enumerate() {
            let got = i32::from_le_bytes([
                pcm32[idx * 4],
                pcm32[idx * 4 + 1],
                pcm32[idx * 4 + 2],
                pcm32[idx * 4 + 3],
            ]);
            assert_eq!(got, exp, "32-bit mismatch at sample {idx}");
        }

        let mut flac32 = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::Flac, Some(32), Some(1)),
            std::io::Cursor::new(wav_in),
            &mut flac32,
        )
        .unwrap();
        assert!(flac32.starts_with(b"fLaC"));
        assert!(flac32.len() > 42);
    }

    #[test]
    fn test_mono_direct_i16_fast_path() {
        let samples = [-32768_i16, -1234, 0, 1234, 32767];
        let wav_in = generate_test_wav_i16_mono(44100, &samples);

        let mut le_out = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(16), None),
            std::io::Cursor::new(wav_in.clone()),
            &mut le_out,
        )
        .unwrap();
        assert_eq!(le_out, &wav_in[44..]);

        let mut be_out = Vec::new();
        execute_transcode(
            &make_job(OutputContainer::RawPcmBe, Some(16), Some(1)),
            std::io::Cursor::new(wav_in),
            &mut be_out,
        )
        .unwrap();
        assert_eq!(be_out.len(), samples.len() * 2);
        for (idx, &exp) in samples.iter().enumerate() {
            let got = i16::from_be_bytes([be_out[idx * 2], be_out[idx * 2 + 1]]);
            assert_eq!(got, exp, "mono BE mismatch at sample {idx}");
        }
    }

    #[test]
    fn test_parse_sox_bare_dash_with_effects() {
        let job_pipe = parse_cli_invocation(
            "sox",
            &[
                "-t", "wav", "-", "-t", "flac", "-", "rate", "48000", "trim", "0:05", "=0:25",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job_pipe.input_path, None);
        assert_eq!(job_pipe.output_format, OutputContainer::Flac);
        assert_eq!(job_pipe.target_sample_rate, Some(48000));
        assert_eq!(job_pipe.start_sec, Some(5.0));
        assert_eq!(job_pipe.end_sec, Some(25.0));

        let job_rel = parse_cli_invocation(
            "sox",
            &["-", "-t", "raw", "-", "trim", "1.5", "2.5"].map(String::from),
        )
        .unwrap();
        assert_eq!(job_rel.input_path, None);
        assert_eq!(job_rel.output_format, OutputContainer::RawPcmLe);
        assert_eq!(job_rel.start_sec, Some(1.5));
        assert_eq!(job_rel.end_sec, Some(4.0));

        let job_file_out_dash = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-", "rate", "44100"].map(String::from),
        )
        .unwrap();
        assert_eq!(
            job_file_out_dash.input_path,
            Some(PathBuf::from("/tmp/in.wav"))
        );
        assert_eq!(job_file_out_dash.output_format, OutputContainer::Wav);
        assert_eq!(job_file_out_dash.target_sample_rate, Some(44100));
    }

    #[test]
    fn test_parse_time_offset_rejects_invalid_values() {
        let invalid = [
            "NaN", "nan", "inf", "-inf", "infinity", "-1.5", "-0.001", "1:-05", "-1:05", "", "   ",
            "1:2:3:4", "abc",
        ];
        for raw in invalid {
            assert!(
                parse_time_offset(raw).is_err(),
                "expected error for invalid time offset '{raw}'"
            );
        }
    }

    #[test]
    fn test_unbounded_streaming_readonly_source() {
        let wav_bytes = generate_test_wav_44100_stereo(0.5);
        let expected_pcm = &wav_bytes[44..];

        // Non-seekable ReadOnlySource with start_sec = None.
        let ro_full =
            symphonia::core::io::ReadOnlySource::new(std::io::Cursor::new(wav_bytes.clone()));
        let job_full = make_job(OutputContainer::RawPcmLe, Some(16), Some(2));
        let mut out_full = Vec::new();
        execute_transcode_media_source(&job_full, Box::new(ro_full), &mut out_full).unwrap();
        assert_eq!(out_full, expected_pcm);

        // Non-seekable ReadOnlySource with start_sec = Some(0.25) forward skip.
        let ro_skip =
            symphonia::core::io::ReadOnlySource::new(std::io::Cursor::new(wav_bytes.clone()));
        let mut job_skip = make_job(OutputContainer::RawPcmLe, Some(16), Some(2));
        job_skip.start_sec = Some(0.25);
        let mut out_skip = Vec::new();
        execute_transcode_media_source(&job_skip, Box::new(ro_skip), &mut out_skip).unwrap();
        // 0.25s..0.50s = 11025 stereo 16-bit frames = 44100 bytes.
        assert_eq!(out_skip.len(), 44100);
        assert_eq!(out_skip, &expected_pcm[44100..]);
    }

    #[test]
    fn test_broken_pipe_and_cli_help_and_unknown_flags() {
        let symphonia_pipe = TranscodeError::Symphonia(symphonia::core::errors::Error::IoError(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed"),
        ));
        assert!(is_broken_pipe(&symphonia_pipe));

        let eof_err = TranscodeError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "eof",
        ));
        assert!(!is_broken_pipe(&eof_err));

        assert!(matches!(
            parse_cli_invocation("lms-transcode", &["-h".to_string()]),
            Err(TranscodeError::Help)
        ));
        assert!(matches!(
            parse_cli_invocation("sox", &["--help".to_string()]),
            Err(TranscodeError::Help)
        ));

        // After `--`, `-h` is a positional file path rather than help.
        let after_dd = parse_cli_invocation("flac", &["--".into(), "-h".into()]).unwrap();
        assert_eq!(after_dd.input_path, Some(PathBuf::from("-h")));

        let unknown_cases = [
            ("faad", vec!["--bogus"]),
            ("flac", vec!["--unknown-flag"]),
            ("wvunpack", vec!["--bad"]),
            ("sox", vec!["--bad"]),
            ("lms-transcode", vec!["--mode", "invalid-mode"]),
        ];
        for (bin, args) in unknown_cases {
            let owned: Vec<String> = args.into_iter().map(String::from).collect();
            assert!(
                matches!(
                    parse_cli_invocation(bin, &owned),
                    Err(TranscodeError::Parse(_))
                ),
                "expected Parse error for {bin} {owned:?}"
            );
        }
    }

    #[test]
    fn test_sec_053_054_audio_bounds_and_zero_channel_sample_buf() {
        assert!(validate_audio_params(44100, 2, 16).is_ok());
        assert!(validate_audio_params(8000, 1, 8).is_ok());
        assert!(validate_audio_params(384_000, 8, 32).is_ok());
        assert!(validate_audio_params(7999, 2, 16).is_err());
        assert!(validate_audio_params(384_001, 2, 16).is_err());
        assert!(validate_audio_params(44100, 0, 16).is_err());
        assert!(validate_audio_params(44100, 9, 16).is_err());
        assert!(validate_audio_params(44100, 2, 12).is_err());

        // Zero-channel AudioBuffer must return Err rather than allocating/panicking.
        let zero_buf = symphonia::core::audio::AudioBuffer::<f32>::unused();
        let mut holder: Option<ManagedSampleBuffer<f32>> = None;
        let res = copy_decoded_interleaved(
            &mut holder,
            symphonia::core::audio::AudioBufferRef::F32(std::borrow::Cow::Owned(zero_buf)),
        );
        assert!(res.is_err());
    }

    #[test]
    fn test_logic_011_015_mode_detection_precedence_and_explicit_options() {
        // Substring inside path must not falsely trigger flac mode over sox.
        let path_substr = parse_cli_invocation(
            "lms-transcode",
            &["/music/force-raw-format/in.ogg", "-t", "wav", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(path_substr.output_format, OutputContainer::Wav);
        assert_eq!(
            path_substr.input_path,
            Some(PathBuf::from("/music/force-raw-format/in.ogg"))
        );

        // wvunpack with --skip/--until must detect wvunpack rather than flac.
        let wv_skip = parse_cli_invocation(
            "lms-transcode",
            &["/tmp/in.wv", "-rq", "--skip=10", "--until=20", "-o", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(wv_skip.output_format, OutputContainer::RawPcmLe);
        assert_eq!(wv_skip.start_sec, Some(10.0));
        assert_eq!(wv_skip.end_sec, Some(20.0));

        // sox -e signed-integer must not trigger faad mode.
        let sox_e = parse_cli_invocation(
            "lms-transcode",
            &[
                "-q",
                "-t",
                "raw",
                "-e",
                "signed-integer",
                "-b",
                "16",
                "-r",
                "44100",
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
        assert_eq!(sox_e.output_format, OutputContainer::Wav);

        // flac separate -c -d with --force-raw-format decodes to RawPcmLe.
        let flac_sep_cd = parse_cli_invocation(
            "flac",
            &[
                "-c",
                "-d",
                "--force-raw-format",
                "--endian=little",
                "/tmp/in.flac",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(flac_sep_cd.output_format, OutputContainer::RawPcmLe);
        assert_eq!(flac_sep_cd.raw_input, None);

        // flac encode with --force-raw-format encodes to Flac and populates raw_input
        // without polluting target_sample_rate/target_channels/bits_per_sample.
        let flac_enc_raw = parse_cli_invocation(
            "flac",
            &[
                "-c",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
                "--bps=16",
                "--sample-rate=44100",
                "--channels=2",
                "-",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(flac_enc_raw.output_format, OutputContainer::Flac);
        assert_eq!(flac_enc_raw.target_sample_rate, None);
        assert_eq!(flac_enc_raw.target_channels, None);
        assert_eq!(flac_enc_raw.bits_per_sample, None);
        assert_eq!(
            flac_enc_raw.raw_input,
            Some(RawPcmInputFormat {
                sample_rate: 44100,
                channels: 2,
                bits_per_sample: 16,
                endian_big: false,
            })
        );

        let flac_enc_raw_be = parse_cli_invocation(
            "flac",
            &[
                "-cs",
                "--totally-silent",
                "--compression-level-0",
                "--force-raw-format",
                "--endian=big",
                "--sign=signed",
                "--bps=24",
                "--sample-rate=96000",
                "--channels=2",
                "-",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(flac_enc_raw_be.output_format, OutputContainer::Flac);
        assert_eq!(flac_enc_raw_be.target_sample_rate, None);
        assert_eq!(flac_enc_raw_be.target_channels, None);
        assert_eq!(flac_enc_raw_be.bits_per_sample, None);
        assert_eq!(
            flac_enc_raw_be.raw_input,
            Some(RawPcmInputFormat {
                sample_rate: 96000,
                channels: 2,
                bits_per_sample: 24,
                endian_big: true,
            })
        );

        // flac --force-raw-format encode missing required input parameters must fail.
        assert!(parse_cli_invocation(
            "flac",
            &["-c", "--force-raw-format", "--endian=big", "-"].map(String::from)
        )
        .is_err());

        // --dsf, --dff, and --sign=unsigned are rejected explicitly.
        assert!(
            parse_cli_invocation("wvunpack", &["--dsf", "/tmp/in.wv"].map(String::from)).is_err()
        );
        assert!(
            parse_cli_invocation("wvunpack", &["--dff", "/tmp/in.wv"].map(String::from)).is_err()
        );
        assert!(parse_cli_invocation(
            "flac",
            &[
                "-dcs",
                "--force-raw-format",
                "--sign=unsigned",
                "/tmp/in.flac"
            ]
            .map(String::from)
        )
        .is_err());

        // Out-of-bounds rate/channels/bps are rejected at CLI parse time.
        assert!(parse_cli_invocation(
            "flac",
            &["--sample-rate=999999", "/tmp/in.flac"].map(String::from)
        )
        .is_err());
        assert!(
            parse_cli_invocation("flac", &["--channels=12", "/tmp/in.flac"].map(String::from))
                .is_err()
        );
        assert!(
            parse_cli_invocation("flac", &["--bps=12", "/tmp/in.flac"].map(String::from)).is_err()
        );
        assert!(
            parse_cli_invocation("sox", &["/tmp/in.wav", "-r", "100", "-"].map(String::from))
                .is_err()
        );

        // Explicit file -o outputs populate output_path.
        let flac_out_file = parse_cli_invocation(
            "flac",
            &["-d", "-o", "/tmp/out.wav", "/tmp/in.flac"].map(String::from),
        )
        .unwrap();
        assert_eq!(
            flac_out_file.output_path,
            Some(PathBuf::from("/tmp/out.wav"))
        );
        let wv_out_file = parse_cli_invocation(
            "wvunpack",
            &["/tmp/in.wv", "-w", "-o/tmp/out.wav"].map(String::from),
        )
        .unwrap();
        assert_eq!(wv_out_file.output_path, Some(PathBuf::from("/tmp/out.wav")));
        let sox_out_file =
            parse_cli_invocation("sox", &["/tmp/in.wav", "/tmp/out.flac"].map(String::from))
                .unwrap();
        assert_eq!(
            sox_out_file.output_path,
            Some(PathBuf::from("/tmp/out.flac"))
        );
    }

    #[test]
    fn test_raw_pcm_to_flac_transcode_and_roundtrip() {
        let job = parse_cli_invocation(
            "flac",
            &[
                "-cs",
                "--totally-silent",
                "--compression-level-0",
                "--force-raw-format",
                "--endian=big",
                "--sign=signed",
                "--bps=16",
                "--sample-rate=44100",
                "--channels=2",
                "-",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job.output_format, OutputContainer::Flac);
        assert_eq!(job.target_sample_rate, None);
        assert_eq!(job.target_channels, None);
        assert_eq!(job.bits_per_sample, None);
        assert_eq!(
            job.raw_input,
            Some(RawPcmInputFormat {
                sample_rate: 44100,
                channels: 2,
                bits_per_sample: 16,
                endian_big: true,
            })
        );

        let frames = 4410usize;
        let mut raw_be = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let l = (((i as i32 * 137) % 30_000) - 15_000) as i16;
            let r = -l;
            raw_be.extend_from_slice(&l.to_be_bytes());
            raw_be.extend_from_slice(&r.to_be_bytes());
        }

        // Stream through unseekable ReadOnlySource (matching stdin pipe execution).
        let mut flac_out = Vec::new();
        let ro = symphonia::core::io::ReadOnlySource::new(std::io::Cursor::new(raw_be.clone()));
        execute_transcode_media_source(&job, Box::new(ro), &mut flac_out).unwrap();
        assert!(flac_out.starts_with(b"fLaC"));

        let decode_be = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::RawPcmBe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
            raw_input: None,
        };
        let mut decoded_be = Vec::new();
        execute_transcode(&decode_be, std::io::Cursor::new(flac_out), &mut decoded_be).unwrap();
        assert_eq!(decoded_be, raw_be);

        // 24-bit little-endian raw PCM preserves 24-bit precision in FLAC.
        let job_24 = parse_cli_invocation(
            "flac",
            &[
                "-c",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
                "--bps=24",
                "--sample-rate=48000",
                "--channels=2",
                "-",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(job_24.target_sample_rate, None);
        assert_eq!(job_24.target_channels, None);
        assert_eq!(job_24.bits_per_sample, None);

        let samples_24 = [1_234_567i32, -2_345_678, 8_388_607, -8_388_608];
        let mut raw_24_le = Vec::with_capacity(samples_24.len() * 3);
        for &s in &samples_24 {
            let [b0, b1, b2, _] = s.to_le_bytes();
            raw_24_le.extend_from_slice(&[b0, b1, b2]);
        }
        let mut flac_24 = Vec::new();
        execute_transcode(
            &job_24,
            std::io::Cursor::new(raw_24_le.clone()),
            &mut flac_24,
        )
        .unwrap();
        assert!(flac_24.starts_with(b"fLaC"));
        let packed_meta = u64::from_be_bytes([
            flac_24[18],
            flac_24[19],
            flac_24[20],
            flac_24[21],
            flac_24[22],
            flac_24[23],
            flac_24[24],
            flac_24[25],
        ]);
        assert_eq!((packed_meta >> 44) & 0x0F_FFFF, 48_000);
        assert_eq!(((packed_meta >> 41) & 0x07) + 1, 2);
        assert_eq!(((packed_meta >> 36) & 0x1F) + 1, 24);

        let decode_24 = TranscodeJob {
            input_path: None,
            output_path: None,
            output_format: OutputContainer::RawPcmLe,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(24),
            raw_input: None,
        };
        let mut decoded_24_le = Vec::new();
        execute_transcode(
            &decode_24,
            std::io::Cursor::new(flac_24),
            &mut decoded_24_le,
        )
        .unwrap();
        assert_eq!(decoded_24_le.len(), raw_24_le.len());
        for (idx, (&expected, chunk)) in samples_24
            .iter()
            .zip(decoded_24_le.chunks_exact(3))
            .enumerate()
        {
            let got = i32::from_le_bytes([0, chunk[0], chunk[1], chunk[2]]) >> 8;
            assert!(
                (got - expected).abs() <= 1,
                "24-bit sample {idx} mismatch: expected {expected}, got {got}"
            );
        }

        // Raw PCM with explicit target_sample_rate exercises resampler -> FLAC.
        let mut resampled_job = job;
        resampled_job.target_sample_rate = Some(48000);
        let mut flac_resampled = Vec::new();
        execute_transcode(
            &resampled_job,
            std::io::Cursor::new(raw_be),
            &mut flac_resampled,
        )
        .unwrap();
        assert!(flac_resampled.starts_with(b"fLaC"));
        let mut decoded_resampled = Vec::new();
        execute_transcode(
            &decode_be,
            std::io::Cursor::new(flac_resampled),
            &mut decoded_resampled,
        )
        .unwrap();
        let out_frames = decoded_resampled.len() / 4;
        assert!((out_frames as i64 - 4800).abs() < 50);
    }

    #[test]
    fn test_logic_016_unknown_length_hints_and_checked_scaling() {
        assert_eq!(
            compute_total_out_frames_hint(None, 0, 44100, true, 44100, 48000),
            None
        );
        assert_eq!(
            compute_total_out_frames_hint(Some(44100), 0, 88200, true, 44100, 48000),
            Some(48000)
        );
        assert_eq!(
            compute_total_out_frames_hint(Some(u64::MAX), 0, u64::MAX, false, 44100, 48000),
            None
        );

        let mut flac_hdr = Vec::new();
        write_flac_header(&mut flac_hdr, 44100, 2, 16, MAX_FLAC_CODED_NUMBER + 1).unwrap();
        // Total samples field (lower 36 bits of bytes 22..30) must be 0 (unknown).
        let packed = u64::from_be_bytes(flac_hdr[22..30].try_into().unwrap());
        assert_eq!(packed & MAX_FLAC_CODED_NUMBER, 0);
    }

    #[test]
    fn test_sec_082_093_flag_operands_and_broken_pipe_usage() {
        let flac_mode_name = parse_cli_invocation(
            "flac",
            &["-d", "-o", "--mode", "/tmp/in.flac"].map(String::from),
        )
        .unwrap();
        assert_eq!(flac_mode_name.output_path, Some(PathBuf::from("--mode")));
        assert_eq!(
            flac_mode_name.input_path,
            Some(PathBuf::from("/tmp/in.flac"))
        );

        let faad_help_name =
            parse_cli_invocation("faad", &["-o", "-h", "/tmp/in.m4a"].map(String::from)).unwrap();
        assert_eq!(faad_help_name.output_path, Some(PathBuf::from("-h")));
        assert_eq!(
            faad_help_name.input_path,
            Some(PathBuf::from("/tmp/in.m4a"))
        );

        let wv_help_name = parse_cli_invocation(
            "lms-transcode",
            &["--mode", "wvunpack", "-o", "--help", "/tmp/in.wv"].map(String::from),
        )
        .unwrap();
        assert_eq!(wv_help_name.output_path, Some(PathBuf::from("--help")));

        let flac_long_out = parse_cli_invocation(
            "lms-transcode",
            &[
                "--mode",
                "flac",
                "-d",
                "--output-name",
                "--mode",
                "/tmp/in.flac",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(flac_long_out.output_path, Some(PathBuf::from("--mode")));

        struct BrokenPipeWriter;
        impl Write for BrokenPipeWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "broken pipe",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = write_usage(&mut BrokenPipeWriter).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn test_logic_066_067_068_070_ranges_sox_trim_encodings_and_swap() {
        // end_sec < start_sec rejected in faad, flac, wvunpack.
        assert!(parse_cli_invocation(
            "faad",
            &["-j", "20", "-e", "10", "/tmp/in.m4a"].map(String::from)
        )
        .is_err());
        assert!(parse_cli_invocation(
            "flac",
            &["--skip=20", "--until=10", "/tmp/in.flac"].map(String::from)
        )
        .is_err());
        assert!(parse_cli_invocation(
            "wvunpack",
            &["-s", "20", "-u", "10", "/tmp/in.wv"].map(String::from)
        )
        .is_err());

        // Equal start and end offsets remain valid.
        let equal_faad = parse_cli_invocation(
            "faad",
            &["-j", "10", "-e", "10", "/tmp/in.m4a"].map(String::from),
        )
        .unwrap();
        assert_eq!(equal_faad.start_sec, Some(10.0));
        assert_eq!(equal_faad.end_sec, Some(10.0));

        // parse_sox_trim generalizes past the old 5-effect list.
        let err = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "/tmp/out.wav", "trim", "5", "fade", "1"].map(String::from),
        )
        .unwrap_err();
        assert!(
            matches!(
                &err,
                TranscodeError::Parse(msg) if msg.contains("unsupported sox effect 'fade'")
            ),
            "unexpected error for trim followed by fade: {err:?}"
        );

        // Trailing -x / $-x$ endian-swap resolution before freezing RawPcm endianness.
        let swap_after = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "-x", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(swap_after.output_format, OutputContainer::RawPcmBe);

        let double_swap = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "-x", "$-x$", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(double_swap.output_format, OutputContainer::RawPcmLe);

        let big_then_swap = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "-B", "-x", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(big_then_swap.output_format, OutputContainer::RawPcmLe);

        let little_then_swap = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "-L", "-x", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(little_then_swap.output_format, OutputContainer::RawPcmBe);

        // Unsupported output encodings rejected, while signed-integer is accepted.
        for bad_flag in ["-u", "-f", "-A", "-U"] {
            assert!(
                parse_cli_invocation(
                    "sox",
                    &["/tmp/in.wav", "-t", "raw", bad_flag, "-"].map(String::from)
                )
                .is_err(),
                "expected error for output encoding {bad_flag}"
            );
        }
        assert!(parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "-e", "unsigned-integer", "-"].map(String::from)
        )
        .is_err());
        assert!(parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "--encoding=floating-point", "-"].map(String::from)
        )
        .is_err());

        let signed_out = parse_cli_invocation(
            "sox",
            &["/tmp/in.wav", "-t", "raw", "-e", "signed-integer", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(signed_out.output_format, OutputContainer::RawPcmLe);
    }

    #[test]
    fn test_logic_072_source_wrapper_fifo_seekability_and_header_flushes() {
        let wav_bytes = generate_test_wav_44100_stereo(0.1);
        let regular = SourceWrapper::new(std::io::Cursor::new(wav_bytes.clone()));
        assert!(regular.is_seekable());
        assert_eq!(regular.byte_len(), Some(wav_bytes.len() as u64));

        // Simulate a FIFO / pipe whose Read+Seek implementation returns ESPIPE on seek.
        struct FifoReader(std::io::Cursor<Vec<u8>>);
        impl Read for FifoReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Seek for FifoReader {
            fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
                Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
            }
        }

        let fifo = SourceWrapper::new(FifoReader(std::io::Cursor::new(wav_bytes.clone())));
        assert!(!fifo.is_seekable());
        assert_eq!(fifo.byte_len(), None);

        // Verify header flush before decode loop and final flush at shutdown.
        struct FlushTrackingWriter {
            buf: Vec<u8>,
            flush_sizes: Vec<usize>,
        }
        impl Write for FlushTrackingWriter {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.flush_sizes.push(self.buf.len());
                Ok(())
            }
        }

        let mut tracking_out = FlushTrackingWriter {
            buf: Vec::new(),
            flush_sizes: Vec::new(),
        };
        let job = make_job(OutputContainer::Wav, Some(16), Some(2));
        execute_transcode(&job, std::io::Cursor::new(wav_bytes), &mut tracking_out).unwrap();
        assert!(
            tracking_out.flush_sizes.first().copied() == Some(44),
            "expected 44-byte WAV header flush before decode loop, got {:?}",
            tracking_out.flush_sizes
        );
        assert_eq!(
            tracking_out.flush_sizes.last().copied(),
            Some(tracking_out.buf.len())
        );
    }

    #[test]
    fn test_logic_069_112_sample_clamping_flac_frame_limit_and_wav_boundary() {
        // 16-bit and 24-bit clamping in ContainerWriter and write_flac_frame.
        let extreme_samples = vec![vec![
            i32::MIN,
            -8_388_609,
            -8_388_608,
            -32_769,
            -32_768,
            0,
            32_767,
            32_768,
            8_388_607,
            8_388_608,
            i32::MAX,
        ]];
        let expected_i16 = [
            -32_768i16, -32_768, -32_768, -32_768, -32_768, 0, 32_767, 32_767, 32_767, 32_767,
            32_767,
        ];
        let expected_i24 = [
            -8_388_608i32,
            -8_388_608,
            -8_388_608,
            -32_769,
            -32_768,
            0,
            32_767,
            32_768,
            8_388_607,
            8_388_607,
            8_388_607,
        ];

        for fmt in [
            OutputContainer::RawPcmLe,
            OutputContainer::Wav,
            OutputContainer::RawPcmBe,
        ] {
            let mut out16 = Vec::new();
            let mut idx = 0u64;
            let mut pending = vec![Vec::new()];
            let mut w16 = ContainerWriter {
                out: &mut out16,
                format: fmt,
                channels: 1,
                bps: 16,
                flac_frame_idx: &mut idx,
                flac_pending: &mut pending,
            };
            w16.write_samples(&extreme_samples, extreme_samples[0].len())
                .unwrap();
            assert_eq!(out16.len(), expected_i16.len() * 2);
            for (i, &exp) in expected_i16.iter().enumerate() {
                let got = if fmt == OutputContainer::RawPcmBe {
                    i16::from_be_bytes([out16[i * 2], out16[i * 2 + 1]])
                } else {
                    i16::from_le_bytes([out16[i * 2], out16[i * 2 + 1]])
                };
                assert_eq!(got, exp, "16-bit clamp mismatch at {i} for {fmt:?}");
            }

            let mut out24 = Vec::new();
            let mut w24 = ContainerWriter {
                out: &mut out24,
                format: fmt,
                channels: 1,
                bps: 24,
                flac_frame_idx: &mut idx,
                flac_pending: &mut pending,
            };
            w24.write_samples(&extreme_samples, extreme_samples[0].len())
                .unwrap();
            assert_eq!(out24.len(), expected_i24.len() * 3);
            for (i, &exp) in expected_i24.iter().enumerate() {
                let b = &out24[i * 3..i * 3 + 3];
                let got = if fmt == OutputContainer::RawPcmBe {
                    let sign = if (b[0] & 0x80) != 0 { 0xFF } else { 0x00 };
                    i32::from_be_bytes([sign, b[0], b[1], b[2]])
                } else {
                    let sign = if (b[2] & 0x80) != 0 { 0xFF } else { 0x00 };
                    i32::from_le_bytes([b[0], b[1], b[2], sign])
                };
                assert_eq!(got, exp, "24-bit clamp mismatch at {i} for {fmt:?}");
            }
        }

        // FLAC frame clamping for 16-bit and 24-bit via full FLAC decode roundtrip.
        for (bps, expected) in [
            (
                16u16,
                expected_i16
                    .iter()
                    .map(|&v| i32::from(v))
                    .collect::<Vec<_>>(),
            ),
            (24u16, expected_i24.to_vec()),
        ] {
            let mut flac_bytes = Vec::new();
            write_flac_header(&mut flac_bytes, 44100, 1, bps, expected.len() as u64).unwrap();
            write_flac_frame(&mut flac_bytes, 0, expected.len(), 1, bps, &extreme_samples).unwrap();
            if bps == 24 {
                // Verify exact big-endian 24-bit clamped payload in FLAC frame.
                let payload = &flac_bytes[42 + 9..42 + 9 + expected.len() * 3];
                for (i, &exp) in expected.iter().enumerate() {
                    let b = &payload[i * 3..i * 3 + 3];
                    let sign = if (b[0] & 0x80) != 0 { 0xFF } else { 0x00 };
                    let raw = i32::from_be_bytes([sign, b[0], b[1], b[2]]);
                    assert_eq!(raw, exp, "FLAC 24-bit raw frame clamp mismatch at {i}");
                }
            }
            let mut pcm_back = Vec::new();
            execute_transcode(
                &make_job(OutputContainer::RawPcmLe, Some(bps), Some(1)),
                std::io::Cursor::new(flac_bytes),
                &mut pcm_back,
            )
            .unwrap();
            let stride = usize::from(bps / 8);
            assert_eq!(pcm_back.len(), expected.len() * stride);
            for (i, &exp) in expected.iter().enumerate() {
                let got = if bps == 16 {
                    i32::from(i16::from_le_bytes([pcm_back[i * 2], pcm_back[i * 2 + 1]]))
                } else {
                    let b = &pcm_back[i * 3..i * 3 + 3];
                    let sign = if (b[2] & 0x80) != 0 { 0xFF } else { 0x00 };
                    i32::from_le_bytes([b[0], b[1], b[2], sign])
                };
                if bps == 16 {
                    assert_eq!(got, exp, "FLAC 16-bit clamp mismatch at {i}");
                } else {
                    assert!(
                        (got - exp).abs() <= 1,
                        "FLAC 24-bit roundtrip mismatch at {i}: got {got}, exp {exp}"
                    );
                }
            }
        }

        // FLAC 31-bit frame number limit: 0x7FFF_FFFF ok, 0x8000_0000 errors.
        let one_sample = vec![vec![0i32]];
        let mut frame_ok = Vec::new();
        write_flac_frame(&mut frame_ok, MAX_FLAC_FRAME_NUMBER, 1, 1, 16, &one_sample).unwrap();
        assert_eq!(frame_ok[4], 0xFD); // 6-byte UTF-8 prefix for 0x7FFF_FFFF
        let mut frame_err = Vec::new();
        assert!(write_flac_frame(
            &mut frame_err,
            MAX_FLAC_FRAME_NUMBER + 1,
            1,
            1,
            16,
            &one_sample
        )
        .is_err());

        // WAV u32::MAX - 36 header size boundary (8-bit mono: 1 frame = 1 byte).
        let max_finite_bytes = u64::from(u32::MAX) - 36;
        let mut wav_exact = Vec::new();
        write_wav_header(&mut wav_exact, 44100, 1, 8, Some(max_finite_bytes)).unwrap();
        assert_eq!(
            u32::from_le_bytes(wav_exact[4..8].try_into().unwrap()),
            u32::MAX
        );
        assert_eq!(
            u32::from_le_bytes(wav_exact[40..44].try_into().unwrap()),
            u32::MAX - 36
        );

        let mut wav_over = Vec::new();
        write_wav_header(&mut wav_over, 44100, 1, 8, Some(max_finite_bytes + 1)).unwrap();
        assert_eq!(
            u32::from_le_bytes(wav_over[4..8].try_into().unwrap()),
            0xFFFF_FFFF
        );
        assert_eq!(
            u32::from_le_bytes(wav_over[40..44].try_into().unwrap()),
            0xFFFF_FFFF
        );
    }

    #[test]
    fn test_logic_071_non_finite_start_zero_timebase_and_seek_error() {
        let wav_bytes = generate_test_wav_44100_stereo(0.2);

        for bad_start in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
            let mut job = make_job(OutputContainer::RawPcmLe, Some(16), Some(2));
            job.start_sec = Some(bad_start);
            let mut out = Vec::new();
            assert!(
                execute_transcode(&job, std::io::Cursor::new(wav_bytes.clone()), &mut out).is_err(),
                "expected error for non-finite/negative start_sec {bad_start}"
            );
        }

        // Zero TimeBase must return Err rather than dividing by zero.
        let tb_zero_denom = symphonia::core::units::TimeBase { numer: 1, denom: 0 };
        let tb_zero_numer = symphonia::core::units::TimeBase {
            numer: 0,
            denom: 44100,
        };
        assert!(validate_time_base(Some(tb_zero_denom)).is_err());
        assert!(validate_time_base(Some(tb_zero_numer)).is_err());
        assert!(ts_to_frames(100, Some(tb_zero_denom), 44100).is_err());
        assert!(ts_to_frames(100, Some(tb_zero_numer), 44100).is_err());

        // Seek error (e.g., start_sec past duration -> SeekError(OutOfRange))
        // must propagate cleanly instead of being silently swallowed.
        let mut oob_job = make_job(OutputContainer::RawPcmLe, Some(16), Some(2));
        oob_job.start_sec = Some(5.0);
        let mut out_oob = Vec::new();
        assert!(
            execute_transcode(&oob_job, std::io::Cursor::new(wav_bytes), &mut out_oob).is_err()
        );
    }

    #[test]
    fn test_logic_073_resampler_delay_trim_and_eof_drain() {
        // Test rem == 0 (2048 frames) and rem == 1023 (pad < output_delay).
        for input_frames in [2048usize, 2047usize, 1024usize] {
            let mut samples = Vec::with_capacity(input_frames);
            for i in 0..input_frames {
                // Smooth low-frequency sine wave starting with non-zero phase so leading
                // zero-delay frames would be immediately distinguishable from signal.
                let phase = 0.4 + (i as f32) * 0.01;
                samples.push(0.5 * phase.sin());
            }
            let wav_in = generate_test_wav_f32(44100, 1, &samples);
            let mut job = make_job(OutputContainer::RawPcmLe, Some(16), Some(1));
            job.target_sample_rate = Some(48000);
            let mut out = Vec::new();
            execute_transcode(&job, std::io::Cursor::new(wav_in), &mut out).unwrap();

            let expected_frames =
                ((u128::from(input_frames as u64) * 48_000 + 22_050) / 44_100) as usize;
            let got_frames = out.len() / 2;
            assert_eq!(
                got_frames, expected_frames,
                "exact resampled frame count mismatch for input_frames={input_frames}"
            );

            // First sample must reflect trimmed filter delay (~6380), not
            // zero-padded warm-up frames.
            let first_sample = i16::from_le_bytes([out[0], out[1]]);
            assert!(
                first_sample > 2000,
                "filter delay not trimmed for input_frames={input_frames}: {first_sample}"
            );
        }
    }

    #[test]
    fn test_qa_010_007_writer_validation_broken_pipe_seek_accuracy_end_gt_dur_and_corrupt_eof() {
        // QA-010: Invalid bps/channels rejected before Vec allocation.
        let planar = vec![vec![0i32; 4]];
        let mut idx = 0u64;
        let mut pending = vec![Vec::new()];
        for bad_bps in [0u16, 12, 20, 64] {
            let mut out = Vec::new();
            let mut writer = ContainerWriter {
                out: &mut out,
                format: OutputContainer::RawPcmLe,
                channels: 1,
                bps: bad_bps,
                flac_frame_idx: &mut idx,
                flac_pending: &mut pending,
            };
            assert!(writer.write_samples(&planar, 4).is_err());
            assert!(out.is_empty());
        }
        let mut out_ch = Vec::new();
        let mut bad_ch_writer = ContainerWriter {
            out: &mut out_ch,
            format: OutputContainer::Wav,
            channels: 0,
            bps: 16,
            flac_frame_idx: &mut idx,
            flac_pending: &mut pending,
        };
        assert!(bad_ch_writer.write_samples(&planar, 4).is_err());

        // Broken-pipe BufWriter flushes across WAV, RawPcmLe, and FLAC.
        struct BrokenPipeWriter;
        impl Write for BrokenPipeWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "broken pipe",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "broken pipe flush",
                ))
            }
        }
        let wav_small = generate_test_wav_44100_stereo(0.1);
        for fmt in [
            OutputContainer::Wav,
            OutputContainer::RawPcmLe,
            OutputContainer::Flac,
        ] {
            let mut bw = std::io::BufWriter::with_capacity(64, BrokenPipeWriter);
            let err = execute_transcode(
                &make_job(fmt, Some(16), Some(2)),
                std::io::Cursor::new(wav_small.clone()),
                &mut bw,
            )
            .unwrap_err();
            assert!(
                is_broken_pipe(&err),
                "expected BrokenPipe for {fmt:?}, got {err:?}"
            );
        }

        // Sample-content seek accuracy on seekable Cursor and ReadOnlySource.
        let total_frames = 44100usize;
        let data_bytes = (total_frames * 4) as u32;
        let mut distinct_wav = Vec::with_capacity(44 + data_bytes as usize);
        distinct_wav.extend_from_slice(b"RIFF");
        distinct_wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        distinct_wav.extend_from_slice(b"WAVEfmt ");
        distinct_wav.extend_from_slice(&16u32.to_le_bytes());
        distinct_wav.extend_from_slice(&1u16.to_le_bytes());
        distinct_wav.extend_from_slice(&2u16.to_le_bytes());
        distinct_wav.extend_from_slice(&44100u32.to_le_bytes());
        distinct_wav.extend_from_slice(&(44100u32 * 4).to_le_bytes());
        distinct_wav.extend_from_slice(&4u16.to_le_bytes());
        distinct_wav.extend_from_slice(&16u16.to_le_bytes());
        distinct_wav.extend_from_slice(b"data");
        distinct_wav.extend_from_slice(&data_bytes.to_le_bytes());
        for f in 0..total_frames {
            let l = ((f * 7) % 30000) as i16;
            let r = -(((f * 13 + 5) % 30000) as i16);
            distinct_wav.extend_from_slice(&l.to_le_bytes());
            distinct_wav.extend_from_slice(&r.to_le_bytes());
        }

        let mut seek_job = make_job(OutputContainer::RawPcmLe, Some(16), Some(2));
        seek_job.start_sec = Some(0.25);
        seek_job.end_sec = Some(0.75);
        let expected_slice = &distinct_wav[44 + 11025 * 4..44 + 33075 * 4];

        let mut seekable_out = Vec::new();
        execute_transcode(
            &seek_job,
            std::io::Cursor::new(distinct_wav.clone()),
            &mut seekable_out,
        )
        .unwrap();
        assert_eq!(seekable_out, expected_slice);

        let mut nonseek_out = Vec::new();
        let ro =
            symphonia::core::io::ReadOnlySource::new(std::io::Cursor::new(distinct_wav.clone()));
        execute_transcode_media_source(&seek_job, Box::new(ro), &mut nonseek_out).unwrap();
        assert_eq!(nonseek_out, expected_slice);

        // end_sec > duration caps cleanly to available audio across RawPcmLe and Wav.
        let mut long_end_raw_job = make_job(OutputContainer::RawPcmLe, Some(16), Some(2));
        long_end_raw_job.start_sec = Some(0.25);
        long_end_raw_job.end_sec = Some(10.0);
        let mut long_end_raw = Vec::new();
        execute_transcode(
            &long_end_raw_job,
            std::io::Cursor::new(distinct_wav.clone()),
            &mut long_end_raw,
        )
        .unwrap();
        let expected_tail = &distinct_wav[44 + 11025 * 4..];
        assert_eq!(long_end_raw, expected_tail);

        let mut long_end_wav_job = make_job(OutputContainer::Wav, Some(16), Some(2));
        long_end_wav_job.start_sec = Some(0.25);
        long_end_wav_job.end_sec = Some(10.0);
        let mut long_end_wav = Vec::new();
        execute_transcode(
            &long_end_wav_job,
            std::io::Cursor::new(distinct_wav.clone()),
            &mut long_end_wav,
        )
        .unwrap();
        assert_eq!(long_end_wav.len(), 44 + expected_tail.len());
        assert_eq!(
            u32::from_le_bytes(long_end_wav[40..44].try_into().unwrap()) as usize,
            expected_tail.len()
        );

        // Corrupt stream: garbage bytes rejected cleanly.
        let mut err_out = Vec::new();
        assert!(execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(16), Some(2)),
            std::io::Cursor::new(b"not a valid audio container at all".to_vec()),
            &mut err_out,
        )
        .is_err());

        // Corrupt FLAC frames after valid STREAMINFO header return Err cleanly.
        let mut corrupt_flac = Vec::new();
        write_flac_header(&mut corrupt_flac, 44100, 2, 16, 4096).unwrap();
        corrupt_flac.extend_from_slice(&[0xFF; 256]);
        let mut corrupt_flac_out = Vec::new();
        assert!(execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(16), Some(2)),
            std::io::Cursor::new(corrupt_flac),
            &mut corrupt_flac_out,
        )
        .is_err());

        // Premature EOF WAV (declares 1.0s, truncated mid-stream) does not panic.
        let mut truncated_wav = distinct_wav[..44 + 4000].to_vec();
        let mut trunc_out = Vec::new();
        let _ = execute_transcode(
            &make_job(OutputContainer::RawPcmLe, Some(16), Some(2)),
            std::io::Cursor::new(truncated_wav.split_off(0)),
            &mut trunc_out,
        );
        assert!(trunc_out.len() <= 4000);
    }
}
