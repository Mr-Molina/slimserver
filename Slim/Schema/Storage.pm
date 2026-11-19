package Slim::Schema::Storage;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

# Shim to get a backtrace out of throw_exception

use strict;
use vars qw(@ISA);

use Slim::Utils::OSDetect;
my $sqlHelperClass;

BEGIN {
	$sqlHelperClass = Slim::Utils::OSDetect->getOS()->sqlHelperClass();

	my $storageClass = $sqlHelperClass->storageClass();
	eval "use $storageClass";
	die $@ if $@;

	push @ISA, $storageClass;
}

use Carp::Clan qw/DBIx::Class/;
use File::Slurp;
use File::Spec;
use Scalar::Util qw(refaddr);

use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Prefs;

my %rebuilt;

sub dbh {
	my $self = shift;

	# Make sure we're connected unless we are already shutting down
	unless ( $main::stop ) {
		eval { $self->ensure_connected };
	}

	if ($@) {
		return $self->throw_exception($@);
	}

	return $self->_dbh;
}

sub throw_exception {
	my ($self, $msg) = @_;

	if (
		$msg =~ /(?:database disk image is malformed|is not a database)/i
		&& $msg =~ m{(?:SQLite|(?:^|[\\/=])(?:library|persist)\.db\b|\b(?:persistentdb|tracks_persistent)\b)}i
	) {

		my $dbh = eval { $self->_dbh };
		my $dbbase;

		if ( $msg =~ m{(?:^|[\\/=])((?:library|persist)\.db)\b}i ) {
			$dbbase = lc($1);
		}
		elsif ( $msg =~ /\b(?:persistentdb|tracks_persistent)\b/i ) {
			$dbbase = 'persist.db';
		}
		elsif ( $dbh ) {
			local $dbh->{HandleError} = undef;
			local $dbh->{PrintError}  = 0;
			local $dbh->{RaiseError}  = 1;

			my $dbs = eval { $dbh->selectall_arrayref('PRAGMA database_list') };
			if ( $dbs && grep { defined $_->[1] && $_->[1] eq 'persistentdb' } @$dbs ) {
				my ($check) = eval { $dbh->selectrow_array('PRAGMA persistentdb.quick_check(1)') };
				if ( $@ || !$check || lc($check) ne 'ok' ) {
					$dbbase = 'persist.db';
				}
			}

			if ( !$dbbase ) {
				my ($check) = eval { $dbh->selectrow_array('PRAGMA main.quick_check(1)') };
				if ( $@ || !$check || lc($check) ne 'ok' ) {
					$dbbase = 'library.db';
				}
			}
		}

		if ( $dbbase ) {
			my $dbfile = Slim::Utils::SQLiteHelper->dbFile($dbbase, $dbbase =~ /persist/);

			if ( $dbh ) {
				if ( my $kids = eval { $dbh->{CachedKids} } ) {
					for my $sth ( values %$kids ) {
						eval { $sth->finish } if $sth;
					}
					%$kids = ();
				}
				eval { $self->disconnect };
				eval { $dbh->disconnect } if eval { $dbh->{Active} };
				eval { $self->_dbh(undef) };
			}
			eval { Slim::Schema->clearCachedDbh() };

			my $unlinkFailed = 0;
			for my $file ( $dbfile, "$dbfile-wal", "$dbfile-shm" ) {
				if ( lstat($file) || -e $file ) {
					if ( !unlink($file) && (lstat($file) || -e $file) ) {
						logError("Unable to unlink corrupt SQLite file $file: $!");
						$unlinkFailed = 1;
					}
				}
			}

			if (!$rebuilt{$dbfile} && !$unlinkFailed && !-f $dbfile) {

				# make sure we don't come here again
				$rebuilt{$dbfile}++;

				logError("$msg\nTrying to rebuild $dbfile from scratch.");

				$msg = '';

				Slim::Schema->_connect();

				my $new_storage = eval { Slim::Schema->storage };
				my $err         = $@;
				my $rebuilt_dbh = $new_storage ? eval { $new_storage->dbh } : undef;
				$err ||= $@;

				if ( !$rebuilt_dbh ) {
					$rebuilt_dbh = eval { Slim::Schema->dbh };
					$err ||= $@;
				}

				if ( $new_storage && ref($new_storage) && refaddr($new_storage) != refaddr($self) ) {
					eval { $self->_dbh(undef) };

					if ( !$rebuilt_dbh ) {
						logError("Unable to open the database image - even tried creating it from scratch!");
						logError("Fatal. Exiting. [$err]");
						exit;
					}

					return $rebuilt_dbh;
				}

				if ( !$rebuilt_dbh ) {
					eval { $self->ensure_connected };

					if ($@) {
						logError("Unable to open the database image - even tried creating it from scratch!");
						logError("Fatal. Exiting. [$@]");
						exit;
					}

					$rebuilt_dbh = $self->_dbh;
				}

				return $rebuilt_dbh;
			}
		}
		else {
			logError("Unable to determine corrupt SQLite database file from error; skipping automatic wipe: $msg");
		}

	} elsif ($msg =~ /SQLite.*no such table: sqlite_stat1/i) {
		# no need to croak on a new restart from scratch, users think it's a bad thing
		return;
	}

	logBacktrace($msg);

	# Need to propagate the real error so that DBIx::Class::Storage will
	# do the right thing and reconnect to the DB.
	croak($msg);
}

1;

__END__
