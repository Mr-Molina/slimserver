# Work Order: WO-015 — Live `lyrion.molinadeveloping.com` Infrastructure & Service Discovery

- **ID**: WO-015
- **Timestamp**: 2026-10-20T19:39:45-04:00
- **Subagent Role**: Discovery Scout (Live Lyrion Deployment)
- **Status**: [EXECUTED & VERIFIED]

## 1. Task Goal
Perform a strictly read-only discovery survey on the local host (and local reverse-proxy / tunnel / container / systemd configurations) to determine how `https://lyrion.molinadeveloping.com/` is hosted and where its active Lyrion Music Server installation resides.

## 2. Target Checks (Read-Only)
1. Check running processes and systemd services related to Lyrion / Squeezebox / LMS / reverse proxies / tunnels:
   - `ps aux | grep -E 'slimserver|squeezebox|lyrion|gdresized|cloudflared|nginx|caddy|traefik|apache|haproxy'`
   - `systemctl list-unit-files | grep -E 'lyrion|squeeze|lms|slim|cloudflared|nginx|caddy|docker'`
   - `docker ps -a` (if docker is active)
2. Check reverse proxy / tunnel configurations (e.g., `/etc/nginx/`, `/etc/cloudflared/`, `/etc/caddy/`, `/etc/hosts`, or home directory configs) for `lyrion.molinadeveloping.com` to see what backend IP/port and host/container it routes to.
3. If `lyrion.molinadeveloping.com` routes to a local installation (e.g. `/usr/share/squeezeboxserver`, `/usr/share/lyrionmusicserver`, `/usr/share/perl5/Slim`, `/opt/`, or a Docker container or another LAN host), identify:
   - Exact installation directory, binary directory (`Bin/x86_64-linux`), and Perl module directory (`Slim/`, `CPAN/`).
   - Version (`slimserver.pl --version` or package version via `dpkg -l | grep -iE 'lyrion|squeeze|logitech'`).
   - Service unit file path and runtime user/group.
   - Whether `useLocalImageproxy` or `gdresized` is currently active in `/var/lib/squeezeboxserver/prefs/server.prefs` (or equivalent prefs path).
4. **Constraint (Rule 33)**: Do NOT run `curl` against external domain `https://lyrion.molinadeveloping.com/` directly; inspect local process/service/proxy/DNS state only. Do NOT modify any files other than `/home/console-admin/Github/slimserver/.agents/knowledge/DIAGNOSTIC_LOG.md` and appending your Fulfillment Summary below.

## 3. Fulfillment Section

### Status: `[EXECUTED & VERIFIED]`
- **Completed At**: 2026-10-20T19:46:00-04:00
- **Auditor/Scout**: Discovery Scout (Live Lyrion Deployment)

### 1. Host Identification & Routing Topology
- **Production Host**: Dedicated LAN host `10.0.0.10` (hostname: `lyrion`). Accessible via SSH as `console-admin@10.0.0.10`.
- **Local Dev Host**: `10.0.0.90` (`pc-1`). Does NOT run LMS directly; hosts only the git source checkout (`/home/console-admin/Github/slimserver`) and Docker container `kiosk-ingress` for tablet kiosks.
- **Reverse Proxy Routing**:
  - Web server: NGINX on `10.0.0.10` (`/etc/nginx/sites-available/lyrion.molinadeveloping.com`).
  - Public hostname: `lyrion.molinadeveloping.com` (also responds on `10.0.0.10`).
  - Ports:
    - 80/tcp: HTTP canonical 301 redirect to `https://lyrion.molinadeveloping.com$request_uri`.
    - 443/tcp: TLS 1.3 proxy terminating SSL (`/etc/ssl/molinadeveloping.com/fullchain.cer`), proxying upstream to `http://127.0.0.1:9000`.
    - 9001/tcp: SSL proxy (`/etc/ssl/certs/lyrion.crt`), proxying to `http://127.0.0.1:9000`.
    - 9000/tcp: LMS HTTP web interface and JSON-RPC API.
    - 9090/tcp: LMS CLI control socket.
    - 3483/tcp & udp: LMS SlimProto control and player discovery.
  - Tunnels: No cloudflared tunnel on `10.0.0.10`. Router edge handles WAN routing to `10.0.0.10`.

