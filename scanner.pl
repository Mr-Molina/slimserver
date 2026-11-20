#!/usr/bin/perl

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2025 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#

require 5.010;
use strict;

my $scanner_exit_code;
END {
	$? = $scanner_exit_code if defined $scanner_exit_code;
}

use FindBin qw($Bin);
use lib $Bin;
use Config;

use constant SLIM_SERVICE => 0;
use constant SCANNER      => 1;
use constant RESIZER      => 0;
use constant TRANSCODING  => 0;
use constant PERFMON      => 0;
use constant ISWINDOWS    => ( $^O =~ /^m?s?win/i ) ? 1 : 0;
use constant ISACTIVEPERL => 0;
use constant ISMAC        => ( $^O =~ /darwin/i ) ? 1 : 0;
use constant DEBUGLOG     => ( grep { /--nodebuglog/ } @ARGV ) ? 0 : 1;
use constant INFOLOG      => ( grep { /--noinfolog/ } @ARGV ) ? 0 : 1;
use constant STATISTICS   => ( grep { /--nostatistics/ } @ARGV ) ? 0 : 1;
use constant SB1SLIMP3SYNC=> 0;
use constant WEBUI        => 0;
use constant HAS_AIO      => 0;
use constant LOCALFILE    => 0;
use constant NOMYSB       => 1;

our $REVISION    = undef;
our $BUILDDATE   = undef;

BEGIN {
	# hack a Strawberry Perl specific path into the environment variable - XML::Parser::Expat needs it!
	if (ISWINDOWS) {
		my $path = File::Basename::dirname($^X);
		$path =~ s/perl(?=.bin)/c/i;
		$ENV{PATH} = "$path;" . $ENV{PATH} if -d $path;
	}

	our $VERSION = '9.2.0';
	use Slim::bootstrap;
	use Slim::Utils::OSDetect;

	Slim::bootstrap->loadModules([qw(version Time::HiRes DBI HTML::Parser XML::Parser::Expat YAML::XS)], []);

	# By default, tell Audio::Scan not to get artwork to save memory
	# Where needed, this is locally changed to 0.
	$ENV{AUDIO_SCAN_NO_ARTWORK} = 1;
};

# Force XML::Simple to use XML::Parser for speed. This is done
# here so other packages don't have to worry about it. If we
# don't have XML::Parser installed, we fall back to PurePerl.
#
# Only use XML::Simple 2.15 an above, which has support for pass-by-ref
use XML::Simple qw(2.15);

eval {
	local($^W) = 0;      # Suppress warning from Expat.pm re File::Spec::load()
	require XML::Parser;
};

if (!$@) {
	$XML::Simple::PREFERRED_PARSER = 'XML::Parser';
}

use Errno ();
use Fcntl ();
use Getopt::Long;
use File::Path;
use File::Spec::Functions qw(:ALL);

use Slim::Utils::Log;
use Slim::Utils::Prefs;
use Slim::Music::Import;
use Slim::Music::Info;
use Slim::Music::PlaylistFolderScan;
use Slim::Music::ContributorPictureScan;
use Slim::Music::ReleaseTypes;
use Slim::Music::VirtualLibraries;
use Slim::Player::ProtocolHandlers;
use Slim::Utils::Misc;
use Slim::Utils::OSDetect;
use Slim::Utils::PluginManager;
use Slim::Utils::Progress;
use Slim::Utils::Strings qw(string);
use Slim::Media::MediaFolderScan;

if ( INFOLOG || DEBUGLOG ) {
	require Data::Dump;
	require Slim::Utils::PerlRunTime;
}

our $prefs;
our $progress;
our $pidfile;

my $cleanupDone;
my $scanCompleted     = 0;
my $scanAborted       = 0;
my $ownsScannerLock   = 0;

my %allowed_db_helpers = (
	SQLite => 'Slim::Utils::SQLiteHelper',
	MySQL  => 'Slim::Utils::MySQLHelper',
);

my $sqlHelperClass = Slim::Utils::OSDetect->getOS()->sqlHelperClass();
eval "use $sqlHelperClass";
die $@ if $@;

