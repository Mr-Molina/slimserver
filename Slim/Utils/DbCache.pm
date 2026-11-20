package Slim::Utils::DbCache;

# Lightweight, efficient, and fast cache
#
# This class is roughly 9x faster for get, and 12x faster for set than using Cache::FileCache.
# Using a SQLite database also makes it much faster to remove the cache.

use strict;

use Config;
use DBD::SQLite;
use Digest::MD5 ();
use Fcntl qw(:flock O_CREAT O_RDWR);
use File::Basename qw(dirname);
use File::Path qw(mkpath);
use File::Spec::Functions qw(catfile);
use Scalar::Util ();
use Storable qw(freeze thaw);
use Time::HiRes ();

use constant HAS_64BIT_INT             => $Config{ivsize} >= 8;
use constant DEFAULT_EXPIRES_TIME      => 60 * 60;
use constant IDLE_THRESHOLD            => 600;       # player inactivity before we consider it unused
use constant WAL_AUTOCHECKPOINT        => 1000;
use constant DEFAULT_MMAP_SIZE         => 67108864;
use constant HIGHMEM_MMAP_SIZE         => 268435456;
use constant DEFAULT_JOURNAL_SIZE_LIMIT => 16777216;
use constant DEFAULT_TEMP_STORE        => 'MEMORY';

my %ALLOWED_PRAGMAS = map { $_ => 1 } qw(
	analysis_limit
	auto_vacuum
	busy_timeout
	cache_size
	foreign_keys
	integrity_check
	journal_mode
	journal_size_limit
	locking_mode
	mmap_size
	optimize
	page_size
	quick_check
	shrink_memory
	synchronous
	temp_store
	wal_autocheckpoint
	wal_checkpoint
);

BEGIN { require Math::BigInt unless HAS_64BIT_INT; }

sub new {
	my ( $class, $args ) = @_;

	return unless $args->{namespace};

	if ( defined $args->{root} ) {
		_validate_db_path($args->{root});
	}

	$args->{default_expires_in} ||= DEFAULT_EXPIRES_TIME;

	my $self = bless $args, $class;

	if ( !defined $self->{root} ) {
		require Slim::Utils::Prefs;
		$self->{root} = _validate_db_path(
			Slim::Utils::Prefs::preferences('server')->get('cachedir')
		);

		Scalar::Util::weaken( my $weak_self = $self );

		# Update root value if cachedir changes
		Slim::Utils::Prefs::preferences('server')->setChange( sub {
			return unless $weak_self;
			$weak_self->wipe;
			$weak_self->setRoot( $_[1] );
			$weak_self->_init_db;
		}, 'cachedir' );
	}

	return $self;
}

sub getRoot {
	return shift->{root};
}

sub setRoot {
	my ( $self, $root ) = @_;

	$self->{root} = defined $root ? _validate_db_path($root) : $root;
}

sub wipe {
	my $self = shift;

	if ( my $dbh = $self->_init_db ) {
		$dbh->do('DELETE FROM cache'); # truncate
		$self->_close_db;
	}
}
*clear = \&wipe;

sub set {
	my ( $self, $key, $data, $expiry ) = @_;

	$self->_init_db;

	$expiry = _canonicalize_expiration_time(defined $expiry ? $expiry : $self->{default_expires_in});

	my $id = _key($key);

	if (ref $data) {
		$data = freeze( $data );
	}
	elsif (utf8::is_utf8($data)) {
		utf8::encode($data);
	}

	# Insert or replace the value
	my $set = $self->{set_sth};
	$set->bind_param( 1, $id );
	$set->bind_param( 2, $data, DBI::SQL_BLOB );
	$set->bind_param( 3, $expiry ) unless $self->{noexpiry};
	$set->execute;
}

sub get {
	my ( $self, $key ) = @_;

	$self->_init_db;

	my $id = _key($key);

	my $get = $self->{get_sth};
	$get->execute($id);

	my ($data, $expiry) = $get->fetchrow_array;

	if ($expiry && !$self->{noexpiry} && $expiry >= 0 && $expiry < time()) {
		$data = undef;
#		$self->{delete_sth}->execute($id);
	}

	if ( defined $data ) {
		my $thawed = eval { thaw($data) };
		if ( $@ ) {
			# Plain scalar (not Storable-frozen): decode UTF-8 bytes back to characters
			utf8::decode($data) unless ref $data;
		}
		else {
			$data = $thawed;
			utf8::decode($data) if defined $data && !ref $data;
		}
	}

	return $data;
}

