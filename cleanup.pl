#!/usr/bin/env perl

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
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

use FindBin qw($RealBin);
use File::Spec ();
use lib $RealBin, File::Spec->catdir($RealBin, 'CPAN');

use Config;
use constant ISWINDOWS    => ( $^O =~ /^m?s?win/i ) ? 1 : 0;
use constant ISMAC        => ( $^O =~ /darwin/i ) ? 1 : 0;
use constant ISACTIVEPERL => 0;

use strict;
use Errno ();
use Fcntl ();
use Socket;
use utf8;

use constant SLIM_SERVICE => 0;
use constant SCANNER      => 0;
use constant RESIZER      => 0;
use constant DEBUG        => 1;
use constant TRANSCODING  => 0;
use constant PERFMON      => 0;
use constant DEBUGLOG     => 0;
use constant INFOLOG      => 0;
use constant STATISTICS   => 0;
use constant SB1SLIMP3SYNC=> 0;
use constant WEBUI        => 0;
use constant LOCALFILE    => 0;
use constant NOMYSB       => 1;

# load these later, don't need them right now
require Cwd;
require File::Basename;
require File::Path;
require File::Spec::Functions;
require Getopt::Long;

require Slim::Utils::OSDetect;
require Slim::Utils::Light;

our $VERSION = '9.2.0';

if (DEBUG && $@) {
	print "GUI can't be loaded: $@\n";
}

my ($os);
my $folderListErrors = 0;

our $prefsdir;

sub main {
	my ($all, $cache, $filecache, $database, $prefs, $logs, $dryrun);

	if ( !Getopt::Long::GetOptions(
		'all'        => \$all,
		'cache'      => \$cache,
		'filecache'  => \$filecache,
		'prefs'      => \$prefs,
		'prefsdir=s' => \$prefsdir,
		'logs'       => \$logs,
		'database'   => \$database,
		'dryrun'     => \$dryrun,
	) ) {
		usage();
		exit 1;
	}

	unshift @ARGV, ('--prefsdir', $prefsdir) if defined $prefsdir && $prefsdir ne '';

	Slim::Utils::OSDetect::init();
	$os = Slim::Utils::OSDetect->getOS();

	if (checkForSC()) {
		print sprintf("\n%s\n\n", Slim::Utils::Light::string('CLEANUP_PLEASE_STOP_SC'));
		exit 1;
	}

	$folderListErrors = 0;
	my $folders = getFolderList({
		'all'       => $all,
		'cache'     => $cache,
		'filecache' => $filecache,
		'prefs'     => $prefs,
		'logs'      => $logs,
		'database'  => $database,
	});

	unless (scalar @$folders) {
		usage();
		exit($folderListErrors ? 1 : 0);
	}

	my $errors = cleanup($folders, $dryrun);
	$errors += $folderListErrors;

	if ($errors) {
		warn "Cleanup completed with $errors error(s).\n";
		exit 1;
	}

	print sprintf("\n%s\n\n", Slim::Utils::Light::string('CLEANUP_PLEASE_RESTART_SC'));
}

sub usage {
	my $usage = <<EOF;
%s: $0 [--all] [--cache] [--database] [--filecache] [--prefs] [--logs]

%s

	--database     %s
	--filecache    %s
	--prefs        %s
	--logs         %s

	--cache   (!)  %s

	--all     (!!) %s

	--dryrun       %s

EOF
	print sprintf($usage,
		Slim::Utils::Light::string('CLEANUP_USAGE'),
		Slim::Utils::Light::string('CLEANUP_COMMAND_LINE'),
		Slim::Utils::Light::string('CLEANUP_DB'),
		Slim::Utils::Light::string('CLEANUP_FILECACHE'),
		Slim::Utils::Light::string('CLEANUP_PREFS'),
		Slim::Utils::Light::string('CLEANUP_LOGS'),
		Slim::Utils::Light::string('CLEANUP_CACHE'),
		Slim::Utils::Light::string('CLEANUP_ALL'),
		Slim::Utils::Light::string('CLEANUP_DRYRUN'),
	);
}

