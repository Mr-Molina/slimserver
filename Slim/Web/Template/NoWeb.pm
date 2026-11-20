package Slim::Web::Template::NoWeb;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;
use Cwd ();
use FileHandle ();
use File::Spec::Functions qw(catdir);

use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::OSDetect;
use Slim::Utils::Prefs;
use Slim::Utils::Strings;

my $log = logger('network.http');
my $prefs = preferences('server');
my $absolutePathRegex = main::ISWINDOWS ? qr{^(?:/|[a-z]:)}i :  qr{^/};


sub new {
	my $class = shift;

	my $self = {
		templateDirs => [],
		skinTemplates => {},
	};

	bless $self, $class;

	push @{ $self->{templateDirs} }, Slim::Utils::OSDetect::dirsFor('HTML');

	return $self;
}

sub addTemplateDirectory {
	my ($class, $dir) = @_;

	main::INFOLOG && $log->is_info && $log->info("Adding template directory $dir");

	# reset cache
	delete $class->{skinDirs};
	$class->{skinTemplates} = {};

	push @{ $class->{templateDirs} }, $dir if ( not grep({$_ eq $dir} @{ $class->{templateDirs} } ));
}

sub isaSkin {
	my $class = shift;
	my $name  = uc(shift // '');

	return 'Default' if $name eq 'DEFAULT';
	return 'EN'      if $name eq 'EN';
	return 0;
}

sub _generateContentFromFile {
	my ($class, $type, $path, $params) = @_;

	my $skin = $params->{'skinOverride'} || $prefs->get('skin');

	$params->{'thumbSize'} = $prefs->get('thumbSize') unless defined $params->{'thumbSize'};
	$params->{'systemSkin'} = $skin;
	$params->{'systemLanguage'} = $prefs->get('language');
	$params->{'allLinks'} = $prefs->get('additionalPlaylistButtons');

	main::INFOLOG && $log->is_info && $log->info("generating from $path with type: $type");

	if ($type eq 'fill') {

		return $class->_fillTemplate($params, $path, $skin);
	}

	my ($content, $mtime, $inode, $size) = $class->_getFileContent(
		$path,
		$skin,
		1,
		$type eq 'mtime' ? 1 : 0,
		$params->{contentAsFh},
	);

	if ($type eq 'mtime') {

		return ($mtime, $inode, $size);
	}

	# some callers want the mtime for last-modified
	if (wantarray()) {
		return ($content, $mtime, $inode, $size);
	} else {
		return $content;
	}
}

sub _fillTemplate {}

# Retrieves the file specified as $path, relative to the
# INCLUDE_PATH of the given skin.
# Uses binmode to read file if $binary is specified.
# Returns a reference to the file data.

sub _getFileContent {
	my ($class, $path, $skin, $binary, $statOnly, $contentAsFh) = @_;

	my ($content, $template, $mtime, $inode, $size);

	return if !defined $path || $path eq '' || $path =~ /\0|%2e|\.\./i;

	if ( $path !~ $absolutePathRegex  ) {
		# Fixup relative paths according to skin
		$path = $class->fixHttpPath($skin, $path) || return;
	}

	return unless -f $path && -r _;

	main::INFOLOG && $log->is_info && $log->info("Reading http file for ($path)");

	if ( $statOnly ) {
		($inode, $size, $mtime) = (stat(_))[1,7,9];
		return (\$content, $mtime, $inode, $size);
	}

	if ( $contentAsFh ) {
		my $fh = FileHandle->new($path, 'r') || return;
		binmode $fh if $binary;
		return $fh;
	}

	open($template, '<', $path);

	if ($template) {
		($inode, $size, $mtime) = (stat($template))[1,7,9];

		local $/ = undef;
		binmode($template) if $binary;
		$content = <$template>;
		close $template;

		if (!length($content) && $log->is_debug) {

			main::DEBUGLOG && $log->debug("File empty: $path");
		}

	} else {

		logError("Couldn't open: $path");
	}

	return (\$content, $mtime, $inode, $size);
}


# Finds the first occurance of a file specified by $path in the
# list of directories in the INCLUDE_PATH of the specified $skin

sub fixHttpPath {
	my ($class, $skin, $path) = @_;

	return undef if !defined $path || $path eq '' || $path =~ /\0|%2e|\.\./i;

	my $skindirs = $class->_getSkinDirs($skin);

	my $lang = lc($prefs->get('language') // 'en');
	my $sep  = main::ISWINDOWS ? qr{[\\/]} : qr{/};

	for my $dir (@{$skindirs}) {

		my $canonDir = Cwd::abs_path($dir);
		next unless defined $canonDir;
		$canonDir =~ s{[\\/]+$}{};

		my $fullpath = catdir($canonDir, $path);

		# We can have $file.$language files that need to be processed.
		my $langpath = join('.', $fullpath, $lang);
		my $found    = '';

		if ($lang ne 'en' && -f $langpath && -r _) {

			$found = $langpath;

		} elsif (-f $fullpath && -r _) {

			$found = $fullpath;
		}

		# Always canonicalize via abs_path and enforce separator-bounded directory containment
		if ($found) {
			$found = Cwd::abs_path($found);

			if (!$found || $found !~ m{^\Q$canonDir\E$sep} || !-f $found || !-r _) {
				$found = '';
			}
		}

		if ($found) {

			main::INFOLOG && $log->is_info && $log->info("Found path $found");

			return $found;
		}
	}

	main::INFOLOG && $log->is_info && $log->info("Couldn't find path: $path");

	return undef;
}

sub _getSkinDirs {
	my ($class, $skin) = @_;

	$skin = 'Default' unless defined $skin && $skin =~ /^[A-Za-z0-9_-]+$/;
	my $cacheKey = uc($skin);

	return $class->{skinDirs}->{$cacheKey} if $class->{skinDirs} && $class->{skinDirs}->{$cacheKey};

	my %seen;
	my @skins = grep { !$seen{uc($_)}++ } ($skin, 'Default', 'EN');
	my @dirs = ();
	foreach my $s (@skins) {
		foreach ( @{ $class->{templateDirs} } ) {
			push @dirs, catdir($_, $s);
		}
	}

	$class->{skinDirs}->{$cacheKey} = \@dirs;

	return $class->{skinDirs}->{$cacheKey};
}

1;