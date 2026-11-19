#![deny(unsafe_op_in_unsafe_fn)]

use lms_mp3cut::GaplessCutStream;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

/// Parsed and validated command-line options for `lms-mp3cut`.
#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    file: PathBuf,
    cache_dir: Option<PathBuf>,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    index_only: bool,
}

/// Top-level CLI parse outcome.
#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Help,
    Run(CliOptions),
}

fn print_usage() {
    eprintln!(
        "Usage: lms-mp3cut --file <path> [--cache-dir <dir>] \
         [--start-ms <ms>] [--end-ms <ms>] [--index-only] [-h|--help] \
         [-- <path>]"
    );
}

fn is_flag_like(val: &OsStr) -> bool {
    val.to_string_lossy().starts_with('-')
}

fn take_path_arg(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, String> {
    let Some(val) = iter.next() else {
        return Err(format!("Error: {flag} requires an argument"));
    };
    if val.is_empty() {
        return Err(format!("Error: {flag} requires a non-empty path"));
    }
    if is_flag_like(&val) {
        return Err(format!(
            "Error: {flag} requires a path, got flag-like argument '{}'; \
             use '--' before paths starting with '-'",
            val.to_string_lossy()
        ));
    }
    Ok(PathBuf::from(val))
}

fn take_u64_arg(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<u64, String> {
    let Some(val) = iter.next() else {
        return Err(format!("Error: {flag} requires an argument"));
    };
    let Some(text) = val.to_str() else {
        return Err(format!(
            "Error: invalid {flag} value: {}",
            val.to_string_lossy()
        ));
    };
    if text.starts_with('-') {
        return Err(format!(
            "Error: {flag} requires a non-negative integer, got '{text}'"
        ));
    }
    text.parse::<u64>()
        .map_err(|_| format!("Error: invalid {flag} value: {text}"))
}

fn parse_args<I>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut iter = args.into_iter();
    // Skip argv[0].
    let _program = iter.next();

    let mut file: Option<PathBuf> = None;
    let mut cache_dir: Option<PathBuf> = None;
    let mut start_ms: Option<u64> = None;
    let mut end_ms: Option<u64> = None;
    let mut index_only = false;
    let mut after_double_dash = false;

    while let Some(arg) = iter.next() {
        if after_double_dash {
            if file.is_some() {
                return Err(format!(
                    "Error: unexpected argument after '--': {}",
                    arg.to_string_lossy()
                ));
            }
            if arg.is_empty() {
                return Err(String::from("Error: file path must not be empty"));
            }
            file = Some(PathBuf::from(arg));
            continue;
        }

        match arg.to_str() {
            Some("--") => {
                after_double_dash = true;
            }
            Some("--help") | Some("-h") => {
                return Ok(CliAction::Help);
            }
            Some("--index-only") => {
                index_only = true;
            }
            Some("--file") => {
                file = Some(take_path_arg(&mut iter, "--file")?);
            }
            Some("--cache-dir") => {
                cache_dir = Some(take_path_arg(&mut iter, "--cache-dir")?);
            }
            Some("--start-ms") => {
                start_ms = Some(take_u64_arg(&mut iter, "--start-ms")?);
            }
            Some("--end-ms") => {
                end_ms = Some(take_u64_arg(&mut iter, "--end-ms")?);
            }
            Some(other) => {
                return Err(format!("Error: unrecognized argument: {other}"));
            }
            None => {
                return Err(format!(
                    "Error: unrecognized argument: {}",
                    arg.to_string_lossy()
                ));
            }
        }
    }

    let Some(file_path) = file else {
        return Err(String::from("Error: missing required --file argument"));
    };

    if index_only && cache_dir.is_none() {
        return Err(String::from("Error: --index-only requires --cache-dir"));
    }

    if let (Some(start), Some(end)) = (start_ms, end_ms) {
        if start > end {
            return Err(format!(
                "Error: --start-ms ({start}) must not exceed --end-ms ({end})"
            ));
        }
    }

    Ok(CliAction::Run(CliOptions {
        file: file_path,
        cache_dir,
        start_ms,
        end_ms,
        index_only,
    }))
}

fn run(opts: CliOptions) -> ExitCode {
    let cache_ref = opts.cache_dir.as_deref();

    if opts.index_only {
        // Pre-scan and generate cache without streaming audio.
        return match GaplessCutStream::open(Path::new(&opts.file), cache_ref, None, None) {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Error indexing MP3 file {}: {err}", opts.file.display());
                ExitCode::FAILURE
            }
        };
    }

    let mut cutter = match GaplessCutStream::open(
        Path::new(&opts.file),
        cache_ref,
        opts.start_ms,
        opts.end_ms,
    ) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "Error opening MP3 cut stream for {}: {err}",
                opts.file.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let mut buf = [0u8; 32768];

    loop {
        let n = match cutter.read_chunk(&mut buf) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("Error reading MP3 slice: {err}");
                return ExitCode::FAILURE;
            }
        };
        if n == 0 {
            break;
        }
        match lock.write_all(&buf[..n]) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                // Downstream reader closed pipe; terminate cleanly.
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                eprintln!("Error writing audio chunk to stdout: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    match lock.flush() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error flushing stdout: {err}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    match parse_args(std::env::args_os()) {
        Ok(CliAction::Help) => {
            print_usage();
            ExitCode::SUCCESS
        }
        Ok(CliAction::Run(opts)) => run(opts),
        Err(msg) => {
            eprintln!("{msg}");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_args;
    use super::CliAction;
    use super::CliOptions;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn os_args(items: &[&str]) -> Vec<OsString> {
        let mut out = Vec::with_capacity(items.len() + 1);
        out.push(OsString::from("lms-mp3cut"));
        for item in items {
            out.push(OsString::from(item));
        }
        out
    }

    #[cfg(unix)]
    fn non_utf8_os_path() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(b"/music/track_\xff.mp3".to_vec())
    }

    #[cfg(windows)]
    fn non_utf8_os_path() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        // Include unpaired UTF-16 surrogate (0xD800) for non-UTF-8 path testing.
        let wide = [
            0x0043, 0x003a, 0x005c, 0x006d, 0x0075, 0x0073, 0x0069, 0x0063, 0x005c, 0xd800, 0x002e,
            0x006d, 0x0070, 0x0033,
        ];
        OsString::from_wide(&wide)
    }

    #[test]
    fn parse_valid_stream_args() {
        let args = os_args(&[
            "--file",
            "/tmp/song.mp3",
            "--cache-dir",
            "/tmp/cache",
            "--start-ms",
            "1000",
            "--end-ms",
            "5000",
        ]);
        let parsed = parse_args(args);
        assert_eq!(
            parsed,
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from("/tmp/song.mp3"),
                cache_dir: Some(PathBuf::from("/tmp/cache")),
                start_ms: Some(1000),
                end_ms: Some(5000),
                index_only: false,
            }))
        );
    }

    #[test]
    fn parse_valid_index_only_with_cache_dir() {
        let args = os_args(&[
            "--file",
            "/tmp/song.mp3",
            "--cache-dir",
            "/tmp/cache",
            "--index-only",
        ]);
        let parsed = parse_args(args);
        assert_eq!(
            parsed,
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from("/tmp/song.mp3"),
                cache_dir: Some(PathBuf::from("/tmp/cache")),
                start_ms: None,
                end_ms: None,
                index_only: true,
            }))
        );
    }

    #[test]
    fn parse_help_short_and_long() {
        assert_eq!(parse_args(os_args(&["-h"])), Ok(CliAction::Help));
        assert_eq!(parse_args(os_args(&["--help"])), Ok(CliAction::Help));
    }

    #[test]
    fn reject_index_only_without_cache_dir() {
        let args = os_args(&["--file", "/tmp/song.mp3", "--index-only"]);
        let res = parse_args(args);
        assert!(res.is_err());
    }

    #[test]
    fn reject_start_ms_greater_than_end_ms() {
        let args = os_args(&[
            "--file",
            "/tmp/song.mp3",
            "--start-ms",
            "5001",
            "--end-ms",
            "5000",
        ]);
        let res = parse_args(args);
        assert!(res.is_err());
    }

    #[test]
    fn accept_equal_start_and_end_ms() {
        let args = os_args(&[
            "--file",
            "/tmp/song.mp3",
            "--start-ms",
            "2500",
            "--end-ms",
            "2500",
        ]);
        let res = parse_args(args);
        assert!(res.is_ok());
    }

    #[test]
    fn reject_flag_swallowing_and_missing_values() {
        assert!(parse_args(os_args(&["--file"])).is_err());
        assert!(parse_args(os_args(&["--file", "--cache-dir", "/tmp"])).is_err());
        assert!(parse_args(os_args(&["--file", "a.mp3", "--cache-dir", "--index-only"])).is_err());
        assert!(parse_args(os_args(&["--file", "a.mp3", "--start-ms", "--end-ms"])).is_err());
        assert!(parse_args(os_args(&["--file", "a.mp3", "--end-ms", "-10"])).is_err());
    }

    #[test]
    fn parse_non_utf8_os_string_file_path() {
        let raw_path = non_utf8_os_path();
        let args = vec![
            OsString::from("lms-mp3cut"),
            OsString::from("--file"),
            raw_path.clone(),
        ];
        let parsed = parse_args(args);
        assert_eq!(
            parsed,
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from(raw_path),
                cache_dir: None,
                start_ms: None,
                end_ms: None,
                index_only: false,
            }))
        );
    }

    #[test]
    fn parse_double_dash_separator_for_leading_dash_path() {
        let args = os_args(&[
            "--cache-dir",
            "/tmp/cache",
            "--index-only",
            "--",
            "-leading-dash.mp3",
        ]);
        let parsed = parse_args(args);
        assert_eq!(
            parsed,
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from("-leading-dash.mp3"),
                cache_dir: Some(PathBuf::from("/tmp/cache")),
                start_ms: None,
                end_ms: None,
                index_only: true,
            }))
        );
    }

    #[test]
    fn parse_double_dash_treats_help_flag_as_filename() {
        let args = os_args(&["--", "--help"]);
        let parsed = parse_args(args);
        assert_eq!(
            parsed,
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from("--help"),
                cache_dir: None,
                start_ms: None,
                end_ms: None,
                index_only: false,
            }))
        );
    }

    #[test]
    fn reject_extra_positional_arg_after_double_dash() {
        let args = os_args(&["--file", "a.mp3", "--", "b.mp3"]);
        assert!(parse_args(args).is_err());

        let args2 = os_args(&["--", "a.mp3", "b.mp3"]);
        assert!(parse_args(args2).is_err());
    }
}
