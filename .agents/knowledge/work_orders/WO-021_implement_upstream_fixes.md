# Work Order: WO-021 — Implement Upstream Bug Fixes, PRs & CLI `--help` Support on `Rust-Experiment`

- **ID**: WO-021
- **Timestamp**: 2026-10-20T20:41:30-04:00
- **Subagent Role**: Task 8 Implementer (`cca0ce89-2d26-4f22-8e50-bca00ee4d6e1`)
- **Status**: [EXECUTED & VERIFIED]

## 1. Task Goal
Implement all 10 upstream bug fixes, open PR improvements, and CLI `--help` ergonomics improvements on `Rust-Experiment`, rebuild both `x86_64` and `aarch64` release binaries, add unit/integration test assertions in `t/rust_integration.t`, and verify all Rust and Perl test suites pass.

## 2. Pre-Modification Backups (Rules 30, 31, 35)
Before editing any Perl files, copy backups to `/home/console-admin/Github/slimserver/.agents/backups/`:
- `Slim/Formats/Movie.pm` -> `.agents/backups/Movie.pm.bak`
- `Slim/Player/Pipeline.pm` -> `.agents/backups/Pipeline.pm.bak`
- `Slim/Player/Source.pm` -> `.agents/backups/Source.pm.bak`
- `Slim/Networking/Async/HTTP.pm` -> `.agents/backups/HTTP.pm.bak`
- `Slim/Formats/MP3.pm` -> `.agents/backups/MP3.pm.bak`
- `Slim/Utils/Scanner/Local.pm` -> `.agents/backups/Local.pm.bak`
- `Slim/Utils/Misc.pm` -> `.agents/backups/Misc.pm.bak`
- `Slim/Formats/Playlists/Base.pm` -> `.agents/backups/Playlists_Base.pm.bak`
- `Slim/Schema.pm` -> `.agents/backups/Schema.pm.bak`
- `Slim/Control/Queries.pm` -> `.agents/backups/Queries.pm.bak`
- `Slim/Networking/Slimproto.pm` -> `.agents/backups/Slimproto.pm.bak`

## 3. Target Modifications (Read exact target lines in `.agents/knowledge/work_orders/WO-020_upstream_fixes_recon.md`)
1. **Target 1 — Issue #1692 (`/home/console-admin/Github/slimserver/Slim/Formats/Movie.pm`)**:
   - At lines 159–161, unwrap array ref before calling `sanitizeYearTag`:
     ```perl
     	if ( defined $tags->{YEAR} ) {
     		my $year = $tags->{YEAR};
     		$year = $year->[0] if ref $year eq 'ARRAY';
     		$tags->{YEAR} = $class->sanitizeYearTag($year);
     	}
     ```
2. **Target 2 — PR #1656 (`/home/console-admin/Github/slimserver/Slim/Player/Pipeline.pm` & `/home/console-admin/Github/slimserver/Slim/Player/Source.pm`)**:
   - Apply the exact PR #1656 diff (`gh pr diff 1656 --repo LMS-Community/slimserver`):
     - In `Pipeline.pm`: call `Slim::Networking::Select::removeWrite($writer);` before `$writer->close();` at both EOF and `close`.
     - In `Source.pm`: in `_readNextChunk`, if `${*$fd}{'pipeline_pending_size'} && defined ${*$fd}{'pipeline_writer'}`, call `Slim::Networking::Select::addWrite(${*$fd}{'pipeline_writer'}, sub {_wakeupOnWritable(shift, $client);}, 1);`, and refactor `_wakeupOnReadable` / `_wakeupOnWritable` to share `_wakeupStream($master)`.
3. **Target 3 — PR #1696 (`/home/console-admin/Github/slimserver/Slim/Networking/Async/HTTP.pm`)**:
   - In `read_body` (around lines 330–332), after `addRead( $self->socket, \&_http_read_body )`:
     ```perl
     	my $socket = $self->socket;
     	if ( $socket->_rbuf_length || ( $socket->can('pending') && $socket->pending() ) ) {
     		_http_read_body( $socket, $self, $args );
     	}
     ```
