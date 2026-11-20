package Slim::Formats::Movie;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;
use base qw(Slim::Formats);
use Fcntl qw(:seek SEEK_SET);

use Audio::Scan;

use Slim::Formats::MP3;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::SoundCheck;

my %tagMapping = (
	AART => 'ALBUMARTIST',	# bug 10724 - support aART as Album Artist
	ALB  => 'ALBUM',
	ART  => 'ARTIST',
	CMT  => 'COMMENT',
	COVR => 'ARTWORK',
	CON  => 'CONDUCTOR',
	CPIL => 'COMPILATION',
	DAY  => 'YEAR',
	GNRE => 'GENRE',
	GEN  => 'GENRE',
	LYR  => 'LYRICS',
	NAM  => 'TITLE',
	SOAA => 'ALBUMARTISTSORT',
	SOAL => 'ALBUMSORT',
	SOAR => 'ARTISTSORT',
	SOCO => 'COMPOSERSORT',
	SONM => 'TITLESORT',
	TMPO => 'BPM',
	TRKN => 'TRACKNUM',
	WRT  => 'COMPOSER',
	WRK  => 'WORK',
	GRP  => 'GROUPING',
	'MusicBrainz Album Id'     => 'MUSICBRAINZ_ALBUM_ID',
	'MusicBrainz Album Type'   => 'RELEASETYPE',
	'MusicBrainz Artist Id'    => 'MUSICBRAINZ_ARTIST_ID',
	'MusicBrainz Album Artist' => 'ALBUMARTIST',
	'MusicBrainz Album Artist Id' => 'MUSICBRAINZ_ALBUMARTIST_ID',
	'MusicBrainz Track Id'     => 'MUSICBRAINZ_ID',
	'MusicBrainz Release Track Id' => 'MUSICBRAINZ_RELEASETRACKID',
	'MusicBrainz Sortname'     => 'ARTISTSORT',
	'MusicBrainz Album Status' => 'MUSICBRAINZ_ALBUM_STATUS',
);