sub _handleAbortSignal {
	my ($sig) = @_;

	$SIG{TERM} = 'IGNORE';
	$SIG{INT}  = 'IGNORE';
	$scanAborted = 1;

	eval {
		if ($INC{'Slim/Utils/Log.pm'}) {
			logger('server')->warn("Scanner received SIG$sig, aborting and rolling back...");
		} else {
			warn "Scanner received SIG$sig, aborting...\n";
		}
	};

	cleanup();
	$scanner_exit_code = ($sig eq 'INT') ? 130 : 143;
	exit($scanner_exit_code);
}

sub main {

	our ($rescan, $playlists, $onlineLibrary, $wipe, $force, $prefsFile, $priority);
	our ($quiet, $dbtype, $logfile, $logdir, $logconf, $debug, $help, $cachedir);

	our $LogTimestamp = 1;

	my $changes = 0;

	$prefs = preferences('server');
	$cachedir = $prefs->get('cachedir');

	$prefs->readonly;

	my $prefsdir;
	Getopt::Long::Configure('no_pass_through');

	if ( !GetOptions(
		'force'        => \$force,
		# 'cleanup'      => sub {},
		'rescan'       => \$rescan,
		'wipe'         => \$wipe,
		'playlists'    => \$playlists,
		'onlinelibrary'=> \$onlineLibrary,
		# prefsdir parsed by Slim::Utils::Prefs
		'prefsdir=s'   => \$prefsdir,
		'prefsfile=s'  => \$prefsFile,
		'pidfile=s'    => \$pidfile,
		# these values are parsed separately, we don't need these values in a variable - just get them off the list
		'nodebuglog'   => sub {},
		'noinfolog'    => sub {},
		'nostatistics' => sub {},
		'progress'     => \$progress,
		'priority=i'   => \$priority,
		'logfile=s'    => \$logfile,
		'logdir=s'     => \$logdir,
		'logconfig=s'  => \$logconf,
		'debug=s'      => \$debug,
		'quiet'        => \$quiet,
		'dbtype=s'     => \$dbtype,
		'LogTimestamp!'=> \$LogTimestamp,
		'help'         => \$help,
	) ) {
		usage();
		$scanner_exit_code = 1;
		exit 1;
	}

	unshift @ARGV, ('--prefsdir', $prefsdir) if defined $prefsdir;

	if ( defined $dbtype && !exists $allowed_db_helpers{$dbtype} ) {
		warn "Invalid --dbtype '$dbtype'. Allowed values: " . join(', ', sort keys %allowed_db_helpers) . "\n";
		usage();
		$scanner_exit_code = 1;
		exit 1;
	}

	# If dbsource has been changed via settings, it overrides the default
	if ( !defined $dbtype && $prefs->get('dbtype') ) {
		$dbtype = $prefs->get('dbtype') =~ /SQLite/ ? 'SQLite' : 'MySQL';
	}

	if ( defined $dbtype ) {
		# For testing SQLite, can specify a different database type
		$sqlHelperClass = $allowed_db_helpers{$dbtype};
		if ( Slim::bootstrap::tryModuleLoad($sqlHelperClass) || $@ ) {
			die($@ || "Couldn't load $sqlHelperClass\n");
		}
	}

	if ($help || (!$rescan && !$wipe && !$playlists && !$onlineLibrary)) {
		usage();
		exit;
	}

	$SIG{HUP}  = 'IGNORE';
	$SIG{TERM} = \&_handleAbortSignal;
	$SIG{INT}  = \&_handleAbortSignal;

	# Start a fresh scanner.log on every scan
	if ( my $file = Slim::Utils::Log->scannerLogFile() ) {
		unlink $file if -e $file;
	}

	Slim::Utils::Log->init({
		'logconf' => $logconf,
		'logdir'  => $logdir,
		'logfile' => $logfile,
		'logtype' => 'scanner',
		'debug'   => $debug,
	});

	# Start/stop profiler during runtime (requires Devel::NYTProf)
	# and NYTPROF env var set to 'start=no'
	if ( $ENV{NYTPROF} && $INC{'Devel/NYTProf.pm'} && $ENV{NYTPROF} =~ /start=no/ ) {
		$SIG{USR1} = sub {
			DB::enable_profile();
			warn "Profiling enabled...\n";
		};

		$SIG{USR2} = sub {
			DB::disable_profile();
			warn "Profiling disabled...\n";
		};
	}


	# Redirect STDERR to the log file.
	if (!$progress) {
		tie *STDERR, 'Slim::Utils::Log::Trapper';
	}

	STDOUT->autoflush(1);

	my $log = logger('server');

	($REVISION, $BUILDDATE) = Slim::Utils::Misc::parseRevision();

	$log->error("Starting Lyrion Music Server scanner (v$main::VERSION, $REVISION, $BUILDDATE) perl $]");

	# Bring up strings, database, etc.
	initializeFrameworks($log);

	# Set priority, command line overrides pref
	if (defined $priority) {
		Slim::Utils::Misc::setPriority($priority);
	} else {
		Slim::Utils::Misc::setPriority( $prefs->get('scannerPriority') );
	}

	# Load appropriate DB module
	my $dbModule = $sqlHelperClass =~ /MySQL/ ? 'DBD::mysql' : 'DBD::SQLite';
	Slim::bootstrap::tryModuleLoad($dbModule);
	if ( $@ ) {
		logError("Couldn't load $dbModule [$@]");
		exit;
	}

	if ( $sqlHelperClass ) {
		main::INFOLOG && $log->info("Server SQL init...");
		$sqlHelperClass->init();
	}

	if (!$force && Slim::Music::Import->stillScanning) {

		msg("Import: There appears to be an existing scanner running.\n");
		msg("Import: If this is not the case, run with --force\n");
		msg("Exiting!\n");
		$scanner_exit_code = 1;
		exit 1;
	}

	if (!$force && defined $pidfile && !-l $pidfile && -f $pidfile) {
		if (open my $fh, '<', $pidfile) {
			my $existing_pid = <$fh>;
			close $fh;
			if (defined $existing_pid && $existing_pid =~ /^\s*(\d+)\s*$/) {
				my $pid = $1;
				if ($pid > 0 && $pid != $$ && (kill(0, $pid) || $!{EPERM})) {
					msg("Import: Existing scanner process (PID $pid) appears to be running.\n");
					msg("Import: If this is not the case, run with --force\n");
					msg("Exiting!\n");
					$scanner_exit_code = 1;
					exit 1;
				}
			}
		}
	}

	# pull in the memory usage module if requested.
	if (main::INFOLOG && logger('server.memory')->is_info) {
		if ( Slim::bootstrap::tryModuleLoad('Slim::Utils::MemoryUsage') ) {

			logError("Couldn't load Slim::Utils::MemoryUsage: [$@]");

		} else {

			Slim::Utils::MemoryUsage->init();
		}
	}

	main::INFOLOG && $log->info("Cache init...");
	Slim::Utils::Cache->init();

	Slim::Music::VirtualLibraries->init();

	if ($playlists) {

		Slim::Music::PlaylistFolderScan->init;
		Slim::Music::Import->scanPlaylistsOnly(1);

	} else {

		Slim::Music::Import->scanOnlineLibraryOnly($onlineLibrary);
		Slim::Media::MediaFolderScan->init;
		Slim::Music::PlaylistFolderScan->init;
		Slim::Music::ContributorPictureScan->init;
		Slim::Music::ReleaseTypes->init();

	}

	# Load any plugins that define import modules
	# useCache is 0 so scanner does not modify the plugin cache file
	Slim::Utils::PluginManager->init( 'import', 0 );

	main::INFOLOG && $log->info("Server Info init...");

	Slim::Music::Info::init();

	# need to re-init the strings, as plugins might have added new tokens
	Slim::Utils::Strings::init();

	Slim::Utils::PluginManager->load('import');

	checkDataSource();

	main::INFOLOG && $log->info("Scanner done init...\n");

	# Perform pre-scan steps specific to the database type, i.e. SQLite needs to copy to a new file
	$sqlHelperClass->beforeScan();

	$ownsScannerLock = 1;
	save_pid_file();

	# Take the db out of autocommit mode - this makes for a much faster scan.
	# Scanner::Local will commit every few operations
	Slim::Schema->dbh->{'AutoCommit'} = 0;

	my $scanType = 'SETUP_STANDARDRESCAN';

	if ($wipe) {
		$scanType = 'SETUP_WIPEDB';

	} elsif ($playlists) {
		$scanType = 'SETUP_PLAYLISTRESCAN';
	}

	# Flag the database as being scanned.
	Slim::Music::Import->setIsScanning($scanType);

	if ($wipe) {

		eval { Slim::Schema->wipeAllData; };

		if ($@) {
			logError("Failed when calling Slim::Schema->wipeAllData: [$@]");
			logError("This is a fatal error. Exiting");
			$scanAborted = 1;
			cleanup();
			$scanner_exit_code = 1;
			exit 1;
		}
	}

	# Don't wrap the below in a transaction - we want to have the server
	# periodically update the db. This is probably better than a giant
	# commit at the end, but is debatable.
	#
	# NB: Slim::Schema::throw_exception really isn't right now - it's just
	# printing an error and bt(). Once the server can handle & log
	# exceptions properly, it should croak(), so the exception is
	# propagated to the higher levels.
	#

	# Use our Importers to scan.
	eval {

		if ($wipe) {
			Slim::Music::Import->resetImporters;
		}

		$changes = Slim::Music::Import->runScan;
	};

	if ($@) {

		logError("Failed when running main scan: [$@]");
		logError("Skipping post-process & Not updating lastRescanTime!");
		$scanAborted = 1;

	} else {

		# Run mergeVariousArtists, artwork scan, etc.
		eval { Slim::Music::Import->runScanPostProcessing; };

		if ($@) {

			logError("Failed when running scan post-process: [$@]");
			logError("Not updating lastRescanTime!");
			$scanAborted = 1;

		} else {

			if ($changes) {
				Slim::Music::Import->setLastScanTime;
				Slim::Music::Import->setLastScanTimeIsDST();
			}

			# Notify server we are done scanning
			$sqlHelperClass->afterScan();

			$scanCompleted = 1 unless $scanAborted;
		}
	}

	# Cleanup after we're done, we can't rely on this being called from a sig handler
	cleanup();

	$scanner_exit_code = ($scanCompleted && !$scanAborted) ? 0 : 1;
	exit($scanner_exit_code);

	# To debug scanner memory usage, uncomment this line and kill -USR2 the scanner process
	# after it's finished scanning.
	# while (1) { sleep 1 }
}

