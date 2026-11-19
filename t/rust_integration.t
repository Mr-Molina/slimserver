#!/usr/bin/perl
use strict;
use warnings;

use FindBin qw($Bin);
use lib "$Bin/..";
use lib "$Bin/../lib";
use File::Temp qw(tempdir);
use IO::Socket::UNIX;
use MIME::Base64 qw(encode_base64);
use Digest::MD5 qw(md5_hex);
use JSON::PP qw(decode_json);
use POSIX qw(WNOHANG);
use constant {
    SCANNER         => 0,
    RESIZER         => 1,
    ISWINDOWS       => ( $^O =~ /^m?s?win/i ) ? 1 : 0,
    ISMAC           => ( $^O =~ /darwin/i ) ? 1 : 0,
    TRANSCODING     => 1,
    PERFMON         => 0,
    DEBUGLOG        => 0,
    INFOLOG         => 0,
    STATISTICS      => 1,
    SB1SLIMP3SYNC   => 1,
    WEBUI           => 1,
    NOMYSB          => 1,
    LOCALFILE       => 0,
    NOBROWSECACHE   => 0,
};
use Test::More tests => 18;

use Config;

my $root      = "$Bin/..";
my $arch      = ($Config{archname} || '') =~ /aarch64/ ? 'aarch64-linux' : 'x86_64-linux';
my $bin_dir   = "$root/Bin/$arch";
my $html_root = "$root/HTML";

sub run_quiet {
    my (@cmd) = @_;
    open my $saved_out, '>&', \*STDOUT or die "dup STDOUT: $!";
    open my $saved_err, '>&', \*STDERR or die "dup STDERR: $!";
    open STDOUT, '>', '/dev/null' or die "redir STDOUT: $!";
    open STDERR, '>', '/dev/null' or die "redir STDERR: $!";
    my $rc = system(@cmd);
    open STDOUT, '>&', $saved_out or die "restore STDOUT: $!";
    open STDERR, '>&', $saved_err or die "restore STDERR: $!";
    close $saved_out;
    close $saved_err;
    return $rc;
}

sub run_capture {
    my (@cmd) = @_;
    open my $saved_err, '>&', \*STDERR or die "dup STDERR: $!";
    open STDERR, '>', '/dev/null' or die "redir STDERR: $!";
    my $pid = open my $pipe, '-|', @cmd;
    my $open_err = $!;
    open STDERR, '>&', $saved_err or die "restore STDERR: $!";
    close $saved_err;
    die "exec @cmd failed: $open_err" unless defined $pid;
    binmode $pipe;
    local $/;
    my $out = <$pipe>;
    close $pipe;
    my $rc = $?;
    return ($rc, defined $out ? $out : '');
}

ok( -x "$bin_dir/gdresized",         "gdresized Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-transcode",     "lms-transcode Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-mp3cut",        "lms-mp3cut Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-audioscan",     "lms-audioscan Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-scanner-accel", "lms-scanner-accel Rust binary exists and is executable" );

my $tmp = tempdir( CLEANUP => 1 );
my $sock_path = "$tmp/sbs_artwork_test";
my $gd_pid;

END {
    if ( defined $gd_pid && $gd_pid > 0 ) {
        my $reaped = waitpid( $gd_pid, WNOHANG );
        if ( $reaped == 0 ) {
            kill 'TERM', $gd_pid;
            for ( 1 .. 20 ) {
                $reaped = waitpid( $gd_pid, WNOHANG );
                last if $reaped != 0;
                select( undef, undef, undef, 0.05 );
            }
            if ( $reaped == 0 ) {
                kill 'KILL', $gd_pid;
                waitpid( $gd_pid, 0 );
            }
        }
    }
    if ( defined $sock_path && -e $sock_path ) {
        unlink $sock_path;
    }
}

$gd_pid = fork();
defined $gd_pid or die "fork failed: $!";
if ( $gd_pid == 0 ) {
    exec( "$bin_dir/gdresized", "--socket", $sock_path, "--max-requests", "1" );
    exit 1;
}

for ( 1 .. 50 ) {
    last if -S $sock_path;
    select( undef, undef, undef, 0.05 );
}

require Slim::bootstrap;
require Slim::Utils::OSDetect;
Slim::Utils::OSDetect::init();
Slim::bootstrap->loadModules( undef, undef, "$Bin/.." );
require DBI;
require Image::Scale;
require Slim::Utils::GDResizer;
my $img_path = -f "$html_root/EN/html/images/b_delete.gif"
    ? "$html_root/EN/html/images/b_delete.gif"
    : "$html_root/Classic/html/images/b_delete.gif";
my ($src_ref, undef) = Slim::Utils::GDResizer->resize(
    file   => $img_path,
    width  => 40,
    height => 40,
    mode   => 'o',
    format => 'png',
);

