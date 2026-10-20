# Lyrion Music Server (`slimserver`) Incremental Rust Conversion Design Specification

- **Branch**: `Rust-Experiment`
- **Target Repository**: `/home/console-admin/Github/slimserver` (`v9.2.0`)
- **Host Platform**: Debian GNU/Linux 13 (`trixie`, `x86_64`), Perl `5.40.1` (`x86_64-linux-gnu-thread-multi`), Rust `1.99.1`

---

## 1. Intent, Goals, Non-Goals & Anti-Goals

### Intent & Why
Lyrion Music Server (`slimserver`) currently relies on:
1. Pre-compiled C/C++ Perl XS `.so` binaries (`CPAN/arch/<perl_version>/<arch>/`) that must be rebuilt for every Perl minor release (`5.10`–`5.42`) across every CPU architecture (`x86_64`, `aarch64`, `armhf`, `i386`, `riscv64`, `powerpc`).
2. Aging C/C++ CLI binaries in `Bin/x86_64-linux/` (`faad`, `flac`, `mac`, `sox`, `wvunpack`) and a second Perl interpreter daemon (`gdresized.pl`) for out-of-process artwork resizing.
3. Custom LMS-maintained C parsers (`Image::Scale`, `MP3::Cut::Gapless`, `Audio::Scan`) that parse untrusted user media headers and embedded cover art using raw C pointer arithmetic.

Converting the native tool stack to a single unified Cargo workspace (`/home/console-admin/Github/slimserver/rust/`) ordered strictly by ROI eliminates C memory-safety vulnerabilities on untrusted media files, replaces heavy secondary Perl/C helper processes with lean static Rust binaries, and replaces per-Perl-version XS compilation with stable C-ABI / IPC boundaries.

### Goals
1. **ROI Tier 1A (`lms-gdresized` & `lms-image-core`)**: Drop-in Rust replacement for `gdresized.pl` listening on `/tmp/sbs_artwork` (`SOCK_STREAM`) with 100% wire-protocol compatibility (`pack('Z* Z* Z* Z* Z*')`), exact `Slim::Utils::GDResizer` resizing semantics (`m`, `p`, `o`, `F` modes, Bug 17140 PNG transparency fallback, explicit format override, descending series chaining), embedded audio tag artwork extraction, and bit-exact SQLite `artwork.db` / `imgproxy.db` compatibility (`Slim::Utils::DbCache::_key` MD5 decimal-concatenation key and `pack('A3LS', ...)` 9-byte blob header).
2. **ROI Tier 1B (`lms-transcode` CLI Transcoder)**: Drop-in Rust CLI replacement for the bundled `Bin/x86_64-linux/` audio transcoders (`faad`, `flac`, `wvunpack`, `sox`) supporting multi-call (`argv[0]`) and subcommand dispatch, `convert.conf` flag parity (`-q`, `-w`, `-f 1|2`, `-b 1`, `-j %s`, `-e %u`, `-d`, `-c`, `-s`, `--totally-silent`, `--force-raw-format`, `--endian=little|big`, `--sign=signed`, `--skip=%t`, `--until=%v`, `--compression-level-0`, `--ignore-chunk-sizes`), and streaming `stdin`/`stdout` PCM/WAV/FLAC/MP3 pipelines.
3. **ROI Tier 1C (`lms-mp3cut-ffi` & `lms-audioscan-ffi` / Perl Bridges)**:
   - Pure-Rust implementation of `MP3::Cut::Gapless` (`lms-mp3cut`), parsing LAME Xing/Info headers (`enc_delay`, `enc_padding`), caching `.mllt` frame indices in `<librarycachedir>/mp3cut/<md5>.mllt`, performing sample-accurate bit-reservoir-aware MP3 frame cutting, and exposing a C-ABI / Perl wrapper compatible with `Slim/Formats/Playlists/CUE.pm:781-797` and `Slim/Player/Protocols/File.pm:208-258`.
   - Pure-Rust metadata & frame-offset scanner (`lms-audioscan`) supporting `scan`, `scan_tags`, `scan_fh`, `find_frame_fh`, `find_frame_fh_return_info`, and `$ENV{AUDIO_SCAN_NO_ARTWORK}` semantics (`1` = integer byte length + offset; `0` = raw artwork bytes) across MP3/ID3v2, FLAC/OggFLAC, Ogg Vorbis, Opus, MP4/M4A/ALAC, WAV, AIFF, ASF/WMA, APEv2/WavPack/Musepack, and DSF/DFF.
4. **ROI Tier 2 (`lms-scanner-fast` Pre-Indexer & Artwork Pre-Cacher)**: Parallel Rust directory walker (`find`) and batch tag/artwork pre-cacher that populates `scanned_files (url, timestamp, filesize)` in `<librarycachedir>/library.db` and pre-renders uncached cover art in `<librarycachedir>/artwork.db` while preserving `scanner.pl` plugin importer hooks.

