package Slim::Web::Graphics;

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2025 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use Cwd ();
use Scalar::Util qw(blessed);
use File::Basename;
use File::Slurp ();
use URI ();
use URI::Escape ();

use Slim::Player::ProtocolHandlers;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::ArtworkCache;
use Slim::Utils::Prefs;
use Slim::Utils::ImageResizer;
use Slim::Web::ImageProxy;

use constant ONE_DAY         => 86400;
use constant ONE_YEAR        => ONE_DAY * 365;
use constant MAX_DIMENSION   => 4096;
use constant MAX_FETCH_BYTES => 16 * 1024 * 1024;
use constant MAX_REDIRECTS   => 5;

my $prefs = preferences('server');
my $log   = logger('artwork');

my $skinMgr;

my ($cache, $imgProxyCache);

sub init {
	# Get cache for artwork
	$cache = Slim::Utils::ArtworkCache->new();

	Slim::Utils::ImageResizer->initDaemon();
	Slim::Web::ImageProxy->init();

	if (main::SCANNER) {
		require Slim::Web::Template::NoWeb;
		$skinMgr = Slim::Web::Template::NoWeb->new();
	}
	else {
		$skinMgr = Slim::Web::HTTP::getSkinManager();
	}
}

sub serverResizesArt { 1 }

sub _cached {
	my ($path, $force) = @_;

	# force reading from cache when using the external resizer daemon
	return if !main::SCANNER && main::NOBROWSECACHE && !$force;

	my $isInfo = main::INFOLOG && $log->is_info;

	if ( my $cached = $cache->get($path) ) {
		if ( $path =~ m|^music/[a-f0-9]{8}/cover| && $cached->{mtime} == 0 ) {
			return $cached;
		}
		elsif ( my $orig = $cached->{original_path} ) {
			# Check mtime of original artwork has not changed,
			# unless it's a /music or /contributor path, where we don't care if
			# it has changed.  The scanner should deal with changes there.
			if ( $path !~ m{^music/|^contributor/} && -r $orig ) {
				my $mtime = (stat _)[9];
				if ( $cached->{mtime} != $mtime ) {
					main::INFOLOG && $isInfo && $log->info( "  current mtime $mtime != cached mtime " . $cached->{mtime} );
					return;
				}
			}

			if ( main::INFOLOG && $isInfo ) {
				my $type = $cached->{content_type};
				my $size = length( ${ $cached->{data_ref} } );
				$log->info( "  from cache: $type ($size bytes for $path)" );
			}

			return $cached;
		}
	}

	return;
}

sub _sanitizeSkinOverride {
	my ($params) = @_;
	return unless $params && defined $params->{skinOverride};
	return $params->{skinOverride} =~ /^[A-Za-z0-9_-]+$/ ? $params->{skinOverride} : undef;
}

sub _confinePluginsCachePath {
	my ($candidate, $real_cachedir) = @_;
	return unless defined $candidate && defined $real_cachedir;

	my $real_path = Cwd::realpath($candidate);
	return unless defined $real_path;

	$real_cachedir =~ s{[\/\\]+$}{};
	return $real_path if index($real_path, "$real_cachedir/") == 0;
	return $real_path if main::ISWINDOWS && index(lc($real_path), lc("$real_cachedir\\")) == 0;
	return;
}

sub _fetchExternalArtwork {
	my ($fetch_url, $orig_url, $redirect_count, $on_success, $on_error) = @_;

	if ( !Slim::Web::ImageProxy::_validateExternalHttpUrl($fetch_url) ) {
		$on_error->();
		return;
	}

	Slim::Networking::SimpleAsyncHTTP->new(
		sub {
			my $http = shift;
			if ( $http->code && $http->code =~ /^30[12378]$/ ) {
				my $loc = $http->headers ? $http->headers->header('Location') : undef;
				if ( $loc && $loc !~ /[\r\n\0]/ && ($redirect_count || 0) < MAX_REDIRECTS ) {
					my $next_url = eval { URI->new_abs($loc, $fetch_url)->as_string };
					if ( $next_url && Slim::Web::ImageProxy::_validateExternalHttpUrl($next_url) ) {
						_fetchExternalArtwork($next_url, $orig_url, ($redirect_count || 0) + 1, $on_success, $on_error);
						return;
					}
				}
				$on_error->();
				return;
			}
			my $content_ref = $http->contentRef;
			if ( !$content_ref || ref($content_ref) ne 'SCALAR' || !defined($$content_ref) || length($$content_ref) > MAX_FETCH_BYTES ) {
				$on_error->();
				return;
			}
			$on_success->($http);
		},
		sub {
			my ($http, $err, $res) = @_;
			if ( $res && $res->code && $res->code =~ /^30[12378]$/ ) {
				my $loc = $res->header('Location');
				if ( $loc && $loc !~ /[\r\n\0]/ && ($redirect_count || 0) < MAX_REDIRECTS ) {
					my $next_url = eval { URI->new_abs($loc, $fetch_url)->as_string };
					if ( $next_url && Slim::Web::ImageProxy::_validateExternalHttpUrl($next_url) ) {
						_fetchExternalArtwork($next_url, $orig_url, ($redirect_count || 0) + 1, $on_success, $on_error);
						return;
					}
				}
			}
			$on_error->();
		},
		{
			timeout     => 15,
			maxRedirect => 0,
		}
	)->get($fetch_url);
}

