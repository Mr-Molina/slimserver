#![deny(unsafe_op_in_unsafe_fn)]

use lms_mp3cut::decode_mllt_cache;
use lms_mp3cut::encode_mllt_cache;
use lms_mp3cut::is_metadata_valid_for_file;
use lms_mp3cut::mllt_cache_path_from_metadata;
use lms_mp3cut::open_source_mp3;
use lms_mp3cut::read_mllt_cache_safe;
use lms_mp3cut::scan_mp3_frames_reader;
use lms_mp3cut::write_mllt_cache_atomic;
use lms_mp3cut::GaplessCutStream;
use lms_mp3cut::Mp3CutError;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;
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
    val.as_encoded_bytes().starts_with(b"-")
}

#[cfg(unix)]
fn split_long_option(arg: &OsStr) -> Option<(&'static str, OsString)> {
    const PREFIXES: [(&str, &[u8]); 4] = [
        ("--file=", b"--file="),
        ("--cache-dir=", b"--cache-dir="),
        ("--start-ms=", b"--start-ms="),
        ("--end-ms=", b"--end-ms="),
    ];
    let raw = arg.as_bytes();
    for (flag, prefix) in PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return Some((flag, OsStr::from_bytes(rest).to_os_string()));
        }
    }
    None
}

#[cfg(windows)]
fn split_long_option(arg: &OsStr) -> Option<(&'static str, OsString)> {
    const PREFIXES: [&str; 4] = ["--file=", "--cache-dir=", "--start-ms=", "--end-ms="];
    let wide: Vec<u16> = arg.encode_wide().collect();
    for flag in PREFIXES {
        let prefix: Vec<u16> = flag.encode_utf16().collect();
        if wide.starts_with(&prefix) {
            return Some((flag, OsString::from_wide(&wide[prefix.len()..])));
        }
    }
    None
}

#[cfg(not(any(unix, windows)))]
fn split_long_option(arg: &OsStr) -> Option<(&'static str, OsString)> {
    const PREFIXES: [&str; 4] = ["--file=", "--cache-dir=", "--start-ms=", "--end-ms="];
    let text = arg.to_str()?;
    for flag in PREFIXES {
        if let Some(rest) = text.strip_prefix(flag) {
            return Some((flag, OsString::from(rest)));
        }
    }
    None
}

fn parse_path_value(
    val: OsString,
    flag: &str,
    allow_leading_dash: bool,
) -> Result<PathBuf, String> {
    if val.is_empty() {
        return Err(format!("Error: {flag} requires a non-empty path"));
    }
    if !allow_leading_dash && is_flag_like(&val) {
        return Err(format!(
            "Error: {flag} requires a path, got flag-like argument '{}'; \
             use '{flag}=<path>' or '-- <path>' for paths starting with '-'",
            val.to_string_lossy()
        ));
    }
    Ok(PathBuf::from(val))
}

