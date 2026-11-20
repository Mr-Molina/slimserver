package MP3::Cut::Gapless;

use strict;

our $VERSION = '0.03';

use Errno qw(EINTR);
use POSIX qw(WNOHANG);

require XSLoader;
XSLoader::load('MP3::Cut::Gapless', $VERSION);
{
    no warnings 'redefine';
    *__xs_read = \&read;
    *read = \&_perl_read;
}

sub _validate_no_nul {
    my ( $val, $label ) = @_;
    return unless defined $val;
    if ( $val =~ /\0/ ) {
        die "Invalid $label: NUL byte not allowed\n";
    }
}

sub _validate_ms {
    my ( $val, $label ) = @_;
    return undef unless defined $val;
    if ( ref($val) || $val !~ /\A[0-9]+\z/ ) {
        die "Invalid $label: must be a non-negative integer\n";
    }
    return 0 + $val;
}

sub _sanitize_filename_part {
    my ($part) = @_;
    return '' unless defined $part;
    my $clean = "$part";
    $clean =~ s/[\x00-\x1f\x7f]+//g;
    $clean =~ s{[/\\]+}{}g;
    1 while $clean =~ s/\.\.//g;
    $clean =~ s/^\s+|\s+$//g;
    return $clean;
}

sub _sanitize_default_filename {
    my ($track) = @_;
    my @parts = grep { length($_) > 0 } map { _sanitize_filename_part($_) } (
        $track->{position},
        $track->{performer},
        $track->{title},
    );
    my $base = join( ' - ', @parts );
    $base =~ s/[\x00-\x1f\x7f]+//g;
    $base =~ s{[/\\]+}{}g;
    1 while $base =~ s/\.\.//g;
    $base =~ s/^\.+//;
    $base =~ s/^\s+|\s+$//g;
    $base = 'track' unless length($base);
    return "$base.mp3";
}

sub new {
    my ( $class, %args ) = @_;

    if ( !$args{file} && !$args{cue} ) {
        die "Either file or cue argument must be specified";
    }

    _validate_no_nul( $args{file},      'file' );
    _validate_no_nul( $args{cue},       'cue' );
    _validate_no_nul( $args{cache_dir}, 'cache_dir' );

    if ( exists $args{start_ms} && defined $args{start_ms} ) {
        $args{start_ms} = _validate_ms( $args{start_ms}, 'start_ms' );
    }
    if ( exists $args{end_ms} && defined $args{end_ms} ) {
        $args{end_ms} = _validate_ms( $args{end_ms}, 'end_ms' );
    }
    if ( defined $args{start_ms} && defined $args{end_ms} && $args{start_ms} > $args{end_ms} ) {
        die "Invalid range: start_ms ($args{start_ms}) exceeds end_ms ($args{end_ms})\n";
    }

    my $self = bless \%args, $class;

    if ( $self->{cue} ) {
        $self->_parse_cue;
    }

    _validate_no_nul( $self->{file}, 'file' );

    if ( !defined $self->{file} || $self->{file} eq '' || !-f $self->{file} ) {
        die "Invalid file: " . ( defined $self->{file} ? $self->{file} : 'undef' );
    }

    if ( defined $self->{cache_dir} && $self->{cache_dir} ne '' ) {
        require Digest::MD5;
        require File::Spec;

        if ( !-d $self->{cache_dir} ) {
            require File::Path;
            File::Path::mkpath( $self->{cache_dir} );
        }

        # Cache key is filename + size + mtime with explicit delimiters so if the
        # file changes or boundaries shift we will not read a stale cache file.
        my @st = stat $self->{file};
        die "Unable to stat $self->{file}: $!" unless @st;
        my ($size, $mtime) = @st[7, 9];
        my $key_file = $self->{file};
        utf8::encode($key_file) if utf8::is_utf8($key_file);
        $self->{cache_file} = File::Spec->catfile(
            $self->{cache_dir},
            Digest::MD5::md5_hex( join( "\0", $key_file, $size, $mtime ) ) . '.mllt'
        );
    }

    # Pre-scan / open the stream for the requested range (or full file)
    $self->_init;

    return $self;
}

sub tracks {
    my $self = shift;

    return @{ $self->{_tracks} || [] };
}

