#!/usr/bin/perl
#
# Stand-alone interface to GDResizer
#
# TODO:
# Better error handling
#

use strict;
use FindBin qw($Bin);
use lib $Bin;

use constant RESIZER           => 1;
use constant SLIM_SERVICE      => 0;
use constant PERFMON           => 0;
use constant SCANNER           => 0;
use constant ISWINDOWS         => ( $^O =~ /^m?s?win/i ) ? 1 : 0;
use constant DEBUG             => ( grep { /--debug/ } @ARGV ) ? 1 : 0;
use constant DEBUGLOG          => DEBUG;
use constant INFOLOG           => 0;
use constant SOCKET_PATH       => '/tmp/sbs_artwork';
use constant SOCKET_IO_TIMEOUT => 15;
use constant MAX_FRAME_BYTES   => 32 * 1024 * 1024;
use constant LOCALFILE         => 0;
use constant NOMYSB            => 1;

# Copied from Slim::bootstrap to reduce memory overhead of unnecessary stuff
BEGIN {
	use Config;
	use File::Spec::Functions qw(catdir);
	use Slim::Utils::OSDetect;	# XXX would be nice to do without this

	my $libPath = $Bin;
	my @SlimINC = ();

	Slim::Utils::OSDetect::init();

	if (my $libs = Slim::Utils::OSDetect::dirsFor('libpath')) {
		# On Debian, RH and SUSE, our CPAN directory is located in the same dir as strings.txt
		$libPath = $libs;
	}

	# NB: The user may be on a platform who's perl reports a
	# different x86 version than we've supplied - but it may work
	# anyways.
	my $arch = $Config::Config{'archname'};
	   $arch =~ s/^i[3456]86-/i386-/;
	   $arch =~ s/gnu-//;

	# Check for use64bitint Perls
	my $is64bitint = $arch =~ /64int/;

	# Some ARM platforms use different arch strings, just assume any arm*linux system
	# can run our binaries, this will fail for some people running invalid versions of Perl
	# but that's OK, they'd be broken anyway.
	if ( $arch =~ /^arm.*linux/ ) {
		$arch = $arch =~ /gnueabihf/
			? 'arm-linux-gnueabihf-thread-multi'
			: 'arm-linux-gnueabi-thread-multi';
		$arch .= '-64int' if $is64bitint;
	}

	# Same thing with PPC
	if ( $arch =~ /^(?:ppc|powerpc).*linux/ ) {
		$arch = 'powerpc-linux-thread-multi';
		$arch .= '-64int' if $is64bitint;
	}

	my $perlmajorversion = $Config{'version'};
	   $perlmajorversion =~ s/\.\d+$//;

	@SlimINC = (
		catdir($libPath,'CPAN','arch',$perlmajorversion, $arch),
		catdir($libPath,'CPAN','arch',$perlmajorversion, $arch, 'auto'),
		catdir($libPath,'CPAN','arch',$Config{'version'}, $Config::Config{'archname'}),
		catdir($libPath,'CPAN','arch',$Config{'version'}, $Config::Config{'archname'}, 'auto'),
		catdir($libPath,'CPAN','arch',$perlmajorversion, $Config::Config{'archname'}),
		catdir($libPath,'CPAN','arch',$perlmajorversion, $Config::Config{'archname'}, 'auto'),
		catdir($libPath,'CPAN','arch',$Config::Config{'archname'}),
		catdir($libPath,'CPAN','arch',$perlmajorversion),
		catdir($libPath,'lib'),
		catdir($libPath,'CPAN'),
		$libPath,
	);

	# This works like 'use lib'
	# prepend our directories to @INC so we look there first.
	unshift @INC, @SlimINC;
};

use Cwd qw(realpath);
use Errno ();
use File::Basename qw(dirname);
use File::Spec;
use File::Temp qw(tempdir);
use IO::Select;
use IO::Socket::UNIX;
use Socket qw(SOL_SOCKET);
use Time::HiRes ();

use Slim::Utils::ArtworkCache;
use Slim::Utils::GDResizer;

our ($faster);

while ($_ = shift @ARGV) {
	if ($_ eq '--faster') {
		$faster = 1;
	}
}

