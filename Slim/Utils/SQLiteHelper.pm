package Slim::Utils::SQLiteHelper;

# Logitech Media Server Copyright 2003-2024 Logitech.
# Lyrion Music Server Copyright 2024-2026 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

=head1 NAME

Slim::Utils::SQLiteHelper

=head1 SYNOPSIS

Slim::Utils::SQLiteHelper->init

=head1 DESCRIPTION

=head1 METHODS

=cut

use strict;

use Digest::MD5 qw(md5_hex);
use Fcntl qw(:flock O_CREAT O_RDWR);
use File::Basename;
use File::Path;
use File::Slurp;
use File::Spec::Functions qw(catfile);
use JSON::XS qw(encode_json);
use Time::HiRes qw(sleep);

use Slim::Utils::ArtworkCache;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::OSDetect;
use Slim::Utils::Prefs;
use Slim::Utils::SQLHelper;
use Slim::Utils::Progress;
use Slim::Utils::Strings ();

my $log = logger('database.info');

my $prefs = preferences('server');

$prefs->setChange( \&setCacheSize, 'dbhighmem' ) unless defined &main::SCANNER && main::SCANNER();

sub storageClass { 'DBIx::Class::Storage::DBI::SQLite' };

sub default_dbsource { 'dbi:SQLite:dbname=%s' }

# Remember if the main server is running or not, to avoid LWP timeout delays
my $serverDown = 0;
use constant MAX_RETRIES                => 5;
use constant WAL_AUTOCHECKPOINT         => 1000;
use constant DEFAULT_MMAP_SIZE          => 268435456;
use constant DEFAULT_PERSIST_MMAP_SIZE  => 67108864;
use constant DEFAULT_JOURNAL_SIZE_LIMIT => 16777216;
use constant DEFAULT_TEMP_STORE         => 'MEMORY';

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

# Scanning flag is set during scanning
my $SCANNING = 0;
my $scannerLockFh;

END {
	__PACKAGE__->_releaseScannerLock() if defined &main::SCANNER && main::SCANNER();
}

sub init {
	my ( $class, $dbh ) = @_;

	# Make sure we're running the right version of DBD::SQLite
	if ( $DBD::SQLite::VERSION lt 1.34 ) {
		die "DBD::SQLite version 1.34 or higher required\n";
	}

	# Reset dbsource pref if it's not for SQLite
	#                                              ... or if it's using the long filename Windows doesn't like
	my $dbsource = $prefs->get('dbsource') || '';
	my $validDbsource = ($dbsource =~ /library\.db/) && eval { local $SIG{__DIE__}; _validateDSN($dbsource); 1 };
	if ( !$validDbsource ) {
		$prefs->set( dbsource => default_dbsource() );
		$prefs->set( dbsource => $class->source() );
	}

	if ( !(defined &main::SCANNER && main::SCANNER()) ) {
		# Event handler for notifications from scanner process
		Slim::Control::Request::addDispatch(
			['scanner', 'notify', '_msg'],
			[0, 0, 0, \&_notifyFromScanner]
		);
	}
}

sub _validateDBPath {
	my ($path) = @_;

	die "Invalid SQLite database path: undefined\n" unless defined $path && length $path;
	die "Invalid SQLite database path: disallowed characters\n" if $path =~ /[;\0\r\n?%]/;
	die "Invalid SQLite database path: dot-traversal disallowed\n" if $path =~ /(?:^|[\/\\=:])\.\.(?:[\/\\]|$)/;

	return $path;
}

sub _validateDSN {
	my ($dsn) = @_;

	die "Invalid SQLite DSN: undefined\n" unless defined $dsn && length $dsn;
	die "Invalid SQLite DSN: disallowed characters\n" if $dsn =~ /[;\0\r\n?%]/;
	die "Invalid SQLite DSN: URI parameters disallowed\n" if $dsn =~ /uri=/i;
	die "Invalid SQLite DSN: dot-traversal disallowed\n" if $dsn =~ /(?:^|[\/\\=:])\.\.(?:[\/\\]|$)/;
	die "Invalid SQLite DSN: must start with dbi:SQLite:dbname=\n"
		unless $dsn =~ /^dbi:SQLite:dbname=(.+)$/;

	_validateDBPath($1);

	return $dsn;
}

