package Slim::Web::ImageProxy;

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

=head1 NAME

Slim::Web::ImageProxy

=head1 SYNOPSIS

	use Slim::Web::ImageProxy;

	# get an artwork file's url, including potential proxy path
	my $resize_url = Slim::Web::ImageProxy::proxiedImage($songData->{artwork_url});

	# register a custom handler to define an image's URL based on some URL
	Slim::Web::ImageProxy->registerHandler(
		match => qr/someradio\.com\/graphics\/covers\/[sml]\/.*/,
		func  => sub {
			my ($url, $spec) = @_;

			# figure out what minimum file size we'll need to satisfy the resizing spec
			my $size = Slim::Web::ImageProxy->getRightSize($spec, {
				70  => 's',
				160 => 'm',
				300 => 'l',
			}) || 'l';
			$url =~ s/\/[sml]\//\/$size\//;

			return $url;
		},
	);

	# register an external image proxy to be used for all external images
	# using an external resizing proxy can improve performance on slow LMS systems
	Slim::Web::ImageProxy->registerHandler(
		id => 'someExternalImageProxy',
		func  => sub {
			my ($url, $spec) = @_;

			# parse the resizing parameters
			my ($width, $height, $mode, $bgcolor, $ext) = Slim::Web::Graphics->parseSpec($spec);

			# return the full URL to your resizing service
			return sprintf(
				'https://www.yourdomain.com/imageresizer?url=%s&width=%s&height=%s',
				uri_escape_utf8($url),
				$width,
				$height
			);
		},
		external => 1,
		desc => 'Some fast external image proxy'
	);


=head1 DESCRIPTION

The ImageProxy module allows you to have artwork resized on the server-side without relying on external systems.

Besides resizing artwork, custom handlers can eg. convert track IDs to image URLs, build a query string to request
the smallest image possible for the given resizing spec etc.

By registering an external image proxy you can off-load the downloading and resizing of potentially large external
artwork to some other machine. This can be considerably faster if you're running LMS on a slow system, or if the
original artwork is of considerable size, where the bandwidth to download the image would be the bottleneck.

=cut

use strict;
use Cwd ();
use Image::Scale;
use Socket qw(AF_INET AF_INET6 SOCK_STREAM getaddrinfo unpack_sockaddr_in unpack_sockaddr_in6);
use Tie::RegexpHash;
use URI ();
use URI::Escape qw(uri_escape_utf8);

use Exporter::Lite;
our @EXPORT_OK = qw(proxiedImage);

use Slim::Networking::SimpleAsyncHTTP;
use Slim::Utils::ImageResizer;
use Slim::Utils::Misc;
use Slim::Utils::Log;
use Slim::Utils::Prefs;
use Slim::Web::Graphics;

tie my %handlers, 'Tie::RegexpHash';
my %externalHandlers;

use constant ONE_YEAR => 86400 * 365;
use constant MAX_FETCH_BYTES => 16 * 1024 * 1024;
use constant MAX_REDIRECTS   => 5;
use constant ACCEPT_IMAGE_FORMATS => 'image/jpeg,image/png;q=0.9' . (Image::Scale->gif_version() ? ',image/gif;q=0.1' : '');
use constant REDIRECT_IMAGE_TO_COMPATIBLE => 'https://api.lms-community.org/img/compatible/';

my $log   = logger('artwork.imageproxy');
my $prefs = preferences('server');
my $cache;

my %queue;

sub _isBlockedIpv4 {
	my ($ip4) = @_;
	return 1 unless defined $ip4 && length($ip4) == 4;

	my @b = unpack('C4', $ip4);
	return 1 if $b[0] == 0;                                    # 0.0.0.0/8 (unspecified)
	return 1 if $b[0] == 10;                                   # 10.0.0.0/8 (RFC1918)
	return 1 if $b[0] == 100 && $b[1] >= 64 && $b[1] <= 127;   # 100.64.0.0/10 (CGNAT)
	return 1 if $b[0] == 127;                                  # 127.0.0.0/8 (loopback)
	return 1 if $b[0] == 169 && $b[1] == 254;                  # 169.254.0.0/16 (link-local)
	return 1 if $b[0] == 172 && $b[1] >= 16 && $b[1] <= 31;    # 172.16.0.0/12 (RFC1918)
	return 1 if $b[0] == 192 && $b[1] == 168;                  # 192.168.0.0/16 (RFC1918)
	return 1 if $b[0] >= 224;                                  # 224.0.0.0/4 (multicast) & 240.0.0.0/4 (reserved)

	return 0;
}

