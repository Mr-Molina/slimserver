# Work Order: WO-001

- **ID**: WO-001
- **Timestamp**: 2026-10-20T15:15:00-04:00
- **Subagent Role**: Discovery Scout
- **Status**: COMPLETED

## Task Goal
Perform a comprehensive read-only survey of `/home/console-admin/Github/slimserver` and the local host environment to determine everything needed to build, package, or run Lyrion Music Server (`slimserver`).

## Scope & Constraints
- **Target Directories**:
  - `/home/console-admin/Github/slimserver`
  - `/home/console-admin/Github` (check for sibling repositories such as `slimserver-vendor`, `slimserver-platforms`, etc.)
- **Checks Required**:
  1. Top-level directory layout, entrypoints (`slimserver.pl`, `scanner.pl`, `cleanup.pl`), and `.github/workflows/`.
  2. Host OS distribution, architecture (`uname -a`, `/etc/os-release`), Perl version (`perl -v`), and whether compiled XS modules exist in `/home/console-admin/Github/slimserver/CPAN/arch/` matching the host Perl version and architecture.
  3. Check installed system packages vs required packages (`libio-socket-ssl-perl`, `libnet-ssleay-perl`, `netcat-traditional`, `EV`, `Audio::Scan`, `Image::Scale`, `DBD::SQLite`, `YAML::XS`, `Sub::Name`, `HTML::Parser`, `JSON::XS`, `XML::Parser`, `Digest::SHA1`, etc.) and test `perl -ICPAN /home/console-admin/Github/slimserver/t/verifyStrings.pl`.
  4. Check if there are build scripts, Dockerfiles, or vendor build tools in `/home/console-admin/Github/slimserver` or `/home/console-admin/Github`.
- **Rules**: Read-only survey. Do not modify files. Verify all command exit codes.

## Fulfillment Section

### Fulfillment Summary (`[EXECUTED & VERIFIED]`)

#### 1. Repository Topology & Sibling Repositories
- **Repository Root (`/home/console-admin/Github/slimserver`)**:
  - Branch: `public/9.2` (Lyrion Music Server v`9.2.0`).
  - Primary Perl Entrypoints:
    - `/home/console-admin/Github/slimserver/slimserver.pl` (Main server daemon/application)
    - `/home/console-admin/Github/slimserver/scanner.pl` (Standalone media library scanner)
    - `/home/console-admin/Github/slimserver/cleanup.pl` (Cache/prefs/database/logs cleanup utility; requires `-I/home/console-admin/Github/slimserver` or CWD `/home/console-admin/Github/slimserver`)
    - `/home/console-admin/Github/slimserver/gdresized.pl` (Standalone UNIX-socket artwork resizer daemon)
  - Key Subdirectories:
    - `/home/console-admin/Github/slimserver/Slim/` — Core LMS Perl namespace (`Slim::bootstrap`, `Slim::Utils::*`, `Slim::Player::*`, `Slim::Web::*`, `Slim::Plugin::*`, `Slim::Schema::*`, etc.)
    - `/home/console-admin/Github/slimserver/CPAN/` — Bundled pure-Perl CPAN dependencies and `CPAN/arch/<perl-version>/<arch>/` pre-compiled XS binary `.so` modules (`5.10` through `5.42`).
    - `/home/console-admin/Github/slimserver/lib/` — Additional pure-Perl library overrides (`AnyEvent`, `Audio`, `Cache`, `CGI`, `Class`, `DBIx`, `HTML`, `Log`, `MPEG`, `Net`, `Protocol`, `Template`, `Tie`).
    - `/home/console-admin/Github/slimserver/Bin/x86_64-linux/` — Pre-compiled 64-bit Linux helper binaries for transcoding/decoding: `faad`, `flac`, `mac` (statically linked), `sox`, `wvunpack`.
    - `/home/console-admin/Github/slimserver/HTML/`, `Firmware/`, `Graphics/`, `IR/`, `SQL/` — Web UI templates, Squeezebox firmware, fonts/graphics, IR maps, SQLite schemas.
    - `/home/console-admin/Github/slimserver/.devcontainer/` — Contains `Dockerfile` (based on `mcr.microsoft.com/devcontainers/base:bookworm`), `docker-compose.yml`, `devcontainer.json`, and `post-create.sh`.
    - `/home/console-admin/Github/slimserver/t/` — Contains `00_smoketest.sh`, `verifyStrings.pl`, and `gdresizer_explicit_format.t`.