sub initializeFrameworks {
	my $log = shift;

	main::INFOLOG && $log->info("Server OSDetect init...");

	Slim::Utils::OSDetect::init();
	Slim::Utils::OSDetect::getOS->initSearchPath();

	# initialize Server subsystems
	main::INFOLOG && $log->info("Server settings init...");

	Slim::Utils::Prefs::init();

	Slim::Utils::Prefs::makeCacheDir();

	main::INFOLOG && $log->info("Server strings init...");

	Slim::Utils::Strings::init();

	# Bug 16188 - create dummy protocol entries for all protocol handlers known to the main server
	# this ensures that when we scan a url for one of these protocols we treat it as a valid remote entry

	for my $handler(@{$prefs->get('registeredhandlers') || []}) {

		if (!defined Slim::Player::ProtocolHandlers->handlerForProtocol($handler)) {
			Slim::Player::ProtocolHandlers->registerHandler( $handler => 1 );
		}
	}
}

sub usage {
	print <<EOF;
Usage: $0 [debug options] [--rescan] [--wipe]

Command line options:

	--force         Force a scan, even if we think a scan is already taking place.
	--rescan        Look for new files since the last scan.
	--wipe          Wipe the DB and start from scratch
	--playlists     Only scan files in your playlistdir.
	--onlinelibrary Only update online library content
	--progress      Show a progress bar of the scan.
	--prefsdir      Specify alternative preferences directory.
	--priority      set process priority from -20 (high) to 20 (low)
	--logfile       Send all debugging messages to the specified logfile.
	--logdir        Specify folder location for log file
	--logconfig     Specify pre-defined logging configuration file
	--nodebuglog    Disable all debug-level logging (compiled out).
	--noinfolog     Disable all debug-level & info-level logging (compiled out).
	--nostatistics  Disable the TracksPersistent table used to keep to statistics across rescans (compiled out).
	--debug         various debug options
	--quiet         keep silent

Examples:

	$0 --rescan

EOF

}