sub _runLmsAudioscan {
	my ($subcmd, @args) = @_;
	return undef unless ($ENV{LMS_USE_RUST} // 1);

	my $bin = eval { Slim::Utils::Misc::findbin('lms-audioscan') };
	return undef unless defined $bin && -x $bin;

	my @cmd_args = ($subcmd, @args);
	my $json = eval {
		open(my $pipe, '-|', $bin, @cmd_args) or die "open failed: $!";
		binmode($pipe, ':raw');
		local $/;
		my $out = <$pipe>;
		close($pipe);
		die "exit $?" if $? != 0;
		$out;
	};
	return undef if $@ || !defined $json || !length $json;

	my $data = eval {
		require JSON::XS;
		JSON::XS::decode_json($json);
	};
	return undef if $@ || ref $data ne 'HASH';

	if (!$ENV{AUDIO_SCAN_NO_ARTWORK} && ref $data->{tags} eq 'HASH' && defined $data->{tags}->{COVR}) {
		my $covr = $data->{tags}->{COVR};
		if (!ref($covr) && $covr !~ /^\d+$/) {
			eval {
				require MIME::Base64;
				$data->{tags}->{COVR} = MIME::Base64::decode_base64($covr);
			};
		}
	}

	return $data;
}

sub _mergeAudioscanFallback {
	my ($target, $fallback) = @_;
	return $target unless $fallback && ref $fallback eq 'HASH';
	return $fallback unless $target && ref $target eq 'HASH';

	if (ref $fallback->{info} eq 'HASH') {
		$target->{info} = {} unless ref $target->{info} eq 'HASH';
		my $tinfo = $target->{info};
		my $finfo = $fallback->{info};

		$tinfo->{dlna_profile} = $finfo->{dlna_profile}
			if !defined $tinfo->{dlna_profile} && defined $finfo->{dlna_profile};
		$tinfo->{song_length_ms} = $finfo->{song_length_ms}
			if !defined $tinfo->{song_length_ms} && defined $finfo->{song_length_ms};

		if (ref $finfo->{tracks} eq 'ARRAY' && @{$finfo->{tracks}}) {
			if (ref $tinfo->{tracks} ne 'ARRAY' || !@{$tinfo->{tracks}}) {
				$tinfo->{tracks} = $finfo->{tracks};
			}
			else {
				for my $i (0 .. $#{$finfo->{tracks}}) {
					my $ft = $finfo->{tracks}->[$i];
					next unless ref $ft eq 'HASH';
					$tinfo->{tracks}->[$i] ||= {};
					my $st = $tinfo->{tracks}->[$i];
					next unless ref $st eq 'HASH';
					for my $k (keys %$ft) {
						$st->{$k} = $ft->{$k} if !defined $st->{$k} && defined $ft->{$k};
					}
				}
			}
		}
	}

	if (ref $fallback->{tags} eq 'HASH' && (ref $target->{tags} ne 'HASH' || !keys %{$target->{tags}})) {
		$target->{tags} = $fallback->{tags};
	}

	return $target;
}

sub getTag {
	my $class = shift;
	my $file  = shift || return {};

	my @scan_opts = $ENV{AUDIO_SCAN_NO_ARTWORK} ? ('--no-artwork') : ();
	my $s = _runLmsAudioscan('scan', @scan_opts, $file);
	if (!$s || ref $s ne 'HASH' || ref $s->{info} ne 'HASH' || !$s->{info}->{song_length_ms}) {
		$s = Audio::Scan->scan( $file );
	}
	else {
		my $info = $s->{info};
		my $tracks = (ref $info->{tracks} eq 'ARRAY') ? $info->{tracks} : [];
		my $is_mp4_mov = (@$tracks > 0 || $file =~ /\.(?:mp4|m4[abprv]|mov|k3g|skm|3g[p2])$/i) ? 1 : 0;
		my $needs_track_fields = $is_mp4_mov && (
			!@$tracks
			|| grep {
				ref $_ ne 'HASH'
				|| !defined $_->{audio_object_type}
				|| !defined $_->{audio_type}
			} @$tracks
		);
		if (!defined $info->{dlna_profile} || $needs_track_fields) {
			my $fallback;
			if ($is_mp4_mov && open(my $fh, '<:raw', $file)) {
				$fallback = Audio::Scan->scan_fh( mp4 => $fh, { filter => 0x01 } );
				close $fh;
			}
			$fallback ||= Audio::Scan->scan( $file );
			$s = _mergeAudioscanFallback($s, $fallback);
		}
	}
	return unless $s && ref $s eq 'HASH';

	my $info = $s->{info};
	my $tags = $s->{tags};

	return unless $info && ref $info eq 'HASH' && $info->{song_length_ms};
	$tags = {} unless $tags && ref $tags eq 'HASH';

	# skip files with video tracks
	my $tracks = (ref $info->{tracks} eq 'ARRAY') ? $info->{tracks} : [];
	for my $track (@$tracks) {
		return if ref $track eq 'HASH' && exists $track->{width};
	}

	# map the existing tag names to the expected tag names
	$class->_doTagMapping($tags);

	$tags->{OFFSET}       = 0;
	$tags->{RATE}         = $info->{samplerate};
	$tags->{SIZE}         = $info->{file_size};
	$tags->{SECS}         = $info->{song_length_ms} / 1000;
	$tags->{BITRATE}      = $info->{avg_bitrate};
	$tags->{DLNA_PROFILE} = $info->{dlna_profile} || undef;
	$tags->{LEADING_MDAT} = $info->{leading_mdat} || undef;

	if ( my $track = $tracks->[0] ) {
		# MP4 file
		$tags->{SAMPLESIZE} = $track->{bits_per_sample};
		$tags->{CHANNELS}   = $track->{channels};

		# If encoding is alac, the file is lossless.
		if ( $track->{encoding} && $track->{encoding} eq 'alac' ) {
			$tags->{LOSSLESS}     = 1;
			$tags->{VBR_SCALE}    = 1;
			$tags->{CONTENT_TYPE} = 'alc';
		}
		elsif ( $track->{encoding} && $track->{encoding} eq 'drms' ) {
			$tags->{DRM} = 1;
		}

		# Check for HD-AAC file, if the file has 2 tracks and AOTs of 2/37
		if ( defined $track->{audio_object_type} && (my $track2 = $tracks->[1]) ) {
			if ( defined $track2->{audio_object_type} && $track->{audio_object_type} == 2 && $track2->{audio_object_type} == 37 ) {
				$tags->{LOSSLESS}     = 1;
				$tags->{VBR_SCALE}    = 1;
				$tags->{SAMPLESIZE}   = $track2->{bits_per_sample};
				$tags->{CONTENT_TYPE} = 'sls';
			}
		}
	}
	elsif ( $info->{bitrate} ) {
		# ADTS file
		$tags->{OFFSET}   = $info->{audio_offset}; # ID3v2 tags may be present
		$tags->{BITRATE}  = $info->{bitrate};
		$tags->{CHANNELS} = $info->{channels};

		if ( $info->{id3_version} ) {
			$tags->{TAGVERSION} = $info->{id3_version};

			Slim::Formats::MP3->doTagMapping($tags);
		}
	}

	return $tags;
}

sub getCoverArt {
	my $class = shift;
	my $file  = shift || return undef;

	# Enable artwork in Audio::Scan
	local $ENV{AUDIO_SCAN_NO_ARTWORK} = 0;

	my $s = _runLmsAudioscan('scan-tags', $file) || Audio::Scan->scan_tags($file);
	return unless $s && ref $s eq 'HASH' && ref $s->{tags} eq 'HASH';

	return $s->{tags}->{COVR};
}

sub _doTagMapping {
	my ($class, $tags) = @_;

	# map iTunes specific keys
	foreach my $old (keys %$tags) {
		my $new = $old;
		$new =~ s/^----:com.apple.iTunes://i;
		$tags->{$new} ||= $tags->{$old};
	}

	# map the existing tag names to the expected tag names
	while ( my ($old, $new) = each %tagMapping ) {
		foreach ($old, uc($old)) {
			if ( exists $tags->{$_} ) {
				$tags->{$new} = delete $tags->{$_};
				last;
			}
		}
	}

	# Prefer MUSICBRAINZ_RELEASETRACKID to MusicBrainz Track Id (MUSICBRAINZ_ID)
	$tags->{MUSICBRAINZ_ID} = delete $tags->{MUSICBRAINZ_RELEASETRACKID} if $tags->{MUSICBRAINZ_RELEASETRACKID};

	# Special handling for DATE tags
	# Parse the date down to just the year, for compatibility with other formats
	if ( defined $tags->{YEAR} ) {
		my $year = $tags->{YEAR};
		$year = $year->[0] if ref $year eq 'ARRAY';
		$tags->{YEAR} = $class->sanitizeYearTag($year);
	}

	# Unroll the disc info.
	if ( $tags->{DISK} ) {
		($tags->{DISC}, $tags->{DISCC}) = split /\//, delete $tags->{DISK};
	}

	# Look for iTunes SoundCheck data, unless we have a TXXX track gain tag
	if ( !$tags->{REPLAYGAIN_TRACK_GAIN} ) {
		if ( $tags->{ITUNNORM} ) {
			$tags->{REPLAYGAIN_TRACK_GAIN} = Slim::Utils::SoundCheck::normStringTodB( delete $tags->{ITUNNORM} );
		}
	}

	# Flag if we have embedded cover art
	if ( $tags->{ARTWORK} ) {
		if ( $ENV{AUDIO_SCAN_NO_ARTWORK} ) {
			# In 'no artwork' mode, ARTWORK is the length
			$tags->{COVER_LENGTH} = $tags->{ARTWORK};
		}
		else {
			$tags->{COVER_LENGTH} = length( $tags->{ARTWORK} );
		}
	}
}

# never used for ADTS because there is no initial block
sub volatileInitialAudioBlock { 1 }

sub getInitialAudioBlock {
	my ($class, $fh, $track, $time) = @_;

	my $sourcelog = logger('player.source');

	# When playing the start of a virtual cue sheet track, findFrameBoundaries will not have been called
	if ( !exists ${*$fh}{_mp4_seek_header} && $track->url =~ /#([^-]+)-([^-]+)$/ ) {
		$class->findFrameBoundaries( $fh, undef, $1 );
	}

	# no seek header => ADTS file and no need of InitialAudioBlock
	return undef if (!exists ${*$fh}{_mp4_seek_header});

	main::INFOLOG && $sourcelog->is_info && $sourcelog->info(
	    'Reading initial audio block: length ' . length( ${ ${*$fh}{_mp4_seek_header} } )
	);

	return ${ delete ${*$fh}{_mp4_seek_header} };
}

sub _validateAdtsStream {
	my ($fh) = @_;
	return 0 unless defined $fh;

	seek($fh, 0, SEEK_SET) or return 0;

	# Skip optional ID3v2 header if present
	my $id3_hdr = '';
	my $n = read($fh, $id3_hdr, 10);
	return 0 unless defined $n && $n >= 7;

	my $audio_offset = 0;
	if ($n == 10 && substr($id3_hdr, 0, 3) eq 'ID3') {
		my ($b5, $b6, $b7, $b8, $b9) = unpack('x5 C5', $id3_hdr);
		if ($b6 < 0x80 && $b7 < 0x80 && $b8 < 0x80 && $b9 < 0x80) {
			my $id3_size = ($b6 << 21) | ($b7 << 14) | ($b8 << 7) | $b9;
			$audio_offset = 10 + $id3_size + (($b5 & 0x10) ? 10 : 0);
		}
	}

	seek($fh, $audio_offset, SEEK_SET) or return 0;

	my $buf = '';
	my $eof = 0;
	my $pos = 0;
	my $valid_frames = 0;
	my $max_scan_junk = 65536;

	while (1) {
		if (!$eof && (length($buf) - $pos) < 8192) {
			substr($buf, 0, $pos, '') if $pos > 0;
			$pos = 0;
			my $chunk = '';
			my $got = read($fh, $chunk, 65536);
			return 0 unless defined $got;
			if ($got == 0) {
				$eof = 1;
			}
			else {
				$buf .= $chunk;
			}
		}

		my $buflen = length($buf);
		last if $pos + 7 > $buflen;

		my $b0 = ord(substr($buf, $pos, 1));
		my $b1 = ord(substr($buf, $pos + 1, 1));

		if ($b0 != 0xFF || ($b1 & 0xF6) != 0xF0) {
			last if $valid_frames > 0;
			$pos++;
			$max_scan_junk--;
			return 0 if $max_scan_junk <= 0;
			next;
		}

		my $min_hdr = ($b1 & 0x01) ? 7 : 9;
		if ($pos + $min_hdr > $buflen) {
			return 0 if $valid_frames == 0;
			last;
		}

		my $freq_idx = (ord(substr($buf, $pos + 2, 1)) >> 2) & 0x0F;
		return 0 if $freq_idx == 15;

		my $frame_length = (unpack('N', substr($buf, $pos + 3, 4)) >> 13) & 0x1FFF;
		return 0 if $frame_length < 7 || $frame_length < $min_hdr;

		$valid_frames++;
		last if $pos + $frame_length > $buflen;

		$pos += $frame_length;
	}

	return $valid_frames > 0 ? 1 : 0;
}

sub findFrameBoundaries {
	my ($class, $fh, $offset, $time) = @_;

	if (!defined $fh || (!defined $time && !defined $offset)) {
		return 0;
	}

	# Need a localFh to have own seek pointer
	open(my $localFh, '<&=', $fh) or return -1;
	$localFh->seek(0, SEEK_SET);

	my $info = defined $time
		? Audio::Scan->find_frame_fh_return_info( mp4 => $localFh, int($time * 1000) )
		: undef;

	if ($info && ref $info eq 'HASH' && ref $info->{tracks} eq 'ARRAY' && $info->{tracks}->[0]) {
		# Since getInitialAudioBlock will be called right away, stash the new seek header so
		# we don't have to scan again
		${*$fh}{_mp4_seek_header} = \($info->{seek_header});
		return $info->{seek_offset};
	}
	else {
		# ADTS, validate frame header bytes before XS scan to avoid infinite loop on len==0
		return -1 unless _validateAdtsStream($localFh);

		# ADTS, need to scan bitrate
		seek($localFh, 0, SEEK_SET);
		my $scan = Audio::Scan->scan_fh( aac => $localFh );
		my $aac_info = ($scan && ref $scan eq 'HASH' && ref $scan->{info} eq 'HASH') ? $scan->{info} : {};

		if (defined $time) {
			return -1 unless $aac_info->{bitrate} && $aac_info->{bitrate} > 0;
			$offset = int($aac_info->{bitrate} * $time / 8) + ($aac_info->{audio_offset} || 0);
		}
		else {
			$offset = abs($offset);
		}

		# an ADTS frame is max 8191 bytes, so we'll capture one for sure
		seek($localFh, $offset, SEEK_SET);
		my $bytes_read = read($localFh, my $buffer, 16384);
		return -1 unless defined $bytes_read && $bytes_read >= 7;

		my $buflen = length($buffer);

		# iterate in buffer till we find a valid ADTS frame with positive frame length and next-frame sync
		for (my $pos = 0; $pos + 7 <= $buflen && ($pos = index($buffer, "\xFF", $pos)) >= 0; $pos++) {
			last if $pos + 7 > $buflen;

			my $b1 = ord(substr($buffer, $pos + 1, 1));
			# ADTS syncword 0xFFF (12 bits) + ID (1 bit) + layer 00 (2 bits) + protection_absent (1 bit)
			next unless ($b1 & 0xF6) == 0xF0;

			# Sampling frequency index 15 is forbidden
			my $freq_idx = (ord(substr($buffer, $pos + 2, 1)) >> 2) & 0x0F;
			next if $freq_idx == 15;

			my $min_hdr = ($b1 & 0x01) ? 7 : 9;
			my $length  = (unpack('N', substr($buffer, $pos + 3, 4)) >> 13) & 0x1FFF;
			next if $length < $min_hdr;
			next if $pos + $length + 2 > $buflen;

			my $nb0 = ord(substr($buffer, $pos + $length, 1));
			my $nb1 = ord(substr($buffer, $pos + $length + 1, 1));
			if ($nb0 == 0xFF && ($nb1 & 0xF6) == 0xF0) {
				return $offset + $pos;
			}
		}
	}

	# nothing found
	return -1;
}

sub canSeek { 1 }

sub parseStream {
	my ( $class, $dataref, $args ) = @_;
	return -1 unless defined $$dataref;

	# stitch new data to existing buf and init parser if needed
	$args->{_scanbuf} .= $$dataref;
	$args->{_need} ||= 8;
	$args->{_offset} ||= 0;

	my $len = length($$dataref);
	my $offset = $args->{_offset};
	my $log = logger('player.streaming');
	my $buflen = length($args->{_scanbuf});

	while ($offset + 8 <= $buflen && $buflen > $offset + $args->{_need} + 8) {
		my $atom_size = unpack('N', substr($args->{_scanbuf}, $offset, 4));
		my $atom_type = substr($args->{_scanbuf}, $offset + 4, 4);
		my $hdr_size  = 8;

		# Handle 64-bit extended atom size (size == 1)
		if ($atom_size == 1) {
			last if $offset + 16 > $buflen;
			my ($hi, $lo) = unpack('NN', substr($args->{_scanbuf}, $offset + 8, 8));
			# Reject 64-bit size underflow (< 16) or overflow (> 2^53)
			if (($hi == 0 && $lo < 16) || $hi > 0x001FFFFF) {
				$log->warn("invalid 64-bit MP4 atom size ($hi:$lo) for $atom_type");
				return 0;
			}
			$atom_size = ($hi * 4294967296) + $lo;
			$hdr_size  = 16;
		}
		elsif ($atom_size == 0) {
			if ($atom_type eq 'mdat') {
				$args->{_atom} = $atom_type;
				$args->{_need} = 0;
				$args->{"_${atom_type}_"} = ($args->{_range} || 0) + $offset;
				$args->{_audio_size} = 0;
				last;
			}
			$log->warn("zero-sized non-mdat MP4 atom $atom_type at offset $offset");
			return 0;
		}
		elsif ($atom_size < 8) {
			$log->warn("undersized MP4 atom $atom_type ($atom_size bytes) at offset $offset");
			return 0;
		}

		$args->{_atom} = $atom_type;
		$args->{_need} = $atom_size;
		$args->{"_$args->{_atom}_"} = ($args->{_range} || 0) + $offset;

		# a bit of sanity check
		if ($offset == 0 && ($args->{_range} || 0) == 0 && $args->{_atom} ne 'ftyp') {
			$log->warn("no header! this is supposed to be a mp4 track");
			return 0;
		}

		# if there is a stco, the first entry is the audio_offset
		if ($args->{_atom} eq 'stco') {
			if ($atom_size < 20) {
				$log->warn("undersized stco atom ($atom_size bytes)");
				return 0;
			}
			last if $offset + 20 > $buflen;
			$args->{_audio_offset} = unpack('N', substr($args->{_scanbuf}, $offset + 16, 4));
			main::DEBUGLOG && $log->is_debug && $log->debug("found audio offset with stco $args->{_audio_offset}");
		}

		# need to dive into atoms to find optional stco
		my $step = ($args->{_atom} !~ /^(moov|trak|mdia|minf|stbl)$/) ? $args->{_need} : $hdr_size;
		if ($step <= 0 || $offset + $step <= $offset) {
			$log->warn("invalid MP4 atom step $step at offset $offset");
			return 0;
		}
		$offset += $step;
		main::DEBUGLOG && $log->is_debug && $log->debug("atom $args->{_atom} at ", $args->{"_$args->{_atom}_"}, " of size $args->{_need}");

		# mdat reached = audio offset & size acquired
		if ($args->{_atom} eq 'mdat') {
			$args->{_audio_size} = $args->{_need};
			last;
		}
	}

	$args->{_offset} = $offset;
	return -1 unless $args->{_mdat_};

	# now make sure we have acquired a full moov atom
	if (!$args->{_moov_}) {
		# no 'moov' found but EoF
		if (!$len) {
			$log->warn("no 'moov' found before EOF => track probably not playable");
			return 0;
		}

		# already waiting for bottom 'moov', we need more
		return -1 if $args->{_range};

		# top 'moov' not found, need to seek beyond 'mdat'
		$args->{_range} = $args->{_offset};
		$args->{_scanbuf} = '';
		$args->{_offset} = 0;
		delete $args->{_need};

		return $args->{_range};
	} elsif ($args->{_atom} eq 'moov' && $len) {
		return -1;
	}

	# finally got it, align to beginning of 'moov'
	substr($args->{_scanbuf}, 0, $args->{_moov_} - $args->{_range}, '');

	# put at least 16 bytes after mdat or it confuses audio::scan (and header creation)
	my $fh = File::Temp->new( DIR => Slim::Utils::Misc::getTempDir);
	$fh->write($args->{_scanbuf} . pack('N', $args->{_audio_size}) . 'mdat' . ' ' x 16);
	$fh->flush();
	$fh->seek(0, 0);

	my $scan = _runLmsAudioscan('scan', '--info-only', $fh->filename);
	if (!$scan || ref $scan ne 'HASH' || ref $scan->{info} ne 'HASH') {
		$fh->seek(0, 0);
		$scan = Audio::Scan->scan_fh( mp4 => $fh );
	}
	else {
		my $sinfo = $scan->{info};
		my $stracks = (ref $sinfo->{tracks} eq 'ARRAY') ? $sinfo->{tracks} : [];
		my $needs_track_fields = !@$stracks || grep {
			ref $_ ne 'HASH'
			|| !defined $_->{audio_type}
			|| !defined $_->{audio_object_type}
		} @$stracks;
		if (!defined $sinfo->{dlna_profile} || $needs_track_fields) {
			$fh->seek(0, 0);
			my $fallback = Audio::Scan->scan_fh( mp4 => $fh );
			$scan = _mergeAudioscanFallback($scan, $fallback);
		}
	}
	return 0 unless $scan && ref $scan eq 'HASH' && ref $scan->{info} eq 'HASH';

	my $info = $scan->{info};
	$info->{fh} = $fh;

	# audio offset from stco or mdat position, but audio_size needs adjustment
	$info->{audio_offset} = $args->{_audio_offset} || ($args->{_mdat_} + 8);
	$info->{audio_size} -= $info->{audio_offset} - $args->{_mdat_};

	# MPEG-4 audio = 64,  MPEG-4 ADTS main = 102, MPEG-4 ADTS Low Complexity = 103
	# MPEG-4 ADTS Scalable Sampling Rate = 104
	if (ref $info->{tracks} eq 'ARRAY' && $info->{tracks}->[0] && defined $info->{tracks}->[0]->{audio_type} && $info->{tracks}->[0]->{audio_type} == 64) {
		$info->{processors} = { 'aac' => \&setADTSProcess };
	}

	return $info;
}

sub setADTSProcess {
	my ($bufref) = @_;
	my $pos = 0;
	my $codec = {};
	my $buflen = defined $bufref && defined $$bufref ? length($$bufref) : 0;
	my %atoms = (
		stsd => 16,
		mp4a => 36,
		meta => 12,
		moov => 8,
		trak => 8,
		mdia => 8,
		minf => 8,
		stbl => 8,
		utda => 8,
		ilst => 8,
	);

	while ($pos + 8 <= $buflen) {
		my $atom_start = $pos;
		my $len = unpack("N", substr($$bufref, $pos, 4));
		my $type = substr($$bufref, $pos + 4, 4);
		my $hdr_len = 8;

		if ($len == 1) {
			last if $pos + 16 > $buflen;
			my ($hi, $lo) = unpack("NN", substr($$bufref, $pos + 8, 8));
			last if ($hi == 0 && $lo < 16) || $hi > 0x001FFFFF;
			$len = ($hi * 4294967296) + $lo;
			$hdr_len = 16;
		}
		elsif ($len < 8) {
			last;
		}

		my $atom_end = ($len > $buflen - $atom_start) ? $buflen : $atom_start + $len;
		$pos = $atom_start + $hdr_len;

		last if $type eq 'mdat';

		if ($type eq 'esds') {
			my $offset = 4;
			last if $pos + $offset + 1 > $atom_end;
			last unless unpack("C", substr($$bufref, $pos + $offset++, 1)) == 0x03;
			last if $pos + $offset + 1 > $atom_end;
			my $data = unpack("C", substr($$bufref, $pos + $offset, 1));
			$offset += 3 if $data == 0x80 || $data == 0x81 || $data == 0xfe;
			$offset += 4;
			last if $pos + $offset + 1 > $atom_end;
			last unless unpack("C", substr($$bufref, $pos + $offset++, 1)) == 0x04;
			last if $pos + $offset + 1 > $atom_end;
			$data = unpack("C", substr($$bufref, $pos + $offset, 1));
			$offset += 3 if $data == 0x80 || $data == 0x81 || $data == 0xfe;
			$offset += 14;
			last if $pos + $offset + 1 > $atom_end;
			last unless unpack("C", substr($$bufref, $pos + $offset++, 1)) == 0x05;
			last if $pos + $offset + 1 > $atom_end;
			$data = unpack("C", substr($$bufref, $pos + $offset, 1));
			$offset += 3 if $data == 0x80 || $data == 0x81 || $data == 0xfe;
			$offset++;
			last if $pos + $offset + 4 > $atom_end;
			$data = unpack("N", substr($$bufref, $pos + $offset, 4));
			$codec->{freq_index} = ($data >> 23) & 0x0f;
			$codec->{channel_config} = ($data >> 19) & 0x0f;
			$codec->{object_type} = $data >> 27;
			$codec->{object_type} = ($data >> 10) & 0x1f if $codec->{object_type} == 5 || $codec->{object_type} == 29;
			$pos = $atom_end;
		} elsif ($type eq 'stsz') {
			my $offset = 4;
			last if $pos + $offset + 4 > $atom_end;
			$codec->{frame_size} = unpack("N", substr($$bufref, $pos + $offset, 4));
			if (!$codec->{frame_size}) {
				$offset += 4;
				$codec->{frames} = [];
				last if $pos + $offset + 4 > $atom_end;
				$codec->{entries} = unpack("N", substr($$bufref, $pos + $offset, 4));
				$offset += 4;
				my $avail = $atom_end - ($pos + $offset);
				my $max_entries = $avail > 0 ? int($avail / 4) : 0;
				if ($codec->{entries} > $max_entries) {
					logger('player.source')->warn("clamping stsz entries $codec->{entries} to $max_entries");
					$codec->{entries} = $max_entries;
				}
				if ($codec->{entries} > 0) {
					$codec->{frames} = [ unpack("N[$codec->{entries}]", substr($$bufref, $pos + $offset, $codec->{entries} * 4)) ];
				}
			}
			$pos = $atom_end;
		} else {
			if (exists $atoms{$type}) {
				my $step = $atoms{$type};
				last if $len < $step || $atom_start + $step > $buflen;
				$pos = $atom_start + $step;
			} else {
				$pos = $atom_end;
			}
		}

		last if $pos <= $atom_start;
		last if ($codec->{frame_size} || $codec->{entries}) && $codec->{channel_config};
	}

	# don't want to send a header when doing AAC demuxs
	$$bufref = '' if defined $bufref && defined $$bufref;

	# use a closure to hold context
	return sub {
		return extractADTS($codec, @_);
	}
}

sub extractADTS {
	my ($codec, undef, $chunk_size, $offset) = @_;
	my $consumed = 0;
	my @ADTSHeader = (0xFF,0xF1,0,0,0,0,0xFC);

	$codec->{inbuf} .= substr($_[1], $offset // 0);
	substr($_[1], $offset // 0) = '';
	$codec->{frame_index} ||= 0;

	while ($codec->{frame_size} || ($codec->{entries} && $codec->{frame_index} < $codec->{entries})) {
		my $frame_size = $codec->{frame_size} || $codec->{frames}->[$codec->{frame_index}];
		last if !defined $frame_size || $frame_size <= 0 || $frame_size + 7 > 0x1fff;
		last if $frame_size + $consumed > length($codec->{inbuf}) || length($_[1]) + $frame_size + 7 > $chunk_size;

		my $obj_type = $codec->{object_type} // 2;
		my $freq_idx = $codec->{freq_index} // 0;
		my $chan_cfg = $codec->{channel_config} // 0;

		$ADTSHeader[2] = (((($obj_type & 0x3) - 1) << 6) + ($freq_idx << 2) + ($chan_cfg >> 2));
		$ADTSHeader[3] = ((($chan_cfg & 0x3) << 6) + (($frame_size + 7) >> 11));
		$ADTSHeader[4] = ((($frame_size + 7) & 0x7ff) >> 3);
		$ADTSHeader[5] = (((($frame_size + 7) & 7) << 5) + 0x1f);

		$_[1] .= pack("CCCCCCC", @ADTSHeader) . substr($codec->{inbuf}, $consumed, $frame_size);

		$codec->{frame_index}++;
		$consumed += $frame_size;
	}

	substr($codec->{inbuf}, 0, $consumed, '');
	return length $codec->{inbuf};
}

# AAAAAAAA AAAABCCD EEFFFFGH HHIJKLMM MMMMMMMM MMMOOOOO OOOOOOPP
#
# Header consists of 7 bytes without CRC.
#
# Letter	Length (bits)	Description
# A	12	syncword 0xFFF, all bits must be 1
# B	1	MPEG Version: 0 for MPEG-4, 1 for MPEG-2
# C	2	Layer: always 0
# D	1	set to 1 as there is no CRC
# E	2	profile, the MPEG-4 Audio Object Type minus 1
# F	4	MPEG-4 Sampling Frequency Index (15 is forbidden)
# G	1	private bit, guaranteed never to be used by MPEG, set to 0 when encoding, ignore when decoding
# H	3	MPEG-4 Channel Configuration (in the case of 0, the channel configuration is sent via an inband PCE)
# I	1	originality, set to 0 when encoding, ignore when decoding
# J	1	home, set to 0 when encoding, ignore when decoding
# K	1	copyrighted id bit, the next bit of a centrally registered copyright identifier, set to 0 when encoding, ignore when decoding
# L	1	copyright id start, signals that this frame's copyright id bit is the first bit of the copyright id, set to 0 when encoding, ignore when decoding
# M	13	frame length, this value must include 7 bytes of header
# O	11	Buffer fullness
# P	2	Number of AAC frames (RDBs) in ADTS frame minus 1, for maximum compatibility always use 1 AAC frame per ADTS frame
#
# ISO 14496 Part 3 Table 1.13
#
# A: Profile (2=LC, 5=SBR, 29=PS), R: Core SampleRate, M: Main SampleRate,
# C: Channels, X: Extensions, S=???, T=???, E=extension bit
#
# AOT=2      AOT=5|29   AOT=2 (extended)
# AAAA ARRR  AAAA ARRR  AAAA ARRR
# RCCC CXXX  RCCC CMMM  RCCC CXXX
#            MPPP PP    SSSS SSSS
#                       SSST TTTT
#                       ERRR R


1;
