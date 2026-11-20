package Slim::Utils::ImageResizer;

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2025 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use Errno ();
use MIME::Base64 qw(encode_base64);
use Socket ();
use Time::HiRes ();

use Slim::Utils::ArtworkCache;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Prefs;

# UNIX domain socket for optional artwork resizing daemon, if this is
# present we will use async artwork resizing via the external daemon
use constant SOCKET_PATH               => '/tmp/sbs_artwork';
use constant SOCKET_TIMEOUT            => 15;
use constant MAX_PENDING_REQUESTS      => 64;
use constant DAEMON_WATCHDOG_INTERVAL  => 6;
use constant DAEMON_STARTUP_GRACE      => 12;
use constant DAEMON_RAPID_CRASH_WINDOW => 30;
use constant DAEMON_MAX_CRASHES        => 5;
use constant DAEMON_MAX_TIMEOUTS       => 3;
use constant DAEMON_BACKOFF_BASE       => 5;
use constant DAEMON_BACKOFF_MAX        => 120;
use constant DAEMON_CIRCUIT_COOLDOWN   => 300;

my $prefs = preferences('server');
my $log   = logger('artwork');

my $pending_requests = 0;
my ($hasDaemon, $daemon, @daemonArgs);
my $daemon_last_start_time = 0;
my $daemon_last_crash_time = 0;
my $daemon_crash_count     = 0;
my $daemon_timeout_count   = 0;
my $daemon_next_retry_time = 0;
my $daemon_circuit_open    = 0;
my $daemon_half_open       = 0;

sub _inStartupGrace {
	return 0 unless $daemon && $daemon->alive;
	my $started = $daemon->start_time || $daemon_last_start_time;
	return ($started && time() <= $started + DAEMON_STARTUP_GRACE) ? 1 : 0;
}

sub _recordDaemonCrash {
	my $now    = time();
	my $uptime = $daemon_last_start_time ? ($now - $daemon_last_start_time) : 0;

	if ( ($daemon_last_start_time && $uptime >= DAEMON_RAPID_CRASH_WINDOW)
		|| (!$daemon_half_open && $daemon_last_crash_time && ($now - $daemon_last_crash_time) > (DAEMON_RAPID_CRASH_WINDOW + DAEMON_BACKOFF_MAX)) ) {
		$daemon_crash_count = 0;
	}

	$daemon_last_start_time = 0;
	$daemon_last_crash_time = $now;
	$hasDaemon = 0;
	$daemon_timeout_count = 0;
	$daemon_half_open = 0;
	$daemon_crash_count++;

	if ($daemon_crash_count >= DAEMON_MAX_CRASHES) {
		$daemon_circuit_open    = 1;
		$daemon_next_retry_time = $now + DAEMON_CIRCUIT_COOLDOWN;
		$log->error(sprintf(
			"Resizing daemon crashed %d consecutive times (uptime < %ds); circuit breaker open for %ds, falling back to in-process GDResizer",
			$daemon_crash_count, DAEMON_RAPID_CRASH_WINDOW, DAEMON_CIRCUIT_COOLDOWN
		));
		return;
	}

	my $backoff = DAEMON_BACKOFF_BASE * (2 ** ($daemon_crash_count - 1));
	$backoff = DAEMON_BACKOFF_MAX if $backoff > DAEMON_BACKOFF_MAX;
	$daemon_next_retry_time = $now + $backoff;

	$log->warn(sprintf(
		"Resizing daemon exited unexpectedly (%d/%d); backing off for %ds",
		$daemon_crash_count, DAEMON_MAX_CRASHES, $backoff
	));
}

sub _unlinkStaleSocket {
	return if main::ISWINDOWS;

	my ($warn_msg, $only_if_dead) = @_;
	return if $only_if_dead && $daemon && $daemon->alive;
	return unless lstat(SOCKET_PATH);
	if (-l _) {
		$log->warn("Refusing to unlink symlink at daemon socket path: " . SOCKET_PATH);
		return;
	}
	if (-S _ && -o _) {
		$log->warn($warn_msg) if defined $warn_msg;
		unlink SOCKET_PATH;
	}
}

