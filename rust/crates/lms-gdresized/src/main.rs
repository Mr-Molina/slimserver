#![deny(unsafe_op_in_unsafe_fn)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

#[derive(Debug, Default)]
struct CliOptions {
    socket_path: Option<String>,
    faster: bool,
    max_requests: Option<usize>,
    cli_resize: bool,
    spec: Option<String>,
    file: Option<String>,
    stdin: bool,
}

fn parse_cli_args<I: IntoIterator<Item = String>>(args: I) -> CliOptions {
    let mut opts = CliOptions::default();
    let mut iter = args.into_iter().peekable();
    let _ = iter.next(); // skip argv[0]

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--socket" => {
                if let Some(val) = iter.next() {
                    opts.socket_path = Some(val);
                }
            }
            "--faster" => {
                opts.faster = true;
            }
            "--max-requests" => {
                if let Some(val) = iter.next() {
                    if let Ok(n) = val.parse::<usize>() {
                        opts.max_requests = Some(n);
                    }
                }
            }
            "--cli-resize" => {
                opts.cli_resize = true;
            }
            "--spec" => {
                if let Some(val) = iter.next() {
                    opts.spec = Some(val);
                }
            }
            "--file" => {
                if let Some(val) = iter.next() {
                    opts.file = Some(val);
                }
            }
            "--stdin" => {
                opts.stdin = true;
            }
            _ => {
                // Ignore unknown legacy flags like --debug, d_startup, etc.
            }
        }
    }
    opts
}

pub fn resize_one_shot(
    input: &[u8],
    spec: &str,
    faster: bool,
) -> Result<(String, Vec<u8>), Box<dyn std::error::Error>> {
    let parsed_spec = lms_image::parse_single_spec(spec);
    let (bytes, format) = lms_image::resize_image(input, &parsed_spec, faster)?;
    Ok((format.as_str().to_string(), bytes))
}

fn process_client_request(
    stream: &mut UnixStream,
    faster: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream);
    let mut raw_line = Vec::new();
    reader.read_until(b'\n', &mut raw_line)?;

    if raw_line.is_empty() {
        return Err("empty frame".into());
    }

    let mut raw = raw_line.as_slice();
    if raw.ends_with(b"\r\n") {
        raw = &raw[..raw.len() - 2];
    } else if raw.ends_with(b"\n") {
        raw = &raw[..raw.len() - 1];
    }

    let mut null_indices = Vec::with_capacity(5);
    for (i, &b) in raw.iter().enumerate() {
        if b == 0 {
            null_indices.push(i);
            if null_indices.len() == 5 {
                break;
            }
        }
    }

    if null_indices.len() < 5 {
        return Err("malformed frame: fewer than 5 null-terminated fields".into());
    }

    let file_bytes = &raw[..null_indices[0]];
    let spec_bytes = &raw[null_indices[0] + 1..null_indices[1]];
    let cacheroot_bytes = &raw[null_indices[1] + 1..null_indices[2]];
    let cachekey_bytes = &raw[null_indices[2] + 1..null_indices[3]];
    let data_bytes = &raw[null_indices[3] + 1..null_indices[4]];

    let file = std::str::from_utf8(file_bytes)
        .map_err(|e| format!("invalid UTF-8 in file path: {e}"))?;
    let mut spec = std::str::from_utf8(spec_bytes)
        .map_err(|e| format!("invalid UTF-8 in spec: {e}"))?;
    let cacheroot = std::str::from_utf8(cacheroot_bytes)
        .map_err(|e| format!("invalid UTF-8 in cacheroot: {e}"))?;
    let cachekey = std::str::from_utf8(cachekey_bytes)
        .map_err(|e| format!("invalid UTF-8 in cachekey: {e}"))?;
    let data_str = std::str::from_utf8(data_bytes)
        .map_err(|e| format!("invalid UTF-8 in data: {e}"))?;

    if spec.is_empty() {
        spec = "XxX";
    }

    if file.is_empty() || cacheroot.is_empty() || cachekey.is_empty() {
        return Err("missing required parameters".into());
    }

    let decoded_bytes;
    let input_source = if file == "data" {
        use base64::Engine;
        decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(data_str.trim().as_bytes())
            .map_err(|e| format!("invalid base64 data: {e}"))?;
        lms_image::InputSource::Bytes(&decoded_bytes)
    } else {
        let p = Path::new(file);
        if !p.exists() {
            return Err(format!("file not found: {file}").into());
        }
        lms_image::InputSource::Path(p)
    };

    let mut specs: Vec<String> = spec
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if specs.is_empty() {
        specs.push("XxX".to_string());
    }

    let is_imageproxy = cachekey.starts_with("imageproxy");
    let store = lms_cache::DbArtworkStore::open(Path::new(cacheroot), is_imageproxy)?;

    lms_image::gdresize_to_cache(input_source, &specs, &store, cachekey, faster)?;

    Ok(())
}