sub cleanup {

	# cleanup() is called at the end of main() and possibly again from a sig handler
	# We only want it to run once.
	return if $cleanupDone;
	$cleanupDone = 1;

	if ($ownsScannerLock) {
		eval { Slim::Utils::PluginManager->shutdownPlugins(); };

		# Make sure to flush committed work or rollback aborted scans.
		if ($INC{'Slim/Schema.pm'} && Slim::Schema->storage) {
			if ($scanCompleted && !$scanAborted) {
				Slim::Music::Import->setIsScanning(0);
				Slim::Schema->forceCommit;
			} else {
				eval {
					if (Slim::Schema->can('rollback')) {
						Slim::Schema->rollback;
					} elsif (my $dbh = Slim::Schema->dbh) {
						$dbh->rollback unless $dbh->{'AutoCommit'};
					}
					if (my $dbh = Slim::Schema->dbh) {
						$dbh->{'AutoCommit'} = 1;
					}
				};
				if ($@) {
					eval { logger('server')->error("Failed to rollback aborted scan transaction: [$@]"); };
				}
				eval { Slim::Music::Import->setIsScanning(0); };
			}

			eval { Slim::Schema->disconnect; };
		}

		# Notify server we are exiting
		eval { $sqlHelperClass->exitScan(); };
		eval { $sqlHelperClass->cleanup; };

		remove_pid_file();
	}
}