sub _validateLivePeer {
	my ($fh) = @_;
	return 0 unless $fh;

	if (!lstat(SOCKET_PATH) || -l _ || !-S _ || !-o _) {
		$log->warn("Daemon socket path failed ownership/type validation: " . SOCKET_PATH);
		return 0;
	}

	if ($^O eq 'linux' && Socket->can('SO_PEERCRED')) {
		my $so_peercred = eval { Socket::SO_PEERCRED() };
		if (defined $so_peercred) {
			my $cred = getsockopt($fh, Socket::SOL_SOCKET(), $so_peercred);
			if (!defined $cred || length($cred) != 12) {
				$log->warn("Failed to read SO_PEERCRED from daemon socket");
				return 0;
			}
			my ($pid, $uid, $gid) = unpack('i I I', $cred);
			if (!defined $uid || $uid != $> || !defined $pid || $pid <= 0) {
				$log->warn(sprintf("Refusing daemon socket peer with unexpected uid=%s (expected %d)", defined $uid ? $uid : 'undef', $>));
				return 0;
			}
		}
	}

	return 1;
}

sub hasDaemon {
	return 0 if main::ISWINDOWS;

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
}

sub initDaemon {
	return if main::ISWINDOWS;

	startDaemon() if isDaemonEnabled();
	$prefs->setChange(\&_checkDaemonStatus, 'useLocalImageproxy');
}

