#![deny(unsafe_op_in_unsafe_fn)]

use md5::{Digest, Md5};
use std::path::Path;

#[derive(thiserror::Error, Debug)]
pub enum CacheError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid artwork blob: {0}")]
    InvalidBlob(String),
    #[error("Lock error: {0}")]
    Lock(String),
    #[error("System clock error: {0}")]
    Clock(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkRecord {
    pub content_type: String,
    pub mtime: u32,
    pub original_path: String,
    pub data: Vec<u8>,
}

/// Computes the 64-bit integer cache key identical to Perl
/// `Slim::Utils::DbCache::_key`.
///
/// In Perl:
/// ```perl
/// my $md5 = Digest::MD5::md5_hex($key);
/// return (hex( substr($md5, 0, 8) ) << 28) | hex( substr($md5, 8, 7) );
/// ```
/// 32-bit high bits and 28-bit low bits are bitwise packed into a 60-bit
/// integer.
pub fn md5_key_i64(key: &str) -> i64 {
    let digest = Md5::digest(key.as_bytes());
    let head: [u8; 8] = digest
        .get(..8)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 8]);
    (u64::from_be_bytes(head) >> 4) as i64
}

/// Encodes an `ArtworkRecord` into a binary blob matching Perl
/// `Slim::Utils::ArtworkCache` `pack('A3LS', ...)`.
///
/// Binary layout:
/// - `[0..3]`: 3-byte space-padded ASCII `content_type` (`pack('A3')`)
/// - `[3..7]`: 4-byte little-endian `mtime` (`pack('L')`)
/// - `[7..9]`: 2-byte little-endian length of original file path (`pack('S')`)
/// - `[9..9+path_len]`: original file path bytes
/// - `[9+path_len..]`: raw image data payload
pub fn encode_artwork_blob(record: &ArtworkRecord) -> Result<Vec<u8>, CacheError> {
    if record.content_type.is_empty()
        || !record.content_type.is_ascii()
        || record
            .content_type
            .chars()
            .any(|ch| ch == '\0' || ch.is_whitespace())
    {
        return Err(CacheError::InvalidBlob(
            "content_type must be non-empty ASCII without NUL or whitespace".to_string(),
        ));
    }
    if record.content_type.len() > 3 {
        return Err(CacheError::InvalidBlob(format!(
            "content_type length {} exceeds maximum 3 bytes",
            record.content_type.len()
        )));
    }

    let mut ct = [b' '; 3];
    for (dst, src) in ct.iter_mut().zip(record.content_type.as_bytes()) {
        *dst = *src;
    }

    let path_bytes = record.original_path.as_bytes();
    let path_len = u16::try_from(path_bytes.len()).map_err(|_| {
        CacheError::InvalidBlob(format!(
            "original_path length {} exceeds maximum {}",
            path_bytes.len(),
            u16::MAX
        ))
    })?;

    let capacity = 9usize
        .checked_add(path_bytes.len())
        .and_then(|n| n.checked_add(record.data.len()))
        .ok_or_else(|| CacheError::InvalidBlob("artwork blob size overflow".to_string()))?;

    let mut buf = Vec::with_capacity(capacity);
    buf.extend_from_slice(&ct);
    buf.extend_from_slice(&record.mtime.to_le_bytes());
    buf.extend_from_slice(&path_len.to_le_bytes());
    buf.extend_from_slice(path_bytes);
    buf.extend_from_slice(&record.data);
    Ok(buf)
}

/// Decodes an `ArtworkRecord` from a binary blob packed by
/// `encode_artwork_blob` or Perl `pack('A3LS', ...)`.
pub fn decode_artwork_blob(blob: &[u8]) -> Result<ArtworkRecord, CacheError> {
    let [c0, c1, c2, m0, m1, m2, m3, p0, p1, rest @ ..] = blob else {
        return Err(CacheError::InvalidBlob(format!(
            "blob length {} is less than minimum 9-byte header",
            blob.len()
        )));
    };

    let ct_bytes = [*c0, *c1, *c2];
    let raw_ct = std::str::from_utf8(&ct_bytes)
        .map_err(|e| CacheError::InvalidBlob(format!("invalid UTF-8 in content_type: {e}")))?;
    let content_type = raw_ct.trim_end_matches([' ', '\0']).to_string();
    let mtime = u32::from_le_bytes([*m0, *m1, *m2, *m3]);
    let path_len = usize::from(u16::from_le_bytes([*p0, *p1]));

    let (Some(path_bytes), Some(data_bytes)) = (rest.get(..path_len), rest.get(path_len..)) else {
        return Err(CacheError::InvalidBlob(format!(
            "blob length {} is less than header (9) + path_len ({path_len})",
            blob.len()
        )));
    };

    let original_path = String::from_utf8_lossy(path_bytes).into_owned();
    let data = data_bytes.to_vec();

    Ok(ArtworkRecord {
        content_type,
        mtime,
        original_path,
        data,
    })
}