sub checkDataSource {
	my $mediadirs = Slim::Utils::Misc::getMediaDirs();
	my $modified = 0;

	foreach my $audiodir (@$mediadirs) {
		if (defined $audiodir && $audiodir =~ m|[/\\]$|) {
			$audiodir =~ s|[/\\]$||;
			$modified++;
		}
	}

	$prefs->set('mediadirs', $mediadirs) if $modified;

	return if !Slim::Schema::hasLibrary();

	$sqlHelperClass->checkDataSource();
}

sub save_pid_file {
	return unless defined $pidfile && $pidfile ne '';
	logger('')->info("Scanner saving pid file.");

	if (-l $pidfile) {
		die "Refusing to write PID file '$pidfile': target is a symlink\n";
	}

	my $nofollow = eval { Fcntl::O_NOFOLLOW() } || 0;
	my $flags    = Fcntl::O_WRONLY() | Fcntl::O_CREAT() | $nofollow;
	sysopen(my $fh, $pidfile, $flags, 0644)
		or die "Cannot safely open PID file '$pidfile': $!\n";

	my @lst = lstat($pidfile);
	my @fst = stat($fh);
	if (!@lst || !@fst || -l $pidfile || !-f $fh || $lst[0] != $fst[0] || $lst[1] != $fst[1]) {
		close $fh;
		die "Refusing to write PID file '$pidfile': unsafe file type or symlink race\n";
	}

	truncate($fh, 0) or do {
		my $err = $!;
		close $fh;
		die "Cannot truncate PID file '$pidfile': $err\n";
	};
	seek($fh, 0, 0);
	print {$fh} $$ or do {
		my $err = $!;
		close $fh;
		die "Cannot write PID file '$pidfile': $err\n";
	};
	close $fh;
}

sub remove_pid_file {
	return unless defined $pidfile && $pidfile ne '';
	return if -l $pidfile || !-f $pidfile;
	if (open my $fh, '<', $pidfile) {
		my $content = do { local $/; <$fh> };
		close $fh;
		if (defined $content && $content =~ /^\s*(\d+)\s*$/ && $1 == $$) {
			unlink $pidfile;
		}
	}
}

sub END {
	my $exit_status = $?;
	cleanup() if $ownsScannerLock && !$cleanupDone;
	Slim::bootstrap::theEND();
	$? = $exit_status;
}

sub idleStreams {}

main();

__END__