sub _inspect_existing_socket {
	my ($sock_path) = @_;
	return 'missing' unless lstat($sock_path);

	if (-l _ || !-S _) {
		die "Refusing to replace non-socket path: $sock_path\n";
	}
	if (!-o _) {
		die "Refusing to replace socket not owned by current user: $sock_path\n";
	}

	my $probe = IO::Socket::UNIX->new(
		Type => SOCK_STREAM,
		Peer => $sock_path,
	);
	if ($probe) {
		close($probe);
		die "Refusing to replace active socket in use by live peer: $sock_path\n";
	}
	if ($!{ENOENT}) {
		return 'missing';
	}
	if ($!{ECONNREFUSED}) {
		return 'stale';
	}
	die "Failed probing existing socket $sock_path: $!\n";
}

sub _bind_owner_only_socket {
	my ($sock_path) = @_;
	my $state = _inspect_existing_socket($sock_path);

	my $parent = dirname($sock_path);
	my $stage_dir = tempdir('.gdresized-sock-XXXXXX', DIR => $parent, CLEANUP => 1);
	chmod(0700, $stage_dir) or die "Failed to secure staging dir $stage_dir: $!\n";

	my $staged_sock = "$stage_dir/s";
	my $old_umask = umask(0077);
	my $sock = eval {
		IO::Socket::UNIX->new(
			Type   => SOCK_STREAM,
			Local  => $staged_sock,
			Listen => SOMAXCONN,
		);
	};
	my $bind_err = $@ || $!;
	umask($old_umask);
	$sock || die "Unable to open socket: $bind_err\n";

	chmod(0600, $staged_sock) or die "Failed to set permissions on $staged_sock: $!\n";

	# Re-verify target immediately before publishing
	$state = _inspect_existing_socket($sock_path);
	if ($state eq 'missing') {
		if (!link($staged_sock, $sock_path)) {
			die "Unable to publish socket $sock_path: $!\n";
		}
		unlink($staged_sock);
	}
	else {
		rename($staged_sock, $sock_path) or die "Unable to replace stale socket $sock_path: $!\n";
	}

	my @st = lstat($sock_path);
	if (!@st || -l _ || !-S _ || !-o _) {
		die "Socket validation failed after bind: $sock_path\n";
	}

	return ($sock, $st[0], $st[1]);
}

sub _validate_peer {
	my ($client) = @_;
	return 1 unless $^O eq 'linux' && Socket->can('SO_PEERCRED');
	my $so_peercred = eval { Socket::SO_PEERCRED() };
	return 1 unless defined $so_peercred;

	my $cred = getsockopt($client, SOL_SOCKET, $so_peercred);
	return 0 unless defined $cred && length($cred) == 12;
	my ($pid, $uid, $gid) = unpack('i I I', $cred);
	return (defined $uid && $uid == $> && defined $pid && $pid > 0) ? 1 : 0;
}

sub _set_socket_timeouts {
	my ($sock, $secs) = @_;
	my $tv = eval { pack('l!l!', $secs, 0) };
	return unless defined $tv;
	if (Socket->can('SO_RCVTIMEO')) {
		my $opt = eval { Socket::SO_RCVTIMEO() };
		setsockopt($sock, SOL_SOCKET, $opt, $tv) if defined $opt;
	}
	if (Socket->can('SO_SNDTIMEO')) {
		my $opt = eval { Socket::SO_SNDTIMEO() };
		setsockopt($sock, SOL_SOCKET, $opt, $tv) if defined $opt;
	}
}

sub _read_frame_with_timeout {
	my ($client, $timeout_secs) = @_;
	my $sel = IO::Select->new($client);
	my $deadline = Time::HiRes::time() + $timeout_secs;
	my $buf = '';

	while (1) {
		my $nl = index($buf, "\n");
		if ($nl >= 0) {
			return substr($buf, 0, $nl + 1);
		}

		my $remaining = $deadline - Time::HiRes::time();
		die "Client read timed out\n" if $remaining <= 0;

		my @ready = $sel->can_read($remaining);
		die "Client read timed out\n" unless @ready;

		my $n = sysread($client, my $chunk, 65536);
		if (!defined $n) {
			next if $!{EINTR};
			die "Client read failed: $!\n";
		}
		if ($n == 0) {
			die "Malformed frame: unexpected EOF before newline\n";
		}
		if (length($buf) + $n > MAX_FRAME_BYTES) {
			die "Frame exceeds maximum allowed size\n";
		}
		$buf .= $chunk;
	}
}

