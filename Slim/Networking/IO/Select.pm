package Slim::Networking::IO::Select;


# Logitech Media Server Copyright 2003-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use Exporter::Lite;

our @EXPORT;
BEGIN {
	@EXPORT = qw(addRead addWrite addError removeRead removeWrite removeError);
}

use IO::Select ();
use Scalar::Util qw(weaken);

use Slim::Utils::Errno;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::PerlRunTime;

our $USE_EV;
BEGIN {
	if (!defined $USE_EV) {
		local $SIG{__DIE__};
		$USE_EV = eval {
			require EV;
			EV->can('READ') && EV->can('WRITE') && EV->can('LOOP_NONBLOCK') && EV->can('LOOP_ONESHOT') && EV->can('io') && EV->can('loop');
		} ? 1 : 0;
	}
}

use constant MODE_READ     => $USE_EV ? EV::READ()          : 1;
use constant MODE_WRITE    => $USE_EV ? EV::WRITE()         : 2;
use constant MODE_ERROR    => 4;
use constant LOOP_NONBLOCK => $USE_EV ? EV::LOOP_NONBLOCK() : 1;
use constant LOOP_ONESHOT  => $USE_EV ? EV::LOOP_ONESHOT()  : 2;

our $depth = 0;

my ($sel_read, $sel_write, $sel_error);
my @deferred_watchers;

=head1 NAME

Slim::Networking::Select

=head1 SYNOPSIS

Slim::Utils::Select::addRead( $socket, \&callback )

=head1 DESCRIPTION

This module encapsulates all select() related code, handled by Lyrion Music Server's main loop.

Usually, you'll want to use higher such as L<Slim::Networking::Async::HTTP>.

=head1 FUNCTIONS

=cut

my $log = logger('server.select');

sub _safeFileno {
	my $fh = shift;
	return -1 unless defined $fh;
	my $fno = eval { fileno($fh) };
	return defined($fno) ? $fno : -1;
}

sub _slotForMode {
	my $mode = shift;
	return $mode == MODE_READ  ? '_ev_r'
		 : $mode == MODE_WRITE ? '_ev_w'
		 : $mode == MODE_ERROR ? '_ev_e'
		 : undef;
}

sub _ioSlotForMode {
	my $mode = shift;
	return $mode == MODE_READ  ? '_iosel_r'
		 : $mode == MODE_WRITE ? '_iosel_w'
		 : $mode == MODE_ERROR ? '_iosel_e'
		 : undef;
}

sub _ioSelectForMode {
	my $mode = shift;
	if ($mode == MODE_READ) {
		return $sel_read //= IO::Select->new();
	}
	elsif ($mode == MODE_WRITE) {
		return $sel_write //= IO::Select->new();
	}
	elsif ($mode == MODE_ERROR) {
		return $sel_error //= IO::Select->new();
	}
	return undef;
}

=head2 addRead( $sock, $callback )

Add a socket to the select loop for reading.

$callback will be notified when the socket is readable.

=cut

sub addRead {
	_add( MODE_READ, @_ );
}

=head2 removeRead( $sock )

Remove a socket from the select loop and callback notification for reading.

=cut

sub removeRead {
	_remove( MODE_READ, shift );
}

=head2 addWrite( $sock, $callback )

Add a socket to the select loop for writing.

$callback will be notified when the socket is writable..

=cut

sub addWrite {
	_add( MODE_WRITE, @_ );
}

=head2 removeWrite( $sock )

Remove a socket from the select loop and callback notification for write.

=cut

sub removeWrite {
	_remove( MODE_WRITE, shift );
}

=head2 addError( $sock, $callback )

Add a socket to the select loop for error notification.

=cut

sub addError {
	_add( MODE_ERROR, @_ );
}

=head2 removeError( $sock )

Remove a socket from the select loop and callback notification for errors.

=cut

sub removeError {
	_remove( MODE_ERROR, shift );
}