sub resize {
	my ($class, $file, $cachekey, $specs, $callback, $cache) = @_;

	my $isDebug = main::DEBUGLOG && $log->is_debug;

	if (ref $file) {
		if (ref($file) ne 'SCALAR' || !defined $$file || !length($$file)) {
			$log->warn("Rejecting empty or invalid scalar-ref image data");
			$callback && $callback->(undef, undef);
			return 0;
		}
	}

	# Encode only a local wire copy for daemon IPC so caller's $file is never mutated in place
	my $wire_file = $file;
	utf8::encode($wire_file) if defined($wire_file) && !ref($wire_file) && utf8::is_utf8($wire_file);

	# Check for callback, and that the gdresized daemon is running, read/writable, and within queue bounds
	if (!main::ISWINDOWS && hasDaemon() && $callback && $pending_requests < MAX_PENDING_REQUESTS && (ref($wire_file) || (defined($wire_file) && !utf8::is_utf8($wire_file)))) {
		$cache      ||= Slim::Utils::ArtworkCache->new();
		my $cacheroot = $cache->getRoot();

		my $wire_specs     = defined $specs     ? "$specs"     : '';
		my $wire_cacheroot = defined $cacheroot ? "$cacheroot" : '';
		my $wire_cachekey  = defined $cachekey  ? "$cachekey"  : '';
		utf8::encode($wire_specs)     if utf8::is_utf8($wire_specs);
		utf8::encode($wire_cacheroot) if utf8::is_utf8($wire_cacheroot);
		utf8::encode($wire_cachekey)  if utf8::is_utf8($wire_cachekey);

		my $wire_target = ref($file) ? 'data' : $wire_file;
		for my $field ($wire_target, $wire_specs, $wire_cacheroot, $wire_cachekey) {
			if (!defined $field || $field =~ /[\0\r\n]/) {
				$log->error("Rejecting IPC resize request due to NUL/CR/LF control byte in framing field");
				$callback && $callback->(undef, undef);
				return 0;
			}
		}

		if (!length($wire_target) || !length($wire_cacheroot) || !length($wire_cachekey)) {
			$log->error("Rejecting IPC resize request due to missing required framing field");
			$callback && $callback->(undef, undef);
			return 0;
		}

		my $b64_data = '';
		if (ref $file) {
			my $raw_bytes = $$file;
			if (utf8::is_utf8($raw_bytes) && !utf8::downgrade($raw_bytes, 1)) {
				$log->error("Rejecting IPC resize request due to wide characters in binary image data");
				$callback && $callback->(undef, undef);
				return 0;
			}
			$b64_data = encode_base64($raw_bytes, '');
		}

		my $frame = pack('Z* Z* Z* Z* Z*', $wire_target, $wire_specs, $wire_cacheroot, $wire_cachekey, $b64_data) . "\015\012";
		if (utf8::is_utf8($frame) && !utf8::downgrade($frame, 1)) {
			$log->error("Rejecting IPC resize request: packed frame contains wide characters");
			$callback && $callback->(undef, undef);
			return 0;
		}

		require AnyEvent::Socket;
		require AnyEvent::Handle;

		main::DEBUGLOG && $isDebug && $log->debug("Using gdresized daemon to resize (pending requests: $pending_requests)");

		$pending_requests++;

		# Daemon available, do an async resize
		AnyEvent::Socket::tcp_connect( 'unix/', SOCKET_PATH, sub {
			my $fh = shift;
			if (!$fh || !_validateLivePeer($fh)) {
				my $err            = $fh ? "peer validation failed" : "$!";
				my $is_connrefused = !$fh && $!{ECONNREFUSED};
				close($fh) if $fh;
				$log->warn("daemon failed to connect: $err");

				if ($is_connrefused) {
					_unlinkStaleSocket("Removing stale daemon socket: " . SOCKET_PATH, 1);
				}

				$hasDaemon = 0;
				$pending_requests-- if $pending_requests > 0;
				if ($pending_requests == 0) {
					main::DEBUGLOG && $isDebug && $log->debug("no more pending requests");
				}

				# Fallback to resizing the old way
				sync_resize($file, $cachekey, $specs, $callback, $cache);

				return;
			}

			my $handle;
			my $done = 0;

			# Timer in case daemon craps out
			my $timeout;
			$timeout = sub {
				return if $done;
				$done = 1;

				$log->error("daemon timed out");

				$handle && $handle->destroy;
				$handle = undef;

				$hasDaemon = 0;
				$daemon_timeout_count++;

				$pending_requests-- if $pending_requests > 0;
				if ($pending_requests == 0) {
					main::DEBUGLOG && $isDebug && $log->debug("no more pending requests");
				}

				if ($daemon_timeout_count >= DAEMON_MAX_TIMEOUTS && $daemon && $daemon->alive) {
					$log->error("Resizing daemon timed out $daemon_timeout_count consecutive times; restarting daemon");
					_recordDaemonCrash();
					stopDaemon();
					my $wait = $daemon_next_retry_time - time();
					$wait = 1 if $wait < 1;
					Slim::Utils::Timers::killTimers(undef, \&_checkDaemonStatus);
					Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
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
					return if $done;
					$done = 1;

					my $result = delete $_[0]->{rbuf};

					main::DEBUGLOG && $isDebug && $log->debug("daemon result: " . (defined $result ? $result : '<undef>'));

					$_[0]->destroy;
					$handle = undef;

					Slim::Utils::Timers::killTimers(undef, $timeout);

					$pending_requests-- if $pending_requests > 0;
					if ($pending_requests == 0) {
						main::DEBUGLOG && $isDebug && $log->debug("no more pending requests");
					}

					if (defined $result && $result =~ /^OK\b/) {
						$daemon_timeout_count = 0;
						# tell caller that the data is passed in the cache
						$callback && $callback->(undef, undef, 1);
					}
					else {
						if (!defined $result || $result !~ /^Error:/) {
							$hasDaemon = 0;
						}
						else {
							$daemon_timeout_count = 0;
						}
						my $msg = defined $result ? $result : 'no response';
						$msg =~ s/[\r\n]+$//;
						$log->warn("daemon resize failed ($msg); falling back to local resize");
						sync_resize($file, $cachekey, $specs, $callback, $cache);
					}
				},
			);

			main::INFOLOG && $log->is_info && $log->info(sprintf("file=%s, spec=%s, cacheroot=%s, cachekey=%s, imagedata=%s bytes\n", $wire_target, $wire_specs, $wire_cacheroot, $wire_cachekey, (ref $file ? length($$file) : 0)));

			$handle->push_write($frame);
		}, sub {
			# prepare callback, used to set the timeout
			return SOCKET_TIMEOUT;
		} );

		return;
	}
	else {
		if (!main::ISWINDOWS && hasDaemon() && $callback && $pending_requests >= MAX_PENDING_REQUESTS) {
			main::DEBUGLOG && $isDebug && $log->debug("gdresized queue limit reached ($pending_requests >= " . MAX_PENDING_REQUESTS . "); falling back to sync_resize");
		}
		# No daemon or queue saturated, resize synchronously in-process
		return sync_resize($file, $cachekey, $specs, $callback, $cache);
	}
}