- **Sibling Repositories in `/home/console-admin/Github`**:
  - Present related repos: `LMS-Cast`, `lms-material`, `Lyrion-MaterialSkin`, `squeezelite-esp32`.
  - **Not cloned locally**:
    - `LMS-Community/slimserver-platforms` (contains `buildme.pl` and packaging specs for Debian `.deb`, Fedora `.rpm`, `tarball`, `pcp`, `docker`, `macos`, `win64`).
    - `LMS-Community/slimserver-vendor` (contains `CPAN/buildme.sh` and C/XS source tarballs used only when compiling `CPAN/arch/` binaries or `Bin/` transcoding tools for unsupported Perl versions/architectures).

#### 2. Host OS, Perl Version & Pre-Compiled XS Binary Compatibility
- **Host OS**: Debian GNU/Linux 13 (trixie) v13.7, kernel `6.12.113+deb13-amd64` (`x86_64`).
- **Host Perl**: `v5.40.1` (`version='5.40.1'`, `api_versionstring='5.40.0'`, `archname='x86_64-linux-gnu-thread-multi'`).
- **Bundled XS Match**:
  - `Slim::bootstrap` normalizes `archname` by stripping `gnu-` (`$arch =~ s/gnu-//;`), mapping `x86_64-linux-gnu-thread-multi` to `x86_64-linux-thread-multi` and `version` `5.40.1` to major version `5.40`.
  - `/home/console-admin/Github/slimserver/CPAN/arch/5.40/x86_64-linux-thread-multi/` is **present in the repository** and contains all **18 pre-compiled XS `.so` binaries**:
    1. `Audio::Scan` (`auto/Audio/Scan/Scan.so`)
    2. `Class::XSAccessor` (`auto/Class/XSAccessor/XSAccessor.so`)
    3. `DBD::SQLite` (`auto/DBD/SQLite/SQLite.so`)
    4. `DBI` (`auto/DBI/DBI.so`)
    5. `Digest::SHA1` (`auto/Digest/SHA1/SHA1.so`)
    6. `Encode::Detect::Detector` (`auto/Encode/Detect/Detector/Detector.so`)
    7. `EV` (`auto/EV/EV.so`)
    8. `HTML::Parser` (`auto/HTML/Parser/Parser.so`)
    9. `Image::Scale` (`auto/Image/Scale/Scale.so`)
    10. `IO::AIO` (`auto/IO/AIO/AIO.so`)
    11. `IO::Interface` (`auto/IO/Interface/Interface.so`)
    12. `JSON::XS` (`auto/JSON/XS/XS.so`)
    13. `Linux::Inotify2` (`auto/Linux/Inotify2/Inotify2.so`)
    14. `MP3::Cut::Gapless` (`auto/MP3/Cut/Gapless/Gapless.so`)
    15. `Sub::Name` (`auto/Sub/Name/Name.so`)
    16. `Template::Stash::XS` (`auto/Template/Stash/XS/XS.so`)
    17. `XML::Parser::Expat` (`auto/XML/Parser/Expat/Expat.so`)
    18. `YAML::XS::LibYAML` (`auto/YAML/XS/LibYAML/LibYAML.so`)
  - **Empirical Verification**:
    - `ldd` across all 18 `.so` files and all 5 binaries in `/home/console-admin/Github/slimserver/Bin/x86_64-linux/` resolved 100% of dynamic library links (`0` missing libraries).
    - `perl /home/console-admin/Github/slimserver/slimserver.pl --help d_startup` exited `0`, loading all 10 `@default_required_modules` on the first attempt and passing all 31 module version checks in `/home/console-admin/Github/slimserver/modules.conf`.
    - Loading all 18 XS modules simultaneously in a single Perl 5.40.1 invocation exited `0` (`ALL_18_XS_MODULES_LOADED_OK`).
    - Running `/home/console-admin/Github/slimserver/t/gdresizer_explicit_format.t` (with `/usr/share/perl/5.40` prepended so system `Test::More` supersedes legacy `CPAN/Test/More.pm`) passed `1..2 ok` (exit code `0`).