sub _add {
	my ( $mode, $fh, $cb, $idle ) = @_;

	my $fno = _safeFileno($fh);

	if (main::DEBUGLOG && $log->is_debug) {
		$log->debug(
			sprintf('fh=>%s(%d), mode=%s, cb=%s, idle=%d',
				defined($fh) ? $fh : 'undef', $fno,
				$mode == MODE_READ ? 'READ' : $mode == MODE_WRITE ? 'WRITE' : $mode == MODE_ERROR ? 'ERROR' : "??-$mode",
				Slim::Utils::PerlRunTime::realNameForCodeRef($cb),
				$idle || -1));
		if (!defined $fh || $fno < 0) {
			logBacktrace('Invalid FH');
		}
	}

	return unless defined $fh && $fno >= 0 && defined $cb;

	# Replace any existing watcher for this mode cleanly
	_remove($mode, $fh);

	if (!$USE_EV) {
		my $sel    = _ioSelectForMode($mode) || return;
		my $ioslot = _ioSlotForMode($mode)  || return;
		${*$fh}{$ioslot} = {
			cb   => $cb,
			idle => $idle,
		};
		if ($depth >= 2 && !$idle) {
			${*$fh}{"_iosel_def_$ioslot"} = 1;
			my $w_ref = $fh;
			weaken($w_ref) if ref($w_ref);
			push @deferred_watchers, [ $w_ref, $ioslot, $mode ];
		}
		else {
			$sel->add($fh);
		}
		return;
	}

	if ($mode == MODE_ERROR) {
		${*$fh}{_ev_e} = {
			cb   => $cb,
			idle => $idle,
		};
		return;
	}

	my $slot = _slotForMode($mode) || return;

	# Keep only a weak reference to $fh inside the EV watcher closure so
	# $fh -> ${*$fh}{$slot} -> EV::io -> closure -> $fh never forms a strong cycle.
	my $weak_fh = $fh;
	weaken($weak_fh) if ref($weak_fh);

	my $w = EV::io(
		$fno,
		$mode,
		sub {
			my ($watcher, $revents) = @_;

			if (!defined $weak_fh || !defined eval { fileno($weak_fh) }) {
				$watcher->stop if defined $watcher;
				_removeAll($weak_fh) if defined $weak_fh;
				return;
			}

			my $sock = $weak_fh;

			if (defined $revents && ($revents & EV::ERROR())) {
				my $err_entry = ${*$sock}{_ev_e};
				my $pt        = ${*$sock}{passthrough} || [];
				_removeAll($sock);
				if ($err_entry && $err_entry->{cb}) {
					eval {
						local $SIG{__DIE__} = 'DEFAULT';
						$err_entry->{cb}->($sock, @$pt);
					};
					if ($@) {
						my $error = "$@";
						my $func  = main::DEBUGLOG ? Slim::Utils::PerlRunTime::realNameForCodeRef($err_entry->{cb}) : 'unk';
						logError("Select error task failed calling $func: $error; fh=$sock");
					}
				}
				return;
			}

			# If we've recursed into the loop via idleStreams, stop non-idle
			# watchers for this nested pass to avoid level-triggered CPU spin
			# and re-arm them when depth unwinds.
			if ($depth >= 2 && !$idle) {
				$watcher->stop if defined $watcher;
				if (!${*$sock}{"_ev_def_$slot"}) {
					${*$sock}{"_ev_def_$slot"} = 1;
					my $w_ref = $sock;
					weaken($w_ref) if ref($w_ref);
					push @deferred_watchers, [ $w_ref, $slot ];
				}
				return;
			}

			main::PERFMON && (my $now = AnyEvent->time);

			eval {
				# This die handler lets us get a correct backtrace if callback crashes
				local $SIG{__DIE__} = 'DEFAULT';

				$cb->( $sock, @{ ${*$sock}{passthrough} || [] } );
			};

			my $err = $@;

			main::PERFMON && Slim::Utils::PerfMon->check('io', AnyEvent->time - $now, undef, $cb);

			if ($err) {
				_removeAll($sock);
				my $func = main::DEBUGLOG ? Slim::Utils::PerlRunTime::realNameForCodeRef($cb) : 'unk';
				logError("Select task failed calling $func: $err; fh=$sock");
			}
			elsif (!defined eval { fileno($sock) }) {
				_removeAll($sock);
			}
		},
	);

	${*$fh}{$slot} = $w;
}

