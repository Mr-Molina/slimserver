#![deny(unsafe_op_in_unsafe_fn)]

use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum AccelError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("Cache error: {0}")]
    Cache(#[from] lms_cache::CacheError),
    #[error("Image error: {0}")]
    Image(#[from] lms_image::ImageError),
    #[error("Walk error: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("Argument error: {0}")]
    Arg(String),
}

fn percent_encode_path(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        if b == b'/'
            || b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\''
                    | b'(' | b')' | b':' | b'@' | b'&' | b'='
                    | b'+' | b'$' | b',' | b'[' | b']'
            )
        {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{:02X}", b);
        }
    }
    out
}

fn path_to_file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let canon_buf = if path.is_absolute() {
        None
    } else {
        std::fs::canonicalize(path).ok()
    };
    let effective_path = canon_buf.as_deref().unwrap_or(path);
    let target_bytes = effective_path.as_os_str().as_bytes();
    let stripped = if target_bytes.starts_with(b"file://") {
        &target_bytes[7..]
    } else {
        target_bytes
    };
    let encoded = percent_encode_path(stripped);
    if encoded.starts_with('/') {
        format!("file://{}", encoded)
    } else {
        format!("file:///{}", encoded)
    }
}

pub fn populate_scanned_files(db_path: &Path, basedir: &Path) -> Result<usize, AccelError> {
    if !basedir.exists() {
        return Err(AccelError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("directory not found: {}", basedir.display()),
        )));
    }

    let mut count = 0usize;
    let mut conn = rusqlite::Connection::open(db_path)?;
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO scanned_files (url, timestamp, filesize) VALUES (?1, ?2, ?3)",
        )?;

        for entry_res in walkdir::WalkDir::new(basedir).follow_links(true) {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };

            let url = path_to_file_url(entry.path());
            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let filesize = if metadata.is_dir() { 0i64 } else { metadata.len() as i64 };

            stmt.execute(rusqlite::params![url, mtime, filesize])?;
            count += 1;
        }
    }
    tx.commit()?;
    Ok(count)
}

pub fn precache_artwork_batch(
    cache_root: &Path,
    jobs: &[(PathBuf, String)],
    specs: &[String],
) -> Result<usize, AccelError> {
    if jobs.is_empty() {
        return Ok(0);
    }
    if specs.is_empty() {
        return Err(AccelError::Arg("No specs provided".to_string()));
    }
    let store = lms_cache::DbArtworkStore::open(cache_root, false)?;

    use rayon::prelude::*;
    let success_count: usize = jobs
        .par_iter()
        .map(|(img_path, cachekey)| {
            match lms_image::gdresize_to_cache(
                lms_image::InputSource::Path(img_path),
                specs,
                &store,
                cachekey,
                true,
            ) {
                Ok(_) => 1,
                Err(err) => {
                    eprintln!("Failed to resize {}: {}", img_path.display(), err);
                    0
                }
            }
        })
        .sum();

    Ok(success_count)
}