4. **Target 4 — Issue #1650 (`/home/console-admin/Github/slimserver/Slim/Formats/MP3.pm`)**:
   - At lines 406–414, preserve `ORIGINALYEAR` from `TDOR`/`XDOR` and prioritize `TDRC`/`TYER` over `TDOR`/`XDOR` for `YEAR`:
     ```perl
     		my $year;
     		$tags->{ORIGINALYEAR} ||= $tags->{TDOR} || $tags->{XDOR};
     		foreach ( qw(TDRC TYER TDOR XDOR TDAT) ) {
     			$year ||= $tags->{$_};
     			delete $tags->{$_};
     		}
     		$tags->{YEAR} ||= $year if $year;
     ```
5. **Target 5 — Issues #1256 & #1404 (`/home/console-admin/Github/slimserver/Slim/Utils/Scanner/Local.pm`)**:
   - At lines 84–88, replace `(stat _)[9], (stat _)[7]` inside `$sth->execute(...)` with explicit scalars so a failed/invalidated `stat _` never collapses 3 bind variables into 1:
     ```perl
     			my @stat = stat $path;
     			my $mtime = $stat[9] || 0;
     			my $size  = $stat[7] || 0;
     			$sth->execute(
     				$file,
     				$mtime, # mtime
     				$size,  # size
     			);
     ```
6. **Target 6 — Issue #1677 (`Slim/Utils/Misc.pm`, `Slim/Formats/Playlists/Base.pm`, `Slim/Utils/Scanner/Local.pm`)**:
   - In `Slim/Utils/Misc.pm` (`sub inMediaFolder`): accept optional `$type` (`my ($path, $type) = @_; my $mediadirs = getMediaDirs($type);`).
   - In `Slim/Formats/Playlists/Base.pm` (`sub _updateMetaData`): before calling `Slim::Schema->updateOrCreate` when `!defined $track`, if `Slim::Music::Info::isFileURL($entry)` and `scalar grep { $_ } @{ Slim::Utils::Misc::getAudioDirs() }`, return `undef` unless `Slim::Utils::Misc::inMediaFolder($entry, 'audio') || Slim::Utils::Misc::inPlaylistFolder($entry)`.
   - In `Slim/Utils/Scanner/Local.pm` (`sub deleted`, around line 769 before `DELETE FROM tracks WHERE id = ?`): also delete any `playlist_track` entries referencing `$url` (`my $psth = $dbh->prepare_cached('DELETE FROM playlist_track WHERE track = ?'); $psth->execute($url);`).
7. **Target 7 — Issue #1673 (`/home/console-admin/Github/slimserver/Slim/Schema.pm`)**:
   - In `_newTrack` (line 1805) and `_postCheckAttributes` (line 3073), pass `$contributors->{'COMPOSER'} || $contributors->{'ALBUMARTIST'} || $contributors->{'ARTIST'}` to `_createWork`.
   - In `_createWork($self, $work, $workSort, $composerOrList, $create)`: accept either a scalar composer ID or an arrayref of composer IDs (`my @composerIDs = ref $composerOrList eq 'ARRAY' ? @$composerOrList : ($composerOrList ? ($composerOrList) : ());`). Check if any `$cid` in `@composerIDs` already has `SELECT id FROM works WHERE titlesearch = ? AND composer = ?`; if still not found and `$lastAlbum && $lastAlbum->{id}`, check `SELECT works.id FROM works JOIN tracks ON tracks.work = works.id WHERE works.titlesearch = ? AND tracks.album = ? LIMIT 1` so multi-movement arrangements on the same album sharing the same `WORK` tag group into a single work.
8. **Target 8 — Issue #1665 (`/home/console-admin/Github/slimserver/Slim/Control/Queries.pm`)**:
   - At lines 346–348 in `albumsQuery`:
     ```perl
     				if ($tags ne 'CC') {
     					if (defined $sort && $sort ne 'album') {
     						$order_by = "albumsSearch.fulltextweight DESC, $sort";
     					} else {
     						$order_by = $sort = "albumsSearch.fulltextweight DESC, albums.titlesort";
     					}
     				}
     ```
