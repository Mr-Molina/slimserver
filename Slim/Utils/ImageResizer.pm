package Slim::Utils::ImageResizer;

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2025 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use Config;
use Errno ();
use File::Spec::Functions qw(catdir);
use MIME::Base64 qw(encode_base64);
use Scalar::Util qw(blessed);
use Time::HiRes ();

use Slim::Utils::ArtworkCache;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Prefs;

# UNIX domain socket for optional artwork resizing daemon, if this is
# present we will use async artwork resizing via the external daemon
use constant SOCKET_PATH               => '/tmp/sbs_artwork';
use constant SOCKET_TIMEOUT            => 15;
use constant DAEMON_WATCHDOG_INTERVAL  => 6;
use constant DAEMON_STARTUP_GRACE      => 12;
use constant DAEMON_RAPID_CRASH_WINDOW => 30;
use constant DAEMON_MAX_CRASHES        => 5;
use constant DAEMON_BACKOFF_BASE       => 5;
use constant DAEMON_BACKOFF_MAX        => 120;

my $prefs = preferences('server');
my $log   = logger('artwork');

my ($gdresizein, $gdresizeout, $gdresizeproc);

my $pending_requests = 0;
my ($hasDaemon, $daemon, @daemonArgs);
my $daemon_last_start_time = 0;
my $daemon_crash_count     = 0;
my $daemon_next_retry_time = 0;
my $daemon_circuit_open    = 0;

sub _inStartupGrace {
	return 0 unless $daemon && $daemon->alive;
	my $started = $daemon->start_time || $daemon_last_start_time;
	return ($started && time() <= $started + DAEMON_STARTUP_GRACE) ? 1 : 0;
}

sub _recordDaemonCrash {
	$daemon_last_start_time = 0;
	$hasDaemon = 0;
	$daemon_crash_count++;

	if ($daemon_crash_count >= DAEMON_MAX_CRASHES) {
		$daemon_circuit_open    = 1;
		$daemon_next_retry_time = 0;
		$log->error(sprintf(
			"Resizing daemon crashed %d times within %ds; circuit breaker open, falling back to in-process GDResizer",
			$daemon_crash_count, DAEMON_RAPID_CRASH_WINDOW
		));
		return;
	}

	my $backoff = DAEMON_BACKOFF_BASE * (2 ** ($daemon_crash_count - 1));
	$backoff = DAEMON_BACKOFF_MAX if $backoff > DAEMON_BACKOFF_MAX;
	$daemon_next_retry_time = time() + $backoff;

	$log->warn(sprintf(
		"Resizing daemon exited unexpectedly (%d/%d); backing off for %ds",
		$daemon_crash_count, DAEMON_MAX_CRASHES, $backoff
	));
}

sub _unlinkStaleSocket { if (!main::ISWINDOWS) {
	my ($warn_msg, $only_if_dead) = @_;
	return unless lstat(SOCKET_PATH);
	if (-l _) {
		$log->warn("Refusing to unlink symlink at daemon socket path: " . SOCKET_PATH);
		return;
	}
	return if $only_if_dead && $daemon && $daemon->alive;
	if (-S _ && -o _) {
		$log->warn($warn_msg) if defined $warn_msg;
		unlink SOCKET_PATH;
	}
} }

sub hasDaemon { if (!main::ISWINDOWS) {
	my ($class, $check) = @_;

	if (main::SCANNER || main::ISWINDOWS) {
		$hasDaemon = 0;
		return $hasDaemon;
	}

	if (!defined $hasDaemon || $check) {
		if (lstat(SOCKET_PATH) && !-l _ && -S _ && -o _ && -r _ && -w _) {
			$hasDaemon = 1;
		}
		elsif (_inStartupGrace()) {
			# Daemon is still within startup grace window; do not unlink socket or cache dead state yet
			$hasDaemon = undef;
			return 0;
		}
		else {
			$hasDaemon = 0;
			_unlinkStaleSocket(undef, 1);
		}
	}

	return $hasDaemon;
} }

sub initDaemon { if (!main::ISWINDOWS) {
	startDaemon() if isDaemonEnabled();
	$prefs->setChange(\&_checkDaemonStatus, 'useLocalImageproxy');
} }

