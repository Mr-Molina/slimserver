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
use Test::More tests => 7;

my $root = "$Bin/..";
my $bin_dir = "$root/Bin/x86_64-linux";

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
my $img_path = -f "$root/HTML/EN/html/images/b_delete.gif"
    ? "$root/HTML/EN/html/images/b_delete.gif"
    : "$root/HTML/Classic/html/images/b_delete.gif";
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
