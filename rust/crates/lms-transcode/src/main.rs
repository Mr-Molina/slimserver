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

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeJob {
    pub input_path: Option<PathBuf>,
    pub output_format: OutputContainer,
    pub start_sec: Option<f64>,
    pub end_sec: Option<f64>,
    pub target_sample_rate: Option<u32>,
    pub target_channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
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

fn detect_mode_from_args(args: &[OsString]) -> String {
    let mut pre_dash: Vec<&str> = Vec::new();
    for arg in args {
        if arg == OsStr::new("--") {
            break;
        }
        if let Some(s) = arg.to_str() {
            pre_dash.push(s);
        }
    }
    let has_j = pre_dash.contains(&"-j");
    let has_e = pre_dash.contains(&"-e");
    let has_w = pre_dash.contains(&"-w");
    let has_f = pre_dash.contains(&"-f");
    if has_j || has_e || (has_w && has_f) {
        return "faad".to_string();
    }
    if pre_dash.iter().any(|a| {
        a.starts_with("--skip") || a.starts_with("--until") || a.contains("force-raw-format")
    }) {
        return "flac".to_string();
    }
    if pre_dash
        .iter()
        .any(|a| matches!(*a, "-wq" | "-rq" | "--dsf" | "--dff"))
    {
        return "wvunpack".to_string();
    }
    "sox".to_string()
}

fn parse_faad_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut bits_per_sample = None;
    let mut input_path = None;
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
                    other => other.parse::<u16>().map_err(|e| {
                        TranscodeError::Parse(format!("invalid faad bits '{other}': {e}"))
                    })?,
                });
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
                if let Some(rest) = s.strip_prefix("-j") {
                    start_sec = Some(parse_time_offset(rest)?);
                } else if let Some(rest) = s.strip_prefix("-e") {
                    end_sec = Some(parse_time_offset(rest)?);
                } else {
                    return Err(TranscodeError::Parse(format!("unknown faad option '{s}'")));
                }
            }
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

fn parse_endian_flag(val: &str, opt: &str) -> Result<bool, TranscodeError> {
    match val {
        "little" => Ok(false),
        "big" => Ok(true),
        other => Err(TranscodeError::Parse(format!(
            "invalid {opt} value '{other}'"
        ))),
    }
}