fn handle_client_connection(mut stream: UnixStream, faster: bool) {
    let res = process_client_request(&mut stream, faster);
    let response = match res {
        Ok(()) => "OK\r\n".to_string(),
        Err(e) => format!("Error: {e}\r\n"),
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

pub fn run_server(
    sock_path: &Path,
    faster: bool,
    max_requests: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }

    let listener = UnixListener::bind(sock_path)?;
    let mut handled = 0;

    while match max_requests {
        Some(max) => handled < max,
        None => true,
    } {
        let (stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        };
        handled += 1;
        handle_client_connection(stream, faster);
    }

    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }

    Ok(())
}

pub fn run_server_n_requests(
    sock_path: &Path,
    faster: bool,
    n: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    run_server(sock_path, faster, Some(n))
}

fn main() {
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && raw_args[1..].iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: gdresized [options]");
        println!("Options:");
        println!("  --socket <path>       UNIX domain socket path (default: /tmp/sbs_artwork)");
        println!("  --faster              Use faster resizing algorithm");
        println!("  --max-requests <num>  Exit after processing num requests");
        println!("  --cli-resize          One-shot CLI resize mode");
        println!("  --spec <spec>         Resize specification");
        println!("  --file <path>         Input image file");
        println!("  --stdin               Read input image from stdin");
        println!("  -h, --help            Display this help message");
        return;
    }
    let opts = parse_cli_args(raw_args);

    if opts.cli_resize {
        let spec = opts.spec.as_deref().unwrap_or("XxX");
        let image_bytes = if let Some(ref path) = opts.file {
            match std::fs::read(path) {
                Ok(raw) => {
                    if lms_image::sniff_format(&raw).is_some() {
                        raw
                    } else {
                        lms_image::extract_embedded_artwork(Path::new(path)).unwrap_or(raw)
                    }
                }
                Err(e) => {
                    eprintln!("Error reading file: {e}");
                    std::process::exit(1);
                }
            }
        } else if opts.stdin {
            use std::io::Read;
            let mut buf = Vec::new();
            if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
                eprintln!("Error reading stdin: {e}");
                std::process::exit(1);
            }
            buf
        } else {
            eprintln!("Error: --cli-resize requires either --file <path> or --stdin");
            std::process::exit(1);
        };

        match resize_one_shot(&image_bytes, spec, opts.faster) {
            Ok((fmt, bytes)) => {
                let mut stdout = std::io::stdout();
                if let Err(e) = writeln!(stdout, "{fmt}") {
                    eprintln!("Error writing stdout: {e}");
                    std::process::exit(1);
                }
                if let Err(e) = stdout.write_all(&bytes) {
                    eprintln!("Error writing stdout: {e}");
                    std::process::exit(1);
                }
                let _ = stdout.flush();
            }
            Err(e) => {
                eprintln!("Error resizing image: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let socket_path = opts
        .socket_path
        .unwrap_or_else(|| "/tmp/sbs_artwork".to_string());
    let path = Path::new(&socket_path);

    if let Err(e) = run_server(path, opts.faster, opts.max_requests) {
        eprintln!("Daemon error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat as ImgFmt, RgbImage};
    use std::io::{Cursor, Read, Write};
    use std::os::unix::net::UnixStream;

    fn make_test_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |_, _| image::Rgb([180, 90, 45])));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg).unwrap();
        buf
    }

    #[test]
    fn test_socket_malformed_frame_and_invalid_base64_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("sbs_artwork_test");
        let server_sock = sock_path.clone();
        let handle = std::thread::spawn(move || {
            run_server_n_requests(&server_sock, false, 2).unwrap();
        });
        let mut ready = false;
        for _ in 0..100 {
            if sock_path.exists() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready, "server socket did not appear");

        {
            let mut stream = UnixStream::connect(&sock_path).unwrap();
            stream.write_all(b"data\x00100x100_o\0/tmp\0key1\0!!!NOT_BASE64!!!\0\r\n").unwrap();
            let mut resp = String::new();
            stream.read_to_string(&mut resp).unwrap();
            assert!(resp.starts_with("Error: "));
        }

        {
            let jpg = make_test_jpeg_bytes(80, 80);
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &jpg);
            let cacheroot = dir.path().to_str().unwrap();
            let frame = format!("data\040x40_o\0{}\0music/1/cover_40x40_o\0{}\0\r\n", cacheroot, b64);
            let mut stream = UnixStream::connect(&sock_path).unwrap();
            stream.write_all(frame.as_bytes()).unwrap();
            let mut resp = String::new();
            stream.read_to_string(&mut resp).unwrap();
            assert_eq!(resp, "OK\r\n");
        }
        handle.join().unwrap();
    }

    #[test]
    fn test_cli_resize_one_shot() {
        let jpg = make_test_jpeg_bytes(80, 80);
        let (fmt, data) = resize_one_shot(&jpg, "40x40_o", false).unwrap();
        assert_eq!(fmt, "jpg");
        assert!(!data.is_empty());
    }

    #[test]
    fn test_parse_cli_args() {
        let args = vec![
            "gdresized".to_string(),
            "--socket".to_string(),
            "/tmp/test.sock".to_string(),
            "--faster".to_string(),
            "--max-requests".to_string(),
            "42".to_string(),
            "--debug".to_string(),
            "-d_startup".to_string(),
        ];
        let opts = parse_cli_args(args);
        assert_eq!(opts.socket_path.as_deref(), Some("/tmp/test.sock"));
        assert!(opts.faster);
        assert_eq!(opts.max_requests, Some(42));
    }

    #[test]
    fn test_socket_file_input_and_artwork_store_verification() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("sbs_artwork_file_test");
        let server_sock = sock_path.clone();

        let jpg_path = dir.path().join("test_cover.jpg");
        let jpg_bytes = make_test_jpeg_bytes(100, 100);
        std::fs::write(&jpg_path, &jpg_bytes).unwrap();

        let handle = std::thread::spawn(move || {
            run_server_n_requests(&server_sock, false, 1).unwrap();
        });
        let mut ready = false;
        for _ in 0..100 {
            if sock_path.exists() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready, "server socket did not appear");

        let cacheroot = dir.path().to_str().unwrap();
        let frame = format!(
            "{}\050x50_m\0{}\0music/99/cover\0\0\r\n",
            jpg_path.to_str().unwrap(),
            cacheroot
        );
        let mut stream = UnixStream::connect(&sock_path).unwrap();
        stream.write_all(frame.as_bytes()).unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        assert_eq!(resp, "OK\r\n");
        handle.join().unwrap();

        // Verify SQLite store contains the resized image
        let store = lms_cache::DbArtworkStore::open(dir.path(), false).unwrap();
        let record = store.get("music/99/cover").unwrap().expect("record must exist");
        assert_eq!(record.content_type, "jpg");
        assert!(!record.data.is_empty());
    }
}
