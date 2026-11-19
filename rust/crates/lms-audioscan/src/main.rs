#![deny(unsafe_op_in_unsafe_fn)]

use lms_audioscan::find_frame_offset;
use lms_audioscan::scan_file;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

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

fn write_stdout_line(line: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()
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

fn is_negative_i64_token(arg: &OsStr) -> bool {
    let Some(s) = arg.to_str() else {
        return false;
    };
    s.starts_with('-')
        && s.len() > 1
        && s.as_bytes()[1..].iter().all(|b| b.is_ascii_digit())
        && s.parse::<i64>().is_ok()
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
            if !is_negative_i64_token(&arg) {
                let lossy = arg.to_string_lossy();
                if lossy.starts_with('-') && arg.len() > 1 {
                    return Err(format!("Unknown option: {lossy}"));
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
                let include_tags = !cfg.info_only;
                let res = scan_file(Path::new(path_arg), true, include_tags, cfg.no_artwork)
                    .map_err(|err| format!("Scan error: {err}"))?;
                let json = serde_json::to_string(&res)
                    .map_err(|err| format!("JSON serialization error: {err}"))?;
                write_stdout_line(&json).map_err(|err| format!("Stdout error: {err}"))
            }
            _ => Err("Usage: lms-audioscan scan [options] <path>".to_string()),
        },
        "scan-tags" => {
            if cfg.info_only {
                return Err("--info-only cannot be used with scan-tags".to_string());
            }
            match rest {
                [path_arg] => {
                    let res = scan_file(Path::new(path_arg), false, true, cfg.no_artwork)
                        .map_err(|err| format!("Scan error: {err}"))?;
                    let json = serde_json::to_string(&res)
                        .map_err(|err| format!("JSON serialization error: {err}"))?;
                    write_stdout_line(&json).map_err(|err| format!("Stdout error: {err}"))
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
                    let res =
                        find_frame_offset(Path::new(path_arg), Some(format_str), target_offset)
                            .map_err(|err| format!("Find frame error: {err}"))?;
                    let json = serde_json::to_string(&res)
                        .map_err(|err| format!("JSON serialization error: {err}"))?;
                    write_stdout_line(&json).map_err(|err| format!("Stdout error: {err}"))
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