fn parse_flac_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut is_encode = false;
    let mut force_raw = false;
    let mut endian_big = false;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut bits_per_sample = None;
    let mut target_sample_rate = None;
    let mut target_channels = None;
    let mut input_path = None;
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
            "--decode" => is_encode = false,
            "--stdout" => {}
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
                if val != "signed" && val != "unsigned" {
                    return Err(TranscodeError::Parse(format!(
                        "invalid --sign value '{val}'"
                    )));
                }
            }
            "--bps" => {
                let val = next_str_arg(&mut iter, "--bps")?;
                bits_per_sample =
                    Some(val.parse::<u16>().map_err(|e| {
                        TranscodeError::Parse(format!("invalid --bps '{val}': {e}"))
                    })?);
            }
            "--sample-rate" => {
                let val = next_str_arg(&mut iter, "--sample-rate")?;
                target_sample_rate = Some(val.parse::<u32>().map_err(|e| {
                    TranscodeError::Parse(format!("invalid --sample-rate '{val}': {e}"))
                })?);
            }
            "--channels" => {
                let val = next_str_arg(&mut iter, "--channels")?;
                target_channels = Some(val.parse::<u16>().map_err(|e| {
                    TranscodeError::Parse(format!("invalid --channels '{val}': {e}"))
                })?);
            }
            "-o" | "--output-name" => {
                let _ = next_os_arg(&mut iter, s)?;
            }
            _ => {
                if let Some(val) = s.strip_prefix("--skip=") {
                    start_sec = Some(parse_time_offset(val)?);
                } else if let Some(val) = s.strip_prefix("--until=") {
                    end_sec = Some(parse_time_offset(val)?);
                } else if let Some(val) = s.strip_prefix("--endian=") {
                    endian_big = parse_endian_flag(val, "--endian=")?;
                } else if let Some(val) = s.strip_prefix("--sign=") {
                    if val != "signed" && val != "unsigned" {
                        return Err(TranscodeError::Parse(format!(
                            "invalid --sign= value '{val}'"
                        )));
                    }
                } else if let Some(val) = s.strip_prefix("--bps=") {
                    bits_per_sample = Some(val.parse::<u16>().map_err(|e| {
                        TranscodeError::Parse(format!("invalid --bps '{val}': {e}"))
                    })?);
                } else if let Some(val) = s.strip_prefix("--sample-rate=") {
                    target_sample_rate = Some(val.parse::<u32>().map_err(|e| {
                        TranscodeError::Parse(format!("invalid --sample-rate '{val}': {e}"))
                    })?);
                } else if let Some(val) = s.strip_prefix("--channels=") {
                    target_channels = Some(val.parse::<u16>().map_err(|e| {
                        TranscodeError::Parse(format!("invalid --channels '{val}': {e}"))
                    })?);
                } else if let Some(lvl) = s.strip_prefix("--compression-level-") {
                    if !matches!(lvl, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8") {
                        return Err(TranscodeError::Parse(format!(
                            "invalid flac compression level '{lvl}'"
                        )));
                    }
                    is_encode = true;
                } else if !s.starts_with("--") {
                    let mut saw_c = false;
                    let mut saw_d = false;
                    for ch in s.chars().skip(1) {
                        match ch {
                            'c' => saw_c = true,
                            'd' => saw_d = true,
                            's' | 'f' | 'w' => {}
                            '0'..='8' => is_encode = true,
                            _ => {
                                return Err(TranscodeError::Parse(format!(
                                    "unknown flac option '-{ch}'"
                                )));
                            }
                        }
                    }
                    if saw_c && !saw_d {
                        is_encode = true;
                    }
                } else {
                    return Err(TranscodeError::Parse(format!("unknown flac option '{s}'")));
                }
            }
        }
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

fn parse_wvunpack_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut input_path = None;
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
            "-q" | "--dsf" | "--dff" => {}
            "-o" => {
                let _ = next_os_arg(&mut iter, "-o")?;
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
    if matches!(peek_str, "rate" | "gain" | "vol" | "channels" | "dither") {
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
        let rate = s
            .parse::<u32>()
            .map_err(|e| TranscodeError::Parse(format!("invalid sox rate '{s}': {e}")))?;
        if rate == 0 {
            return Err(TranscodeError::Parse("sox rate must be positive".into()));
        }
        *target_sample_rate = Some(rate);
        return Ok(());
    }
    Err(TranscodeError::Parse(
        "missing sample rate for sox rate".into(),
    ))
}

