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

/// Computes the 64-bit integer cache key identical to Perl `Slim::Utils::DbCache::_key`.
///
/// In Perl:
/// ```perl
/// my $md5 = Digest::MD5::md5_hex($key);
/// return hex( substr($md5, 0, 8) ) . hex( substr($md5, 8, 7) );
/// ```
/// Leading zero nibbles in `substr($md5, 8, 7)` evaluate as numbers without leading zeroes,
/// and the two decimal representations are concatenated as strings before being parsed as a 64-bit signed int.
pub fn md5_key_i64(key: &str) -> i64 {
    let digest = Md5::digest(key.as_bytes());
    let hex = format!("{:032x}", digest);
    let hi = u32::from_str_radix(&hex[0..8], 16).unwrap_or(0);
    let lo = u32::from_str_radix(&hex[8..15], 16).unwrap_or(0);
    let decimal_concat = format!("{}{}", hi, lo);
    decimal_concat.parse::<i64>().unwrap_or(0)
}

/// Encodes an `ArtworkRecord` into a binary blob matching Perl `Slim::Utils::ArtworkCache` `pack('A3LS', ...)`.
///
/// Binary layout:
/// - `[0..3]`: 3-byte space-padded ASCII `content_type` (`pack('A3')`)
/// - `[3..7]`: 4-byte little-endian `mtime` (`pack('L')`)
/// - `[7..9]`: 2-byte little-endian length of original file path (`pack('S')`)
/// - `[9..9+path_len]`: original file path UTF-8 bytes
/// - `[9+path_len..]`: raw image data payload
pub fn encode_artwork_blob(record: &ArtworkRecord) -> Vec<u8> {
    let mut ct = [b' '; 3];
    let ct_bytes = record.content_type.as_bytes();
    let copy_len = ct_bytes.len().min(3);
    ct[..copy_len].copy_from_slice(&ct_bytes[..copy_len]);

    let path_bytes = record.original_path.as_bytes();
    let path_len = path_bytes.len().min(u16::MAX as usize) as u16;

    let mut buf = Vec::with_capacity(9 + path_bytes.len() + record.data.len());
    buf.extend_from_slice(&ct);
    buf.extend_from_slice(&record.mtime.to_le_bytes());
    buf.extend_from_slice(&path_len.to_le_bytes());
    buf.extend_from_slice(&path_bytes[..path_len as usize]);
    buf.extend_from_slice(&record.data);
    buf
}

/// Decodes an `ArtworkRecord` from a binary blob packed by `encode_artwork_blob` or Perl `pack('A3LS', ...)`.
pub fn decode_artwork_blob(blob: &[u8]) -> Result<ArtworkRecord, CacheError> {
    if blob.len() < 9 {
        return Err(CacheError::InvalidBlob(format!(
            "blob length {} is less than minimum 9-byte header",
            blob.len()
        )));
    }

    let raw_ct = std::str::from_utf8(&blob[0..3])
        .map_err(|e| CacheError::InvalidBlob(format!("invalid UTF-8 in content_type: {e}")))?;
    let content_type = raw_ct.trim_end_matches([' ', '\0']).to_string();

    let mtime_bytes: [u8; 4] = blob[3..7]
        .try_into()
        .map_err(|_| CacheError::InvalidBlob("failed to read mtime bytes".to_string()))?;
    let mtime = u32::from_le_bytes(mtime_bytes);

    let path_len_bytes: [u8; 2] = blob[7..9]
        .try_into()
        .map_err(|_| CacheError::InvalidBlob("failed to read path_len bytes".to_string()))?;
    let path_len = u16::from_le_bytes(path_len_bytes) as usize;

    if blob.len() < 9 + path_len {
        return Err(CacheError::InvalidBlob(format!(
            "blob length {} is less than header (9) + path_len ({path_len})",
            blob.len()
        )));
    }

    let original_path = std::str::from_utf8(&blob[9..9 + path_len])
        .map_err(|e| CacheError::InvalidBlob(format!("invalid UTF-8 in original_path: {e}")))?
        .to_string();

    let data = blob[9 + path_len..].to_vec();

    Ok(ArtworkRecord {
        content_type,
        mtime,
        original_path,
        data,
    })
}

/// SQLite artwork database store providing bit-exact compatibility with Perl LMS `DbArtworkCache` / `DbCache`.
pub struct DbArtworkStore {
    conn: std::sync::Mutex<rusqlite::Connection>,
    is_imageproxy: bool,
}