9. **Target 9 — Issue #1580 (`/home/console-admin/Github/slimserver/Slim/Networking/Slimproto.pm`)**:
   - At line 155 in `slimproto_accept`, add `setsockopt ($clientsock, Socket::SOL_SOCKET(), Socket::SO_KEEPALIVE(), 1);` immediately after `TCP_NODELAY`.
10. **Target 10 — `--help` / `-h` Support in `lms-transcode` & `lms-gdresized` + Integration Test Assertions**:
    - In `/home/console-admin/Github/slimserver/rust/crates/lms-transcode/src/main.rs` (`fn main`) and `/home/console-admin/Github/slimserver/rust/crates/lms-gdresized/src/main.rs` (`fn main`), check if `args[1..].iter().any(|a| a == "--help" || a == "-h")`; if so, print usage to `stdout` and `return;`.
    - Rebuild both `x86_64` and `aarch64` release binaries and copy them to `Bin/x86_64-linux/` and `Bin/aarch64-linux/`.
    - Extend `/home/console-admin/Github/slimserver/t/rust_integration.t` from `1..9` to `1..12` to also verify:
      - `ok 10`: all 5 Rust binaries (`gdresized`, `lms-transcode`, `lms-mp3cut`, `lms-audioscan`, `lms-scanner-accel`) exit `0` on `--help` without blocking on `stdin` or socket binding.
      - `ok 11`: `Slim::Formats::Movie` unwraps arrayref `YEAR => ['1987', '1987']` to `1987` (Issue #1692 regression test).
      - `ok 12`: `Slim::Formats::MP3::doTagMapping` prioritizes `TDRC` (`2000`) over `TDOR` (`1966`) while preserving `ORIGINALYEAR = 1966` (Issue #1650 regression test).

## 4. Verification & Git Commit
- Run `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, `t/rust_integration.t` (`1..12`), `t/gdresizer_explicit_format.t` (`1..2`), `t/verifyStrings.pl`, and `t/00_smoketest.sh`.
- Commit all modified tracked files to `Rust-Experiment` (`git commit -m "fix: incorporate upstream PRs (#1656, #1696), bugfixes (#1692, #1650, #1256, #1677, #1673, #1665, #1580), and CLI --help support"`).

## 5. Fulfillment Section
- **Status**: `[EXECUTED & VERIFIED]`
- **Completed At**: 2026-10-20T20:53:19-04:00
- **Commit**: `30eebde2f` (`fix: incorporate upstream PRs (#1656, #1696), bugfixes (#1692, #1650, #1256, #1677, #1673, #1665, #1580), and CLI --help support`)
- **Backups Created**: All 11 files backed up in `.agents/backups/`
- **Key Artifacts Modified**:
  - `Slim/Formats/Movie.pm`, `Slim/Player/Pipeline.pm`, `Slim/Player/Source.pm`, `Slim/Networking/Async/HTTP.pm`, `Slim/Formats/MP3.pm`
  - `Slim/Utils/Scanner/Local.pm`, `Slim/Utils/Misc.pm`, `Slim/Formats/Playlists/Base.pm`, `Slim/Schema.pm`, `Slim/Control/Queries.pm`, `Slim/Networking/Slimproto.pm`
  - `rust/crates/lms-transcode/src/main.rs`, `rust/crates/lms-gdresized/src/main.rs`, `rust/crates/lms-audioscan/src/main.rs`
  - `Bin/x86_64-linux/*`, `Bin/aarch64-linux/*`
  - `t/rust_integration.t` (expanded to 12 tests)
  - Subagent Summary: `/home/console-admin/Github/slimserver/.agents/knowledge/subagent_summaries/20261020_wo021_upstream_fixes.md`
- **Verification Evidence**:
  - `cargo clippy --all-targets -- -D warnings`: Exit code 0, 0 warnings
  - `cargo test --all-targets`: Exit code 0, 37/37 tests pass
  - `perl t/rust_integration.t`: Exit code 0, 12/12 tests pass
  - `perl t/gdresizer_explicit_format.t`: Exit code 0, 2/2 tests pass
  - `perl t/verifyStrings.pl`: Exit code 0, 0 errors
  - `bash t/00_smoketest.sh`: Exit code 0, live server status & date verified