fn current_unix_secs() -> Result<i64, CacheError> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| CacheError::Clock(format!("system clock before UNIX_EPOCH: {e}")))?
        .as_secs();
    i64::try_from(secs)
        .map_err(|e| CacheError::Clock(format!("system clock seconds overflow i64: {e}")))
}

fn imgproxy_expiry_secs() -> Result<i64, CacheError> {
    current_unix_secs()?
        .checked_add(86400 * 30)
        .ok_or_else(|| CacheError::Clock("imgproxy expiry timestamp overflow".into()))
}

/// SQLite artwork database store providing bit-exact compatibility with Perl
/// LMS `DbArtworkCache` / `DbCache`.
pub struct DbArtworkStore {
    conn: std::sync::Mutex<rusqlite::Connection>,
    is_imageproxy: bool,
}

fn has_dot_or_dotdot_segment(path: &Path) -> bool {
    if path.components().any(|comp| {
        matches!(
            comp,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return true;
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str()
            .as_bytes()
            .split(|&b| b == b'/')
            .any(|seg| seg == b"." || seg == b"..")
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        units
            .split(|&u| u == u16::from(b'/') || u == u16::from(b'\\'))
            .any(|seg| seg == [u16::from(b'.')] || seg == [u16::from(b'.'), u16::from(b'.')])
    }

    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn is_root_owned_system_temp_symlink(
    cur: &Path,
    link_meta: &std::fs::Metadata,
) -> Result<bool, CacheError> {
    use std::os::unix::fs::MetadataExt;
    if (cur == Path::new("/tmp") || cur == Path::new("/var"))
        && link_meta.file_type().is_symlink()
        && link_meta.uid() == 0
    {
        let target_meta = std::fs::metadata(cur)?;
        return Ok(target_meta.is_dir() && target_meta.uid() == 0);
    }
    Ok(false)
}

fn verify_cache_dir(cache_root: &Path) -> Result<(), CacheError> {
    if cache_root.as_os_str().is_empty() {
        return Err(CacheError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache_root must not be empty",
        )));
    }
    if has_dot_or_dotdot_segment(cache_root) {
        return Err(CacheError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache_root must not contain '.' or '..' components",
        )));
    }

    let mut normalized_cache_root = std::path::PathBuf::new();

    for comp in cache_root.components() {
        match comp {
            std::path::Component::Prefix(prefix) => {
                normalized_cache_root.push(prefix.as_os_str());
                continue;
            }
            std::path::Component::RootDir => {
                normalized_cache_root.push(comp.as_os_str());
                continue;
            }
            std::path::Component::CurDir | std::path::Component::ParentDir => {
                return Err(CacheError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cache_root must not contain '.' or '..' components",
                )));
            }
            std::path::Component::Normal(_) => {
                normalized_cache_root.push(comp.as_os_str());
            }
        }

        match std::fs::symlink_metadata(&normalized_cache_root) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    #[cfg(target_os = "macos")]
                    if is_root_owned_system_temp_symlink(&normalized_cache_root, &meta)? {
                        continue;
                    }
                    return Err(CacheError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "cache_root and its ancestors must not be symlinks",
                    )));
                }
                if !meta.is_dir() {
                    return Err(CacheError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "cache_root ancestor is not a directory",
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&normalized_cache_root) {
                    Ok(()) => {}
                    Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(create_err) => return Err(CacheError::Io(create_err)),
                }
                let meta = std::fs::symlink_metadata(&normalized_cache_root)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(CacheError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "cache_root is not a valid non-symlink directory",
                    )));
                }
            }
            Err(e) => return Err(CacheError::Io(e)),
        }
    }

    let leaf_meta = std::fs::symlink_metadata(&normalized_cache_root)?;
    if leaf_meta.file_type().is_symlink() || !leaf_meta.is_dir() {
        return Err(CacheError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache_root is not a valid non-symlink directory",
        )));
    }
    Ok(())
}

fn verify_regular_or_absent(path: &Path) -> Result<(), CacheError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CacheError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "database path must not be a symlink",
                )));
            }
            if !meta.file_type().is_file() {
                return Err(CacheError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "database path must be a regular file",
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CacheError::Io(e)),
    }
    Ok(())
}