my $cachekey = "music/test/cover_20x20_o.jpg";
my $client = IO::Socket::UNIX->new(
    Type => SOCK_STREAM,
    Peer => $sock_path,
) or die "Socket connect failed: $!";

my $frame = pack( 'Z* Z* Z* Z* Z*',
    'data',
    '20x20_o.jpg',
    $tmp,
    $cachekey,
    encode_base64( $$src_ref, '' ),
) . "\015\012";

print $client $frame;
my $resp = <$client>;
close $client;
waitpid( $gd_pid, 0 );
$gd_pid = undef;

is( $resp, "OK\015\012", "Rust gdresized daemon processed UNIX socket frame and returned OK" );

# Verify Perl DbCache 60-bit packed MD5 key, A3LS header, and JPEG SOI read-back from SQLite artwork.db
my $key_input = $cachekey;
utf8::encode($key_input) if utf8::is_utf8($key_input);
my $md5 = md5_hex($key_input);
my $db_key;
if ( $Config{ivsize} >= 8 ) {
    $db_key = ( hex( substr( $md5, 0, 8 ) ) << 28 ) | hex( substr( $md5, 8, 7 ) );
}
else {
    require Math::BigInt;
    $db_key = Math::BigInt->new( '0x' . substr( $md5, 0, 8 ) )
        ->blsft(28)
        ->bior( hex( substr( $md5, 8, 7 ) ) )
        ->bstr();
}
my $dbh = DBI->connect( "dbi:SQLite:dbname=$tmp/artwork.db", "", "", { RaiseError => 1 } );
my ($blob) = $dbh->selectrow_array( "SELECT v FROM cache WHERE k = ?", undef, $db_key );
$dbh->disconnect();

my ($ct, $mtime, $pathlen) = unpack( 'A3LS', $blob );
my $img_data = substr( $blob, 9 + $pathlen );
is( $ct, 'jpg', "SQLite artwork.db row matches Perl DbCache key and A3LS jpg header" );
ok(
    defined $img_data && length($img_data) > 2 && substr( $img_data, 0, 2 ) eq "\xFF\xD8",
    "SQLite artwork.db payload begins with valid JPEG SOI (0xFF 0xD8) header"
);

# Test 9 & 10: Verify MP3::Cut::Gapless delegation to Rust and C XS fallback
my $mp3_file = "$tmp/test_gapless.mp3";
my $frame_data = pack( 'C4', 0xFF, 0xFB, 0x90, 0x64 ) . ( "\0" x ( 417 - 4 ) );
open my $mp3_fh, '>', $mp3_file or die "Cannot write $mp3_file: $!";
binmode $mp3_fh;
print $mp3_fh $frame_data x 2;
close $mp3_fh;

require MP3::Cut::Gapless;

{
    local $ENV{LMS_USE_RUST} = 1;
    my $cut = MP3::Cut::Gapless->new(
        file      => $mp3_file,
        cache_dir => "$tmp/cache",
        start_ms  => 0,
        end_ms    => 50,
    );
    my $n = $cut->read( my $buf, 4096 );
    ok( $n > 0 && length($buf) > 0, 'MP3::Cut::Gapless->read delegates to Rust lms-mp3cut without SIGSEGV' );
}

{
    local $ENV{LMS_USE_RUST} = 0;
    my $cut_xs = MP3::Cut::Gapless->new(
        file     => $mp3_file,
        start_ms => 0,
        end_ms   => 50,
    );
    my $n_xs = $cut_xs->read( my $buf_xs, 4096 );
    ok( $n_xs > 0 && length($buf_xs) > 0, 'MP3::Cut::Gapless->read falls back cleanly to C XS when LMS_USE_RUST=0' );
}

# Test 11: Verify all 5 Rust release binaries exit 0 on --help without blocking
my @binaries = qw(gdresized lms-transcode lms-mp3cut lms-audioscan lms-scanner-accel);
my $help_pass = 1;
for my $b (@binaries) {
    my $exit = run_quiet( "$bin_dir/$b", '--help' );
    if ($exit != 0) {
        $help_pass = 0;
        diag("$b --help exited with $exit");
    }
}
ok( $help_pass, 'All 5 Rust release binaries exit 0 on --help without blocking' );

# Test 12: Slim::Formats::Movie unwraps arrayref YEAR => ['1987', '1987'] to 1987 (Issue #1692)
require Slim::Formats::Movie;

my $movie_tags = { DAY => [ '1987', '1987' ] };
Slim::Formats::Movie->_doTagMapping($movie_tags);
is( $movie_tags->{YEAR}, '1987', 'Slim::Formats::Movie unwraps arrayref YEAR to 1987 (Issue #1692)' );