sub write {
    my ( $self, $track, $filename ) = @_;

    if ( !ref($track) ) {
        die "Invalid track argument to write()\n";
    }

    if ( !defined $filename || $filename eq '' ) {
        # Default filename is "position - performer - title.mp3"
        $filename = _sanitize_default_filename($track);
    }
    else {
        _validate_no_nul( $filename, 'filename' );
    }

    if ( -e $filename ) {
        warn "$filename already exists, will not overwrite\n";
        return;
    }

    my $start_ms = defined $track->{start_ms} ? _validate_ms( $track->{start_ms}, 'track start_ms' ) : undef;
    my $end_ms   = defined $track->{end_ms}   ? _validate_ms( $track->{end_ms},   'track end_ms' )   : undef;
    if ( defined $start_ms && defined $end_ms && $start_ms > $end_ms ) {
        die "Invalid track range: start_ms ($start_ms) exceeds end_ms ($end_ms)\n";
    }

    # Reset XS counter for read()
    $self->__reset_read() if exists $self->{_mp3c};

    delete $self->{start_ms};
    delete $self->{end_ms};

    $self->{start_ms} = $start_ms if defined $start_ms;
    $self->{end_ms}   = $end_ms   if defined $end_ms;

    if ( !exists $self->{_mp3c} ) {
        $self->_close_rust_pipe;
        $self->_init;
    }

    print "Writing $filename...\n";

    open my $fh, '>', $filename or die "Unable to open $filename for writing: $!\n";
    binmode $fh or do {
        my $err = $!;
        close $fh;
        die "Unable to set binmode on $filename: $err\n";
    };

    while (1) {
        my $n = $self->read( my $buf, 65536 );
        if ( !defined $n ) {
            close $fh;
            die "Error reading MP3 stream while writing $filename\n";
        }
        last if $n == 0;

        my $offset = 0;
        my $len    = length($buf);
        while ( $offset < $len ) {
            my $written = syswrite $fh, $buf, $len - $offset, $offset;
            if ( !defined $written ) {
                next if $!{EINTR};
                my $err = $!;
                close $fh;
                die "Error writing to $filename: $err\n";
            }
            $offset += $written;
        }
    }

    close $fh or die "Error closing $filename: $!\n";
}