sub resize {
	my ($class, $file, $cachekey, $specs, $callback, $cache) = @_;

	my $isDebug = main::DEBUGLOG && $log->is_debug;

	# Encode only a local wire copy for daemon IPC so caller's $file is never mutated in place
	my $wire_file = $file;
	utf8::encode($wire_file) if !ref($wire_file) && utf8::is_utf8($wire_file);

	# Check for callback, and that the gdresized daemon running and read/writable
	# unfortunately AnyEvent seems to struggle with wide characters...
	if (!main::ISWINDOWS && hasDaemon() && $callback && (ref($wire_file) || !utf8::is_utf8($wire_file))) {
		require AnyEvent::Socket;
		require AnyEvent::Handle;

		# Get cache root for passing to daemon
		$cache      ||= Slim::Utils::ArtworkCache->new();
		my $cacheroot = $cache->getRoot();

		main::DEBUGLOG && $isDebug && $log->debug("Using gdresized daemon to resize (pending requests: $pending_requests)");

		$pending_requests++;

		# Daemon available, do an async resize
		AnyEvent::Socket::tcp_connect( 'unix/', SOCKET_PATH, sub {
			my $fh = shift || do {
				my $err            = "$!";
				my $is_connrefused = $!{ECONNREFUSED};
				$log->warn("daemon failed to connect: $err");

				if ($is_connrefused) {
					_unlinkStaleSocket("Removing stale daemon socket: " . SOCKET_PATH);
				}

				$hasDaemon = 0;

				if ( --$pending_requests == 0 ) {
					main::DEBUGLOG && $isDebug && $log->debug("no more pending requests");
				}

				# Fallback to resizing the old way
				sync_resize($file, $cachekey, $specs, $callback, $cache);

				return;
			};

			my $handle;

			# Timer in case daemon craps out
			my $timeout = sub {
				$log->error("daemon timed out");

				$handle && $handle->destroy;
				$handle = undef;

				if ( --$pending_requests == 0 ) {
					main::DEBUGLOG && $isDebug && $log->debug("no more pending requests");
				}

				# Fallback to resizing the old way
				sync_resize($file, $cachekey, $specs, $callback, $cache);
			};
			Slim::Utils::Timers::setTimer( undef, Time::HiRes::time() + SOCKET_TIMEOUT, $timeout );

			$handle = AnyEvent::Handle->new(
				fh       => $fh,
				on_read  => sub {},
				on_eof   => undef,
				on_error => sub {
					my $result = delete $_[0]->{rbuf};

					main::DEBUGLOG && $isDebug && $log->debug("daemon result: " . (defined $result ? $result : '<undef>'));

					$_[0]->destroy;
					$handle = undef;

					Slim::Utils::Timers::killTimers(undef, $timeout);

					if ( --$pending_requests == 0 ) {
						main::DEBUGLOG && $isDebug && $log->debug("no more pending requests");
					}

					if (defined $result && $result =~ /^OK\b/) {
						# tell caller that the data is passed in the cache
						$callback && $callback->(undef, undef, 1);
					}
					else {
						my $msg = defined $result ? $result : 'no response';
						$msg =~ s/[\r\n]+$//;
						$log->warn("daemon resize failed ($msg); falling back to local resize");
						sync_resize($file, $cachekey, $specs, $callback, $cache);
					}
				},
			);

			main::INFOLOG && $log->is_info && $log->info(sprintf("file=%s, spec=%s, cacheroot=%s, cachekey=%s, imagedata=%s bytes\n", ref $file ? 'data' : $wire_file, $specs, $cacheroot, $cachekey, (ref $file ? length($$file) : 0)));

			$handle->push_write( pack('Z* Z* Z* Z* Z*', ref $file ? 'data' : $wire_file, $specs, $cacheroot, $cachekey, (ref $file ? encode_base64($$file, '') : '')) . "\015\012" );
		}, sub {
			# prepare callback, used to set the timeout
			return SOCKET_TIMEOUT;
		} );

		return;
	}
	else {
		# No daemon, resize synchronously in-process
		return sync_resize($file, $cachekey, $specs, $callback, $cache);
	}
}

sub sync_resize {
	my ( $file, $cachekey, $specs, $callback, $cache ) = @_;

	require Slim::Utils::GDResizer;

	my $isDebug = main::DEBUGLOG && $log->is_debug;

	my ($ref, $format);

	my @spec = split(',', $specs);
	eval {
		($ref, $format) = Slim::Utils::GDResizer->gdresize(
			file      => $file,
			spec      => \@spec,
			cache     => $cache || Slim::Utils::ArtworkCache->new(),
			cachekey  => $cachekey,
			debug     => $isDebug,
		);
	};

	if ( main::DEBUGLOG && $isDebug && $@ ) {
		$file = '' if ref $file;
		$log->error("Error resizing $file: $@");
	}

	$callback && $callback->($ref, $format);

	return $@ ? 0 : 1;
}

sub isDaemonEnabled { if (!main::ISWINDOWS) {
	($prefs->get('useLocalImageproxy') || 0) == 2;
} }