# Test 13: Slim::Formats::MP3::doTagMapping prioritizes TDRC over TDOR and preserves ORIGINALYEAR (Issue #1650)
require Slim::Formats::MP3;
my $mp3_tags = { TDRC => '2000', TDOR => '1966' };
Slim::Formats::MP3->doTagMapping($mp3_tags);
ok( $mp3_tags->{YEAR} eq '2000' && $mp3_tags->{ORIGINALYEAR} eq '1966', 'Slim::Formats::MP3::doTagMapping prioritizes TDRC over TDOR while preserving ORIGINALYEAR (Issue #1650)' );

# Test 14: Functional check for lms-audioscan on repository silence.mp3
my $silence_mp3 = "$html_root/EN/html/silence.mp3";
my ( $as_rc, $as_out ) = run_capture( "$bin_dir/lms-audioscan", 'scan', $silence_mp3 );
my $as_json = eval { decode_json($as_out) };
ok(
    $as_rc == 0 && ref($as_json) eq 'HASH' && ref( $as_json->{info} ) eq 'HASH' && ( $as_json->{info}{samplerate} || 0 ) > 0,
    'lms-audioscan scan returns valid JSON stream info for silence.mp3'
) or diag("rc=$as_rc err=$@ out=$as_out");

# Test 15 & 16: Functional checks for lms-scanner-accel (symlinked dir URL preservation & non-audio .jpg exclusion)
my $real_album = "$tmp/real_album";
my $scan_root  = "$tmp/scan_root";
mkdir $real_album or die "mkdir $real_album: $!";
mkdir $scan_root  or die "mkdir $scan_root: $!";

open my $track_fh, '>', "$real_album/track.mp3" or die "write track.mp3: $!";
binmode $track_fh;
print $track_fh $frame_data x 2;
close $track_fh;

open my $jpg_fh, '>', "$real_album/cover.jpg" or die "write cover.jpg: $!";
binmode $jpg_fh;
print $jpg_fh "\xFF\xD8\xFF\xD9";
close $jpg_fh;

symlink( $real_album, "$scan_root/linked_album" ) or die "symlink failed: $!";

my $scan_db = "$tmp/library.db";
my $scan_dbh = DBI->connect( "dbi:SQLite:dbname=$scan_db", "", "", { RaiseError => 1 } );
$scan_dbh->do("CREATE TABLE scanned_files (url text NOT NULL COLLATE NOCASE, timestamp int(10), filesize int(10))");
$scan_dbh->disconnect();

my $accel_rc = run_quiet( "$bin_dir/lms-scanner-accel", 'scan-dir', '--db', $scan_db, '--dir', $scan_root );
$scan_dbh = DBI->connect( "dbi:SQLite:dbname=$scan_db", "", "", { RaiseError => 1 } );
my $urls = $scan_dbh->selectcol_arrayref("SELECT url FROM scanned_files");
$scan_dbh->disconnect();

ok(
    $accel_rc == 0 && grep( { m{/linked_album/track\.mp3$} } @$urls ),
    'lms-scanner-accel scan-dir preserves symlinked directory URL in scanned_files'
) or diag( "rc=$accel_rc urls=" . join( ', ', @$urls ) );
ok(
    !grep( { m{\.jpg$}i } @$urls ),
    'lms-scanner-accel scan-dir excludes non-audio .jpg files'
) or diag( "urls=" . join( ', ', @$urls ) );

# Test 17 & 18: Functional checks for lms-transcode (-f 1 WAV vs -f 2 -b 1 raw PCM)
my ( $wav_rc, $wav_out ) = run_capture( "$bin_dir/lms-transcode", '-q', '-w', '-f', '1', '-b', '1', $silence_mp3 );
ok(
    $wav_rc == 0 && length($wav_out) > 44 && substr( $wav_out, 0, 4 ) eq 'RIFF' && substr( $wav_out, 8, 4 ) eq 'WAVE',
    'lms-transcode -f 1 emits valid RIFF/WAVE header'
) or diag( "rc=$wav_rc len=" . length($wav_out) );

my ( $pcm_rc, $pcm_out ) = run_capture( "$bin_dir/lms-transcode", '-q', '-w', '-f', '2', '-b', '1', $silence_mp3 );
ok(
    $pcm_rc == 0 && length($pcm_out) > 0 && substr( $pcm_out, 0, 4 ) ne 'RIFF' && length($wav_out) == length($pcm_out) + 44,
    'lms-transcode -f 2 -b 1 emits raw PCM without RIFF header matching WAV payload size'
) or diag( "rc=$pcm_rc pcm_len=" . length($pcm_out) . " wav_len=" . length($wav_out) );
