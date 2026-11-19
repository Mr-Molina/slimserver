#![deny(unsafe_op_in_unsafe_fn)]

use rusqlite::Connection;
use std::borrow::Cow;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const DEFAULT_AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "alac", "ape", "dff", "dsf", "fla", "flac", "flc", "l16", "l24", "lpcm",
    "m4a", "m4b", "mp+", "mp2", "mp3", "mp4", "mpc", "oga", "ogg", "ogf", "opus", "pcm", "spx",
    "wav", "wave", "wma", "wv",
];

const NAS_IGNORED_DIRS: &[&str] = &[
    "@eaDir",
    "#recycle",
    "#snapshot",
    ".AppleDouble",
    "lost+found",
];

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

impl From<Box<dyn std::error::Error>> for AccelError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        match err.downcast::<rusqlite::Error>() {
            Ok(db) => AccelError::Db(*db),
            Err(e) => AccelError::Arg(e.to_string()),
        }
    }
}

/// Normalizes `.` and `..` path components lexically without resolving
/// symlinks.
fn normalize_lexical_path(path: &Path) -> PathBuf {
    let abs: Cow<'_, Path> = if path.is_absolute() {
        Cow::Borrowed(path)
    } else if let Ok(cwd) = std::env::current_dir() {
        Cow::Owned(cwd.join(path))
    } else {
        Cow::Borrowed(path)
    };

    let mut normalized = PathBuf::new();
    let mut normal_depth = 0usize;

    for comp in abs.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normal_depth > 0 {
                    normalized.pop();
                    normal_depth -= 1;
                } else if !normalized.has_root() {
                    normalized.push(Component::ParentDir.as_os_str());
                }
            }
            Component::Normal(c) => {
                normalized.push(c);
                normal_depth += 1;
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn percent_encode_path(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        if b == b'/'
            || b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_'
                    | b'.'
                    | b'!'
                    | b'~'
                    | b'*'
                    | b'('
                    | b')'
                    | b':'
                    | b'@'
                    | b'&'
                    | b'='
                    | b'+'
                    | b'$'
                    | b','
                    | b'['
                    | b']'
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

#[cfg(unix)]
fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    Cow::Owned(normalized.into_bytes())
}

fn path_to_file_url(path: &Path) -> String {
    let normalized = normalize_lexical_path(path);
    let raw_bytes = path_bytes(&normalized);
    let stripped = raw_bytes.strip_prefix(b"file://").unwrap_or(&raw_bytes);
    let encoded = percent_encode_path(stripped);
    if encoded.starts_with('/') {
        format!("file://{}", encoded)
    } else {
        format!("file:///{}", encoded)
    }
}

fn is_ignored_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') {
        return true;
    }
    NAS_IGNORED_DIRS.iter().any(|&ignored| name == ignored)
}

fn is_temp_playlist_file(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let lower = name.to_string_lossy().to_ascii_lowercase();
    lower.starts_with("__") && (lower.ends_with(".m3u") || lower.ends_with(".m3u8"))
}

fn is_supported_extension(path: &Path, allowed_exts: &[String]) -> bool {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    allowed_exts.iter().any(|allowed| allowed == &ext)
}

fn parse_extensions_csv(csv: &str) -> Result<Vec<String>, AccelError> {
    let exts: Vec<String> = csv
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if exts.is_empty() {
        return Err(AccelError::Arg(
            "No valid extensions provided for --extensions".to_string(),
        ));
    }
    Ok(exts)
}

fn has_url_index(conn: &Connection) -> Result<bool, AccelError> {
    let mut list_stmt = conn.prepare("PRAGMA index_list('scanned_files')")?;
    let index_names: Vec<String> = list_stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    for idx_name in index_names {
        let escaped = idx_name.replace('"', "\"\"");
        let pragma_sql = format!("PRAGMA index_info(\"{}\")", escaped);
        let mut info_stmt = conn.prepare(&pragma_sql)?;
        let cols: Vec<(i64, Option<String>)> = info_stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if let Some((0, Some(col_name))) = cols.first() {
            if col_name.eq_ignore_ascii_case("url") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn flush_batch(
    conn: &mut Connection,
    batch: &mut Vec<(String, i64, i64)>,
) -> Result<(), AccelError> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO scanned_files (url, timestamp, filesize) VALUES (?1, ?2, ?3)",
        )?;
        for (url, mtime, filesize) in batch.iter() {
            stmt.execute(rusqlite::params![url, mtime, filesize])?;
        }
    }
    tx.commit()?;
    batch.clear();
    Ok(())
}

