# Work Order: WO-022 — Deploy Upstream Fixes & Updated `aarch64` Binaries to `10.0.0.10` (`https://lyrion.molinadeveloping.com/`)

- **ID**: WO-022
- **Timestamp**: 2026-10-20T20:54:30-04:00
- **Subagent Role**: Infrastructure Administrator (`d98ec07a-5ff3-49fe-a36f-ac09c5162586`)
- **Status**: COMPLETED

## 1. Task Goal
Back up the 10 additional baseline Perl files on `10.0.0.10`, deploy the updated `aarch64` Rust release binaries (`gdresized`, `lms-transcode` with `--help` support) and the 10 upstream bugfix/PR Perl patches to `10.0.0.10` (preserving `9.1.1` compatibility), restart `lyrionmusicserver.service`, and verify `rust_integration.t` (`1..12`) and live HTTP/HTTPS JSON-RPC on `10.0.0.10`.

## 2. Target Steps
1. **Immutable Baseline Backup on `10.0.0.10` (Rules 30, 31, 35)**:
   - Note: `OS.pm.bak`, `Debian.pm.bak`, `GDResizer.pm.bak`, `ImageResizer.pm.bak`, `Local.pm.bak`, and `Gapless.pm.bak` are already stored in `/var/backups/lms-rust-baseline-20261020/` on `10.0.0.10` (do NOT overwrite `Local.pm.bak`!).
   - Back up the 10 newly targeted Perl files on `10.0.0.10` into `/var/backups/lms-rust-baseline-20261020/`:
     - `/usr/share/perl5/Slim/Formats/Movie.pm` -> `/var/backups/lms-rust-baseline-20261020/Movie.pm.bak`
     - `/usr/share/perl5/Slim/Player/Pipeline.pm` -> `/var/backups/lms-rust-baseline-20261020/Pipeline.pm.bak`
     - `/usr/share/perl5/Slim/Player/Source.pm` -> `/var/backups/lms-rust-baseline-20261020/Source.pm.bak`
     - `/usr/share/perl5/Slim/Networking/Async/HTTP.pm` -> `/var/backups/lms-rust-baseline-20261020/HTTP.pm.bak`
     - `/usr/share/perl5/Slim/Formats/MP3.pm` -> `/var/backups/lms-rust-baseline-20261020/MP3.pm.bak`
     - `/usr/share/perl5/Slim/Utils/Misc.pm` -> `/var/backups/lms-rust-baseline-20261020/Misc.pm.bak`
     - `/usr/share/perl5/Slim/Formats/Playlists/Base.pm` -> `/var/backups/lms-rust-baseline-20261020/Playlists_Base.pm.bak`
     - `/usr/share/perl5/Slim/Schema.pm` -> `/var/backups/lms-rust-baseline-20261020/Schema.pm.bak`
     - `/usr/share/perl5/Slim/Control/Queries.pm` -> `/var/backups/lms-rust-baseline-20261020/Queries.pm.bak`
     - `/usr/share/perl5/Slim/Networking/Slimproto.pm` -> `/var/backups/lms-rust-baseline-20261020/Slimproto.pm.bak`
2. **Diff Inspection (`9.1.1` vs `9.2.0`) & Surgical Patching**:
   - Fetch the `.bak` files from `10.0.0.10:/var/backups/lms-rust-baseline-20261020/` to `/tmp/lms-10-0-0-10-stage/` on `10.0.0.90`.
   - Compare each against `c85a3a8ba` (`9.2.0` baseline). For any file that is 100% identical between `9.1.1` and `9.2.0` baseline, copy the modified file from `/home/console-admin/Github/slimserver/`; for any file that has `9.1.1` vs `9.2.0` differences (such as `Local.pm` or `Schema.pm`), apply ONLY the surgical bugfix hunks from commit `30eebde2f` onto `10.0.0.10`'s file using `edit_file` so `9.1.1` compatibility is 100% preserved!
3. **Deploy Updated `aarch64` Binaries & Patched Perl Modules to `10.0.0.10`**:
   - Copy `/home/console-admin/Github/slimserver/Bin/aarch64-linux/{gdresized,lms-transcode,lms-mp3cut,lms-audioscan,lms-scanner-accel}`, `/home/console-admin/Github/slimserver/t/rust_integration.t`, and the patched Perl modules to `/tmp/lms-rust-stage/` on `10.0.0.10`.
   - Install the 5 `aarch64` binaries into `/usr/share/squeezeboxserver/Bin/aarch64-linux/` (`0755 root:root`) and the patched Perl modules into `/usr/share/perl5/Slim/...` (`0644 root:root`).
   - Verify Perl syntax (`perl -c`) on `10.0.0.10` for `/usr/sbin/squeezeboxserver` and all modified modules.
