# Work Order: WO-016 — Cross-Compile `aarch64-unknown-linux-gnu` Binaries & Multi-Arch Debian Perl Bridges

- **ID**: WO-016
- **Timestamp**: 2026-10-20T19:50:30-04:00
- **Subagent Role**: Task 8 Implementer (`cca0ce89-2d26-4f22-8e50-bca00ee4d6e1`)
- **Status**: [EXECUTED & VERIFIED]

## 1. Task Goal
Cross-compile all 5 Rust release binaries (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`) for `aarch64-unknown-linux-gnu`, place them in `/home/console-admin/Github/slimserver/Bin/aarch64-linux/`, and update the Perl bridges (`Slim/Utils/OS/Debian.pm`, `Slim/Utils/OS.pm`, `Slim/Utils/ImageResizer.pm`, `Slim/Utils/GDResizer.pm`, `CPAN/MP3/Cut/Gapless.pm`, `Slim/Utils/Scanner/Local.pm`, and `t/rust_integration.t`) so they work seamlessly across both `x86_64-linux` and `aarch64-linux` as well as Debian package layout (`/usr/share/squeezeboxserver/Bin/aarch64-linux/`).

## 2. Target Steps
1. **Cross-Compile for `aarch64-unknown-linux-gnu`**:
   - Run `/home/console-admin/.cargo/bin/rustup target add aarch64-unknown-linux-gnu`.
   - Build release binaries for `aarch64-unknown-linux-gnu`:
     ```bash
     CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/aarch64-linux-gnu-gcc \
       /home/console-admin/.cargo/bin/cargo build --release --target aarch64-unknown-linux-gnu \
       --manifest-path /home/console-admin/Github/slimserver/rust/Cargo.toml
     ```
   - Copy the 5 `aarch64` release binaries (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`) to `/home/console-admin/Github/slimserver/Bin/aarch64-linux/` (`chmod 0755`) and verify with `/usr/bin/file` that all 5 are `ELF 64-bit LSB pie executable, ARM aarch64`.
2. **Multi-Arch & Debian Package Path Resolution in Perl Bridges**:
   - In `/home/console-admin/Github/slimserver/Slim/Utils/OS/Debian.pm`:
     Update `sub gdresized` so when `$ENV{LMS_USE_RUST} // 1`, it checks `/usr/share/squeezeboxserver/Bin/aarch64-linux/gdresized`, `/usr/share/squeezeboxserver/Bin/x86_64-linux/gdresized`, `$Bin/Bin/aarch64-linux/gdresized`, and `$Bin/Bin/x86_64-linux/gdresized`, returning the first executable match, or falling back to `"/usr/sbin/squeezeboxserver-resized"`.
   - In `/home/console-admin/Github/slimserver/Slim/Utils/OS.pm` and `/home/console-admin/Github/slimserver/Slim/Utils/GDResizer.pm`:
     Update `sub gdresized` / `sub rust_gdresized_bin` to check both the current host architecture (`POSIX::uname` or `$Config::Config{archname} =~ /aarch64/ ? 'aarch64-linux' : 'x86_64-linux'`) and `/usr/share/squeezeboxserver/Bin/$arch/gdresized` as well as `$Bin/Bin/$arch/gdresized`.
   - In `/home/console-admin/Github/slimserver/Slim/Utils/ImageResizer.pm`:
     Update `if ($command =~ /\.pl$/i)` to `if ($command =~ /\.pl$/i || (-f $command && -T $command))` so extensionless Perl scripts like `/usr/sbin/squeezeboxserver-resized` (when `LMS_USE_RUST=0`) are wrapped with `$Config{'perlpath'}`, while native ELF binaries (`-B $command`) run directly.
   - In `/home/console-admin/Github/slimserver/CPAN/MP3/Cut/Gapless.pm` (`_rust_bin`) and `/home/console-admin/Github/slimserver/Slim/Utils/Scanner/Local.pm` (`rustScannerAccelBin`):
     Check architecture-matched directories first (`my $arch = ($Config::Config{archname} || '') =~ /aarch64/ ? 'aarch64-linux' : 'x86_64-linux';` across `$FindBin::Bin/Bin/$arch`, `/usr/share/squeezeboxserver/Bin/$arch`, `/home/console-admin/Github/slimserver/Bin/$arch`).
   - In `/home/console-admin/Github/slimserver/t/rust_integration.t`:
     Resolve `$bin_dir` dynamically based on `$Config{archname} =~ /aarch64/ ? 'aarch64-linux' : 'x86_64-linux'` and fall back to `/usr/share/squeezeboxserver/Bin/$arch` and `/usr/share/squeezeboxserver/HTML/...` if run on a packaged Debian host (`10.0.0.10`)!
3. **Verify & Commit on `Rust-Experiment`**:
   - Re-run `t/rust_integration.t`, `t/gdresizer_explicit_format.t`, and `t/verifyStrings.pl` on `10.0.0.90`.
   - Stage and commit (`git add Bin/aarch64-linux/ Slim/Utils/OS/Debian.pm Slim/Utils/OS.pm Slim/Utils/GDResizer.pm Slim/Utils/ImageResizer.pm CPAN/MP3/Cut/Gapless.pm Slim/Utils/Scanner/Local.pm t/rust_integration.t && git commit -m "feat(rust): add aarch64 release binaries and multi-arch Debian package path resolution"`).

## 3. Fulfillment Section
- **Status**: `[EXECUTED & VERIFIED]`
- **Completed At**: 2026-10-20T19:55:25-04:00
- **Commit**: `6c0a90ca6` (`feat(rust): add aarch64 release binaries and multi-arch Debian package path resolution`)
- **Key Artifacts**:
  - `Bin/aarch64-linux/{gdresized,lms-transcode,lms-mp3cut,lms-audioscan,lms-scanner-accel}` (verified ARM aarch64 ELF)
  - `Slim/Utils/OS/Debian.pm`, `Slim/Utils/OS.pm`, `Slim/Utils/GDResizer.pm`, `Slim/Utils/ImageResizer.pm`
  - `CPAN/MP3/Cut/Gapless.pm`, `Slim/Utils/Scanner/Local.pm`, `t/rust_integration.t`
  - Subagent Summary: `/home/console-admin/Github/slimserver/.agents/knowledge/subagent_summaries/20261020_wo016_aarch64_build.md`
- **Verification Evidence**:
  - `cargo build --release --target aarch64-unknown-linux-gnu`: Exit code 0
  - `file Bin/aarch64-linux/*`: All 5 Rust binaries verified `ELF 64-bit LSB pie executable, ARM aarch64`
  - `perl t/rust_integration.t`: 9/9 tests pass (exit code 0)
  - `perl t/gdresizer_explicit_format.t`: 2/2 tests pass (exit code 0)
  - `perl t/verifyStrings.pl`: 0 errors (exit code 0)
