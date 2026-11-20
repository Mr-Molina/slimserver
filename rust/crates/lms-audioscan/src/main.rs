#![deny(unsafe_op_in_unsafe_fn)]

use lms_audioscan::find_frame_offset;
use lms_audioscan::scan_file;
use lms_audioscan::ScanResult;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

const MAX_SINGLE_BINARY_B64_BYTES: usize = 24 * 1024 * 1024;
const MAX_TOTAL_BINARY_B64_BYTES: usize = 32 * 1024 * 1024;
const MAX_JSON_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

struct CliConfig {
    no_artwork: bool,
    info_only: bool,
    positionals: Vec<OsString>,
}

enum ParseOutcome {
    Help,
    Run(CliConfig),
}

fn env_flag_true(name: &str) -> bool {
    match env::var(name) {
        Ok(val) => val == "1" || val.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

struct CappedWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let next = self.written.saturating_add(buf.len());
        if next > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON output exceeds maximum allowed size",
            ));
        }
        let n = self.inner.write(buf)?;
        self.written = self.written.saturating_add(n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_json_line<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let stdout = io::stdout().lock();
    let capped = CappedWriter {
        inner: stdout,
        written: 0,
        limit: MAX_JSON_OUTPUT_BYTES,
    };
    let mut writer = BufWriter::new(capped);
    serde_json::to_writer(&mut writer, value)
        .map_err(|err| format!("JSON serialization error: {err}"))?;
    writer
        .write_all(b"\n")
        .map_err(|err| format!("Stdout error: {err}"))?;
    writer.flush().map_err(|err| format!("Stdout error: {err}"))
}

fn cap_b64_payload(val: &mut serde_json::Value, remaining_budget: &mut usize) {
    let Some(s) = val.as_str() else {
        return;
    };
    let len = s.len();
    if len <= MAX_SINGLE_BINARY_B64_BYTES && len <= *remaining_budget {
        *remaining_budget -= len;
    } else {
        let approx_bytes = (len / 4).saturating_mul(3);
        *val = serde_json::json!(approx_bytes);
    }
}

fn is_standard_base64(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let body_end = if bytes.ends_with(b"==") {
        bytes.len() - 2
    } else if bytes.ends_with(b"=") {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    bytes[..body_end]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

fn should_cap_binary_string(s: &str) -> bool {
    s.len() > MAX_SINGLE_BINARY_B64_BYTES || (s.len() >= 1024 && is_standard_base64(s))
}

/// Cap a picture object's base64 payload field (`image` or `image_data`).
fn cap_picture_object(item: &mut serde_json::Value, remaining_budget: &mut usize) {
    let mut handled = false;
    for key in ["image", "image_data"] {
        if let Some(slot) = item.get_mut(key) {
            cap_b64_payload(slot, remaining_budget);
            handled = true;
        }
    }
    if !handled && item.as_str().is_some_and(should_cap_binary_string) {
        cap_b64_payload(item, remaining_budget);
    }
}

fn cap_picture_object_list(val: &mut serde_json::Value, remaining_budget: &mut usize) {
    if let Some(arr) = val.as_array_mut() {
        for item in arr {
            cap_picture_object(item, remaining_budget);
        }
    } else {
        cap_picture_object(val, remaining_budget);
    }
}

fn cap_scan_result_binaries(result: &mut ScanResult) {
    let mut remaining_budget = MAX_TOTAL_BINARY_B64_BYTES;

    for (key, val) in &mut result.tags {
        match key.as_str() {
            "COVR" | "COVER ART (FRONT)" => {
                cap_b64_payload(val, &mut remaining_budget);
            }
            "APIC" => {
                if let Some(arr) = val.as_array_mut() {
                    let is_nested = arr.first().is_some_and(|v| v.is_array());
                    if is_nested {
                        for entry in arr {
                            if let Some(slot) = entry.get_mut(3) {
                                cap_b64_payload(slot, &mut remaining_budget);
                            }
                        }
                    } else if let Some(slot) = arr.get_mut(3) {
                        cap_b64_payload(slot, &mut remaining_budget);
                    }
                }
            }
            "ALLPICTURES" | "WM/Picture" | "WM/PICTURE" => {
                cap_picture_object_list(val, &mut remaining_budget);
            }
            _ => {
                if let Some(arr) = val.as_array_mut() {
                    for item in arr {
                        if item.as_str().is_some_and(should_cap_binary_string) {
                            cap_b64_payload(item, &mut remaining_budget);
                        }
                    }
                } else if val.as_str().is_some_and(should_cap_binary_string) {
                    cap_b64_payload(val, &mut remaining_budget);
                }
            }
        }
    }
}

fn print_usage() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "Usage: lms-audioscan [options] <subcommand> [args...]"
    )?;
    writeln!(
        stdout,
        "Subcommands: scan <path>, scan-tags <path>, find-frame <format> <path> <offset_or_ms>"
    )?;
    writeln!(stdout, "Options:")?;
    writeln!(stdout, "  -h, --help     Display this help message")?;
    writeln!(
        stdout,
        "  --no-artwork   Omit embedded artwork payloads (or AUDIO_SCAN_NO_ARTWORK=1)"
    )?;
    writeln!(
        stdout,
        "  --info-only    Extract stream info only, omitting tags (scan)"
    )?;
    writeln!(
        stdout,
        "  --             Treat all following arguments as positional"
    )?;
    stdout.flush()
}

