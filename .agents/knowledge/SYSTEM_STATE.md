# System State (`slimserver` / Lyrion Music Server)

- **Last Updated**: 2026-11-05
- **Repository**: `/home/console-admin/Github/slimserver` (`Rust-Experiment` branch)
- **Project Scope**: Lyrion Music Server (LMS) core server and Rust acceleration stack (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`), plus integration with `Plugins::DJMix` Auto-DJ engine and `Plugins::Spotty`.
- **Deployment & Live Certification State**: `[EXECUTED & VERIFIED]` — Transitioned production LMS from legacy `10.0.0.10` (`aarch64`) to `10.0.0.50` (`workstation-1`, Debian 13 `trixie`, `x86_64-unknown-linux-gnu`, Lyrion `9.1.1`). Live restart verified via Work Order `WO-027`: `gdresized` (PID 10171) spawned under `squeezeboxserver`, listening on `/tmp/sbs_artwork` socket (legacy Perl resizer terminated), JSON-RPC `serverstatus` active (545 songs, 21 albums, 203 artists), and `Plugins::DJMix` v0.2.0 confirmed operational (`binary_available: 1`, `spotty_available: 1`).

---

## 1. Host & Target Environments

| Environment | Hostname / IP | Architecture & OS | Role & Paths | Status |
| :--- | :--- | :--- | :--- | :--- |
| **Production Lyrion Host** | `10.0.0.50` (`workstation-1`) | Debian 13 (`trixie`) `x86_64-unknown-linux-gnu` | Live production host. LMS `9.1.1` (`lyrionmusicserver.service`, port `9000`). Native Rust artwork resizer daemon `/usr/share/squeezeboxserver/Bin/x86_64-linux/gdresized` listening on `/tmp/sbs_artwork`. `Plugins::DJMix` deployed at `/var/lib/squeezeboxserver/Plugins/DJMix/`. | `[EXECUTED & VERIFIED]` Live & Operational |
| **Legacy Production Host** | `10.0.0.10` (`lyrion.molinadeveloping.com`) | Debian 13 (`trixie`) `aarch64-unknown-linux-gnu` | Former production host; retained for historical reference and `aarch64` cross-build verification. Deployed LMS `9.1.1` with `aarch64` Rust binaries. | Retained / Inactive for Primary Production |
| **Development Host** | `10.0.0.90` (local) | Debian 13 (`trixie`) `x86_64-unknown-linux-gnu` | Local development and build workstation; repository root `/home/console-admin/Github/slimserver`. | Active |

---

## 2. Verified Daemons & Services on `10.0.0.50`

- **LMS Service**: `lyrionmusicserver.service` (PID 10167 `squeezeboxserver`).
- **Artwork Daemon**: `/usr/share/squeezeboxserver/Bin/x86_64-linux/gdresized` (PID 10171), listening on `/tmp/sbs_artwork` UNIX domain socket.
- **CastBridge Helper**: `/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/CastBridge/Bin/squeeze2cast-linux-x86-64-static` (PID 10172).
- **HTTP / JSON-RPC Port**: `0.0.0.0:9000` (LISTEN).
- **DJMix Plugin**: `/var/lib/squeezeboxserver/Plugins/DJMix/Bin/x86_64-linux/lms-djmix` (`v0.2.0`).
- **Spotty Helper**: `/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/Spotty/Bin/i386-linux/spotty-x86_64`.

---

## 3. Verified JSON-RPC Endpoints on `http://10.0.0.50:9000/jsonrpc.js`

1. **`serverstatus`**:
   - `version`: `9.1.1`
   - `info total songs`: `545`
   - `info total albums`: `21`
   - `info total artists`: `203`
   - Active player: `Guest Bathroom speaker (Cast)` (connected: `1`)

2. **`djmix status`**:
   - `status`: `ok`
   - `version`: `0.2.0`
   - `binary_available`: `1` (`/var/lib/squeezeboxserver/Plugins/DJMix/Bin/x86_64-linux/lms-djmix`)
   - `spotty_available`: `1` (`/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/Spotty/Bin/i386-linux/spotty-x86_64`)
   - `reorder`: `1`, `transition_bars`: `8`, `preset`: `bass_swap`, `target_lufs`: `-14`

---

## 4. Deep Hardware & Software Memory Leak Audit & Remediation (2026-11-05 — WO-028 through WO-032)

- **Status**: `[EXECUTED & VERIFIED]` — Full deep hardware audit (`10.0.0.50`) and deep software/memory leak audit completed across Perl core and Rust binaries.
- **Perl Core Hardening (`WO-028`)**:
  - Remediated `transcoded` method compatibility issue in `Slim::Control::Queries` and `Slim::Player::Song`, eliminating the 108 lines/min Home Assistant polling exception storm on `10.0.0.50`.
  - Added child PID capture and non-blocking `WNOHANG` reaping in `Slim::Player::Pipeline` to prevent zombie transcode helper processes.
  - Weakened `$client` closure references in `Slim::Player::Source` to prevent client object memory leak cycles.
- **Rust Binaries & Crates Hardening (`WO-029`, `WO-030`)**:
  - `lms-gdresized`: Enforced bounded IPC line reads (`64 KiB`), stdin cap (`64 MiB`), socket timeouts (`15s`), and orphan PPID exit watchdog.
  - `lms-image`: Bounded image dimensions (`16384` source / `8192` target) to mitigate decompression bombs.
  - `lms-transcode`: Added graceful `BrokenPipe` exit `0` and `2 GiB` stdin cap.
  - `lms-mp3cut`: Added `catch_unwind` FFI panic boundary guards and safety invariants.
  - `lms-audioscan`: Capped inline Base64 artwork parsing at `16 MiB`.
  - `lms-scanner-accel`: Batched SQLite transactions (`5000` rows) with explicit WAL flush control.
- **Compilation, Deployment & Hardware Live Verification (`WO-031`, `WO-032`)**:
  - Rebuilt all 5 `x86_64` release binaries (`35/35` cargo tests, `0` clippy warnings, `0` style audit errors) and deployed with 100% SHA-256 parity to `/usr/share/squeezeboxserver/Bin/x86_64-linux/` on `10.0.0.50`.
  - Remediated Avahi mDNS conflict on `10.0.0.50` (`enp0s25` / `#host-name=lyrion`), achieving `0` mDNS collisions and `0` server log exceptions post-restart.