sub _isBlockedIpv6 {
	my ($ip6) = @_;
	return 1 unless defined $ip6 && length($ip6) == 16;

	return 1 if $ip6 eq ("\x00" x 16);                                       # ::/128 (unspecified)
	return 1 if $ip6 eq (("\x00" x 15) . "\x01");                            # ::1/128 (loopback)
	return 1 if substr($ip6, 0, 12) eq (("\x00" x 10) . "\xff\xff");         # ::ffff:0:0/96 (IPv4-mapped)
	if (substr($ip6, 0, 12) eq ("\x00" x 12)) {                              # ::/96 (IPv4-compatible)
		return 1 if _isBlockedIpv4(substr($ip6, 12, 4));
	}

	my $b0 = ord(substr($ip6, 0, 1));
	my $b1 = ord(substr($ip6, 1, 1));
	return 1 if ($b0 & 0xfe) == 0xfc;                                        # fc00::/7 (ULA)
	return 1 if $b0 == 0xfe && ($b1 & 0xc0) == 0x80;                         # fe80::/10 (link-local)
	return 1 if $b0 == 0xff;                                                 # ff00::/8 (multicast)

	return 0;
}

sub _validateExternalHttpUrl {
	my $url = (@_ > 1 && defined $_[0] && $_[0] eq __PACKAGE__) ? $_[1] : $_[0];

	return 0 unless defined $url && !ref($url) && length($url);
	return 0 if $url =~ /[\r\n\0\x00-\x1f\x7f]/;
	return 0 unless $url =~ m{^https?://}i;

	my $uri = eval { URI->new($url) };
	return 0 unless $uri;

	my $scheme = lc($uri->scheme // '');
	return 0 unless $scheme eq 'http' || $scheme eq 'https';

	my $auth = eval { $uri->authority };
	return 0 unless defined $auth && length($auth);
	return 0 if defined($uri->userinfo) || $auth =~ /\@/;

	my $host = eval { $uri->host };
	return 0 unless defined $host && length($host);
	$host =~ s/^\[(.*)\]$/$1/;
	return 0 if $host =~ /^(?:localhost(?:\.localdomain)?|.*\.localhost)\.?$/i;

	my ($err, @res) = getaddrinfo($host, undef, { socktype => SOCK_STREAM });
	return 0 if $err || !@res;

	for my $r (@res) {
		if ($r->{family} == AF_INET) {
			my (undef, $ip4) = unpack_sockaddr_in($r->{addr});
			return 0 if _isBlockedIpv4($ip4);
		}
		elsif ($r->{family} == AF_INET6) {
			my (undef, $ip6) = unpack_sockaddr_in6($r->{addr});
			return 0 if _isBlockedIpv6($ip6);
		}
		else {
			return 0;
		}
	}

	return 1;
}

sub init {
	$cache ||= Slim::Web::ImageProxy::Cache->new();

	Slim::Utils::ImageResizer->initDaemon();

	# clean up  stale cache files
	Slim::Utils::Misc::deleteFiles($prefs->get('cachedir'), qr/^imgproxy_[a-f0-9]+$/i);
}

sub getImage {
	my ($class, $client, $path, $params, $callback, $spec, @args) = @_;

	main::DEBUGLOG && $log->debug("Get artwork for URL: " . (defined $path ? $path : '<undef>'));

	if ( !defined $path || $path =~ /[\r\n\0]/ ) {
		main::INFOLOG && $log->info("Artwork ID not found, returning 404");

		_artworkError( $client, $params, $spec, 404, $callback, @args );
		return;
	}

	# check the cache for this url
	$cache ||= Slim::Web::ImageProxy::Cache->new();

	my $path2;

	# some clients require the .png ending, but we don't store it in the cache - try them both
	if ($path =~ /(.*)\.png$/i) {
		$path2 = $1;
	}
	elsif ($path !~ /\.(?:jpe?g|gif|bmp)$/i) {
		$path2 = "$path.png";
	}

	if ( my $cached = $cache->get($path) || ($path2 && $cache->get($path2)) ) {
		main::DEBUGLOG && $log->is_debug && $log->debug( 'Got cached artwork of type ' . $cached->{content_type} . ' and ' . length(${$cached->{data_ref}}) . ' bytes length' );

		_setHeaders($args[1], $cached->{content_type});

		$callback && $callback->( $client, $params, $cached->{data_ref}, @args );

		return;
	}

	my ($url) = $path =~ m|imageproxy/(.*)/[^/]*|s;

	if ( !$url || $url =~ /[\r\n\0]/ ) {
		main::INFOLOG && $log->info("Artwork ID not found, returning 404");

		_artworkError( $client, $params, $spec, 404, $callback, @args );
		return;
	}

	my $handler = $class->getHandlerFor($url);
	if (!$handler && $url =~ /\.svg(?:[?#].*)?$/is) {
		main::INFOLOG && $log->is_info && $log->info("Blocking SVG image proxy request: $url");

		_artworkError( $client, $params, $spec, 404, $callback, @args );
		return;
	}

	my $handleProxiedUrl = sub {
		my ($url, $from_handler) = @_;

		if ( !$url || $url =~ /[\r\n\0]/ || $url !~ /^(?:file|https?):/i ) {
			main::INFOLOG && $log->info("No artwork found, returning 404");

			_artworkError( $client, $params, $spec, 404, $callback, @args );
			return;
		}

		if ( $url =~ /^file:/i && !$from_handler ) {
			$log->warn("Denied direct file: URI request in ImageProxy: $url");

			_artworkError( $client, $params, $spec, 403, $callback, @args );
			return;
		}

		main::DEBUGLOG && $log->debug("Found URL to get artwork: $url");

		my $pre_shrunk;
		my %headers = (
			'Accept'     => ACCEPT_IMAGE_FORMATS,
			'User-Agent' => Slim::Utils::Misc::userAgentString('legacy')
		);

		if ( $url =~ /^https?:/i ) {
			if ( !_validateExternalHttpUrl($url) ) {
				$log->warn("Blocked disallowed ImageProxy URL: $url");
				_artworkError( $client, $params, $spec, 403, $callback, @args );
				return;
			}

			# use external image proxy if one is defined
			if ( $spec && $spec !~ /^\.(png|jpe?g)/i && (my $imageproxy = $prefs->get('useLocalImageproxy')) ) {
				if ( my $external = $externalHandlers{$imageproxy} ) {
					if ( $external->{func} ) {
						my ($url2, $header) = $external->{func}->(uri_escape_utf8($url), $spec);

						if ($url2) {
							if ( !_validateExternalHttpUrl($url2) ) {
								$log->warn("Blocked disallowed external image proxy URL: $url2");
								_artworkError( $client, $params, $spec, 403, $callback, @args );
								return;
							}
							$url = $url2;

							# parse header and add it to the request
							if ($header && $header !~ /[\r\n\0]/ && $header =~ /(.*?):\s*(.*)/) {
								$headers{$1} = $2;
							}
						}
						$pre_shrunk = 1;

						main::DEBUGLOG && $log->debug("Using custom image proxy: $url");
					}
				}
			}

			if (!$pre_shrunk && $url =~ /^https?:.*\.webp(?:$|\?)/i) {
				main::DEBUGLOG && $log->debug("Redirect WEBP images to JPEG conversion service");
				$url = urlToCloudResizer($url);
				%headers = (%headers, Slim::Utils::Misc::apiHeaders(__PACKAGE__));
				if ( !_validateExternalHttpUrl($url) ) {
					$log->warn("Blocked disallowed cloud resizer URL: $url");
					_artworkError( $client, $params, $spec, 403, $callback, @args );
					return;
				}
			}
		}

		$queue{$url} ||= [];

		# we're going to queue up requests, so we don't need to download
		# the same file multiple times due to a race condition
		push @{ $queue{$url} }, {
			client   => $client,
			cachekey => $path,
			params   => $params,
			callback => $callback,
			spec     => $spec,
			args     => \@args,
			pre_shrunk => $pre_shrunk,
		};

		if ( $url =~ /^file:/i ) {
			my $file_path = Slim::Utils::Misc::pathFromFileURL($url);
			_resizeFromFile($url, $file_path);
		}
		elsif ( $url =~ /^https?:/i ) {
			# no need to do the http request if we're already fetching it
			return if scalar @{ $queue{$url} } > 1;

			_fetchExternalUrl($url, $url, undef, 0, \%headers);
		}
	};

	# some plugin might have registered to deal with this image URL
	if ($handler) {
		$url = $handler->($url, $spec, sub {
			$handleProxiedUrl->($_[0], 1);
		});
		return unless defined $url;
		$handleProxiedUrl->($url, 1);
		return;
	}

	$handleProxiedUrl->($url, 0);
}

sub _fetchExternalUrl {
	my ($fetch_url, $queue_url, $original_url, $redirect_count, $headers_ref) = @_;

	if ( !_validateExternalHttpUrl($fetch_url) ) {
		$log->warn("Blocked disallowed ImageProxy HTTP URL: " . (defined $fetch_url ? $fetch_url : '<undef>'));
		_drainQueueError($queue_url, 403);
		return;
	}

	my %headers = $headers_ref ? %$headers_ref : ();
	Slim::Networking::SimpleAsyncHTTP->new(
		\&_gotArtwork,
		\&_gotArtworkError,
		{
			timeout        => 30,
			cache          => 1,
			maxRedirect    => 0,
			queueUrl       => $queue_url,
			originalUrl    => $original_url,
			redirectCount  => $redirect_count || 0,
			requestHeaders => \%headers,
		},
	)->get( $fetch_url, %headers );
}

sub _drainQueueError {
	my ($queue_url, $error) = @_;

	while ( my $item = shift @{ $queue{$queue_url} } ) {
		my $client   = $item->{client};
		my $spec     = $item->{spec};
		my $args     = $item->{args};
		my $params   = $item->{params};
		my $callback = $item->{callback};

		_artworkError( $client, $params, $spec, $error, $callback, @$args );
	}

	delete $queue{$queue_url};
}

sub _handleRedirect {
	my ($http, $res, $queue_url) = @_;

	my $params         = $http->params || {};
	my $redirect_count = $params->{redirectCount} || 0;
	my $code = ($res && $res->can('code') && $res->code)
		? $res->code
		: ($http && $http->can('code') ? $http->code : undef);
	return 0 unless $code && $code =~ /^30[12378]$/;

	my $location = $res
		? ($res->can('header')
			? $res->header('Location')
			: ($res->can('headers') && $res->headers && $res->headers->can('header')
				? $res->headers->header('Location')
				: undef))
		: undef;

	return 0 unless $location && $redirect_count < MAX_REDIRECTS;
	return 0 if $location =~ /[\r\n\0]/;

	my $next_url = eval { URI->new_abs($location, $http->url)->as_string };
	return 0 unless $next_url && _validateExternalHttpUrl($next_url);

	_fetchExternalUrl(
		$next_url,
		$queue_url,
		$params->{originalUrl},
		$redirect_count + 1,
		$params->{requestHeaders},
	);
	return 1;
}

sub _gotArtwork {
	my $http = shift;
	my $url  = $http->url;
	my $params = $http->params || {};
	my $queue_url = $params->{queueUrl} || $params->{originalUrl} || $url;

	my $code = $http->can('code') ? $http->code : undef;
	if ( $code && $code =~ /^30[12378]$/ ) {
		return if _handleRedirect($http, $http, $queue_url);
		_drainQueueError($queue_url, 403);
		return;
	}

	my $content_ref = $http->contentRef;
	if ( ref($content_ref) eq 'SCALAR' && defined($$content_ref) && length($$content_ref) > MAX_FETCH_BYTES ) {
		$log->warn("ImageProxy payload exceeds MAX_FETCH_BYTES for $url");
		_drainQueueError($queue_url, 500);
		return;
	}

	if (main::DEBUGLOG && $log->is_debug) {
		$log->debug('Received artwork of type ' . ($http->headers ? $http->headers->content_type : '') . ' and ' . (($http->headers && $http->headers->content_length) || length(${$content_ref})) . ' bytes length' );
	}

	my $ct_header = ($http->headers && $http->headers->content_type) ? $http->headers->content_type : '';
	my $is_webp = ($ct_header =~ /webp/i)
		|| (ref($content_ref) eq 'SCALAR' && defined($$content_ref) && length($$content_ref) >= 12 && substr($$content_ref, 0, 12) =~ /^RIFF.{4}WEBP/s);

	if ( $is_webp ) {
		if ($params->{originalUrl}) {
			# external image proxy already did the resizing
			$log->error("WEBP images are not supported, returning 500");
			return _gotArtworkError($http);
		}

		main::INFOLOG && $log->is_info && $log->info("WEBP images are not supported, try to convert to JPG");
		my $cloud_url = urlToCloudResizer($url);
		my %cloud_headers = (
			'Accept' => ACCEPT_IMAGE_FORMATS,
			Slim::Utils::Misc::apiHeaders(__PACKAGE__),
		);
		_fetchExternalUrl($cloud_url, $queue_url, $queue_url, 0, \%cloud_headers);
		return;
	}

	# Always sniff magic bytes via GDResizer::_content_type
	require Slim::Utils::GDResizer;
	my $filetype = Slim::Utils::GDResizer::_content_type($content_ref, 1);
	if ( !$filetype ) {
		main::INFOLOG && $log->is_info && $log->info("Invalid or unsupported image magic bytes from $url (Content-Type: $ct_header)");
		return _gotArtworkError($http);
	}

	if ( $ct_header =~ /text/i ) {
		main::INFOLOG && $log->is_info && $log->info("File type actually seems to be '$filetype', rather than $ct_header");
	}

	_resizeFromFile($queue_url, $content_ref, $http, $filetype);
}

sub _gotArtworkError {
	my ($http, $err_msg, $res) = @_;
	my $url       = $http->url;
	my $params    = $http->params || {};
	my $queue_url = $params->{queueUrl} || $params->{originalUrl} || $url;

	my $code = ($res && $res->can('code') && $res->code)
		? $res->code
		: ($http && $http->can('code') ? $http->code : undef);
	if ( $code && $code =~ /^30[12378]$/ ) {
		return if _handleRedirect($http, $res || $http, $queue_url);
		_drainQueueError($queue_url, 403);
		return;
	}

	my $error = 404;

	if ($http->headers && ($http->headers->content_type || '') =~ /text/i) {
		$error = 500;
		main::INFOLOG && $log->is_info && $log->info("Server returned error: " . $http->content);
	}
	elsif ($http->error && $http->error =~ /(\d{3})/) {
		$error = $1;
		main::INFOLOG && $log->is_info && $log->info("Server returned error: " . $http->error);
	}
	elsif (main::INFOLOG && $log->is_info) {
		$log->info("Unexpected failure fetching $url: " . ($http->error // ''));
	}

	# File does not exist, return error
	main::INFOLOG && $log->info("Artwork not found, returning $error: " . $url);

	_drainQueueError($queue_url, $error);
}

sub _resizeFromFile {
	my ($url, $fullpath, $http, $sniffed_type) = @_;

	$cache ||= Slim::Web::ImageProxy::Cache->new();

	if ( !$sniffed_type && ref($fullpath) eq 'SCALAR' ) {
		require Slim::Utils::GDResizer;
		$sniffed_type = Slim::Utils::GDResizer::_content_type($fullpath, 1);
	}

	while ( my $item = shift @{ $queue{$url} }) {
		my $client   = $item->{client};
		my $spec     = $item->{spec};
		my $args     = $item->{args};
		my $params   = $item->{params};
		my $callback = $item->{callback};
		my $cachekey = $item->{cachekey};

		# no need to resize data if we've got it from an external image proxy
		if ( ($spec =~ /^\.(?:png|jpe?g)/i || $item->{pre_shrunk}) && $http && $sniffed_type && $sniffed_type =~ /^(?:png|jpg)$/ ) {
			main::DEBUGLOG && $log->debug("No resizing required - already resized remotely, or original size requested");

			my $ct = $sniffed_type;

			$cache->set( $cachekey, {
				content_type  => $ct,
				mtime         => 0,
				original_path => undef,
				data_ref      => $fullpath,
			} );

			_setHeaders($args->[1], $ct);

			$callback && $callback->( $client, $params, $fullpath, @$args );
		}
		else {
			Slim::Utils::ImageResizer->resize($fullpath, $cachekey, $spec, sub {
				my ($body, $format) = @_;

				# Resized image should now be in cache
				my $response = $args->[1];

				if ( !ref $fullpath && $fullpath =~ /(?:^|[\/\\])imgproxy_[a-f0-9]+$/i ) {
					my $cachedir   = $prefs->get('cachedir');
					my $real_cache = $cachedir ? Cwd::realpath($cachedir) : undef;
					my $real_file  = Cwd::realpath($fullpath);
					if ( $real_cache && $real_file ) {
						$real_cache =~ s{[\/\\]+$}{};
						if ( index($real_file, "$real_cache/") == 0 || (main::ISWINDOWS && index(lc($real_file), lc("$real_cache\\")) == 0) ) {
							unlink $real_file;
						}
					}
				}

				if ( !($body && $format && ref $body eq 'SCALAR') && (my $c = $cache->get($cachekey)) ) {
					$body = $c->{data_ref};
					$format = $c->{content_type};
				}
				elsif ( !($body && $format && ref $body eq 'SCALAR') ) {
					# resize command failed, return 500
					main::INFOLOG && $log->info("  Resize failed, returning 500");
					$log->error("Artwork resize for $cachekey failed");

					_artworkError( $client, $params, $spec, 500, $callback, @$args );
					return;
				}

				if ( $body && ref $body eq 'SCALAR' ) {
					require Slim::Utils::GDResizer;
					$format = Slim::Utils::GDResizer::_content_type($body, 1) || $format;
				}

				if ($body && $format) {
					_setHeaders($args->[1], $format);
				}

				$callback && $callback->( $client, $params, $body, @$args );
			}, $cache );
		}
	}

	delete $queue{$url};
}

sub _setHeaders {
	my ($response, $format) = @_;

	my $ct = $format =~ /image/ ? $format : "image/$format";
	$ct =~ s/jpg/jpeg/;

	$response->content_type($ct);
	$response->header( 'X-Content-Type-Options' => 'nosniff' );
	$response->header( 'Cache-Control' => 'max-age=' . ONE_YEAR );
	$response->expires( time() + ONE_YEAR );
}

sub _artworkError {
	my ($client, $params, $spec, $code, $callback, @args) = @_;

	my $response = $args[1];

	my ($width, $height, $mode, $bgcolor, $ext) = Slim::Web::Graphics->parseSpec($spec);
	my $skin = ($params && defined $params->{'skinOverride'} && $params->{'skinOverride'} =~ /^[A-Za-z0-9_-]+$/)
		? $params->{'skinOverride'}
		: $prefs->get('skin');

	require Slim::Utils::GDResizer;
	my ($res, $format) = Slim::Utils::GDResizer->resize(
		file   => Slim::Web::HTTP::fixHttpPath($skin, 'html/images/radio.png'),
		width  => $width,
		height => $height,
		mode   => $mode,
	);

	my $sniffed = (ref $res eq 'SCALAR') ? Slim::Utils::GDResizer::_content_type($res, 1) : undef;
	$format = $sniffed || $format || 'png';
	my $ct = 'image/' . $format;
	$ct =~ s/jpg/jpeg/;

	$response->content_type($ct);
	$response->code($code || 404);
	$response->header( 'X-Content-Type-Options' => 'nosniff' );
	$response->expires( time() - 1 );
	$response->header( 'Cache-Control' => 'no-cache' );

	$callback->( $client, $params, $res, @args );
}

# Return a proxied image URL if
# - the given url is a fully qualified url, and
# - a custom handler for the given url has been defined (eg. radiotime), or
# - the $force parameter is passed in
#
# $force can be used to create custom handlers dealing with custom "urls".
# Eg. there could be a pattern to only pass some album id, together with a keyword,
# like "spotify::album::123456". It's then up to the image handler to get the real url.
sub proxiedImage {
	my ($url, $force) = @_;

	# only proxy external URLs
	return $url unless $force || ($url && $url =~ /^https?:/i);

#	main::DEBUGLOG && $log->debug("Use proxied image URL for: $url");

	# Unfortunately Squeezeplay can't handle images without a file extension?!? We need to return some extension,
	# though it might be different from the actual content type returned. But we don't know better at this point.
	my $ext = '.png';

	if ($url =~ /(\.(?:jpg|jpeg|png|gif))/) {
		$ext = $1;
		$ext =~ s/jpeg/jpg/;
	}

	return '/imageproxy/' . uri_escape_utf8($url) . '/image' . $ext;
}

# allow plugins to register custom handlers for the image url
sub registerHandler {
	my ( $class, %params ) = @_;

	if ( $params{external} && $params{desc} && $params{id} && $params{func} ) {
		$externalHandlers{ $params{id} } = \%params;
		main::DEBUGLOG && $log->is_debug && $log->debug("Registered external image proxy: " . $params{desc});
		return 1;
	}

	if ( ref $params{match} ne 'Regexp' ) {
		$log->error( 'registerProvider called without a regular expression ' . ref $params{match} );
		return;
	}

	if ( ref $params{func} ne 'CODE' ) {
		$log->error( 'registerProvider called without a code reference' );
		return;
	}

	$handlers{ $params{match} } = $params{func};

	if ( main::DEBUGLOG && $log->is_debug ) {
		my $name = Slim::Utils::PerlRunTime::realNameForCodeRef( $params{func} );
		$log->debug( "Registered new artwork URL handler for " . $params{match} . ": $name" );
	}

	return 1;
}

sub getHandlerFor {
	my ( $class, $url ) = @_;
	return $handlers{ $url };
}

sub getExternalHandlers {
	return \%externalHandlers;
}

# helper method to get the correc sizing parameter out of a list of possible sizes
# my $size = Slim::Web::ImageProxy->getRightSize("180x180_m.jpg", {
#	70  => 's',
#	150 => 'm',
#	300 => 'l',
#	600 => 'g',
# });
# -> would return 'l', which is the smallest to cover 180px width/height
sub getRightSize {
	my $class = shift;
	my $spec  = shift;      # resizing specification
	my $sizes = shift;      # a list of size/param tuples - param will be returned for the smallest fit

	my ($width, $height) = Slim::Web::Graphics->parseSpec($spec);

	if ($width || $height) {
		$width  ||= $height;
		$height ||= $width;

		my $min = ($width > $height ? $width : $height);

		# get smallest size larger than what we need
		# use <=> comparison to make sure the sort is done numerically!
		foreach (sort { $a <=> $b } keys %$sizes) {
			return $sizes->{$_} if $_ >= $min;
		}
	}
}

sub urlToCloudResizer {
	return REDIRECT_IMAGE_TO_COMPATIBLE . uri_escape_utf8($_[0]);
}

1;


# custom artwork cache, extended to expire content after 30 days
package Slim::Web::ImageProxy::Cache;

use base 'Slim::Utils::DbArtworkCache';

use strict;

use constant PURGE_INTERVAL => 3600 * 8;  # interval between purge cycles
# image proxy cache is slow to purge due to the large item sizes, so we do it in smaller chunks
use constant INCREMENTAL_PURGE_CHUNKSIZE => 20;
use constant INCREMENTAL_PURGE_INTERVAL => 10;
use constant FIRST_PURGE_DELAY => 20;

sub new {
	my $class = shift;
	my $root = shift;

	if ( !$cache ) {
		$cache = $class->SUPER::new($root, 'imgproxy', 86400*30);

		if ( !main::SCANNER ) {
			# start purge routine in a few seconds
			require Slim::Utils::Timers;
			Slim::Utils::Timers::setTimer( undef, time() + FIRST_PURGE_DELAY + rand(FIRST_PURGE_DELAY), \&cleanup );
		}
	}

	return $cache;
}

sub cleanup {
	my ($class, $force) = @_;

	# after startup don't purge if a player is on - retry later
	my $interval;

	$cache->checkActivity(sub {
		my $client = shift;
		main::INFOLOG && $log->is_info && $log->info('Skipping cache purge due to client activity: ' . $client->name);
		$interval = 300 + 60 * rand(5);
	}) unless $force;

	my $now = Time::HiRes::time();

	if (!$interval) {
		my $deleted = $cache->purge(INCREMENTAL_PURGE_CHUNKSIZE);
		main::INFOLOG && $log->is_info && $log->info(sprintf("ImageProxy cache purge: %i records - %f sec", $deleted, Time::HiRes::time() - $now));

		if ($deleted < INCREMENTAL_PURGE_CHUNKSIZE) {
			# no more items to purge, so we can wait longer
			$interval = PURGE_INTERVAL;
		} else {
			# still items to purge, so we do it again in a short while
			$interval = INCREMENTAL_PURGE_INTERVAL + rand(INCREMENTAL_PURGE_INTERVAL);
		}
	}

	Slim::Utils::Timers::setTimer( undef, $now + ($interval || PURGE_INTERVAL), \&cleanup );
}

1;