fn take_path_arg(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, String> {
    let Some(val) = iter.next() else {
        return Err(format!("Error: {flag} requires an argument"));
    };
    parse_path_value(val, flag, false)
}

fn parse_u64_value(val: &OsStr, flag: &str) -> Result<u64, String> {
    if val.is_empty() {
        return Err(format!("Error: {flag} requires a non-negative integer"));
    }
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

fn take_u64_arg(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<u64, String> {
    let Some(val) = iter.next() else {
        return Err(format!("Error: {flag} requires an argument"));
    };
    parse_u64_value(&val, flag)
}

fn set_unique<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("Error: duplicate {flag} argument"));
    }
    *slot = Some(value);
    Ok(())
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
    let mut help_requested = false;
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

        if let Some((prefix, val)) = split_long_option(&arg) {
            match prefix {
                "--file=" => {
                    let path = parse_path_value(val, "--file", true)?;
                    set_unique(&mut file, path, "--file")?;
                }
                "--cache-dir=" => {
                    let path = parse_path_value(val, "--cache-dir", true)?;
                    set_unique(&mut cache_dir, path, "--cache-dir")?;
                }
                "--start-ms=" => {
                    let ms = parse_u64_value(&val, "--start-ms")?;
                    set_unique(&mut start_ms, ms, "--start-ms")?;
                }
                "--end-ms=" => {
                    let ms = parse_u64_value(&val, "--end-ms")?;
                    set_unique(&mut end_ms, ms, "--end-ms")?;
                }
                _ => {
                    return Err(format!(
                        "Error: unrecognized argument: {}",
                        arg.to_string_lossy()
                    ));
                }
            }
            continue;
        }

        match arg.as_encoded_bytes() {
            b"--" => {
                after_double_dash = true;
            }
            b"--help" | b"-h" => {
                if help_requested {
                    return Err(String::from("Error: duplicate --help/-h flag"));
                }
                help_requested = true;
            }
            b"--index-only" => {
                if index_only {
                    return Err(String::from("Error: duplicate --index-only flag"));
                }
                index_only = true;
            }
            b"--file" => {
                let path = take_path_arg(&mut iter, "--file")?;
                set_unique(&mut file, path, "--file")?;
            }
            b"--cache-dir" => {
                let path = take_path_arg(&mut iter, "--cache-dir")?;
                set_unique(&mut cache_dir, path, "--cache-dir")?;
            }
            b"--start-ms" => {
                let ms = take_u64_arg(&mut iter, "--start-ms")?;
                set_unique(&mut start_ms, ms, "--start-ms")?;
            }
            b"--end-ms" => {
                let ms = take_u64_arg(&mut iter, "--end-ms")?;
                set_unique(&mut end_ms, ms, "--end-ms")?;
            }
            _ => {
                return Err(format!(
                    "Error: unrecognized argument: {}",
                    arg.to_string_lossy()
                ));
            }
        }
    }

    if help_requested {
        return Ok(CliAction::Help);
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

fn run_index_only(file: &Path, cache_dir: &Path) -> Result<(), String> {
    let (mut mp3_file, mp3_meta) = open_source_mp3(file)
        .map_err(|err| format!("Error indexing MP3 file {}: {err}", file.display()))?;
    let mp3_size = mp3_meta.len();
    let cache_path = mllt_cache_path_from_metadata(file, cache_dir, &mp3_meta).map_err(|err| {
        format!(
            "Error resolving .mllt cache path for {}: {err}",
            file.display()
        )
    })?;

    if let Some(data) = read_mllt_cache_safe(&cache_path)
        .map_err(|err| format!("Error reading .mllt cache {}: {err}", cache_path.display()))?
    {
        if let Ok(meta) = decode_mllt_cache(&data) {
            if is_metadata_valid_for_file(&meta, mp3_size) {
                return Ok(());
            }
        }
    }

    let meta = scan_mp3_frames_reader(&mut mp3_file, mp3_size)
        .map_err(|err| format!("Error indexing MP3 file {}: {err}", file.display()))?;
    let encoded = encode_mllt_cache(&meta)
        .map_err(|err| format!("Error encoding .mllt cache {}: {err}", cache_path.display()))?;
    write_mllt_cache_atomic(&cache_path, &encoded)
        .map_err(|err| format!("Error writing .mllt cache {}: {err}", cache_path.display()))
}

fn stream_chunks<F, W>(mut read_chunk: F, writer: &mut W) -> Result<(), String>
where
    F: FnMut(&mut [u8]) -> Result<usize, Mp3CutError>,
    W: Write,
{
    let mut buf = [0u8; 32768];

    loop {
        let n = match read_chunk(&mut buf) {
            Ok(bytes) => bytes,
            Err(Mp3CutError::Io(ref err)) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("Error reading MP3 slice: {err}")),
        };
        if n == 0 {
            break;
        }
        match writer.write_all(&buf[..n]) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                // Downstream reader closed pipe; terminate cleanly.
                return Ok(());
            }
            Err(err) => return Err(format!("Error writing audio chunk to stdout: {err}")),
        }
    }

    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == ErrorKind::BrokenPipe => return Ok(()),
            Err(err) => return Err(format!("Error flushing stdout: {err}")),
        }
    }
}

