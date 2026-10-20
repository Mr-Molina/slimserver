#!/usr/bin/perl
use strict;
use warnings;

use FindBin qw($Bin);
use lib "$Bin/..";
use File::Temp qw(tempdir);
use IO::Socket::UNIX;
use MIME::Base64 qw(encode_base64);
use Digest::MD5 qw(md5_hex);
use DBI;
use constant RESIZER => 1;
use Test::More tests => 9;

use Config;

my $root = "$Bin/..";
my $arch = ($Config{archname} || '') =~ /aarch64/ ? 'aarch64-linux' : 'x86_64-linux';
my $bin_dir = "$root/Bin/$arch";
if ( !-d $bin_dir && -d "/usr/share/squeezeboxserver/Bin/$arch" ) {
    $bin_dir = "/usr/share/squeezeboxserver/Bin/$arch";
}

my $html_root = -d "$root/HTML" ? "$root/HTML" : "/usr/share/squeezeboxserver/HTML";

ok( -x "$bin_dir/gdresized",         "gdresized Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-transcode",     "lms-transcode Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-mp3cut",        "lms-mp3cut Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-audioscan",     "lms-audioscan Rust binary exists and is executable" );
ok( -x "$bin_dir/lms-scanner-accel", "lms-scanner-accel Rust binary exists and is executable" );

my $tmp = tempdir( CLEANUP => 1 );
my $sock_path = "$tmp/sbs_artwork_test";

my $pid = fork();
if ( $pid == 0 ) {
    exec( "$bin_dir/gdresized", "--socket", $sock_path, "--max-requests", "1" );
    exit 1;
}

for ( 1 .. 50 ) {
    last if -S $sock_path;
    select( undef, undef, undef, 0.05 );
}

require Image::Scale;
require Slim::Utils::GDResizer;
my $img_path = -f "$html_root/EN/html/images/b_delete.gif"
    ? "$html_root/EN/html/images/b_delete.gif"
    : (-f "$html_root/Classic/html/images/b_delete.gif"
        ? "$html_root/Classic/html/images/b_delete.gif"
        : "/usr/share/squeezeboxserver/HTML/Classic/html/images/b_delete.gif");
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
waitpid( $pid, 0 );

is( $resp, "OK\015\012", "Rust gdresized daemon processed UNIX socket frame and returned OK" );

# Verify Perl DbCache MD5 decimal key and A3LS header read-back from SQLite artwork.db
my $md5 = md5_hex($cachekey);
my $db_key = hex( substr( $md5, 0, 8 ) ) . hex( substr( $md5, 8, 7 ) );
my $dbh = DBI->connect( "dbi:SQLite:dbname=$tmp/artwork.db", "", "", { RaiseError => 1 } );
my ($blob) = $dbh->selectrow_array( "SELECT v FROM cache WHERE k = ?", undef, $db_key );
$dbh->disconnect();

my ($ct, $mtime, $pathlen) = unpack( 'A3LS', $blob );
my $img_data = substr( $blob, 9 + $pathlen );
is( $ct, 'jpg', "SQLite artwork.db row matches Perl DbCache key and A3LS jpg header" );

# Test 8 & 9: Verify MP3::Cut::Gapless delegation to Rust and C XS fallback
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