#### 3. CI Workflows, Packaging Pipelines & Host Package Audit
- **Workflows Inspected (`/home/console-admin/Github/slimserver/.github/workflows/` & `.github/actions/build/action.yaml`)**:
  - `00_smoketest.yaml`: Runs on `ubuntu-24.04` as non-root; installs `libio-socket-ssl-perl`, `libnet-ssleay-perl`, `netcat-traditional`; runs `perl -ICPAN t/verifyStrings.pl` and `bash t/00_smoketest.sh` (which starts `slimserver.pl`, waits for TCP port `9000` via `nc -z localhost 9000`, queries `/jsonrpc.js` via `curl`, and validates JSON output with `jq`).
  - `00_build.yaml` & `.github/actions/build/action.yaml`: Checks out `LMS-Community/slimserver-platforms` (`ref: public/9.2`) into `platforms/`, installs `apt-transport-https debhelper devscripts` (for Debian builds) or `rpm-build rpmdevtools systemd-rpm-macros rsync jq unzip perl which` (for RPM builds in `fedora:44`), and runs `platforms/buildme.pl --build <flavour> --buildDir <work> --sourceDir <src> --destDir <publish> --releaseType <nightly|release>`.
- **Host Package Status (`dpkg-query` / `which`)**:
  - **Installed (`[EXECUTED & VERIFIED]`)**:
    - Core runtime & SSL: `perl` (`5.40.1`), `libio-socket-ssl-perl`, `libnet-ssleay-perl`, `libwww-perl`, `libdata-dump-perl`, `libssl-dev`, `libgomp1`, `libasound2t64`, `zlib1g-dev`
    - Utilities & Build Tools: `netcat-traditional` (`/usr/bin/nc`), `curl`, `git`, `rsync`, `unzip`, `gcc`, `g++`, `make`, `cmake`, `docker`, `dpkg-buildpackage`
  - **Missing on Host (by use case)**:
    1. **To run `t/verifyStrings.pl` and `t/00_smoketest.sh` unaltered**:
       - `libjson-perl`: `/home/console-admin/Github/slimserver/t/verifyStrings.pl` line 8 has `use JSON;` (which is unused in the script body, while `CPAN/` only bundles `JSON::XS`, not `JSON.pm`), causing `perl -I/home/console-admin/Github/slimserver/CPAN /home/console-admin/Github/slimserver/t/verifyStrings.pl` to exit `2` (`Can't locate JSON.pm in @INC`).
       - `jq`: `/home/console-admin/Github/slimserver/t/00_smoketest.sh` lines 37-54 pipe `curl` JSON-RPC responses into `jq`.
    2. **To build official distribution packages (`.deb`, `.tar.gz`, etc.)**:
       - Clone `https://github.com/LMS-Community/slimserver-platforms` (`public/9.2` branch) and run `platforms/buildme.pl`.
       - For `.deb` packages: `debhelper` and `devscripts` are not currently installed (though `dpkg-buildpackage` is installed).
    3. **Optional Runtime Perl Modules & External Transcoders (from `.devcontainer/Dockerfile`)**:
       - `lame` (MP3 encoding) and `opus-tools` (`opusenc`/`opusdec`) are not installed (`flac`, `sox`, `faad`, `mac`, `wvunpack` are already bundled in `/home/console-admin/Github/slimserver/Bin/x86_64-linux/`).
       - `libcrypt-openssl-rsa-perl`, `libcrypt-blowfish-perl`, `libfont-freetype-perl` (note: `Linux::Inotify2` is already bundled in `CPAN/arch/5.40`).
    4. **To recompile `CPAN/arch/` XS modules from scratch (NOT needed on this host)**:
       - Would require cloning `https://github.com/LMS-Community/slimserver-vendor` (`public/9.2` branch) and installing `nasm`/`yasm`/`libgd-dev`. Not needed because `CPAN/arch/5.40/x86_64-linux-thread-multi/` is already built and verified working.