sub remove {
	my ( $self, $key ) = @_;

	$self->_init_db;

	my $id = _key($key);
	$self->{delete_sth}->execute($id);
}

sub checkActivity {
	my ( $self, $cb) = @_;

	my $hourOfTheDay = (localtime())[2];

	# don't purge if a player is active - retry later
	for my $client ( Slim::Player::Client::clients() ) {
		if ($client->controller->isPlaying()
			# Purge the cache in the early morning hours (2..4), even if some activity is detected
			|| (($hourOfTheDay < 2 || $hourOfTheDay > 4) && $client->power && (Time::HiRes::time() - $client->lastActivityTime) < IDLE_THRESHOLD)
		) {
			$cb->($client) if $cb;
			last;
		}
	}
}

sub purge {
	my ( $self, $limit ) = @_;

	return 0 if $self->{noexpiry};

	my $dbh = $self->_init_db;

	$dbh->sqlite_progress_handler(20, sub {
		main::idleStreams() if defined &main::idleStreams;
		return 0;
	}) if !(defined &main::SCANNER && main::SCANNER());

	my $now = time();
	my $deleted;
	eval {
		if ( defined $limit ) {
			$limit = int($limit);
			if ( $limit > 0 ) {
				my $sth = $dbh->prepare(
					'DELETE FROM cache WHERE k IN (SELECT k FROM cache WHERE t >= 0 AND t < ? LIMIT ?)'
				);
				$sth->bind_param( 1, $now,   DBI::SQL_INTEGER );
				$sth->bind_param( 2, $limit, DBI::SQL_INTEGER );
				$deleted = $sth->execute();
			}
		}
		else {
			my $sth = $dbh->prepare(
				'DELETE FROM cache WHERE t >= 0 AND t < ?'
			);
			$sth->bind_param( 1, $now, DBI::SQL_INTEGER );
			$deleted = $sth->execute();
		}
	};
	my $err = $@;

	$dbh->sqlite_progress_handler(0, undef)
		if $dbh && $dbh->{Active} && !(defined &main::SCANNER && main::SCANNER());

	die $err if $err;

	return int($deleted) if $deleted;
}

sub _key {
	my ( $key ) = @_;

	utf8::encode($key) if utf8::is_utf8($key);

	# Get a 60-bit unsigned int from MD5 (SQLite uses 64-bit signed ints for the key)
	# Bitwise pack 32-bit hi and 28-bit lo into a 60-bit integer
	my $md5 = Digest::MD5::md5_hex($key);
	if ( HAS_64BIT_INT ) {
		return (hex( substr($md5, 0, 8) ) << 28) | hex( substr($md5, 8, 7) );
	}

	return Math::BigInt->new('0x' . substr($md5, 0, 8))
		->blsft(28)
		->bior(hex( substr($md5, 8, 7) ))
		->bstr();
}

