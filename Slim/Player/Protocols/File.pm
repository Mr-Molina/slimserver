package Slim::Player::Protocols::File;


# Logitech Media Server Copyright 2001-2024 Logitech, Vidur Apparao.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;
use base qw(IO::File);

use File::Spec::Functions qw(catdir);
use IO::String;

use Slim::Music::Info;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Prefs;
use Slim::Formats;
use Slim::Player::Source;

use constant MAXCHUNKSIZE => 32768;

my $log = logger('player.source');
my $prefs = preferences('server');

sub new {
	my $class = shift;
	my $args  = shift;

	if (!$args || ref($args) ne 'HASH' || !$args->{'song'}) {
		logWarning("No song passed!");
		return undef;
	}

	my $self = $class->open($args);

	if (defined($self)) {
		${*$self}{'song'}        = $args->{'song'};
		${*$self}{'client'}      = $args->{'client'};
		${*$self}{'contentType'} = Slim::Music::Info::contentType($args->{'song'}->currentTrack())
			unless defined ${*$self}{'contentType'};

		${*$self}{'handler'} = $class;
	}

	return $self;
}

sub open {
	my $class = shift;
	my $args  = shift;

	return undef unless $args && ref($args) eq 'HASH';

	my $song     = $args->{'song'};
	return undef unless $song;

	my $track    = $song->currentTrack();
	return undef unless $track;

	my $url      = $track->url;
	return undef unless defined $url;

	my $client   = $args->{'client'};
	my $seekdata = $song->seekdata();

	my $filepath = $class->pathFromFileURL($url);

	if (!defined $filepath || $filepath eq '' || $filepath =~ /\0/ || !-f $filepath) {
		logError("Invalid or non-regular file: " . (defined $filepath ? $filepath : '<undef>'));
		$client->controller()->playerStreamingFailed($client, 'PROBLEM_OPENING') if $client && $client->controller();
		return undef;
	}

	my $size       = $track->audio_size() || -s $filepath || 0;
	my $duration   = $track->secs() || 0;
	my $offset     = $track->audio_offset() || 0;
	my $samplerate = $track->samplerate() || 0;
	my $samplesize = $track->samplesize() || 0;
	my $channels   = $track->channels() || 0;
	my $blockalign = $track->block_alignment() || 1;
	my $endian     = $track->endian() || '';
	my $drm        = $track->drm();

	if ( main::INFOLOG && $log->is_info ) {
		$log->info("duration: [$duration] size: [$size] endian [$endian] offset: [$offset] for $url");
	}

	if ($drm) {
		logWarning("[$url] has DRM. Skipping.");
		$client->controller()->playerStreamingFailed($client, 'PROBLEM_DRM') if $client && $client->controller();
		return undef;
	}

	if (!$size && !$duration) {
		logWarning("[$url] not bothering opening file with zero size or duration");
		$client->controller()->playerStreamingFailed($client, 'PROBLEM_OPENING') if $client && $client->controller();
		return undef;
	}

	main::INFOLOG && $log->info("Opening file $filepath");

	my $sock = $class->SUPER::new();
	if (!$sock || !$sock->SUPER::open($filepath, '<')) {
		logError("could not open $filepath: $!");
		return undef;
	}

	binmode($sock);

	my $format = Slim::Music::Info::contentType($track);

	${*$sock}{'url'}         = $url;
	${*$sock}{'position'}    = 0;
	${*$sock}{'contentType'} = $format;
	${*$sock}{'logicalEndOfStream'} = $size + $offset;

	$song->samplerate($samplerate) if ($samplerate);
	$song->samplesize($samplesize) if ($samplesize);
	$song->channels($channels) if ($channels);

	$song->totalbytes($size);
	$song->duration($duration);
	$song->offset($offset);
	$song->blockalign($blockalign);

	my $seekoffset = $offset;
	my $streamLength = $size;
	my $maxOffset = $size + $offset;

	if (defined $seekdata && ref($seekdata) eq 'HASH') {

		if (   ! $seekdata->{sourceStreamOffset}
			&& ! $seekdata->{restartOffset}
			&& $seekdata->{'timeOffset'}
			&& $class->canSeek($client, $song) )
		{
			$seekdata->{sourceStreamOffset} = _timeToOffset($sock, $format, $song, $seekdata->{'timeOffset'});
		}

		if ($seekdata->{restartOffset}) {								# used for reconnect
			$streamLength = $song->streamLength();
			$seekoffset = $seekdata->{restartOffset};
			$seekoffset = $offset if $seekoffset < $offset;
			$seekoffset = $maxOffset if $seekoffset > $maxOffset;
		} elsif ($seekdata->{sourceStreamOffset}) {						# used for seeking
			$seekoffset = $seekdata->{sourceStreamOffset};
			$seekoffset = $offset if $seekoffset < $offset;
			$seekoffset = $maxOffset if $seekoffset > $maxOffset;
			$streamLength -= $seekoffset - $offset;
			$streamLength = 0 if $streamLength < 0;
		} else {
			$seekoffset = $offset;										# normal case
		}
	}

	# Bug 6836 - support CUE files for Ogg
	# Also used for Xing frame in MP3 when seeking
	# WAV header, and ASF header
	${*$sock}{'initialAudioBlockRemaining'} = 0;

	${*$sock}{'streamFormat'} = (ref($args->{'transcoder'}) eq 'HASH') ? $args->{'transcoder'}->{'streamformat'} : undef;

	if ( $seekoffset
		&& !$song->stripHeader
		# We do not need to worry about an initialAudioBlock when we are restarting
		# as getSeekDataByPosition() will not have allowed a restart within the
		# initialAudioBlock.
		&& !($seekdata && $seekdata->{restartOffset}) )
	{
		my $streamClass = _streamClassForFormat($format);

		if (!defined($song->initialAudioBlock()) &&
			$streamClass && $streamClass->can('getInitialAudioBlock'))
		{
			# We stash the initial audio block in the song because we may well want it
			# multiple times.
			$song->initialAudioBlock($streamClass->getInitialAudioBlock($sock, $track, ($seekdata ? $seekdata->{'timeOffset'} : undef)));
		}

		if (defined($song->initialAudioBlock()) && length($song->initialAudioBlock())) {
			my $length = length($song->initialAudioBlock());
			main::DEBUGLOG && $log->debug("Got initial audio block of size $length");
			if ($seekoffset <= $length) {
				# Might as well just play from the start normally
				$streamLength = $size + $offset;
				$offset = $seekoffset = 0;
			} else {
				${*$sock}{'initialAudioBlockRemaining'} = $length;
				${*$sock}{'initialAudioBlockRef'} = \($song->initialAudioBlock());
				$streamLength += $length;
			}

			# For some files, we can't cache the audio block because it's different each time
			$song->initialAudioBlock(undef) if $streamClass && $streamClass->can('volatileInitialAudioBlock') && $streamClass->volatileInitialAudioBlock($track);
		}
	}

	if (defined $seekoffset) {
		$seekoffset = 0 if $seekoffset < 0;
		main::INFOLOG && $log->info("Seeking in $seekoffset into $filepath");
		my $seekres = $sock->sysseek($seekoffset, 0);
		if (!defined($seekres)) {
			logError("could not seek to $seekoffset for $filepath: $!");
			$sock->close();
			$client->controller()->playerStreamingFailed($client, 'PROBLEM_OPENING') if $client && $client->controller();
			return undef;
		} else {
			if ($client) {
				my $songOffset = $song->offset() || 0;
				$client->songBytes($seekoffset - ($song->stripHeader ? $songOffset : ${*$sock}{'initialAudioBlockRemaining'}));
			}
			${*$sock}{'position'} = $seekres + 0;
			if ($seekoffset > $offset && $seekdata && $seekdata->{'timeOffset'}) {
				$song->startOffset($seekdata->{'timeOffset'});
			}
		}
	} else {
		$client->songBytes(0) if $client;
	}

	$song->streamLength($streamLength);

	# Bug 17727 - playback issues with mp3 + cue sheets on Windows
	# there's a bug in MP3::Cut::Gapless which fails the cache file check on Windows
	if ( !main::ISWINDOWS && defined($format) && $format eq 'mp3' && $track->virtual ) {
		eval {
			# Return a gapless MP3 stream for cue sheet tracks
			# XXX avoid calling the above stuff for these tracks, it's just wasted
			require MP3::Cut::Gapless;

			my ($start_ms, $end_ms);

			if ( $url =~ /#([^-]+)-([^-]+)$/ ) {
				$start_ms = sprintf "%d", $1 * 1000;
				$end_ms   = sprintf "%d", $2 * 1000; # XXX last track should be undef
			}

			if ( defined $seekdata && $seekdata->{timeOffset} ) {
				# XXX error checks?
				$start_ms += sprintf "%d", $seekdata->{timeOffset} * 1000;
			}

			main::INFOLOG && $log->is_info && $log->info("Opening gapless MP3 stream from time " . (defined $start_ms ? $start_ms : 'undef') . " to " . (defined $end_ms ? $end_ms : 'undef'));

			${*$sock}{mp3cut} = MP3::Cut::Gapless->new(
				file      => $filepath,
				cache_dir => catdir( $prefs->get('librarycachedir'), 'mp3cut' ),
				start_ms  => $start_ms,
				end_ms    => $end_ms,
			);
		};
		if ($@) {
			$log->warn("Unable to play MP3 cue track in gapless mode: $@");
			delete ${*$sock}{mp3cut};
		}
	}

	return $sock;
}