sub startDaemon { if (!main::ISWINDOWS) {
	return if $daemon && $daemon->alive;
	return if $daemon_circuit_open;
	return if time() < $daemon_next_retry_time;

	$log->info("Starting daemon...");

	@daemonArgs = ();
	if (main::INFOLOG && $log->is_info) {
		unshift @daemonArgs, '--debug';
	}

	my $command = Slim::Utils::OSDetect::getOS->gdresized();

	if ($command =~ /\.pl$/i || (-f $command && -T $command)) {
		if ($Config{'perlpath'} && -x $Config{'perlpath'}) {
			unshift @daemonArgs, $command;
			$command  = $Config{'perlpath'};
		}
		# pick up our custom Perl build if in use
		elsif (main::ISMAC && -x $^X && $^X !~ m|/usr/bin/perl|) {
			unshift @daemonArgs, $command;
			$command = $^X;
		}
	}

	$daemon_last_start_time = time();
	$hasDaemon = undef;

	eval {
		$daemon = Proc::Background->new(
			{ 'die_upon_destroy' => 1 },
			$command,
			@daemonArgs
		);
	};

	if ($@ || !($daemon && $daemon->alive)) {
		$log->error("Failed to start resizing daemon: " . ($@ || 'process exited immediately'));
		_recordDaemonCrash();
		if (!$daemon_circuit_open) {
			my $wait = $daemon_next_retry_time - time();
			$wait = DAEMON_WATCHDOG_INTERVAL if $wait < DAEMON_WATCHDOG_INTERVAL;
			Slim::Utils::Timers::killTimers(undef, \&_checkDaemonStatus);
			Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
		}
		return;
	}
	elsif (main::INFOLOG && $log->is_info) {
		$log->info('Started resizing daemon: pid ' . $daemon->pid);
	}

	_updateDaemonStatus();

	Slim::Utils::Timers::killTimers(undef, \&_checkDaemonStatus);
	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + DAEMON_WATCHDOG_INTERVAL, \&_checkDaemonStatus);
} }

sub stopDaemon { if (!main::ISWINDOWS) {
	$log->info("Stopping daemon...");
	Slim::Utils::Timers::killTimers( undef, \&_checkDaemonStatus );
	Slim::Utils::Timers::killTimers( undef, \&hasDaemon );
	$daemon_last_start_time = 0;
	$daemon && $daemon->die;
	$daemon = undef;
	$hasDaemon = 0;
	_unlinkStaleSocket();
} }

sub _checkDaemonStatus { if (!main::ISWINDOWS) {
	Slim::Utils::Timers::killTimers( undef, \&_checkDaemonStatus );

	if (!isDaemonEnabled()) {
		$daemon_crash_count     = 0;
		$daemon_circuit_open    = 0;
		$daemon_next_retry_time = 0;
		$daemon_last_start_time = 0;
		stopDaemon() if $daemon && $daemon->alive;
		return;
	}

	if ($daemon_circuit_open) {
		$hasDaemon = 0;
		return;
	}

	if ($daemon && $daemon->alive) {
		# restart if we changed logging options
		if ( (scalar grep /debug/i, @daemonArgs) ? !$log->is_info : $log->is_info ) {
			stopDaemon();
		}
		# LMS thinks there's no daemon, but it's still running - restart it after startup grace
		elsif (!_inStartupGrace() && !hasDaemon(undef, 1)) {
			_recordDaemonCrash();
			stopDaemon();
			return if $daemon_circuit_open;
		}
		elsif (!_inStartupGrace() && $daemon_last_start_time && (time() - $daemon_last_start_time >= DAEMON_RAPID_CRASH_WINDOW)) {
			$daemon_crash_count     = 0;
			$daemon_next_retry_time = 0;
		}
	}
	elsif ($daemon_last_start_time) {
		if (time() - $daemon_last_start_time < DAEMON_RAPID_CRASH_WINDOW) {
			_recordDaemonCrash();
		}
		else {
			$daemon_crash_count     = 0;
			$daemon_next_retry_time = 0;
			$daemon_last_start_time = 0;
			$hasDaemon              = 0;
		}
	}

	return if $daemon_circuit_open;

	if (!($daemon && $daemon->alive) && isDaemonEnabled()) {
		if (time() >= $daemon_next_retry_time) {
			startDaemon();
			return;
		}

		my $wait = $daemon_next_retry_time - time();
		$wait = 1 if $wait < 1;
		Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
		return;
	}

	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + DAEMON_WATCHDOG_INTERVAL, \&_checkDaemonStatus);
} }

sub _updateDaemonStatus { if (!main::ISWINDOWS) {
	Slim::Utils::Timers::killTimers(undef, \&hasDaemon);
	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 5, \&hasDaemon, 1);
} }

1;