impl DbArtworkStore {
    pub fn open(cache_root: &Path, is_imageproxy: bool) -> Result<Self, CacheError> {
        std::fs::create_dir_all(cache_root)?;
        let db_filename = if is_imageproxy { "imgproxy.db" } else { "artwork.db" };
        let db_path = cache_root.join(db_filename);
        let conn = rusqlite::Connection::open(db_path)?;

        conn.execute_batch(
            "PRAGMA locking_mode = NORMAL;
             PRAGMA synchronous = OFF;
             PRAGMA journal_mode = WAL;",
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
        let blob = encode_artwork_blob(record);
        let conn = self.conn.lock().map_err(|e| CacheError::Lock(e.to_string()))?;

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
        let conn = self.conn.lock().map_err(|e| CacheError::Lock(e.to_string()))?;

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

    #[test]
    fn test_dbcache_key_matches_perl_leading_zero_nibbles() {
        // 1. Key with leading zero in nibbles 8..15:
        // perl -MDigest::MD5=md5_hex -e '$m=md5_hex("test/21"); print "$m -> ".hex(substr($m,0,8)).hex(substr($m,8,7))."\n"'
        // md5_hex("test/21") = "72e64b0f018e085acec8285c35c36e44"
        // hex("72e64b0f") = 1927695119, hex("018e085") = 1630341 -> "19276951191630341"
        assert_eq!(md5_key_i64("test/21"), 19276951191630341_i64);

        // 2. Standard artwork cache key:
        // md5_hex("music/12345/cover_100x100_o") = "c78c3d3e2923430b16a433d8de85427c"
        // hex("c78c3d3e") = 3347856702, hex("2923430") = 43136048 -> "334785670243136048"
        assert_eq!(md5_key_i64("music/12345/cover_100x100_o"), 334785670243136048_i64);
    }

    #[test]
    fn test_artwork_blob_roundtrip_and_a3ls_header_layout() {
        let rec = ArtworkRecord {
            content_type: "jpg".to_string(),
            mtime: 1700000000,
            original_path: "/music/test.mp3".to_string(),
            data: vec![0xFF, 0xD8, 0xFF, 0xE0],
        };
        let encoded = encode_artwork_blob(&rec);
        assert_eq!(&encoded[0..3], b"jpg");
        assert_eq!(u32::from_le_bytes(encoded[3..7].try_into().unwrap()), 1700000000);
        assert_eq!(u16::from_le_bytes(encoded[7..9].try_into().unwrap()), 15);
        let decoded = decode_artwork_blob(&encoded).unwrap();
        assert_eq!(decoded, rec);
    }

    #[test]
    fn test_sqlite_artwork_and_imgproxy_tables() {
        let dir = tempfile::tempdir().unwrap();
        let store = DbArtworkStore::open(dir.path(), false).unwrap();
        let rec = ArtworkRecord {
            content_type: "png".to_string(),
            mtime: 0,
            original_path: String::new(),
            data: vec![0x89, b'P', b'N', b'G'],
        };
        store.set("music/abc/cover_50x50_o", &rec).unwrap();
        let loaded = store.get("music/abc/cover_50x50_o").unwrap().unwrap();
        assert_eq!(loaded, rec);

        // Also test imgproxy.db table
        let store_proxy = DbArtworkStore::open(dir.path(), true).unwrap();
        let rec_proxy = ArtworkRecord {
            content_type: "jpg".to_string(),
            mtime: 12345678,
            original_path: "/proxy/image.jpg".to_string(),
            data: vec![1, 2, 3, 4],
        };
        store_proxy.set("proxy/test_key", &rec_proxy).unwrap();
        let loaded_proxy = store_proxy.get("proxy/test_key").unwrap().unwrap();
        assert_eq!(loaded_proxy, rec_proxy);
    }

    #[test]
    fn test_artwork_blob_error_handling() {
        // Truncated blob (< 9 bytes)
        assert!(decode_artwork_blob(&[1, 2, 3]).is_err());

        // Blob shorter than indicated path length
        let mut malformed = vec![b'j', b'p', b'g', 0, 0, 0, 0, 50, 0]; // path_len = 50, but blob has 9 bytes
        assert!(decode_artwork_blob(&malformed).is_err());
        malformed.extend_from_slice(b"short");
        assert!(decode_artwork_blob(&malformed).is_err());
    }
}