sub _validate_pragma {
	my ($pragma) = @_;

	die "Invalid SQLite PRAGMA: undefined\n" unless defined $pragma;
	$pragma =~ s/^\s+|\s+$//g;

	die "Invalid SQLite PRAGMA: disallowed characters\n"
		if !length($pragma) || $pragma =~ /[;\0'"\r\n]/ || $pragma =~ /--|\/\*/;

	if ( $pragma =~ /^(?:([a-zA-Z_][a-zA-Z0-9_]*)\.)?([a-zA-Z_][a-zA-Z0-9_]*)(?:\s*=\s*([a-zA-Z0-9_-]+)|\s*\(\s*([a-zA-Z0-9_-]+)\s*\))?$/ ) {
		my ($schema, $name, $eq_val, $arg_val) = ($1, $2, $3, $4);
		die "Disallowed SQLite PRAGMA schema: $schema\n"
			if defined $schema && $schema !~ /^(?:main|temp)$/i;
		die "Disallowed SQLite PRAGMA name: $name\n"
			unless $ALLOWED_PRAGMAS{lc($name)};

		my $validated = defined $schema ? "$schema.$name" : $name;
		$validated .= " = $eq_val" if defined $eq_val;
		$validated .= "($arg_val)" if defined $arg_val;

		return ($validated, lc($name));
	}

	die "Malformed SQLite PRAGMA: $pragma\n";
}

sub pragma {
	my ( $self, $pragma ) = @_;

	my ($validated, $pragma_name) = _validate_pragma($pragma);

	if ( my $dbh = $self->_init_db ) {
		$dbh->do("PRAGMA $validated");

		if ( $pragma_name eq 'locking_mode' ) {
			# if changing the locking_mode we need to run a statement to change the lock
			$dbh->do('SELECT 1 FROM cache LIMIT 1');
		}
	}
}

sub close {
	my $self = shift;

	$self->_close_db;
}

# The following function is mostly borrowed from Cache::BaseCache

# map of expiration formats to their respective time in seconds
my %_Expiration_Units = ( map(($_,             1), qw(s second seconds sec)),
                          map(($_,            60), qw(m minute minutes min)),
                          map(($_,         60*60), qw(h hour hours)),
                          map(($_,      60*60*24), qw(d day days)),
                          map(($_,    60*60*24*7), qw(w week weeks)),
                          map(($_,   60*60*24*30), qw(M month months)),
                          map(($_,  60*60*24*365), qw(y year years)) );

# turn a string in the form "[number] [unit]" into an explicit number
# of seconds from the present.  E.g, "10 minutes" returns "600"
sub _canonicalize_expiration_time {
	my ( $expiry ) = @_;

	if ( lc( $expiry ) eq 'now' ) {
		$expiry = 0;
	}
	elsif ( lc( $expiry ) eq 'never' ) {
		$expiry = -1;
	}
	elsif ( $expiry =~ /^\s*([+-]?(?:\d+|\d*\.\d*))\s*$/ ) {
		$expiry = $1;
	}
	elsif ( $expiry =~ /^\s*([+-]?(?:\d+|\d*\.\d*))\s*(\w*)\s*$/ && $_Expiration_Units{ $2 } ) {
		$expiry = time() + ( $_Expiration_Units{ $2 } ) * $1;
	}
	else {
		$expiry = DEFAULT_EXPIRES_TIME;
	}

	# "If value is less than 60*60*24*30 (30 days), time is assumed to be
	# relative from the present. If larger, it's considered an absolute Unix time."
	if ( $expiry <= 2592000 && $expiry > -1 ) {
		$expiry += time();
	}

	return $expiry;
}

sub _validate_db_path {
	my ($path) = @_;

	die "Invalid SQLite database path: undefined\n" unless defined $path && length $path;
	die "Invalid SQLite database path: disallowed characters\n" if $path =~ /[;\0\r\n?%]/;
	die "Invalid SQLite database path: dot-traversal disallowed\n" if $path =~ /(?:^|[\/\\=:])\.\.(?:[\/\\]|$)/;

	return $path;
}

sub _validate_dsn {
	my ($dsn) = @_;

	die "Invalid SQLite DSN: undefined\n" unless defined $dsn && length $dsn;
	die "Invalid SQLite DSN: disallowed characters\n" if $dsn =~ /[;\0\r\n?%]/;
	die "Invalid SQLite DSN: URI parameters disallowed\n" if $dsn =~ /uri=/i;
	die "Invalid SQLite DSN: dot-traversal disallowed\n" if $dsn =~ /(?:^|[\/\\=:])\.\.(?:[\/\\]|$)/;
	die "Invalid SQLite DSN: must start with dbi:SQLite:dbname=\n"
		unless $dsn =~ /^dbi:SQLite:dbname=(.+)$/;

	_validate_db_path($1);

	return $dsn;
}

sub _get_dbfile {
	my $self = shift;

	my $namespace = $self->{namespace};
	die "Invalid DbCache namespace: undefined\n" unless defined $namespace && length $namespace;
	die "Invalid DbCache namespace: disallowed characters\n"
		if $namespace =~ /[;\\\0]/ || $namespace eq '.' || $namespace eq '..' || $namespace =~ /\.\./;

	# namespace should not be longer than 8 characters on Windows, as it was causing DB corruption
	if ( defined &main::ISWINDOWS && main::ISWINDOWS() && length($namespace) > 8 ) {
		$namespace = lc(substr($namespace, 0, 4)) . substr(Digest::MD5::md5_hex($namespace), 0, 4);
	}
	# some plugins use paths in their namespace - which was compatible with FileCache, but no longer is
	elsif ( !(defined &main::ISWINDOWS && main::ISWINDOWS()) ) {
		$namespace =~ s/\//-/g;
	}

	my $root = _validate_db_path($self->{root});

	return _validate_db_path( catfile( $root, $namespace . '.db' ) );
}

# only try to re-build once per dbfile
my %rebuilt;

sub _is_lock_error {
	my ($err) = @_;
	return defined $err && $err =~ /database is locked|database table is locked|SQLITE_BUSY|SQLITE_LOCKED/i;
}

sub _is_corrupt_error {
	my ($err) = @_;
	return 0 unless defined $err;
	return 0 if _is_lock_error($err);
	return $err =~ /(?:database disk image is malformed|file is not a database|is not a database|malformed database schema)/i ? 1 : 0;
}

sub _try_acquire_lock {
	my ($lockfile, $max_attempts, $delay) = @_;

	$max_attempts ||= 50;
	$delay        ||= 0.1;

	my $dir = dirname($lockfile);
	eval { mkpath($dir) } if $dir && !-d $dir;

	my $fh;
	if ( !sysopen($fh, $lockfile, O_CREAT | O_RDWR) ) {
		warn "Unable to open lock file $lockfile: $!\n";
		return undef;
	}

	for my $attempt ( 1 .. $max_attempts ) {
		if ( flock($fh, LOCK_EX | LOCK_NB) ) {
			return $fh;
		}
		Time::HiRes::sleep($delay) if $attempt < $max_attempts;
	}

	CORE::close($fh);
	return undef;
}

sub _quarantine_corrupt_db {
	my ($dbfile, $lock_held) = @_;

	my $lock_fh;
	if ( !$lock_held ) {
		$lock_fh = _try_acquire_lock("$dbfile.lock", 50, 0.1);
		if ( !$lock_fh ) {
			warn "Unable to acquire exclusive quarantine lock for $dbfile\n";
			return 0;
		}
	}

	my $ts     = time();
	my $suffix = sprintf('.corrupt.%d.%d', $ts, $$);
	my $n      = 0;
	while ( -e "$dbfile$suffix" || lstat("$dbfile$suffix") ) {
		$n++;
		$suffix = sprintf('.corrupt.%d.%d.%d', $ts, $$, $n);
	}

	my $ok = 1;
	for my $file ( $dbfile, "$dbfile-wal", "$dbfile-shm" ) {
		if ( lstat($file) || -e $file ) {
			my $target = "$file$suffix";
			if ( !rename($file, $target) && (lstat($file) || -e $file) ) {
				warn "Unable to quarantine $file to $target: $!\n";
				$ok = 0;
			}
		}
	}

	if ( $lock_fh ) {
		flock($lock_fh, LOCK_UN);
		CORE::close($lock_fh);
	}

	return $ok;
}

sub _recover_corrupt_db {
	my ($self, $dbfile, $err) = @_;

	my $lock_fh = _try_acquire_lock("$dbfile.lock", 50, 0.1);
	if ( !$lock_fh ) {
		die "Unable to acquire exclusive recovery lock for $dbfile: $err\n";
	}

	my $dbh;
	eval {
		# Re-check under exclusive lock in case another process already recovered it
		my $recheck_dbh;
		eval {
			$recheck_dbh = $self->_init_db(1);
			local $recheck_dbh->{HandleError} = undef;
			local $recheck_dbh->{RaiseError}  = 1;
			local $recheck_dbh->{PrintError}  = 0;
			my ($qc) = $recheck_dbh->selectrow_array('PRAGMA quick_check(1)');
			die 'SQLite database disk image is malformed (' . (defined $qc ? $qc : 'undef') . ')'
				if !defined $qc || lc($qc) ne 'ok';
		};
		if ( my $recheck_err = $@ ) {
			$self->_close_db;
			if ( $recheck_dbh ) {
				eval { $recheck_dbh->{HandleError} = undef };
				eval { $recheck_dbh->disconnect } if eval { $recheck_dbh->{Active} };
			}
			die $recheck_err unless _is_corrupt_error($recheck_err);

			_quarantine_corrupt_db($dbfile, 1)
				or die "Unable to quarantine corrupt $dbfile: $recheck_err\n";

			$dbh = $self->_init_db(1);
		}
		else {
			$dbh = $recheck_dbh;
		}
	};
	my $rec_err = $@;

	flock($lock_fh, LOCK_UN);
	CORE::close($lock_fh);

	die $rec_err if $rec_err;

	return $dbh;
}

sub _init_db {
	my $self  = shift;
	my $retry = shift || 0;

	return $self->{dbh} if $self->{dbh};

	my $dbfile = $self->_get_dbfile;
	my $dsn    = _validate_dsn("dbi:SQLite:dbname=$dbfile");

	my $dbh;
	my ($set_sth, $get_sth, $delete_sth);

	eval {
		$dbh = DBI->connect( $dsn, '', '', {
			AutoCommit => 1,
			PrintError => 0,
			RaiseError => 1,
			sqlite_use_immediate_transaction => 1,
		} ) or die $DBI::errstr || "Unable to connect to $dbfile";

		$dbh->do('PRAGMA busy_timeout = 5000');

		# caches do see a lot of updates/writes/deletes - enable auto_vacuum
		if ( !$dbh->selectrow_array('PRAGMA auto_vacuum') ) {
			$dbh->do('PRAGMA auto_vacuum = FULL');
			# XXX - running a vacuum automatically might take a long time on larger cache files
			# only enable auto_vacuum when a file is newly created
			#$dbh->do('VACUUM');
		}

		$dbh->do('PRAGMA synchronous = NORMAL');
		$dbh->do('PRAGMA journal_mode = WAL');
		$dbh->do('PRAGMA wal_autocheckpoint = ' . WAL_AUTOCHECKPOINT);

		my ($dbhighmem, $dbjournalsize);
		if (defined &main::RESIZER && main::RESIZER()) {
			require Slim::Utils::Light;
			$dbhighmem = Slim::Utils::Light::getPref('dbhighmem');
			$dbjournalsize = Slim::Utils::Light::getPref('dbjournalsize');
		}
		else {
			require Slim::Utils::Prefs;
			my $prefs = Slim::Utils::Prefs::preferences('server');
			$dbhighmem = $prefs->get('dbhighmem');
			$dbjournalsize = $prefs->get('dbjournalsize');
		}

		my $journal_size_limit = (defined $dbjournalsize && $dbjournalsize =~ /^\d+$/)
			? ($dbjournalsize * 1024 * 1024)
			: DEFAULT_JOURNAL_SIZE_LIMIT;
		$dbh->do("PRAGMA journal_size_limit = $journal_size_limit");

		$dbh->do('PRAGMA mmap_size = ' . ($dbhighmem ? HIGHMEM_MMAP_SIZE : DEFAULT_MMAP_SIZE));
		$dbh->do('PRAGMA temp_store = ' . DEFAULT_TEMP_STORE);

		# Use negative KiB cache_size semantics: 20 MiB for dbhighmem, 8000 KiB otherwise
		$dbh->do('PRAGMA cache_size = ' . ($dbhighmem ? -20000 : -8000));

		# Create the table, note that using an integer primary key
		# is much faster than any other kind of key, such as a char
		# because it doesn't have to create an index
		my $started_tx = $dbh->{AutoCommit} ? eval { $dbh->begin_work; 1 } : 0;
		eval {
			if ($self->{noexpiry}) {
				$dbh->do('CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB)');
			}
			else {
				$dbh->do('CREATE TABLE IF NOT EXISTS cache (k INTEGER PRIMARY KEY, v BLOB, t INTEGER)');
				$dbh->do('CREATE INDEX IF NOT EXISTS expiry ON cache (t)');
			}
			$dbh->commit if $started_tx;
		};
		if ( my $ddl_err = $@ ) {
			eval { $dbh->rollback } if $started_tx;
			die $ddl_err;
		}

		if ($self->{noexpiry}) {
			$set_sth = $dbh->prepare('INSERT OR REPLACE INTO cache (k, v) VALUES (?, ?)');
			$get_sth = $dbh->prepare('SELECT v FROM cache WHERE k = ?');
		}
		else {
			$set_sth = $dbh->prepare('INSERT OR REPLACE INTO cache (k, v, t) VALUES (?, ?, ?)');
			$get_sth = $dbh->prepare('SELECT v, t FROM cache WHERE k = ?');
		}
		$delete_sth = $dbh->prepare('DELETE FROM cache WHERE k = ?');
	};

	if ( $@ ) {
		my $err = $@;

		for my $sth ($set_sth, $get_sth, $delete_sth) {
			eval { $sth->finish } if $sth;
		}
		undef $set_sth;
		undef $get_sth;
		undef $delete_sth;

		if ( $dbh ) {
			eval { $dbh->{HandleError} = undef };
			eval { $dbh->disconnect };
			undef $dbh;
		}
		$self->_close_db;

		if ( _is_lock_error($err) ) {
			if ( $retry < 2 ) {
				Time::HiRes::sleep(0.1);
				return $self->_init_db($retry + 1);
			}
			die "Database $dbfile is locked: $err\n";
		}
		elsif ( _is_corrupt_error($err) ) {
			if ( $retry || $rebuilt{$dbfile}++ ) {
				die "Unable to read/create corrupt $dbfile: $err\n";
			}

			warn "${err}Quarantining corrupt $dbfile and starting from scratch.\n";

			# Make sure cachedir exists
			Slim::Utils::Prefs::makeCacheDir() unless defined &main::RESIZER && main::RESIZER();

			return $self->_recover_corrupt_db($dbfile, $err);
		}
		else {
			# Make sure cachedir exists for fresh environments, without touching any live DB
			Slim::Utils::Prefs::makeCacheDir() unless defined &main::RESIZER && main::RESIZER();

			if ( !$retry && !-e $dbfile ) {
				return $self->_init_db(1);
			}

			die "Unable to read/create $dbfile: $err\n";
		}
	}

	$self->{set_sth}    = $set_sth;
	$self->{get_sth}    = $get_sth;
	$self->{delete_sth} = $delete_sth;
	$self->{dbh}        = $dbh;

	# set up error handler to log the db file causing problems
	$dbh->{HandleError} = sub {
		my ($msg, $handle, $value) = @_;

		my $dbfile = $self->_get_dbfile;

		require Slim::Utils::Log;
		Slim::Utils::Log::logBacktrace($msg);
		Slim::Utils::Log::logError($dbfile);

		if ( _is_corrupt_error($msg) ) {
			# we've already tried to recover - give up
			if ($rebuilt{$dbfile}++) {
				Slim::Utils::Log::logError("Please stop the server, remove $dbfile and restart.");
			}
			else {
				Slim::Utils::Log::logError("Quarantining corrupt $dbfile and rebuilding from scratch.");
				my $err_dbh = $handle ? eval { $handle->isa('DBI::st') ? $handle->{Database} : $handle } : undef;
				eval { $handle->finish } if $handle && eval { $handle->isa('DBI::st') };
				$self->close();
				if ( $err_dbh ) {
					eval { $err_dbh->{HandleError} = undef };
					eval { $err_dbh->disconnect } if eval { $err_dbh->{Active} };
				}
				undef $handle;
				eval { $self->_recover_corrupt_db($dbfile, $msg) };
				if ( $@ ) {
					Slim::Utils::Log::logError("Unable to quarantine corrupt $dbfile: $@");
				}
			}
		}

		# Return 0 so DBI continues normal error handling and RaiseError fires
		return 0;
	};

	return $dbh;
}

sub _close_db {
	my $self = shift;

	if ( my $dbh = $self->{dbh} ) {
		for my $sth_key (qw(set_sth get_sth delete_sth)) {
			eval { $self->{$sth_key}->finish } if $self->{$sth_key};
		}
		eval { $dbh->{HandleError} = undef };
		eval { $dbh->disconnect };

		delete $self->{$_} for qw(set_sth get_sth delete_sth dbh);
	}
}

1;
