#![deny(unsafe_op_in_unsafe_fn)]

use std::env;
use std::path::Path;
use lms_audioscan::{find_frame_offset, scan_file};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args[1..].iter().any(|a| a == "--help" || a == "-h" || a == "help") {
        println!("Usage: lms-audioscan <subcommand> [args...]");
        println!("Subcommands: scan <path>, scan-tags <path>, find-frame <format> <path> <offset_or_ms>");
        println!("Options: -h, --help  Display this help message");
        if args.len() < 2 {
            std::process::exit(1);
        } else {
            return;
        }
    }

    let no_artwork = env::var("AUDIO_SCAN_NO_ARTWORK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    match args[1].as_str() {
        "scan" => {
            if args.len() < 3 {
                eprintln!("Usage: lms-audioscan scan <path>");
                std::process::exit(1);
            }
            match scan_file(Path::new(&args[2]), true, true, no_artwork) {
                Ok(res) => match serde_json::to_string(&res) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("JSON serialization error: {e}");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("Scan error: {e}");
                    std::process::exit(1);
                }
            }
        }
        "scan-tags" => {
            if args.len() < 3 {
                eprintln!("Usage: lms-audioscan scan-tags <path>");
                std::process::exit(1);
            }
            match scan_file(Path::new(&args[2]), false, true, no_artwork) {
                Ok(res) => match serde_json::to_string(&res) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("JSON serialization error: {e}");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("Scan error: {e}");
                    std::process::exit(1);
                }
            }
        }
        "find-frame" => {
            if args.len() < 5 {
                eprintln!("Usage: lms-audioscan find-frame <format> <path> <offset_or_ms>");
                std::process::exit(1);
            }
            let format = &args[2];
            let path = Path::new(&args[3]);
            let target_offset = match args[4].parse::<i64>() {
                Ok(val) => val,
                Err(e) => {
                    eprintln!("Invalid offset_or_ms '{}': {e}", args[4]);
                    std::process::exit(1);
                }
            };
            match find_frame_offset(path, Some(format), target_offset) {
                Ok(res) => match serde_json::to_string(&res) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("JSON serialization error: {e}");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("Find frame error: {e}");
                    std::process::exit(1);
                }
            }
        }
        cmd => {
            eprintln!("Unknown subcommand: {cmd}");
            std::process::exit(1);
        }
    }
}
