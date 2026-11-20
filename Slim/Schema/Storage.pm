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

use constant _ALLOWED_SQL_HELPERS => {
	'Slim::Utils::SQLiteHelper' => 'DBIx::Class::Storage::DBI::SQLite',
};

BEGIN {
	require Config;
	require File::Spec;

	my ($vol, $dir) = File::Spec->splitpath(File::Spec->rel2abs(__FILE__));
	my @dirs = File::Spec->splitdir($dir);
	pop @dirs while @dirs && $dirs[-1] eq '';

	if ( @dirs >= 2 && $dirs[-1] eq 'Schema' && $dirs[-2] eq 'Slim' ) {
		splice(@dirs, -2);
		my $root = File::Spec->catpath($vol, File::Spec->catdir(@dirs), '');
		my ($major, $minor) = split /\./, ($Config::Config{version} || '');
		my $ver  = (defined $major && defined $minor) ? "$major.$minor" : '';
		my $arch = $Config::Config{archname} || '';
		(my $arch_short = $arch) =~ s/-gnu//;

		if ( !grep { /(?:^|[\\\/])CPAN(?:$|[\\\/])/ } @INC ) {
			my @extra = grep { -d $_ } (
				$ver ? File::Spec->catdir($root, 'CPAN', 'arch', $ver) : (),
				($ver && $arch) ? File::Spec->catdir($root, 'CPAN', 'arch', $ver, $arch) : (),
				($ver && $arch_short && $arch_short ne $arch) ? File::Spec->catdir($root, 'CPAN', 'arch', $ver, $arch_short) : (),
				File::Spec->catdir($root, 'CPAN'),
			);
			my %have = map { $_ => 1 } @INC;
			for my $inc_dir ( reverse @extra ) {
				unshift @INC, $inc_dir unless $have{$inc_dir};
			}
		}
	}

	my $os = eval { Slim::Utils::OSDetect->getOS() };

	if ( $os ) {
		$sqlHelperClass = $os->sqlHelperClass();

		die 'Unsupported SQL helper class: ' . (defined $sqlHelperClass ? $sqlHelperClass : '<undef>')
			unless defined $sqlHelperClass && exists _ALLOWED_SQL_HELPERS->{$sqlHelperClass};

		require Slim::Utils::SQLiteHelper;

		my $storageClass    = $sqlHelperClass->storageClass();
		my $expectedStorage = _ALLOWED_SQL_HELPERS->{$sqlHelperClass};

		die 'Unsupported storage class: ' . (defined $storageClass ? $storageClass : '<undef>')
			unless defined $storageClass && $storageClass eq $expectedStorage;

		require DBIx::Class::Storage::DBI::SQLite;

		push @ISA, $storageClass;

		if ( defined &main::SCANNER && defined &main::PERFMON ) {
			require Slim::Utils::Log;
			Slim::Utils::Log->import();
			require Slim::Utils::Misc;
			require Slim::Utils::Prefs;
		}
	}
	else {
		require DBIx::Class::Storage::DBI::SQLite;

		push @ISA, 'DBIx::Class::Storage::DBI::SQLite';
	}
}

use Carp::Clan qw/DBIx::Class/;
use Fcntl qw(:flock);
use File::Slurp;
use File::Spec;
use Scalar::Util qw(refaddr);
use subs qw(logError logBacktrace);

unless ( defined &logError ) {
	*logError = sub {
		if ( defined &Slim::Utils::Log::logError ) {
			goto &Slim::Utils::Log::logError;
		}
		warn join(' ', @_), "\n";
	};
}

unless ( defined &logBacktrace ) {
	*logBacktrace = sub {
		if ( defined &Slim::Utils::Log::logBacktrace ) {
			goto &Slim::Utils::Log::logBacktrace;
		}
		warn join(' ', @_), "\n";
	};
}

my %rebuilt;

sub dbh {
	my $self = shift;

	# Make sure we're connected unless we are already shutting down
	unless ( $main::stop ) {
		local $@;
		eval { $self->ensure_connected };
		if ( my $err = $@ ) {
			return $self->throw_exception($err);
		}
	}

	return $self->_dbh;
}

sub _is_transient_lock_error {
	my $err = shift;
	return defined($err) && $err =~ /(?:database(?: table)? is locked|\bbusy\b|SQLITE_BUSY|SQLITE_LOCKED)/i;
}