/// Scans `basedir` and populates `scanned_files` in `db_path` using default LMS
/// audio extensions.
pub fn populate_scanned_files(db_path: &Path, basedir: &Path) -> Result<usize, AccelError> {
    populate_scanned_files_with_extensions(db_path, basedir, None)
}

/// Scans `basedir` and populates `scanned_files` in `db_path` with an optional
/// extension allow-list.
pub fn populate_scanned_files_with_extensions(
    db_path: &Path,
    basedir: &Path,
    extensions: Option<&[String]>,
) -> Result<usize, AccelError> {
    if !basedir.exists() {
        return Err(AccelError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("directory not found: {}", basedir.display()),
        )));
    }

    let allowed_exts: Vec<String> = match extensions {
        Some(custom) => {
            if custom.is_empty() {
                return Err(AccelError::Arg(
                    "Extension allow-list must not be empty".to_string(),
                ));
            }
            custom
                .iter()
                .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        }
        None => DEFAULT_AUDIO_EXTENSIONS
            .iter()
            .map(|&s| s.to_string())
            .collect(),
    };
    if allowed_exts.is_empty() {
        return Err(AccelError::Arg(
            "Extension allow-list must not be empty".to_string(),
        ));
    }

    let mut conn = Connection::open(db_path)?;
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000; \
         PRAGMA journal_mode = WAL; \
         PRAGMA synchronous = NORMAL; \
         PRAGMA journal_size_limit = 16777216; \
         PRAGMA cache_size = -64000; \
         PRAGMA temp_store = MEMORY;",
    )?;

    if !has_url_index(&conn)? {
        conn.execute(
            "CREATE INDEX IF NOT EXISTS scannedUrlIndex ON scanned_files (url)",
            [],
        )?;
    }

    let mut batch: Vec<(String, i64, i64)> = Vec::with_capacity(5_000);
    let walk_root = normalize_lexical_path(basedir);

    let walker = walkdir::WalkDir::new(&walk_root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| !is_ignored_entry(entry));

    for entry_res in walker {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                eprintln!("Warning: walkdir error: {}", err);
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        if is_temp_playlist_file(entry.path()) {
            continue;
        }

        if !is_supported_extension(entry.path(), &allowed_exts) {
            continue;
        }

        if std::fs::File::open(entry.path()).is_err() {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                eprintln!(
                    "Warning: metadata error for {}: {}",
                    entry.path().display(),
                    err
                );
                continue;
            }
        };

        let url = path_to_file_url(entry.path());
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let filesize = metadata.len() as i64;

        batch.push((url, mtime, filesize));
    }

    let count = batch.len();
    flush_batch(&mut conn, &mut batch)?;
    Ok(count)
}

/// Resizes and caches artwork jobs in parallel, propagating fatal cache errors
/// and all-failed batches.
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

    use rayon::iter::IntoParallelRefIterator;
    use rayon::iter::ParallelIterator;
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
                Ok(_) => Ok(1usize),
                Err(lms_image::ImageError::Cache(cache_err)) => Err(AccelError::Cache(cache_err)),
                Err(err) => {
                    eprintln!("Failed to resize {}: {}", img_path.display(), err);
                    Ok(0usize)
                }
            }
        })
        .try_reduce(|| 0usize, |a, b| Ok(a + b))?;

    if success_count == 0 {
        return Err(AccelError::Arg(format!(
            "All {} artwork precache jobs failed",
            jobs.len()
        )));
    }

    Ok(success_count)
}

#[derive(Debug, PartialEq)]
enum CliCommand {
    Help,
    ScanDir {
        db_path: PathBuf,
        basedir: PathBuf,
        extensions: Option<Vec<String>>,
    },
    PrecacheArt {
        cache_root: PathBuf,
        specs: Vec<String>,
        jobs: Vec<(PathBuf, String)>,
    },
}

fn parse_job_arg(arg: &str) -> (PathBuf, String) {
    if let Some((f, k)) = arg.split_once("::") {
        (PathBuf::from(f), k.to_string())
    } else {
        (PathBuf::from(arg), arg.to_string())
    }
}

