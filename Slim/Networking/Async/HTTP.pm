package Slim::Networking::Async::HTTP;

# Logitech Media Server Copyright 2003-2024 Logitech.
# Lyrion Music Server Copyright 2024-2026 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

# This class provides an async HTTP implementation.
# The constructor takes an optional hash with two keys to hash
# - 'options': set parameters for underlying socket object
#   Slim::Networking::Async::HTTP->new( { options => {
#               SSL_cipher_list => 'DEFAULT:!DH',
#               SSL_verify_mode => Net::SSLeay::VERIFY_NONE } })
# - 'socks': use a socks proxy to tunnel the request
# See code below for better explanation

use strict;

use constant MIN_IO_SOCKET_SSL => '2.020';

BEGIN {
	my $hasSSL;

	sub hasSSL {
		return $hasSSL if defined $hasSSL;

		$hasSSL = 0;

		eval {
			require Slim::Networking::Async::Socket::HTTPS;
			$hasSSL = 1;
		};

		if ($@) {
			msg("Async::HTTP: Unable to load IO::Socket::SSL, HTTPS requests will fail\n$@\n");
		}
		# if we're using an outdated version of IO::Socket::SSL, log a warning
		elsif (Slim::Utils::Versions->compareVersions(MIN_IO_SOCKET_SSL, $IO::Socket::SSL::VERSION) > 0) {
			msg("You're using a rather old version of IO::Socket::SSL (v$IO::Socket::SSL::VERSION) - please try to update to at least " . MIN_IO_SOCKET_SSL . " for improved compatibility.\n");
		}

		return $hasSSL;
	}
}

use base 'Slim::Networking::Async';

use HTTP::Headers;
use HTTP::Request;
use HTTP::Response;
use HTTP::Cookies;
use MIME::Base64 qw(encode_base64);
use Socket qw(inet_aton inet_pton AF_INET AF_INET6);
use URI;
use File::Spec::Functions qw(catdir);

use Slim::Networking::Async::Socket::HTTP;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Prefs;
use Slim::Utils::Timers;
use Slim::Utils::Versions;

use constant BUFSIZE               => 16 * 1024;
use constant DEFAULT_MAX_BODY_SIZE => 32 * 1024 * 1024;

my $prefs = preferences('server');

my $cookieJar;

my $log = logger('network.asynchttp');

__PACKAGE__->mk_accessor( rw => qw(
	uri request response saveAs fh timeout maxRedirect socks options insecureHTTPS _is_redirect _bytes_read
) );

sub init {
	$cookieJar = HTTP::Cookies->new( file => catdir($prefs->get('cachedir'), 'cookies.dat'), autosave => 1 );
}

sub new {
	my ($class, $args) = @_;
	my $self = $class->SUPER::new;

	if ( $args->{socks} ) {
		eval {
			require Slim::Networking::Async::Socket::HTTPSocks;
			require Slim::Networking::Async::Socket::HTTPSSocks if hasSSL();
		};

		if (!$@) {
			# no need for a hash mk_accessor type as we don't access individual keys
			$self->socks($args->{socks});
			main::INFOLOG && $log->info("Using SOCKS $args->{socks}->{ProxyAddr}::$args->{socks}->{ProxyPort} to connect");
		}
	}

	$self->options($args->{options});
	$self->insecureHTTPS( defined $args->{insecureHTTPS} ? $args->{insecureHTTPS} : $prefs->get('insecureHTTPS') );

	return $self;
}