sub _rust_bin {
    return undef unless ($ENV{LMS_USE_RUST} // 1);
    return undef unless $^O eq 'linux';
    require Config;
    require FindBin;
    require File::Basename;
    require File::Spec;
    require Cwd;

    my $archname = $Config::Config{archname} || '';
    my $arch = $archname =~ /^(?:aarch64|arm64)(?:-|$)/i ? 'aarch64-linux'
             : $archname =~ /^(?:x86_64|amd64)(?:-|$)/i  ? 'x86_64-linux'
             : return undef;

    my @seeds;
    for my $candidate ( $FindBin::RealBin, $FindBin::Bin ) {
        push @seeds, $candidate if defined $candidate && $candidate ne '';
    }
    my $self_dir = File::Basename::dirname( Cwd::abs_path(__FILE__) || File::Spec->rel2abs(__FILE__) );
    push @seeds, $self_dir if defined $self_dir && $self_dir ne '';

    my %seen;
    my @candidate_dirs;
    for my $seed (@seeds) {
        my $dir = Cwd::abs_path($seed) || File::Spec->rel2abs($seed);
        for ( 1 .. 8 ) {
            last unless defined $dir && $dir ne '';
            my $bin_dir = File::Spec->catdir( $dir, 'Bin', $arch );
            if ( !$seen{$bin_dir}++ ) {
                push @candidate_dirs, $bin_dir;
            }
            my $parent = File::Basename::dirname($dir);
            last if !defined $parent || $parent eq $dir;
            $dir = $parent;
        }
    }

    my $sys_dir = "/usr/share/squeezeboxserver/Bin/$arch";
    push @candidate_dirs, $sys_dir unless $seen{$sys_dir}++;

    for my $dir (@candidate_dirs) {
        my $bin = File::Spec->catfile( $dir, 'lms-mp3cut' );
        return $bin if -f $bin && -x _;
    }
    return undef;
}

sub _close_rust_pipe {
    my $self = shift;
    my $rfh  = delete $self->{_rust_fh};
    my $pid  = delete $self->{_rust_pid};
    return 1 unless $rfh;

    if ( defined $pid && $pid > 0 ) {
        my $reaped = 0;
        for ( 1 .. 5 ) {
            $reaped = waitpid( $pid, WNOHANG );
            if ( $reaped == -1 && $!{EINTR} ) {
                next;
            }
            last if $reaped != 0;
            select( undef, undef, undef, 0.01 );
        }
        if ( $reaped == 0 ) {
            kill 'TERM', $pid;
            for ( 1 .. 20 ) {
                $reaped = waitpid( $pid, WNOHANG );
                if ( $reaped == -1 && $!{EINTR} ) {
                    next;
                }
                last if $reaped != 0;
                select( undef, undef, undef, 0.025 );
            }
        }
        if ( $reaped == 0 ) {
            kill 'KILL', $pid;
            for ( 1 .. 10 ) {
                $reaped = waitpid( $pid, WNOHANG );
                if ( $reaped == -1 && $!{EINTR} ) {
                    next;
                }
                last if $reaped != 0;
                select( undef, undef, undef, 0.01 );
            }
        }
    }

    my $close_ok = close($rfh);
    return $close_ok;
}

sub _perl_read {
    my $self = shift;
    if ( $self->{_rust_fh} ) {
        my $len = $_[1] || 32768;
        my $n;
        while (1) {
            $n = CORE::read( $self->{_rust_fh}, $_[0], $len );
            last if defined $n;
            next if $!{EINTR};
            last;
        }
        if ( defined $n && $n > 0 ) {
            $self->{_rust_bytes_read} += $n;
            return $n;
        }

        my $rfh       = delete $self->{_rust_fh};
        my $pid       = delete $self->{_rust_pid};
        my $close_ok  = close($rfh);
        my $child_err = $?;

        if ( !$close_ok && defined $pid && $pid > 0 && $!{EINTR} ) {
            for ( 1 .. 10 ) {
                my $r = waitpid( $pid, WNOHANG );
                if ( $r == $pid ) {
                    $child_err = $?;
                    $close_ok  = 1;
                    last;
                }
                last if $r == -1 && !$!{EINTR};
                select( undef, undef, undef, 0.01 );
            }
        }

        if ( !$self->{_rust_bytes_read} ) {
            if ( !$close_ok || $child_err != 0 || !defined $n ) {
                warn "lms-mp3cut failed (exit=" . ( $child_err >> 8 ) . ", sig=" . ( $child_err & 127 ) . "); falling back to XS\n";
            }
            else {
                warn "lms-mp3cut returned unexpected empty stream; falling back to XS\n";
            }
            eval { $self->_init_xs; 1 } or do {
                warn "MP3::Cut::Gapless XS fallback failed: $@";
                return undef;
            };
            return $self->__xs_read(@_);
        }

        if ( !$close_ok || $child_err != 0 || !defined $n ) {
            warn "lms-mp3cut failed mid-stream (exit=" . ( $child_err >> 8 ) . ", sig=" . ( $child_err & 127 ) . ")\n";
            return undef;
        }

        return 0;
    }
    return $self->__xs_read(@_);
}

sub _init_xs {
    my $self = shift;

    open my $fh, '<', $self->{file} or die "Unable to open $self->{file} for reading: $!";
    binmode $fh or do {
        my $err = $!;
        close $fh;
        die "Unable to set binmode on $self->{file}: $err";
    };
    $self->{_fh} = $fh;

    my $mp3c = eval { $self->__init(@_) };
    if ( $@ || !defined $mp3c ) {
        my $err = $@ || "XS __init failed";
        close delete $self->{_fh} if $self->{_fh};
        die $err;
    }
    $self->{_mp3c} = $mp3c;
}

sub _init {
    my $self = shift;

    delete $self->{_rust_bytes_read};
    delete $self->{_rust_pid};

    if ( my $bin = _rust_bin() ) {
        my @cmd = ( $bin, '--file=' . $self->{file} );
        push @cmd, ( '--cache-dir=' . $self->{cache_dir} ) if defined $self->{cache_dir} && $self->{cache_dir} ne '';
        push @cmd, ( '--start-ms', $self->{start_ms} ) if defined $self->{start_ms};
        push @cmd, ( '--end-ms',   $self->{end_ms} )   if defined $self->{end_ms};
        my $pid = open my $rfh, '-|', @cmd;
        if ( defined $pid ) {
            if ( binmode $rfh ) {
                $self->{_rust_fh}         = $rfh;
                $self->{_rust_pid}        = $pid;
                $self->{_rust_bytes_read} = 0;
                return;
            }
            $self->{_rust_fh}  = $rfh;
            $self->{_rust_pid} = $pid;
            $self->_close_rust_pipe;
        }
    }

    $self->_init_xs(@_);
}

sub _parse_cue {
    my $self = shift;

    require Audio::Cuefile::Parser;
    require MP3::Cut::Gapless::Track;
    require File::Spec;

    my $cue = Audio::Cuefile::Parser->new( $self->{cue} );

    if ( !$self->{file} ) {
        $self->{file} = $cue->file || die "No FILE entry found in cue sheet";

        # Handle relative path
        my ($vol, $dirs, $file) = File::Spec->splitpath( $self->{file} );
        if ( $dirs !~ m{^[/\\]} ) {
            my ($cvol, $cdirs, undef) = File::Spec->splitpath( File::Spec->rel2abs( $self->{cue} ) );
            $self->{file} = File::Spec->rel2abs( $self->{file}, File::Spec->catdir($cvol, $cdirs) );
        }
    }

    $self->{_tracks} = [];
    for my $track ( $cue->tracks ) {
        push @{ $self->{_tracks} }, MP3::Cut::Gapless::Track->new($track);
    }

    # Set end_ms values, last track will fill to the end
    for ( my $i = 0; $i < scalar @{ $self->{_tracks} } - 1; $i++ ) {
        $self->{_tracks}->[$i]->{end_ms} = $self->{_tracks}->[$i + 1]->{start_ms};
    }
}

sub DESTROY {
    my $self = shift;
    local ( $?, $!, $@ );

    $self->_close_rust_pipe;

    if ( my $fh = delete $self->{_fh} ) {
        close $fh;
    }

    $self->__cleanup( delete $self->{_mp3c} ) if exists $self->{_mp3c};
}

1;
__END__

=head1 NAME

MP3::Cut::Gapless - Split an MP3 file without gaps (based on pcutmp3)

=head1 SYNOPSIS

    use MP3::Cut::Gapless;
    
    # Cut file using a cue sheet
    my $cut = MP3::Cut::Gapless->new(
        cue => 'file.cue'
    );
    for my $track ( $cut->tracks ) {
        $cut->write( $track );
    }
    
    # Or, cut at defined points and stream the rewritten file
    my $cut = MP3::Cut::Gapless->new(
        file      => 'long.mp3',
        cache_dir => '/var/cache/mp3cut',
        start_ms  => 15000,
        end_ms    => 30000,
    );
    open my $out, '>', '15-30.mp3';
    while ( $cut->read( my $buf, 4096 ) ) {
        syswrite $out, $buf;
    }
    close $out;

=head1 DESCRIPTION

This module performs sample-granular splitting of an MP3 file.  Most MP3 splitters only split
on frame boundaries which can leave gaps or noise between files due to MP3's bit reservoir.
This module, which is based on the Java pcutmp3 tool, rewrites the LAME tag and adjusts the
audio frames as necessary to make the split completely gapless when played with a compatible
decoder that supports LAME encoder delay and padding.

There are two main ways to use this module.

1. Using a cue sheet, you can split a large input file into a series of tracks.

2. Realtime transcoding can be performed by specifying an input file and a start/end time,
after which the rewritten MP3 can be streamed to an audio device.

=head1 METHODS

=head2 new( %args )

Arguments:

=over 4

    cue => $cue_sheet

Optional. Cue sheet to use to determine cut points.

    file => $file_to_cut

If not specified, the FILE entry from the cue will be used.

    start_ms => $start_time_in_milliseconds
    end_ms   => $end_time_in_milliseconds

Optional, can be used to manually define cut points.

    cache_dir => "/path/to/cache/dir"

Optional. When new() is called, a complete scan must be done of the
MP3 file to determine the number of frames and their locations. This 
can be somewhat time-consuming depending on the size of the file,
disk/network speed, and so on. A cache file can be created to avoid this
operation if the file needs to be cut a second time. This is most useful
when manually cutting a file several times.

=back

=head2 tracks()

Returns an array of L<MP3::Cut::Gapless::Track> objects. Only available if a cue sheet was used.

=head2 write( $track, [ $filename ] )

Write the given L<MP3::Cut::Gapless::Track> object to a file. If no filename is provided, the
default filename is "<position> - <performer> - <title>.mp3". If the file already exists, it
will not be overwritten.

=head2 read( $buf, $block_size_hint )

Reads a chunk of the rewritten MP3 file into $buf. Returns the number of bytes read.
Only complete MP3 frames are returned, so you may receive more or less data than $block_size_hint.

=head1 SEE ALSO

pcutmp3 originally by Sebastian Gesemann L<http://wiki.themixingbowl.org/Pcutmp3>

L<Audio::Scan>

=head1 AUTHOR

Andy Grundman, E<lt>andy@slimdevices.comE<gt>

=head1 COPYRIGHT AND LICENSE

Copyright (C) 2010 Logitech, Inc.

This program is free software; you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation; either version 2 of the License, or
(at your option) any later version.

=cut