enum NegativeNumericToken {
    NotNegativeDigits,
    ValidI64,
    Overflow(String),
}

fn classify_negative_numeric_token(arg: &OsStr) -> NegativeNumericToken {
    let Some(s) = arg.to_str() else {
        return NegativeNumericToken::NotNegativeDigits;
    };
    if s.starts_with('-') && s.len() > 1 && s.as_bytes()[1..].iter().all(|b| b.is_ascii_digit()) {
        match s.parse::<i64>() {
            Ok(_) => NegativeNumericToken::ValidI64,
            Err(err) => {
                NegativeNumericToken::Overflow(format!("Invalid numeric argument '{s}': {err}"))
            }
        }
    } else {
        NegativeNumericToken::NotNegativeDigits
    }
}

fn parse_args() -> Result<ParseOutcome, String> {
    let mut no_artwork = env_flag_true("AUDIO_SCAN_NO_ARTWORK");
    let mut info_only = false;
    let mut after_double_dash = false;
    let mut positionals: Vec<OsString> = Vec::new();

    let mut args = env::args_os();
    let _program = args.next();

    for arg in args {
        if !after_double_dash {
            if let Some(s) = arg.to_str() {
                match s {
                    "--" => {
                        after_double_dash = true;
                        continue;
                    }
                    "-h" | "--help" => return Ok(ParseOutcome::Help),
                    "--no-artwork" => {
                        no_artwork = true;
                        continue;
                    }
                    "--info-only" => {
                        info_only = true;
                        continue;
                    }
                    _ => {}
                }
            }
            match classify_negative_numeric_token(&arg) {
                NegativeNumericToken::ValidI64 => {}
                NegativeNumericToken::Overflow(msg) => return Err(msg),
                NegativeNumericToken::NotNegativeDigits => {
                    let lossy = arg.to_string_lossy();
                    if lossy.starts_with('-') && arg.len() > 1 {
                        return Err(format!("Unknown option: {lossy}"));
                    }
                }
            }
        }
        positionals.push(arg);
    }

    Ok(ParseOutcome::Run(CliConfig {
        no_artwork,
        info_only,
        positionals,
    }))
}

fn execute(cfg: CliConfig) -> Result<(), String> {
    let Some((subcmd, rest)) = cfg.positionals.split_first() else {
        if let Err(err) = print_usage() {
            return Err(format!("Stdout error: {err}"));
        }
        return Err(String::new());
    };

    let Some(subcmd_str) = subcmd.to_str() else {
        return Err(format!("Unknown subcommand: {}", subcmd.to_string_lossy()));
    };

    match subcmd_str {
        "scan" => match rest {
            [path_arg] => {
                let path = Path::new(path_arg);
                let include_tags = !cfg.info_only;
                let mut res = scan_file(path, true, include_tags, cfg.no_artwork)
                    .map_err(|err| format!("Scan error: {err}"))?;
                cap_scan_result_binaries(&mut res);
                write_json_line(&res)
            }
            _ => Err("Usage: lms-audioscan scan [options] <path>".to_string()),
        },
        "scan-tags" => {
            if cfg.info_only {
                return Err("--info-only cannot be used with scan-tags".to_string());
            }
            match rest {
                [path_arg] => {
                    let path = Path::new(path_arg);
                    let mut res = scan_file(path, false, true, cfg.no_artwork)
                        .map_err(|err| format!("Scan error: {err}"))?;
                    cap_scan_result_binaries(&mut res);
                    write_json_line(&res)
                }
                _ => Err("Usage: lms-audioscan scan-tags [options] <path>".to_string()),
            }
        }
        "find-frame" => {
            if cfg.info_only {
                return Err("--info-only cannot be used with find-frame".to_string());
            }
            match rest {
                [format_arg, path_arg, offset_arg] => {
                    let format_str = format_arg.to_str().ok_or_else(|| {
                        format!("Invalid format: {}", format_arg.to_string_lossy())
                    })?;
                    let offset_str = offset_arg.to_str().ok_or_else(|| {
                        format!("Invalid offset_or_ms '{}'", offset_arg.to_string_lossy())
                    })?;
                    let target_offset = offset_str
                        .parse::<i64>()
                        .map_err(|err| format!("Invalid offset_or_ms '{offset_str}': {err}"))?;
                    let path = Path::new(path_arg);
                    let res = find_frame_offset(path, Some(format_str), target_offset)
                        .map_err(|err| format!("Find frame error: {err}"))?;
                    write_json_line(&res)
                }
                _ => Err(
                    "Usage: lms-audioscan find-frame <format> <path> <offset_or_ms>".to_string(),
                ),
            }
        }
        other => Err(format!("Unknown subcommand: {other}")),
    }
}