impl DbArtworkStore {
    pub fn open(cache_root: &Path, is_imageproxy: bool) -> Result<Self, CacheError> {
        verify_cache_dir(cache_root)?;
        let db_filename = if is_imageproxy {
            "imgproxy.db"
        } else {
            "artwork.db"
        };
        let db_path = cache_root.join(db_filename);
        verify_regular_or_absent(&db_path)?;
        verify_regular_or_absent(&cache_root.join(format!("{db_filename}-wal")))?;
        verify_regular_or_absent(&cache_root.join(format!("{db_filename}-shm")))?;
        verify_regular_or_absent(&cache_root.join(format!("{db_filename}-journal")))?;
        let conn = rusqlite::Connection::open(db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;

        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        if page_count == 0 {
            conn.execute_batch("PRAGMA auto_vacuum = FULL;")?;
        }

        conn.execute_batch(
            "PRAGMA locking_mode = NORMAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA journal_mode = WAL;
             PRAGMA journal_size_limit = 16777216;",
        )?;

        if is_imageproxy {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB, t INTEGER);
                 CREATE INDEX IF NOT EXISTS expiry ON cache (t);",
            )?;
        } else {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB);",
            )?;
        }

        Ok(Self {
            conn: std::sync::Mutex::new(conn),
            is_imageproxy,
        })
    }

    pub fn set(&self, key: &str, record: &ArtworkRecord) -> Result<(), CacheError> {
        let k = md5_key_i64(key);
        let blob = encode_artwork_blob(record)?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| CacheError::Lock(e.to_string()))?;

        if self.is_imageproxy {
            let expires = imgproxy_expiry_secs()?;
            let mut stmt =
                conn.prepare_cached("INSERT OR REPLACE INTO cache (k, v, t) VALUES (?1, ?2, ?3)")?;
            stmt.execute(rusqlite::params![k, blob, expires])?;
        } else {
            let mut stmt =
                conn.prepare_cached("INSERT OR REPLACE INTO cache (k, v) VALUES (?1, ?2)")?;
            stmt.execute(rusqlite::params![k, blob])?;
        }
        Ok(())
    }

    /// Inserts or replaces a batch of artwork records in a single atomic
    /// SQLite transaction.
    pub fn set_batch(&self, entries: &[(String, ArtworkRecord)]) -> Result<(), CacheError> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut encoded = Vec::with_capacity(entries.len());
        for (key, record) in entries {
            encoded.push((md5_key_i64(key), encode_artwork_blob(record)?));
        }

        let expires = if self.is_imageproxy {
            Some(imgproxy_expiry_secs()?)
        } else {
            None
        };

        let mut conn = self
            .conn
            .lock()
            .map_err(|e| CacheError::Lock(e.to_string()))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let sql = if expires.is_some() {
                "INSERT OR REPLACE INTO cache (k, v, t) VALUES (?1, ?2, ?3)"
            } else {
                "INSERT OR REPLACE INTO cache (k, v) VALUES (?1, ?2)"
            };
            let mut stmt = tx.prepare_cached(sql)?;
            for (k, blob) in &encoded {
                if let Some(exp) = expires {
                    stmt.execute(rusqlite::params![k, blob, exp])?;
                } else {
                    stmt.execute(rusqlite::params![k, blob])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<ArtworkRecord>, CacheError> {
        let k = md5_key_i64(key);
        let conn = self
            .conn
            .lock()
            .map_err(|e| CacheError::Lock(e.to_string()))?;

        if self.is_imageproxy {
            let now = current_unix_secs()?;
            let mut stmt = conn.prepare_cached("SELECT v, t FROM cache WHERE k = ?1")?;
            let mut rows = stmt.query(rusqlite::params![k])?;
            if let Some(row) = rows.next()? {
                let blob: Vec<u8> = row.get(0)?;
                let t: Option<i64> = row.get(1)?;
                if let Some(t_val) = t {
                    if t_val >= 0 && t_val < now {
                        return Ok(None);
                    }
                }
                return Ok(Some(decode_artwork_blob(&blob)?));
            }
        } else {
            let mut stmt = conn.prepare_cached("SELECT v FROM cache WHERE k = ?1")?;
            let mut rows = stmt.query(rusqlite::params![k])?;
            if let Some(row) = rows.next()? {
                let blob: Vec<u8> = row.get(0)?;
                return Ok(Some(decode_artwork_blob(&blob)?));
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    static TMPDIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn create_test_tempdir() -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let _guard = TMPDIR_ENV_LOCK
            .lock()
            .map_err(|e| format!("tmpdir env lock poisoned: {e}"))?;
        Ok(tempfile::tempdir()?)
    }

    #[cfg(unix)]
    struct TmpdirEnvGuard {
        prev: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl TmpdirEnvGuard {
        fn set(path: &Path) -> Self {
            let prev = std::env::var_os("TMPDIR");
            std::env::set_var("TMPDIR", path);
            Self { prev }
        }
    }

    #[cfg(unix)]
    impl Drop for TmpdirEnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(val) => std::env::set_var("TMPDIR", val),
                None => std::env::remove_var("TMPDIR"),
            }
        }
    }

    fn legacy_md5_key_i64(key: &str) -> Result<i64, Box<dyn std::error::Error>> {
        let digest = Md5::digest(key.as_bytes());
        let hex = format!("{:032x}", digest);
        let hi_hex = hex.get(0..8).ok_or("missing hi hex slice")?;
        let lo_hex = hex.get(8..15).ok_or("missing lo hex slice")?;
        let hi = u32::from_str_radix(hi_hex, 16)?;
        let lo = u32::from_str_radix(lo_hex, 16)?;
        Ok(((hi as i64) << 28) | (lo as i64))
    }

    #[test]
    fn test_dbcache_key_matches_perl_60bit_packing() {
        // 1. Key with leading zero in nibbles 8..15:
        // md5_hex("test/21") = "72e64b0f018e085acec8285c35c36e44"
        // (0x72e64b0f << 28) | 0x018e085 = 0x072e_64b0_f018_e085
        assert_eq!(md5_key_i64("test/21"), 0x072e_64b0_f018_e085_i64);

        // 2. Standard artwork cache key:
        // md5_hex("music/12345/cover_100x100_o") =
        //   "c78c3d3e2923430b16a433d8de85427c"
        // (0xc78c3d3e << 28) | 0x2923430 = 0x0c78_c3d3_e292_3430
        assert_eq!(
            md5_key_i64("music/12345/cover_100x100_o"),
            0x0c78_c3d3_e292_3430_i64
        );
    }

    #[test]
    fn test_md5_key_i64_matches_legacy_hex_computation() -> TestResult {
        let samples = [
            "",
            "test/21",
            "music/12345/cover_100x100_o",
            "music/999/cover_300x300_m",
            "http://example.com/art/\u{00e9}.jpg",
            "a",
            "0",
            "high-bit-digest-probe-1",
            "high-bit-digest-probe-2",
        ];
        for key in samples {
            assert_eq!(md5_key_i64(key), legacy_md5_key_i64(key)?);
        }
        Ok(())
    }

    #[test]
    fn test_artwork_blob_roundtrip_and_a3ls_header_layout() -> TestResult {
        let rec = ArtworkRecord {
            content_type: "jpg".to_string(),
            mtime: 1700000000,
            original_path: "/music/test.mp3".to_string(),
            data: vec![0xFF, 0xD8, 0xFF, 0xE0],
        };
        let encoded = encode_artwork_blob(&rec)?;
        assert_eq!(encoded.get(0..3), Some(b"jpg".as_slice()));
        let mtime_slice: [u8; 4] = encoded.get(3..7).ok_or("missing mtime slice")?.try_into()?;
        assert_eq!(u32::from_le_bytes(mtime_slice), 1700000000);
        let len_slice: [u8; 2] = encoded
            .get(7..9)
            .ok_or("missing path_len slice")?
            .try_into()?;
        assert_eq!(u16::from_le_bytes(len_slice), 15);
        let decoded = decode_artwork_blob(&encoded)?;
        assert_eq!(decoded, rec);
        Ok(())
    }

    #[test]
    fn test_artwork_blob_rejects_oversized_path() -> TestResult {
        let max_len = usize::from(u16::MAX);
        let exact = ArtworkRecord {
            content_type: "jpg".to_string(),
            mtime: 1,
            original_path: "a".repeat(max_len),
            data: vec![1, 2],
        };
        let encoded = encode_artwork_blob(&exact)?;
        let decoded = decode_artwork_blob(&encoded)?;
        assert_eq!(decoded.original_path.len(), max_len);

        let oversized = ArtworkRecord {
            content_type: "jpg".to_string(),
            mtime: 1,
            original_path: "a".repeat(max_len + 1),
            data: vec![1, 2],
        };
        assert!(matches!(
            encode_artwork_blob(&oversized),
            Err(CacheError::InvalidBlob(_))
        ));
        Ok(())
    }

    #[test]
    fn test_artwork_blob_rejects_invalid_content_type() {
        let invalid_types = [
            "",
            " ",
            " jpg",
            "jpg ",
            "j\0g",
            "jp\t",
            "jp\u{00e9}",
            "jpeg",
            "webp",
            "1234",
        ];
        for ct in invalid_types {
            let rec = ArtworkRecord {
                content_type: ct.to_string(),
                mtime: 42,
                original_path: "/music/a.mp3".to_string(),
                data: vec![0x01],
            };
            assert!(
                matches!(encode_artwork_blob(&rec), Err(CacheError::InvalidBlob(_))),
                "expected InvalidBlob for content_type {ct:?}"
            );
        }
    }

    #[test]
    fn test_artwork_blob_non_utf8_original_path_roundtrip() -> TestResult {
        let raw_path = b"/music/\xff\xfe/cover.mp3";
        let path_len = u16::try_from(raw_path.len())?;
        let mut raw_blob = Vec::new();
        raw_blob.extend_from_slice(b"png");
        raw_blob.extend_from_slice(&123u32.to_le_bytes());
        raw_blob.extend_from_slice(&path_len.to_le_bytes());
        raw_blob.extend_from_slice(raw_path);
        raw_blob.extend_from_slice(&[0x89, b'P', b'N', b'G']);

        let decoded = decode_artwork_blob(&raw_blob)?;
        assert_eq!(decoded.content_type, "png");
        assert_eq!(decoded.mtime, 123);
        assert_eq!(decoded.original_path, "/music/\u{FFFD}\u{FFFD}/cover.mp3");
        assert_eq!(decoded.data, vec![0x89, b'P', b'N', b'G']);

        let reencoded = encode_artwork_blob(&decoded)?;
        let redecoded = decode_artwork_blob(&reencoded)?;
        assert_eq!(redecoded, decoded);
        Ok(())
    }

    #[test]
    fn test_sqlite_artwork_and_imgproxy_tables() -> TestResult {
        let dir = create_test_tempdir()?;
        let store = DbArtworkStore::open(dir.path(), false)?;
        let rec = ArtworkRecord {
            content_type: "png".to_string(),
            mtime: 0,
            original_path: String::new(),
            data: vec![0x89, b'P', b'N', b'G'],
        };
        store.set("music/abc/cover_50x50_o", &rec)?;
        let loaded = store
            .get("music/abc/cover_50x50_o")?
            .ok_or("expected cached artwork record")?;
        assert_eq!(loaded, rec);

        // Verify persisted SQLite WAL/auto_vacuum PRAGMAs across connections,
        // and per-connection synchronous/journal_size_limit PRAGMAs on store.conn.
        let conn_check = rusqlite::Connection::open(dir.path().join("artwork.db"))?;
        let journal_mode: String =
            conn_check.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let auto_vacuum: i64 = conn_check.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(auto_vacuum, 1);
        drop(conn_check);

        {
            let store_conn = store.conn.lock().map_err(|e| format!("lock failed: {e}"))?;
            let synchronous: i64 =
                store_conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
            let journal_size_limit: i64 =
                store_conn.query_row("PRAGMA journal_size_limit", [], |row| row.get(0))?;
            assert_eq!(synchronous, 1);
            assert_eq!(journal_size_limit, 16777216);
        }

        // Also test imgproxy.db table
        let store_proxy = DbArtworkStore::open(dir.path(), true)?;
        let rec_proxy = ArtworkRecord {
            content_type: "jpg".to_string(),
            mtime: 12345678,
            original_path: "/proxy/image.jpg".to_string(),
            data: vec![1, 2, 3, 4],
        };
        store_proxy.set("proxy/test_key", &rec_proxy)?;
        let loaded_proxy = store_proxy
            .get("proxy/test_key")?
            .ok_or("expected cached proxy record")?;
        assert_eq!(loaded_proxy, rec_proxy);

        // Verify t = -1 (never expire), t = 0 (expired), and t = 1 (expired)
        let conn_direct = rusqlite::Connection::open(dir.path().join("imgproxy.db"))?;
        conn_direct.execute(
            "UPDATE cache SET t = -1 WHERE k = ?1",
            rusqlite::params![md5_key_i64("proxy/test_key")],
        )?;
        assert_eq!(store_proxy.get("proxy/test_key")?, Some(rec_proxy));

        conn_direct.execute(
            "UPDATE cache SET t = 0 WHERE k = ?1",
            rusqlite::params![md5_key_i64("proxy/test_key")],
        )?;
        assert_eq!(store_proxy.get("proxy/test_key")?, None);

        conn_direct.execute(
            "UPDATE cache SET t = 1 WHERE k = ?1",
            rusqlite::params![md5_key_i64("proxy/test_key")],
        )?;
        drop(conn_direct);
        assert_eq!(store_proxy.get("proxy/test_key")?, None);
        Ok(())
    }

    #[test]
    fn test_sqlite_concurrent_reader_writer() -> TestResult {
        let dir = create_test_tempdir()?;
        let root = dir.path().to_path_buf();
        let initial_store = DbArtworkStore::open(&root, false)?;
        drop(initial_store);

        let writer_root = root.clone();
        let writer = std::thread::spawn(move || -> Result<(), String> {
            let store = DbArtworkStore::open(&writer_root, false).map_err(|e| e.to_string())?;
            for idx in 0..25u32 {
                let rec = ArtworkRecord {
                    content_type: "jpg".to_string(),
                    mtime: idx,
                    original_path: format!("/music/{idx}.mp3"),
                    data: vec![idx as u8; 8],
                };
                store
                    .set(&format!("concurrent/key/{idx}"), &rec)
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        });

        let mut readers = Vec::new();
        for _ in 0..2 {
            let reader_root = root.clone();
            readers.push(std::thread::spawn(move || -> Result<(), String> {
                let store = DbArtworkStore::open(&reader_root, false).map_err(|e| e.to_string())?;
                for idx in 0..25u32 {
                    let _ = store
                        .get(&format!("concurrent/key/{idx}"))
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }));
        }

        writer
            .join()
            .map_err(|_| "writer thread panicked")?
            .map_err(|e| format!("writer error: {e}"))?;
        for reader in readers {
            reader
                .join()
                .map_err(|_| "reader thread panicked")?
                .map_err(|e| format!("reader error: {e}"))?;
        }

        let verify_store = DbArtworkStore::open(&root, false)?;
        let last = verify_store
            .get("concurrent/key/24")?
            .ok_or("expected last written record")?;
        assert_eq!(last.mtime, 24);
        Ok(())
    }

    #[test]
    fn test_artwork_blob_error_handling() {
        // Truncated blob (< 9 bytes)
        assert!(decode_artwork_blob(&[1, 2, 3]).is_err());

        // Blob shorter than indicated path length (path_len = 50, 9-byte blob)
        let mut malformed = vec![b'j', b'p', b'g', 0, 0, 0, 0, 50, 0];
        assert!(decode_artwork_blob(&malformed).is_err());
        malformed.extend_from_slice(b"short");
        assert!(decode_artwork_blob(&malformed).is_err());
    }

    #[test]
    fn test_set_batch_atomic_commit_and_rollback() -> TestResult {
        let dir = create_test_tempdir()?;
        let store = DbArtworkStore::open(dir.path(), false)?;

        // Empty batch succeeds cleanly.
        store.set_batch(&[])?;

        // Multi-entry batch commits atomically.
        let batch = vec![
            (
                "batch/key/1".to_string(),
                ArtworkRecord {
                    content_type: "jpg".to_string(),
                    mtime: 100,
                    original_path: "/music/1.mp3".to_string(),
                    data: vec![1, 2, 3],
                },
            ),
            (
                "batch/key/2".to_string(),
                ArtworkRecord {
                    content_type: "png".to_string(),
                    mtime: 200,
                    original_path: "/music/2.mp3".to_string(),
                    data: vec![4, 5, 6],
                },
            ),
        ];
        store.set_batch(&batch)?;
        assert_eq!(store.get("batch/key/1")?, Some(batch[0].1.clone()));
        assert_eq!(store.get("batch/key/2")?, Some(batch[1].1.clone()));

        // Invalid record in batch leaves earlier entries unwritten.
        let invalid_batch = vec![
            (
                "batch/invalid/1".to_string(),
                ArtworkRecord {
                    content_type: "jpg".to_string(),
                    mtime: 300,
                    original_path: "/music/3.mp3".to_string(),
                    data: vec![7, 8],
                },
            ),
            (
                "batch/invalid/2".to_string(),
                ArtworkRecord {
                    content_type: "jpeg".to_string(),
                    mtime: 400,
                    original_path: "/music/4.mp3".to_string(),
                    data: vec![9, 10],
                },
            ),
        ];
        assert!(matches!(
            store.set_batch(&invalid_batch),
            Err(CacheError::InvalidBlob(_))
        ));
        assert_eq!(store.get("batch/invalid/1")?, None);
        assert_eq!(store.get("batch/invalid/2")?, None);

        // SQLite failure on a later entry rolls back earlier inserts in the same
        // transaction.
        let fail_key = md5_key_i64("batch/rollback/2");
        {
            let conn = store.conn.lock().map_err(|e| format!("lock failed: {e}"))?;
            conn.execute_batch(&format!(
                "CREATE TRIGGER abort_second_insert BEFORE INSERT ON cache
                 WHEN NEW.k = {fail_key}
                 BEGIN
                     SELECT RAISE(ABORT, 'simulated mid-batch failure');
                 END;"
            ))?;
        }

        let rollback_batch = vec![
            (
                "batch/rollback/1".to_string(),
                ArtworkRecord {
                    content_type: "jpg".to_string(),
                    mtime: 500,
                    original_path: "/music/5.mp3".to_string(),
                    data: vec![11, 12],
                },
            ),
            (
                "batch/rollback/2".to_string(),
                ArtworkRecord {
                    content_type: "png".to_string(),
                    mtime: 600,
                    original_path: "/music/6.mp3".to_string(),
                    data: vec![13, 14],
                },
            ),
        ];
        assert!(matches!(
            store.set_batch(&rollback_batch),
            Err(CacheError::Sqlite(_))
        ));
        assert_eq!(store.get("batch/rollback/1")?, None);
        assert_eq!(store.get("batch/rollback/2")?, None);

        // Imgproxy mode also commits batches with expiry timestamps.
        let proxy_store = DbArtworkStore::open(dir.path(), true)?;
        proxy_store.set_batch(&batch)?;
        assert_eq!(proxy_store.get("batch/key/1")?, Some(batch[0].1.clone()));
        assert_eq!(proxy_store.get("batch/key/2")?, Some(batch[1].1.clone()));

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_open_rejects_symlink_dir_and_symlink_db_file() -> TestResult {
        let dir = create_test_tempdir()?;
        let real_dir = dir.path().join("real_cache");
        std::fs::create_dir(&real_dir)?;
        let link_dir = dir.path().join("link_cache");
        std::os::unix::fs::symlink(&real_dir, &link_dir)?;

        assert!(matches!(
            DbArtworkStore::open(&link_dir, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let reg_as_dir = dir.path().join("not_a_dir");
        std::fs::write(&reg_as_dir, b"file")?;
        assert!(matches!(
            DbArtworkStore::open(&reg_as_dir, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let db_target = dir.path().join("target.db");
        std::fs::write(&db_target, b"")?;
        let sym_db_dir = dir.path().join("sym_db_cache");
        std::fs::create_dir(&sym_db_dir)?;
        std::os::unix::fs::symlink(&db_target, sym_db_dir.join("artwork.db"))?;
        assert!(matches!(
            DbArtworkStore::open(&sym_db_dir, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let dir_db_cache = dir.path().join("dir_db_cache");
        std::fs::create_dir(&dir_db_cache)?;
        std::fs::create_dir(dir_db_cache.join("imgproxy.db"))?;
        assert!(matches!(
            DbArtworkStore::open(&dir_db_cache, true),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_open_rejects_symlinked_ancestor_existing_and_missing_tail() -> TestResult {
        let dir = create_test_tempdir()?;
        let real_parent = dir.path().join("real_parent");
        std::fs::create_dir(&real_parent)?;
        let real_leaf = real_parent.join("existing_leaf");
        std::fs::create_dir(&real_leaf)?;

        let sym_parent = dir.path().join("sym_parent");
        std::os::unix::fs::symlink(&real_parent, &sym_parent)?;

        assert!(matches!(
            DbArtworkStore::open(&sym_parent.join("existing_leaf"), false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let missing_tail = sym_parent.join("missing_a").join("missing_b");
        assert!(matches!(
            DbArtworkStore::open(&missing_tail, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(
            !real_parent.join("missing_a").exists(),
            "missing tail segment must not be created behind a symlinked ancestor"
        );

        let valid_nested = real_parent.join("valid_a").join("valid_b");
        let store = DbArtworkStore::open(&valid_nested, false)?;
        drop(store);
        assert!(valid_nested.is_dir());

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_open_rejects_symlink_journal_sidecar_files() -> TestResult {
        let dir = create_test_tempdir()?;
        let target = dir.path().join("target.bin");
        std::fs::write(&target, b"")?;

        let art_dir = dir.path().join("sym_art_journal");
        std::fs::create_dir(&art_dir)?;
        std::os::unix::fs::symlink(&target, art_dir.join("artwork.db-journal"))?;
        assert!(matches!(
            DbArtworkStore::open(&art_dir, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let proxy_dir = dir.path().join("sym_proxy_journal");
        std::fs::create_dir(&proxy_dir)?;
        std::os::unix::fs::symlink(&target, proxy_dir.join("imgproxy.db-journal"))?;
        assert!(matches!(
            DbArtworkStore::open(&proxy_dir, true),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_open_rejects_tmpdir_pointing_to_user_symlink() -> TestResult {
        let dir = create_test_tempdir()?;
        let real_tmp = dir.path().join("real_tmp");
        std::fs::create_dir(&real_tmp)?;
        let sym_tmp = dir.path().join("sym_tmp");
        std::os::unix::fs::symlink(&real_tmp, &sym_tmp)?;

        let _lock = TMPDIR_ENV_LOCK
            .lock()
            .map_err(|e| format!("tmpdir env lock poisoned: {e}"))?;
        let _env_guard = TmpdirEnvGuard::set(&sym_tmp);

        assert_eq!(std::env::temp_dir(), sym_tmp);
        assert!(matches!(
            DbArtworkStore::open(&sym_tmp.join("cache"), false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            DbArtworkStore::open(&sym_tmp.join("nested").join("cache"), false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(
            !real_tmp.join("cache").exists() && !real_tmp.join("nested").exists(),
            "cache directories must not be created behind a symlinked TMPDIR"
        );

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_open_rejects_curdir_and_parentdir_before_creating_subdirs() -> TestResult {
        let dir = create_test_tempdir()?;
        let base = dir.path().join("base");
        std::fs::create_dir(&base)?;

        assert!(matches!(
            DbArtworkStore::open(Path::new("."), false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(matches!(
            DbArtworkStore::open(Path::new(".."), false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let cur_mid = base.join("sub_cur").join(".").join("leaf");
        assert!(matches!(
            DbArtworkStore::open(&cur_mid, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(
            !base.join("sub_cur").exists(),
            "CurDir segment must be rejected before creating sub_cur"
        );

        let cur_leaf = base.join("sub_cur_leaf").join(".");
        assert!(matches!(
            DbArtworkStore::open(&cur_leaf, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(
            !base.join("sub_cur_leaf").exists(),
            "trailing CurDir segment must be rejected before creating sub_cur_leaf"
        );

        let parent_mid = base.join("sub_parent").join("..").join("escaped");
        assert!(matches!(
            DbArtworkStore::open(&parent_mid, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(
            !base.join("sub_parent").exists() && !base.join("escaped").exists(),
            "ParentDir segment must be rejected before creating sub_parent"
        );

        let parent_leaf = base.join("sub_parent_leaf").join("..");
        assert!(matches!(
            DbArtworkStore::open(&parent_leaf, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(
            !base.join("sub_parent_leaf").exists(),
            "trailing ParentDir segment must be rejected before creating sub_parent_leaf"
        );

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_open_rejects_trailing_slash_or_dot_on_symlinked_leaf() -> TestResult {
        let dir = create_test_tempdir()?;
        let real_leaf = dir.path().join("real_leaf");
        std::fs::create_dir(&real_leaf)?;
        let sym_leaf = dir.path().join("sym_leaf");
        std::os::unix::fs::symlink(&real_leaf, &sym_leaf)?;

        let trailing_slash = std::path::PathBuf::from(format!("{}/", sym_leaf.display()));
        assert!(matches!(
            DbArtworkStore::open(&trailing_slash, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let double_trailing_slash = std::path::PathBuf::from(format!("{}//", sym_leaf.display()));
        assert!(matches!(
            DbArtworkStore::open(&double_trailing_slash, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let trailing_dot = std::path::PathBuf::from(format!("{}/.", sym_leaf.display()));
        assert!(matches!(
            DbArtworkStore::open(&trailing_dot, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let trailing_dot_slash = std::path::PathBuf::from(format!("{}/./", sym_leaf.display()));
        assert!(matches!(
            DbArtworkStore::open(&trailing_dot_slash, false),
            Err(CacheError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidInput
        ));

        let valid_trailing_slash = std::path::PathBuf::from(format!("{}/", real_leaf.display()));
        let store = DbArtworkStore::open(&valid_trailing_slash, false)?;
        drop(store);

        Ok(())
    }
}
