#![deny(unsafe_op_in_unsafe_fn)]

use lms_mp3cut::GaplessCutStream;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        "Usage: lms-mp3cut --file <path> [--cache-dir <dir>] [--start-ms <ms>] [--end-ms <ms>] [--index-only]"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut file: Option<PathBuf> = None;
    let mut cache_dir: Option<PathBuf> = None;
    let mut start_ms: Option<u64> = None;
    let mut end_ms: Option<u64> = None;
    let mut index_only = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--file" => {
                i += 1;
                if i < args.len() {
                    file = Some(PathBuf::from(&args[i]));
                } else {
                    eprintln!("Error: --file requires an argument");
                    print_usage();
                    return ExitCode::FAILURE;
                }
            }
            "--cache-dir" => {
                i += 1;
                if i < args.len() {
                    cache_dir = Some(PathBuf::from(&args[i]));
                } else {
                    eprintln!("Error: --cache-dir requires an argument");
                    print_usage();
                    return ExitCode::FAILURE;
                }
            }
            "--start-ms" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<u64>() {
                        Ok(v) => start_ms = Some(v),
                        Err(_) => {
                            eprintln!("Error: invalid start-ms: {}", args[i]);
                            return ExitCode::FAILURE;
                        }
                    }
                } else {
                    eprintln!("Error: --start-ms requires an argument");
                    print_usage();
                    return ExitCode::FAILURE;
                }
            }
            "--end-ms" => {
                i += 1;
                if i < args.len() {
                    match args[i].parse::<u64>() {
                        Ok(v) => end_ms = Some(v),
                        Err(_) => {
                            eprintln!("Error: invalid end-ms: {}", args[i]);
                            return ExitCode::FAILURE;
                        }
                    }
                } else {
                    eprintln!("Error: --end-ms requires an argument");
                    print_usage();
                    return ExitCode::FAILURE;
                }
            }
            "--index-only" => {
                index_only = true;
            }
            "--help" | "-h" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("Error: unrecognized argument: {other}");
                print_usage();
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    let file_path = match file {
        Some(f) => f,
        None => {
            eprintln!("Error: missing required --file argument");
            print_usage();
            return ExitCode::FAILURE;
        }
    };

    let cache_ref = cache_dir.as_deref();

    if index_only {
        // Pre-scan and generate cache without streaming
        match GaplessCutStream::open(Path::new(&file_path), cache_ref, None, None) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error indexing MP3 file {}: {e}", file_path.display());
                ExitCode::FAILURE
            }
        }
    } else {
        let mut cutter = match GaplessCutStream::open(Path::new(&file_path), cache_ref, start_ms, end_ms) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error opening MP3 cut stream for {}: {e}", file_path.display());
                return ExitCode::FAILURE;
            }
        };

        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let mut buf = [0u8; 32768];

        loop {
            let n = match cutter.read_chunk(&mut buf) {
                Ok(bytes) => bytes,
                Err(e) => {
                    eprintln!("Error reading MP3 slice: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if n == 0 {
                break;
            }
            match lock.write_all(&buf[..n]) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    // Downstream reader closed pipe; clean termination
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Error writing audio chunk to stdout: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }

        let _ = lock.flush();
        ExitCode::SUCCESS
    }
}
