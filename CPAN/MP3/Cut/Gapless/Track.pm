package MP3::Cut::Gapless::Track;

use strict;

sub new {
    my $class = shift;
    my $track = shift;

    if ( !defined $track || !eval { $track->isa('Audio::Cuefile::Parser::Track') } ) {
        die "Invalid track: expected Audio::Cuefile::Parser::Track object\n";
    }

    my $self = {
        index     => $track->index,
        start_ms  => _parseMSF( $track->index ),
        performer => $track->performer,
        position  => $track->position,
        title     => $track->title,
    };

    bless $self, $class;

    return $self;
}

sub position { $_[0]->{position} }
sub performer { $_[0]->{performer} }
sub title { $_[0]->{title} }
sub index { $_[0]->{index} }
sub start_ms { $_[0]->{start_ms} }
sub end_ms { $_[0]->{end_ms} }

sub _parseMSF {
    my $msf = shift;

    if ( !defined $msf || ref($msf) || $msf !~ /\A\s*([0-9]+):([0-9]{1,2}):([0-9]{1,2})\s*\z/ ) {
        die "Invalid MSF timestamp: " . ( defined $msf ? $msf : 'undef' ) . "\n";
    }

    my ( $min, $sec, $frm ) = ( 0 + $1, 0 + $2, 0 + $3 );
    if ( $sec >= 60 || $frm >= 75 ) {
        die "Out-of-range MSF timestamp: $msf\n";
    }

    return int( ((60 * $min) + $sec + ($frm / 75)) * 1000 + 0.5 );
}

1;
__END__

=head1 NAME

MP3::Cut::Gapless::Track - A track within a cue sheet

=head1 SYNOPSIS

    # Cut file using a cue sheet
    my $cut = MP3::Cut::Gapless->new(
        cue => 'file.cue'
    );
    for my $track ( $cut->tracks ) {
        $cut->write( $track, $track->position . '.mp3' );
    }
    
=head1 DESCRIPTION

This is a lightweight object representing a track within a cue sheet.

=head1 METHODS

=head2 new( Audio::Cuefile::Parser::Track object )

new() takes an L<Audio::Cuefile::Parser::Track> object, and is not designed
to be called directly.

=head2 position()

The position of the track in the cue sheet, for example "01".

=head2 performer()

The performer of the track.

=head2 title()

The title of the track.

=head2 index()

The index of the track, in MM:SS:FF notation.

=head2 start_ms()

The start of the track in milliseconds.

=head2 end_ms()

The end of the track in milliseconds.  This value will be undef for the last
track in the cue sheet, which is assumed to extend to the end of the track.

=head1 AUTHOR

Andy Grundman, E<lt>andy@slimdevices.comE<gt>

=head1 COPYRIGHT AND LICENSE

Copyright (C) 2010 Logitech, Inc.

This program is free software; you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation; either version 2 of the License, or
(at your option) any later version.

=cut
