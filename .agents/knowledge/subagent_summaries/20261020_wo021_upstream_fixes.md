# Subagent Summary: WO-021 Upstream Bug Fixes, PRs & CLI Ergonomics

- **Date**: 2026-10-20
- **Role**: Task 8 Implementer (`cca0ce89-2d26-4f22-8e50-bca00ee4d6e1`)
- **Work Order**: `WO-021` (`/home/console-admin/Github/slimserver/.agents/knowledge/work_orders/WO-021_implement_upstream_fixes.md`)
- **Status**: `[EXECUTED & VERIFIED]`
- **Commit**: `30eebde2f` (`fix: incorporate upstream PRs (#1656, #1696), bugfixes (#1692, #1650, #1256, #1677, #1673, #1665, #1580), and CLI --help support`)

## 1. Files Modified, Created, or Deleted
- Backups created in `.agents/backups/`:
  - `Movie.pm.bak`, `Pipeline.pm.bak`, `Source.pm.bak`, `HTTP.pm.bak`, `MP3.pm.bak`, `Local.pm.bak`, `Misc.pm.bak`, `Playlists_Base.pm.bak`, `Schema.pm.bak`, `Queries.pm.bak`, `Slimproto.pm.bak`
- Modified source files:
  - `Slim/Formats/Movie.pm` (Issue #1692: unwrapped arrayref YEAR tag)
  - `Slim/Player/Pipeline.pm` & `Slim/Player/Source.pm` (PR #1656: pipeline writer write-watcher on pending bytes, waking up stalled stream on large embedded covers)
  - `Slim/Networking/Async/HTTP.pm` (PR #1696: drain socket buffers immediately if data arrived with headers)
  - `Slim/Formats/MP3.pm` (Issue #1650: prioritized TDRC/TYER over TDOR/XDOR for YEAR and captured ORIGINALYEAR)
  - `Slim/Utils/Scanner/Local.pm` (Issues #1256 & #1404: stat scalar capture before bind; Issue #1677: deleted cascade for playlist_track)
  - `Slim/Utils/Misc.pm` (Issue #1677: inMediaFolder optional media type argument)
  - `Slim/Formats/Playlists/Base.pm` (Issue #1677: guard updateOrCreate against local files outside active audio dirs)
  - `Slim/Schema.pm` (Issue #1673: _createWork multi-composer and same-album grouping)
  - `Slim/Control/Queries.pm` (Issue #1665: FTS sort fallback to titlesort instead of titlesearch length)
  - `Slim/Networking/Slimproto.pm` (Issue #1580: SO_KEEPALIVE on client sockets)
  - `rust/crates/lms-transcode/src/main.rs`, `rust/crates/lms-gdresized/src/main.rs`, `rust/crates/lms-audioscan/src/main.rs` (CLI `--help` / `-h` early exit 0)
  - `Bin/x86_64-linux/*` & `Bin/aarch64-linux/*` (Rebuilt release binaries)
  - `t/rust_integration.t` (Expanded from 9 to 12 tests)
  - `WO-021_implement_upstream_fixes.md` (Updated with fulfillment summary)

## 2. Rationale for Each Change
- **Target 1 (Issue #1692)**: Prevents array reference stringification (`ARRAY(0x...)`) from corrupting Movie release years.
- **Target 2 (PR #1656)**: Eliminates stream stalls when transcoding files with >2MB embedded album art by watching pipeline writer writability when buffers fill.
- **Target 3 (PR #1696)**: Prevents 10-second playback delays on remote HLS streams where the playlist body arrives in the same TCP packet as the HTTP header.
- **Target 4 (Issue #1650)**: Correctly assigns compilation album years from release year (`TDRC`) while preserving recording year (`TDOR`) in `ORIGINALYEAR`.
- **Target 5 (Issues #1256 & #1404)**: Protects against DBI bind count mismatches during directory scans when `stat _` is empty.
- **Target 6 (Issue #1677)**: Ensures deleted/removed media files are pruned from playlist references and not spuriously re-added to the database.
- **Target 7 (Issue #1673)**: Groups multi-movement classical works with arranger-specific composer credits into a single coherent Work.
- **Target 8 (Issue #1665)**: Restores alphabetical album sorting for equal-weight FTS search results.
- **Target 9 (Issue #1580)**: Enables TCP keep-alives on Squeezebox 2/3 connections to prevent 60-second idle socket drops.
- **Target 10 (CLI `--help` & Integration Tests)**: Allows all 5 Rust binaries to exit cleanly with usage text on `--help`, verified by integration tests.

## 3. Verification Evidence
- `cargo clippy --all-targets -- -D warnings`: Exit code 0, 0 warnings.
- `cargo test --all-targets`: Exit code 0, 37/37 tests pass.
- `perl t/rust_integration.t`: Exit code 0, 12/12 tests pass (including `--help` verification, Movie arrayref unwrap, and MP3 compilation year mapping).
- `perl t/gdresizer_explicit_format.t`: Exit code 0, 2/2 tests pass.
- `perl t/verifyStrings.pl`: Exit code 0, 0 errors.
- `bash t/00_smoketest.sh`: Exit code 0, serverstatus and date RPC queries return 200 OK.
- Zero lingering processes or logs.