### Non-Goals & Anti-Goals
- **Anti-Goal 1**: Rewriting `slimserver.pl`, `Slim::Schema` (`DBIx::Class`), or the 50+ Perl plugins in `Slim/Plugin/*` in Rust. Breaking existing third-party Perl plugins is an immediate failure.
- **Anti-Goal 2**: Replacing upstream-maintained core Perl XS bindings (`EV`, `DBD::SQLite`, `DBI`, `JSON::XS`, `YAML::XS`, `HTML::Parser`, `XML::Parser::Expat`).
- **Anti-Goal 3**: Breaking fallback compatibility. Every Perl integration point (`Slim/Utils/OS.pm`, `Slim/Utils/GDResizer.pm`, `CPAN/MP3/Cut/Gapless.pm`, `Slim/Utils/Scanner/Local.pm`) must support graceful fallback or environment toggle (`LMS_USE_RUST=1` default enabled when compiled Rust binaries exist) so existing Perl tests and CI smoketests (`t/verifyStrings.pl`, `t/00_smoketest.sh`, `t/gdresizer_explicit_format.t`) always pass.

---

## 2. Exact Technical Contracts & Invariants

### 2.1 Cargo Workspace Topology (`/home/console-admin/Github/slimserver/rust/Cargo.toml`)
- `rust/crates/lms-cache`: SQLite `DbCache` / `DbArtworkCache` key hashing (`md5_key_i64`) and `pack('A3LS', ...)` binary blob serializer/deserializer using `rusqlite` (`bundled`) and `md-5`.
- `rust/crates/lms-image`: Pure-Rust image sniffer, resizer (`m`, `p`, `o`, `F` modes, Bug 17140 transparency rule, explicit format rule, `resize_series`), and embedded audio tag artwork extractor (`ID3v2 APIC`, `FLAC PICTURE`, `MP4 covr`, `ASF WM/Picture`, `APEv2 COVER ART (FRONT)`).
- `rust/crates/lms-gdresized`: Binary crate producing `/home/console-admin/Github/slimserver/Bin/x86_64-linux/gdresized` (and invoked by `Slim/Utils/OS.pm` / `gdresized.pl`), implementing the `/tmp/sbs_artwork` UNIX stream socket server and `--cli-resize` one-shot CLI mode.
- `rust/crates/lms-mp3cut`: Pure-Rust gapless MP3 frame indexer, `.mllt` cache reader/writer, LAME Xing/Info parser, bit-reservoir back-pointer splicer, and `cdylib` C-ABI (`liblms_mp3cut.so`) + CLI helper.
- `rust/crates/lms-audioscan`: Pure-Rust audio tag & stream header parser (`scan`, `scan_tags`, `scan_fh`, `find_frame_fh`, `find_frame_fh_return_info`) with `AUDIO_SCAN_NO_ARTWORK` offset/length contract and JSON/C-ABI bridge (`liblms_audioscan.so` / `lms-audioscan`).
- `rust/crates/lms-transcode`: Binary crate producing `/home/console-admin/Github/slimserver/Bin/x86_64-linux/lms-transcode` supporting `--mode faad|flac|wvunpack|sox` and argv[0] multicall dispatch for `convert.conf` pipelines.
- `rust/crates/lms-scanner-accel`: Parallel filesystem walker (`scanned_files` table bulk inserter) and multi-threaded artwork pre-cacher (`artwork.db` bulk writer) invoked by `Slim/Utils/Scanner/Local.pm` and `Slim/Music/Artwork.pm`.

### 2.2 SQLite `DbCache` Key & `DbArtworkCache` Blob Format Invariants
- **`md5_key_i64(key: &str) -> i64`** (`Slim/Utils/DbCache.pm:156-165`):
  - Compute 32-character lowercase hex MD5 of UTF-8 `key` bytes: `md5_hex`.
  - Parse `hi = u32::from_str_radix(&md5_hex[0..8], 16)` and `lo = u32::from_str_radix(&md5_hex[8..15], 16)`.
  - Format decimal string `format!("{}{}", hi, lo)` and parse as `i64`.
- **`encode_artwork_blob(content_type: &str, mtime: u32, original_path: &str, image_bytes: &[u8]) -> Vec<u8>`** (`Slim/Utils/ArtworkCache.pm:63-114`):
  - `content_type`: 3-byte right-space-padded ASCII (`b"jpg"`, `b"png"`, `b"gif"`, or `b"   "`).
  - `mtime`: 4-byte little-endian `u32` (`mtime.to_le_bytes()`).
  - `path_len`: 2-byte little-endian `u16` (`(path_bytes.len() as u16).to_le_bytes()`).
  - Followed by `path_bytes` (`original_path.as_bytes()`) and `image_bytes`.
- **Database Tables**:
  - `artwork.db` (when `!cachekey.starts_with("imageproxy")`):
    `CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB);`
    `INSERT OR REPLACE INTO cache (k, v) VALUES (?1, ?2);`
  - `imgproxy.db` (when `cachekey.starts_with("imageproxy")`):
    `CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB, t INTEGER); CREATE INDEX IF NOT EXISTS expiry ON cache (t);`
    `INSERT OR REPLACE INTO cache (k, v, t) VALUES (?1, ?2, ?3);` where `t = unix_epoch_secs + 2592000`.

### 2.3 `/tmp/sbs_artwork` Wire Frame Contract
- Input stream: 5 null-terminated (`\0`) fields followed by `\r\n`:
  `file\0spec\0cacheroot\0cachekey\0base64_data\0\r\n`
- When `file == "data"`, decode `base64_data` (ignoring whitespace/newlines) as raw input bytes and record `original_path = ""` and `mtime = 0`.
- When `spec` is empty, default to `"XxX"`.
- Output stream: `"OK\r\n"` on success, or `"Error: <message>\r\n"` on failure.
