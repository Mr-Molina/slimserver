package Slim::Utils::Timers;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

=head1 NAME

Slim::Utils::Timers

=head1 SYNOPSIS

    # Run someCode( $client, @args ) in 20 seconds
    Slim::Utils::Timers::setTimer( $client, Time::HiRes::time() + 20, \&someCode, @args );

    # On second thought, don't run it
    Slim::Utils::Timers::killTimers( $client, \&someCode );

=head1 DESCRIPTION

Schedule functions to run at some point in the future.

Two classes of timers exist - Normal timers and High Priority timers.  High timers are used by
animation routines to ensure smooth animation even when other normal timers may be
scheduled to run.

High timers are also allowed to run any time main::idleStreams() is called, so long-running
tasks should call main::idleStreams() to yield time to animation and audio streaming.

=head1 METHODS

=cut

use strict;

use EV;
use Scalar::Util ();
use Time::HiRes ();

use Slim::Utils::Log;
use Slim::Utils::Misc;

use constant CLEANUP_INTERVAL => 30;
use constant EV_KILL          => 0xDEADBEEF;

my %TIMERS = ();
my %TIMER_LOCATIONS = ();
my $CLEANUP;
my $CLEANUP_TIMER;

my $log = logger('server.timers');

=head2 setHighTimer( $obj, $when, $coderef, @args )

Schedule a high priority timer.  See setTimer for documentation.

=cut

sub setHighTimer {
	my $w = _makeTimer(@_);

	$w->priority(2);

	return $w;
}

=head2 setTimer( $obj, $when, $coderef, @args )

Schedule a normal priority timer.  Returns a reference to the internal timer
object.  This can be passed to killSpecific to remove this specific timer.

=over 4

=item obj

$obj can be any value you would like passed to $coderef as the first argument.

=item when

A hi-res epoch time value for when the timer should fire.  Typically, this is set
with Time::HiRes::time() + $seconds.

=item coderef

A code reference that will be run when the timer fires.  It is passed $obj as the first
argument, followed by any other arguments specified.

=item args

An array of any other arguments to be passed to $coderef.

=back

=cut

*setTimer = \&_makeTimer;

=head2 killTimers ( $obj, $coderef )

Remove all normal timers that match the $obj and $coderef.  Returns the number
of timers removed.

=cut

sub killTimers {
	my $objRef = shift;
	my $subptr = shift || return;

	if ( !defined $objRef ) {
		$objRef = '';
	}

	if ( exists $TIMERS{$subptr} && exists $TIMERS{$subptr}->{$objRef} ) {
		my @timers = @{ $TIMERS{$subptr}->{$objRef} };
		for my $timer ( @timers ) {
			if ( $timer ) {
				delete $TIMER_LOCATIONS{"$timer"};
				$timer->stop;
				$timer->cb->(EV_KILL);
			}
		}

		if ( exists $TIMERS{$subptr} ) {
			delete $TIMERS{$subptr}->{$objRef};

			if ( !keys %{ $TIMERS{$subptr} } ) {
				delete $TIMERS{$subptr};
			}
		}

		return 1;
	}

	return;
}

=head2 killHighTimers ( $obj, $coderef )

Remove all high timers that match the $obj and $coderef.  Returns the number
of timers removed.

=cut

*killHighTimers = \&killTimers;

=head2 killOneTimer( $obj, $coderef )

Remove at most one normal or high timer.  If you know there is only one timer
that will match, this method is slightly faster than killTimers.  Returns 1 if
the timer was removed, 0 if the timer could not be found.

=cut

sub killOneTimer {
	my $objRef = shift;
	my $subptr = shift || return 0;

	if ( !defined $objRef ) {
		$objRef = '';
	}

	return 0 unless exists $TIMERS{$subptr} && exists $TIMERS{$subptr}->{$objRef};

	my $timers  = $TIMERS{$subptr}->{$objRef};
	my $removed = 0;

	for ( my $i = 0; $i < @{$timers}; ) {
		my $timer = $timers->[$i];
		if ( !defined $timer ) {
			splice @{$timers}, $i, 1;
			next;
		}
		delete $TIMER_LOCATIONS{"$timer"};
		splice @{$timers}, $i, 1;
		$timer->stop;
		$timer->cb->(EV_KILL);
		$removed = 1;
		last;
	}

	if ( !@{$timers} ) {
		delete $TIMERS{$subptr}->{$objRef};
		if ( !keys %{ $TIMERS{$subptr} } ) {
			delete $TIMERS{$subptr};
		}
	}

	return $removed;
}

