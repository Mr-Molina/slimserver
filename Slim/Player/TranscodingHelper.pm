package Slim::Player::TranscodingHelper;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use File::Spec::Functions qw(catdir);
use Scalar::Util qw(blessed);

use Slim::Player::CapabilitiesHelper;
use Slim::Music::Info;
use Slim::Player::Sync;
use Slim::Utils::DateTime;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::OSDetect;
use Slim::Utils::Prefs;

{
	if (main::ISWINDOWS) {
		require Win32;
	}
}

sub init {
	loadConversionTables();
}

our %commandTable = ();
our %capabilities = ();
our %binaries = ();
my %proxies = ();

sub Conversions {
	return \%commandTable;
}

sub Capabilities {
	return $capabilities{$_[0]};
}

my $log = logger('player.source');

my $prefs = preferences('server');

sub loadConversionTables {

	my @convertFiles = ();

	main::INFOLOG && $log->info("Loading conversion config files...");

	# custom convert files allowed at server root or root of plugin directories
	for my $baseDir (Slim::Utils::OSDetect::dirsFor('convert')) {

		push @convertFiles, (
			catdir($baseDir, 'convert.conf'),
			catdir($baseDir, 'custom-convert.conf'),
			catdir($baseDir, 'slimserver-convert.conf'),
		);
	}

	foreach my $baseDir (Slim::Utils::PluginManager->dirsFor('convert')) {

		push @convertFiles, catdir($baseDir, 'custom-convert.conf');
	}

	foreach my $convertFileName (reverse @convertFiles) {

		# can't read? next.
		next unless -r $convertFileName;

		open(CONVERT, $convertFileName) || next;

		while (my $line = <CONVERT>) {

			# skip comments and whitespace
			next if $line =~ /^\s*#/;
			next if $line =~ /^\s*$/;

			# get rid of comments and leading and trailing white space
			$line =~ s/#.*$//o;
			$line =~ s/^\s*//o;
			$line =~ s/\s*$//o;

			if ($line =~ /^proxy\s+(\S+)\s+(\S+)/i) {
				$proxies{$1} = $2;
			}
			elsif ($line =~ /^(\S+)\s+(\S+)\s+(\S+)\s+(\S+)$/) {

				my $inputtype  = $1;
				my $outputtype = $2;
				my $clienttype = $3;
				my $clientid   = lc($4);
				my $profile = "$inputtype-$outputtype-$clienttype-$clientid";

				# if profile is duplicated, find the highest index to create a unique instance
				if ($commandTable{$profile}) {
					my $max = 0;
					for my $existing (keys %commandTable) {
						if ($existing =~ /^\Q$profile\E-(\d+)$/ && $1 > $max) {
							$max = $1;
						}
					}
					$profile .= '-' . ($max + 1);
				}

				$line = <CONVERT>;
				if ($line =~ /^\s+#\s+(\S.*)/) {
					_getCapabilities($profile, $1);
					$line = <CONVERT>;
				} else {
					$capabilities{$profile} = {I => 'noArgs', F => 'noArgs'};	# default capabilities
				}

				my $command = $line;

				$command =~ s/^\s*//o;
				$command =~ s/\s*$//o;

				if ( main::DEBUGLOG && $log->is_debug ) {
					$log->debug(
						"input: '$inputtype' output: '$outputtype' clienttype: " .
						"'$clienttype': clientid: '$clientid': '$command'"
					);
				}

				next unless defined $command && $command !~ /^\s*$/;

				$commandTable{$profile} = $command;
			}
		}

		close CONVERT;
	}
}

# Capabilities
# I - can transcode from stdin
# F - can transcode from a named file
# R - can transcode from a remote URL (URL types unspecified)
#
# E - extensions syntax E:{<key>=<value>,<key>=<value>}
#		NOSTART=I/F/R : no $START$ field when transcoding from I/F/R
#		NOHEADER=I/F/R : strip out header when transcoding from I/F/R (waf/aif only)
#
# O - can seek to a byte offset in the source stream (not yet implemented)
# T - can seek to a start time offset
# U - can seek to start time offset and finish at end time offset
#
# D - can downsample
# B - can limit bitrate
#
# default is "IF"

# Substitution strings for variable capabilities
# %f, $FILE$ - file path (local files)
# %F, $URL$  - full URL (remote streams)
#
# %o - stream start byte offset
#
# %S - stream samples start offset (not yet implemented)
# %s - stream seconds start offset
# %t - stream time (m:ss) start offset
# %U - stream samples end offset (not yet implemented)
# %u - stream seconds end offset
# %v - stream time (m:ss) end offset
# %w - stream seconds duration

