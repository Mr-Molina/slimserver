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

/// SQLite artwork database store providing bit-exact compatibility with Perl
/// LMS `DbArtworkCache` / `DbCache`.
pub struct DbArtworkStore {
    conn: std::sync::Mutex<rusqlite::Connection>,
    is_imageproxy: bool,
}

impl DbArtworkStore {
    pub fn open(cache_root: &Path, is_imageproxy: bool) -> Result<Self, CacheError> {
        std::fs::create_dir_all(cache_root)?;
        let db_filename = if is_imageproxy {
            "imgproxy.db"
        } else {
            "artwork.db"
        };
        let db_path = cache_root.join(db_filename);
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
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let expires = now + 86400 * 30;
            conn.execute(
                "INSERT OR REPLACE INTO cache (k, v, t) VALUES (?1, ?2, ?3)",
                rusqlite::params![k, blob, expires],
            )?;
        } else {
            conn.execute(
                "INSERT OR REPLACE INTO cache (k, v) VALUES (?1, ?2)",
                rusqlite::params![k, blob],
            )?;
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<ArtworkRecord>, CacheError> {
        let k = md5_key_i64(key);
        let conn = self
            .conn
            .lock()
            .map_err(|e| CacheError::Lock(e.to_string()))?;

        if self.is_imageproxy {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
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
        let invalid_types = ["", " ", " jpg", "jpg ", "j\0g", "jp\t", "jp\u{00e9}"];
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
        let dir = tempfile::tempdir()?;
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
        let dir = tempfile::tempdir()?;
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
}