sub sync_resize {
	my ( $file, $cachekey, $specs, $callback, $cache ) = @_;

	require Slim::Utils::GDResizer;

	my $isDebug = main::DEBUGLOG && $log->is_debug;

	my ($ref, $format);

	my @spec = split(',', defined $specs ? $specs : '');
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

sub isDaemonEnabled {
	return 0 if main::ISWINDOWS;

	return (($prefs->get('useLocalImageproxy') || 0) == 2) ? 1 : 0;
}

sub startDaemon {
	return if main::ISWINDOWS;

	return if $daemon && $daemon->alive;
	if ($daemon_circuit_open) {
		return if time() < $daemon_next_retry_time;
		$daemon_circuit_open = 0;
		$daemon_crash_count  = DAEMON_MAX_CRASHES - 1;
		$daemon_half_open    = 1;
	}
	return if time() < $daemon_next_retry_time;

	$log->info("Starting daemon...");

	@daemonArgs = ();
	if (main::INFOLOG && $log->is_info) {
		unshift @daemonArgs, '--debug';
	}

	require Slim::Utils::GDResizer;
	my $command = Slim::Utils::GDResizer->rust_gdresized_bin()
		|| Slim::Utils::OSDetect::getOS->gdresized();

	if ($command =~ /\.pl$/i || (-f $command && -T $command)) {
		if ($^X && -x $^X) {
			unshift @daemonArgs, $command;
			$command = $^X;
		}
	}

	$daemon_last_start_time = time();
	$daemon_timeout_count   = 0;
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
		my $wait = $daemon_next_retry_time - time();
		$wait = DAEMON_WATCHDOG_INTERVAL if $wait < DAEMON_WATCHDOG_INTERVAL;
		Slim::Utils::Timers::killTimers(undef, \&_checkDaemonStatus);
		Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
		return;
	}
	elsif (main::INFOLOG && $log->is_info) {
		$log->info('Started resizing daemon: pid ' . $daemon->pid);
	}

	_updateDaemonStatus();

	Slim::Utils::Timers::killTimers(undef, \&_checkDaemonStatus);
	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + DAEMON_WATCHDOG_INTERVAL, \&_checkDaemonStatus);
}

sub stopDaemon {
	return if main::ISWINDOWS;

	$log->info("Stopping daemon...");
	Slim::Utils::Timers::killTimers( undef, \&_checkDaemonStatus );
	Slim::Utils::Timers::killTimers( undef, \&hasDaemon );
	$daemon_last_start_time = 0;
	$daemon_timeout_count   = 0;
	$daemon && $daemon->die;
	$daemon = undef;
	$hasDaemon = 0;
	_unlinkStaleSocket();
}

sub _checkDaemonStatus {
	return if main::ISWINDOWS;

	Slim::Utils::Timers::killTimers( undef, \&_checkDaemonStatus );

	if (!isDaemonEnabled()) {
		$daemon_crash_count     = 0;
		$daemon_last_crash_time = 0;
		$daemon_timeout_count   = 0;
		$daemon_circuit_open    = 0;
		$daemon_half_open       = 0;
		$daemon_next_retry_time = 0;
		$daemon_last_start_time = 0;
		stopDaemon() if $daemon && $daemon->alive;
		return;
	}

	if ($daemon_circuit_open) {
		if (time() < $daemon_next_retry_time) {
			$hasDaemon = 0;
			my $wait = $daemon_next_retry_time - time();
			$wait = DAEMON_WATCHDOG_INTERVAL if $wait < DAEMON_WATCHDOG_INTERVAL;
			Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
			return;
		}
		$log->info("Resizing daemon circuit breaker cooldown elapsed; attempting half-open restart");
		$daemon_circuit_open = 0;
		$daemon_crash_count  = DAEMON_MAX_CRASHES - 1;
		$daemon_half_open    = 1;
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
			if ($daemon_circuit_open) {
				my $wait = $daemon_next_retry_time - time();
				$wait = DAEMON_WATCHDOG_INTERVAL if $wait < DAEMON_WATCHDOG_INTERVAL;
				Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
				return;
			}
		}
		elsif (!_inStartupGrace() && $daemon_last_start_time && (time() - $daemon_last_start_time >= DAEMON_RAPID_CRASH_WINDOW)) {
			$daemon_crash_count     = 0;
			$daemon_last_crash_time = 0;
			$daemon_next_retry_time = 0;
			$daemon_half_open       = 0;
		}
	}
	elsif ($daemon_last_start_time) {
		if (time() - $daemon_last_start_time < DAEMON_RAPID_CRASH_WINDOW) {
			_recordDaemonCrash();
		}
		else {
			$daemon_crash_count     = 0;
			$daemon_last_crash_time = 0;
			$daemon_next_retry_time = 0;
			$daemon_last_start_time = 0;
			$daemon_half_open       = 0;
			$hasDaemon              = 0;
		}
	}

	if ($daemon_circuit_open) {
		my $wait = $daemon_next_retry_time - time();
		$wait = DAEMON_WATCHDOG_INTERVAL if $wait < DAEMON_WATCHDOG_INTERVAL;
		Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $wait, \&_checkDaemonStatus);
		return;
	}

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
}

sub _updateDaemonStatus {
	return if main::ISWINDOWS;

	Slim::Utils::Timers::killTimers(undef, \&hasDaemon);
	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 5, \&hasDaemon, 1);
}

1;