fn parse_scan_dir_args<I>(mut args: I) -> Result<CliCommand, AccelError>
where
    I: Iterator<Item = String>,
{
    let mut db_path = None;
    let mut basedir = None;
    let mut extensions = None;
    let mut after_double_dash = false;

    while let Some(arg) = args.next() {
        if after_double_dash {
            return Err(AccelError::Arg(format!(
                "Unexpected positional argument for scan-dir: {}",
                arg
            )));
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliCommand::Help),
            "--" => {
                after_double_dash = true;
            }
            "--db" => {
                let val = args
                    .next()
                    .ok_or_else(|| AccelError::Arg("Missing value for --db".to_string()))?;
                db_path = Some(PathBuf::from(val));
            }
            "--dir" => {
                let val = args
                    .next()
                    .ok_or_else(|| AccelError::Arg("Missing value for --dir".to_string()))?;
                basedir = Some(PathBuf::from(val));
            }
            "--extensions" => {
                let val = args
                    .next()
                    .ok_or_else(|| AccelError::Arg("Missing value for --extensions".to_string()))?;
                extensions = Some(parse_extensions_csv(&val)?);
            }
            other => {
                return Err(AccelError::Arg(format!(
                    "Unknown argument for scan-dir: {}",
                    other
                )));
            }
        }
    }

    let db =
        db_path.ok_or_else(|| AccelError::Arg("Missing required --db argument".to_string()))?;
    let dir =
        basedir.ok_or_else(|| AccelError::Arg("Missing required --dir argument".to_string()))?;

    Ok(CliCommand::ScanDir {
        db_path: db,
        basedir: dir,
        extensions,
    })
}

fn parse_precache_art_args<I>(mut args: I) -> Result<CliCommand, AccelError>
where
    I: Iterator<Item = String>,
{
    let mut cache_root = None;
    let mut specs = Vec::new();
    let mut jobs = Vec::new();
    let mut after_double_dash = false;

    while let Some(arg) = args.next() {
        if after_double_dash {
            jobs.push(parse_job_arg(&arg));
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliCommand::Help),
            "--" => {
                after_double_dash = true;
            }
            "--cache-root" => {
                let val = args
                    .next()
                    .ok_or_else(|| AccelError::Arg("Missing value for --cache-root".to_string()))?;
                cache_root = Some(PathBuf::from(val));
            }
            "--spec" => {
                let val = args
                    .next()
                    .ok_or_else(|| AccelError::Arg("Missing value for --spec".to_string()))?;
                for s in val.split(',') {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        specs.push(trimmed.to_string());
                    }
                }
            }
            other if other.starts_with('-') => {
                return Err(AccelError::Arg(format!(
                    "Unknown argument for precache-art: {}",
                    other
                )));
            }
            other => {
                jobs.push(parse_job_arg(other));
            }
        }
    }

    let root = cache_root
        .ok_or_else(|| AccelError::Arg("Missing required --cache-root argument".to_string()))?;

    Ok(CliCommand::PrecacheArt {
        cache_root: root,
        specs,
        jobs,
    })
}

fn parse_cli<I>(mut args: I) -> Result<CliCommand, AccelError>
where
    I: Iterator<Item = String>,
{
    let cmd = args
        .next()
        .ok_or_else(|| AccelError::Arg("Missing command".to_string()))?;

    match cmd.as_str() {
        "-h" | "--help" | "help" => Ok(CliCommand::Help),
        "scan-dir" => parse_scan_dir_args(args),
        "precache-art" => parse_precache_art_args(args),
        other => Err(AccelError::Arg(format!("Unknown command: {}", other))),
    }
}

fn print_usage() {
    eprintln!("Usage: lms-scanner-accel <command> [options]");
    eprintln!("Commands:");
    eprintln!("  scan-dir --db <db_path> --dir <basedir> [--extensions <csv>]");
    eprintln!("  precache-art --cache-root <dir> --spec <comma_specs> [--] <file::cachekey>...");
    eprintln!("Options:");
    eprintln!("  -h, --help    Show this help message");
}

