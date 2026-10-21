# Work Order: WO-004A

- **ID**: WO-004A
- **Timestamp**: 2026-10-20T15:59:00-04:00
- **Subagent Role**: Discovery Scout (Artwork & Cache Interfaces)
- **Status**: COMPLETED

## Task Goal
Extract the exact technical specifications, wire protocol, serialization format, SQLite schema, and Perl caller contracts for Tier 1A (`gdresized.pl` / `Slim::Utils::GDResizer`) and `Image::Scale` / `ArtworkCache` so a zero-placeholder Rust conversion plan can be authored.

## Scope & Constraints
- **Target Files**:
  - `/home/console-admin/Github/slimserver/gdresized.pl`
  - `/home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm`
  - `/home/console-admin/Github/slimserver/Slim/Utils/ArtworkCache.pm`
  - `/home/console-admin/Github/slimserver/Slim/Utils/DbArtworkCache.pm`
  - `/home/console-admin/Github/slimserver/Slim/Utils/DbCache.pm`
  - `/home/console-admin/Github/slimserver/Slim/Web/ImageProxy.pm`
  - `/home/console-admin/Github/slimserver/t/gdresizer_explicit_format.t`
- **Deliverables**:
  1. Complete specification of `Slim::Utils::GDResizer::resize` and `gdresize` (all arguments, spec parsing regexes/modes `m`, `p`, `o`, `F`, `c`, format selection, bgcolor hex handling, series of spec resizing, daemon socket invocation in `_Async`, and UNIX socket wire framing).
  2. Exact SQLite database file path, table schema, pragma settings, and value serialization format (check whether `DbArtworkCache` / `DbCache` uses `Storable::nfreeze`, `pack`, or raw columns).
  3. Every caller of `Image::Scale` and `Slim::Utils::GDResizer` across the repository with line numbers.
- **Rules**: Read-only survey of repository code. Append Fulfillment Summary to this file.

## Fulfillment Section

### Fulfillment Summary (`[EXECUTED & VERIFIED]`)

#### 1. `gdresized.pl` Daemon Lifecycle & `/tmp/sbs_artwork` UNIX Socket Wire Protocol
- **Daemon Binary / Script Resolution**:
  - `Slim::Utils::OS->gdresized()` ([Slim/Utils/OS.pm:237-239](file:///home/console-admin/Github/slimserver/Slim/Utils/OS.pm#L237-L239)): returns `"$Bin/gdresized.pl"`.
  - `Slim::Utils::OS::Debian->gdresized()` ([Slim/Utils/OS/Debian.pm:104-106](file:///home/console-admin/Github/slimserver/Slim/Utils/OS/Debian.pm#L104-L106)): returns `"/usr/sbin/squeezeboxserver-resized"`.
  - `Slim::Utils::OS::RedHat->gdresized()` ([Slim/Utils/OS/RedHat.pm:93-95](file:///home/console-admin/Github/slimserver/Slim/Utils/OS/RedHat.pm#L93-L95)): returns `"/usr/libexec/lyrionmusicserver-resized"`.
- **Daemon Spawn & Watchdog ([Slim/Utils/ImageResizer.pm:35-50, 172-251](file:///home/console-admin/Github/slimserver/Slim/Utils/ImageResizer.pm#L35-L50))**:
  - Enabled on non-Windows when server preference `useLocalImageproxy == 2`.
  - Spawned via `Proc::Background->new({ 'die_upon_destroy' => 1 }, $command, @daemonArgs)` where `@daemonArgs` contains `--debug` if `logger('artwork')->is_info` is true (`gdresized.pl` also accepts `--faster`).
  - `hasDaemon()` checks `!main::SCANNER && !main::ISWINDOWS && -r '/tmp/sbs_artwork' && -w _`.
  - Watchdog timer runs every `DAEMON_WATCHDOG_INTERVAL = 6` seconds (`_checkDaemonStatus`).
- **Socket Setup ([gdresized.pl:21, 104-121](file:///home/console-admin/Github/slimserver/gdresized.pl#L104-L121))**:
  - Path: `/tmp/sbs_artwork` (`IO::Socket::UNIX`, `Type => SOCK_STREAM`, `Listen => SOMAXCONN`).
  - Unlinks existing `/tmp/sbs_artwork` before binding, ignores `SIGHUP`, and unlinks `/tmp/sbs_artwork` + `exit 0` on `SIGTERM`, `SIGINT`, `SIGQUIT`.
- **Client -> Daemon Request Frame ([Slim/Utils/ImageResizer.pm:59-132](file:///home/console-admin/Github/slimserver/Slim/Utils/ImageResizer.pm#L59-L132))**:
  - Only used when `!main::ISWINDOWS && hasDaemon() && $callback && !utf8::is_utf8($file)` (note: if `$file` has the Perl UTF-8 flag set or no `$callback` is provided, `ImageResizer::resize` falls back to in-process `sync_resize`).
  - Timeout: `SOCKET_TIMEOUT = 15` seconds; on connect failure or timeout, falls back to in-process `sync_resize`.
  - Frame payload written via `AnyEvent::Handle->push_write`:
    ```perl
    pack('Z* Z* Z* Z* Z*',
        ref $file ? 'data' : $file,
        $specs,
        $cacheroot,
        $cachekey,
        (ref $file ? encode_base64($$file, '') : '')
    ) . "\015\012"
    ```
  - Byte layout on wire: 5 null-terminated (`0x00`) strings back-to-back, terminated by `\r\n` (`0x0D 0x0A`):
    1. `file` (`Z*`): Filesystem path string, or literal `"data"` when resizing in-memory bytes.
    2. `spec` (`Z*`): Single spec or comma-separated list of specs (e.g. `"100x100_o"` or `"200x200_o,100x100_o,64x64_m,50x50_o,41x41_m,40x40_m"`).
    3. `cacheroot` (`Z*`): Directory path containing the SQLite `.db` file (`$cache->getRoot()`).
    4. `cachekey` (`Z*`): Cache key string (if `cachekey =~ /^imageproxy/`, daemon uses `imgproxy.db`; otherwise `artwork.db`).
    5. `data` (`Z*`): Base64-encoded raw image bytes (without newlines) if `file eq "data"`, else empty string `""`.
- **Daemon -> Client Response Frame ([gdresized.pl:128-182](file:///home/console-admin/Github/slimserver/gdresized.pl#L128-L182))**:
  - Daemon reads one line (`my $buf = <$client>;`), unpacks `unpack 'Z* Z* Z* Z* Z*', $buf`, decodes Base64 if `$data` is non-empty, defaults `$spec ||= 'XxX'`, updates cache root (`$cache->setRoot($cacheroot); $cache->pragma('locking_mode = NORMAL');`), splits `@spec = split ',', $spec`, and calls `Slim::Utils::GDResizer->gdresize(...)`.
  - On success: writes `"OK\015\012"` (`"OK\r\n"`) and closes client socket.
  - On exception: writes `"Error: $@\015\012"` and closes client socket.
  - Client `AnyEvent::Handle` detects EOF in `on_error`, cancels timeout timer, and invokes `$callback->(undef, undef, 1)` where the 3rd argument (`$readFromCache = 1`) instructs the caller to fetch the result from SQLite (`$cache->get($cachekey)`).

---

#### 2. `Slim::Utils::GDResizer` Methods & Image Processing Specification
- **`gdresize($class, %args)` ([Slim/Utils/GDResizer.pm:465-558](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L465-L558))**:
  - Arguments: `file` (path or scalar ref), `spec` (arrayref of spec strings), `cache` (`DbArtworkCache`), `cachekey` (string), `debug` (bool), `faster` (bool, passed by `gdresized.pl` though unused inside `GDResizer.pm`).
  - **Multi-spec branch (`@$spec > 1`)**:
    - Parses each element with `^([^x]+)x([^_]+)_(\w)$` into `{ width, height, mode }`.
    - Calls `$class->resizeSeries(file => $file, series => $specs, debug => $debug)`.
    - Writes each returned `[ $resized_ref, $format, $width, $height, $mode ]` to `$cache`:
      - Constructs key by taking `$cachekey` and `$spec = "${width}x${height}_${mode}"`: if `$key =~ s/(\.\w{3,4})$/_$spec$1/` fails, appends `$key .= $spec`.
      - Calls `_cache($cache, $key, $s->[0], $file, $s->[1])`.
  - **Single-spec branch (`@$spec <= 1`)**:
    - Parses `$spec->[0]` with regex:
      `^(?:([0-9X]+)x([0-9X]+))?(?:_(\w))?(?:_([\da-fA-F]+))?(?:\.(\w+))?$`
      capturing `($width, $height, $mode, $bgcolor, $ext)`.
    - Calls `($ref, $format) = $class->resize(file => $file, width => $width, height => $height, mode => $mode, bgcolor => $bgcolor, format => $ext, debug => $debug)`.
    - Clears `$file = undef if ref $file`.
    - If `$cache && $cachekey`, calls `_cache($cache, $cachekey, $ref, $file, $format)`.
    - Returns `($ref, $format)`.
- **`resizeSeries($class, %args)` ([Slim/Utils/GDResizer.pm:255-284](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L255-L284))**:
  - Sorts `@{ delete $args{series} }` descending by width: `sort { $b->{width} <=> $a->{width} }`.
  - For each `{ width, height, mode }` (defaulting `height` to `width`):
    - Calls `($resized_ref, $format) = $class->resize(%args)`.
    - Deletes `$args{file}` after first resize.
    - If `!$args{original} || ($args{width} >= 100 && $args{height} >= 100)`, sets `$args{original} = $resized_ref` so subsequent smaller thumbnails downscale from the previous step's output rather than decoding the full source again.
    - Pushes `[ $resized_ref, $format, $args{width}, $args{height}, $args{mode} ]`.
- **`resize($class, %args)` ([Slim/Utils/GDResizer.pm:33-227](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L33-L227))**:
  - **Inputs**: `original` (scalar ref), `file` (path or scalar ref), `format` (`'png'` or `'jpg'`), `width` (int or `'X'`), `height` (int or `'X'`), `bgcolor` (6- or 8-char hex string), `mode` (`'m'`, `'p'`, `'F'`, `'o'`), `debug`.
  - **Embedded Audio Tag Extraction ([Slim/Utils/GDResizer.pm:60-79, 286-387](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L60-L79))**:
    - Triggered when `$file` is a path, `$file !~ /\.(?:jpe?g|gif|png)$/i`, and `!_content_type_file($file, 0, 1)`.
    - `_read_tag($file)` runs `Audio::Scan->scan_tags($file)` with `AUDIO_SCAN_NO_ARTWORK = 1` to get `(offset, length)` or `(undef, undef, \$artwork)` from:
      1. `APIC` (ID3v2; if multiple, lowest `image_type` `$a->[1] <=> $b->[1]`, offset `$pic->[4]`, length `$pic->[3]`)
      2. `ALLPICTURES` (FLAC/Vorbis; lowest `picture_type`, offset `$pic->{offset}`, length `$pic->{image_data}`)
      3. `COVR` (MP4/M4A/ALAC; `COVR_offset`, `COVR`)
      4. `WM/Picture` (WMA; lowest `image_type`, `offset`, `image`)
      5. `COVER ART (FRONT)` (APE; `COVER ART (FRONT)_offset`, `COVER ART (FRONT)`)
      6. `APPLICATION->{1163084622}` (Escient; strips `'PIC1'` header)
    - If offset is missing or magic bytes at `$offset` fail `_content_type_file($file, $offset, 'silent')` (ID3v2 unsync or Vorbis base64, CPAN Bug 95410), `_read_data_from_tag($file)` re-runs `Audio::Scan->scan_tags($file)` with `AUDIO_SCAN_NO_ARTWORK = 0` to extract raw image bytes into `$origref`.
  - **Magic Bytes Detection (`_content_type`, [Slim/Utils/GDResizer.pm:406-435](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L406-L435))**:
    - `\x89PNG\x0d\x0a\x1a\x0a` -> `'png'`
    - `GIF\d\d[a-z]` -> `'gif'`
    - `\xff\xd8` -> `'jpg'`
    - `BM` -> `'bmp'`
    - Unknown -> dies `"Can't resize unknown type, magic: ..."` (unless `$silent`).
  - **Short-Circuit & Validation ([Slim/Utils/GDResizer.pm:87-101](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L87-L101))**:
    - `'X'` for `width` or `height` becomes `undef`.
    - If `!$width && !$height` and (`!$explicit_format || $explicit_format eq $in_format`): returns unresized bytes and `$in_format`.
    - If `($width && $width !~ /^\d+$/) || ($height && $height !~ /^\d+$/)`: returns unresized bytes and `$in_format`.
  - **Format & Padding Rules ([Slim/Utils/GDResizer.pm:104-150](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L104-L150))**:
    - If `$bgcolor && length($bgcolor) != 6 && length($bgcolor) != 8`, resets `$bgcolor = 0xFFFFFF`.
    - Default `$mode = 'm'`.
    - Default `$format`: `'jpg'` if `$in_format eq 'jpg'`, else `'png'`.
    - In modes `'m'` or `'p'`: if `!$explicit_format && $format ne 'png'` and target aspect ratio differs from source aspect ratio (`($in_height / $in_width) != ($height / $width)` or reciprocal), switches `$format = 'png'` for transparent padding (Bug 17140). If `$explicit_format` was explicitly passed, keeps `$explicit_format` (tested in [t/gdresizer_explicit_format.t](file:///home/console-admin/Github/slimserver/t/gdresizer_explicit_format.t)).
  - **Resize Modes ([Slim/Utils/GDResizer.pm:140-211](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L140-L211))**:
    - `m` (max) and `p` (pad): Clamps target dimensions if both `$width > $in_width && $height > $in_height` to avoid upscaling while preserving target aspect ratio; calls `$im->resize({ width => $width, height => $height, bgcolor => hex($bgcolor), keep_aspect => 1 })`.
    - `F` (fit): If `$width >= $in_width && $height >= $in_height`, calls `$im->resize({ width => $in_width, height => $in_height, bgcolor => hex($bgcolor), keep_aspect => 1 })`; else resizes to `$width x $height` with `keep_aspect => 1`.
    - `o` (or any other mode character such as `c`): Ignores height during resize (scales proportionally by width only, no padding). Clamps `$width` if `$width > $in_width && $height > $in_height`; calls `$im->resize({ width => $width })`.
  - **Output**:
    - `$format eq 'png'` -> `(\$im->as_png, 'png')`
    - Otherwise -> `(\$im->as_jpeg, 'jpg')` (JPEG quality defaults to `90` in `Image::Scale`).
- **`getSize($class, $ref)` ([Slim/Utils/GDResizer.pm:229-236](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm#L229-L236))**:
  - Returns `($im->width, $im->height)` via `Image::Scale->new($ref)`, or `(0, 0)` on error.

---

#### 3. SQLite Database Files, Schema, Pragmas, Key Hashing & Binary Value Layout
- **Class Hierarchy**:
  - `Slim::Utils::ArtworkCache` ([Slim/Utils/ArtworkCache.pm:1-30](file:///home/console-admin/Github/slimserver/Slim/Utils/ArtworkCache.pm#L1-L30)) is a factory returning a singleton `Slim::Utils::DbArtworkCache->new($root, 'artwork')`.
  - `Slim::Utils::DbArtworkCache` ([Slim/Utils/ArtworkCache.pm:32-136](file:///home/console-admin/Github/slimserver/Slim/Utils/ArtworkCache.pm#L32-L136); no separate `DbArtworkCache.pm` file exists) inherits from `Slim::Utils::DbCache` ([Slim/Utils/DbCache.pm](file:///home/console-admin/Github/slimserver/Slim/Utils/DbCache.pm)).
  - `Slim::Web::ImageProxy::Cache` ([Slim/Web/ImageProxy.pm:539-598](file:///home/console-admin/Github/slimserver/Slim/Web/ImageProxy.pm#L539-L598) and [gdresized.pl:186-198](file:///home/console-admin/Github/slimserver/gdresized.pl#L186-L198)) inherits from `Slim::Utils::DbArtworkCache` with `namespace = 'imgproxy'` and `expires = 86400 * 30` (30 days).
- **Database File Locations & Schemas**:
  1. **Local Artwork Cache (`<librarycachedir>/artwork.db`)**:
     - `namespace = 'artwork'`, `noexpiry = 1`.
     - Schema: `CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB)`
  2. **ImageProxy Cache (`<cachedir>/imgproxy.db`)**:
     - `namespace = 'imgproxy'`, `noexpiry = 0`, `default_expires_in = 2592000` (30 days; stored in `t` as Unix epoch `time() + 2592000`).
     - Schema:
       ```sql
       CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB, t INTEGER);
       CREATE INDEX IF NOT EXISTS expiry ON cache (t);
       ```
- **SQLite Connection & Pragmas ([Slim/Utils/DbCache.pm:258-300](file:///home/console-admin/Github/slimserver/Slim/Utils/DbCache.pm#L258-L300))**:
  - `PRAGMA auto_vacuum = FULL` (when newly created)
  - `PRAGMA synchronous = OFF`
  - `PRAGMA journal_mode = WAL`
  - `PRAGMA wal_autocheckpoint = 200` (`10000` in `scanner.pl`)
  - `PRAGMA journal_size_limit = <dbjournalsize * 1048576>` (if preference set)
  - `PRAGMA cache_size = 20000; PRAGMA temp_store = MEMORY` (if `dbhighmem`) else `PRAGMA cache_size = 300`
  - `PRAGMA locking_mode = NORMAL` (set by `gdresized.pl` when switching `cacheroot`)
- **Primary Key (`k`) Calculation ([Slim/Utils/DbCache.pm:156-165](file:///home/console-admin/Github/slimserver/Slim/Utils/DbCache.pm#L156-L165))**:
  - Given `$key` (UTF-8 encoded bytes), computes `$md5 = Digest::MD5::md5_hex($key)`.
  - Concatenates the decimal strings of the first 8 hex digits (32-bit unsigned int) and the next 7 hex digits (28-bit unsigned int):
    `$id = hex(substr($md5, 0, 8)) . hex(substr($md5, 8, 7));`
  - Because `max(hex(8 digits)) = 4294967295` and `max(hex(7 digits)) = 268435455`, the concatenated decimal string is at most `4294967295268435455` (`< 2^63 - 1 = 9223372036854775807`), so SQLite always stores `k` as a positive 64-bit signed integer (`INTEGER PRIMARY KEY` / rowid).
- **Blob Value (`v`) Binary Serialization ([Slim/Utils/ArtworkCache.pm:63-114](file:///home/console-admin/Github/slimserver/Slim/Utils/ArtworkCache.pm#L63-L114))**:
  - **Does NOT use `Storable::freeze` / `nfreeze`**. `DbArtworkCache::set` passes a scalar (not a ref) to `DbCache::set`, bypassing `freeze()`.
  - `v` is a raw `SQL_BLOB` with a fixed 9-byte header (`pack('A3LS', ...)`) followed by variable-length `original_path` and raw image bytes:
    | Offset | Size | Perl `pack` | Rust Type (x86_64/ARM LE) | Description |
    | :--- | :--- | :--- | :--- | :--- |
    | `0..2` | 3 bytes | `A3` | `[u8; 3]` | `content_type` ASCII space-padded (e.g. `b"jpg"`, `b"png"`, `b"gif"`, or `b"   "`) |
    | `3..6` | 4 bytes | `L` | `u32` (native/LE) | `mtime` of source file (`(stat($file))[9]`, or `0` if from memory/proxy) |
    | `7..8` | 2 bytes | `S` | `u16` (native/LE) | `pathlen` — byte length of UTF-8 `original_path` (`0` if `undef`/empty) |
    | `9..9+pathlen` | `pathlen` bytes | — | `[u8; pathlen]` | `original_path` raw UTF-8 bytes |
    | `9+pathlen..` | remaining | — | `&[u8]` | Raw encoded image bytes (JPEG/PNG) |

---

#### 4. Complete Inventory of `Image::Scale`, `Slim::Utils::GDResizer`, and `Slim::Utils::ImageResizer` Callers
- **Direct Callers of `Image::Scale`**:
  1. [Slim/Utils/GDResizer.pm](file:///home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm):
     - Line 5: `use Image::Scale;`
     - Lines 117–119: `Image::Scale->new($file, { offset => $offset, length => $length })` / `Image::Scale->new($origref)`
     - Line 121: `($im->width, $im->height)`
     - Lines 166, 178, 187, 208: `$im->resize(...)`
     - Lines 218, 222: `$im->as_png`, `$im->as_jpeg`
     - Line 233: `Image::Scale->new($ref)` in `getSize()`
  2. [Slim/Web/ImageProxy.pm](file:///home/console-admin/Github/slimserver/Slim/Web/ImageProxy.pm):
     - Line 78: `use Image::Scale;`
     - Line 96: `Image::Scale->gif_version()` (used at compile time to build `ACCEPT_IMAGE_FORMATS`)
  3. [Slim/bootstrap.pm](file:///home/console-admin/Github/slimserver/Slim/bootstrap.pm) & [modules.conf](file:///home/console-admin/Github/slimserver/modules.conf):
     - `modules.conf:22`: `Image::Scale 0.08` (checked at server startup by `Slim::bootstrap::check_valid_versions()`, skipped when `main::SCANNER || main::RESIZER`).
  4. [t/gdresizer_explicit_format.t](file:///home/console-admin/Github/slimserver/t/gdresizer_explicit_format.t):
     - Line 12: `eval { require Image::Scale; 1 } or plan skip_all => 'Image::Scale not available';`
- **Direct Callers of `Slim::Utils::GDResizer`**:
  1. [gdresized.pl](file:///home/console-admin/Github/slimserver/gdresized.pl):
     - Line 91: `use Slim::Utils::GDResizer;`
     - Line 163: `Slim::Utils::GDResizer->gdresize(...)`
  2. [Slim/Utils/ImageResizer.pm](file:///home/console-admin/Github/slimserver/Slim/Utils/ImageResizer.pm):
     - Line 145: `require Slim::Utils::GDResizer;`
     - Line 153: `Slim::Utils::GDResizer->gdresize(...)` (inside `sync_resize`)
  3. [Slim/Web/Graphics.pm](file:///home/console-admin/Github/slimserver/Slim/Web/Graphics.pm):
     - Lines 225, 229: `Slim::Utils::GDResizer->resize(...)` (in-memory coverArt for negative remoteTrack IDs)
  4. [Slim/Web/ImageProxy.pm](file:///home/console-admin/Github/slimserver/Slim/Web/ImageProxy.pm):
     - Lines 262–263: `Slim::Utils::GDResizer::_content_type($http->contentRef, 1)` (magic-byte sniffing when HTTP response Content-Type is `text/*`)
     - Lines 416–422: `Slim::Utils::GDResizer->resize(...)` (resizing fallback `html/images/radio.png` in `_artworkError`)
  5. [Slim/Plugin/JiveExtras/Settings.pm](file:///home/console-admin/Github/slimserver/Slim/Plugin/JiveExtras/Settings.pm):
     - Lines 171, 173: `Slim::Utils::GDResizer->getSize(\$content)`
  6. [t/gdresizer_explicit_format.t](file:///home/console-admin/Github/slimserver/t/gdresizer_explicit_format.t):
     - Lines 15, 20: `Slim::Utils::GDResizer->resize(...)`
- **Direct Callers of `Slim::Utils::ImageResizer`**:
  1. [Slim/Web/Graphics.pm:37, 470](file:///home/console-admin/Github/slimserver/Slim/Web/Graphics.pm#L470): `initDaemon()`, `resize($cover, $path, $spec, sub { ... })`
  2. [Slim/Web/ImageProxy.pm:108, 365](file:///home/console-admin/Github/slimserver/Slim/Web/ImageProxy.pm#L365): `initDaemon()`, `resize($fullpath, $cachekey, $spec, sub { ... }, $cache)`
  3. [Slim/Music/Artwork.pm:718, 803](file:///home/console-admin/Github/slimserver/Slim/Music/Artwork.pm#L803): `resize($path, "music/$coverid/cover_", join(',', @specs), $finished)`
  4. [Slim/Music/ContributorPictureScan.pm:56, 251](file:///home/console-admin/Github/slimserver/Slim/Music/ContributorPictureScan.pm#L251): `resize($img, "contributor/$imgId/image_", $specs)`
  5. [slimserver.pl:1066](file:///home/console-admin/Github/slimserver/slimserver.pl#L1066): `Slim::Utils::ImageResizer->stopDaemon() if $INC{'Slim/Utils/ImageResizer.pm'};`
