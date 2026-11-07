# Work Order: WO-024A

- **ID**: WO-024A
- **Timestamp**: 2026-10-21T21:59:55-04:00
- **Subagent Role**: Research Specialist (Spotify Mix & Protocol Recon)
- **Status**: EXECUTED & VERIFIED

## Task Goal
Perform deep web and environment research on Spotify's playlist "Mix" feature ("Enjoy mixed playlists with the Linux app - Listen to mixed playlists with smooth transitions or mix your very own"), how Spotify implements playlist mixing/Automix/DJ transitions under the hood, whether the official Linux desktop app / Spicetify / `librespot` / Spotty supports it, and how Spotify's APIs (`spclient` / Web API) expose mixability, BPM, key, beatgrids, and transition cue points.

## Scope & Constraints
- **Target Files / Directories**: Read-only web search and local environment check.
- **Rules**: Comply with `/home/console-admin/Github/slimserver/.agents/INFRASTRUCTURE_INVARIANTS.md` and strict read-only constraints.

## Fulfillment Section
- **Status**: `[EXECUTED & VERIFIED]`
- **Subagent**: `14b1f65b-6057-4d65-ad32-6af806470a44` (`Research Specialist`)
- **Summary of Findings**:
  1. **Spotify "Mix" & "Smart Reorder"**: Launched Aug 19, 2025 (custom playlist transitions, BPM/Camelot key display, dual-waveform cue-in/out editor, 3-band EQ bass swap, and real-time beatmatching/time-stretching) and expanded Feb 25, 2026 with **Smart Reorder** (one-tap automatic playlist sequencing by BPM and Camelot key).
  2. **Why Web Player Prompts for Linux App**: `open.spotify.com` runs on HTML5 `<audio>` / EME and lacks Spotify's native C++ `libplayback` multi-deck DSP engine required for real-time time-stretching, beatgrid alignment, and EQ swaps.
  3. **Linux Native App & Spicetify**: Neither `spotify-client` nor `librespot` is currently installed on `10.0.0.90`. Installing `spotify-client` (DEB/Flatpak) + `spicetify-cli` enables native `libplayback` Automix/Mix transitions and allows overriding `RemoteConfigResolver` A/B rollout flags (`mix`, `smart_reorder`) if gated on Linux desktop.
  4. **Under-the-Hood Protocol (`spclient`)**: Playlist transitions are stored in `playlist/v2` protobuf `FormatListAttributes` and per-track `ItemAttributes` (cue-in/out, preset, EQ/volume envelopes). Beatgrids and BPM/key are fetched from internal `wg://audio-attributes/v1/audio-features/{id}` and `audio-analysis/{id}` endpoints.
  5. **Lyrion / `librespot` / Spotty Limitation**: Transitions are rendered strictly client-side in `libplayback`; Spotify Connect and `librespot` (`Issue #1592`) stream discrete tracks without DSP transitions. Public Web API `audio-features`/`audio-analysis` were deprecated Nov 27, 2024 (`HTTP 403`), with workarounds via internal `spclient` tokens, ReccoBeats API, SoundStat API, or local DSP analysis.