sub _write_all_with_timeout {
	my ($client, $payload, $timeout_secs) = @_;
	my $sel = IO::Select->new($client);
	my $deadline = Time::HiRes::time() + $timeout_secs;
	my $offset = 0;
	my $len = length($payload);

	while ($offset < $len) {
		my $remaining = $deadline - Time::HiRes::time();
		return 0 if $remaining <= 0;
		my @ready = $sel->can_write($remaining);
		return 0 unless @ready;

		my $n = syswrite($client, $payload, $len - $offset, $offset);
		if (!defined $n) {
			next if $!{EINTR};
			return 0;
		}
		return 0 if $n == 0;
		$offset += $n;
	}
	return 1;
}

sub _validate_canonical_dir {
	my ($dir) = @_;
	die "Invalid cacheroot: undefined\n" unless defined $dir && length $dir;
	die "Invalid cacheroot: disallowed control characters\n" if $dir =~ /[;\0\r\n?%]/;
	die "Invalid cacheroot: must be absolute\n" unless File::Spec->file_name_is_absolute($dir);
	die "Invalid cacheroot: dot-traversal disallowed\n" if $dir =~ m{(?:^|/)\.\.(?:/|$)};
	die "Invalid cacheroot: does not exist\n" unless lstat($dir);
	die "Invalid cacheroot: symlink disallowed\n" if -l _;
	die "Invalid cacheroot: not a directory\n" unless -d _;
	die "Invalid cacheroot: ownership mismatch\n" unless -o _;

	my $canon = realpath($dir);
	die "Invalid cacheroot: canonicalize failed\n" unless defined $canon && length $canon;
	die "Invalid cacheroot: symlink traversal disallowed\n" unless $canon eq $dir;
	die "Invalid cacheroot: canonical target invalid\n" unless lstat($canon) && !-l _ && -d _ && -o _;
	return $canon;
}

sub _validate_canonical_file {
	my ($path) = @_;
	die "Invalid input file: undefined\n" unless defined $path && length $path;
	die "Invalid input file: disallowed control characters\n" if $path =~ /[\0\r\n]/;
	die "Invalid input file: must be absolute\n" unless File::Spec->file_name_is_absolute($path);
	die "Invalid input file: dot-traversal disallowed\n" if $path =~ m{(?:^|/)\.\.(?:/|$)};
	die "Invalid input file: does not exist\n" unless lstat($path);
	die "Invalid input file: symlink disallowed\n" if -l _;
	die "Invalid input file: not a regular file\n" unless -f _ && -r _;

	my $canon = realpath($path);
	die "Invalid input file: canonicalize failed\n" unless defined $canon && length $canon;
	die "Invalid input file: symlink traversal disallowed\n" unless $canon eq $path;
	die "Invalid input file: canonical target invalid\n" unless lstat($canon) && !-l _ && -f _ && -r _;
	return $canon;
}

my ($socket, $sock_dev, $sock_ino) = _bind_owner_only_socket(SOCKET_PATH);

my $shutdown_requested = 0;
$SIG{PIPE} = 'IGNORE';
$SIG{HUP}  = 'IGNORE';
$SIG{TERM} = $SIG{INT} = $SIG{QUIT} = sub {
	$shutdown_requested = 1;
};

# Lazily instantiate cache stores only after validated cacheroot is received (no '.' CWD fallback)
my ($artworkCache, $imageProxyCache);

DEBUG && warn "$0 listening on " . SOCKET_PATH . "\n";

my $accept_sel = IO::Select->new($socket);