=head2 forgetTimer( $obj )

Remove all timers that match $obj.  Can be used to quickly clear all timers
for a specific $client, for example.

=cut

sub forgetTimer {
	my $objRef = shift;

	return unless defined $objRef;

	my @subs = keys %TIMERS;

	for my $sub ( @subs ) {
		next if $sub eq \&cleanupTimers;
		if ( exists $TIMERS{$sub} && exists $TIMERS{$sub}->{$objRef} ) {
			my @timers = @{ $TIMERS{$sub}->{$objRef} };
			for my $timer ( @timers ) {
				if ( $timer ) {
					next if defined $CLEANUP_TIMER && $timer == $CLEANUP_TIMER;
					delete $TIMER_LOCATIONS{"$timer"};
					$timer->stop;
					$timer->cb->(EV_KILL);
				}
			}

			if ( exists $TIMERS{$sub} ) {
				delete $TIMERS{$sub}->{$objRef};

				if ( !keys %{ $TIMERS{$sub} } ) {
					delete $TIMERS{$sub};
				}
			}
		}
	}

	return;
}

=head2 killSpecific( $timer )

Takes the $timer returned by setTimer or setHighTimer
and removes that specific timer.  Returns 1 if the timer was
removed, 0 if the timer could not be found.

=cut

sub killSpecific {
	my $w = shift;

	return 0 unless defined $w;

	if ( my $loc = delete $TIMER_LOCATIONS{"$w"} ) {
		my ( $sub, $objRef ) = @$loc;
		my $removed = 0;
		if ( exists $TIMERS{$sub} && exists $TIMERS{$sub}->{$objRef} && (my $timers = $TIMERS{$sub}->{$objRef}) ) {
			my $i = 0;
			for my $timer ( @{$timers} ) {
				if ( defined $timer && $timer == $w ) {
					splice @{$timers}, $i, 1;
					$timer->stop;
					$timer->cb->(EV_KILL);
					$removed = 1;
					last;
				}
				$i++;
			}
			if ( !@{$timers} ) {
				delete $TIMERS{$sub}->{$objRef};
				if ( !keys %{ $TIMERS{$sub} } ) {
					delete $TIMERS{$sub};
				}
			}
		}
		return $removed;
	}

	my @subs = keys %TIMERS;

	for my $sub ( @subs ) {
		for my $objRef ( keys %{ $TIMERS{$sub} } ) {
			my $i = 0;
			for my $timer ( @{ $TIMERS{$sub}->{$objRef} } ) {
				if ( defined $timer && $timer == $w ) {
					delete $TIMER_LOCATIONS{"$w"};
					splice @{ $TIMERS{$sub}->{$objRef} }, $i, 1;
					$timer->stop;
					$timer->cb->(EV_KILL);

					if ( !@{ $TIMERS{$sub}->{$objRef} } ) {
						delete $TIMERS{$sub}->{$objRef};
						if ( !keys %{ $TIMERS{$sub} } ) {
							delete $TIMERS{$sub};
						}
					}

					return 1;
				}

				$i++;
			}
		}
	}

	return 0;
}

=head2 firePendingTimer( $obj, $coderef )

Immediately run the specified timer, if it exists.  The timer is then removed
so it will not run at it's previously scheduled time.  Returns 1 if the timer
was run, or undef if the timer does not exist.

=cut

sub firePendingTimer {
	my $objRef = shift;
	my $subptr = shift || return;

	if ( !defined $objRef ) {
		$objRef = '';
	}

	return unless exists $TIMERS{$subptr} && exists $TIMERS{$subptr}->{$objRef};

	my $timers = $TIMERS{$subptr}->{$objRef};

	for ( my $i = 0; $i < @{$timers}; ) {
		my $t = $timers->[$i];
		if ( !defined $t ) {
			splice @{$timers}, $i, 1;
			next;
		}

		splice @{$timers}, $i, 1;
		delete $TIMER_LOCATIONS{"$t"};

		if ( !@{$timers} ) {
			delete $TIMERS{$subptr}->{$objRef};
			if ( !keys %{ $TIMERS{$subptr} } ) {
				delete $TIMERS{$subptr};
			}
		}

		$t->stop;
		$t->invoke;
		return 1;
	}

	if ( !@{$timers} ) {
		delete $TIMERS{$subptr}->{$objRef};
		if ( !keys %{ $TIMERS{$subptr} } ) {
			delete $TIMERS{$subptr};
		}
	}

	return;
}