fn print_usage() {
    eprintln!("Usage: lms-scanner-accel <command> [options]");
    eprintln!("Commands:");
    eprintln!("  scan-dir --db <db_path> --dir <basedir>");
    eprintln!("  precache-art --cache-root <dir> --spec <comma_specs> <file::cachekey>...");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "scan-dir" => {
            let mut db_path = None;
            let mut basedir = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--db" => {
                        if i + 1 < args.len() {
                            db_path = Some(PathBuf::from(&args[i + 1]));
                            i += 2;
                        } else {
                            eprintln!("Error: Missing value for --db");
                            std::process::exit(1);
                        }
                    }
                    "--dir" => {
                        if i + 1 < args.len() {
                            basedir = Some(PathBuf::from(&args[i + 1]));
                            i += 2;
                        } else {
                            eprintln!("Error: Missing value for --dir");
                            std::process::exit(1);
                        }
                    }
                    other => {
                        eprintln!("Error: Unknown argument for scan-dir: {}", other);
                        std::process::exit(1);
                    }
                }
            }
            let db = match db_path {
                Some(p) => p,
                None => {
                    eprintln!("Error: Missing required --db argument");
                    std::process::exit(1);
                }
            };
            let dir = match basedir {
                Some(p) => p,
                None => {
                    eprintln!("Error: Missing required --dir argument");
                    std::process::exit(1);
                }
            };
            match populate_scanned_files(&db, &dir) {
                Ok(count) => {
                    println!("Populated {} scanned files", count);
                }
                Err(e) => {
                    eprintln!("Error populating scanned files: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "precache-art" => {
            let mut cache_root = None;
            let mut specs = Vec::new();
            let mut jobs = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--cache-root" => {
                        if i + 1 < args.len() {
                            cache_root = Some(PathBuf::from(&args[i + 1]));
                            i += 2;
                        } else {
                            eprintln!("Error: Missing value for --cache-root");
                            std::process::exit(1);
                        }
                    }
                    "--spec" => {
                        if i + 1 < args.len() {
                            for s in args[i + 1].split(',') {
                                let trimmed = s.trim();
                                if !trimmed.is_empty() {
                                    specs.push(trimmed.to_string());
                                }
                            }
                            i += 2;
                        } else {
                            eprintln!("Error: Missing value for --spec");
                            std::process::exit(1);
                        }
                    }
                    arg if !arg.starts_with('-') => {
                        if let Some((f, k)) = arg.split_once("::") {
                            jobs.push((PathBuf::from(f), k.to_string()));
                        } else {
                            jobs.push((PathBuf::from(arg), arg.to_string()));
                        }
                        i += 1;
                    }
                    other => {
                        eprintln!("Error: Unknown argument for precache-art: {}", other);
                        std::process::exit(1);
                    }
                }
            }
            let root = match cache_root {
                Some(p) => p,
                None => {
                    eprintln!("Error: Missing required --cache-root argument");
                    std::process::exit(1);
                }
            };
            match precache_artwork_batch(&root, &jobs, &specs) {
                Ok(count) => {
                    println!("Precached {} artwork jobs", count);
                }
                Err(e) => {
                    eprintln!("Error precaching artwork: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "-h" | "--help" | "help" => {
            print_usage();
        }
        cmd => {
            eprintln!("Error: Unknown command: {}", cmd);
            print_usage();
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat as ImgFmt, RgbImage};
    use std::io::Cursor;

    #[test]
    fn test_scan_dir_populates_scanned_files_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();
        std::fs::write(music_dir.join("song1.mp3"), vec![0u8; 1024]).unwrap();
        std::fs::write(music_dir.join("track 03.mp3"), vec![0u8; 2048]).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE scanned_files (url text COLLATE NOCASE, timestamp int(10), filesize int(10))", []).unwrap();
        drop(conn);

        let count = populate_scanned_files(&db_path, &music_dir).unwrap();
        assert!(count >= 3);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT url FROM scanned_files WHERE url LIKE '%track%2003.mp3'")
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        assert!(
            rows.next().unwrap().is_some(),
            "scanned_files.url must contain percent-encoded track%2003.mp3"
        );
    }

    #[test]
    fn test_precache_artwork_batch_populates_artwork_db() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("cover.jpg");
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(120, 120, |_, _| image::Rgb([40, 80, 160])));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg).unwrap();
        std::fs::write(&img_path, buf).unwrap();

        let jobs = vec![(img_path, "music/cover01/cover_".to_string())];
        let specs = vec!["100x100_o".to_string(), "50x50_m".to_string()];
        let count = precache_artwork_batch(dir.path(), &jobs, &specs).unwrap();
        assert_eq!(count, 1);

        let store = lms_cache::DbArtworkStore::open(dir.path(), false).unwrap();
        assert!(store.get("music/cover01/cover_100x100_o").unwrap().is_some());
        assert!(store.get("music/cover01/cover_50x50_m").unwrap().is_some());
    }
}
