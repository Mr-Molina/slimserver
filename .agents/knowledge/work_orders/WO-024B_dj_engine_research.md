# Work Order: WO-024B

- **ID**: WO-024B
- **Timestamp**: 2026-10-21T21:59:55-04:00
- **Subagent Role**: Research Specialist (Lyrion / Rust / Open-Source DJ Mix Engine)
- **Status**: EXECUTED & VERIFIED

## Task Goal
Perform deep research across the web and the `/home/console-admin/Github/slimserver` codebase on recreating automatic DJ playlist mixing (1. organizing tracks by harmonic key, BPM, and mixability; 2. beat-matched tempo-ramping/time-stretching transitions between tracks). Investigate Lyrion Music Server's playback/crossfade/Spotty architecture, existing LMS plugins (`BlissMixer`, `DontStopTheMusic`), Rust audio analysis/DSP crates (`bliss-rs`, `aubio-rs`, `stratum-dsp`, `rubato`, `signalsmith-stretch`, `soundtouch`), and standalone open-source auto-DJ engines (`Mixxx`, `Liquidsoap`, Python/Rust auto-mixers).

## Scope & Constraints
- **Target Files / Directories**: `/home/console-admin/Github/slimserver` (read-only) and web search.
- **Rules**: Comply with `/home/console-admin/Github/slimserver/.agents/INFRASTRUCTURE_INVARIANTS.md` and strict read-only constraints.

## Fulfillment Section
- **Status**: `[EXECUTED & VERIFIED]`
- **Subagent**: `79679e8c-184c-4e8c-a8c3-a630ae0d66ce` (`Research Specialist`)
- **Summary of Findings**:
  1. **LMS Playback & Crossfade Limits**: `Squeezebox2.pm` / `Squeezebox.pm` delegates crossfading (`transitionType`, max 10s) to the player ring buffer via the 24-byte `strm` header without beatgrids, time-stretching, or 3-band EQ bass swaps, and aborts on sample-rate mismatches (`transitionSampleRestriction`).
  2. **Pure-Rust Track Analysis (`stratum-dsp`)**: `stratum-dsp` provides 100% pure-Rust BPM, HMM beatgrid timestamps, Camelot key (`1A`–`12B`), and ITU-R BS.1770-4 LUFS analysis (superior to `bliss-rs`, which only computes a 20-float Euclidean similarity vector without Camelot keys or beatgrids).
  3. **Pure-Rust Beat-Matched Time-Stretching (`timestretch` / `signalsmith-stretch` / `rubato`)**: `timestretch` (`timestretch-rs`, MIT, pure Rust) and `signalsmith-stretch` provide real-time DJ tempo ramping and beatgrid snapping without pitch shift, plus 3-band Linkwitz-Riley / biquad EQ bass-swap crossfading.
  4. **Harmonic + BPM TSP Ordering**: Formulates transition cost $C(i,j)$ over Camelot wheel distance + octave-folded BPM delta + energy contour, solvable via Held-Karp DP ($N \le 20$) or 2-opt/Or-opt ($N > 20$) to reorder Spotify playlists (`PUT /v1/playlists/{id}/tracks`) or LMS queues (`Slim::Player::Playlist`).
  5. **LMS Server-Side Integration**: Can be wired either via Track-Tail Transition Folding in `lms-transcode` (`--mix-next`) + URL `#start-end` slicing (`TranscodingHelper.pm#L542`) or via a continuous `djmix://` protocol handler (`Squeezebox.pm#L961`), supporting both local tracks and `Spotty` (`spt://`) streams.