sub _remove {
	my ( $mode, $fh ) = @_;

	main::DEBUGLOG && $log->is_debug && $log->debug(
		sprintf('fh=>%s(%d), mode=%s',
			defined($fh) ? $fh : 'undef', _safeFileno($fh),
			$mode == MODE_READ ? 'READ' : $mode == MODE_WRITE ? 'WRITE' : $mode == MODE_ERROR ? 'ERROR' : "??-$mode"));

	return unless defined $fh;

	if (my $slot = _slotForMode($mode)) {
		delete ${*$fh}{"_ev_def_$slot"};
		if ($mode == MODE_ERROR) {
			delete ${*$fh}{$slot};
		}
		elsif (my $w = delete ${*$fh}{$slot}) {
			$w->stop;
		}
	}

	if (my $ioslot = _ioSlotForMode($mode)) {
		delete ${*$fh}{"_iosel_def_$ioslot"};
		delete ${*$fh}{$ioslot};
		my $sel = $mode == MODE_READ  ? $sel_read
				: $mode == MODE_WRITE ? $sel_write
				: $mode == MODE_ERROR ? $sel_error
				: undef;
		if ($sel) {
			if (defined eval { fileno($fh) }) {
				$sel->remove($fh);
			}
			else {
				_pruneClosed($sel);
			}
		}
	}
}

sub _removeAll {
	my $fh = shift;
	return unless defined $fh;

	_remove(MODE_READ,  $fh);
	_remove(MODE_WRITE, $fh);
	_remove(MODE_ERROR, $fh);
}

sub _pruneClosed {
	my $sel = shift || return;
	for my $h ($sel->handles) {
		if (!defined $h || !defined eval { fileno($h) }) {
			$sel->remove($h);
		}
	}
}

sub _resumeDeferred {
	while (my $item = shift @deferred_watchers) {
		my ($fh, $slot, $mode) = @$item;
		next unless defined $fh;
		if (defined $mode) {
			next unless delete ${*$fh}{"_iosel_def_$slot"};
			next unless defined eval { fileno($fh) };
			if (${*$fh}{$slot}) {
				my $sel = _ioSelectForMode($mode);
				$sel->add($fh) if $sel;
			}
		}
		else {
			delete ${*$fh}{"_ev_def_$slot"};
			next unless defined eval { fileno($fh) };
			if (my $w = ${*$fh}{$slot}) {
				$w->start;
			}
		}
	}
}