sub artworkRequest {
	my ( $client, $path, $params, $callback, @args ) = @_;

	my $isInfo = main::INFOLOG && $log->is_info;

	main::INFOLOG && $isInfo && $log->info("Artwork request: " . (defined $path ? $path : '<undef>'));

	# Reject unsafe path constructs up front (SEC-005 / SEC-031)
	if ( !defined $path
		|| !length($path)
		|| $path =~ /[\r\n\0]/
		|| $path =~ m{^[/\\]}
		|| $path =~ m{^https?:}i
		|| $path =~ m{^[A-Za-z]:}
		|| $path =~ m{\\\\}
		|| $path =~ m{^//}
		|| $path =~ m{(?:^|[/\\])\.\.(?:[/\\]|$)}
	) {
		send404($client, $params, $callback, @args);
		return;
	}

	my $skin_override = _sanitizeSkinOverride($params);
	my $skin          = $skin_override || $prefs->get('skin');
	my $cache_key     = ($skin_override && $path !~ m{^(?:music|contributor|plugins/cache)/})
		? "$skin_override:$path"
		: $path;

	# We need the HTTP::Response object to control caching
	my $response = $args[1];

	# Handling artwork works like this:
	# * Check if desired version exists in the cache and is fresh
	# * If so, use it
	# * If not:
	# * Determine the absolute path to the requested object
	# * Fire off an async resize call
	# * When resize is done, read newly cached image and callback

	# XXX remote URLs (from protocol handler icon)

	# Check cache for this path
	if ( $path !~ m{^imageproxy/} && (my $c = _cached($cache_key)) ) {
		require Slim::Utils::GDResizer;
		my $sniffed = (ref $c->{data_ref} eq 'SCALAR') ? Slim::Utils::GDResizer::_content_type($c->{data_ref}, 1) : undef;
		my $ct = 'image/' . ($sniffed || $c->{content_type});
		$ct =~ s/jpg/jpeg/;
		$response->content_type($ct);
		$response->header( 'X-Content-Type-Options' => 'nosniff' );

		# Cache music and contributor URLs for 1 year, others for 1 day
		my $exptime = $path =~ m{^(?:music|contributor)/} ? ONE_YEAR : ONE_DAY;

		$response->header( 'Cache-Control' => 'max-age=' . $exptime );
		$response->expires( time() + $exptime );

		$callback->( $client, $params, $c->{data_ref}, @args );

		return;
	}

	my $fullpath;
	my $no_cache;
	my $is_plugins_cache = 0;

	# Parse out spec from path
	# WxH[_m][_bg][.ext]
	# 'X' can be used instead of either W or H to determine automatically
	my ($spec) = File::Basename::basename($path) =~ /_?((?:[0-9X]+x[0-9X]+)?(?:_\w)?(?:_[\da-fA-F]+)?(?:\.\w+)?)$/;
	$spec //= '';

	if ( $spec ne '' ) {
		my ($sw, $sh, $sm, $sbg, $sext) = __PACKAGE__->parseSpec($spec);
		if ( !defined($sw) && !defined($sh) && !defined($sm) && !defined($sbg) && !defined($sext) ) {
			main::INFOLOG && $isInfo && $log->info("  Invalid resize specification: $spec");
			send404($client, $params, $callback, @args);
			return;
		}
	}

	main::INFOLOG && $isInfo && $log->info("  Resize specification: $spec");

	# /music/all_items (used in BrowseDB, just returns html/images/albums.png)
	if ( $path =~ m{^music/all_items} ) {
		# Poor choice of special names...
		$spec =~ s{^items/[^_]+_}{};
		$path = 'html/images/albums_' . $spec;

		# Make sure we have an extension
		if ( $path !~ /\./ ) {
			$path .= '.png';
		}

		$cache_key = $skin_override ? "$skin_override:$path" : $path;

		main::INFOLOG && $isInfo && $log->info("  Special path translated to $path");
	}

	# local image proxy for remote URLs
	elsif ( $path =~ m{^imageproxy/} ) {
		Slim::Web::ImageProxy->getImage($client, $path, $params, $callback, $spec, @args);
		return;
	}

	# If path begins with "music" it's a cover path using either coverid
	# or the old trackid format
	elsif ( $path =~ m{^music/([^/]+)/} ) {
		my $id = $1;

		# Special case:
		# /music/current/cover.jpg (mentioned in CLI docs)
		if ( $id eq 'current' && $client ) {
			my $trackObj = Slim::Player::Playlist::track($client);

			if ( $trackObj && blessed $trackObj ) {
				$id = $trackObj->coverid;

				# if we're dealing with a remote stream, we'll have to ask a protocol handler to return the cover URL
				if ( $trackObj->remote ) {
					my $url     = $trackObj->url;
					my $handler = Slim::Player::ProtocolHandlers->handlerForURL($url);

					if ( $handler && $handler->can('getMetadataFor') ) {
						my $remoteMeta = $handler->getMetadataFor( $client, $url );

						if ( $remoteMeta && (my $cover = $remoteMeta->{cover}) ) {
							# sometimes we'd get an already proxied URL - remove the proxy params to start over
							if ($cover =~ s/^\/?imageproxy\/(.*)\/image\.(?:png|jpg)?$/$1/) {
								$cover = URI::Escape::uri_unescape($cover);
							}

							if ($cover =~ /^https?:/i) {
								$id = 'imageproxy/' . $cover;
								$path=~ s/music\/current/$id/;

								main::INFOLOG && $isInfo && $log->info("  Special path translated to $path");

								Slim::Web::Graphics::artworkRequest($client, $path, $params, $callback, @args);
								return;
							}
							else {
								$fullpath = $path = $cover;
							}
						}
					}
				}
				else {
					$path =~ s/current/$id/;
				}

				$cache_key = $path;
				main::INFOLOG && $isInfo && $log->info("  Special path translated to $path");
			}
		}

		# Fetch the url and cover values
		my $sth;
		my ($url, $cover);

		if ($fullpath) {
			# nothing to do here!
		}
		elsif ( $id =~ /^[0-9a-f]{8}$/ ) {
			# ID is a coverid
			$sth = Slim::Schema->dbh->prepare_cached( qq{
				SELECT url, cover FROM tracks WHERE coverid = ? LIMIT 1
			} );
		}
		elsif ( $id =~ /^-\d+$/ ) {
			# if $id starts with a negative sign, ID is a remote track

			# Bug 16491: Grab the remoteTrack's coverArt and do the resizing on the fly
			my $remoteTrack = Slim::Schema::RemoteTrack->fetchById($id);

			if ( $remoteTrack && (my $coverArtImage = $remoteTrack->coverArt()) ) {
				require Slim::Utils::GDResizer;

				my @arrSpec = split(',', $spec);
				my ($width, $height, $mode, $bgcolor, $ext) = __PACKAGE__->parseSpec($arrSpec[0]);
				my ($res, $format) = Slim::Utils::GDResizer->resize(
					original => \$coverArtImage,
					width    => $width,
					height   => $height,
					mode     => $mode,
					bgcolor  => $bgcolor,
					format   => $ext,
				);

				my $sniffed = (ref $res eq 'SCALAR') ? Slim::Utils::GDResizer::_content_type($res, 1) : undef;
				$format = $sniffed || $format || 'png';
				my $ct = 'image/' . $format;
				$ct =~ s/jpg/jpeg/;
				$response->content_type($ct);
				$response->header( 'X-Content-Type-Options' => 'nosniff' );
				$response->header( 'Cache-Control' => 'max-age=' . ONE_YEAR );
				$response->expires( time() + ONE_YEAR );

				$callback->( $client, $params, $res, @args );
				return;
			}
		}
		else {
			# ID is the trackid, this is deprecated because
			# the artwork can be stale after a rescan
			$sth = Slim::Schema->dbh->prepare_cached( qq{
				SELECT url, cover FROM tracks WHERE id = ?
			} );
		}

		if ( $sth ) {
			$sth->execute($id);
			($url, $cover) = $sth->fetchrow_array;
			$sth->finish;

			# border case: 8 characters we're assuming it's a cover ID, but it could be a track ID, too
			if ( (!$url || !$cover) && $id =~ /\d{8}/ ) {
				$sth = Slim::Schema->dbh->prepare_cached( qq{
					SELECT url, cover FROM tracks WHERE id = ?
				} );

				$sth->execute($id);
				($url, $cover) = $sth->fetchrow_array;
				$sth->finish;
			}
		}

		if ($fullpath) {
			# happy path - nothing to do here
		}
		elsif ( !$url || !$cover ) {
			# Invalid ID or no cover available, use generic CD image
			if ($id =~ /^-/) {
				$path = 'html/images/radio_';
			}
			else {
				$path = 'html/images/cover_';
			}

			$path .= $spec;
			$path =~ s/\.\w+//;
			$path =~ s/_$//;
			$path .= '.png';

			# our default artwork are PNG files
			$spec =~ s/\.\w+$/.png/;

			$cache_key = $skin_override ? "$skin_override:$path" : $path;

			# Don't allow browsers to cache this error image
			$no_cache = 1;

			main::INFOLOG && $isInfo && $log->info("  No cover found, translated to $path");

			# Check cache for this image
			if ( my $c = _cached($cache_key) ) {
				# Don't allow browsers to cache this error image
				require Slim::Utils::GDResizer;
				my $sniffed = (ref $c->{data_ref} eq 'SCALAR') ? Slim::Utils::GDResizer::_content_type($c->{data_ref}, 1) : undef;
				my $ct = 'image/' . ($sniffed || $c->{content_type});
				$ct =~ s/jpg/jpeg/;
				$response->content_type($ct);
				$response->header( 'X-Content-Type-Options' => 'nosniff' );
				$response->header( 'Cache-Control' => 'no-cache' );
				$response->expires( time() - 1 );

				$callback->( $client, $params, $c->{data_ref}, @args );
				return;
			}

			$fullpath = $skinMgr->fixHttpPath($skin, $path);
		}
		else {
			# Image to resize is either a cover path or the audio file if cover is
			# a number (length of embedded art)
			$fullpath = $cover =~ /^\d+$/
				? Slim::Utils::Misc::pathFromFileURL($url)
				: $cover;
		}
	}

	elsif ( $path =~ m{^contributor/([^/]+)/} ) {
		my $id = $1;

		main::INFOLOG && $log->is_info && $log->info("  Looking for contributor portrait for $id");

		my $url;
		if ($id) {
			my $sth = Slim::Schema->dbh->prepare_cached( qq{
				SELECT portrait FROM contributors WHERE portraitid = ? LIMIT 1
			} );

			$sth->execute($id);
			($url) = $sth->fetchrow_array;
			$sth->finish;
		}

		if ($url) {
			$fullpath = Slim::Utils::Misc::pathFromFileURL($url);
			$fullpath = Slim::Utils::Unicode::utf8on($fullpath) if $fullpath;
		}
		else {
			my $fallback_path = 'html/images/artists.png';
			$fullpath = $skinMgr->fixHttpPath($skin, $fallback_path);
			$cache_key = $skin_override ? "$skin_override:$path" : $path;
		}

		main::INFOLOG && $log->is_info && $log->info("  Found contributor portrait at $fullpath");
	}

	# If path begins with "plugins/cache" it is a special path
	# meaning we need to lookup the actual path in our cache directory
	elsif ( $path =~ m{^plugins/cache(?:/(.+))?$} ) {
		my $rel = $1;
		my $cachedir = $prefs->get('cachedir');
		if ( !$rel || !$cachedir ) {
			send404($client, $params, $callback, @args);
			return;
		}
		$cachedir =~ s{/$}{};
		$fullpath = "$cachedir/$rel";
		$is_plugins_cache = 1;
	}

	if ( !$fullpath ) {
		$fullpath = $path;
	}

	if ( $spec ne '' ) {
		# Strip spec off fullpath if necessary, keeping the file extension
		my ($ext) = $spec =~ /(\.\w+)$/;
		$ext ||= '';
		my $qspec = quotemeta($spec);
		$fullpath =~ s/_${qspec}/$ext/;
	}

	# Resolve full path if it's not already a full path (checks Unix and Windows path prefixes)
	# Bug 16814: We also need to check for UNC prefix
	if ( $fullpath !~ m{^/|^https?:}i && $fullpath !~ /^[a-z]:[\\\/]/i && $fullpath !~ /^\\\\/i ) {
		main::INFOLOG && $isInfo && $log->info("  Looking for: $fullpath in skin $skin");
		$fullpath = $skinMgr->fixHttpPath($skin, $fullpath);
	}

	my $real_cachedir;
	if ( $is_plugins_cache ) {
		$real_cachedir = Cwd::realpath($prefs->get('cachedir'));
		if ( !$real_cachedir ) {
			send404($client, $params, $callback, @args);
			return;
		}
	}

	# Support pre-sized files already in place, this is used on SB Touch
	# for app icons because it can't handle resizing so many icons at once
	# It is also used for requests for /cover (the original file)
	if ( $fullpath && $fullpath =~ /(\.(?:jpe?g|jpe|png|gif|bmp))$/i ) {
		my $ext = $1;

		# Add the spec back to the fullpath if there's more than the extension
		my $fullpathspec = $fullpath;

		if ( $spec ne '' && $spec ne $ext ) {
			my $qext = quotemeta($ext);
			$fullpathspec =~ s/(${qext})$/_${spec}$1/;
		}

		my $presized_target = $fullpathspec;
		if ( $is_plugins_cache ) {
			$presized_target = _confinePluginsCachePath($fullpathspec, $real_cachedir);
		}

		if ( $presized_target && -e $presized_target ) {
			main::INFOLOG && $isInfo && $log->info("  Using existing pre-cached file: $presized_target");

			my $body = File::Slurp::read_file($presized_target, binmode => ':raw');
			require Slim::Utils::GDResizer;
			my $sniffed = Slim::Utils::GDResizer::_content_type(\$body, 1);
			if ( !$sniffed ) {
				send404($client, $params, $callback, @args);
				return;
			}

			my $ct = 'image/' . $sniffed;
			$ct =~ s/jpg/jpeg/;
			$response->content_type($ct);
			$response->header( 'X-Content-Type-Options' => 'nosniff' );

			my $exptime = $path =~ m{^(?:music|contributor)/} ? ONE_YEAR : ONE_DAY;
			$response->header( 'Cache-Control' => 'max-age=' . $exptime );
			$response->expires( time() + $exptime );

			$callback->( $client, $params, \$body, @args );
			return;
		}
	}

	if ( $is_plugins_cache ) {
		$fullpath = _confinePluginsCachePath($fullpath, $real_cachedir);
		if ( !$fullpath ) {
			send404($client, $params, $callback, @args);
			return;
		}
	}

	if ( $fullpath && ($fullpath =~ /^https?:/i || -e $fullpath) ) {
		if ( $fullpath =~ /^https?:/i && !Slim::Web::ImageProxy::_validateExternalHttpUrl($fullpath) ) {
			$log->warn("Blocked disallowed remote artwork URL in Graphics: $fullpath");
			send404($client, $params, $callback, @args);
			return;
		}

		main::idleStreams();

		main::INFOLOG && $isInfo && $log->info("  Resizing: $fullpath using spec $spec");

		my $imageCb = sub {
			my ($body, $format) = @_;

			require Slim::Utils::GDResizer;
			my $sniffed = (defined $body && ref $body eq 'SCALAR') ? Slim::Utils::GDResizer::_content_type($body, 1) : undef;
			$format = $sniffed || $format;

			if ( $body && $sniffed && $format !~ /application/ && ref $body eq 'SCALAR' ) {
				my $ct = $format =~ /image/ ? $format : 'image/' . $format;
				$ct =~ s/jpg/jpeg/;
				$response->content_type($ct);
				$response->header( 'X-Content-Type-Options' => 'nosniff' );

				# Cache music and contributor URLs for 1 year, others for 1 day
				my $exptime = $path =~ m{^(?:music|contributor)/} ? ONE_YEAR : ONE_DAY;

				if ( $no_cache ) {
					$response->header( 'Cache-Control' => 'no-cache' );
					$response->expires( time() - 1 );
				}
				else {
					$response->header( 'Cache-Control' => 'max-age=' . $exptime );
					$response->expires( time() + $exptime );
				}
			}
			else {
				# resize command failed, return 500
				main::INFOLOG && $isInfo && $log->info("  Resize failed, returning 500");

				$log->error("Artwork resize for $path failed");
				$response->code(500);
				$response->content_type('text/html');
				$response->header( 'X-Content-Type-Options' => 'nosniff' );
				$response->expires( time() - 1 );
				$response->header( 'Cache-Control' => 'no-cache' );

				$body = \'';
			}

			main::INFOLOG && $isInfo && $log->info("  Done Resizing: $fullpath using spec $spec");
			$callback->( $client, $params, $body, @args );
		};

		# if we don't want to resize the image, just read from the tag and don't cache the full size image
		if ( !$spec && (my $ct = Slim::Music::Info::contentType($fullpath)) ) {
			my $formatClass = Slim::Formats->classForFormat($ct);
			my $body;

			if ($formatClass && $formatClass->can('getCoverArt') && Slim::Formats->loadTagFormatForType($ct)) {
				$body = $formatClass->getCoverArt($fullpath);
			}

			if ($body && length $body) {
				main::INFOLOG && $isInfo && $log->info("  No Resizing required - return raw image data");

				$imageCb->(\$body);
				return;
			}
		}

		my $doResize = sub {
			my $cover = $_[0];
			Slim::Utils::ImageResizer->resize($cover, $cache_key, $spec, sub {
				my ($body, $format, $readFromCache) = @_;

				# if we didn't get a valid reference returned, try to read from cache
				if ( !($body && $format && ref $body eq 'SCALAR') && (my $c = _cached($cache_key, $readFromCache)) ) {
					$body = $c->{data_ref};
					$format = $c->{content_type};
				}

				$imageCb->($body, $format);
			} );
		};

		if ($fullpath =~ /^https?:/i) {
			require Slim::Web::ImageProxy;
			$imgProxyCache ||= Slim::Web::ImageProxy::Cache->new();

			my $proxy_key = Slim::Web::ImageProxy::proxiedImage($fullpath);
			$proxy_key =~ s{^/}{} if defined $proxy_key;
			my $proxy_key_nopng = ($proxy_key && $proxy_key =~ /(.*)\.png$/i) ? $1 : undef;

			my $cached = $imgProxyCache->get($fullpath)
				|| ($proxy_key && $imgProxyCache->get($proxy_key))
				|| ($proxy_key_nopng && $imgProxyCache->get($proxy_key_nopng));

			if ($cached) {
				$doResize->($cached->{data_ref});
			}
			else {
				_fetchExternalArtwork(
					$fullpath,
					$fullpath,
					0,
					sub {
						my $http = shift;
						require Slim::Utils::GDResizer;
						my $sniffed = Slim::Utils::GDResizer::_content_type($http->contentRef, 1);
						if (!$sniffed) {
							send404($client, $params, $callback, @args);
							return;
						}
						$imgProxyCache->set($fullpath, {
							content_type  => $sniffed,
							mtime         => 0,
							original_path => undef,
							data_ref      => $http->contentRef,
						});
						$doResize->($http->contentRef);
					},
					sub {
						send404($client, $params, $callback, @args);
					},
				);
			}
		}
		else {
			$doResize->($fullpath);
		}
	}
	else {
		# File does not exist, return 404
		send404($client, $params, $callback, @args);
	}

	return;
}

sub send404 {
	my ( $client, $params, $callback, @args ) = @_;

	my $response = $args[1];

	main::INFOLOG && $log->is_info && $log->info("  File not found, returning 404");

	$response->code(404);
	$response->content_type('text/html');
	$response->header( 'X-Content-Type-Options' => 'nosniff' );
	$response->expires( time() - 1 );
	$response->header( 'Cache-Control' => 'no-cache' );

	my $body = Slim::Web::HTTP::filltemplatefile('html/errors/404.html', $params);

	$callback->( $client, $params, $body, @args );
}

sub parseSpec {
	my ($class, $spec) = @_;
	return () unless defined $spec && length($spec);

	my ($width, $height, $mode, $bgcolor, $ext) = $spec =~ /^(?:([0-9X]+)x([0-9X]+))?(?:_([mpoF]))?(?:_([0-9a-fA-F]{6}))?(?:\.(png|jpe?g|jpe|gif|bmp))?$/i;
	return () unless defined($width) || defined($height) || defined($mode) || defined($bgcolor) || defined($ext);

	if ( defined $width && $width ne 'X' ) {
		return () if $width !~ /^\d+$/ || $width > MAX_DIMENSION;
	}
	if ( defined $height && $height ne 'X' ) {
		return () if $height !~ /^\d+$/ || $height > MAX_DIMENSION;
	}

	return ($width, $height, $mode, $bgcolor, $ext);
}

1;