sub sysseek {
	my ($self, $pos, $whence) = @_;
	return undef unless defined $self;

	my $res = $self->SUPER::sysseek($pos, $whence);
	if (defined $res) {
		${*$self}{'position'} = $res + 0;
	}
	return $res;
}

sub close {
	my $self = shift;
	return 1 unless defined $self;

	delete ${*$self}{'mp3cut'};
	delete ${*$self}{'initialAudioBlockRef'};
	delete ${*$self}{'initialAudioBlockRemaining'};
	delete ${*$self}{'song'};
	delete ${*$self}{'client'};
	delete ${*$self}{'handler'};

	return $self->SUPER::close(@_);
}

# make this conversion accessible to sub-classes
sub pathFromFileURL {
	Slim::Utils::Misc::pathFromFileURL($_[1]);
}

sub sysread {
	my $self   = $_[0];
	my $n      = $_[2];
	my $offset = $_[3] || 0;

	if (!defined($n) || $n < 0 || $offset < 0) {
		return undef;
	}

	if ($n == 0) {
		return 0;
	}

	if ( ${*$self}{mp3cut} ) {
		# Get audio data from MP3::Cut::Gapless object instead of directly from the file
		if ($offset) {
			my $tmp = '';
			$n = ${*$self}{mp3cut}->read( $tmp, $n );
			if (defined($n) && $n > 0) {
				$_[1] = '' unless defined $_[1];
				if (length($_[1]) < $offset) {
					$_[1] .= "\0" x ($offset - length($_[1]));
				}
				substr($_[1], $offset) = substr($tmp, 0, $n);
			}
		} else {
			$n = ${*$self}{mp3cut}->read( $_[1], $n );
		}
		${*$self}{position} += $n unless (!defined($n) || $n <= 0);
		return $n;
	}

	if (my $length = ${*$self}{'initialAudioBlockRemaining'}) {

		my $chunkLength = $length;
		my $chunkref;

		main::DEBUGLOG && $log->debug("getting initial audio block of size $length");

		if ($length > $n || $length < length(${${*$self}{'initialAudioBlockRef'}})) {
			$chunkLength = $length > $n ? $n : $length;
			my $chunk = substr(${${*$self}{'initialAudioBlockRef'}}, -$length, $chunkLength);
			$chunkref = \$chunk;
			${*$self}{'initialAudioBlockRemaining'} = ($length - $chunkLength);
		} else {
			${*$self}{'initialAudioBlockRemaining'} = 0;
			$chunkref = ${*$self}{'initialAudioBlockRef'};
		}

		delete ${*$self}{'initialAudioBlockRef'} unless ${*$self}{'initialAudioBlockRemaining'};

		if ($offset) {
			$_[1] = '' unless defined $_[1];
			if (length($_[1]) < $offset) {
				$_[1] .= "\0" x ($offset - length($_[1]));
			}
			substr($_[1], $offset) = $$chunkref;
		} else {
			$_[1] = $$chunkref;
		}
		return $chunkLength;
	}

	else {
		my $remaining = (${*$self}{'logicalEndOfStream'} || 0) - (${*$self}{'position'} || 0);
		if ($remaining <= 0) {
			$log->warn("Trying to read past the end of file: " . (${*$self}{'url'} || '') );
			return 0;
		}
		if ($n > $remaining) {$n = $remaining;}

		$n = sysread($self, $_[1], $n, $offset);
		${*$self}{'position'} += $n unless (!defined($n) || $n <= 0);
		return $n;
	}
}