sub _ioSelectLoop {
	my $type = shift;

	if ($depth >= 2) {
		for my $pair (
			[ $sel_read,  '_iosel_r', MODE_READ ],
			[ $sel_write, '_iosel_w', MODE_WRITE ],
			[ $sel_error, '_iosel_e', MODE_ERROR ],
		) {
			my ($sel, $ioslot, $mode) = @$pair;
			next unless $sel;
			for my $fh ($sel->handles) {
				next unless defined $fh;
				if (!defined eval { fileno($fh) }) {
					$sel->remove($fh);
					next;
				}
				my $entry = ${*$fh}{$ioslot};
				next if $entry && $entry->{idle};
				$sel->remove($fh);
				if ($entry && !${*$fh}{"_iosel_def_$ioslot"}) {
					${*$fh}{"_iosel_def_$ioslot"} = 1;
					my $w_ref = $fh;
					weaken($w_ref) if ref($w_ref);
					push @deferred_watchers, [ $w_ref, $ioslot, $mode ];
				}
			}
		}
	}
	elsif (@deferred_watchers) {
		_resumeDeferred();
	}

	_pruneClosed($sel_read)  if $sel_read;
	_pruneClosed($sel_write) if $sel_write;
	_pruneClosed($sel_error) if $sel_error;

	my $timeout = (defined $type && $type == LOOP_NONBLOCK) ? 0 : 0.05;
	my $count = ($sel_read  ? $sel_read->count  : 0)
			  + ($sel_write ? $sel_write->count : 0)
			  + ($sel_error ? $sel_error->count : 0);

	if (!$count) {
		select(undef, undef, undef, $timeout) if $timeout > 0;
		return;
	}

	my ($r_ready, $w_ready, $e_ready) = IO::Select::select($sel_read, $sel_write, $sel_error, $timeout);
	return unless defined $r_ready || defined $w_ready || defined $e_ready;

	if ($e_ready) {
		for my $fh (@$e_ready) {
			next unless defined $fh;
			my $entry = ${*$fh}{_iosel_e};
			next unless $entry && $entry->{cb};
			next if $depth >= 2 && !$entry->{idle};

			my $pt = ${*$fh}{passthrough} || [];
			_removeAll($fh);
			eval {
				local $SIG{__DIE__} = 'DEFAULT';
				$entry->{cb}->($fh, @$pt);
			};
			if ($@) {
				my $error = "$@";
				my $func  = main::DEBUGLOG ? Slim::Utils::PerlRunTime::realNameForCodeRef($entry->{cb}) : 'unk';
				logError("Select error task failed calling $func: $error; fh=$fh");
			}
		}
	}

	for my $pair ([ $r_ready, '_iosel_r' ], [ $w_ready, '_iosel_w' ]) {
		my ($ready, $ioslot) = @$pair;
		next unless $ready;
		for my $fh (@$ready) {
			next unless defined $fh;
			if (!defined eval { fileno($fh) }) {
				_removeAll($fh);
				next;
			}
			my $entry = ${*$fh}{$ioslot};
			next unless $entry && $entry->{cb};
			next if $depth >= 2 && !$entry->{idle};

			main::PERFMON && (my $now = eval { AnyEvent->time });

			eval {
				local $SIG{__DIE__} = 'DEFAULT';
				$entry->{cb}->($fh, @{ ${*$fh}{passthrough} || [] });
			};
			my $err = $@;

			main::PERFMON && defined($now) && Slim::Utils::PerfMon->check('io', AnyEvent->time - $now, undef, $entry->{cb});

			if ($err) {
				_removeAll($fh);
				my $func = main::DEBUGLOG ? Slim::Utils::PerlRunTime::realNameForCodeRef($entry->{cb}) : 'unk';
				logError("Select task failed calling $func: $err; fh=$fh");
			}
			elsif (!defined eval { fileno($fh) }) {
				_removeAll($fh);
			}
		}
	}
}

sub loop {
	my $type = shift;

	# Don't recurse more than once into the loop
	return if $depth >= 2;

	local $depth = $depth + 1;
	my $guard = ($depth >= 2) ? bless([\&_resumeDeferred], 'Slim::Networking::IO::Select::_ScopeGuard') : undef;

	if ($USE_EV) {
		EV::loop( $type );
	}
	else {
		_ioSelectLoop( $type );
	}
}

package Slim::Networking::IO::Select::_ScopeGuard;

sub DESTROY {
	my $self = shift;
	eval { $self->[0]->() };
}

package Slim::Networking::IO::Select;

=head1 SEE ALSO

L<IO::Select>

L<Slim::Networking::Async::HTTP>

L<Slim::Networking::Slimproto>

=cut

1;

__END__

# Local Variables:
# tab-width:4
# indent-tabs-mode:t
# End:
