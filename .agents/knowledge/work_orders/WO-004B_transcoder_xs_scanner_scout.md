# Work Order: WO-004B

- **ID**: WO-004B
- **Timestamp**: 2026-10-20T15:59:00-04:00
- **Subagent Role**: Discovery Scout (Transcoders, XS Media & Scanner Interfaces)
- **Status**: COMPLETED [EXECUTED & VERIFIED]

## Task Goal
Extract the exact technical specifications, CLI flags, Perl caller contracts, and SQLite schemas for Tier 1B (CLI transcoders in `convert.conf`), Tier 1C (`MP3::Cut::Gapless` and `Audio::Scan`), and Tier 2 (`scanner.pl` / `Slim::Utils::Scanner::Local`) so a zero-placeholder Rust conversion plan can be authored.

## Scope & Constraints
- **Target Files**:
  - `/home/console-admin/Github/slimserver/convert.conf`
  - `/home/console-admin/Github/slimserver/types.conf`
  - `/home/console-admin/Github/slimserver/Slim/Player/TranscodingHelper.pm`
  - `/home/console-admin/Github/slimserver/Slim/Player/Song.pm`
  - `/home/console-admin/Github/slimserver/scanner.pl`
  - `/home/console-admin/Github/slimserver/Slim/Utils/Scanner/Local.pm`
  - `/home/console-admin/Github/slimserver/Slim/Formats.pm` and `/home/console-admin/Github/slimserver/Slim/Formats/`
- **Deliverables**:
  1. Every CLI invocation pattern in `convert.conf` for `[faad]`, `[flac]`, `[sox]`, `[wvunpack]`, `[mac]` (exact flags, stdin/stdout behavior, seek/trim flags).
  2. Every caller and method signature of `MP3::Cut::Gapless` across the codebase (exact constructor args, methods, return values, and how `Slim::Player::Song` or `Slim::Formats::MP3` uses it).
  3. Every caller and method signature of `Audio::Scan` across the codebase (`scan`, `scan_data`, `scan_fh`, `find_frame`, `find_frame_fh`, etc., and the exact Perl hash keys expected in `info` and `tags`).
  4. Architecture and execution pipeline of `scanner.pl` and `Slim::Utils::Scanner::Local`.
- **Rules**: Read-only survey of repository code. Append Fulfillment Summary to this file.

## Fulfillment Section

### Status: `[EXECUTED & VERIFIED]`

---

### Deliverable 1: Tier 1B — CLI Transcoders (`convert.conf`, `types.conf`, `Slim::Player::TranscodingHelper`, `Bin/x86_64-linux/`)

#### 1.1 Bundled Binaries & Resolution Order
- **Bundled Binaries in `/home/console-admin/Github/slimserver/Bin/x86_64-linux/`**:
  - `/home/console-admin/Github/slimserver/Bin/x86_64-linux/faad` (380,472 bytes)
  - `/home/console-admin/Github/slimserver/Bin/x86_64-linux/flac` (415,800 bytes)
  - `/home/console-admin/Github/slimserver/Bin/x86_64-linux/mac` (1,554,696 bytes)
  - `/home/console-admin/Github/slimserver/Bin/x86_64-linux/sox` (2,209,040 bytes)
  - `/home/console-admin/Github/slimserver/Bin/x86_64-linux/wvunpack` (154,688 bytes)
- **Search Path Resolution (`Slim::Utils::OS::initSearchPath` & `Slim::Utils::Misc::findbin`)**:
  - `Slim::Utils::OS::initSearchPath` (`/home/console-admin/Github/slimserver/Slim/Utils/OS.pm:85-132`): Sets `$baseDir = $class->dirsFor('Bin')` (`/home/console-admin/Github/slimserver/Bin`). On `x86_64-linux`, unshifts `"/home/console-admin/Github/slimserver/Bin/x86_64-linux"` to the front of `@paths` (`OS.pm:118-120`), followed by `Bin/i386-linux`, `Bin/linux`, `Bin`, and then system `$ENV{PATH}` and `/usr/bin`, `/usr/local/bin`, `/usr/libexec`, `/sw/bin`, `/usr/sbin`, `/opt/bin` (`Slim/Utils/OS/Unix.pm:31-39`).
  - `Slim::Utils::Misc::findbin($executable)` (`/home/console-admin/Github/slimserver/Slim/Utils/Misc.pm:108-145`): Iterates `@findBinPaths` and returns the first path passing `-x`. Thus `/home/console-admin/Github/slimserver/Bin/x86_64-linux/<binary>` always shadows system `$PATH`.
- **Command Tokenization & Variable Substitution (`Slim::Player::TranscodingHelper`)**:
  - `loadConversionTables()` (`TranscodingHelper.pm:51-135`): Parses `/home/console-admin/Github/slimserver/convert.conf`. Profile key is `"$inputtype-$outputtype-$clienttype-$clientid"`. Capabilities default to `{I => 'noArgs', F => 'noArgs'}` if no `# <caps>` comment is present (`L222`).
  - `checkBin($profile, $ignoreprefsettings)` (`L263-307`): Extracts all `[binary]` tokens in `$commandTable{$profile}` and resolves each via `Slim::Utils::Misc::findbin($1)`.
  - `tokenizeConvertCommand2($transcoder, $filepath, $fullpath, $noPipe, $quality)` (`L519-673`):
    - Replaces `[bin]` with `'"' . $binaries{$1} . '"'` (`L530-533`).
    - Extracts cuesheet anchor `#start-end` from `$fullpath` (`L542-544`) and adds `$transcoder->{'start'}` (populated from `$seekdata->{timeOffset}` in `Slim/Player/Song.pm:439-441`).
    - Capabilities & Substitutions (`L551-633`):
      - `I`: Input from stdin (`$FILE$` -> `"-"`)
      - `F`: Input from file (`$FILE$` -> `"\"$filepath\""`)
      - `R`: Remote URL (`$URL$` -> `"\"$fullpath\""`)
      - `T`: Start time seek (`%s` = seconds int/float; `%t` = `mm:ss.xx` via `Slim::Utils::DateTime::fracSecToMinSec($start)`)
      - `U`: End time trim (`%u` = seconds int/float; `%v` = `mm:ss.xx`; `%w` = duration `$end - $start` in seconds)
      - `D`: Resample/rate limit (`%d` = `samplerateLimit` in Hz; `%D` = `samplerateLimit / 1000` in kHz)
      - `B`: Bitrate limit (`%b` = `rateLimit * 1000` in bps; `%B` = `rateLimit` in kbps)
      - `E`: Extensions such as `NOSTART=I` (disables `T` when `streamMode eq 'I'`), `SAMPLERATE=%d`
      - Stream parameters: `$SAMPLESIZE$`, `$SAMPLERATE$`, `$CHANNELS$`, `$OCHANNELS$`, `$QUALITY$`, `$-x$` (`""` on big-endian or `"-x"` on little-endian, `L585`).
    - Appends `' & |'` on Unix (`L668`) when `!defined($noPipe)` so `FileHandle->new($command)` (`Song.pm:611`) or `Slim::Player::Pipeline->new($sock, $command, !$handler->isRemote)` (`Song.pm:582-600`) spawns the pipeline.