sub _safe_uri_string {
	my $uri = shift;
	return '' unless defined $uri;

	my $u = eval { ref($uri) ? $uri->clone : URI->new("$uri") };
	if ( $u && $u->can('userinfo') && defined $u->userinfo ) {
		$u->userinfo(undef);
		return $u->as_string;
	}
	return $u->as_string if $u;

	my $s = "$uri";
	$s =~ s{^([a-z][a-z0-9+.\-]*://)[^/\@#?]+\@}{$1}i;
	return $s;
}

sub _normalize_host {
	my $host = shift;
	return '' unless defined $host;
	$host =~ s/^\s+|\s+$//g;
	if ( $host =~ /^\[([^\]]+)\](?::\d+)?$/ ) {
		$host = $1;
	}
	elsif ( $host !~ /:/ || $host =~ /^[^:]+:\d+$/ ) {
		$host =~ s/:\d+$//;
	}
	$host =~ s/\.+$//;
	return lc($host);
}

sub _is_metadata_host {
	my $host = _normalize_host(shift);
	return 0 unless length $host;

	return 1 if $host eq '169.254.169.254'
		|| $host eq '100.100.100.200'
		|| $host eq 'fd00:ec2::254'
		|| $host eq 'metadata.google.internal'
		|| $host eq 'metadata.goog'
		|| $host eq 'metadata'
		|| $host eq 'instance-data';

	return 0;
}

sub _is_blocked_ipv4_bytes {
	my $bytes = shift;
	return 0 unless defined $bytes && length($bytes) == 4;
	my ($b0, $b1) = unpack('CC', $bytes);

	return 1 if $b0 == 0;                                  # 0.0.0.0/8
	return 1 if $b0 == 10;                                 # 10.0.0.0/8 (RFC1918)
	return 1 if $b0 == 100 && $b1 >= 64 && $b1 <= 127;     # 100.64.0.0/10 (CGNAT)
	return 1 if $b0 == 127;                                # 127.0.0.0/8 (loopback)
	return 1 if $b0 == 169 && $b1 == 254;                  # 169.254.0.0/16 (link-local / metadata)
	return 1 if $b0 == 172 && $b1 >= 16 && $b1 <= 31;      # 172.16.0.0/12 (RFC1918)
	return 1 if $b0 == 192 && $b1 == 168;                  # 192.168.0.0/16 (RFC1918)
	return 1 if $b0 >= 224;                                # 224.0.0.0/4 multicast + 240.0.0.0/4 reserved

	return 0;
}

sub _is_blocked_ip {
	my $ip = shift;
	return 0 unless defined $ip && length $ip;

	$ip = _normalize_host($ip);
	$ip =~ s/%.+$//; # strip IPv6 zone ID

	# Standard IPv4 or integer/octal/hex IPv4 literal (no DNS lookup)
	if ( $ip =~ /^(?:0x[0-9a-f]+|0[0-7]*|\d+)(?:\.(?:0x[0-9a-f]+|0[0-7]*|\d+)){0,3}$/i ) {
		my $packed = inet_aton($ip);
		return 1 unless defined $packed;
		return _is_blocked_ipv4_bytes($packed);
	}

	# IPv6 literal
	if ( $ip =~ /:/ ) {
		my $packed6 = inet_pton(AF_INET6, $ip);
		return 1 unless defined $packed6 && length($packed6) == 16;

		# :: (unspecified) and ::1 (loopback)
		return 1 if $packed6 eq ("\x00" x 16);
		return 1 if $packed6 eq (("\x00" x 15) . "\x01");

		# IPv4-mapped IPv6 ::ffff:0:0/96 or IPv4-compatible ::/96
		my $prefix12 = substr($packed6, 0, 12);
		if ( $prefix12 eq (("\x00" x 10) . "\xff\xff") || $prefix12 eq ("\x00" x 12) ) {
			return _is_blocked_ipv4_bytes(substr($packed6, 12, 4));
		}

		my ($w0) = unpack('n', $packed6);
		return 1 if ($w0 & 0xfe00) == 0xfc00; # fc00::/7 unique local
		return 1 if ($w0 & 0xffc0) == 0xfe80; # fe80::/10 link-local
		return 1 if ($w0 & 0xff00) == 0xff00; # ff00::/8 multicast
	}

	return 0;
}

sub _is_blocked_redirect_host {
	my $host = _normalize_host(shift);
	return 1 unless length $host;

	return 1 if $host eq 'localhost'
		|| $host =~ /\.localhost$/
		|| $host eq 'localhost.localdomain';
	return 1 if _is_metadata_host($host);
	return 1 if _is_blocked_ip($host);

	return 0;
}

sub _is_same_origin {
	my ($u1, $u2) = @_;
	return 0 unless $u1 && $u2;
	my $s1 = lc($u1->scheme // '');
	my $s2 = lc($u2->scheme // '');
	my $h1 = _normalize_host($u1->host);
	my $h2 = _normalize_host($u2->host);
	my $p1 = $u1->port;
	my $p2 = $u2->port;
	return ($s1 eq $s2 && $h1 eq $h2 && defined($p1) && defined($p2) && $p1 == $p2) ? 1 : 0;
}

sub new_socket {
	my $self = shift;

	# merge with user-defined socket parameters
	push @_, %{$self->options} if $self->options;

	my %args = @_;

	# Validate resolved target IP/host across redirects and against metadata endpoints
	if ( _is_metadata_host($args{Host}) || _is_metadata_host($args{PeerAddr}) ) {
		$log->error("Blocked connection to metadata target");
		$@ = "Blocked connection to metadata target";
		return undef;
	}

	if ( $self->_is_redirect && !$self->use_proxy ) {
		if ( _is_blocked_redirect_host($args{Host}) || (defined $args{PeerAddr} && _is_blocked_redirect_host($args{PeerAddr})) ) {
			$log->error("Blocked redirect to restricted target: " . ($args{PeerAddr} || $args{Host} || 'unknown'));
			$@ = "Blocked redirect to restricted target";
			return undef;
		}
	}

	if ( my $proxy = $self->use_proxy ) {

		main::INFOLOG && $log->info("Using proxy $proxy to connect");

		my ($pserver, $pport) = split /:/, $proxy;

		return Slim::Networking::Async::Socket::HTTP->new(
			@_,
			PeerAddr => $pserver,
			PeerPort => $pport || 80,
		);
	}

	# Create SSL socket if URI is https
	if ( lc($self->request->uri->scheme || '') eq 'https' ) {
		if ( hasSSL() ) {
			# From http://bugs.slimdevices.com/show_bug.cgi?id=18152:
			# We increasingly find servers *requiring* use of the SNI extension to TLS.
			# IO::Socket::SSL supports this and, in combination with the Net:HTTPS 'front-end',
			# will default to using a server name (PeerAddr || Host || PeerHost). But this will fail
			# if PeerAddr has been set to, say, IPv4 or IPv6 address form. And LMS does that through
			# DNS lookup.
			# So we will probably need to explicitly set "SSL_hostname" if we are to succeed with such
			# a server.

			my $ssl_host = $args{SSL_hostname};
			if ( !defined $ssl_host || !length $ssl_host ) {
				$ssl_host = $args{Host};
				$ssl_host = $self->request->uri->host if (!defined $ssl_host || !length $ssl_host) && $self->request && $self->request->uri;
			}
			if ( defined $ssl_host ) {
				if ( $ssl_host =~ /^\[([^\]]+)\](?::\d+)?$/ ) {
					$ssl_host = $1;
				}
				elsif ( $ssl_host !~ /:/ || $ssl_host =~ /^[^:]+:\d+$/ ) {
					$ssl_host =~ s/:\d+$//;
				}
				$args{SSL_hostname} = $ssl_host if length $ssl_host;
			}

			if ( $self->insecureHTTPS ) {
				$args{SSL_verify_mode} //= Net::SSLeay::VERIFY_NONE();
			}
			else {
				$args{SSL_verify_mode}     //= IO::Socket::SSL::SSL_VERIFY_PEER();
				$args{SSL_verifycn_scheme} //= 'http';
				$args{SSL_verifycn_name}   //= $args{SSL_hostname} if defined $args{SSL_hostname};
			}

			if ($self->socks) {
				return Slim::Networking::Async::Socket::HTTPSSocks->new( %{$self->socks}, %args );
			}
			else {
				return Slim::Networking::Async::Socket::HTTPS->new( %args );
			}
		}
		else {
			# Fail closed: never downgrade HTTPS to plaintext HTTP port 80
			$log->error("Cannot connect to https URL without IO::Socket::SSL");
			$@ = "IO::Socket::SSL is required for HTTPS connections";
			return undef;
		}
	}
	elsif ($self->socks) {
		return Slim::Networking::Async::Socket::HTTPSocks->new( %{$self->socks}, @_ );
	}
	else {
		return Slim::Networking::Async::Socket::HTTP->new( @_ );
	}
}

sub use_proxy {
	my $self = shift;

	# Proxy will be used for non-local HTTP requests
	if ( my $proxy = $prefs->get('webproxy') ) {
		my $host   = $self->request->uri->host;
		my $scheme = $self->request->uri->scheme;
		if ( lc($scheme || '') ne 'https' && defined $host && $host !~ /^(?:localhost|127\.0\.0\.1|\[?::1\]?)$/i ) {
			return $proxy;
		}
	}

	return;
}

sub send_request {
	my ( $self, $args, $redirect ) = @_;

	$self->maxRedirect( $args->{maxRedirect} // $prefs->get('maxRedirects') );
	$self->response( undef ) unless $redirect;
	$self->_is_redirect( $redirect ? 1 : 0 );
	$self->_bytes_read( 0 );

	if ( $args->{Timeout} ) {
		$self->timeout( $args->{Timeout} );
	}

	# option to save directly to a file
	if ( $args->{saveAs} ) {
		$self->saveAs( $args->{saveAs} );
	}

	$self->request(
		$args->{request}
		||
		HTTP::Request->new( $args->{method} => $args->{url} )
	);

	if ( $self->request->uri !~ /^https?:/i ) {
		my $error = 'Cannot request non-HTTP URL ' . _safe_uri_string($self->request->uri);
		return $self->_http_error( $error, $args );
	}

	if ( lc($self->request->uri->scheme || '') eq 'https' && !hasSSL() ) {
		my $error = "Can't connect to https URL lack of IO::Socket::SSL: " . _safe_uri_string($self->request->uri);
		return $self->_http_error( $error, $args );
	}

	my $target_host = eval { $self->request->uri->host };
	if ( _is_metadata_host($target_host) ) {
		my $error = 'Blocked request to metadata target: ' . _safe_uri_string($self->request->uri);
		return $self->_http_error( $error, $args );
	}

	if ( $redirect && _is_blocked_redirect_host($target_host) ) {
		my $error = 'Blocked redirect to restricted target: ' . _safe_uri_string($self->request->uri);
		return $self->_http_error( $error, $args );
	}

	if ( (defined $self->request->method && $self->request->method =~ /[\r\n\0]/)
		|| $self->request->uri->as_string =~ /[\r\n\0]/ ) {
		return $self->_http_error( 'Invalid control characters in HTTP request', $args );
	}

	if ( !$self->request->protocol ) {
		$self->request->protocol( 'HTTP/1.0' );
	}

	# the used class is NET::HTTP::Method which now supports HTTP 1.1
	# XXX until we support chunked encoding, force 1.0
	# $self->request->protocol('HTTP/1.0');

	$self->add_headers();

	$self->write_async( {
		host        => $self->request->uri->host,
		port        => $self->request->uri->port,
		content_ref => \&_format_request,
		Timeout     => $self->timeout,
		skipDNS     => ( $self->use_proxy ) ? 1 : 0,
		onError     => \&_http_error,
		onRead      => \&_http_read,
		passthrough => [ $args ],
	} );
}

# add standard request headers
sub add_headers {
	my $self = shift;

	my $headers = $self->request->headers;

	# handle basic auth if username, password provided
	if ( my $userinfo = $self->request->uri->userinfo ) {
		$headers->header( Authorization => 'Basic ' . encode_base64( $userinfo, '' ) );
	}

	my $host = $self->request->uri->host;
	# http://bugs.slimdevices.com/show_bug.cgi?id=18151
	# Host needs port number unless we're using default (http - 80, https - 443).
	# Note that both curl & wget suppress the default port number. Source comment in wget suggests
	# that broken server-side software often doesn't recognize the port argument.
	if ( $self->request->uri->port != $self->request->uri->default_port ) {
		$host .= ':' . $self->request->uri->port;
	}

	# Host doesn't use init_header so it will be changed if we're redirecting
	$headers->header( Host => $host );

	$headers->init_header( 'User-Agent'    => Slim::Utils::Misc::userAgentString() );
	$headers->init_header( Accept          => '*/*' );
	$headers->init_header( 'Cache-Control' => 'no-cache' );
	# only init 'Connection' header if HTTP is 1.0, otherwise leave if to Net::HTTP::Method
	$headers->init_header( Connection      => 'close' ) if $self->request->protocol =~ m|HTTP/1.0|i;

	if ( $headers->header('User-Agent') !~ /^NSPlayer/ ) {
		$headers->init_header( 'Icy-Metadata' => 1 );
	}

	# Add cookies
	if ( !main::SCANNER && $cookieJar ) {
		$cookieJar->add_cookie_header( $self->request );
	}
}

# allow people to access our cookie jar
sub cookie_jar {
	return $cookieJar;
}

sub _format_request {
	my $self = shift;

	my $fullpath = $self->request->uri->path_query;
	$fullpath = "/$fullpath" unless $fullpath =~ /^\//;

	# Proxy requests require full URL
	if ( $self->use_proxy ) {
		$fullpath = $self->request->uri->as_string;
	}

	$fullpath =~ s/[\r\n\0]+//g;

	my $method = $self->request->method;
	$method =~ s/[\r\n\0]+//g;

	my @h;
	$self->request->headers->scan( sub {
		my ($k, $v) = @_;
		$k =~ s/^://;
		if ( $k !~ /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/ ) {
			$log->warn("Skipping invalid HTTP header key");
			return;
		}
		$v = '' unless defined $v;
		$v =~ s/[\r\n\0]+/ /g;
		push @h, $k, $v;
	} );

	# Add POST body if any
	my $content_ref = $self->request->content_ref;
	if ( ref $content_ref ) {
		push @h, $$content_ref;
	}

	# Support HTTP 1.1 and keep-alive
	my ($version) = $self->request->protocol =~ m|HTTP/(\S*)|i;
	$version = '1.0' if $version ne '1.1';
	$self->socket->http_version($version);
	$self->socket->keep_alive(1) if ($version == '1.1' && $self->request->header('Connection') !~ /close/i) ||
									$self->request->header('Connection') =~ /keep-alive/i;

	my $request = $self->socket->format_request(
		$method,
		$fullpath,
		@h,
	);

	return \$request;
}

# After reading headers, some callers may want to continue and
# read the body
sub read_body {
	my $self = shift;
	my $args = shift;

	$self->_bytes_read( 0 );

	$self->socket->set( passthrough => [ $self, $args ] );

	# Timer in case the server never sends any body data
	my $timeout = $self->timeout || $prefs->get('remotestreamtimeout');
	Slim::Utils::Timers::setTimer( $self->socket, Time::HiRes::time() + $timeout, \&_http_read_timeout, $self, $args );

	Slim::Networking::Select::addError( $self->socket, \&_http_socket_error );
	Slim::Networking::Select::addRead( $self->socket, \&_http_read_body );

	# Some or all of the body may have arrived together with the headers. It then sits in Net::HTTP's read buffer,
	# or in IO::Socket::SSL's buffer of decrypted data, and the socket won't become readable for it again. With a
	# server keeping the connection open we would wait until it closes it or the timeout fires, so read it now.
	my $socket = $self->socket;
	if ( $socket->_rbuf_length || ( $socket->can('pending') && $socket->pending() ) ) {
		_http_read_body( $socket, $self, $args );
	}
}

sub suspend_stream {
	my $self = shift;

	Slim::Utils::Timers::killTimers( $self->socket, \&_http_read_timeout );
	Slim::Networking::Select::removeRead( $self->socket );
}

sub resume_stream {
	my $self = shift;

	my $timeout = $self->timeout || $prefs->get('remotestreamtimeout');

	Slim::Utils::Timers::setTimer( $self->socket, Time::HiRes::time() + $timeout, \&_http_read_timeout, $self->socket->get('passthrough') );
	Slim::Networking::Select::addRead( $self->socket, \&_http_read_body );
}

sub _http_socket_error {
	my ( $socket, $self, $args ) = @_;

	Slim::Utils::Timers::killTimers( $socket, \&_http_socket_error );
	Slim::Utils::Timers::killTimers( $socket, \&_http_read_timeout );

	$self->disconnect;

	return $self->_http_error( "Error on HTTP socket: $!", $args );
}

sub _http_error {
	my ( $self, $error, $args ) = @_;

	if ( my $socket = $self->socket ) {
		Slim::Utils::Timers::killTimers( $socket, \&_http_socket_error );
		Slim::Utils::Timers::killTimers( $socket, \&_http_read_timeout );
	}

	if ( $self->fh ) {
		$self->fh->close;
		$self->fh(undef);
	}

	$self->disconnect;

	# Bug 8801, Only print an error if the caller doesn't have an onError handler
	if ( my $ecb = $args->{onError} ) {
		my $passthrough = $args->{passthrough} || [];
		$ecb->( $self, $error, @{$passthrough} );
	}
	else {
		$log->error("Error: [$error]");
	}
}

sub _http_read {
	my ( $self, $args ) = @_;

	my ($code, $mess, @h) = eval { $self->socket->read_response_headers };

	# XXX - this is a hack to work around some mis-configured streaming services.
	#       Net::HTTP::Methods would reject status which don't start with HTTP/.
	#       Some streaming services return "ICY 200 OK" instead. Let's deal with
	#       them in a more relaxed way and give them another try.
	if ($@ && $@ =~ /ICY 200 OK/) {
		($code, $mess, @h) = eval { $self->socket->read_response_headers( laxed => 1 ) };
		@h = $self->socket->_read_header_lines() if !$@ && $code == 200 && $mess =~ /OK/ && !scalar @h;
	}

	if ($@) {
		$self->_http_error( "Error reading headers: $@", $args );
		return;
	}

	if ($code) {
		# headers complete, remove ourselves from select loop
		Slim::Networking::Select::removeError( $self->socket );
		Slim::Networking::Select::removeRead( $self->socket );

		# do we have a previous response from a redirect?
		my $previous = [];
		if ( $self->response ) {
			if ( $self->response->previous ) {
				$previous = $self->response->previous;
			}
			push @{$previous}, $self->response->clone;
		}

		my $headers = HTTP::Headers->new;
		while ( @h ) {
			my ($k, $v) = splice @h, 0, 2;
			$headers->push_header( $k => $v );
		}
		$self->response( HTTP::Response->new( $code, $mess, $headers ) );

		# Save previous response
		$self->response->previous( $previous );

		$self->response->request( $self->request );

		# Save cookies
		$cookieJar->extract_cookies( $self->response ) if !main::SCANNER && $cookieJar;

		if ( main::DEBUGLOG && $log->is_debug ) {

			$log->debug("Headers read. code: $code status: $mess");
			$log->debug( Data::Dump::dump( $self->response->headers ) );
		}

		if ( $code !~ /[23]\d\d/ ) {
			return $self->_http_error( $self->response->status_line, $args );
		}

		# Handle redirects
		if ( $code =~ /^30[1237]$/ ) {

			my $location = $self->response->header('Location');

			# check max redirects
			if ( $location && scalar @{$previous} < $self->maxRedirect ) {

				$self->disconnect;

				my $old_uri = $self->request->uri->clone;
				my $new_uri = URI->new_abs( $location, $old_uri );

				if ( !$new_uri || ($new_uri->scheme || '') !~ /^https?$/i ) {
					return $self->_http_error( 'Blocked redirect to non-HTTP URL: ' . _safe_uri_string($new_uri || $location), $args );
				}

				if ( lc($old_uri->scheme || '') eq 'https' && lc($new_uri->scheme || '') ne 'https' ) {
					return $self->_http_error( 'Blocked HTTPS to HTTP redirect: ' . _safe_uri_string($new_uri), $args );
				}

				if ( lc($new_uri->scheme || '') eq 'https' && !hasSSL() ) {
					return $self->_http_error( "Can't connect to https URL lack of IO::Socket::SSL: " . _safe_uri_string($new_uri), $args );
				}

				if ( _is_blocked_redirect_host($new_uri->host) ) {
					return $self->_http_error( 'Blocked redirect to restricted target: ' . _safe_uri_string($new_uri), $args );
				}

				# Strip sensitive headers on cross-origin redirects
				if ( !_is_same_origin($old_uri, $new_uri) ) {
					$self->request->headers->remove_header('Authorization', 'Proxy-Authorization', 'Cookie');
				}

				# Convert non-GET/HEAD requests to GET on 301, 302, 303
				if ( $code =~ /^30[123]$/ && uc($self->request->method || '') !~ /^(?:GET|HEAD)$/ ) {
					$self->request->method('GET');
					$self->request->content('');
					$self->request->headers->remove_header('Content-Length', 'Content-Type', 'Content-Encoding', 'Transfer-Encoding');
				}

				# change the request object to the new location
				delete $args->{request};
				$self->request->uri( $new_uri );

				if ( main::INFOLOG && $log->is_info ) {
					$log->info(sprintf("Redirecting to %s", _safe_uri_string($self->request->uri)));
				}

				# Does the caller want to modify redirecting URLs?
				if ( $args->{onRedirect} ) {
					my $passthrough = $args->{passthrough} || [];
					$args->{onRedirect}->( $self->request, @{$passthrough} );
				}

				$self->send_request( {
					request => $self->request,
					%{$args},
				}, 1 );

				return;
			}
			else {
				my $error = 'Redirection without location';

				if ($location) {
					$error = ($location =~ /^https/i && !hasSSL()) ? "Can't connect to https URL lack of IO::Socket::SSL: " . _safe_uri_string($location) : 'Redirection limit exceeded';
				}

				$log->warn($error);

				$self->disconnect;

				if ( my $cb = $args->{onError} ) {
					my $passthrough = $args->{passthrough} || [];
					return $cb->( $self, $error, @{$passthrough} );
				}

				return;
			}
		}

		# Enforce max body size before buffering large responses into memory
		if ( !$self->saveAs && !$args->{onStream} && !$args->{readLimit} ) {
			my $max_body = defined $args->{maxBodySize} ? $args->{maxBodySize} : DEFAULT_MAX_BODY_SIZE;
			my $cl = $headers->content_length;
			if ( $max_body && defined $cl && $cl =~ /^\d+$/ && $cl > $max_body ) {
				return $self->_http_error( "Response Content-Length ($cl) exceeds maximum body size ($max_body)", $args );
			}
		}

		# Does the caller want a callback on headers?
		if ( my $cb = $args->{onHeaders} ) {
			my $passthrough = $args->{passthrough} || [];
			return $cb->( $self, @{$passthrough} );
		}

		$self->_bytes_read( 0 );

		# if not, keep going and read the body
		$self->socket->set( passthrough => [ $self, $args ] );

		# Timer in case the server never sends any body data
		my $timeout = $self->timeout || $prefs->get('remotestreamtimeout');
		Slim::Utils::Timers::setTimer( $self->socket, Time::HiRes::time() + $timeout, \&_http_socket_error, $self, $args );

		Slim::Networking::Select::addError( $self->socket, \&_http_socket_error );
		Slim::Networking::Select::addRead( $self->socket, \&_http_read_body );

		# in a *confirmed* keep-alive situation, the whole body might already be in the buffer,
		# so there could be no further event which would lead to an error timeout, so body reading
		# must be forced. But if nothing has been read yet, attempting to force body reading can lead to
		# a false empty body result, so let it to the event loop.
		# Body might also be empty but a keep-alive with no content-length in the response is an error
		# if everything has already been read, _http_body_read will unsubscribe to event loop
		# we just subscrive above ... a bit unefficient
		if ( (!defined $self->response->headers->header('Connection') ||  $self->response->headers->header('Connection') =~ /keep-alive/i)
			&& ($self->socket->_rbuf_length > 0 || $self->socket->_rbuf_length == ($headers->content_length || 0)) )
		{
			_http_read_body( $self->socket, $self, $args )
		}
	}
}

sub _http_read_body {
	my ( $socket, $self, $args ) = @_;

	my $iteration = 1;

	# BODY_READ label allows redo to drain SSL's internal plaintext buffer without
	# recursion. When IO::Socket::SSL decrypts a TLS record it buffers more plaintext
	# than read_entity_body returns in one call. The OS FD then appears unreadable to
	# the event loop (libev), so the callback won't fire again even though data remains.
	# redo re-executes the block from the top, consuming buffered data iteratively.
	BODY_READ: {

		my $result = $socket->read_entity_body( my $buf, BUFSIZE );
		if ( !defined $result ) {
			Slim::Utils::Timers::killTimers( $socket, \&_http_socket_error );
			Slim::Utils::Timers::killTimers( $socket, \&_http_read_timeout );
			return $self->_http_error( "Error reading body: $!", $args );
		}
		return if $result < 0;

		Slim::Utils::Timers::killTimers( $socket, \&_http_socket_error );
		Slim::Utils::Timers::killTimers( $socket, \&_http_read_timeout );

		if ( $result ) {
			$self->_bytes_read( ($self->_bytes_read || 0) + $result );
			main::DEBUGLOG && $log->debug("Read body: [$result] bytes");
		}

		if ( !$self->saveAs && !$args->{onStream} ) {
			my $max_body = defined $args->{maxBodySize} ? $args->{maxBodySize} : DEFAULT_MAX_BODY_SIZE;
			if ( $max_body && ($self->_bytes_read || 0) > $max_body ) {
				return $self->_http_error( "Response body exceeds maximum size ($max_body bytes)", $args );
			}
		}

		# Are we saving directly to a file?
		if ( $result && $self->saveAs && !$self->fh ) {
			open my $fh, '>', $self->saveAs or do {
				return $self->_http_error( 'Unable to open ' . $self->saveAs . " for writing: $!", $args );
			};

			binmode $fh;

			if ( main::DEBUGLOG && $log->is_debug ) {
				$log->debug("Writing response directly to " . $self->saveAs);
			}

			$self->fh( $fh );
		}

		if ( $result && $self->saveAs ) {
			# Write directly to a file
			$self->fh->write( $buf, length $buf ) or do {
				return $self->_http_error( 'Unable to write to ' . $self->saveAs . ": $!", $args );
			};
		}
		elsif ( $args->{onStream} ) {
			# The caller wants a callback on every chunk of data streamed
			my $pt   = $args->{passthrough} || [];
			if ( !$result ) {
				$buf = defined $result ? "" : undef;
			}
			my $more = $args->{onStream}->( $self, \$buf, @{$pt} );

			# onStream callback can signal to stop the stream by returning false
			if ( !$more ) {
				$result = 0;
			}
		}
		else {
			# Add buffer to Response object
			$self->response->add_content( $buf ) if $result;
		}

		# Does the caller want us to quit reading early (i.e. for mp3 frames)?
		if ( $args->{readLimit} && ($self->_bytes_read || 0) >= $args->{readLimit} ) {

			Slim::Utils::Timers::killTimers( $socket, \&_http_socket_error );
			Slim::Utils::Timers::killTimers( $socket, \&_http_read_timeout );
			if ( $self->fh ) {
				$self->fh->close;
				$self->fh(undef);
			}

			# close and remove the socket
			$self->disconnect;

			if ( main::DEBUGLOG && $log->is_debug ) {
				$log->debug(sprintf("Body read (stopped after %d bytes)", $self->_bytes_read || 0));
			}

			if ( my $cb = $args->{onBody} ) {
				my $passthrough = $args->{passthrough} || [];
				return $cb->( $self, @{$passthrough} );
			}
			return;
		}

		my $cl = $self->response->headers->header('Content-Length');
		if ( (defined $result && $result == 0) || (defined $cl && $cl =~ /^\d+$/ && ($self->_bytes_read || 0) >= $cl) ) {
			# if here, we've reached the end of the body

			if ( $self->fh ) {
				$self->fh->close;
				$self->fh(undef);
			}

			# close and remove the socket if not keep-alive
			if ( ($self->response->headers->header('Connection') || '') =~ /close/i ||
				(($self->request->headers->header('Connection') || '') !~ /keep-alive/i && $self->request->protocol =~ m|HTTP/1.0|i) ) {
				$self->disconnect;
				main::DEBUGLOG && $log->debug("closing mode");
			}
			else {
				Slim::Networking::Select::removeError( $self->socket );
				Slim::Networking::Select::removeRead( $self->socket );
				main::DEBUGLOG && $log->debug("keep-alive mode");
			}

			main::DEBUGLOG && $log->debug("Body read");

			if ( my $cb = $args->{onBody} ) {
				my $passthrough = $args->{passthrough} || [];
				$cb->( $self, @{$passthrough} );
			}
		}
		else {
			# More body data to read

			# Some servers may never send EOF, but we want to return whatever data we've read
			my $timeout = $self->timeout || $prefs->get('remotestreamtimeout');
			Slim::Utils::Timers::setTimer( $socket, Time::HiRes::time() + $timeout, \&_http_read_timeout, $self, $args );

			# For SSL sockets, IO::Socket::SSL decrypts whole TLS records at once into an
			# internal buffer. After read_entity_body returns, the OS FD may be empty (all
			# TLS record bytes consumed) even though plaintext data remains in the SSL buffer.
			# The event loop won't fire the callback again in that case, so we redo the read
			# loop ourselves to drain whatever SSL has already buffered.
			if ( $socket->can('pending') && $socket->pending() ) {
				# avoid starving the event loop with a long-running read loop by yielding every 10 iterations
				# this should never be needed, I've never seen more than 2 iterations in testing, but just in case ...
				main::idle() if $iteration++ % 10 == 0;
				redo BODY_READ;
			}
		}

	} # end BODY_READ
}

sub _http_read_timeout {
	my ( $socket, $self, $args ) = @_;

	$log->warn("Timed out waiting for more body data, returning what we have");

	Slim::Utils::Timers::killTimers( $socket, \&_http_socket_error );
	Slim::Utils::Timers::killTimers( $socket, \&_http_read_timeout );
	Slim::Networking::Select::removeError( $socket );
	Slim::Networking::Select::removeRead( $socket );

	# close and remove the socket
	if ( $self->fh ) {
		$self->fh->close;
		$self->fh(undef);
	}
	$self->disconnect;

	if ( my $cb = $args->{onBody} ) {
		my $passthrough = $args->{passthrough} || [];
		$cb->( $self, @{$passthrough} );
	}
}

1;