### 2. Live Operating System & Architecture
- **OS**: Debian GNU/Linux 13 (trixie), release 13.7.
- **Kernel & Arch**: `Linux lyrion 6.12.33-v8+ #1887 SMP PREEMPT Thu Jun 12 14:21:08 BST 2025 aarch64 GNU/Linux`.
- **Target CPU Architecture**: `aarch64` (ARM 64-bit / Raspberry Pi).
  > [!CRITICAL]
  > Production host is `aarch64`, whereas the development workstation (`pc-1`) is `x86_64`. All Rust binaries (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`) must be cross-compiled for target `aarch64-unknown-linux-gnu` before deployment to `10.0.0.10`. Dev host already has `gcc-aarch64-linux-gnu` installed.

### 3. Service & Package Details
- **Installed Package**: `lyrionmusicserver 9.1.1` (Architecture: `all`, status: `ii`).
- **Live Running Version**: `9.1.1` (verified via CLI socket query on port 9090 `version ?` -> `version 9.1.1`).
- **Systemd Service Unit**: `/usr/lib/systemd/system/lyrionmusicserver.service`
  - Drop-in: `/etc/systemd/system/lyrionmusicserver.service.d/01-core-pinning.conf` (`CPUAffinity=1 2 3`, `MemoryMax=650M`, `MemoryHigh=550M`, `OOMScoreAdjust=500`).
  - Service User: `squeezeboxserver`
  - Service Group: `nogroup`
  - ExecStart: `/usr/sbin/squeezeboxserver --prefsdir /var/lib/squeezeboxserver/prefs --logdir /var/log/squeezeboxserver/ --cachedir /var/lib/squeezeboxserver/cache --charset utf8 --quiet`

### 4. Running Daemons & Process Tree
- **LMS Master Process**: PID 695 (`/usr/bin/perl /usr/sbin/squeezeboxserver`)
- **GDResizer Helper Daemon**: PID 713 (`/usr/bin/perl /usr/sbin/squeezeboxserver-resized`)
- **Plugin Daemon**: PID 715 (`/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/CastBridge/Bin/squeeze2cast-linux-aarch64-static`)
- **Reverse Proxy**: NGINX master/worker processes.

### 5. File System & Module Layout on `10.0.0.10`
- **Main Server Executable**: `/usr/sbin/squeezeboxserver`
- **Resized Helper Executable**: `/usr/sbin/squeezeboxserver-resized` (Perl script listening on UNIX socket `/tmp/sbs_artwork`)
- **Core Perl Modules (`Slim/`)**: `/usr/share/perl5/Slim/`
  - `/usr/share/perl5/Slim/Utils/OS.pm`
  - `/usr/share/perl5/Slim/Utils/GDResizer.pm`
  - `/usr/share/perl5/Slim/Utils/ImageResizer.pm`
  - `/usr/share/perl5/Slim/Utils/Scanner/Local.pm`
- **CPAN Perl Modules & XS Binaries**: `/usr/share/squeezeboxserver/CPAN/`
  - `/usr/share/squeezeboxserver/CPAN/MP3/Cut/Gapless.pm`
  - `/usr/share/squeezeboxserver/CPAN/arch/5.40/aarch64-linux-thread-multi/auto/MP3/Cut/Gapless/Gapless.so`
- **Bundled Helper Binaries**: `/usr/share/squeezeboxserver/Bin/aarch64-linux/`
  - `alac`, `faad`, `flac`, `mac`, `sox`, `wvunpack`
- **Perl Environment**: Perl 5.40.1 (`aarch64-linux-gnu-thread-multi`)
- **Preferences & Configuration**:
  - Prefs Directory: `/var/lib/squeezeboxserver/prefs`
  - Server Prefs File: `/var/lib/squeezeboxserver/prefs/server.prefs`
  - `useLocalImageproxy: 2` (Actively set to `2`, enabling `/usr/sbin/squeezeboxserver-resized`)
  - Cache Directory: `/var/lib/squeezeboxserver/cache`
  - Log Directory: `/var/log/squeezeboxserver` (Log: `/var/log/squeezeboxserver/server.log`)