fn main() -> ExitCode {
    let outcome = match parse_args() {
        Ok(outcome) => outcome,
        Err(msg) => {
            let _ = writeln!(io::stderr().lock(), "{msg}");
            return ExitCode::FAILURE;
        }
    };

    match outcome {
        ParseOutcome::Help => {
            if let Err(err) = print_usage() {
                let _ = writeln!(io::stderr().lock(), "Stdout error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        ParseOutcome::Run(cfg) => {
            if let Err(msg) = execute(cfg) {
                if !msg.is_empty() {
                    let _ = writeln!(io::stderr().lock(), "{msg}");
                }
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cap_scan_result_binaries_enforces_cumulative_budget_for_binary_tags() {
        let mut tags = serde_json::Map::new();
        // Two 20 MiB valid base64 binary tags (< 24 MiB single cap, > 32 MiB total).
        let chunk_len = 20 * 1024 * 1024;
        tags.insert("GEOB".to_string(), serde_json::json!("A".repeat(chunk_len)));
        tags.insert("PRIV".to_string(), serde_json::json!("B".repeat(chunk_len)));
        tags.insert(
            "WM/Picture".to_string(),
            serde_json::json!({
                "mime_type": "image/jpeg",
                "image_type": 3,
                "image": "C".repeat(4096),
            }),
        );
        tags.insert("TIT2".to_string(), serde_json::json!("Normal Title"));

        let mut res = ScanResult {
            info: serde_json::Map::new(),
            tags,
        };
        cap_scan_result_binaries(&mut res);

        // First 20 MiB binary tag fits inside 32 MiB budget.
        assert!(res.tags["GEOB"].is_string());
        // Second 20 MiB binary tag exceeds remaining 12 MiB budget -> approx bytes.
        assert_eq!(res.tags["PRIV"].as_u64(), Some(15 * 1024 * 1024));
        // WM/Picture image stays within remaining 12 MiB budget.
        assert!(res.tags["WM/Picture"]["image"].is_string());
        // Normal text tag is untouched.
        assert_eq!(res.tags["TIT2"].as_str(), Some("Normal Title"));
    }

    #[test]
    fn test_cap_scan_result_binaries_zero_budget_leaves_text_tags_intact() {
        let mut tags = serde_json::Map::new();
        tags.insert(
            "ALLPICTURES".to_string(),
            serde_json::json!([
                {"image_data": "A".repeat(16 * 1024 * 1024)},
                {"image_data": "B".repeat(16 * 1024 * 1024)},
            ]),
        );
        tags.insert(
            "WM/Picture".to_string(),
            serde_json::json!([
                {"image_type": 3, "image": "D".repeat(4096)},
            ]),
        );
        tags.insert("PRIV".to_string(), serde_json::json!("C".repeat(4096)));
        tags.insert("TIT2".to_string(), serde_json::json!("ABCD"));

        let mut res = ScanResult {
            info: serde_json::Map::new(),
            tags,
        };
        cap_scan_result_binaries(&mut res);

        assert_eq!(res.tags["WM/Picture"][0]["image"].as_u64(), Some(3072));
        assert_eq!(res.tags["PRIV"].as_u64(), Some(3072));
        assert_eq!(res.tags["TIT2"].as_str(), Some("ABCD"));
    }
}