fn main() {
    let command = match parse_cli(std::env::args().skip(1)) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("Error: {}", e);
            print_usage();
            std::process::exit(1);
        }
    };

    match command {
        CliCommand::Help => {
            print_usage();
        }
        CliCommand::ScanDir {
            db_path,
            basedir,
            extensions,
        } => {
            match populate_scanned_files_with_extensions(&db_path, &basedir, extensions.as_deref())
            {
                Ok(count) => {
                    println!("Populated {} scanned files", count);
                }
                Err(e) => {
                    eprintln!("Error populating scanned files: {}", e);
                    std::process::exit(1);
                }
            }
        }
        CliCommand::PrecacheArt {
            cache_root,
            specs,
            jobs,
        } => match precache_artwork_batch(&cache_root, &jobs, &specs) {
            Ok(count) => {
                println!("Precached {} artwork jobs", count);
            }
            Err(e) => {
                eprintln!("Error precaching artwork: {}", e);
                std::process::exit(1);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat as ImgFmt, RgbImage};
    use std::io::Cursor;

    fn create_scanned_files_table(db_path: &Path, with_lms_index: bool) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE scanned_files \
             (url text NOT NULL COLLATE NOCASE, timestamp int(10), filesize int(10))",
            [],
        )
        .unwrap();
        if with_lms_index {
            conn.execute("CREATE INDEX scannedUrlIndex ON scanned_files (url)", [])
                .unwrap();
        }
    }

    #[test]
    fn test_scan_dir_populates_scanned_files_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();
        std::fs::write(music_dir.join("song1.mp3"), vec![0u8; 1024]).unwrap();
        std::fs::write(music_dir.join("track 03.mp3"), vec![0u8; 2048]).unwrap();

        create_scanned_files_table(&db_path, false);

        let count = populate_scanned_files(&db_path, &music_dir).unwrap();
        assert_eq!(count, 2);

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
    fn test_percent_encode_apostrophe_and_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();
        let track_path = music_dir.join("O'Connor's Café.mp3");
        let url = path_to_file_url(&track_path);
        assert!(
            url.starts_with("file:///"),
            "expected file:/// prefix: {url}"
        );
        assert!(
            url.ends_with("/O%27Connor%27s%20Caf%C3%A9.mp3"),
            "expected %27 apostrophe and UTF-8 percent encoding, got: {url}"
        );
        assert!(
            !url.contains('\''),
            "apostrophe must not remain unencoded in URL: {url}"
        );
    }

    #[test]
    fn test_lexical_normalization_preserves_dot_dot_without_canonicalize() {
        let dir = tempfile::tempdir().unwrap();
        let music_dir = dir.path().join("music");
        let sub_dir = music_dir.join("rock");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(music_dir.join("track.flac"), vec![1u8; 16]).unwrap();

        let unnormalized = sub_dir.join("..").join(".").join("track.flac");
        let url = path_to_file_url(&unnormalized);
        assert!(
            !url.contains("/../") && !url.contains("/./"),
            "lexical normalization must collapse . and ..: {url}"
        );
        assert!(url.ends_with("/music/track.flac"), "unexpected url: {url}");
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_basedir_preserves_configured_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real_storage");
        let link_dir = dir.path().join("music_symlink");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(real_dir.join("album's track.flac"), vec![0u8; 64]).unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let db_path = dir.path().join("library.db");
        create_scanned_files_table(&db_path, true);

        let count = populate_scanned_files(&db_path, &link_dir).unwrap();
        assert_eq!(count, 1);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let url: String = conn
            .query_row("SELECT url FROM scanned_files", [], |r| r.get(0))
            .unwrap();
        let expected_prefix = path_to_file_url(&link_dir);
        assert!(
            url.starts_with(&expected_prefix),
            "URL {url} must preserve symlinked basedir prefix {expected_prefix}"
        );
        assert!(
            url.ends_with("/album%27s%20track.flac"),
            "URL {url} must encode apostrophe as %27"
        );
    }

    #[test]
    fn test_lms_traversal_filters_and_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();

        // Supported audio files (playlists excluded by default)
        std::fs::write(music_dir.join("keep1.mp3"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("keep2.FLAC"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("playlist.m3u8"), vec![0u8; 16]).unwrap();

        // Unsupported extensions and extensionless files
        std::fs::write(music_dir.join("cover.jpg"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("notes.txt"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("README"), vec![0u8; 16]).unwrap();

        // Dotfiles and dot-directories
        std::fs::write(music_dir.join(".hidden.mp3"), vec![0u8; 16]).unwrap();
        let hidden_dir = music_dir.join(".hidden_dir");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(hidden_dir.join("secret.mp3"), vec![0u8; 16]).unwrap();

        // NAS system directories
        for nas_dir in [
            "@eaDir",
            "#recycle",
            "#snapshot",
            ".AppleDouble",
            "lost+found",
        ] {
            let p = music_dir.join(nas_dir);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("ignored.mp3"), vec![0u8; 16]).unwrap();
        }

        let db_path = dir.path().join("library.db");
        create_scanned_files_table(&db_path, true);

        let count = populate_scanned_files(&db_path, &music_dir).unwrap();
        assert_eq!(count, 2);

        // Verify custom extensions override
        let db_custom = dir.path().join("custom.db");
        create_scanned_files_table(&db_custom, true);
        let custom_exts = vec!["flac".to_string()];
        let custom_count =
            populate_scanned_files_with_extensions(&db_custom, &music_dir, Some(&custom_exts))
                .unwrap();
        assert_eq!(custom_count, 1);

        // Verify no duplicate index was created when scannedUrlIndex already exists
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn.prepare("PRAGMA index_list('scanned_files')").unwrap();
        let index_names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(index_names, vec!["scannedUrlIndex".to_string()]);
    }

    #[test]
    fn test_unique_url_schema_skips_duplicate_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("unique.db");
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();
        std::fs::write(music_dir.join("track.mp3"), vec![0u8; 16]).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE scanned_files \
             (url text PRIMARY KEY COLLATE NOCASE, timestamp int(10), filesize int(10))",
            [],
        )
        .unwrap();
        drop(conn);

        let count = populate_scanned_files(&db_path, &music_dir).unwrap();
        assert_eq!(count, 1);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn.prepare("PRAGMA index_list('scanned_files')").unwrap();
        let index_names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(index_names.len(), 1);
    }

    #[test]
    fn test_precache_artwork_batch_populates_artwork_db() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("cover.jpg");
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(120, 120, |_, _| {
            image::Rgb([40, 80, 160])
        }));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImgFmt::Jpeg)
            .unwrap();
        std::fs::write(&img_path, buf).unwrap();

        let jobs = vec![(img_path, "music/cover01/cover_".to_string())];
        let specs = vec!["100x100_o".to_string(), "50x50_m".to_string()];
        let count = precache_artwork_batch(dir.path(), &jobs, &specs).unwrap();
        assert_eq!(count, 1);

        let store = lms_cache::DbArtworkStore::open(dir.path(), false).unwrap();
        assert!(store
            .get("music/cover01/cover_100x100_o")
            .unwrap()
            .is_some());
        assert!(store.get("music/cover01/cover_50x50_m").unwrap().is_some());
    }

    #[test]
    fn test_precache_artwork_batch_propagates_all_failed_and_cache_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing_img = dir.path().join("does_not_exist.jpg");
        let jobs = vec![(missing_img, "music/missing/cover_".to_string())];
        let specs = vec!["100x100_o".to_string()];

        let err = precache_artwork_batch(dir.path(), &jobs, &specs).unwrap_err();
        assert!(
            matches!(err, AccelError::Arg(_)),
            "expected Arg error when all jobs fail, got: {:?}",
            err
        );

        let bad_cache_root = dir.path().join("not_a_dir");
        std::fs::write(&bad_cache_root, b"occupied").unwrap();
        let cache_err = precache_artwork_batch(&bad_cache_root, &jobs, &specs).unwrap_err();
        assert!(
            matches!(cache_err, AccelError::Cache(_)),
            "expected Cache error for unwritable cache root, got: {:?}",
            cache_err
        );
    }

    #[test]
    fn test_cli_parsing_help_extensions_and_double_dash() {
        assert_eq!(
            parse_cli(["--help".to_string()].into_iter()).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            parse_cli(["scan-dir".to_string(), "-h".to_string()].into_iter()).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            parse_cli(
                [
                    "scan-dir".to_string(),
                    "--db".to_string(),
                    "lib.db".to_string(),
                    "--dir".to_string(),
                    "/music".to_string(),
                    "--extensions".to_string(),
                    ".mp3, FLAC".to_string(),
                ]
                .into_iter()
            )
            .unwrap(),
            CliCommand::ScanDir {
                db_path: PathBuf::from("lib.db"),
                basedir: PathBuf::from("/music"),
                extensions: Some(vec!["mp3".to_string(), "flac".to_string()]),
            }
        );
        assert_eq!(
            parse_cli(
                [
                    "precache-art".to_string(),
                    "--cache-root".to_string(),
                    "/cache".to_string(),
                    "--spec".to_string(),
                    "100x100_o".to_string(),
                    "--".to_string(),
                    "-weird-cover.jpg::key1".to_string(),
                ]
                .into_iter()
            )
            .unwrap(),
            CliCommand::PrecacheArt {
                cache_root: PathBuf::from("/cache"),
                specs: vec!["100x100_o".to_string()],
                jobs: vec![(PathBuf::from("-weird-cover.jpg"), "key1".to_string())],
            }
        );
    }

    #[test]
    fn test_default_audio_extensions_include_flc_m4b_pcm_mp_plus() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("audio.db");
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();

        for filename in ["track.flc", "audiobook.m4b", "raw.pcm", "muse.mp+"] {
            std::fs::write(music_dir.join(filename), vec![0u8; 16]).unwrap();
        }

        create_scanned_files_table(&db_path, true);
        let count = populate_scanned_files(&db_path, &music_dir).unwrap();
        assert_eq!(count, 4);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT url FROM scanned_files ORDER BY url")
            .unwrap();
        let urls: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(urls.len(), 4);
        assert!(urls.iter().any(|u| u.ends_with("/track.flc")));
        assert!(urls.iter().any(|u| u.ends_with("/audiobook.m4b")));
        assert!(urls.iter().any(|u| u.ends_with("/raw.pcm")));
        assert!(urls.iter().any(|u| u.ends_with("/muse.mp+")));
    }

    #[test]
    fn test_m3u_excluded_by_default_and_included_when_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();

        std::fs::write(music_dir.join("song.mp3"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("list.m3u"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("list.m3u8"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("list.pls"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("video.asf"), vec![0u8; 16]).unwrap();

        let default_db = dir.path().join("default.db");
        create_scanned_files_table(&default_db, true);
        let default_count = populate_scanned_files(&default_db, &music_dir).unwrap();
        assert_eq!(default_count, 1);

        let cmd = parse_cli(
            [
                "scan-dir".to_string(),
                "--db".to_string(),
                "explicit.db".to_string(),
                "--dir".to_string(),
                music_dir.to_string_lossy().into_owned(),
                "--extensions".to_string(),
                "m3u".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        let explicit_exts = match cmd {
            CliCommand::ScanDir {
                extensions: Some(exts),
                ..
            } => exts,
            other => panic!("unexpected parsed command: {:?}", other),
        };
        assert_eq!(explicit_exts, vec!["m3u".to_string()]);

        let explicit_db = dir.path().join("explicit.db");
        create_scanned_files_table(&explicit_db, true);
        let explicit_count =
            populate_scanned_files_with_extensions(&explicit_db, &music_dir, Some(&explicit_exts))
                .unwrap();
        assert_eq!(explicit_count, 1);

        let conn = rusqlite::Connection::open(&explicit_db).unwrap();
        let url: String = conn
            .query_row("SELECT url FROM scanned_files", [], |r| r.get(0))
            .unwrap();
        assert!(url.ends_with("/list.m3u"), "unexpected url: {url}");
    }

    #[test]
    fn test_temp_underscore_playlists_skipped_even_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("playlist.db");
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();

        std::fs::write(music_dir.join("valid.m3u"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("__foo.m3u"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("__bar.m3u8"), vec![0u8; 16]).unwrap();
        std::fs::write(music_dir.join("__CAPS.M3U"), vec![0u8; 16]).unwrap();

        create_scanned_files_table(&db_path, true);
        let exts = vec!["m3u".to_string(), "m3u8".to_string()];
        let count =
            populate_scanned_files_with_extensions(&db_path, &music_dir, Some(&exts)).unwrap();
        assert_eq!(count, 1);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let url: String = conn
            .query_row("SELECT url FROM scanned_files", [], |r| r.get(0))
            .unwrap();
        assert!(url.ends_with("/valid.m3u"), "unexpected url: {url}");
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_file_skipped() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("perms.db");
        let music_dir = dir.path().join("music");
        std::fs::create_dir_all(&music_dir).unwrap();

        let readable = music_dir.join("readable.mp3");
        let unreadable = music_dir.join("unreadable.mp3");
        std::fs::write(&readable, vec![0u8; 16]).unwrap();
        std::fs::write(&unreadable, vec![0u8; 16]).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        if std::fs::File::open(&unreadable).is_ok() {
            // Running as root or with DAC override; skip permission assertion.
            let _ = std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644));
            return;
        }

        create_scanned_files_table(&db_path, true);
        let count = populate_scanned_files(&db_path, &music_dir).unwrap();
        let _ = std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644));
        assert_eq!(count, 1);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let url: String = conn
            .query_row("SELECT url FROM scanned_files", [], |r| r.get(0))
            .unwrap();
        assert!(url.ends_with("/readable.mp3"), "unexpected url: {url}");
    }
}