4. **Restart `lyrionmusicserver.service` & Run Live Independent Verification on `10.0.0.10`**:
   - Restart `lyrionmusicserver.service`, wait 5 seconds, and verify:
     1. `systemctl is-active lyrionmusicserver.service` -> `active`
     2. `ps aux` shows native `/usr/share/squeezeboxserver/Bin/aarch64-linux/gdresized` listening on `/tmp/sbs_artwork`
     3. `/usr/bin/perl -I/usr/share/perl/5.40 -I/usr/share/squeezeboxserver/CPAN/arch/5.40/aarch64-linux-thread-multi -I/usr/share/squeezeboxserver/CPAN/arch/5.40 -I/usr/share/squeezeboxserver/CPAN -I/usr/share/perl5 /tmp/lms-rust-stage/rust_integration.t` passes **`1..12`** (`ok 1` through `ok 12`) on `10.0.0.10` (`aarch64`)!
     4. `http://127.0.0.1:9000/jsonrpc.js` and `https://127.0.0.1:443/jsonrpc.js` (`Host: lyrion.molinadeveloping.com`) return HTTP 200 and valid `serverstatus`.
     5. Clean up `/tmp/lms-rust-stage` on `10.0.0.10` and `/tmp/lms-10-0-0-10-stage` on `10.0.0.90`.

## 3. Fulfillment Section

### Status: `[EXECUTED & VERIFIED]`
- **Step 1 — Immutable Baseline Backup (`[EXECUTED & VERIFIED]`)**:
  Backed up all 10 additional baseline Perl files (`Movie.pm.bak`, `Pipeline.pm.bak`, `Source.pm.bak`, `HTTP.pm.bak`, `MP3.pm.bak`, `Misc.pm.bak`, `Playlists_Base.pm.bak`, `Schema.pm.bak`, `Queries.pm.bak`, `Slimproto.pm.bak`) into `/var/backups/lms-rust-baseline-20261020/` on `10.0.0.10` alongside the 6 existing backups from `WO-017` (`Local.pm.bak` preserved).
- **Step 2 — Diff Inspection & Surgical Patching (`[EXECUTED & VERIFIED]`)**:
  Compared `9.1.1` (`10.0.0.10`) vs `9.2.0` (`c85a3a8ba`): `Pipeline.pm`, `Source.pm`, `Playlists/Base.pm`, and `Slimproto.pm` were 100% identical; `Movie.pm`, `HTTP.pm`, `MP3.pm`, `Misc.pm`, `Queries.pm`, `Schema.pm`, and `Local.pm` differed and were surgically patched in `/tmp/lms-10-0-0-10-stage/` using `edit_file` to preserve 100% of `9.1.1` compatibility.
- **Step 3 — Deployment & Syntax Verification (`[EXECUTED & VERIFIED]`)**:
  Installed the 5 updated `aarch64` Rust release binaries (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`) and 11 patched Perl modules onto `10.0.0.10`. Verified `gdresized --help` (`exit 0`), `lms-transcode --help` (`exit 0`), and `/usr/sbin/squeezeboxserver syntax OK`.
- **Step 4 — Service Restart & Live Independent Verification (`[EXECUTED & VERIFIED]`)**:
  - `systemctl is-active lyrionmusicserver.service` -> `active`.
  - `ps aux`: `/usr/bin/perl /usr/sbin/squeezeboxserver` (PID 10435) spawned `/usr/share/squeezeboxserver/Bin/aarch64-linux/gdresized` (PID 10439, `2,084` KB RSS) bound to `/tmp/sbs_artwork`.
  - `rust_integration.t` on `10.0.0.10` (`aarch64`): passed **`1..12`** (`ok 1` through `ok 12`, exit code `0`).
  - Live HTTP `:9000` & HTTPS `:443` (`Host: lyrion.molinadeveloping.com`) JSON-RPC `serverstatus`: HTTP 200, valid JSON (`532` songs, `19` albums, `201` artists, `16` players).
  - Removed `/tmp/lms-rust-stage` on `10.0.0.10` and `/tmp/lms-10-0-0-10-stage` locally.
- **Handoff Summary**: `/home/console-admin/Github/slimserver/.agents/knowledge/subagent_summaries/20261020_wo022_live_upstream_fixes.md`