#
# %b - limit bitrate: b/s
# %B - limit bitrate: kb/s
# %d - samplerate: samples/s
# %D - samplerate: ksamples/s

# %C, $CHANNELS$   - source channel count
# %c, $OCHANNELS$  - output channel count
#     $SAMPLESIZE$ - source sample size (in bits)
#     $SAMPLERATE$ - source sample rate (in Hz)
# %i               - clientid
# %I, $CLIENTID$   - clientid     ( : or . replaced by - )
# %p               - player model
# %P, $PLAYER$     - player model ( SPACE or QOUTE replaced by _ )
# %g               - groupid
# %G, $GROUPID$    - groupid     ( formatted as MAC. if no group is present use CLIENTID )
# %n               - player name
# %N, $NAME$       - player name  ( SPACE or QOUTE replaced by _ )
# %q, $QUALITY$    - quality
# %Q,              - quality ( fractal notation: if = '0' return '01' )
#     ${FILENAME}$ - contents of {FILENAME} (may contain other $*$ substitutions )

# specific combinations match before wildcards

sub _getCapabilities {
	my ($profile, $capabilities) = @_;

	$capabilities{$profile} = {};
	unless ($capabilities =~ /^([A-Z](\:\{\w+=[^}]+\})?)+$/) {
		$log->error("Capabilities for $profile: syntax error in $capabilities");
		return;
	}

	while ($capabilities) {
		my $can = substr($capabilities, 0, 1, '');
		my $args;

		if ($capabilities =~ /^:\{(\w+=[^}]+)\}/) {
			$capabilities = $';
			if ($can eq 'E') {
				$args = { split (/\s*[=,]\s*/, $1) };
			} else {
				$args = $1;
			}
		} else {
			if ($can =~ /OTUDB/) {
				$log->error("Capabilities for $profile: missing arguments for '$can'");
			}
			$args = 'noArgs';
		}

		$capabilities{$profile}->{$can} = $args;
	}

	# for convenience, always define an extension field
	$capabilities{$profile}->{'E'} ||= {};
}

sub isEnabled {
	my $profile = shift;

	return (defined $commandTable{$profile}) && enabledFormat($profile);
}

sub enabledFormat {
	my $profile = shift;

	main::DEBUGLOG && $log->debug("Checking to see if $profile is enabled");

	my @disabled = @{ $prefs->get('disabledformats') || [] };

	if (!@disabled) {
		return 1;
	}

	if ( main::DEBUGLOG && $log->is_debug ) {
		$log->debug("There are " . scalar @disabled . " disabled formats...");
	}

	my $baseProfile = ($profile =~ /^(.+)-\d+$/) ? $1 : undef;

	for my $format (@disabled) {

		main::DEBUGLOG && $log->debug("Testing $format vs $profile");

		if ($format eq $profile || (defined $baseProfile && $format eq $baseProfile)) {

			main::DEBUGLOG && $log->debug("** $profile Disabled **");

			return 0;
		}
	}

	return 1;
}

sub checkBin {
	my $profile            = shift;
	my $ignoreprefsettings = shift;

	my $command;

	main::DEBUGLOG && $log->debug("Checking formats for: $profile");

	# get the command for this profile
	$command = $commandTable{$profile};

	# if the user's disabled the profile, then skip it unless we're changing the prefs...
	return undef unless $command && ( defined($ignoreprefsettings) || enabledFormat($profile) );

	main::DEBUGLOG && $log->debug("   enabled");

	if ($command) {
		main::DEBUGLOG && $log->debug("  Found command: $command");
	}

	# if we don't have one or more of the requisite binaries, then move on.
	while ($command && $command =~ /\[([^]]+)\]/g) {

		my $binName = $1;

		if ($binName eq 'lms-transcode' && !($ENV{LMS_USE_RUST} // 1)) {
			$command = undef;
			$@ = $binName;
			main::DEBUGLOG && $log->debug("   skipping $binName (LMS_USE_RUST disabled)");
			last;
		}

		my $binary = $binaries{$binName};

		if (!$binary) {
			$binary = Slim::Utils::Misc::findbin($binName);
		}

		if ($binary) {

			$binaries{$binName} = $binary;

		} else {

			$command = undef;

			$@ = $binName;

			$log->warn("   couldn't find binary for: $binName");
			last;
		}
	}

	return $command;
}