sub throw_exception {
	my ($self, $msg) = @_;

	if (
		!_is_transient_lock_error($msg)
		&& $msg =~ /(?:database disk image is malformed|is not a database)/i
		&& $msg =~ m{(?:SQLite|(?:^|[\\/=])(?:library|persist)\.db\b|\b(?:persistentdb|tracks_persistent)\b)}i
	) {

		my $dbh = do { local $@; eval { $self->_dbh } };
		my $in_txn = ( $dbh && do { local $@; eval { !$dbh->{AutoCommit} } } ) ? 1 : 0;
		my $dbbase;
		my $transient_lock = 0;
		my $verified_healthy = 0;

		if ( $msg =~ m{(?:^|[\\/=])((?:library|persist)\.db)\b}i ) {
			$dbbase = lc($1);
		}
		elsif ( $msg =~ /\b(?:persistentdb|tracks_persistent)\b/i ) {
			$dbbase = 'persist.db';
		}

		if ( $dbh ) {
			local $dbh->{HandleError} = undef;
			local $dbh->{PrintError}  = 0;
			local $dbh->{RaiseError}  = 1;

			if ( !$dbbase || $dbbase eq 'persist.db' ) {
				local $@;
				my $dbs = eval { $dbh->selectall_arrayref('PRAGMA database_list') };
				if ( _is_transient_lock_error($@) ) {
					$transient_lock = 1;
				}
				elsif ( $dbs && grep { defined $_->[1] && $_->[1] eq 'persistentdb' } @$dbs ) {
					local $@;
					my ($check) = eval { $dbh->selectrow_array('PRAGMA persistentdb.quick_check(1)') };
					if ( _is_transient_lock_error($@) ) {
						$transient_lock = 1;
					}
					elsif ( !$@ && defined $check && lc($check) eq 'ok' ) {
						$verified_healthy = 1;
					}
					else {
						$dbbase = 'persist.db';
						$verified_healthy = 0;
					}
				}
			}

			if ( !$transient_lock && ( !$dbbase || $dbbase eq 'library.db' ) ) {
				local $@;
				my ($check) = eval { $dbh->selectrow_array('PRAGMA main.quick_check(1)') };
				if ( _is_transient_lock_error($@) ) {
					$transient_lock = 1;
				}
				elsif ( !$@ && defined $check && lc($check) eq 'ok' ) {
					$verified_healthy = 1;
				}
				else {
					$dbbase = 'library.db';
					$verified_healthy = 0;
				}
			}
		}

		if ( $transient_lock ) {
			logError("Transient SQLite lock/busy error during integrity check; skipping recovery: $msg");
		}
		elsif ( $verified_healthy ) {
			my $label = $dbbase || 'SQLite';
			logError("SQLite PRAGMA quick_check(1) passed for $label; skipping quarantine: $msg");
		}
		elsif ( $in_txn ) {
			logError("SQLite error occurred inside an active transaction; skipping quarantine and croaking: $msg");
		}
		elsif ( $dbbase ) {
			require Slim::Utils::SQLiteHelper unless Slim::Utils::SQLiteHelper->can('dbFile');
			my $dbfile   = Slim::Utils::SQLiteHelper->dbFile($dbbase, $dbbase =~ /persist/);
			my $lockfile = "$dbfile.lock";

			my $scanner_lock;
			if ( $sqlHelperClass && exists _ALLOWED_SQL_HELPERS->{$sqlHelperClass} && $sqlHelperClass->can('_scannerLockFile') ) {
				local $@;
				$scanner_lock = eval { $sqlHelperClass->_scannerLockFile() };
			}
			if ( !defined $scanner_lock || !length $scanner_lock ) {
				my $cachedir = do {
					local $@;
					eval {
						require Slim::Utils::Prefs unless Slim::Utils::Prefs->can('preferences');
						Slim::Utils::Prefs::preferences('server')->get('librarycachedir');
					};
				};
				if ( defined $cachedir && length $cachedir ) {
					$scanner_lock = File::Spec->catfile($cachedir, 'scanner.lock');
				}
				else {
					my ($vol, $dir) = File::Spec->splitpath($dbfile);
					$scanner_lock   = File::Spec->catpath($vol, $dir, 'scanner.lock');
				}
			}
			if ( Slim::Utils::SQLiteHelper->can('_validateDBPath') ) {
				$scanner_lock = Slim::Utils::SQLiteHelper::_validateDBPath($scanner_lock);
			}
			my $scanner_busy = 0;
			if ( -e $scanner_lock ) {
				if ( open(my $scan_fh, '<', $scanner_lock) ) {
					if ( !flock($scan_fh, LOCK_SH | LOCK_NB) ) {
						$scanner_busy = 1;
					}
					close $scan_fh;
				}
				else {
					$scanner_busy = 1;
				}
			}

			if ( $scanner_busy ) {
				logError("Scanner lock active ($scanner_lock); skipping quarantine: $msg");
			}
			elsif ( open(my $lock_fh, '>>', $lockfile) ) {
				if ( flock($lock_fh, LOCK_EX | LOCK_NB) ) {
					if ( $dbh ) {
						if ( my $kids = do { local $@; eval { $dbh->{CachedKids} } } ) {
							for my $sth ( values %$kids ) {
								local $@;
								eval { $sth->finish } if $sth;
							}
							%$kids = ();
						}
						{ local $@; eval { $self->disconnect }; }
						{ local $@; eval { $dbh->disconnect } if eval { $dbh->{Active} }; }
						{ local $@; eval { $self->_dbh(undef) }; }
					}
					{ local $@; eval { Slim::Schema->clearCachedDbh() }; }

					my @sidecars = ( $dbfile, "$dbfile-wal", "$dbfile-shm" );
					my $tag      = sprintf('.corrupt.%d.%d', time(), $$);
					my $seq      = 0;
					while ( grep { lstat("${_}${tag}") || -e "${_}${tag}" } @sidecars ) {
						$seq++;
						$tag = sprintf('.corrupt.%d.%d.%d', time(), $$, $seq);
					}

					my $quarantineFailed = 0;
					for my $file ( @sidecars ) {
						if ( lstat($file) || -e $file ) {
							my $qfile = "$file$tag";
							if ( !rename($file, $qfile) && (lstat($file) || -e $file) ) {
								logError("Unable to quarantine corrupt SQLite file $file to $qfile: $!");
								$quarantineFailed = 1;
							}
							else {
								logError("Quarantined corrupt SQLite file $file to $qfile");
							}
						}
					}

					if (!$rebuilt{$dbfile} && !$quarantineFailed && !-e $dbfile) {

						# make sure we don't come here again
						$rebuilt{$dbfile}++;

						logError("$msg\nTrying to rebuild $dbfile from scratch.");

						$msg = '';

						{
							local $@;
							eval { Slim::Schema->_connect() };
							if ( my $connect_err = $@ ) {
								close $lock_fh;
								my $fatal = "Unable to open the database image - even tried creating it from scratch: $connect_err";
								logError($fatal);
								logBacktrace($fatal);
								croak($fatal);
							}
						}

						my ($new_storage, $rebuilt_dbh, $err);
						{
							local $@;
							$new_storage = eval { Slim::Schema->storage };
							$err         = $@;
						}
						{
							local $@;
							$rebuilt_dbh = $new_storage ? eval { $new_storage->dbh } : undef;
							$err ||= $@;
						}

						if ( !$rebuilt_dbh ) {
							local $@;
							$rebuilt_dbh = eval { Slim::Schema->dbh };
							$err ||= $@;
						}

						if ( $new_storage && ref($new_storage) && refaddr($new_storage) != refaddr($self) ) {
							{ local $@; eval { $self->_dbh(undef) }; }
							close $lock_fh;

							if ( !$rebuilt_dbh ) {
								my $fatal = "Unable to open the database image - even tried creating it from scratch! [$err]";
								logError($fatal);
								logBacktrace($fatal);
								croak($fatal);
							}

							return $rebuilt_dbh;
						}

						if ( !$rebuilt_dbh ) {
							local $@;
							eval { $self->ensure_connected };

							if ($@) {
								my $reconn_err = $@;
								close $lock_fh;
								my $fatal = "Unable to open the database image - even tried creating it from scratch! [$reconn_err]";
								logError($fatal);
								logBacktrace($fatal);
								croak($fatal);
							}

							$rebuilt_dbh = $self->_dbh;
						}

						close $lock_fh;
						return $rebuilt_dbh;
					}
				}
				else {
					logError("SQLite recovery lock already held ($lockfile); skipping concurrent quarantine: $msg");
				}
				close $lock_fh;
			}
			else {
				logError("Unable to acquire SQLite recovery lock ($lockfile): $!; skipping quarantine");
			}
		}
		else {
			logError("Unable to determine corrupt SQLite database file from error; skipping automatic wipe: $msg");
		}

	} elsif ($msg =~ /SQLite.*no such table: sqlite_stat1/i) {
		logError("SQLite missing sqlite_stat1 table: $msg");
	}

	logBacktrace($msg);

	# Need to propagate the real error so that DBIx::Class::Storage will
	# do the right thing and reconnect to the DB.
	croak($msg);
}

1;

__END__