fn parse_sox_os(args: &[OsString]) -> Result<TranscodeJob, TranscodeError> {
    let mut output_format = OutputContainer::Wav;
    let mut start_sec = None;
    let mut end_sec = None;
    let mut target_sample_rate = None;
    let mut target_channels = None;
    let mut bits_per_sample = None;
    let mut input_path = None;
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
            "-q" | "--ignore-length" | "-D" | "--no-dither" | "-s" | "-u" | "-f" | "-A" | "-U"
            | "-X" => {}
            "-L" => {
                if is_output {
                    endian_big = false;
                    if output_format == OutputContainer::RawPcmBe {
                        output_format = OutputContainer::RawPcmLe;
                    }
                }
            }
            "-B" | "-x" | "$-x$" => {
                if is_output {
                    endian_big = true;
                    if output_format == OutputContainer::RawPcmLe {
                        output_format = OutputContainer::RawPcmBe;
                    }
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
                    apply_sox_output_type(val, endian_big, &mut output_format)?;
                }
            }
            "-r" | "--rate" => {
                let val = next_str_arg(&mut iter, s)?;
                let rate = val
                    .parse::<u32>()
                    .map_err(|e| TranscodeError::Parse(format!("invalid sox rate '{val}': {e}")))?;
                if is_output {
                    target_sample_rate = Some(rate);
                }
            }
            "-c" | "--channels" => {
                let val = next_str_arg(&mut iter, s)?;
                let ch = val.parse::<u16>().map_err(|e| {
                    TranscodeError::Parse(format!("invalid sox channels '{val}': {e}"))
                })?;
                if is_output {
                    target_channels = Some(ch);
                }
            }
            "-b" | "--bits" => {
                let val = next_str_arg(&mut iter, s)?;
                let bps = val
                    .parse::<u16>()
                    .map_err(|e| TranscodeError::Parse(format!("invalid sox bits '{val}': {e}")))?;
                if is_output {
                    bits_per_sample = Some(bps);
                }
            }
            "-C" | "--compression" => {
                let _ = next_str_arg(&mut iter, s)?;
            }
            "-e" | "--encoding" => {
                let _ = next_str_arg(&mut iter, s)?;
            }
            "--endian" => {
                let val = next_str_arg(&mut iter, "--endian")?;
                let big = parse_endian_flag(val, "--endian")?;
                if is_output {
                    endian_big = big;
                    if output_format == OutputContainer::RawPcmLe
                        || output_format == OutputContainer::RawPcmBe
                    {
                        output_format = if endian_big {
                            OutputContainer::RawPcmBe
                        } else {
                            OutputContainer::RawPcmLe
                        };
                    }
                }
            }
            _ => {
                if let Some(val) = s.strip_prefix("--endian=") {
                    let big = parse_endian_flag(val, "--endian=")?;
                    if is_output {
                        endian_big = big;
                        if output_format == OutputContainer::RawPcmLe
                            || output_format == OutputContainer::RawPcmBe
                        {
                            output_format = if endian_big {
                                OutputContainer::RawPcmBe
                            } else {
                                OutputContainer::RawPcmLe
                            };
                        }
                    }
                } else if let Some(val) = s.strip_prefix("--type=") {
                    if is_output {
                        apply_sox_output_type(val, endian_big, &mut output_format)?;
                    }
                } else if let Some(val) = s.strip_prefix("--encoding=") {
                    if val.is_empty() {
                        return Err(TranscodeError::Parse("empty --encoding=".into()));
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

const MAX_FLAC_BLOCK_LEN: usize = 65_536;
const MAX_FLAC_CODED_NUMBER: u64 = 0x000F_FFFF_FFFF;
const MAX_FLAC_SAMPLE_RATE: u32 = 0x000F_FFFF;

fn validate_audio_params(sample_rate: u32, channels: u16, bps: u16) -> Result<(), TranscodeError> {
    if sample_rate == 0 {
        return Err(TranscodeError::Other("invalid sample rate: 0".into()));
    }
    if !(1..=8).contains(&channels) {
        return Err(TranscodeError::Other(format!(
            "unsupported channel count {channels}: must be 1..=8"
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
    let tot = total_samples.min(MAX_FLAC_CODED_NUMBER);
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
                    frame_bytes.extend_from_slice(&(s as i16).to_be_bytes());
                }
                24 => {
                    let [_, b1, b2, b3] = s.to_be_bytes();
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

    let block_align = channels.saturating_mul(bps / 8);
    let byte_rate = sample_rate
        .saturating_mul(u32::from(channels))
        .saturating_mul(u32::from(bps / 8));

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
            .saturating_mul(ch_count)
            .saturating_mul(bytes_per_sample);

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
                            16 => buf.extend_from_slice(&(s as i16).to_le_bytes()),
                            24 => {
                                let [b0, b1, b2, _] = s.to_le_bytes();
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
                            16 => buf.extend_from_slice(&(s as i16).to_be_bytes()),
                            24 => {
                                let [_, b1, b2, b3] = s.to_be_bytes();
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
    let needed_frames = n_frames.max(decoded.capacity());
    let needed_samples = needed_frames.saturating_mul(sc);
    let needs_new = match holder {
        Some(existing) => {
            existing.frame_cap < needed_frames
                || existing.buf.capacity() < needed_samples
                || existing.spec != spec
        }
        None => true,
    };
    if needs_new {
        let buf = SampleBuffer::<T>::new(needed_frames as u64, spec);
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

fn ts_to_frames(
    ts: u64,
    time_base: Option<symphonia::core::units::TimeBase>,
    sample_rate: u32,
) -> u64 {
    if let Some(tb) = time_base {
        let t = tb.calc_time(ts);
        let secs = t.seconds as f64 + t.frac;
        (secs * f64::from(sample_rate)).round().max(0.0) as u64
    } else {
        ts
    }
}

pub fn execute_transcode<R: Read + Seek + Send + Sync + 'static, W: Write>(
    job: &TranscodeJob,
    input: R,
    output: W,
) -> Result<(), TranscodeError> {
    execute_transcode_media_source(job, Box::new(SourceWrapper(input)), output)
}

fn execute_transcode_media_source<W: Write>(
    job: &TranscodeJob,
    media_source: Box<dyn MediaSource>,
    mut output: W,
) -> Result<(), TranscodeError> {
    let is_seekable = media_source.is_seekable();
    let mss = MediaSourceStream::new(media_source, Default::default());
    let hint = Hint::new();
    let meta_opts = Default::default();
    let fmt_opts = Default::default();
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &meta_opts)
        .map_err(TranscodeError::Symphonia)?;
    let mut format = probed.format;

    let (track_id, src_sample_rate, src_channels, codec_params, track_n_frames, time_base) = {
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
        let time_base = track.codec_params.time_base;
        (
            track_id,
            src_sample_rate,
            src_channels,
            codec_params,
            n_frames,
            time_base,
        )
    };

    if src_sample_rate == 0 {
        return Err(TranscodeError::Other(
            "invalid source sample rate: 0".into(),
        ));
    }
    if src_channels == 0 {
        return Err(TranscodeError::Other(
            "invalid source channel count: 0".into(),
        ));
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

    let start_frame: u64 = job
        .start_sec
        .map(|s| (s.max(0.0) * f64::from(src_sample_rate)).round() as u64)
        .unwrap_or(0);
    let end_frame: u64 = job
        .end_sec
        .map(|s| (s.max(0.0) * f64::from(src_sample_rate)).round() as u64)
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
    if start_frame > 0 && is_seekable {
        let start_sec = job.start_sec.unwrap_or(0.0).max(0.0);
        let seek_time =
            symphonia::core::units::Time::new(start_sec.trunc() as u64, start_sec.fract());
        if let Ok(seeked_to) = format.seek(
            symphonia::core::formats::SeekMode::Accurate,
            SeekTo::Time {
                time: seek_time,
                track_id: Some(track_id),
            },
        ) {
            current_frame_pos = ts_to_frames(seeked_to.actual_ts, time_base, src_sample_rate);
            decoder.reset();
        }
    }

    let total_frames_hint = if end_frame != u64::MAX {
        Some(end_frame.saturating_sub(start_frame))
    } else {
        track_n_frames.map(|n| n.saturating_sub(start_frame))
    };
    let total_out_frames_hint = total_frames_hint.map(|f| {
        if out_sample_rate != src_sample_rate {
            let ratio = f64::from(out_sample_rate) / f64::from(src_sample_rate);
            (f as f64 * ratio).round().max(0.0) as u64
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

    let tc = usize::from(out_channels);
    let needs_resample = out_sample_rate != src_sample_rate;
    let resample_chunk_size = 1024usize;
    let mut resampler = if needs_resample {
        use rubato::FastFixedIn;
        use rubato::PolynomialDegree;
        let ratio = f64::from(out_sample_rate) / f64::from(src_sample_rate);
        Some(
            FastFixedIn::<f32>::new(ratio, 1.0, PolynomialDegree::Cubic, resample_chunk_size, tc)
                .map_err(|e| TranscodeError::Resample(e.to_string()))?,
        )
    } else {
        None
    };

    let mut resample_pending_in = vec![Vec::<f32>::new(); tc];
    let mut resample_in_cursor: usize = 0;
    let mut resample_chunk_in = if needs_resample {
        vec![vec![0.0f32; resample_chunk_size]; tc]
    } else {
        Vec::new()
    };
    let mut resample_chunk_out = resampler
        .as_ref()
        .map(|r| rubato::Resampler::output_buffer_allocate(r, true));

    let mut mapped_planar = vec![Vec::<f32>::new(); tc];
    let mut int_samples = vec![Vec::<i32>::new(); tc];
    let mut flac_frame_idx = 0u64;
    let mut flac_pending = vec![Vec::<i32>::new(); tc];

    let mut sample_buf: Option<ManagedSampleBuffer<f32>> = None;
    let mut sample_buf_i16: Option<ManagedSampleBuffer<i16>> = None;
    let mut out_bytes = Vec::<u8>::new();

    let direct_i16 = !needs_resample
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
            ts_to_frames(packet.ts(), time_base, src_sample_rate)
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
            for ch in 0..tc {
                if let Some(ch_vec) = mapped_planar.get_mut(ch) {
                    ch_vec.clear();
                    ch_vec.reserve(sliced_frames);
                }
            }

            for f in slice_start..slice_end {
                let frame_base = f.saturating_mul(sc);
                if sc == tc {
                    for ch in 0..tc {
                        let s = samples.get(frame_base + ch).copied().unwrap_or(0.0);
                        if let Some(ch_vec) = mapped_planar.get_mut(ch) {
                            ch_vec.push(s);
                        }
                    }
                } else if sc == 2 && tc == 1 {
                    let left = samples.get(frame_base).copied().unwrap_or(0.0);
                    let right = samples.get(frame_base + 1).copied().unwrap_or(0.0);
                    if let Some(ch_vec) = mapped_planar.get_mut(0) {
                        ch_vec.push((left + right) * 0.5);
                    }
                } else if sc == 1 && tc == 2 {
                    let mono = samples.get(frame_base).copied().unwrap_or(0.0);
                    if let Some(ch0) = mapped_planar.get_mut(0) {
                        ch0.push(mono);
                    }
                    if let Some(ch1) = mapped_planar.get_mut(1) {
                        ch1.push(mono);
                    }
                } else {
                    for ch in 0..tc {
                        let s = if ch < sc {
                            samples.get(frame_base + ch).copied().unwrap_or(0.0)
                        } else {
                            0.0
                        };
                        if let Some(ch_vec) = mapped_planar.get_mut(ch) {
                            ch_vec.push(s);
                        }
                    }
                }
            }

            if let Some(ref mut res) = resampler {
                use rubato::Resampler;
                for ch in 0..tc {
                    if let (Some(src), Some(dst)) =
                        (mapped_planar.get(ch), resample_pending_in.get_mut(ch))
                    {
                        dst.extend_from_slice(src);
                    }
                }
                loop {
                    let available = resample_pending_in
                        .first()
                        .map_or(0, |v| v.len().saturating_sub(resample_in_cursor));
                    if available < resample_chunk_size {
                        break;
                    }
                    let chunk_end = resample_in_cursor + resample_chunk_size;
                    for ch in 0..tc {
                        if let (Some(src), Some(dst)) = (
                            resample_pending_in
                                .get(ch)
                                .and_then(|v| v.get(resample_in_cursor..chunk_end)),
                            resample_chunk_in.get_mut(ch),
                        ) {
                            dst.copy_from_slice(src);
                        }
                    }
                    resample_in_cursor = chunk_end;

                    let Some(ref mut chunk_out) = resample_chunk_out else {
                        return Err(TranscodeError::Resample(
                            "missing resampler output buffer".into(),
                        ));
                    };
                    let (_in_frames, out_len) = res
                        .process_into_buffer(&resample_chunk_in, chunk_out, None)
                        .map_err(|e| TranscodeError::Resample(e.to_string()))?;
                    convert_f32_to_int_into(chunk_out, out_len, out_bps, &mut int_samples);
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

                let pending_len = resample_pending_in.first().map_or(0, Vec::len);
                if resample_in_cursor == pending_len {
                    for ch in 0..tc {
                        if let Some(buf) = resample_pending_in.get_mut(ch) {
                            buf.clear();
                        }
                    }
                    resample_in_cursor = 0;
                } else if resample_in_cursor >= resample_chunk_size.saturating_mul(4) {
                    let rem = pending_len.saturating_sub(resample_in_cursor);
                    for ch in 0..tc {
                        if let Some(buf) = resample_pending_in.get_mut(ch) {
                            buf.copy_within(resample_in_cursor.., 0);
                            buf.truncate(rem);
                        }
                    }
                    resample_in_cursor = 0;
                }
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

    if let Some(ref mut res) = resampler {
        use rubato::Resampler;
        let pending_len = resample_pending_in.first().map_or(0, Vec::len);
        let rem = pending_len.saturating_sub(resample_in_cursor);
        if rem > 0 {
            for ch in 0..tc {
                if let Some(dst) = resample_chunk_in.get_mut(ch) {
                    dst.fill(0.0);
                    if let Some(src) = resample_pending_in
                        .get(ch)
                        .and_then(|v| v.get(resample_in_cursor..))
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
            let Some(ref mut chunk_out) = resample_chunk_out else {
                return Err(TranscodeError::Resample(
                    "missing resampler output buffer".into(),
                ));
            };
            let (_in_frames, out_len) = res
                .process_into_buffer(&resample_chunk_in, chunk_out, None)
                .map_err(|e| TranscodeError::Resample(e.to_string()))?;
            let ratio = f64::from(out_sample_rate) / f64::from(src_sample_rate);
            let valid_out = ((rem as f64 * ratio).round().max(0.0) as usize).min(out_len);
            convert_f32_to_int_into(chunk_out, valid_out, out_bps, &mut int_samples);
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

    if job.output_format == OutputContainer::Flac {
        let rem = flac_pending.first().map_or(0, Vec::len);
        if rem > 0 {
            write_flac_frame(
                &mut output,
                flac_frame_idx,
                rem,
                out_channels,
                out_bps,
                &flac_pending,
            )?;
        }
    }

    Ok(())
}

fn sanitize_unit_sample(sample: f32) -> f64 {
    if sample.is_finite() {
        f64::from(sample).clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn convert_f32_to_int_into(planar: &[Vec<f32>], num_frames: usize, bps: u16, out: &mut [Vec<i32>]) {
    let tc = planar.len().min(out.len());
    for ch in 0..tc {
        let Some(src) = planar.get(ch).and_then(|v| v.get(..num_frames)) else {
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

fn run(argv0: &OsStr, args: &[OsString]) -> Result<(), TranscodeError> {
    let job = parse_cli_invocation_os(argv0, args)?;
    let stdout = std::io::stdout();
    let mut out_handle = std::io::BufWriter::with_capacity(65536, stdout.lock());

    if let Some(ref path) = job.input_path {
        let file = std::fs::File::open(path)?;
        execute_transcode(&job, file, &mut out_handle)?;
    } else {
        // Stdin is unseekable: stream unbounded; execute_transcode_media_source
        // forward-decode-skips any start_sec offset without truncating or spooling.
        let ro_stdin = symphonia::core::io::ReadOnlySource::new(std::io::stdin());
        execute_transcode_media_source(&job, Box::new(ro_stdin), &mut out_handle)?;
    }

    out_handle.flush()?;
    Ok(())
}

fn print_usage() {
    println!("Usage: lms-transcode [--mode <faad|flac|wvunpack|sox>] [options] <input>");
    println!("Options:");
    println!("  --mode <mode>      Emulate faad, flac, wvunpack, or sox CLI mode");
    println!("  -h, --help         Display this help message");
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
            print_usage();
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            if is_broken_pipe(&e) {
                return std::process::ExitCode::SUCCESS;
            }
            eprintln!("lms-transcode error: {e}");
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
            &["/tmp/in.wv", "-rq", "--skip=10", "--until=20", "-o", "-"].map(String::from),
        )
        .unwrap();
        assert_eq!(job_wv.output_format, OutputContainer::RawPcmLe);
        assert_eq!(job_wv.input_path, Some(PathBuf::from("/tmp/in.wv")));
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
            output_format: OutputContainer::Wav,
            start_sec: Some(0.1),
            end_sec: Some(0.4),
            target_sample_rate: None,
            target_channels: Some(2),
            bits_per_sample: Some(16),
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
            output_format,
            start_sec: None,
            end_sec: None,
            target_sample_rate: None,
            target_channels,
            bits_per_sample,
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
}