#### 1.2 Exact CLI Invocations in `/home/console-admin/Github/slimserver/convert.conf`

1. **`[faad]`** (AAC ADTS `aac`, MP4 AAC `mp4`/`mp4x`, and Apple Lossless `alc`/`alcx` — note LMS's bundled `faad` has a custom ALAC decoder patch):
   - **Flags**:
     - `-q`: Quiet (suppress stderr status banner/progress).
     - `-w`: Write audio output to stdout.
     - `-f 1`: Output format Microsoft WAV (RIFF header + PCM) to stdout.
     - `-f 2`: Output format raw headerless PCM (little-endian signed) to stdout.
     - `-b 1`: Sample format 16-bit PCM (used on `mp4`/`mp4x`/`aac -> pcm`; omitted on `alc/alcx -> pcm` so ALAC bit depth is preserved).
     - `-j %s` (`T:{START=-j %s}`): Seek to `%s` seconds before decoding.
     - `-e %u` (`U:{END=-e %u}`): End decoding at `%u` seconds.
     - Input: `$FILE$` is a file path for `mp4`/`mp4x`/`alc`/`alcx` (capability `F` only; `mp4x`/`alcx` are leading-`mdat` MP4s set in `Slim/Formats.pm:264-267`), or `-` / `$FILE$` for `aac` (capability `IF`).
   - **All `convert.conf` Rules**:
     - `mp4/mp4x -> mp3` (`L101-107`, `FT:{START=-j %s}U:{END=-e %u}`): `[faad] -q -w -f 1 $START$ $END$ $FILE$ | [lame] --silent -q $QUALITY$ $BITRATE$ - -`
     - `aac -> mp3` (`L109-111`, `IF`): `[faad] -q -w -f 1 $FILE$ | [lame] --silent -q $QUALITY$ $BITRATE$ - -`
     - `alc/alcx -> mp3` (`L117-123`, `FT:{START=-j %s}U:{END=-e %u}D:{RESAMPLE=--resample %D}`): `[faad] -q -w -f 1 $START$ $END$ $FILE$ | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ - -`
     - `alc/alcx -> pcm` (`L253-259`, `FT:{START=-j %s}U:{END=-e %u}`): `[faad] -q -w -f 2 $START$ $END$ $FILE$`
     - `mp4/mp4x -> pcm` (`L265-271`, `FT:{START=-j %s}U:{END=-e %u}`): `[faad] -q -w -f 2 -b 1 $START$ $END$ $FILE$`
     - `aac -> pcm` (`L273-275`, `IF`): `[faad] -q -w -f 2 -b 1 $FILE$`
     - `mp4/mp4x -> flc` (`L334-340`, `FT:{START=-j %s}U:{END=-e %u}`): `[faad] -q -w -f 1 $START$ $END$ $FILE$ | [flac] -cs --totally-silent --compression-level-0 --ignore-chunk-sizes -`
     - `aac -> flc` (`L342-344`, `IF`): `[faad] -q -w -f 1 $FILE$ | [flac] -cs --totally-silent --compression-level-0 --ignore-chunk-sizes -`
     - `alc/alcx -> flc` (`L350-356`, `FT:{START=-j %s}U:{END=-e %u}D:{RESAMPLE=-r %d}`): `[faad] -q -w -f 1 $START$ $END$ $FILE$ | [sox] -q -t wav - -t flac -C 0 $RESAMPLE$ -`

2. **`[flac]`** (FLAC `flc` & OggFLAC `ogf` decoder, and WAV/AIFF/raw PCM -> FLAC encoder):
   - **Flags**:
     - `-d`: Decode mode (when omitted, encodes to FLAC).
     - `-c`: Write output to stdout.
     - `-s` / `--totally-silent`: Suppress stderr output.
     - `--ogg`: Input is OggFLAC container (`ogf`).
     - `--force-raw-format --endian=little|big --sign=signed`: Decode to raw headerless PCM (or encode from raw PCM when combined with `--bps=$SAMPLESIZE$ --sample-rate=$SAMPLERATE$ --channels=$CHANNELS$`).
     - `--compression-level-0`: Fast uncompressed/level-0 FLAC encoding for streaming.
     - `--ignore-chunk-sizes`: Ignore WAV `RIFF`/`data` chunk length headers when encoding streamed WAV from `[faad] -w -f 1`.
     - `--skip=%t` (`T:{START=--skip=%t}`): Skip initial samples formatted as `mm:ss.xx` (disabled on stdin via `E:{NOSTART=I}`).
     - `--until=%v` (`U:{END=--until=%v}`): Stop at `mm:ss.xx`.
     - `-- $FILE$`: End of options marker followed by filename or `-` (stdin).
   - **All `convert.conf` Rules**:
     - `flc -> mp3` (`L137-139`, `IFT:{START=--skip=%t}U:{END=--until=%v}D:{RESAMPLE=--resample %D}E:{NOSTART=I}`): `[flac] -dc --totally-silent $START$ $END$ --force-raw-format --endian=little --sign=signed -- $FILE$ | [sox] -q -t raw --endian little --encoding signed-integer -b $SAMPLESIZE$ -r $SAMPLERATE$ -c $CHANNELS$ - -t raw -c 2 - | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ -r --little-endian -s $SAMPLERATE$ --bitwidth $SAMPLESIZE$ - -`
     - `flc -> pcm` (`L185-187`, `IFT:{START=--skip=%t}U:{END=--until=%v}E:{NOSTART=I}`): `[flac] -dcs --force-raw-format --endian=little --sign=signed $START$ $END$ -- $FILE$`
     - `flc -> aif` (`L189-191`, `IFT:{START=--skip=%t}U:{END=--until=%v}E:{NOSTART=I}`): `[flac] -dcs --force-raw-format --endian=big --sign=signed $START$ $END$ -- $FILE$`
     - `ogf -> flc` (`L199-201`, `IFD:{RESAMPLE=-r %d}`): `[flac] --ogg -dcs -- $FILE$ | [sox] -q --ignore-length -t wav - -t flac -C 0 $RESAMPLE$ -`
     - `aif/wav -> flc` (`L298-304`, `IFT:{START=--skip=%t}U:{END=--until=%v}D:{RESAMPLE=-r %d}E:{NOSTART=I}`): `[flac] -cs --totally-silent --compression-level-0 $START$ $END$ -- $FILE$ | [sox] -q -t flac - -t flac -C 0 $RESAMPLE$ -`
     - `lpcm -> flc` (`L306-308`, `IFT:{START=--skip=%t}U:{END=--until=%v}D:{RESAMPLE=-r %d}E:{NOSTART=I}`): `[flac] -cs --totally-silent --compression-level-0 $START$ $END$ --force-raw-format --endian=big --sign=signed --bps=$SAMPLESIZE$ --sample-rate=$SAMPLERATE$ --channels=$CHANNELS$ -- $FILE$ | [sox] -q -t flac - -t flac -C 0 $RESAMPLE$ -`
     - `wma/wmal/wmap/mpc/ape -> flc` (`L314-332`): `... | [flac] -cs --totally-silent --compression-level-0 -`
     - `mp4/mp4x/aac -> flc` (`L334-344`): `[faad] -q -w -f 1 ... | [flac] -cs --totally-silent --compression-level-0 --ignore-chunk-sizes -`
     - `flc -> flc` (`L385-387`, `IFT:{START=--skip=%t}U:{END=--until=%v}D:{RESAMPLE=-r %d}E:{NOSTART=I}`): `[flac] -dcs $START$ $END$ --force-raw-format --endian=little --sign=signed -- $FILE$ | [sox] -q -t raw --endian little --encoding signed-integer -b $SAMPLESIZE$ -r $SAMPLERATE$ -c $CHANNELS$ - -t flac $RESAMPLE$ -C 0 -`

3. **`[sox]`** (Format converter, resampler, channel mixer, Ogg Vorbis `ogg` & Opus `ops` decoder, FLAC re-encoder):
   - **Flags**:
     - `-q`: Quiet mode.
     - `--ignore-length`: Ignore WAV header length from upstream pipe (`ogf -> flc`).
     - Input/Output format specifiers: `-t ogg`, `-t opus`, `-t wav`, `-t flac`, `-t raw`.
     - PCM encoding flags (both modern and legacy SoX flags are used in `convert.conf`!):
       - Modern flags: `--endian little|big --encoding signed-integer -b $SAMPLESIZE$ -r $SAMPLERATE$ -c $CHANNELS$`
       - Legacy flags (`ogg -> aif/pcm`, `ops -> aif/pcm`, `L207, 210, 220, 223`): `-2` (16-bit / 2-byte words), `-s` (signed integer), `$-x$` (`-x` swap endianness when host is little-endian and target is big-endian `aif`).
     - FLAC compression flag: `-C 0` (compression level 0).
     - Resample flag: `-r %d` (`D:{RESAMPLE=-r %d}`) placed in the output format arguments.
     - Effects: `trim %s` (`T:{START=trim %s}` in `ops -> flc`, `L226-227`, placed after output filename `-`).
   - **All `convert.conf` Rules**:
     - `ogg -> mp3` (`L125-127`, `IFD:{RESAMPLE=--resample %D}`): `[sox] -q -t ogg $FILE$ -t wav - | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ - -`
     - `flc -> mp3` (`L137-139`): `... | [sox] -q -t raw --endian little --encoding signed-integer -b $SAMPLESIZE$ -r $SAMPLERATE$ -c $CHANNELS$ - -t raw -c 2 - | [lame] ...`
     - `lpcm -> mp3` (`L141-143`, `IFD:{RESAMPLE=--resample %D}`): `[sox] -q -t raw --endian big --encoding signed-integer -b $SAMPLESIZE$ -r $SAMPLERATE$ -c $CHANNELS$ $FILE$ -t wav - | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ - -`
     - `ogf -> flc` (`L199-201`): `[flac] --ogg -dcs -- $FILE$ | [sox] -q --ignore-length -t wav - -t flac -C 0 $RESAMPLE$ -`
     - `ogg -> aif` (`L206-207`, `IF`): `[sox] -q -t ogg $FILE$ -t raw -r 44100 -c 2 -2 -s $-x$ -`
     - `ogg -> pcm` (`L209-210`, `IF`): `[sox] -q -t ogg $FILE$ -t raw -r 44100 -c 2 -2 -s -`
     - `ops -> mp3` (`L215-217`, `IFD:{RESAMPLE=--resample %D}`): `[sox] -q -t opus $FILE$ -t wav - | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ - -`
     - `ops -> aif` (`L219-220`, `IF`): `[sox] -q -t opus $FILE$ -t raw -r 44100 -c 2 -2 -s $-x$ -`
     - `ops -> pcm` (`L222-223`, `IF`): `[sox] -q -t opus $FILE$ -t raw -r 44100 -c 2 -2 -s -`
     - `ops -> flc` (`L225-227`, `IFD:{RESAMPLE=-r %d}T:{START=trim %s}`): `[sox] -t opus $FILE$ -t flac -C 0 $RESAMPLE$ - $START$`
     - `aif/wav/lpcm -> flc` (`L298-308`): `[flac] ... | [sox] -q -t flac - -t flac -C 0 $RESAMPLE$ -`
     - `ogg -> flc` (`L310-312`, `IFD:{RESAMPLE=-r %d}`): `[sox] -t ogg $FILE$ -t flac -C 0 $RESAMPLE$ -`
     - `sls/alc/alcx/wvp/wvpx -> flc` (`L346-364`): `... | [sox] -q -t wav - -t flac -C 0 $RESAMPLE$ -`
     - `flc -> flc` (`L385-387`): `[flac] ... | [sox] -q -t raw --endian little --encoding signed-integer -b $SAMPLESIZE$ -r $SAMPLERATE$ -c $CHANNELS$ - -t flac $RESAMPLE$ -C 0 -`

4. **`[wvunpack]`** (WavPack `wvp` and WavPack-DSD `wvpx` decoder):
   - **Flags**:
     - `$FILE$`: Positional input file path (capability `F` only; `.wvc` correction file is located alongside `$FILE$`).
     - `-w`: Output Microsoft WAV format (used for PCM WavPack or DSD->PCM decimation when transcoding `wvpx -> flc`).
     - `-r`: Output raw headerless PCM (`wvp -> pcm`).
     - `--dsf`: Output DSF container format without decimation (`wvpx -> dsf`).
     - `--dff`: Output DFF (DSDIFF) container format without decimation (`wvpx -> dff`).
     - `-q`: Quiet mode.
     - `--skip=%t` (`T:{START=--skip=%t}`): Seek to `mm:ss.xx`.
     - `--until=%v` (`U:{END=--until=%v}`): Trim at `mm:ss.xx`.
     - `-o -`: Write output stream to stdout.
   - **All `convert.conf` Rules**:
     - `wvp -> mp3` (`L165-167`, `FT:{START=--skip=%t}U:{END=--until=%v}D:{RESAMPLE=--resample %D}`): `[wvunpack] $FILE$ -wq $START$ $END$ -o - | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ - -`
     - `wvp -> pcm` (`L261-263`, `FT:{START=--skip=%t}U:{END=--until=%v}`): `[wvunpack] $FILE$ -rq $START$ $END$ -o -`
     - `wvp/wvpx -> flc` (`L358-364`, `FT:{START=--skip=%t}U:{END=--until=%v}D:{RESAMPLE=-r %d}`): `[wvunpack] $FILE$ -wq $START$ $END$ -o - | [sox] -q -t wav - -t flac -C 0 $RESAMPLE$ -`
     - `wvpx -> dsf` (`L408-410`, `FT:{START=--skip=%t}U:{END=--until=%v}`): `[wvunpack] $FILE$ -q --dsf $START$ $END$ -o -`
     - `wvpx -> dff` (`L412-414`, `FT:{START=--skip=%t}U:{END=--until=%v}`): `[wvunpack] $FILE$ -q --dff $START$ $END$ -o -`

5. **`[mac]`** (Monkey's Audio `ape` decoder):
   - **Flags**:
     - Positional args: `<input_file> <output_file> -d` where `<input_file>` is `$FILE$`, `<output_file>` is `-` (stdout, emitting WAV with RIFF header), and `-d` is decompress.
     - Capabilities: `F` only (no `I`, `T`, or `U` capabilities).
   - **All `convert.conf` Rules**:
     - `ape -> mp3` (`L161-163`, `FD:{RESAMPLE=--resample %D}`): `[mac] $FILE$ - -d | [lame] --silent -q $QUALITY$ $RESAMPLE$ $BITRATE$ - -`
     - `ape -> pcm` (`L245-247`, `F`): `[mac] $FILE$ - -d`
     - `ape -> flc` (`L330-332`, `F`): `[mac] $FILE$ - -d | [flac] -cs --totally-silent --compression-level-0 -`

---

### Deliverable 2: Tier 1C — `MP3::Cut::Gapless` Interface & Callers

- **Perl Wrapper Module**: `/home/console-admin/Github/slimserver/CPAN/MP3/Cut/Gapless.pm` (`$VERSION = '0.03'`).
  - **Constructor**: `MP3::Cut::Gapless->new(%args)` (`L10-49`)
    - Arguments used by LMS:
      - `file => $filepath` (string path to MP3 file)
      - `cache_dir => $dir` (directory path `<librarycachedir>/mp3cut`)
      - `start_ms => $int_ms` (optional start offset in integer milliseconds)
      - `end_ms => $int_ms` (optional end offset in integer milliseconds)
    - Internal state & `.mllt` cache file calculation (`L27-43`, `L90-99`):
      - Ensures `cache_dir` exists; stats `$self->{file}` for `($size, $mtime) = (stat _)[7, 9]`.
      - Sets `$self->{cache_file} = File::Spec->catfile($self->{cache_dir}, Digest::MD5::md5_hex($self->{file} . $size . $mtime) . '.mllt')`.
      - Opens `$self->{_fh}` (`<`, `binmode`) and invokes XS `$self->{_mp3c} = $self->__init(@_)`.
  - **XS Functions (`Gapless.xs`)**:
    - `$self->__init(@_)`: Reads or generates the `.mllt` frame index cache at `$self->{cache_file}`, locates the MP3 frames corresponding to `start_ms`..`end_ms`, computes LAME encoder delay/padding and bit-reservoir back-pointers, and returns an opaque pointer stored in `$self->{_mp3c}`.
    - `$self->read($buf, $block_size_hint)`: Reads up to `$block_size_hint` bytes of gapless-cut MP3 stream into caller's scalar `$_[1]` (`$buf` is mutated in-place by XS) and returns the number of bytes written (`0` at EOF).
    - `$self->__reset_read()`: Resets stream position (used only by `write()`, unused by LMS).
    - `$self->__cleanup($self->{_mp3c})`: Frees the C struct in `DESTROY` (`L132-138`).
- **All Call Sites in the LMS Codebase (2 locations)**:
  1. **`/home/console-admin/Github/slimserver/Slim/Formats/Playlists/CUE.pm:781-797`** (`processAnchor`):
     - Triggered during CUE sheet scanning on the first virtual track (`$start == 0`) when `$ct eq 'mp3'` and `$attributesHash->{'LAYER_ID'} == 1` and `Audio::Scan->scan_fh(mp3 => $fh, { filter => 0x01 })->{info}->{lame_encoder_version}` is truthy.
     - Invokes:
       ```perl
       MP3::Cut::Gapless->new(
           file      => $path,
           cache_dir => File::Spec::Functions::catdir( $prefs->get('librarycachedir'), 'mp3cut' ),
       );
       ```
     - Purpose: Pre-warms the `<librarycachedir>/mp3cut/<md5>.mllt` frame index cache during library scan; object is immediately destroyed.
  2. **`/home/console-admin/Github/slimserver/Slim/Player/Protocols/File.pm:208-239` (`open`) & `L249-258` (`sysread`)**:
     - Triggered in `Slim::Player::Protocols::File->open` (called from `Slim::Player::Song->open` at `Song.pm:489`) when `!main::ISWINDOWS && $format eq 'mp3' && $track->virtual`:
       - Extracts `#([^-]+)-([^-]+)$` from `$url` (`$start_ms = sprintf "%d", $1 * 1000`, `$end_ms = sprintf "%d", $2 * 1000`).
       - If `$seekdata->{timeOffset}`, adds `sprintf "%d", $seekdata->{timeOffset} * 1000` to `$start_ms`.
       - Stores object in glob slot `${*$sock}{mp3cut}`:
         ```perl
         ${*$sock}{mp3cut} = MP3::Cut::Gapless->new(
             file      => $filepath,
             cache_dir => catdir( $prefs->get('librarycachedir'), 'mp3cut' ),
             start_ms  => $start_ms,
             end_ms    => $end_ms,
         );
         ```
     - In `Slim::Player::Protocols::File->sysread($self, $_[1], $n, $_[3])` (`L249-258`):
       - Intercepts reads when `${*$self}{mp3cut}` is defined:
         ```perl
         $n = ${*$self}{mp3cut}->read( $_[1], $n );
         ${*$self}{position} += $n unless (!defined($n) || $n <= 0);
         return $n;
         ```
  - *(Note: Neither `Slim::Player::Song` nor `Slim::Formats::MP3` calls `MP3::Cut::Gapless` directly.)*

---

### Deliverable 3: Tier 1C — `Audio::Scan` Interface, Callers & Expected Hash Keys

#### 3.1 Environment Variable Contract: `$ENV{AUDIO_SCAN_NO_ARTWORK}`
- Set globally to `1` in `/home/console-admin/Github/slimserver/scanner.pl:58` and `/home/console-admin/Github/slimserver/slimserver.pl:73`.
- **When `$ENV{AUDIO_SCAN_NO_ARTWORK}` is true (`1`)**:
  - `Audio::Scan` skips allocating raw artwork byte buffers and instead populates the image payload field with the **integer byte length** of the artwork, alongside an offset field pointing to the exact byte offset of the image within the file:
    - ID3v2 `APIC` (`[mime, type, desc, length_int, offset_int]`)
    - FLAC/Ogg `ALLPICTURES` (`[{ picture_type, image_data => length_int, mime_type, offset => offset_int }, ...]`)
    - MP4 `COVR` (`length_int`) + `COVR_offset` (`offset_int`)
    - ASF/WMA `WM/Picture` (`{ image_type, image => length_int, offset => offset_int }`)
    - APEv2 `COVER ART (FRONT)` (`length_int`) + `COVER ART (FRONT)_offset` (`offset_int`)
- **When `local $ENV{AUDIO_SCAN_NO_ARTWORK} = 0`** (used in `getCoverArt` across `Slim/Formats/*.pm` and `_read_data_from_tag` in `Slim/Utils/GDResizer.pm:366`):
  - The image payload field (`APIC->[3]`, `ALLPICTURES->[i]->{image_data}`, `COVR`, `WM/Picture->{image}`, `COVER ART (FRONT)`) contains the **raw binary image bytes**.

#### 3.2 All `Audio::Scan` Methods Called in LMS
*(Note: `scan_data` and path-based `find_frame` are never called in LMS; only the 5 class methods below + `$Audio::Scan::VERSION` are used.)*

1. **`Audio::Scan->scan($filepath)`** -> returns `{ info => \%info, tags => \%tags }`:
   - `Slim/Formats/MP3.pm:118` (`getTag`)
   - `Slim/Formats/FLAC.pm:94` (`getTag`), `Slim/Formats/FLAC.pm:209` (`getCoverArt` with `AUDIO_SCAN_NO_ARTWORK=0`)
   - `Slim/Formats/Ogg.pm:77` (`getTag`)
   - `Slim/Formats/Movie.pm:58` (`getTag`)
   - `Slim/Formats/Wav.pm:33` (`getTag`)
   - `Slim/Formats/AIFF.pm:50` (`getTag`)
   - `Slim/Formats/WMA.pm:63` (`getTag`)
   - `Slim/Formats/APE.pm:52` (`getTag`)
   - `Slim/Formats/Musepack.pm:30` (`getTag`)
   - `Slim/Formats/WavPack.pm:15` (`getTag`)
   - `Slim/Formats/DSD.pm:19` (`getTag`)

2. **`Audio::Scan->scan_tags($filepath)`** -> returns `{ tags => \%tags }` (or `{ info => ..., tags => \%tags }`):
   - `Slim/Formats/MP3.pm:167` (`getCoverArt`, also imported by `Wav.pm:116`, `AIFF.pm:124`, `DSD.pm:50`)
   - `Slim/Formats/Ogg.pm:173` (`getCoverArt`)
   - `Slim/Formats/Movie.pm:129` (`getCoverArt`)
   - `Slim/Formats/WMA.pm:133` (`getCoverArt`)
   - `Slim/Formats/APE.pm:112` (`getCoverArt`, also imported by `Musepack.pm:50`, `WavPack.pm:41`)
   - `Slim/Utils/GDResizer.pm:299` (`_read_tag`) & `L367` (`_read_data_from_tag`)
   - `Slim/Menu/TrackInfo.pm:1285` (`tagDump`)

3. **`Audio::Scan->scan_fh($format => $fh, [$opts_hashref])`** -> returns `{ info => \%info, tags => \%tags }`:
   - Supported `$format` strings passed by LMS: `'mp3'`, `'flac'`, `'ogf'`, `'ogg'`, `'opus'`, `'mp4'`, `'aac'`, `'wav'`, `'aif'`, `'asf'`, `'dff'`, `'dsf'`.
   - Call sites:
     - `Slim/Formats/MP3.pm:256` (`mp3 => $fh`)
     - `Slim/Formats/FLAC.pm:830` (`$format || 'flac' => $fh`, where `OggFLAC.pm:60` passes `'ogf'`), `L862` (`flac => $fh`)
     - `Slim/Formats/Ogg.pm:214` (`ogg => $fh`), `L306` (`ogg => $localFh`)
     - `Slim/Formats/OggOpus.pm:26` (`opus => $fh`), `L118` (`opus => $localFh`)
     - `Slim/Formats/Movie.pm:232` (`aac => $localFh`), `L329` (`mp4 => $fh`)
     - `Slim/Formats/Wav.pm:109` (`wav => $fh`)
     - `Slim/Formats/AIFF.pm:117` (`aif => $fh`)
     - `Slim/Formats/WMA.pm:155` (`asf => $localFh`)
     - `Slim/Formats/DFF.pm:20, 42` (`dff => $localFh`)
     - `Slim/Formats/DSF.pm:20, 42` (`dsf => $localFh`)
     - `Slim/Formats/Playlists/CUE.pm:775` (`mp3 => $fh, { filter => 0x01 }` — `filter => 0x01` enables parsing LAME header for `lame_encoder_version`)
     - `Slim/Utils/Scanner/Remote.pm:644` (`asf => $fh`), `L766` (`aac => $fh`)
     - `Slim/Plugin/UPnP/MediaRenderer/ProtocolHandler.pm:341` (`$scan->{asFormat}, $scan->{fh}` for `'mp3'`, `'wav'`, `'aif'`, `'ogg'`, `'opus'`)

4. **`Audio::Scan->find_frame_fh($format => $fh, $offset_or_ms)`** -> returns integer byte offset (`-1` on failure):
   - `Slim/Formats/MP3.pm:196` (`mp3 => $localFh, -$offset` — negative value means byte offset to find next valid MP3 frame), `L234` (`mp3 => $fh, $offset` — positive value is `int($time * 1000)` ms; negative value is byte offset)
   - `Slim/Formats/FLAC.pm:814` (`flac => $fh, int($time * 1000)`)
   - `Slim/Formats/Ogg.pm:333` (`ogg => $fh, int($time * 1000)`)
   - `Slim/Formats/OggOpus.pm:145` (`opus => $fh, int($time * 1000)`)
   - `Slim/Formats/WMA.pm:174` (`asf => $fh, int($time * 1000)`)

5. **`Audio::Scan->find_frame_fh_return_info($format => $fh, $ms)`** -> returns `\%info` hashref:
   - `Slim/Formats/Movie.pm:221` (`mp4 => $localFh, int($time * 1000)`) -> reads `$info->{tracks}->[0]`, `$info->{seek_header}`, `$info->{seek_offset}`
   - `Slim/Formats/OggFLAC.pm:44` (`ogf => $localFh, int($time * 1000)`) -> reads `$info->{seek_header}`, `$info->{seek_offset}`

6. **`$Audio::Scan::VERSION`**:
   - Checked in `Slim/Formats.pm:85` (`>= 0.94` for DSD `dff`/`dsf`), `L90` (`>= 0.99` for WavPack DSD `wvpx`), `L94` (`>= 1.02` for Opus `ops`), and reported in `Slim/Menu/SystemInfo.pm:359` (`Audio::Scan->VERSION`). Must be `>= 1.02` (e.g. `'1.06'`).

#### 3.3 Exact `info` and `tags` Hash Keys Expected by Format Modules
- **Universal `info` keys**:
  - `song_length_ms`, `file_size`, `audio_offset`, `audio_size`, `bitrate`, `samplerate`, `channels`, `bits_per_sample`, `id3_version`, `dlna_profile`, `jenkins_hash` (in `GDResizer.pm`).
- **Format-Specific `info` keys**:
  - **MP3 (`MP3.pm`, `CUE.pm`)**: `stereo`, `layer` (`1` = MP1, `2` = MP2, `3` = MP3), `vbr`, `lame_encoder_version`.
  - **FLAC / OggFLAC (`FLAC.pm`, `OggFLAC.pm`)**: `bitrate_ogg`, `total_samples`, `seek_header`, `seek_offset`.
  - **Ogg / Opus (`Ogg.pm`, `OggOpus.pm`)**: `ogg_type` (`'flac'` redirects to `OggFLAC`), `bitrate_average`, `bitrate_nominal`, `bitrate_upper`, `bitrate_lower`.
  - **MP4 / AAC / ALAC (`Movie.pm`, `Remote.pm`)**: `avg_bitrate`, `leading_mdat` (sets `mp4x`/`alcx`), `seek_header`, `seek_offset`, `samplerate`, `channels`, `tracks` (arrayref of track hashes containing `width`, `bits_per_sample`, `channels`, `encoding` [`'alac'`, `'drms'`], `audio_object_type` [`2` = AAC-LC, `37` = SLS], `audio_type` [`64` = MPEG-4 audio]).
  - **WAV (`Wav.pm`)**: `block_align`, `bits_per_sample`.
  - **AIFF (`AIFF.pm`)**: `compression_type` (`'sowt'` sets little-endian `ENDIAN = 0`).
  - **WMA / ASF (`WMA.pm`, `Remote.pm`)**: `max_bitrate`, `lossless`, `codec_list` (arrayref of `{ name }`), `streams` (arrayref of `{ samplerate, bits_per_sample, encrypted, channels, stream_number, bitrate, stream_type ('ASF_Command_Media'), codec_id (0x0161 | 0x000a) }`).
  - **APE / Musepack / WavPack (`APE.pm`, `Musepack.pm`, `WavPack.pm`)**: `version`, `profile`, `encoder`, `bits_per_sample` (`== 1` in `WavPack.pm` sets `WAVPACKDSD = 1` -> `wvpx`).
  - **DSD (`DSD.pm`, `DSF.pm`, `DFF.pm`)**: `tag_diar_artist`, `tag_diti_title`, `block_size_per_channel` (DSF).
- **Format-Specific `tags` keys**:
  - **ID3v2 (`MP3.pm`, `Wav.pm`, `AIFF.pm`, `FLAC.pm`, `DSD.pm`)**:
    - Standard frames: `COMM` (`[lang, desc, text]` or arrayref of arrays), `TALB`, `TBPM`, `TCOM`, `TCMP`, `YTCP`, `TCON`, `TIT1`, `GRP1`, `TIT2`, `TIT3`, `TSST`, `TPE1`, `TPE2`, `TPE3`, `TPOS`, `TRCK`, `TSOA`, `YTSA`, `TSOP`, `YTSP`, `XSOP`, `TSOT`, `YTST`, `'TST '`, `TSO2`, `YTS2`, `TSOC`, `YTSC`, `UFID`, `USLT` (`[lang, desc, text]`), `TDOR`, `XDOR`, `TDRC`, `TYER`, `TDAT`, `APIC` (`[mime_type, image_type, desc, data_or_len, offset]`, or nested arrayref when multiple pictures exist).
    - ReplayGain frames: `YRVA` / `RVAD` (`[gain]`), `RVA2` (`[type, chan, gain_str, peak_str]`), `RGAD`.
    - `TXXX` / custom frames unpacked as top-level uppercase keys in `tags`: `'ALBUM ARTIST'`, `'ITUNNORM'`, `'REPLAYGAIN_TRACK_GAIN'`, `'REPLAYGAIN_TRACK_PEAK'`, `'REPLAYGAIN_ALBUM_GAIN'`, `'REPLAYGAIN_ALBUM_PEAK'`, `'MUSICBRAINZ ALBUM ARTIST'`, `'MUSICBRAINZ ALBUM ARTIST ID'`, `'MUSICBRAINZ ALBUM ID'`, `'MUSICBRAINZ ALBUM STATUS'`, `'MUSICBRAINZ ALBUM TYPE'`, `'MUSICBRAINZ ARTIST ID'`, `'MUSICBRAINZ TRM ID'`, `'MUSICBRAINZ RELEASE TRACK ID'`, `'RELEASETYPE'`, `'WORK'`, `'PERFORMANCE'`, `'COMPILATION'`, `'BPM'`, `'DISC'`, `'DISCC'`, `'YEAR'`, `'REPLAYGAIN'`.
  - **Vorbis Comments (`FLAC.pm`, `Ogg.pm`, `OggOpus.pm`)**:
    - Uppercase tag keys (`TITLE`, `ARTIST`, `ALBUM`, `ALBUMARTIST`, `TRACKNUMBER`, `DISCNUMBER`, `DISCTOTAL`, `TOTALDISCS`, `DISC`, `DISCC`, `GENRE`, `DATE`, `YEAR`, `ORIGINALYEAR`, `ORIGINALDATE`, `COMMENT`, `DESCRIPTION`, `COMPOSER`, `CONDUCTOR`, `BAND`, `ENSEMBLE`, `ORCHESTRA`, `COMPILATION`, `BPM`, `REPLAYGAIN_*`, `MUSICBRAINZ_*`, `RELEASETYPE`, `WORK`, `GROUPING`, `SUBTITLE`, `DISCSUBTITLE`, `LYRICS`, `UNSYNCEDLYRICS`, `OPUS`, etc.).
    - Special FLAC blocks in `tags`: `ALLPICTURES` (arrayref of `{ picture_type, image_data, mime_type, offset }`), `CUESHEET_BLOCK` (arrayref of lines), `CUESHEET`, `COVERART`, `COVERARTMIME`, `APPLICATION` (hashref keyed by integer application ID: `1885693293` for Windows Media XML, `1163084622` for `'PIC1' . $raw_image`).
  - **MP4 (`Movie.pm`)**:
    - Atom keys: `AART`, `ALB`, `ART`, `CMT`, `COVR`, `COVR_offset`, `CON`, `CPIL`, `DAY`, `GNRE`, `GEN`, `LYR`, `NAM`, `SOAA`, `SOAL`, `SOAR`, `SOCO`, `SONM`, `TMPO`, `TRKN`, `WRT`, `WRK`, `GRP`, `DISK` (`"disc/discc"`), plus freeform `----:com.apple.iTunes:*` keys stripped of prefix or matched case-insensitively (`iTunNORM`, `iTunSMPB`, `MusicBrainz *`, `replaygain_*`, `RELEASETYPE`, `SUBTITLE`, `DISCSUBTITLE`, `CONDUCTOR`).
  - **ASF / WMA (`WMA.pm`)**:
    - `Author`, `Title`, `WM/BeatsPerMinute`, `WM/AlbumArtist`, `WM/AlbumTitle`, `WM/Composer`, `WM/Conductor`, `WM/Genre`, `WM/TrackNumber`, `WM/PartOfACompilation`, `WM/IsCompilation`, `Description`, `WM/PartOfSet`, `WM/ArtistSortOrder`, `WM/AlbumSortOrder`, `WM/Comments`, `WM/Lyrics`, `WM/Year`, `WM/Work`, `WM/ContentGroupDescription`, `WM/SubTitle`, `WM/SetSubTitle`, `IsVBR`, `WM/Picture` (`{ image_type, image, offset }` or arrayref), `MusicBrainz/*`, `replaygain_*`.
  - **APEv2 (`APE.pm`, `Musepack.pm`, `WavPack.pm`)**:
    - `TITLE`, `ARTIST`, `ALBUM`, `ALBUM ARTIST`, `ALBUMARTIST`, `TRACK`, `DISCNUMBER`, `DISC`, `DATE`, `YEAR`, `GENRE`, `COMMENT`, `COMPOSER`, `CONDUCTOR`, `COMPILATION`, `BPM`, `LYRICS`, `WORK`, `GROUPING`, `SUBTITLE`, `DISCSUBTITLE`, `RELEASETYPE`, `MUSICBRAINZ_TRACKID`, `REPLAYGAIN_*`, `COVER ART (FRONT)`, `COVER ART (FRONT)_offset`.
  - **WAV RIFF INFO (`Wav.pm`)**:
    - `IART`, `ICMT`, `ICRD`, `IGNR`, `INAM`, `IPRD`, `TRCK`, `ITRK`.

---

### Deliverable 4: Tier 2 — `scanner.pl` & `Slim::Utils::Scanner::Local` Architecture & SQLite Schema

#### 4.1 `scanner.pl` Execution Pipeline (`/home/console-admin/Github/slimserver/scanner.pl:1-502`)
1. **Initialization (`L49-286`)**:
   - Sets `constant SCANNER => 1` and `$ENV{AUDIO_SCAN_NO_ARTWORK} = 1`.
   - Parses CLI options: `--force`, `--rescan`, `--wipe`, `--playlists`, `--onlinelibrary`, `--prefsdir`, `--prefsfile`, `--pidfile`, `--progress`, `--priority`, `--logfile`, `--logdir`, `--logconfig`, `--debug`, `--quiet`, `--dbtype`, and optional `[singledir]`.
   - Initializes OS detection, preferences (`server.prefs`), SQLiteHelper, Cache, VirtualLibraries, and registers scan importers:
     - `Slim::Media::MediaFolderScan` (`type => 'file'`, `weight => 1`)
     - `Slim::Music::PlaylistFolderScan` (`type => 'file'`, `weight => 10`)
     - `Slim::Music::ContributorPictureScan` (`type => 'artwork'`, `weight => 5`)
     - `Slim::Music::ReleaseTypes` (`type => 'post'`, `weight => 90`)
     - `Slim::Music::VirtualLibraries` (`type => 'post'`, `weight => 100`)
     - Enabled plugins in `'import'` category (e.g. `FullTextSearch`, `OnlineLibrary`).
   - Calls `Slim::Music::Info::init()` -> `Slim::Schema->init()` (connects to SQLite and runs `SQL/SQLite/schema_*_up.sql` migrations up to version `26`).
2. **Transaction & Wipe Setup (`L295-315`)**:
   - Sets `Slim::Schema->dbh->{'AutoCommit'} = 0`.
   - Sets `metainformation.isScanning` (`SETUP_STANDARDRESCAN` | `SETUP_WIPEDB` | `SETUP_PLAYLISTRESCAN`).
   - If `--wipe`, calls `Slim::Schema->wipeAllData`: executes `SQL/SQLite/schema_clear.sql` (`DELETE FROM` all library tables; preserves `persistentdb.tracks_persistent`), wipes `ArtworkCache` (`artwork.db`), and commits.
3. **File Discovery & Diff/Batch Scanning (`Slim::Music::Import->runScan`, `Slim/Utils/Scanner/Local.pm:54-1382`)**:
   - **`find` (`Local.pm:54-115`, `Local/Async.pm:28-182`)**:
     - Purges `scanned_files` under `$basedir` (`DELETE FROM scanned_files WHERE url = '${file}' OR url LIKE '${file}/%'`).
     - Walks directory tree using `File::Next::everything` with `Slim::Utils::Misc::fileFilter` / `folderFilter`.
     - Inserts rows into `scanned_files (url, timestamp, filesize)` (`filesize = 0` for directories; `stat` `mtime` and `size` for files), committing every 200 rows (`Async.pm:175`).
   - **`rescan` (`Local.pm:223-323`)**:
     - Creates temporary diff tables in SQLite:
       - `dbonly`: deleted tracks (`tracks.url NOT IN (SELECT url FROM scanned_files WHERE filesize != 0)`).
       - `diskonly`: new tracks (`scanned_files.url NOT IN (SELECT url FROM tracks WHERE id <= $maxTrackId)`).
       - `changed`: modified tracks (`scanned_files JOIN tracks` where `timestamp` or `filesize` differs) + `changed_albums`.
   - **Chunked Processing (`CHUNK_SIZE = 50` for SQLite, `Local.pm:34, 412-655`)**:
     - Processes `dbonly` (`deleteTracks` -> `deleted`), `changed` (`updateTracks` -> `changed` + `contributor_album` rebuild), and `diskonly` (`addTracks` -> `new` -> `Slim::Schema->updateOrCreateBase`), calling `Slim::Schema->forceCommit` after every 50 rows.
     - Handles single-file CUE sheets and embedded CUE sheets (`VCS` / `CUESHEET_BLOCK`) via `processCueSheet` / `scanPlaylistFileHandle`, creating virtual track rows (`virtual = 1`, `url = "file:///...#start-end"`).
4. **Post-Processing (`Slim::Music::Import->runScanPostProcessing`, `Import.pm:462-521`)**:
   - Runs `post` importers (`ReleaseTypes->startScan` to normalize album `release_type` & build `contributor_album`; `VirtualLibraries->startScan` to populate `library_*` tables; `FullTextSearch` to populate `fulltext` FTS table).
   - Runs `artwork` importers (`ContributorPictureScan->startArtworkScan`).
   - Runs `Slim::Music::Artwork->updateStandaloneArtwork()` (on incremental scans) and `Slim::Music::Artwork->precacheAllArtwork()` (finds `coverid` for tracks with `cover != 0 AND cover_cached IS NULL`, resizes artwork into `artwork.db` via `GDResizer`, sets `albums.artwork = coverid` and `tracks.cover_cached = 1`).
   - Runs `Slim::Schema->optimizeDB()` (`ANALYZE;` from `schema_optimize.sql` + `VACUUM` on `library.db` and `persist.db`).
5. **Completion & Notification (`scanner.pl:344-490`)**:
   - Updates `metainformation` (`lastRescanTime`, `lastRescanTimeIsDST`, `isScanning = 0`), commits, and sends HTTP JSON-RPC notification `['scanner', 'notify', 'end']` / `'exit'` with header `X-Scanner: 1` to `http://<httpaddr>:<httpport>/jsonrpc.js`.

#### 4.2 SQLite Databases, PRAGMAs, Custom Functions & Tables (Schema v26)
- **Database Files (`Slim/Utils/SQLiteHelper.pm:87-115, 348-358`)**:
  - Main DB: `<librarycachedir>/library.db`
  - Attached DB: `<prefsdir>/persist.db` attached as `persistentdb` (`ATTACH '$persistentdb' AS persistentdb`)
  - Artwork Cache DB: `<librarycachedir>/artwork.db` (table `ArtworkCache (k BLOB PRIMARY KEY, t INT, v BLOB)`)
- **PRAGMAs & Custom SQLite Functions**:
  - `PRAGMA synchronous = OFF`, `PRAGMA journal_mode = WAL`, `PRAGMA foreign_keys = ON`, `PRAGMA wal_autocheckpoint = 10000` (in scanner).
  - Custom scalar function: `MD5(x)` returning 32-char lowercase hex MD5 (`SQLiteHelper.pm:363`).
  - Custom collations: `icu_load_collation($locale, $locale)` or fallback `perllocale`.
- **Complete Table Schema (`SQL/SQLite/schema_1_up.sql` through `schema_26_up.sql`)**:
  1. `metainformation (name VARCHAR(255) PRIMARY KEY, value VARCHAR(255))`
  2. `dbix_migration (name VARCHAR(255) PRIMARY KEY, value VARCHAR(255))` (`version = 26`)
  3. `scanned_files (url text COLLATE NOCASE, timestamp int(10), filesize int(10))`
  4. `tracks`:
     - `id INTEGER PRIMARY KEY AUTOINCREMENT`, `url text NOT NULL COLLATE NOCASE`, `title blob`, `titlesort text`, `titlesearch text`, `customsearch text`, `album int(10) REFERENCES albums (id) ON DELETE CASCADE`, `tracknum int(10)`, `content_type varchar(255)`, `timestamp int(10)`, `filesize int(10)`, `audio_size int(10)`, `audio_offset int(10)`, `year smallint(5)`, `secs float`, `cover blob`, `vbr_scale varchar(255)`, `bitrate float`, `samplerate int(10)`, `samplesize int(10)`, `channels tinyint(1)`, `block_alignment int(10)`, `endian bool`, `bpm smallint(5)`, `tagversion varchar(255)`, `drm bool`, `disc tinyint(1)`, `audio bool`, `remote bool`, `lossless bool`, `lyrics text COLLATE NOCASE`, `musicbrainz_id varchar(40)`, `musicmagic_mixable bool`, `replay_gain float`, `replay_peak float`, `extid varchar(64)`, `primary_artist int(10)`, `urlmd5 char(32) NOT NULL DEFAULT '0'`, `coverid char(8)`, `cover_cached tinyint(1)`, `virtual tinyint(1)`, `added_time int(10)`, `updated_time int(10)`, `dlna_profile varchar(32)`, `hash char(16)`, `work int(10)`, `subtitle blob`, `grouping blob`, `performance varchar(255)`, `discsubtitle blob`.
  5. `albums`:
     - `id INTEGER PRIMARY KEY AUTOINCREMENT`, `title blob`, `titlesort text`, `titlesearch text`, `customsearch text`, `compilation bool`, `year smallint(5)`, `artwork char(8)`, `disc tinyint(1)`, `discc tinyint(1)`, `replay_gain float`, `replay_peak float`, `musicbrainz_id varchar(40)`, `musicmagic_mixable bool`, `contributor int(10)`, `extid varchar(64)`, `release_type varchar(64)`, `subtitle blob`, `label varchar(64)`.
  6. `contributors`:
     - `id INTEGER PRIMARY KEY AUTOINCREMENT`, `name blob`, `namesort text`, `namesearch text`, `customsearch text`, `musicbrainz_id varchar(40)`, `musicmagic_mixable bool`, `extid varchar(64)`, `portrait text`, `portraitid char(8)`.
  7. `contributor_track`:
     - `role int(10)`, `contributor int(10) REFERENCES contributors (id) ON DELETE CASCADE`, `track int(10) REFERENCES tracks (id) ON DELETE CASCADE`, `PRIMARY KEY (role, contributor, track)`.
     - Role IDs (`Slim/Schema/Contributor.pm:25-32`): `ARTIST=1`, `COMPOSER=2`, `CONDUCTOR=3`, `BAND=4`, `ALBUMARTIST=5`, `TRACKARTIST=6`, dynamic custom roles `>= 21`.
  8. `contributor_album`:
     - `role int(10)`, `contributor int(10) REFERENCES contributors (id) ON DELETE CASCADE`, `album int(10) REFERENCES albums (id) ON DELETE CASCADE`, `PRIMARY KEY (role, contributor, album)`.
  9. `genres`:
     - `id INTEGER PRIMARY KEY AUTOINCREMENT`, `name blob`, `namesort text`, `namesearch text`, `customsearch text`, `musicmagic_mixable bool`.
  10. `genre_track`:
      - `genre int(10) REFERENCES genres (id) ON DELETE CASCADE`, `track int(10) REFERENCES tracks (id) ON DELETE CASCADE`, `PRIMARY KEY (genre, track)`.
  11. `years (id smallint(5) PRIMARY KEY)`
  12. `comments (id INTEGER PRIMARY KEY AUTOINCREMENT, track int(10) REFERENCES tracks (id) ON DELETE CASCADE, value text COLLATE NOCASE)`
  13. `works (id INTEGER PRIMARY KEY AUTOINCREMENT, composer int(10) NOT NULL REFERENCES contributors (id) ON DELETE CASCADE, title blob NOT NULL, titlesort text NOT NULL, titlesearch text NOT NULL)`
  14. `playlist_track (id INTEGER PRIMARY KEY AUTOINCREMENT, position int(10), playlist int(10) REFERENCES tracks (id) ON DELETE CASCADE, track text NOT NULL)`
  15. `library_track (track int(10) REFERENCES tracks (id) ON DELETE CASCADE, library char(8) NOT NULL, PRIMARY KEY (track, library))` + derived `library_album`, `library_contributor`, `library_genre`.
  16. `progress (id INTEGER PRIMARY KEY AUTOINCREMENT, type varchar(255), name varchar(255), active tinyint(1), total int(10), done int(10), start int(10), finish int(10), info varchar(255))`
  17. `fulltext` (FTS3 virtual table populated by `Slim::Plugin::FullTextSearch::Plugin` post-scan importer) & `fulltext_terms` (`fts4aux`).
  18. `persistentdb.tracks_persistent`:
      - `id INTEGER PRIMARY KEY AUTOINCREMENT`, `url text NOT NULL COLLATE NOCASE`, `musicbrainz_id varchar(40)`, `added int(10)`, `rating tinyint(1)`, `playCount int(10)`, `lastPlayed int(10)`, `urlmd5 char(32) NOT NULL DEFAULT '0'`.