sub getConvertCommand2 {
	my ($songOrTrack, $type, $streamModes, $need, $want, $formatOverride, $rateOverride) = @_;

	my $track;
	my $song;
	my $client;
	my $forceTranscode;

	if ( ref $songOrTrack eq 'Slim::Player::Song' ) {
		$song   = $songOrTrack;
		$track  = $song->currentTrack();
		$client = $song->master();
		my $handler = $song->currentTrackHandler();
		$forceTranscode = $handler->forceTranscode($client, $type) if $handler && $handler->can('forceTranscode');
	}
	else {
		$track = $songOrTrack;
	}

	$type ||= Slim::Music::Info::contentType($track);

	my $player     = $client ? $client->model() : undef;
	my $clientid   = $client ? $client->id() : undef;
	my $clientprefs= $client ? $prefs->client($client) : undef;
	my $transcoder = undef;
	my $error;
	my $backupTranscoder = undef;
	my $url = $track->url;

	my @supportedformats = ();

	# Check if we need to ratelimit
	my $rateLimit
		= $rateOverride ? $rateOverride
		: $client ? _rateLimit($client, $type, $track->bitrate) : 0;

	push @$want, 'B' if $rateLimit && !grep /B/, @$want;

	# Check if we need to downsample
	my $samplerateLimit = $song ? Slim::Player::CapabilitiesHelper::samplerateLimit($song) : 0;
	push @$need, 'D' if $samplerateLimit && !grep /D/, @$need;

	# make sure we only test formats that are supported.
	if ( $formatOverride ) {
		@supportedformats = ($formatOverride);
	}
	elsif ( $client ) {
		@supportedformats = Slim::Player::CapabilitiesHelper::supportedFormats($client);
	}

	if ($prefs->get('prioritizeNative')) {
		my @types = split /,/, ($proxies{$type} || $type);
		unshift @types, 'pcm' if grep /wav/, @types;
		foreach my $type (@types) {
			my ($format) = grep /$type/, @supportedformats;
			@supportedformats = ($format, grep { $_ !~ $type } @supportedformats) if $format;
		}
	}

	# Build the full list of possible profiles
	my @profiles = $forceTranscode ? ("$type-$type-transcode-*") : ();

	foreach my $checkFormat (@supportedformats) {

		if ( $clientid && $player ) {
			push @profiles, (
				"$type-$checkFormat-$player-$clientid",
				"$type-$checkFormat-*-$clientid",
				"$type-$checkFormat-$player-*"
			);
		}

		push @profiles, "$type-$checkFormat-*-*";

		if ($type eq $checkFormat && enabledFormat("$type-$checkFormat-*-*") && !$forceTranscode) {
			push @profiles, "$type-$checkFormat-transcode-*";
		}
	}

	# Test each profile in turn
	PROFILE: foreach (@profiles) {
		my $instance = 0;
	INSTANCE:
		my $profile = $instance ? "$_-$instance" : $_;
		$instance++;
		next PROFILE unless exists $commandTable{$profile};

		my $command = checkBin($profile);
		goto INSTANCE if !$command;

		my $streamMode = undef;
		my $caps = $capabilities{$profile};

		# Find a profile supporting available stream modes
		foreach (@$streamModes) {
			if ($caps->{$_}) {
				$streamMode = $_;
				last;
			}
		}
		if (! $streamMode) {
			main::DEBUGLOG && $log->is_debug
				&& $log->debug("Rejecting $command because no available stream mode supported: ",
							(join(',', @$streamModes)));
			goto INSTANCE;
		}

		# Check for mandatory capabilities
		foreach (@$need) {
			if (! $caps->{$_}) {
				main::DEBUGLOG && $log->is_debug
					&& $log->debug("Rejecting $command because required capability $_ not supported: ");
				if ($_ eq 'D') {
					$error ||= 'UNSUPPORTED_SAMPLE_RATE';
				}
				goto INSTANCE;
			}
		}

		# We can't handle WMA Lossless in firmware.
		if ($command eq "-"
			&& $type eq 'wma' && blessed($track) && $track->lossless) {
				goto INSTANCE
		}

		my $streamformat = (split (/-/, $profile))[1];

		my $defaultRateLimit = $track->samplerate > 44100 ? 48000 : 44100;

		$transcoder = {
			command          => $command,
			profile          => $profile,
			usedCapabilities => [@$need, @$want],
			streamMode       => $streamMode,
			streamformat     => $streamformat,
			rateLimit        => $rateLimit || 320,
			samplerateLimit  => $samplerateLimit || $defaultRateLimit,
			clientid         => $clientid || 'undefined',
			groupid          => $clientprefs ? ($clientprefs->get('syncgroupid') || 0) : 0,
			name             => $client ? ($client->name // 'undefined') : 'undefined',
			player           => $player || 'undefined',
			channels         => $track->channels() || 2,
			sampleSize       => $track->samplesize || 16,
			sampleRate       => $track->samplerate || 44100,
			outputChannels   => $clientprefs ? ($clientprefs->get('outputChannels') || 2) : 2,
			extensions       => $caps->{'E'},
			# aif/wav oddity (for '-' rule)
			# aif - aif: any SB that does not support wav requires aif header stripping
			# wav/aif - pcm: force header stripping
			stripHeader      => (($caps->{'E'}->{'NOHEADER'} // '') =~ /$streamMode/) || ($command eq "-" &&
								(($type =~ /(wav|aif)/ && $streamformat eq 'pcm') ||
                                 ($type eq 'aif' && $streamformat eq 'aif' && !grep {$_ eq 'wav'} @supportedformats))),
		};

		# Check for optional profiles
		my $wanted = 0;
		my @got = ();
		foreach (@$want) {
			if (! $caps->{$_}) {
				$wanted++;
			} else {
				push @got, $_;
			}
		}

		if ($wanted) {
			# Save this - maybe we get a better offer later
			if (!$backupTranscoder || $backupTranscoder->{'wanted'} > $wanted) {
				$backupTranscoder = $transcoder;
				$transcoder = undef;
				$backupTranscoder->{'wanted'} = $wanted;
				$backupTranscoder->{'usedCapabilities'} = [@$need, @got];
			}
			goto INSTANCE;
		}

		last;
	}

	if (!$transcoder && $backupTranscoder) {
		# Use the backup option
		$transcoder = $backupTranscoder;
	}

	if (! $transcoder) {
		main::INFOLOG && $log->info("Error: Didn't find any command matches for type: $type");
	} else {
		main::INFOLOG && $log->is_info && $log->info("Matched: $type->", $transcoder->{'streamformat'}, " via: ", $transcoder->{'command'});
	}

	return wantarray ? ($transcoder, $error) : $transcoder;
}

#sub _dump_string {
#	use bytes;
#	my $res = '';
#	my $string = shift;
#
#	for (my $i = 0; $i < length($string); $i++) {
#		my $c = substr($string, $i, 1);
#		my $o = ord($c);
#		if ($o > 127) {
#			$res .= sprintf("\\x%02X", $o);
#		} else {
#			$res .= $c;
#		}
#	}
#	return $res;
#}

sub _shellEscape {
	my ($val, $allowSpaces) = @_;

	return '' unless defined $val;

	if (!main::ISWINDOWS) {
		if ($allowSpaces) {
			$val =~ s/[\0\r\n]+/ /g;
			$val =~ s/\s+/ /g;
			$val =~ s/([^\w.\/:,=\@+\- ])/\\$1/g;
		}
		else {
			$val =~ s/[\0\r\n]+//g;
			$val =~ s/([^\w.\/:,=\@+\-])/\\$1/g;
		}
		return $val;
	}

	if ($allowSpaces) {
		$val =~ s/[\0\r\n]+/ /g;
		$val =~ s/\s+/ /g;
		$val =~ s/[&|<>^%"\`;\$]/_/g;
		return $val;
	}

	$val =~ s/[\0\r\n]+//g;
	if ($val =~ /[\s"&|<>^%\`;\$\(\)\*\?\[\]\{\}\~#!]/) {
		$val =~ s/(\\*)"/$1$1\\"/g;
		$val =~ s/(\\+)$/$1$1/;
		$val =~ s/%//g;
		return '"' . $val . '"';
	}

	return $val;
}

sub _resolvePrefPlaceholder {
	my ($placeholder) = @_;

	my ($file, $pref) = $placeholder =~ /(.*)\.([^\.]+)$/;
	my $val = '';

	if ($file && $pref) {
		$val = preferences($file)->get($pref) // '';
	}
	else {
		$log->warn("couldn't find file preferences for: $placeholder");
	}

	return _shellEscape($val, 1);
}

sub tokenizeConvertCommand2 {
	my ($transcoder, $filepath, $fullpath, $noPipe, $quality) = @_;

	# Bug 10199 - make sure we do not promote any strings to decoded ones (8859-1 => UFT-8)
	use bytes;

	my $command = $transcoder->{'command'};

	# This must come above the FILE substitutions, otherwise it will break
	# files with [] in their names.

	while ( $command =~ /\[([^\]]+)\]/g ) {
		if (!$binaries{$1}) {
			my $bin = Slim::Utils::Misc::findbin($1);
			$binaries{$1} = $bin if $bin;
		}
	}
	$command =~ s/\[([^\]]+)\]/'"' . ($binaries{$1} \/\/ '') . '"'/eg;

	my ($start, $end);

	my %subs;
	my %capSubs;
	my %vars;

	# Special case for cuesheets. We pass the start and end
	# of the track within the file.
	if (defined $fullpath && $fullpath =~ /#(\d+(?:\.\d+)?)-(\d+(?:\.\d+)?)$/) {
		 ($start, $end) = ($1, $2);
	}

	if ($transcoder->{'start'}) {
		$start = ($start || 0) + $transcoder->{'start'};
	}

	# there should be no start when using stdin (cue or not)
	if ($start && ($transcoder->{'extensions'}->{'NOSTART'} // '') !~ /$transcoder->{'streamMode'}/) {
		push @{$transcoder->{'usedCapabilities'}}, 'T';
	}

	if ($end) {
		push @{$transcoder->{'usedCapabilities'}}, 'U';
	}

	# Start with some legacy ones

	my $profile = $transcoder->{'profile'};
	my $capabilities = $capabilities{$profile};

	# Find what substitutions we need to make
	foreach my $cap ($transcoder->{'streamMode'}, @{$transcoder->{'usedCapabilities'}}) {
		my $capSpec = $capabilities->{$cap};
		next unless defined $capSpec;
		my ($arg, $value) = $capSpec =~ /(\w+)=(.+)/;
		next unless defined $value;
		$capSubs{$arg} = $value;

		# and find what variables they contain
		foreach ($value =~ m/%(.)/g) {
			$vars{$_} = 1;
		}
	}

	$filepath //= '';
	$fullpath //= '';
	$quality  //= '';

	# escape \, $, ", and ` in file names and URLs on Unix.
	# On Windows, escape quotes and any backslashes preceding quotes/end of string.
	if (!main::ISWINDOWS) {
		$filepath =~ s/\\/\\\\/g;
		$filepath =~ s/([\$\"\`])/\\$1/g;
		$fullpath =~ s/\\/\\\\/g;
		$fullpath =~ s/([\$\"\`])/\\$1/g;
	}
	else {
		$filepath =~ s/(\\*)"/$1$1\\"/g;
		$filepath =~ s/(\\+)$/$1$1/;
		$fullpath =~ s/(\\*)"/$1$1\\"/g;
		$fullpath =~ s/(\\+)$/$1$1/;
	}

	my $clientId    = $transcoder->{'clientid'} // 'undefined';
	my $playerModel = $transcoder->{'player'} // 'undefined';
	my $playerName  = $transcoder->{'name'} // $transcoder->{'clientname'} // 'undefined';
	my $groupId     = $transcoder->{'groupid'} // 0;

	# Check to see if we need to flip the endianess on output
	$subs{'-x'}        = (unpack('n', pack('s', 1)) == 1) ? "" : "-x";

	$subs{'FILE'}       = ($filepath eq '-' ? $filepath : '"' . $filepath . '"');
	$subs{'URL'}        = '"' . $fullpath . '"';
	$subs{'QUALITY'}    = $quality;
	$subs{'CHANNELS'}   = $transcoder->{'channels'};
	$subs{'SAMPLESIZE'} = $transcoder->{'sampleSize'};
	$subs{'SAMPLERATE'} = $transcoder->{'sampleRate'};
	$subs{'OCHANNELS'}  = $transcoder->{'outputChannels'};
	$subs{'CLIENTID'}   = _shellEscape(do { (my $tmp = $clientId)    =~ tr/.:/-/;  $tmp });
	$subs{'PLAYER'}     = _shellEscape(do { (my $tmp = $playerModel) =~ tr/\" /_/; $tmp });
	$subs{'NAME'}       = _shellEscape(do { (my $tmp = $playerName)  =~ tr/\" /_/; $tmp });
	$subs{'GROUPID'}    = (!$groupId || $groupId eq '0') ? $subs{'CLIENTID'} : _shellEscape(do { (my $tmp = sprintf ( "g%011x", $groupId) ) =~ s/..\K(?=.)/-/g; $tmp});

	my %varValues;
	my $s = $start // 0;
	my $e = $end   // 0;

	foreach my $v (keys %vars) {
		my $value;

		if ($v eq 's') {$value = "$s";}
		elsif ($v eq 'u') {$value = "$e";}
		elsif ($v eq 't') {$value = Slim::Utils::DateTime::fracSecToMinSec($s);}
		elsif ($v eq 'v') {$value = Slim::Utils::DateTime::fracSecToMinSec($e);}
		elsif ($v eq 'w') {$value = $e - $s;}

		elsif ($v eq 'b') {$value = ($transcoder->{'rateLimit'} || 0) * 1000;}
		elsif ($v eq 'B') {$value = $transcoder->{'rateLimit'};}

		elsif ($v eq 'd') {$value = $transcoder->{'samplerateLimit'};}
		elsif ($v eq 'D') {$value = ($transcoder->{'samplerateLimit'} || 0) / 1000;}

		elsif ($v eq 'f') {$value = $subs{'FILE'};}
		elsif ($v eq 'F') {$value = $subs{'URL'};}

		elsif ($v eq 'i') {$value = _shellEscape($clientId);}
		elsif ($v eq 'I') {$value = $subs{'CLIENTID'};}
		elsif ($v eq 'p') {$value = _shellEscape($playerModel);}
		elsif ($v eq 'P') {$value = $subs{'PLAYER'};}
		elsif ($v eq 'n') {$value = _shellEscape($playerName);}
		elsif ($v eq 'N') {$value = $subs{'NAME'};}
		elsif ($v eq 'C') {$value = $transcoder->{'channels'};}
		elsif ($v eq 'c') {$value = $transcoder->{'outputChannels'};}
		elsif ($v eq 'q') {$value = $quality;}
		elsif ($v eq 'Q') {$value = ($quality eq '0' ? '01' : $quality . '0');}
		elsif ($v eq 'g') {$value = _shellEscape($groupId);}
		elsif ($v eq 'G') {$value = $subs{'GROUPID'};}

		$varValues{$v} = $value;
	}

	# Single-pass %v expansion ONLY over capability templates (never inside $FILE$, $URL$, etc.)
	for my $arg (keys %capSubs) {
		$capSubs{$arg} =~ s/%([sutvwbBdDfFiIpPnNCcqQgG])/exists $varValues{$1} && defined $varValues{$1} ? $varValues{$1} : "%$1"/ge;
		$subs{$arg} = $capSubs{$arg};
	}

	# Single-pass substitution for $KEY$, ${PREF}$, and cleanup of unset $KEY$ placeholders
	$command =~ s!(?:(\s+)\$(-x|\w+)\$|\$\{([^}]+)\}\$|\$(-x|\w+)\$)!
		if (defined $3) {
			_resolvePrefPlaceholder($3);
		}
		else {
			my $ws  = defined $1 ? $1 : '';
			my $key = defined $2 ? $2 : $4;
			exists $subs{$key}
				? ($ws . (defined $subs{$key} ? $subs{$key} : ''))
				: '';
		}
	!ge;

	if (!defined($noPipe)) {
		$command .= (main::ISWINDOWS) ? '' : ' &';
		$command .= ' |';
	}

	main::INFOLOG && $log->is_info && $log->info("Using command for conversion: ", Slim::Utils::Unicode::utf8decode_locale($command));

	return $command;
}

sub _rateLimit {
	my ($client, $type, $bitrate) = @_;

	my $maxRate = 0;

	foreach ($client->syncGroupActiveMembers()) {
		my $rate = Slim::Utils::Prefs::maxRate($_);
		if ($rate && ($maxRate && $maxRate > $rate || !$maxRate)) {
			$maxRate = $rate;
		}
	}

	return 0 unless $maxRate;

	my $native = 0;
	foreach my $format ($client->formats()) {
		if (defined($type) && $type eq $format) {
			$native = 1;
			last;
		}
	}

	# If the input type is a lossy format (bug 9641), we determine whether the
	# input bitrate is within 1% of the maximum, only if the player supports the format natively.
	# We presume that we won't choose an output format that violates the rate limit.

	main::INFOLOG && $log->info("Native:$native type:$type maxRate:$maxRate bitrate:$bitrate");

	if (defined($type) && ($type =~ /mp3|wma|aac|ogg|ops/) && $native) {
		return 0 if ( ($maxRate * 1.01) >= ($bitrate || 0)/1000);
	}

	return $maxRate;
}

1;

__END__