fn run(opts: CliOptions) -> ExitCode {
    let cache_ref = opts.cache_dir.as_deref();

    if opts.index_only {
        let Some(cache_dir) = cache_ref else {
            eprintln!("Error: --index-only requires --cache-dir");
            return ExitCode::FAILURE;
        };
        return match run_index_only(&opts.file, cache_dir) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err}");
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
    match stream_chunks(|buf| cutter.read_chunk(buf), &mut lock) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
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
    use super::run;
    use super::stream_chunks;
    use super::CliAction;
    use super::CliOptions;
    use lms_mp3cut::decode_mllt_cache;
    use lms_mp3cut::encode_mllt_cache;
    use lms_mp3cut::get_mllt_cache_path;
    use lms_mp3cut::FrameEntry;
    use lms_mp3cut::Mp3CutError;
    use lms_mp3cut::Mp3Metadata;
    use std::ffi::OsString;
    use std::io::ErrorKind;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::ExitCode;

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

    #[cfg(unix)]
    fn non_utf8_os_bytes(prefix: &[u8], suffix: &[u8]) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        let mut raw = Vec::with_capacity(prefix.len() + suffix.len());
        raw.extend_from_slice(prefix);
        raw.extend_from_slice(suffix);
        OsString::from_vec(raw)
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

    #[cfg(windows)]
    fn non_utf8_os_bytes(prefix: &[u8], suffix: &[u8]) -> OsString {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = prefix.iter().map(|&b| u16::from(b)).collect();
        wide.push(0xd800);
        wide.extend(suffix.iter().map(|&b| u16::from(b)));
        OsString::from_wide(&wide)
    }

    fn synthetic_mp3_bytes(frame_count: usize) -> Vec<u8> {
        // MPEG-1 Layer III, 128 kbps, 44100 Hz, Stereo => 417-byte frames.
        let mut frame = vec![0u8; 417];
        frame[0] = 0xFF;
        frame[1] = 0xFB;
        frame[2] = 0x90;
        frame[3] = 0x00;
        let mut out = Vec::with_capacity(417 * frame_count);
        for _ in 0..frame_count {
            out.extend_from_slice(&frame);
        }
        out
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
    fn reject_trailing_garbage_with_help() {
        assert!(parse_args(os_args(&["--help", "garbage"])).is_err());
        assert!(parse_args(os_args(&["-h", "--bogus"])).is_err());
        assert!(parse_args(os_args(&["--help", "--file"])).is_err());
        assert!(parse_args(os_args(&["--help", "--help"])).is_err());
        assert!(parse_args(os_args(&["-h", "--help"])).is_err());
    }

    #[test]
    fn reject_duplicate_flags() {
        assert!(parse_args(os_args(&["--file", "a.mp3", "--file", "b.mp3"])).is_err());
        assert!(parse_args(os_args(&["--file=a.mp3", "--file", "b.mp3"])).is_err());
        assert!(parse_args(os_args(&[
            "--file",
            "a.mp3",
            "--cache-dir",
            "/a",
            "--cache-dir=/b"
        ]))
        .is_err());
        assert!(parse_args(os_args(&[
            "--file",
            "a.mp3",
            "--start-ms",
            "10",
            "--start-ms=20"
        ]))
        .is_err());
        assert!(parse_args(os_args(&[
            "--file", "a.mp3", "--end-ms", "10", "--end-ms", "20"
        ]))
        .is_err());
        assert!(parse_args(os_args(&[
            "--file",
            "a.mp3",
            "--cache-dir",
            "/tmp",
            "--index-only",
            "--index-only"
        ]))
        .is_err());
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
        assert!(parse_args(os_args(&["--file="])).is_err());
        assert!(parse_args(os_args(&["--file=a.mp3", "--cache-dir="])).is_err());
        assert!(parse_args(os_args(&["--file=a.mp3", "--start-ms="])).is_err());
        assert!(parse_args(os_args(&["--file=a.mp3", "--end-ms=-5"])).is_err());
        assert!(parse_args(os_args(&["--help=1"])).is_err());
        assert!(parse_args(os_args(&["--index-only=1", "--file=a.mp3"])).is_err());
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
    fn parse_non_utf8_os_string_cache_dir_and_equals_syntax() {
        let raw_file = non_utf8_os_path();
        let raw_cache = non_utf8_os_bytes(b"/cache_", b"_dir");
        let eq_file = non_utf8_os_bytes(b"--file=/music/eq_", b".mp3");
        let eq_cache = non_utf8_os_bytes(b"--cache-dir=/cache/eq_", b"_dir");

        let space_args = vec![
            OsString::from("lms-mp3cut"),
            OsString::from("--file"),
            raw_file.clone(),
            OsString::from("--cache-dir"),
            raw_cache.clone(),
        ];
        assert_eq!(
            parse_args(space_args),
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from(raw_file),
                cache_dir: Some(PathBuf::from(raw_cache)),
                start_ms: None,
                end_ms: None,
                index_only: false,
            }))
        );

        let eq_args = vec![OsString::from("lms-mp3cut"), eq_file, eq_cache];
        let parsed_eq = parse_args(eq_args).expect("non-UTF-8 --flag=value should parse");
        match parsed_eq {
            CliAction::Run(opts) => {
                assert!(!opts.file.as_os_str().is_empty());
                assert!(opts.cache_dir.is_some());
            }
            CliAction::Help => panic!("expected Run outcome"),
        }
    }

    #[test]
    fn parse_equals_syntax_for_leading_dash_and_numeric_flags() {
        let args = os_args(&[
            "--file=-leading-dash.mp3",
            "--cache-dir=-cache-dir",
            "--start-ms=100",
            "--end-ms=250",
            "--index-only",
        ]);
        assert_eq!(
            parse_args(args),
            Ok(CliAction::Run(CliOptions {
                file: PathBuf::from("-leading-dash.mp3"),
                cache_dir: Some(PathBuf::from("-cache-dir")),
                start_ms: Some(100),
                end_ms: Some(250),
                index_only: true,
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

    #[test]
    fn stream_chunks_retries_eintr_and_handles_broken_pipe() {
        struct FlakyFlushWriter {
            buf: Vec<u8>,
            flush_interrupts_left: usize,
        }

        impl Write for FlakyFlushWriter {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                if self.flush_interrupts_left > 0 {
                    self.flush_interrupts_left -= 1;
                    return Err(std::io::Error::from(ErrorKind::Interrupted));
                }
                Ok(())
            }
        }

        let mut step = 0usize;
        let mut writer = FlakyFlushWriter {
            buf: Vec::new(),
            flush_interrupts_left: 2,
        };
        let res = stream_chunks(
            |out| {
                step += 1;
                match step {
                    1 => Err(Mp3CutError::Io(std::io::Error::from(
                        ErrorKind::Interrupted,
                    ))),
                    2 => {
                        out[..5].copy_from_slice(b"hello");
                        Ok(5)
                    }
                    3 => Err(Mp3CutError::Io(std::io::Error::from(
                        ErrorKind::Interrupted,
                    ))),
                    4 => {
                        out[..6].copy_from_slice(b" world");
                        Ok(6)
                    }
                    _ => Ok(0),
                }
            },
            &mut writer,
        );
        assert!(res.is_ok(), "EINTR reads and flushes should retry: {res:?}");
        assert_eq!(writer.buf, b"hello world");
        assert_eq!(writer.flush_interrupts_left, 0);

        struct BrokenPipeWriter;
        impl Write for BrokenPipeWriter {
            fn write(&mut self, _data: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(ErrorKind::BrokenPipe))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let bp_res = stream_chunks(
            |out| {
                out[0] = 0xAA;
                Ok(1)
            },
            &mut BrokenPipeWriter,
        );
        assert!(bp_res.is_ok(), "BrokenPipe should exit cleanly: {bp_res:?}");

        let mut sink = Vec::new();
        let err_res = stream_chunks(
            |_| Err(Mp3CutError::Io(std::io::Error::from(ErrorKind::Other))),
            &mut sink,
        );
        assert!(err_res.is_err());
    }

    #[test]
    fn stream_chunks_handles_large_stream_over_64kb() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mp3_path = dir.path().join("large_stream.mp3");
        let payload = synthetic_mp3_bytes(200);
        assert!(
            payload.len() > 64 * 1024,
            "payload must exceed 64 KB: {}",
            payload.len()
        );
        std::fs::write(&mp3_path, &payload).expect("write synthetic mp3");

        let mut cutter =
            lms_mp3cut::GaplessCutStream::open(&mp3_path, None, None, None).expect("open stream");
        let mut streamed = Vec::new();
        stream_chunks(|buf| cutter.read_chunk(buf), &mut streamed).expect("stream >64 KB payload");
        assert_eq!(streamed.len(), payload.len());
        assert_eq!(streamed, payload);
    }

    #[test]
    fn run_index_only_success_and_error_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mp3_path = dir.path().join("indexed.mp3");
        std::fs::write(&mp3_path, synthetic_mp3_bytes(25)).expect("write synthetic mp3");

        let good_cache_dir = dir.path().join("cache");
        std::fs::create_dir(&good_cache_dir).expect("create cache dir");

        let ok_exit = run(CliOptions {
            file: mp3_path.clone(),
            cache_dir: Some(good_cache_dir.clone()),
            start_ms: None,
            end_ms: None,
            index_only: true,
        });
        assert_eq!(ok_exit, ExitCode::SUCCESS);

        let cache_path = get_mllt_cache_path(&mp3_path, &good_cache_dir).expect("cache path");
        let cached_bytes = std::fs::read(&cache_path).expect("read .mllt cache");
        let decoded = decode_mllt_cache(&cached_bytes).expect("decode .mllt cache");
        assert_eq!(decoded.frames.len(), 25);

        // Cache dir pointing to a regular file cannot hold a `.mllt` entry.
        let file_as_cache_dir = dir.path().join("not_a_dir");
        std::fs::write(&file_as_cache_dir, b"blocking-file").expect("write blocking file");
        let bad_dir_exit = run(CliOptions {
            file: mp3_path.clone(),
            cache_dir: Some(file_as_cache_dir),
            start_ms: None,
            end_ms: None,
            index_only: true,
        });
        assert_eq!(bad_dir_exit, ExitCode::FAILURE);

        let missing_mp3_exit = run(CliOptions {
            file: dir.path().join("missing.mp3"),
            cache_dir: Some(good_cache_dir),
            start_ms: None,
            end_ms: None,
            index_only: true,
        });
        assert_eq!(missing_mp3_exit, ExitCode::FAILURE);
    }

    #[cfg(unix)]
    #[test]
    fn run_index_only_rejects_symlink_and_fifo_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mp3_path = dir.path().join("sec_test.mp3");
        std::fs::write(&mp3_path, synthetic_mp3_bytes(10)).expect("write synthetic mp3");

        let symlink_cache_dir = dir.path().join("sym_cache");
        std::fs::create_dir(&symlink_cache_dir).expect("create symlink cache dir");
        let target_path = dir.path().join("protected.txt");
        std::fs::write(&target_path, b"do_not_overwrite").expect("write target file");

        let sym_cache_path =
            get_mllt_cache_path(&mp3_path, &symlink_cache_dir).expect("symlink cache path");
        std::os::unix::fs::symlink(&target_path, &sym_cache_path).expect("create symlink");

        let sym_exit = run(CliOptions {
            file: mp3_path.clone(),
            cache_dir: Some(symlink_cache_dir),
            start_ms: None,
            end_ms: None,
            index_only: true,
        });
        assert_eq!(sym_exit, ExitCode::FAILURE);
        assert_eq!(
            std::fs::read(&target_path).expect("read protected file"),
            b"do_not_overwrite"
        );

        let fifo_cache_dir = dir.path().join("fifo_cache");
        std::fs::create_dir(&fifo_cache_dir).expect("create fifo cache dir");
        let fifo_cache_path =
            get_mllt_cache_path(&mp3_path, &fifo_cache_dir).expect("fifo cache path");
        let mkfifo_status = std::process::Command::new("mkfifo")
            .arg(&fifo_cache_path)
            .status()
            .expect("invoke mkfifo");
        assert!(mkfifo_status.success(), "mkfifo failed: {mkfifo_status}");

        let fifo_exit = run(CliOptions {
            file: mp3_path,
            cache_dir: Some(fifo_cache_dir),
            start_ms: None,
            end_ms: None,
            index_only: true,
        });
        assert_eq!(fifo_exit, ExitCode::FAILURE);
    }

    #[test]
    fn run_index_only_rebuilds_stale_out_of_range_cache_and_surfaces_write_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mp3_path = dir.path().join("stale_test.mp3");
        let mp3_bytes = synthetic_mp3_bytes(10);
        let mp3_size = mp3_bytes.len() as u64;
        std::fs::write(&mp3_path, &mp3_bytes).expect("write synthetic mp3");

        let cache_dir = dir.path().join("cache");
        std::fs::create_dir(&cache_dir).expect("create cache dir");
        let cache_path = get_mllt_cache_path(&mp3_path, &cache_dir).expect("cache path");

        let stale_meta = Mp3Metadata {
            sample_rate: 44100,
            samples_per_frame: 1152,
            channels: 2,
            enc_delay: 0,
            enc_padding: 0,
            has_xing: false,
            frames: vec![FrameEntry {
                offset: mp3_size + 5000,
                length: 417,
            }],
        };
        let stale_bytes = encode_mllt_cache(&stale_meta).expect("encode stale cache");
        std::fs::write(&cache_path, &stale_bytes).expect("write stale cache");

        // Writable cache dir heals the out-of-range cache.
        let healed_exit = run(CliOptions {
            file: mp3_path.clone(),
            cache_dir: Some(cache_dir.clone()),
            start_ms: None,
            end_ms: None,
            index_only: true,
        });
        assert_eq!(healed_exit, ExitCode::SUCCESS);
        let healed_bytes = std::fs::read(&cache_path).expect("read healed cache");
        let healed_meta = decode_mllt_cache(&healed_bytes).expect("decode healed cache");
        assert_eq!(healed_meta.frames.len(), 10);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            // Re-plant stale out-of-range cache and lock down cache_dir so rebuild fails.
            std::fs::write(&cache_path, &stale_bytes).expect("re-write stale cache");
            std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o555))
                .expect("chmod 0555");

            let write_blocked = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(cache_dir.join("write_probe"))
                .is_err();
            if write_blocked {
                let stale_ro_exit = run(CliOptions {
                    file: mp3_path.clone(),
                    cache_dir: Some(cache_dir.clone()),
                    start_ms: None,
                    end_ms: None,
                    index_only: true,
                });
                assert_eq!(stale_ro_exit, ExitCode::FAILURE);

                let empty_ro_dir = dir.path().join("ro_empty_cache");
                std::fs::create_dir(&empty_ro_dir).expect("create ro empty cache dir");
                std::fs::set_permissions(&empty_ro_dir, std::fs::Permissions::from_mode(0o555))
                    .expect("chmod empty ro dir");
                let empty_ro_exit = run(CliOptions {
                    file: mp3_path,
                    cache_dir: Some(empty_ro_dir.clone()),
                    start_ms: None,
                    end_ms: None,
                    index_only: true,
                });
                assert_eq!(empty_ro_exit, ExitCode::FAILURE);
                std::fs::set_permissions(&empty_ro_dir, std::fs::Permissions::from_mode(0o755))
                    .expect("restore empty ro dir perms");
            }
            std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o755))
                .expect("restore cache dir perms");
        }
    }
}
