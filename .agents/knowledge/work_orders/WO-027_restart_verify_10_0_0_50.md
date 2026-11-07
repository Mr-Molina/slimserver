# Work Order WO-027: Restart `lyrionmusicserver.service` on `10.0.0.50` & Certify Live `Rust-Experiment` + `DJMix` Operation

- **ID**: WO-027
- **Timestamp**: 2026-11-05T19:04:40-05:00
- **Subagent Role**: Infrastructure Administrator (Live Restart & Certification)
- **Status**: [EXECUTED & VERIFIED]
- **Handoff Path**: `/home/console-admin/Github/slimserver/.agents/knowledge/subagent_summaries/20261105_wo027_live_restart_verify_10_0_0_50.md`

## Scope (Phase 4 of Approved `implementation_plan.md`)
1. **Pre-Restart Process/Port Snapshot (verify-autonomous-work.md)**:
   - On `10.0.0.50`, inspect child processes and open ports (`ps aux | grep -E 'squeezebox|gdresized'`, `ss -tulpn | grep 9000`, `ls -l /tmp/sbs_artwork`).
2. **Restart `lyrionmusicserver.service` on `10.0.0.50`**:
   - Execute `sudo systemctl restart lyrionmusicserver.service` on `10.0.0.50`.
3. **Post-Restart Live Certification on `10.0.0.50`**:
   - Verify `systemctl is-active lyrionmusicserver.service` returns `active`.
   - Verify `pgrep -a gdresized` and `ps aux | grep -E 'squeezebox|gdresized'` confirm `/usr/share/squeezeboxserver/Bin/x86_64-linux/gdresized` is running as the native Rust artwork daemon (and Perl `/usr/sbin/squeezeboxserver-resized` is no longer running).
   - Verify `/tmp/sbs_artwork` UNIX socket exists.
   - Verify HTTP JSON-RPC (`http://10.0.0.50:9000/jsonrpc.js`):
     - `["", ["serverstatus", 0, 50]]` -> HTTP 200, `version: "9.1.1"`, song/album/player counts intact.
     - `["", ["djmix", "status"]]` -> `status: "ok"`, `version: "0.2.0"`, `binary_available: 1`, `spotty_available: 1`.
4. **Update `slimserver` Governance Docs (<= 2 local files)**:
   - Back up `/home/console-admin/Github/slimserver/.agents/knowledge/SYSTEM_STATE.md` and `/home/console-admin/Github/slimserver/.agents/knowledge/DIAGNOSTIC_LOG.md` to `/home/console-admin/.gemini/antigravity/brain/77b8fdd0-8a8a-4c51-8d90-a4aaf30db8bb/scratch/backups/slimserver/` (Rule 35).
   - Update `SYSTEM_STATE.md` and `DIAGNOSTIC_LOG.md` in `/home/console-admin/Github/slimserver/.agents/knowledge/` to document the transition from `10.0.0.10` (`aarch64`) to `10.0.0.50` (`workstation-1`, `x86_64`) and the live verification results.
5. Append a Fulfillment Summary to this Work Order and write the handoff summary to the Handoff Path.

---

## Fulfillment Summary

- **Execution State**: `[EXECUTED & VERIFIED]`
- **Pre-Restart Snapshot**: Confirmed `squeezeboxserver` (PID 7757) running legacy Perl `squeezeboxserver-resized` (PID 7761) and CastBridge (PID 7762), listening on port 9000.
- **Service Restart**: `sudo -n systemctl restart lyrionmusicserver.service` completed with exit code `0`.
- **Post-Restart Daemon State**:
  - `systemctl is-active lyrionmusicserver.service` -> `active`
  - PID 10167 (`squeezeboxserver`) spawned native Rust artwork daemon `/usr/share/squeezeboxserver/Bin/x86_64-linux/gdresized` (PID 10171) listening on `/tmp/sbs_artwork`.
  - Legacy Perl `/usr/sbin/squeezeboxserver-resized` terminated.
  - UNIX socket `/tmp/sbs_artwork` verified active (`srwxr-xr-x 1 squeezeboxserver nogroup`).
- **Post-Restart JSON-RPC Verification**:
  - `serverstatus`: LMS `9.1.1`, total songs: `545`, total albums: `21`, total artists: `203`, connected player: `Guest Bathroom speaker (Cast)`.
  - `djmix status`: `status: "ok"`, `version: "0.2.0"`, `binary_available: 1` (`/var/lib/squeezeboxserver/Plugins/DJMix/Bin/x86_64-linux/lms-djmix`), `spotty_available: 1` (`/var/lib/squeezeboxserver/cache/InstalledPlugins/Plugins/Spotty/Bin/i386-linux/spotty-x86_64`), `preset: "bass_swap"`, `target_lufs: "-14"`.
- **Governance Docs**:
  - Created `/home/console-admin/Github/slimserver/.agents/knowledge/SYSTEM_STATE.md` documenting the live `10.0.0.50` environment.
  - Appended entry 27 to `/home/console-admin/Github/slimserver/.agents/knowledge/DIAGNOSTIC_LOG.md`.
  - Backup archived at `/home/console-admin/.gemini/antigravity/brain/77b8fdd0-8a8a-4c51-8d90-a4aaf30db8bb/scratch/backups/slimserver/DIAGNOSTIC_LOG.md.bak`.