while (!$shutdown_requested) {
	my @ready = $accept_sel->can_read(0.5);
	last if $shutdown_requested;
	next unless @ready;

	my $client = $socket->accept();
	if (!$client) {
		last if $shutdown_requested;
		next if $!{EINTR} || $!{EAGAIN} || $!{EWOULDBLOCK} || $!{ECONNABORTED};
		warn "accept() failed: $!\n" if DEBUG;
		last;
	}

	if (!_validate_peer($client)) {
		warn "Rejected unauthorized peer on " . SOCKET_PATH . "\n" if DEBUG;
		close($client);
		next;
	}

	_set_socket_timeouts($client, SOCKET_IO_TIMEOUT);

	eval {
		DEBUG && (my $tv = Time::HiRes::time());

		my $buf = _read_frame_with_timeout($client, SOCKET_IO_TIMEOUT);
		$buf =~ s/\r?\n\z// or die "Malformed frame: missing trailing newline\n";

		my @parts = split(/\0/, $buf, 6);
		if (@parts != 6 || $parts[5] ne '') {
			die "Malformed frame: expected exactly 5 NUL-terminated fields\n";
		}
		my ($file, $spec, $cacheroot, $cachekey, $data) = @parts[0..4];

		for my $f ($file, $spec, $cacheroot, $cachekey, $data) {
			die "Malformed frame: embedded CR/LF disallowed\n" if $f =~ /[\r\n]/;
		}

		if (length($data)) {
			require MIME::Base64;
			$data = MIME::Base64::decode_base64($data);
			die "Decoded inline image data is empty\n" unless defined $data && length($data);
		}
		elsif ($file eq 'data') {
			die "Missing inline image data\n";
		}

		# An empty spec is allowed, this returns the original image
		$spec ||= 'XxX';

		my $imageproxy = $cachekey =~ /^imageproxy/;
		DEBUG && warn sprintf("file=%s, spec=%s, cacheroot=%s, cachekey=%s, imageproxy=%s, imagedata=%s bytes\n", $file, $spec, $cacheroot, $cachekey, $imageproxy || 0, length($data));

		if ( !$file || !$spec || !$cacheroot || !$cachekey ) {
			die sprintf("Invalid parameters: file=%s, spec=%s, cacheroot=%s, cachekey=%s, imageproxy=%s, imagedata=%s bytes\n", $file, $spec, $cacheroot, $cachekey, $imageproxy || 0, length($data || ''));
		}

		my $canon_root = _validate_canonical_dir($cacheroot);
		my $input_arg  = length($data) ? \$data : _validate_canonical_file($file);

		my @spec = split ',', $spec;

		my $cache;
		if ($imageproxy) {
			$imageProxyCache ||= Slim::Web::ImageProxy::Cache->new($canon_root);
			if ($imageProxyCache->getRoot() ne $canon_root) {
				$imageProxyCache->setRoot($canon_root);
			}
			$cache = $imageProxyCache;
		}
		else {
			$artworkCache ||= Slim::Utils::ArtworkCache->new($canon_root);
			if ($artworkCache->getRoot() ne $canon_root) {
				$artworkCache->setRoot($canon_root);
			}
			$cache = $artworkCache;
		}
		$cache->pragma('locking_mode = NORMAL');

		# do resize
		Slim::Utils::GDResizer->gdresize(
			file     => $input_arg,
			debug    => DEBUG,
			faster   => $faster,
			cache    => $cache,
			cachekey => $cachekey,
			spec     => \@spec,
		);

		# send result
		_write_all_with_timeout($client, "OK\015\012", SOCKET_IO_TIMEOUT)
			or warn "Failed writing OK response to client\n";

		DEBUG && warn "OK (" . (Time::HiRes::time() - $tv) . " seconds)\n";
	};

	if ( $@ ) {
		my $err = "$@";
		$err =~ s/[\r\n]+$//;
		_write_all_with_timeout($client, "Error: $err\015\012", SOCKET_IO_TIMEOUT);
		warn "$err\n";
	}

	close($client);
}

close($socket) if $socket;
if (defined $sock_dev && defined $sock_ino) {
	my @st = lstat(SOCKET_PATH);
	if (@st && !-l _ && -S _ && -o _ && $st[0] == $sock_dev && $st[1] == $sock_ino) {
		unlink(SOCKET_PATH);
	}
}

1;

package Slim::Web::ImageProxy::Cache;

use base 'Slim::Utils::DbArtworkCache';

use strict;

sub new {
	my $class = shift;
	my $root = shift;

	return $class->SUPER::new($root, 'imgproxy', 86400*30);
}

__END__

=head1 NAME

gdresized.pl - Artwork resizer daemon

=head1 SYNOPSIS

Resize normal image file or an audio file's embedded tags:

Options:

  --debug          Display debug information
  --faster         Use ugly but fast copyResized function

=cut