=head2 timeChanged()

Notify this subsystem that the system clock has been changed

=cut

sub timeChanged {
	EV::now_update;

	# We could possibly consider going through the list of times adjusting
	# when they should fire but it is probably not worth it.
}

sub _removeTimerFromSlot {
	my ( $subptr, $objRef, $w ) = @_;

	return unless defined $subptr && defined $objRef;
	return unless exists $TIMERS{$subptr} && exists $TIMERS{$subptr}->{$objRef};

	my $timers = $TIMERS{$subptr}->{$objRef};
	for ( my $i = $#$timers; $i >= 0; $i-- ) {
		if ( !defined $timers->[$i] || ( defined $w && $timers->[$i] == $w ) ) {
			splice @{$timers}, $i, 1;
		}
	}

	if ( !@{$timers} ) {
		delete $TIMERS{$subptr}->{$objRef};
		if ( !keys %{ $TIMERS{$subptr} } ) {
			delete $TIMERS{$subptr};
		}
	}
}

sub _makeTimer {
	my ($objRef, $when, $subptr, @args) = @_;

	if ( !defined $objRef ) {
		$objRef = '';
	}

	EV::now_update;
	my $now   = Time::HiRes::time();
	my $delay = $when - $now;
	$delay    = 0 if $delay < 0;

	# We could use AnyEvent->timer here but paying the overhead
	# cost of proxying the method is silly
	my $w;
	$w = EV::timer( $delay, 0, sub {
		if ( defined $_[0] && !ref($_[0]) && $_[0] == EV_KILL ) {
			# Nasty hack to destroy the EV::Timer object properly
			if ( defined $w ) {
				$w->stop;
				delete $TIMER_LOCATIONS{"$w"};
				_removeTimerFromSlot( $subptr, $objRef, $w );
				undef $w;
			}
			return;
		}

		main::PERFMON && ($now = AnyEvent->time);

		eval { $subptr->( $objRef, @args ) };

		main::PERFMON && Slim::Utils::PerfMon->check('timers', AnyEvent->time - $now, undef, $subptr);

		if ( $@ ) {
			my $name = main::DEBUGLOG ? Slim::Utils::PerlRunTime::realNameForCodeRef($subptr) : 'unk';

			logError("Timer $name failed: $@");
		}

		# Destroy the timer after it's been run and prune its slot immediately
		if ( defined $w ) {
			$w->stop;
			delete $TIMER_LOCATIONS{"$w"};
			_removeTimerFromSlot( $subptr, $objRef, $w );
			undef $w;
		}
	} );

	_storeTimer( $subptr, $objRef, $w );

	$TIMER_LOCATIONS{"$w"} = [ $subptr, $objRef ];

	if ( !$CLEANUP ) {
		# start periodic cleanup timer
		$CLEANUP = 1;

		$CLEANUP_TIMER = setTimer( undef, $now + CLEANUP_INTERVAL, \&cleanupTimers );
	}

	return $w;
}

# Periodically go through and clean out empty timers
sub cleanupTimers {
	my @subs = keys %TIMERS;

	for my $sub ( @subs ) {
		my @objs = keys %{ $TIMERS{$sub} };
		for my $obj ( @objs ) {
			my $timers = $TIMERS{$sub}->{$obj};
			for ( my $i = $#$timers; $i >= 0; $i-- ) {
				splice( @{$timers}, $i, 1 ) unless defined $timers->[$i];
			}

			if ( !scalar @{$timers} ) {
				# No timers left for this obj, remove it
				delete $TIMERS{$sub}->{$obj};
			}
		}

		if ( !scalar keys %{ $TIMERS{$sub} } ) {
			# No objs left for this coderef, remove it
			delete $TIMERS{$sub};
		}
	}

	$CLEANUP_TIMER = setTimer( undef, Time::HiRes::time() + CLEANUP_INTERVAL, \&cleanupTimers );
}

sub _storeTimer {
	my ( $subptr, $objRef, $w ) = @_;

	$TIMERS{$subptr} ||= {};
	my $slot = $TIMERS{$subptr}->{$objRef} ||= [];
	push @{$slot}, $w;

	# Ensure the timer is destroyed after it fires (by undef $w above)
	Scalar::Util::weaken( $slot->[-1] );
}

1;

__END__