sub _validatePragma {
	my ($pragma) = @_;

	die "Invalid SQLite PRAGMA: undefined\n" unless defined $pragma;
	$pragma =~ s/^\s+|\s+$//g;

	die "Invalid SQLite PRAGMA: disallowed characters\n"
		if !length($pragma) || $pragma =~ /[;\0'"\r\n]/ || $pragma =~ /--|\/\*/;

	if ( $pragma =~ /^(?:([a-zA-Z_][a-zA-Z0-9_]*)\.)?([a-zA-Z_][a-zA-Z0-9_]*)(?:\s*=\s*([a-zA-Z0-9_-]+)|\s*\(\s*([a-zA-Z0-9_-]+)\s*\))?$/ ) {
		my ($schema, $name, $eq_val, $arg_val) = ($1, $2, $3, $4);
		die "Disallowed SQLite PRAGMA schema: $schema\n"
			if defined $schema && $schema !~ /^(?:main|temp|persistentdb)$/i;
		die "Disallowed SQLite PRAGMA name: $name\n"
			unless $ALLOWED_PRAGMAS{lc($name)};

		my $validated = defined $schema ? "$schema.$name" : $name;
		$validated .= " = $eq_val" if defined $eq_val;
		$validated .= "($arg_val)" if defined $arg_val;

		return ($validated, lc($name));
	}

	die "Malformed SQLite PRAGMA: $pragma\n";
}

sub source {
	my $source;

	my $dbFile = _validateDBPath( catfile( $prefs->get('librarycachedir'), 'library.db' ) );

	# we need to migrate long 7.6.0 file names to shorter 7.6.1 filenames: Perl/Windows can't handle the long version
	_migrateDBFile(catfile( $prefs->get('librarycachedir'), 'squeezebox.db' ), $dbFile);

	$source = _validateDSN( sprintf( $prefs->get('dbsource'), $dbFile ) );

	return $source;
}

sub on_connect_do {
	my $class = shift;

	my $sql = [
		'PRAGMA busy_timeout = 5000',
		'PRAGMA synchronous = NORMAL',
		'PRAGMA journal_mode = WAL',
		'PRAGMA foreign_keys = ON',
		'PRAGMA wal_autocheckpoint = ' . WAL_AUTOCHECKPOINT,
		'PRAGMA mmap_size = ' . DEFAULT_MMAP_SIZE,
		'PRAGMA journal_size_limit = ' . DEFAULT_JOURNAL_SIZE_LIMIT,
		'PRAGMA temp_store = ' . DEFAULT_TEMP_STORE,
		# Default cache_size is 2000 pages, a page is normally 1K but may be different
		# depending on the OS/filesystem.  So default is usually 2MB.
		# Highmem we will try 20M (high) or 500M (max)
		'PRAGMA cache_size = ' . $class->_cacheSize,
	];

	return $sql;
}

sub setCacheSize {
	my $cache_size = __PACKAGE__->_cacheSize;

	return unless Slim::Schema->hasLibrary;

	my $dbh = Slim::Schema->dbh;
	$dbh->do("PRAGMA cache_size = $cache_size");
	$dbh->do("PRAGMA persistentdb.cache_size = $cache_size");
}

sub _cacheSize {
	my $high = $prefs->get('dbhighmem');

	return -8192 if !$high;

	# scanner doesn't take advantage of a huge buffer
	return -32768 if (defined &main::SCANNER && main::SCANNER()) || $high == 1;

	# maximum memory usage for large collections and lots of memory
	return -262144;
}

sub _migrateDBFile {
	my ($src, $dst) = @_;

	return if -f $dst || !-r $src;

	require File::Copy;

	defined &main::DEBUGLOG && main::DEBUGLOG() && $log->is_debug && $log->debug("trying to rename $src to $dst");

	if ( !File::Copy::move( $src, $dst ) ) {
		$log->error("Unable to rename $src to $dst: $!. Please remove it manually.");
	}
}

my $hasICU;
my $currentICU = '';
my $loadedICU = {};
sub collate {
	# Use ICU if built into DBD::SQLite
	if ( !defined $hasICU ) {
		$hasICU = (DBD::SQLite->can('compile_options') && grep /ENABLE_ICU/, DBD::SQLite::compile_options());
	}

	if ($hasICU) {
		my $lang = Slim::Utils::Strings::getLanguage();

		my $collation = Slim::Utils::Strings::getLocales()->{$lang};

		# our ICU data file has no nynorsk collation, but sorts the same as bokmål
		$collation = 'nb_NO' if $collation && $collation eq 'nn_NO';

		if ( $collation && $currentICU ne $collation ) {
			if ( !$loadedICU->{$collation} ) {
				if ( !Slim::Schema->hasLibrary() ) {
					# XXX for i.e. ContributorTracks many_to_many
					return "COLLATE $collation ";
				}

				# Point to our custom small ICU collation data file
				$ENV{ICU_DATA} = Slim::Utils::OSDetect::dirsFor('strings');

				my $dbh = Slim::Schema->dbh;

				my $qcoll = $dbh->quote($collation);
				my $qpath = $dbh->quote($ENV{ICU_DATA});

				# Win32 doesn't always like to read the ICU_DATA env var, so pass it here too
				my $sql = (defined &main::ISWINDOWS && main::ISWINDOWS())
					? "SELECT icu_load_collation($qcoll, $qcoll, $qpath)"
					: "SELECT icu_load_collation($qcoll, $qcoll)";

				eval { $dbh->do($sql) };
				if ( $@ ) {
					$log->error("SQLite ICU collation $collation failed: $@");
					$hasICU = 0;
					return 'COLLATE perllocale ';
				}

				defined &main::DEBUGLOG && main::DEBUGLOG() && $log->is_debug && $log->debug("Loaded ICU collation for $collation");

				$loadedICU->{$collation} = 1;
			}

			$currentICU = $collation;
		}

		return "COLLATE $currentICU " if $currentICU;
	}

	# Fallback to built-in perllocale collation to sort using Unicode Collation Algorithm
	# on systems with a properly installed locale.
	return 'COLLATE perllocale ';
}

=head2 randomFunction()

Returns RANDOM(), SQLite-specific random function

=cut

sub randomFunction { 'RANDOM()' }

=head2 prepend0( $string )

Returns SQLite-specific syntax '0 || $string'

=cut

sub prepend0 { '0 || ' . $_[1] }

=head2 append0( $string )

Returns SQLite-specific syntax '$string || 0'

=cut

sub append0 {  $_[1] . ' || 0' }

=head2 concatFunction()

Returns ' || ', SQLite's concat operator.

=cut

sub concatFunction { ' || ' }

=head2 sqlVersion( $dbh )

=cut

sub sqlVersion { 'SQLite' }

=head2 sqlVersionLong( $dbh )

Returns the long version string, i.e. 5.0.22-standard

=cut

sub sqlVersionLong {
	my $class = shift;
	my $dbh   = shift || return 0;

	return 'DBD::SQLite ' . $DBD::SQLite::VERSION . ' (sqlite ' . $dbh->{sqlite_version} . ')';
}

=head2 canCacheDBHandle( )

Is it permitted to cache the DB handle for the period that the DB is open?

=cut

sub canCacheDBHandle {
	return 1;
}

=head2 checkDataSource()

Called to check the database.

=cut

sub checkDataSource {
	# No longer needed with WAL mode
}

=head2 beforeScan()

Called before a scan is started.

=cut

sub _scannerLockFile {
	my $dir = $prefs->get('librarycachedir') || Slim::Utils::OSDetect::dirsFor('cache');
	return catfile($dir, 'scanner.lock');
}

sub _tryAcquireLock {
	my ($class, $lockFile, $maxAttempts, $delay) = @_;

	$maxAttempts ||= 1;
	$delay       ||= 0.05;

	my $dir = dirname($lockFile);
	eval { mkpath($dir) } if $dir && !-d $dir;

	my $fh;
	if ( !sysopen($fh, $lockFile, O_CREAT | O_RDWR) ) {
		$log->warn("Unable to open lock file $lockFile: $!");
		return undef;
	}

	for my $attempt ( 1 .. $maxAttempts ) {
		if ( flock($fh, LOCK_EX | LOCK_NB) ) {
			return $fh;
		}
		sleep($delay) if $attempt < $maxAttempts;
	}

	close($fh);
	return undef;
}

sub _acquireScannerLock {
	my $class = shift;

	return $scannerLockFh if $scannerLockFh;

	$scannerLockFh = $class->_tryAcquireLock($class->_scannerLockFile(), 20, 0.05);
	return $scannerLockFh;
}

sub _releaseScannerLock {
	my $class = shift;

	if ( $scannerLockFh ) {
		flock($scannerLockFh, LOCK_UN);
		close($scannerLockFh);
		undef $scannerLockFh;
	}
}

sub _checkpointWAL {
	my ($class, $dbh, $mode, $maxRetries) = @_;

	return 0 unless $dbh;

	$mode = uc($mode || 'TRUNCATE');
	$mode = 'TRUNCATE' unless $mode =~ /^(?:PASSIVE|FULL|RESTART|TRUNCATE)$/;
	$maxRetries ||= 5;

	my @schemas = ('main');
	my $dbs = eval {
		local $dbh->{HandleError} = undef;
		local $dbh->{RaiseError}  = 1;
		local $dbh->{PrintError}  = 0;
		$dbh->selectall_arrayref('PRAGMA database_list');
	};
	if ( $dbs ) {
		for my $row ( @$dbs ) {
			push @schemas, $row->[1]
				if defined $row->[1] && $row->[1] eq 'persistentdb';
		}
	}

	my $allOk = 1;
	for my $schema ( @schemas ) {
		my $busy = 1;
		for my $attempt ( 1 .. $maxRetries ) {
			my ($b) = eval {
				local $dbh->{HandleError} = undef;
				local $dbh->{RaiseError}  = 1;
				local $dbh->{PrintError}  = 0;
				$dbh->selectrow_array("PRAGMA $schema.wal_checkpoint($mode)");
			};
			if ( $@ ) {
				$log->warn("WAL checkpoint ($mode) failed on $schema: $@");
				$allOk = 0;
				last;
			}
			$busy = defined $b ? $b : 0;
			last unless $busy;
			sleep(0.1) if $attempt < $maxRetries;
		}
		if ( $busy ) {
			$log->warn("WAL checkpoint ($mode) on $schema remained busy after $maxRetries attempts");
			$allOk = 0;
		}
	}

	return $allOk;
}

sub _isLockError {
	my ($err) = @_;
	return defined $err && $err =~ /database is locked|database table is locked|SQLITE_BUSY|SQLITE_LOCKED/i;
}

sub _isCorruptError {
	my ($err) = @_;
	return 0 unless defined $err;
	return 0 if _isLockError($err);
	return $err =~ /(?:database disk image is malformed|file is not a database|is not a database|malformed database schema)/i ? 1 : 0;
}

sub _quarantineCorruptDB {
	my ($class, $dbFile) = @_;

	my $ts     = time();
	my $suffix = sprintf('.corrupt.%d.%d', $ts, $$);
	my $n      = 0;
	while ( -e "$dbFile$suffix" || lstat("$dbFile$suffix") ) {
		$n++;
		$suffix = sprintf('.corrupt.%d.%d.%d', $ts, $$, $n);
	}

	my $failed = 0;
	for my $file ( $dbFile, "$dbFile-wal", "$dbFile-shm" ) {
		if ( lstat($file) || -e $file ) {
			my $target = "$file$suffix";
			if ( !rename($file, $target) && (lstat($file) || -e $file) ) {
				$log->error("Unable to quarantine corrupt SQLite file $file to $target: $!");
				$failed = 1;
			}
		}
	}

	return !$failed;
}

sub beforeScan {
	my $class = shift;

	if ( defined &main::SCANNER && main::SCANNER() ) {
		my $lock_fh = $class->_acquireScannerLock();
		die "Another scan is already holding scanner.lock\n" unless $lock_fh;
		return 1;
	}

	return 1;
}

=head2 afterScan()

Called after a scan is finished. Notifies main server to copy back the scanner file.

=cut

sub afterScan {
	my $class = shift;

	if ( defined &main::SCANNER && main::SCANNER() && Slim::Schema->hasLibrary ) {
		eval { $class->_checkpointWAL(Slim::Schema->dbh, 'RESTART', 3) };
	}

	$class->updateProgress('end');
}

=head2 optimizeDB()

Called during the Slim::Schema->optimizeDB call to run some DB specific cleanup tasks

=cut

sub optimizeDB {
	my $class = shift;

	# only run VACUUM in the scanner, or if no player is active and no scan is active
	return if !(defined &main::SCANNER && main::SCANNER()) && ( $SCANNING || grep { $_->power() } Slim::Player::Client::clients() );

	$class->vacuum();
	$class->vacuum('persist.db', undef, 1);
}

=head2 exitScan()

Called as the scanner process exits. Used by main process to detect scanner crashes.

=cut

sub exitScan {
	my $class = shift;

	if ( defined &main::SCANNER && main::SCANNER() ) {
		if ( Slim::Schema->hasLibrary ) {
			eval { $class->_checkpointWAL(Slim::Schema->dbh, 'TRUNCATE', 3) };
		}
		$class->_releaseScannerLock();
	}

	$class->updateProgress('exit');
}

=head2 postConnect()

Called immediately after connect.  Sets up MD5() function.

=cut
my %postConnectHandlers;
my %recoveredPersistDB;

sub postConnect {
	my ( $class, $dbh ) = @_;

	$dbh->func( 'MD5', 1, sub { md5_hex( $_[0] ) }, 'create_function' );

	# http://search.cpan.org/~adamk/DBD-SQLite-1.33/lib/DBD/SQLite.pm#Transaction_and_Database_Locking
	# Enable pre-write so any DDL/write transactions acquire RESERVED locks immediately.
	$dbh->{sqlite_use_immediate_transaction} = 1;

	# We create this even if main::STATISTICS is not false so that the SQL always works
	# Track Persistent data is in another file, in the prefs folder rather than cache
	my $persistentdb = _validateDBPath( $class->dbFile('persist.db', 'persistent') );

	# we need to move the persist.db out of the cache folder
	_migrateDBFile($class->dbFile('persist.db'), $persistentdb);

	# we need to migrate long 7.6.0 file names to shorter 7.6.1 filenames: Windows can't handle the long version
	_migrateDBFile($class->dbFile('squeezebox-persistent.db'), $persistentdb);

	my $detachPersistentDB = sub {
		my $dbs = eval {
			local $dbh->{HandleError} = undef;
			local $dbh->{RaiseError}  = 1;
			local $dbh->{PrintError}  = 0;
			$dbh->selectall_arrayref('PRAGMA database_list');
		};
		if ( $dbs && grep { defined $_->[1] && $_->[1] eq 'persistentdb' } @$dbs ) {
			eval {
				local $dbh->{HandleError} = undef;
				local $dbh->{RaiseError}  = 1;
				local $dbh->{PrintError}  = 0;
				$dbh->do('DETACH DATABASE persistentdb');
			};
		}
	};

	my $attachPersistentDB = sub {
		local $dbh->{HandleError} = undef;
		local $dbh->{RaiseError}  = 1;
		local $dbh->{PrintError}  = 0;

		my $dbs = $dbh->selectall_arrayref('PRAGMA database_list');
		if ( !$dbs || !grep { defined $_->[1] && $_->[1] eq 'persistentdb' } @$dbs ) {
			$dbh->do('ATTACH ' . $dbh->quote($persistentdb) . ' AS persistentdb');
		}
		$dbh->do('PRAGMA persistentdb.journal_mode = WAL');
		$dbh->do('PRAGMA persistentdb.synchronous = NORMAL');
		$dbh->do('PRAGMA persistentdb.journal_size_limit = ' . DEFAULT_JOURNAL_SIZE_LIMIT);
		$dbh->do('PRAGMA persistentdb.mmap_size = ' . DEFAULT_PERSIST_MMAP_SIZE);
		$dbh->do('PRAGMA persistentdb.cache_size = ' . $class->_cacheSize);

		my ($qc) = $dbh->selectrow_array('PRAGMA persistentdb.quick_check(1)');
		die 'SQLite persistentdb database disk image is malformed (' . (defined $qc ? $qc : 'undef') . ')'
			if !defined $qc || lc($qc) ne 'ok';

		my ($hasTable) = $dbh->selectrow_array(
			"SELECT name FROM persistentdb.sqlite_master WHERE type='table' AND name='tracks_persistent'"
		);

		if ( !$hasTable ) {
			my $started_tx = $dbh->{AutoCommit} ? eval { $dbh->begin_work; 1 } : 0;
			eval {
				$dbh->do(q{
					CREATE TABLE IF NOT EXISTS persistentdb.tracks_persistent (
						id INTEGER PRIMARY KEY AUTOINCREMENT,
						url text NOT NULL,
						musicbrainz_id varchar(40),
						added int(10),
						rating tinyint(1),
						playCount int(10),
						lastPlayed int(10),
						urlmd5 char(32) NOT NULL default '0'
					)
				});

				for my $ddl (
					'CREATE INDEX IF NOT EXISTS persistentdb.trackMusicBrainzIndex ON tracks_persistent (musicbrainz_id)',
					'CREATE INDEX IF NOT EXISTS persistentdb.trackUrlIndex ON tracks_persistent (url)',
					'CREATE INDEX IF NOT EXISTS persistentdb.trackAddedIndex ON tracks_persistent (added)',
					'CREATE INDEX IF NOT EXISTS persistentdb.trackRatingIndex ON tracks_persistent (rating)',
					'CREATE INDEX IF NOT EXISTS persistentdb.trackPlayCountIndex ON tracks_persistent (playCount)',
					'CREATE INDEX IF NOT EXISTS persistentdb.trackLastPlayedIndex ON tracks_persistent (lastPlayed)',
					'CREATE INDEX IF NOT EXISTS persistentdb.urlmd5Index ON tracks_persistent (urlmd5)',
				) {
					$dbh->do($ddl);
				}

				$dbh->commit if $started_tx;
			};
			if ( my $ddl_err = $@ ) {
				eval { $dbh->rollback } if $started_tx;
				die $ddl_err;
			}
		}
	};

	eval { $attachPersistentDB->() };

	if ( my $err = $@ ) {
		if ( _isCorruptError($err) && !$recoveredPersistDB{$persistentdb}++ ) {
			$log->error("Corrupt persistent database detected ($persistentdb): $err\nQuarantining and rebuilding $persistentdb.");

			my $lockFh = $class->_tryAcquireLock("$persistentdb.lock", 50, 0.1);
			if ( !$lockFh ) {
				die "Unable to acquire exclusive recovery lock for $persistentdb: $err";
			}

			eval {
				$detachPersistentDB->();

				# Re-check under exclusive lock in case another process already recovered it
				eval { $attachPersistentDB->() };
				if ( my $recheck_err = $@ ) {
					die $recheck_err unless _isCorruptError($recheck_err);

					$detachPersistentDB->();
					$class->_quarantineCorruptDB($persistentdb)
						or die "Unable to quarantine corrupt persistent database $persistentdb: $recheck_err";

					$attachPersistentDB->();
				}
			};
			my $rec_err = $@;

			flock($lockFh, LOCK_UN);
			close($lockFh);

			die $rec_err if $rec_err;
			delete $recoveredPersistDB{$persistentdb};
		}
		else {
			die $err;
		}
	}

	# Reset collation load state
	$currentICU = '';
	$loadedICU = {};

	# Check if the DB has been optimized (stats analysis)
	if ( !(defined &main::SCANNER && main::SCANNER()) ) {
		# Check for the presence of the sqlite_stat1 table
		my ($count) = eval { $dbh->selectrow_array( "SELECT COUNT(*) FROM sqlite_stat1 WHERE tbl = 'tracks'", undef, () ) };

		if (!$count) {
			my ($table) = eval { $dbh->selectrow_array("SELECT name FROM sqlite_master WHERE type='table' AND name='tracks'") };

			if ($table) {
				defined &main::INFOLOG && main::INFOLOG() && $log->is_info && $log->info('Optimizing DB because of missing or empty sqlite_stat1 table');
				Slim::Schema->optimizeDB();
			}
		}
	}

	foreach (keys %postConnectHandlers) {
		$_->postDBConnect($dbh);
	}
}

=head2

Allow plugins and others to register handlers which should be called from postConnect

=cut

sub addPostConnectHandler {
	my ( $class, $handler ) = @_;

	if ($handler && $handler->can('postDBConnect')) {
		$postConnectHandlers{$handler}++
	}

	# if we register for the first time, re-initialize the dbh object
	if ( $postConnectHandlers{$handler} == 1 ) {
		Slim::Schema->disconnect;
		Slim::Schema->init;
	}
}

sub updateProgress {
	my $class = shift;

	return if $serverDown > MAX_RETRIES;

	require LWP::UserAgent;
	require HTTP::Request;

	my $log = logger('scan.scanner');

	# Scanner does not have an event loop, so use sync HTTP here.
	# Don't use Slim::Utils::Network, as it comes with too much overhead.
	my $host = ( $prefs->get('httpaddr') || '127.0.0.1' ) . ':' . $prefs->get('httpport');

	my $ua = LWP::UserAgent->new(
		timeout => 5,
	);

	my $req = HTTP::Request->new( POST => "http://${host}/jsonrpc.js" );

	$req->header( 'X-Scanner' => 1 );

	# Handle security if necessary
	if ( my $username = $prefs->get('username') ) {
		my $password = $prefs->get('password');
		$req->authorization_basic($username, $password);
	}

	$req->content( encode_json( {
		id     => 1,
		method => 'slim.request',
		params => [ '', [ 'scanner', 'notify', @_ ] ],
	} ) );

	defined &main::INFOLOG && main::INFOLOG() && $log->is_info
		&& $log->info( 'Notify to server: ' . Data::Dump::dump(\@_) );

	my $res = $ua->request($req);

	if ( $res->is_success ) {
		if ( $res->content =~ /abort/ ) {
			logWarning('Server aborted scan, shutting down');
			Slim::Utils::Progress->clear;

			# let the user know we aborted the scan
			my $progress = Slim::Utils::Progress->new( {
				type  => 'importer',
				name  => 'failure',
				total => 1,
				every => 1,
			} );
			$progress->update('SCAN_ABORTED');

			Slim::Music::Import->setIsScanning(0);
			$class->_releaseScannerLock();

			exit;
		}
		else {
			defined &main::INFOLOG && main::INFOLOG() && $log->is_info && $log->info('Notify to server OK');
		}
		$serverDown = 0;
	}
	else {
		defined &main::INFOLOG && main::INFOLOG() && $log->is_info && $log->info( 'Notify to server failed: ' . $res->status_line );

		if ( $res->content =~ /timeout|refused/ ) {
			# Server is down, avoid further requests
			$serverDown++;
		}
	}
}

=head2 cleanup()

Shut down when Lyrion Music Server is shut down.

=cut

sub cleanup {
	my $class = shift;
	$class->_releaseScannerLock();
}

=head2 pragma()

Run a given PRAGMA statement.

=cut

sub pragma {
	my ( $class, $pragma ) = @_;

	my ($validated, $pragmaName) = _validatePragma($pragma);

	my $dbh = Slim::Schema->dbh;
	$dbh->do("PRAGMA $validated");

	if ( $pragmaName eq 'locking_mode' ) {
		# if changing the locking_mode we need to run a statement on each database to change the lock
		$dbh->do('SELECT 1 FROM metainformation LIMIT 1');
		$dbh->do('SELECT 1 FROM tracks_persistent LIMIT 1');
	}

	# Pass the pragma to the ArtworkCache database (unless scoped to persistentdb)
	Slim::Utils::ArtworkCache->new->pragma($validated)
		unless $validated =~ /^persistentdb\./i;
}

=head2 vacuum()

VACUUM a given database.

If the $optional parameter is passed, the VACUUM will only happen if there's a certain fragmentation.

=cut

sub vacuum {
	my ( $class, $db, $optional, $persistent ) = @_;

	my $dbName = $db || 'library.db';
	my $dbFile = _validateDBPath( $class->dbFile($dbName, $persistent) );

	# files other than the default library.db can be stored in a persistent folder
	$dbFile = _validateDBPath( $class->dbFile($db, 'persistent') ) if $db && !-f $dbFile;

	return unless -f $dbFile;
	my $dbSize = -s _;

	my $lockFh;
	if ( !(defined &main::SCANNER && main::SCANNER()) ) {
		if ( $SCANNING ) {
			$log->warn("Skipping VACUUM for $dbName while scanner is active");
			return;
		}
		$lockFh = $class->_tryAcquireLock($class->_scannerLockFile(), 5, 0.1);
		if ( !$lockFh ) {
			$log->warn("Skipping VACUUM for $dbName: scanner lock is currently held");
			return;
		}
	}

	defined &main::DEBUGLOG && main::DEBUGLOG() && $log->is_debug && $log->debug("Start VACUUM $dbName");

	my $source = _validateDSN( sprintf( $class->default_dbsource(), $dbFile ) );

	# this can't be run from the schema_cleanup.sql, as VACUUM doesn't work inside a transaction
	my $dbh;
	eval {
		$dbh = DBI->connect($source, '', '', {
			RaiseError                       => 1,
			PrintError                       => 0,
			AutoCommit                       => 1,
			sqlite_use_immediate_transaction => 1,
		}) or die $DBI::errstr || "Failed to connect to $dbFile";

		$dbh->do('PRAGMA busy_timeout = 10000');

		if ($optional) {
			my $pages = $dbh->selectrow_array('PRAGMA page_count;');
			my $free  = $dbh->selectrow_array('PRAGMA freelist_count;');
			my $frag  = ($pages && $free) ? $free / $pages : 0;

			defined &main::DEBUGLOG && main::DEBUGLOG() && $log->is_debug && $log->debug("$dbFile: Pages: $pages; Free: $free; Fragmentation: $frag");

			# don't skip this vacuum if fragmentation is higher than 10%;
			$optional = 0 if $frag > 0.1;
		}

		if ( !$optional ) {
			# Flush and truncate WAL frames before VACUUM
			$class->_checkpointWAL($dbh, 'TRUNCATE', 5);

			# use memory as temporary storage for the vaccum if dbhighmem is enabled and db file is smaller than 1GB
			if ( Slim::Utils::OSDetect::getOS->canVacuumInMemory($dbSize) ) {
				defined &main::INFOLOG && main::INFOLOG() && $log->is_info && $log->info("Using memory (RAM) to store temporary table in VACUUM command: $dbName");
				$dbh->do('PRAGMA temp_store = ' . DEFAULT_TEMP_STORE);
			}
			$dbh->do('PRAGMA synchronous = NORMAL');
			$dbh->do('VACUUM');
		}
	};
	my $err = $@;
	eval { $dbh->disconnect } if $dbh;

	if ( $lockFh ) {
		flock($lockFh, LOCK_UN);
		close($lockFh);
	}

	if ($err) {
		$log->error("VACUUM failed for $dbName: $err");
		return;
	}

	defined &main::DEBUGLOG && main::DEBUGLOG() && $log->is_debug && $log->debug("VACUUM $dbName done!");
}

sub dbFile {
	my ( $class, $name, $persistent ) = @_;

	if ( defined $name ) {
		die "Invalid SQLite database name: disallowed characters\n"
			if $name =~ /[;\/\\\0]/ || $name eq '.' || $name eq '..' || $name =~ /\.\./;
	}

	if ($persistent) {
		my $persistDir = Slim::Utils::Prefs::dir() || Slim::Utils::OSDetect::dirsFor('prefs');
		return _validateDBPath( catfile($persistDir, $name) );
	}

	my ($driver, $source, $username, $password) = Slim::Schema->sourceInformation;

	my ($dbname) = $source =~ /dbname=([^;]+)/;
	_validateDBPath($dbname);

	return $dbname unless $name;

	my $dbbase = basename($dbname);
	$dbname =~ s/\Q$dbbase\E$/$name/;

	return _validateDBPath($dbname);
}

sub _finishScannerExit {
	my ($class, $attempt) = @_;

	$attempt ||= 0;

	my $lockFh = $class->_tryAcquireLock($class->_scannerLockFile(), 4, 0.05);
	if ( !$lockFh ) {
		if ( $attempt < 10 ) {
			require Slim::Utils::Timers;
			Slim::Utils::Timers::setTimer(
				$class,
				Time::HiRes::time() + 0.5,
				sub { $class->_finishScannerExit($attempt + 1) },
			);
			return;
		}
		$log->warn('Scanner exit cleanup timed out waiting for scanner lock; clearing scanning state without forced disconnect.');
		Slim::Music::Import->setIsScanning(0);
		Slim::Control::Request::notifyFromArray( undef, [ 'rescan', 'done' ] );
		return;
	}

	eval {
		# Checkpoint WAL while holding the exclusive handshake lock before reconnecting
		if ( Slim::Schema->hasLibrary ) {
			$class->_checkpointWAL(Slim::Schema->dbh, 'TRUNCATE', 5);
		}

		# Reconnect to the database to zero out WAL files
		Slim::Schema->disconnect;
		Slim::Schema->init;

		# Close ArtworkCache to zero out WAL file, it'll be reopened when needed
		Slim::Utils::ArtworkCache->new->close;
	};
	my $err = $@;

	flock($lockFh, LOCK_UN);
	close($lockFh);

	$log->error("Error during post-scan database reconnect: $err") if $err;

	Slim::Music::Import->setIsScanning(0);

	# Clear caches, like the vaObj, etc after scanning has been finished.
	Slim::Control::Request::notifyFromArray( undef, [ 'rescan', 'done' ] );
}

sub _notifyFromScanner {
	my $request = shift;

	my $class = __PACKAGE__;

	my $msg = $request->getParam('_msg');

	my $log = logger('scan.scanner');

	defined &main::INFOLOG && main::INFOLOG() && $log->is_info && $log->info("Notify from scanner: $msg");

	# If user aborted the scan, return an abort message
	if ( Slim::Music::Import->hasAborted ) {
		$request->addResult( abort => 1 );
		$request->setStatusDone();

		Slim::Music::Import->setAborted(0);
		return;
	}

	if ( $msg eq 'start' ) {
		# Scanner has started
		$SCANNING = 1;

		if ( Slim::Utils::OSDetect::getOS->canAutoRescan && $prefs->get('autorescan') ) {
			require Slim::Utils::AutoRescan;
			Slim::Utils::AutoRescan->shutdown;
		}

		Slim::Music::Import->setIsScanning('SETUP_WIPEDB');

		# XXX if scanner doesn't report in with regular progress within a set time period
		# assume scanner is dead.  This is hard to do, as scanner may block for an indefinite
		# amount of time with slow network filesystems, or a large amount of files.
	}
	elsif ( $msg eq 'end' ) {
		if ( $SCANNING ) {
			# Scanner has finished.
			$SCANNING = 0;
		}
	}
	elsif ( $msg eq 'exit' ) {
		my $aborted = $SCANNING;
		$SCANNING = 0;

		if ( $aborted ) {
			$log->warn('Scanner exited without end notification (aborted); executing post-scan WAL cleanup and reconnect.');
		}

		$request->setStatusDone();
		$class->_finishScannerExit(0);
		return;
	}

	$request->setStatusDone();
}

1;