sub canDirectStream {
	return 0;
}

sub isRemote {
	return 0;
}

sub audioScrobblerSource {
	my ( $class, $client, $url ) = @_;

	# Scrobble as 'chosen by user' content
	return 'P';
}

sub contentType {
	my $self = shift;

	return defined($self) ? ${*$self}{'contentType'} : undef;
}


sub _streamClassForFormat {
	my ($streamFormat) = @_;

	if ($streamFormat && Slim::Formats->loadTagFormatForType($streamFormat)) {
		return Slim::Formats->classForFormat($streamFormat);
	}
}

sub _timeToOffset {
	my $sock     = shift;
	my $format   = shift;
	my $song     = shift;
	my $time     = shift;

	return 0 unless $song;

	my $offset   = $song->offset() || 0;
	my $size     = $song->totalbytes() || 0;
	my $duration = $song->duration() || 0;
	my $align    = $song->blockalign() || 1;
	my $maxOffset = $size + $offset;

	$time = 0 unless defined $time;

	# Short circuit the computation if the time for which we're asking
	# is outside the song boundaries
	if ($time < 0 || $duration <= 0) {
		return $offset;
	} elsif ($time >= $duration) {
		return $maxOffset;
	}

	my $byterate = $size / $duration;
	my $seekoffset   = int($byterate * $time);

	my $streamClass = _streamClassForFormat($format);

	if ($streamClass && $streamClass->can('findFrameBoundaries')) {
		# Bug 16068, adjust time if this is a virtual track in a cue sheet
		my $track = $song->currentTrack();
		if ( $track && defined $track->url && $track->url =~ /#([^-]+)-([^-]+)$/ ) {
			$time += $1;
		}

		main::INFOLOG && $log->is_info && $log->info("seeking using $streamClass findFrameBoundaries(" . ($seekoffset + $offset) . ", $time)");
		$seekoffset  = $streamClass->findFrameBoundaries($sock, $seekoffset + $offset, $time);
	} else {
		$seekoffset -= $seekoffset % $align;
		$seekoffset += $offset;
	}

	# Some modules may return -1 to indicate they couldn't find a frame;
	# clamp to valid [offset, size + offset] range
	if ( !defined($seekoffset) || $seekoffset < $offset ) {
		$seekoffset = $offset;
	} elsif ( $seekoffset > $maxOffset ) {
		$seekoffset = $maxOffset;
	}

	main::INFOLOG && $log->info("$time -> $seekoffset (align: $align size: $size duration: $duration)");

	return $seekoffset;
}