sub _homeDirs {
	my @homes;
	for my $var (qw(HOME USERPROFILE)) {
		push @homes, $ENV{$var} if defined $ENV{$var} && $ENV{$var} !~ /^\s*$/;
	}
	if (!ISWINDOWS) {
		my $pw_dir = eval { (getpwuid($<))[7] };
		push @homes, $pw_dir if defined $pw_dir && $pw_dir !~ /^\s*$/;
	}

	my %seen;
	my @resolved;
	for my $h (@homes) {
		my $norm = File::Spec::Functions::canonpath($h);
		my $real = (-e $norm) ? eval { Cwd::realpath($norm) } : undef;
		for my $cand ($norm, $real) {
			next unless defined $cand && $cand !~ /^\s*$/;
			$cand =~ s{\\}{/}g if ISWINDOWS;
			$cand =~ s{/+$}{} if length($cand) > 1;
			my $key = ISWINDOWS ? lc($cand) : $cand;
			push @resolved, $cand unless $seen{$key}++;
		}
	}
	return @resolved;
}

sub _isRootOrHome {
	my ($path) = @_;
	return 1 if !defined $path || $path =~ /^\s*$/;

	my $norm = File::Spec::Functions::canonpath($path);
	return 1 if !defined $norm || $norm =~ /^\s*$/;

	my $cmp = $norm;
	$cmp =~ s{\\}{/}g if ISWINDOWS;

	# Refuse filesystem root (/, //, C:/, C:\, \\server\share)
	return 1 if $cmp =~ m{^/+$};
	return 1 if $cmp =~ m{^[A-Za-z]:/*$};
	return 1 if $cmp =~ m{^//[^/]+(?:/[^/]+)?/*$};
	return 1 if File::Basename::dirname($norm) eq $norm;

	my $cmp_trimmed = $cmp;
	$cmp_trimmed =~ s{/+$}{} if length($cmp_trimmed) > 1;

	for my $home (_homeDirs()) {
		if (ISWINDOWS) {
			return 1 if lc($cmp_trimmed) eq lc($home);
		} else {
			return 1 if $cmp_trimmed eq $home;
		}
	}

	return 0;
}

sub _allowedCleanupRoots {
	my @candidates;
	push @candidates, Slim::Utils::Light::getPref('cachedir');
	for my $key (qw(cache prefs oldprefs log)) {
		push @candidates, $os->dirsFor($key) if $os;
	}

	my %seen;
	my @roots;
	for my $dir (@candidates) {
		next if !defined $dir || $dir =~ /^\s*$/;
		next if -l $dir;
		next if _isRootOrHome($dir);
		next unless -e $dir;

		my $real = Cwd::realpath($dir);
		next unless defined $real;
		next if _isRootOrHome($real);

		my $key = $real;
		$key =~ s{\\}{/}g if ISWINDOWS;
		$key =~ s{/+$}{};
		$key = lc($key) if ISWINDOWS;
		next if $seen{$key}++;

		push @roots, $real;
	}
	return \@roots;
}

sub _isInsideAllowedRoots {
	my ($real_target, $allowed_roots) = @_;
	return 0 unless defined $real_target && $allowed_roots && @$allowed_roots;

	my $t = $real_target;
	$t =~ s{\\}{/}g if ISWINDOWS;
	$t =~ s{/+$}{};

	for my $root (@$allowed_roots) {
		my $r = $root;
		$r =~ s{\\}{/}g if ISWINDOWS;
		$r =~ s{/+$}{};
		next unless length $r;

		if (ISWINDOWS) {
			return 1 if lc($t) eq lc($r) || index(lc($t), lc("$r/")) == 0;
		} else {
			return 1 if $t eq $r || index($t, "$r/") == 0;
		}
	}
	return 0;
}

sub getFolderList {
	my $args = shift;

	my @folders;
	my $cacheFolder = Slim::Utils::Light::getPref('cachedir') || $os->dirsFor('cache');
	my $cacheNeeded = ($args->{all} || $args->{prefs} || $args->{cache} || $args->{filecache} || $args->{logs} || $args->{database});
	my $cacheSafe   = 1;

	if ($cacheNeeded) {
		if (!defined $cacheFolder || $cacheFolder =~ /^\s*$/ || -l $cacheFolder || _isRootOrHome($cacheFolder)) {
			warn "Refusing unsafe cache directory target: " . (defined $cacheFolder ? $cacheFolder : '<undef>') . "\n";
			$folderListErrors++;
			$cacheSafe = 0;
		}
	}

	push @folders, _target('cache', 'cache') if ($args->{all} || $args->{cache});

	if ($cacheSafe && $cacheNeeded) {
		push @folders, {
			label   => 'some legacy files',
			folders => [
				File::Spec::Functions::catdir($cacheFolder, 'MySQL'),
				File::Spec::Functions::catdir($cacheFolder, 'my.cnf'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezecenter-mysql.pid'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezecenter-mysql.sock'),
				File::Spec::Functions::catdir($cacheFolder, 'mysql-error-log.txt'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezebox.db'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezebox.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezebox.db-wal'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezebox-persistent.db'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezebox-persistent.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'squeezebox-persistent.db-wal'),
				File::Spec::Functions::catdir($cacheFolder, 'ArtworkCache.db'),
				File::Spec::Functions::catdir($cacheFolder, 'ArtworkCache.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'ArtworkCache.db-wal'),
			],
		};
	}

	if ($cacheSafe && $args->{filecache}) {
		push @folders, {
			label   => 'file cache (artwork, templates etc.)',
			folders => [
				File::Spec::Functions::catdir($cacheFolder, 'Artwork'),
				File::Spec::Functions::catdir($cacheFolder, 'ArtworkCache'),
				File::Spec::Functions::catdir($cacheFolder, 'audioUploads'),
				File::Spec::Functions::catdir($cacheFolder, 'iTunesArtwork'),
				File::Spec::Functions::catdir($cacheFolder, 'FileCache'),
				File::Spec::Functions::catdir($cacheFolder, 'fonts.bin'),
				File::Spec::Functions::catdir($cacheFolder, 'strings.bin'),
				File::Spec::Functions::catdir($cacheFolder, 'extrastrings.json'),
				File::Spec::Functions::catdir($cacheFolder, 'templates'),
				File::Spec::Functions::catdir($cacheFolder, 'updates'),
				File::Spec::Functions::catdir($cacheFolder, 'cookies.dat'),
				File::Spec::Functions::catdir($cacheFolder, 'plugin-data.yaml'),

				File::Spec::Functions::catdir($cacheFolder, 'cache.db'),
				File::Spec::Functions::catdir($cacheFolder, 'cache.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'cache.db-wal'),

				File::Spec::Functions::catdir($cacheFolder, 'artwork.db'),
				File::Spec::Functions::catdir($cacheFolder, 'artwork.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'artwork.db-wal'),

				File::Spec::Functions::catdir($cacheFolder, 'imgproxy.db'),
				File::Spec::Functions::catdir($cacheFolder, 'imgproxy.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'imgproxy.db-wal'),
			],
		};
	}

	if ($cacheSafe && $args->{database}) {
		push @folders, {
			label   => 'Musiclibrary data',
			folders => [
				File::Spec::Functions::catdir($cacheFolder, 'libmediascan.db'),
				File::Spec::Functions::catdir($cacheFolder, '__db.001'),
				File::Spec::Functions::catdir($cacheFolder, '__db.002'),
				File::Spec::Functions::catdir($cacheFolder, '__db.003'),
				File::Spec::Functions::catdir($cacheFolder, 'library.db'),
				File::Spec::Functions::catdir($cacheFolder, 'library.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'library.db-wal'),
				File::Spec::Functions::catdir($cacheFolder, 'persist.db'),
				File::Spec::Functions::catdir($cacheFolder, 'persist.db-shm'),
				File::Spec::Functions::catdir($cacheFolder, 'persist.db-wal'),
			],
		};
	}

	if ($args->{all} || $args->{prefs}) {
		push @folders, _target('prefs', 'preferences');
		push @folders, _target('oldprefs', 'old preferences (SlimServer <= 6.5)');
	}

	push @folders, _target('log', 'logs') if ($args->{all} || $args->{logs});

	return \@folders;
}

sub _target {
	my ($value, $label) = @_;

	my $f = $os->dirsFor($value);
	if (!defined $f || $f =~ /^\s*$/ || -l $f || _isRootOrHome($f)) {
		warn "Refusing unsafe $label cleanup target: " . (defined $f ? $f : '<undef>') . "\n";
		$folderListErrors++;
		return {
			label   => $label,
			folders => [],
		};
	}

	return {
		label   => $label,
		folders => [ $f ],
	};
}

sub options {

	my $options = [
		{
			name     => 'prefs',
			title    => Slim::Utils::Light::string('CLEANUP_PREFS'),
			position => [30, 20],
		},

		{
			name     => 'filecache',
			title    => Slim::Utils::Light::string('CLEANUP_FILECACHE'),
			position => [30, 40],
		},

		{

			name     => 'database',
			title    => Slim::Utils::Light::string('CLEANUP_DB'),
			position => [30, 60],
		},

		{

			name     => 'logs',
			title    => Slim::Utils::Light::string('CLEANUP_LOGS'),
			position => [30, 80],
		},

		{

			name     => 'cache',
			title    => Slim::Utils::Light::string('CLEANUP_CACHE'),
			position => [30, 120],
		},

		{

			name     => 'all',
			title    => '(!) ' . Slim::Utils::Light::string('CLEANUP_ALL'),
			position => [30, 160],
		},
	];

	return $options;
}

sub checkForSC {
	# 1. Check SlimProto TCP port on localhost
	my $iaddr = inet_aton('127.0.0.1');
	if ($iaddr) {
		my $paddr = sockaddr_in(3483, $iaddr);
		my $proto = getprotobyname('tcp');
		$proto = eval { Socket::IPPROTO_TCP() } // 6 unless defined $proto;

		my $sock;
		if (socket($sock, PF_INET, SOCK_STREAM, $proto)) {
			if (connect($sock, $paddr)) {
				close($sock);
				return 1;
			}
			close($sock);
		} else {
			warn "Warning: socket() failed checking server port 3483: $!\n";
		}
	}

	# 2. Check server/scanner PID files and Unix socket locks
	my @check_dirs;
	my $cacheFolder = Slim::Utils::Light::getPref('cachedir') || ($os ? $os->dirsFor('cache') : undef);
	push @check_dirs, $cacheFolder if defined $cacheFolder;
	if ($os) {
		for my $key (qw(cache prefs log)) {
			my $d = $os->dirsFor($key);
			push @check_dirs, $d if defined $d;
		}
	}
	push @check_dirs, (
		'/var/run/lyrionmusicserver',
		'/var/run/squeezeboxserver',
		'/run/lyrionmusicserver',
		'/run/squeezeboxserver',
	);

	my %seen_dir;
	my @pid_names = qw(
		slimserver.pid
		squeezecenter.pid
		squeezeboxserver.pid
		lyrionmusicserver.pid
		server.pid
		scanner.pid
		slimserver-scanner.pid
		squeezecenter-mysql.pid
	);

	for my $dir (@check_dirs) {
		next unless defined $dir && $dir !~ /^\s*$/ && -d $dir && !-l $dir;
		next if $seen_dir{$dir}++;

		for my $name (@pid_names) {
			my $pid_path = File::Spec::Functions::catfile($dir, $name);
			next unless -f $pid_path && !-l $pid_path;
			if (open my $fh, '<', $pid_path) {
				my $pid = <$fh>;
				close $fh;
				if (defined $pid && $pid =~ /^\s*(\d+)\s*$/) {
					my $num_pid = $1;
					return 1 if $num_pid > 0 && $num_pid != $$ && (kill(0, $num_pid) || $!{EPERM});
				}
			}
		}

		my $lock_path = File::Spec::Functions::catfile($dir, 'scanner.lock');
		if (-f $lock_path && !-l $lock_path) {
			my $mode = ISWINDOWS ? '+<' : '<';
			if (open my $lfh, $mode, $lock_path) {
				if (!flock($lfh, Fcntl::LOCK_EX() | Fcntl::LOCK_NB())) {
					if ($!{EWOULDBLOCK} || $!{EAGAIN}) {
						close $lfh;
						return 1;
					}
				} else {
					flock($lfh, Fcntl::LOCK_UN());
				}
				close $lfh;
			}
		}

		if (!ISWINDOWS) {
			my $sock_path = File::Spec::Functions::catfile($dir, 'squeezecenter-mysql.sock');
			if (-S $sock_path && !-l $sock_path) {
				my $usock;
				if (socket($usock, PF_UNIX, SOCK_STREAM, 0)) {
					if (connect($usock, sockaddr_un($sock_path))) {
						close($usock);
						return 1;
					}
					close($usock);
				} else {
					warn "Warning: Unix socket() failed checking '$sock_path': $!\n";
				}
			}
		}
	}

	# 3. Check live scanner / server processes on /proc-based systems
	if (!ISWINDOWS && -d '/proc' && opendir(my $dh, '/proc')) {
		while (my $entry = readdir($dh)) {
			next unless $entry =~ /^\d+$/;
			next if $entry == $$;
			my $cmdline_path = "/proc/$entry/cmdline";
			next unless -r $cmdline_path;
			if (open my $fh, '<:raw', $cmdline_path) {
				local $/;
				my $cmdline = <$fh>;
				close $fh;
				if (defined $cmdline && $cmdline =~ /(?:^|\0|\/)(?:(?:scanner|slimserver)\.pl|squeezeboxserver(?:-scanner)?|lyrionmusicserver(?:-scanner)?)(?:\0|$)/) {
					closedir $dh;
					return 1;
				}
			}
		}
		closedir $dh;
	}

	return 0;
}

sub cleanup {
	my ($folders, $dryrun) = @_;
	my $errors = 0;
	my $allowed_roots = _allowedCleanupRoots();

	for my $item (@$folders) {
		print sprintf("\n%s %s...\n", Slim::Utils::Light::string('CLEANUP_DELETING'), $item->{label});

		foreach my $target ( @{$item->{folders}} ) {
			if (!defined $target || $target =~ /^\s*$/) {
				warn "Refusing empty cleanup target.\n";
				$errors++;
				next;
			}

			if (_isRootOrHome($target)) {
				warn "Refusing unsafe cleanup target '$target'.\n";
				$errors++;
				next;
			}

			if (-l $target) {
				warn "Refusing symlink cleanup target '$target'.\n";
				$errors++;
				next;
			}

			next unless -e $target;

			my $real_target = Cwd::realpath($target);
			if (!defined $real_target || _isRootOrHome($real_target)) {
				warn "Refusing unresolvable or unsafe cleanup target '$target'.\n";
				$errors++;
				next;
			}

			if (!_isInsideAllowedRoots($real_target, $allowed_roots)) {
				warn "Refusing cleanup target outside allowed directories: '$target' (resolved: '$real_target').\n";
				$errors++;
				next;
			}

			print "-> $target\n";

			next if $dryrun;

			if (-d $real_target) {
				my $rmtree_err;
				eval { File::Path::rmtree($real_target, { error => \$rmtree_err }); };
				if ($@) {
					warn "Error removing directory '$real_target': $@\n";
					$errors++;
				}
				if ($rmtree_err && @$rmtree_err) {
					for my $diag (@$rmtree_err) {
						my ($file, $message) = %$diag;
						if ($file eq '') {
							warn "Error removing directory '$real_target': $message\n";
						} else {
							warn "Error removing '$file': $message\n";
						}
					}
					$errors++;
				}
			}

			elsif (-f $real_target || -S $real_target) {
				if (!unlink $real_target) {
					warn "Error removing file '$real_target': $!\n";
					$errors++;
				}
			}
		}
	}

	return $errors;
}

main();

__END__
