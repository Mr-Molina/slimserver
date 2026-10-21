# Work Order: WO-017 — Baseline Backup, Deployment & Live Verification on `10.0.0.10` (`https://lyrion.molinadeveloping.com/`)

- **ID**: WO-017
- **Timestamp**: 2026-10-20T19:51:00-04:00
- **Subagent Role**: Infrastructure Administrator (`d98ec07a-5ff3-49fe-a36f-ac09c5162586`)
- **Status**: COMPLETED

## 1. Task Goal
Back up the baseline files on `10.0.0.10`, deploy the 5 cross-compiled `aarch64` Rust release binaries (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`) and updated Perl bridge modules to `10.0.0.10`, restart `lyrionmusicserver.service`, and verify live operation (`ps aux` showing native `/usr/share/squeezeboxserver/Bin/aarch64-linux/gdresized`, `/tmp/sbs_artwork` socket, `rust_integration.t` on `10.0.0.10`, and live HTTP JSON-RPC + artwork resize).

## 2. Target Steps
1. **Immutable Baseline Backup on `10.0.0.10` (Rules 30, 31, 35)**:
   - On `10.0.0.10` via `/usr/bin/ssh console-admin@10.0.0.10`, create `/var/backups/lms-rust-baseline-20261020` (`sudo /usr/bin/mkdir -p /var/backups/lms-rust-baseline-20261020`) and copy:
     - `/usr/share/perl5/Slim/Utils/OS.pm` -> `/var/backups/lms-rust-baseline-20261020/OS.pm.bak`
     - `/usr/share/perl5/Slim/Utils/OS/Debian.pm` -> `/var/backups/lms-rust-baseline-20261020/Debian.pm.bak`
     - `/usr/share/perl5/Slim/Utils/GDResizer.pm` -> `/var/backups/lms-rust-baseline-20261020/GDResizer.pm.bak`
     - `/usr/share/perl5/Slim/Utils/ImageResizer.pm` -> `/var/backups/lms-rust-baseline-20261020/ImageResizer.pm.bak`
     - `/usr/share/perl5/Slim/Utils/Scanner/Local.pm` -> `/var/backups/lms-rust-baseline-20261020/Local.pm.bak`
     - `/usr/share/squeezeboxserver/CPAN/MP3/Cut/Gapless.pm` -> `/var/backups/lms-rust-baseline-20261020/Gapless.pm.bak`
   - Verify all 6 backup files exist and are non-empty on `10.0.0.10`.
2. **Stage & Deploy `aarch64` Rust Binaries to `10.0.0.10`**:
   - Copy `/home/console-admin/Github/slimserver/Bin/aarch64-linux/{gdresized,lms-transcode,lms-mp3cut,lms-audioscan,lms-scanner-accel}` and `/home/console-admin/Github/slimserver/t/rust_integration.t` to `/tmp/lms-rust-stage/` on `10.0.0.10` via `/usr/bin/scp`.
   - Install the 5 binaries into `/usr/share/squeezeboxserver/Bin/aarch64-linux/` on `10.0.0.10` (`sudo /usr/bin/install -o root -g root -m 0755 ...`).
   - Run `/usr/bin/ldd /usr/share/squeezeboxserver/Bin/aarch64-linux/{gdresized,lms-transcode,lms-mp3cut,lms-audioscan,lms-scanner-accel}` on `10.0.0.10` to confirm zero missing shared libraries, and run `--help` / smoke check on `10.0.0.10`.
3. **Apply Surgical Perl Bridge Updates on `10.0.0.10`**:
   - First check if `OS.pm`, `Debian.pm`, `GDResizer.pm`, `ImageResizer.pm`, `Local.pm`, and `Gapless.pm` on `10.0.0.10` (`v9.1.1`) are identical or have version diffs vs our `Rust-Experiment` files (`diff -u` against `/var/backups/lms-rust-baseline-20261020/*.bak`).
   - Apply the exact Rust bridge patches cleanly onto `10.0.0.10`'s files (preserving any `9.1.1`-specific code if different) and verify syntax on `10.0.0.10` (`perl -I/usr/share/squeezeboxserver/CPAN -I/usr/share/squeezeboxserver -I/usr/share/perl5 -c /usr/sbin/squeezeboxserver` and `perl -I/usr/share/squeezeboxserver/CPAN/arch/5.40/aarch64-linux-thread-multi -I/usr/share/squeezeboxserver/CPAN -c /usr/share/squeezeboxserver/CPAN/MP3/Cut/Gapless.pm`).
4. **Restart `lyrionmusicserver.service` & Run Live Independent Verification on `10.0.0.10`**:
   - Run `sudo /usr/bin/systemctl restart lyrionmusicserver.service` on `10.0.0.10`.
   - Wait 5 seconds for daemon initialization, then verify on `10.0.0.10`:
     1. `systemctl is-active lyrionmusicserver.service` returns `active`.
     2. `ps aux | grep -E 'squeezeboxserver|gdresized'` confirms `/usr/share/squeezeboxserver/Bin/aarch64-linux/gdresized` is running as a native Rust process (and `/usr/sbin/squeezeboxserver-resized` is no longer used) and `/tmp/sbs_artwork` socket exists.
     3. Run `/tmp/lms-rust-stage/rust_integration.t` on `10.0.0.10` (`perl -I/usr/share/perl/5.40 -I/usr/share/squeezeboxserver/CPAN/arch/5.40/aarch64-linux-thread-multi -I/usr/share/squeezeboxserver/CPAN/arch/5.40 -I/usr/share/squeezeboxserver/CPAN -I/usr/share/perl5 /tmp/lms-rust-stage/rust_integration.t`) and verify `1..9` pass on `aarch64`!
     4. Query `http://127.0.0.1:9000/jsonrpc.js` (`serverstatus`) and `https://127.0.0.1:443/jsonrpc.js` (`-k -H 'Host: lyrion.molinadeveloping.com'`) on `10.0.0.10` and verify HTTP 200 + valid JSON response.
     5. Remove temporary staging directory `/tmp/lms-rust-stage` on `10.0.0.10`.

## 3. Fulfillment Section

### Status: `[EXECUTED & VERIFIED]`
- **Step 1 — Immutable Baseline Backup (`[EXECUTED & VERIFIED]`)**:
  Created `/var/backups/lms-rust-baseline-20261020/` on `10.0.0.10` with verified non-empty backups: `OS.pm.bak` (`12K`), `Debian.pm.bak` (`2.8K`), `GDResizer.pm.bak` (`14K`), `ImageResizer.pm.bak` (`6.7K`), `Local.pm.bak` (`39K`), `Gapless.pm.bak` (`6.7K`).
- **Step 2 — `aarch64` Rust Binaries Deployed (`[EXECUTED & VERIFIED]`)**:
  Installed `gdresized` (`4,769,392` B), `lms-transcode` (`2,939,520` B), `lms-mp3cut` (`546,232` B), `lms-audioscan` (`1,821,232` B), and `lms-scanner-accel` (`4,955,144` B) into `/usr/share/squeezeboxserver/Bin/aarch64-linux/` (`0755 root:root`). `ldd` confirmed all shared libraries resolved on `10.0.0.10`.
- **Step 3 — Surgical Perl Bridge Updates (`[EXECUTED & VERIFIED]`)**:
  Inspected `diff -u` between `10.0.0.10` (`9.1.1`) and `9.2.0` baseline; patched `Local.pm.bak` surgically to preserve `9.1.1`'s `findStandaloneArtwork(\%columnValueHash, ...)` signature while adding `rustScannerAccelBin` and the `lms-scanner-accel` hook, and installed all 6 updated Perl files (`OS.pm`, `Debian.pm`, `GDResizer.pm`, `ImageResizer.pm`, `Local.pm`, `Gapless.pm`). Verified `/usr/sbin/squeezeboxserver syntax OK`, `/usr/share/squeezeboxserver/CPAN/MP3/Cut/Gapless.pm syntax OK`, and `/usr/share/perl5/Slim/Utils/GDResizer.pm syntax OK`.
- **Step 4 — Service Restart & Independent Live Verification (`[EXECUTED & VERIFIED]`)**:
  - `systemctl is-active lyrionmusicserver.service` -> `active`.
  - `ps aux`: `/usr/bin/perl /usr/sbin/squeezeboxserver` (PID 9754) spawned native Rust `/usr/share/squeezeboxserver/Bin/aarch64-linux/gdresized` (PID 9763, `2,008` KB RSS) listening on `/tmp/sbs_artwork` (`srwxr-xr-x 1 squeezeboxserver nogroup 0 Oct 20 20:08 /tmp/sbs_artwork`), replacing `/usr/sbin/squeezeboxserver-resized`.
  - `rust_integration.t` on `10.0.0.10` (`aarch64`): `1..9` passed (`ok 1`..`ok 9`, exit code `0`).
  - Live JSON-RPC `serverstatus` on `http://127.0.0.1:9000/jsonrpc.js` and `https://127.0.0.1:443/jsonrpc.js` (`Host: lyrion.molinadeveloping.com`): HTTP 200, valid JSON (`"version":"9.1.1"`, `"info total songs":532`, `"info total albums":19`, `"info total artists":201`, `"player count":16`).
  - Removed `/tmp/lms-rust-stage` on `10.0.0.10` and `/tmp/lms-10-0-0-10-baseline` locally.
- **Handoff Summary**: `/home/console-admin/Github/slimserver/.agents/knowledge/subagent_summaries/20261020_wo017_live_deploy.md`