sub getSeekData {
	my $class    = shift;
	my $client   = shift;
	my $song     = shift; # ignored at the moment
	my $time     = shift;

	# Do it all later at open time
	return {timeOffset => $time};
}

sub getSeekDataByPosition {
	my (undef, undef, $song, $bytesReceived) = @_;

	return undef unless $song && defined $bytesReceived;

	my $streamLength = $song->streamLength();
	my $initialBlock = $song->initialAudioBlock();

	if ( !$streamLength
		|| (defined($initialBlock) && $bytesReceived < length($initialBlock)) )
	{
		return undef;
	}

	my $totalbytes = $song->totalbytes() || 0;
	my $offset     = $song->offset() || 0;
	my $position   = $totalbytes - ($streamLength - $bytesReceived);

	if ($position <= 0) {
		return undef;
	}

	$position = $totalbytes if $totalbytes && $position > $totalbytes;

	my $seekdata = $song->seekdata; # We preserve the original seekdata so we know the time-offset, if any
	$seekdata = {} unless ref($seekdata) eq 'HASH';
	return {%$seekdata, restartOffset => $position + $offset};
}

sub canSeek {
	my ($class, $client, $song) = @_;

	my $track = $song && $song->currentTrack();
	return 0 unless $track;

	my $url = $track->url;
	return 0 unless defined $url;

	my $type = Slim::Music::Info::contentType($url);

	unless (Slim::Formats->loadTagFormatForType($type)) {return 0;}

	my $formatClass = Slim::Formats->classForFormat($type);

	return ($formatClass && $formatClass->can('canSeek')) ? $formatClass->canSeek($url) : 0;
}

sub canSeekError {
	my ($class, $client, $song) = @_;

	my $track = $song && $song->currentTrack();
	my $type  = ($track && defined $track->url) ? Slim::Music::Info::contentType($track->url) : undef;

	return ('SEEK_ERROR_TYPE_NOT_SUPPORTED', $type);
}

sub getIcon {
	my ( $class, $url ) = @_;

	if (Slim::Music::Info::isSong($url)) {

		my $track = Slim::Schema->objectForUrl({
			'url' => $url,
		});

		if ($track && $track->coverArt) {
			return 'music/' . $track->id . '/cover.png';
		}

	}

	elsif (Slim::Music::Info::isPlaylist($url)) {
		return 'html/images/playlists.png';
	}

	return 'html/images/cover.png';
}

1;

__END__
