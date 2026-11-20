package Slim::Web::HTTP;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2025 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use AnyEvent::Handle;
use CGI::Cookie;
use Digest::SHA1 qw(sha1_base64);
use FileHandle ();
use File::Basename qw(basename);
use File::Spec::Functions qw(:ALL);
use FindBin qw($Bin);
use HTTP::Date qw(time2str);
use HTTP::Daemon ();
{
	# SEC-072: Suppress libwww-perl-daemon/<version> Server header on HTTP::Daemon::ClientConn::send_response
	no warnings 'redefine';
	*HTTP::Daemon::product_tokens = sub { 'Lyrion Music Server' };
}
use HTTP::Headers::ETag;
use HTTP::Status qw(
    RC_FORBIDDEN
	RC_PRECONDITION_FAILED
	RC_UNAUTHORIZED
	RC_MOVED_PERMANENTLY
	RC_NOT_FOUND
	RC_METHOD_NOT_ALLOWED
	RC_OK
	RC_NOT_MODIFIED
);

use MIME::Base64;
use MIME::QuotedPrint;
use Scalar::Util qw(blessed);
use Socket qw(:crlf SOMAXCONN SOL_SOCKET SO_SNDBUF inet_ntoa);
use Storable qw(thaw);
use URI;

use Slim::Networking::Select;
use Slim::Player::HTTP;
use Slim::Music::Info;
use Slim::Utils::Errno;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Network;
use Slim::Utils::OSDetect;
use Slim::Utils::Strings qw(string);
use Slim::Utils::Unicode;
use Slim::Web::HTTP::ClientConn;
use Slim::Web::Pages;
use Slim::Web::Graphics;
use Slim::Web::JSONRPC;
use Slim::Web::Cometd;
use Slim::Web::Time;
use Slim::Utils::Prefs;

use constant HALFYEAR	 => 60 * 60 * 24 * 180;

use constant METADATAINTERVAL => 32768;
use constant MAXCHUNKSIZE     => 32768;

# This used to be 0.05s but the CPU load associated with such fast retries is
# really noticeable when playing remote streams. I guess that it is possible
# that certain combinations of pipe buffers in a transcoding pipeline
# might get caught by this but I have not been able to think of any - Alan.
use constant RETRY_TIME       => 0.40; # normal retry time

use constant MAXKEEPALIVES         => 100;  # bounded keepalive requests per connection
use constant KEEPALIVETIMEOUT      => 75;
use constant MAX_HEADER_COUNT      => 100;
use constant MAX_REQUEST_BODY_SIZE => 16 * 1024 * 1024;
use constant MAX_PARAM_COUNT       => 500;
use constant AUTH_FAIL_WINDOW => 60;
use constant AUTH_FAIL_LIMIT  => 10;
use constant AUTH_BLOCK_SECS  => 60;
use constant AUTH_CACHE_MAX   => 1000;

# Package variables

my $openedport = undef;
my $http_server_socket;
my $connected = 0;
my %openSockets = ();
my %authFailures = ();

my %RESERVED_PARAMS = map { $_ => 1 } qw(
	cookies Content-Type userAgent url_query content contentAsFh
	myClientState playermodel params pageAntiCSRFToken skinOverride
	webroot path host serverResizesArt revision nosetup noserver
);

my $SCANNER_AUTH_TOKEN = _generateEphemeralToken();

sub _generateEphemeralToken {
	my $raw = '';
	if (open my $urandom, '<:raw', '/dev/urandom') {
		read($urandom, $raw, 32);
		close $urandom;
	}
	if (length($raw) == 32) {
		return unpack('H*', $raw);
	}
	return Digest::SHA1::sha1_hex($$ . ':' . Time::HiRes::time() . ':' . rand() . ':' . {});
}

sub scannerAuthToken {
	return $SCANNER_AUTH_TOKEN;
}

sub _constantTimeEq {
	my ($a, $b) = @_;
	return 0 unless defined $a && defined $b;
	use bytes;
	my $lenA   = length($a);
	my $lenB   = length($b);
	my $maxLen = $lenA > $lenB ? $lenA : $lenB;
	my $diff   = $lenA ^ $lenB;
	for (my $i = 0; $i < $maxLen; $i++) {
		my $ca = $i < $lenA ? ord(substr($a, $i, 1)) : 0;
		my $cb = $i < $lenB ? ord(substr($b, $i, 1)) : 0;
		$diff |= ($ca ^ $cb);
	}
	return $diff == 0 ? 1 : 0;
}

sub _htmlEscape {
	my ($str) = @_;
	return '' unless defined $str;
	$str =~ tr/\r\n\0//d;
	$str =~ s/&/&amp;/g;
	$str =~ s/</&lt;/g;
	$str =~ s/>/&gt;/g;
	$str =~ s/"/&quot;/g;
	$str =~ s/'/&#39;/g;
	return $str;
}

sub _isExternalSettingsPeer;
sub _closeErroredSocket;

my $IMAGE_RESIZE_REGEX = qr/\/[\w-]+_(X|\d+)x(X|\d+)
	(?:_([mpsSfFco]))?    # resizeMode, given by a single character
	(?:_[\da-fA-F]+)?     # background color, optional
/x;

our %outbuf = (); # a hash for each writeable socket containing a queue of output segments
                 #   each segment is a hash of a ref to data, an offset and a length

our %sendMetaData   = ();
our %metaDataBytes  = ();
our %streamingFiles = ();
our %peeraddr       = ();
our %peerclient     = ();
our %keepAlives     = ();

my  $skinMgr;

# Cookies to be persisted on the server side prefs: some browsers are resetting cookies regularly
use constant PERSIST_COOKIES => [
	'Squeezebox-albumView',
	'Squeezebox-expandPlayerControl',
	'Squeezebox-expanded-MY_MUSIC',
	'Squeezebox-expanded-FAVORITES',
	'Squeezebox-expanded-PLUGINS',
	'Squeezebox-expanded-PLUGIN_MY_APPS_MODULE_NAME',
	'Squeezebox-expanded-RADIO',
];

# we call these whenever we close a connection
our @closeHandlers = ();

my $log = logger('network.http');

my $prefs = preferences('server');

sub _isExternalSettingsPeer {
	my ($httpClient) = @_;
	my $peer = $peeraddr{$httpClient};
	return 0 unless $peer;
	return 0 if Slim::Utils::Network::ip_is_host($peer);
	return 0 unless $prefs->get('protectSettings') && !$prefs->get('authorize');
	return ( Slim::Utils::Network::ip_is_gateway($peer) || Slim::Utils::Network::ip_on_different_network($peer) ) ? 1 : 0;
}

sub _closeErroredSocket {
	my ($httpClient) = @_;
	return unless $httpClient;
	delete $keepAlives{$httpClient};
	closeStreamingSocket($httpClient, 1);
}

sub _applySecurityHeaders {
	my ($response) = @_;
	return unless $response;
	$response->header('X-Content-Type-Options' => 'nosniff')
		unless defined $response->header('X-Content-Type-Options');
	$response->header('X-Frame-Options' => 'SAMEORIGIN')
		unless defined $response->header('X-Frame-Options');
	$response->header('Referrer-Policy' => 'same-origin')
		unless defined $response->header('Referrer-Policy');
	$response->header('Content-Security-Policy' =>
		"default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: http:; media-src 'self' http: https:; font-src 'self' data:; connect-src 'self' ws: wss:; frame-ancestors 'self'; object-src 'none'; base-uri 'self'")
		unless defined $response->header('Content-Security-Policy');
}

sub _redactedRequestString {
	my ($request) = @_;
	return '' unless $request;
	my $str = $request->as_string();
	$str =~ s/^((?:Authorization|Proxy-Authorization|Cookie|X-Scanner(?:-Token)?|X-Slim-CSRF)\s*:\s*).*$/$1\[REDACTED\]/mig;
	return $str;
}

sub _renderErrorPage {
	my ($code, $templatePath, $fallbackTitle, $params) = @_;
	my $body = (main::WEBUI && $templatePath) ? filltemplatefile($templatePath, $params) : undef;
	if (!defined($body) || ref($body) ne 'SCALAR' || !defined($$body) || $$body eq '') {
		my $msg  = _htmlEscape($fallbackTitle || "HTTP $code");
		my $html = "<h1>$code $msg</h1>";
		$body    = \$html;
	}
	return $body;
}

sub _sanitizeDispositionFilename {
	my ($name) = @_;
	$name = '' unless defined $name;
	$name = Slim::Utils::Misc::unescape($name);
	$name =~ tr/\000-\037\177//d;
	$name =~ s{^.*[/\\]}{}s;
	$name =~ tr/"\/\\;//d;
	$name = 'download' if $name eq '' || $name =~ /^\.+$/;
	return $name;
}

sub _formatSetCookie {
	my (%args) = @_;
	my $cookie = CGI::Cookie->new(%args);
	my $str = $cookie->as_string();
	$str .= '; SameSite=Lax' unless $str =~ /(?:^|;\s*)SameSite=/i;
	return $str;
}

sub _isAuthRateLimited {
	my ($peer) = @_;
	return 0 unless defined $peer && $peer ne '';
	my $entry = $authFailures{$peer} || return 0;
	my $now   = time();
	if ($entry->{blockedUntil} && $entry->{blockedUntil} > $now) {
		return int($entry->{blockedUntil} - $now) || 1;
	}
	return 0;
}

sub _recordAuthFailure {
	my ($peer) = @_;
	return unless defined $peer && $peer ne '';
	my $now = time();
	if (scalar(keys %authFailures) >= AUTH_CACHE_MAX) {
		for my $k (keys %authFailures) {
			my $e = $authFailures{$k};
			if ((!$e->{blockedUntil} || $e->{blockedUntil} <= $now) && ($now - $e->{windowStart} > AUTH_FAIL_WINDOW)) {
				delete $authFailures{$k};
			}
		}
	}
	my $entry = $authFailures{$peer};
	if (!$entry || ($now - $entry->{windowStart} > AUTH_FAIL_WINDOW)) {
		$entry = { count => 0, windowStart => $now, blockedUntil => 0 };
		$authFailures{$peer} = $entry;
	}
	$entry->{count}++;
	if ($entry->{count} >= AUTH_FAIL_LIMIT) {
		$entry->{blockedUntil} = $now + AUTH_BLOCK_SECS;
	}
}

sub _clearAuthFailure {
	my ($peer) = @_;
	delete $authFailures{$peer} if defined $peer && $peer ne '';
}

sub _sendImmediateError {
	my ($httpClient, $response, $code, $templatePath, $fallbackTitle, $params, $reason) = @_;
	if (Scalar::Util::reftype($httpClient) && Scalar::Util::reftype($httpClient) eq 'GLOB' && !defined ${*$httpClient}{'httpd_client_proto'}) {
		${*$httpClient}{'httpd_client_proto'} = 1001;
	}
	_applySecurityHeaders($response);
	$response->code($code);
	$response->content_type('text/html');
	$response->header('Connection' => 'close');
	$response->content_ref(_renderErrorPage($code, $templatePath, $fallbackTitle, $params));
	$response->protocol('HTTP/1.1') unless defined $response->protocol();
	$httpClient->send_response($response);
	closeHTTPSocket($httpClient, 0, $reason);
}

# initialize the http server
sub init {

	if ( main::WEBUI ) {
		require Slim::Web::HTTP::CSRF;
		require Slim::Web::Template::SkinManager;
		$skinMgr = Slim::Web::Template::SkinManager->new();

		# Initialize all the web page handlers.
		Slim::Web::Pages::init();
	}
	else {
		require Slim::Web::Template::NoWeb;
		$skinMgr = Slim::Web::Template::NoWeb->new();
	}

	# Initialize graphics resizing
	Slim::Web::Graphics::init();

	# Initialize JSON RPC
	Slim::Web::JSONRPC::init();

	# Initialize Cometd
	Slim::Web::Cometd::init();

	# Initialize '/time' endpoint - /time/tz serves SqueezeOS
	Slim::Web::Time::init();
}

sub init2 {
	# open HTTP port if specified
	# split into second init function so this can be performed after all server init is complete
	if ($prefs->get('httpport')) {
		Slim::Web::HTTP::openport($prefs->get('httpport'), $::httpaddr);
	} else {
		$openedport = 0; # init complete but no port opened
	}
}

sub getSkinManager {
	return $skinMgr;
}

sub openport {
	my ($listenerport, $listeneraddr) = @_;

	my %tested;
	my $testSocket;

	# start our listener
	foreach my $port ($listenerport, 9000..9010, 9100, 8000, 10000) {

		next if $tested{$port};

		$openedport    = $port;
		$tested{$port} = 1;

		if ( $testSocket = IO::Socket::INET->new(Proto     => "tcp",
				PeerAddr  => 'localhost',
				PeerPort  => $port) )
		{
			$testSocket->close;
		}

		else {

			$http_server_socket = HTTP::Daemon->new(
				LocalPort => $port,
				LocalAddr => $listeneraddr,
				Listen    => SOMAXCONN,
				ReuseAddr => 1,
				Reuse => 1,
				Timeout   => 0.001,
			) and last;
		}

		$log->error("Can't setup the listening port $port for the HTTP server: $!");
	}

	# if none of our ports could be opened, we'll have to give up
	if (!$http_server_socket) {

		$log->logdie("Running out of good ideas for the listening port for the HTTP server - giving up.");
	}

	defined(Slim::Utils::Network::blocking($http_server_socket,0)) || $log->logdie("Cannot set port nonblocking");

	Slim::Networking::Select::addRead($http_server_socket, \&acceptHTTP);

	main::INFOLOG && $log->info("Server $0 accepting http connections on port $openedport");

	if ($openedport != $listenerport) {

		$log->error("Previously configured port $listenerport was busy - we're now using port $openedport instead");

		# we might want to push this message in the user's face
		if (main::ISWINDOWS) {
			$log->error("Please make sure your firewall does allow access to port $openedport!");
		}

		$prefs->set('httpport', $openedport) ;
	}

	if ( $listeneraddr ) {
		$prefs->set( httpaddr => $listeneraddr );
	}
}

sub adjustHTTPPort {

	return unless defined $openedport; # only adjust once init is complete

	# do this on a timer so current page can be updated first and it executed outside select
	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.5, \&_adjustHTTPPortCallback);
}

sub _adjustHTTPPortCallback {

	# if we've already opened a socket, let's close it
	if ($openedport) {
		main::INFOLOG && $log->info("Closing http server socket");

		Slim::Networking::Select::removeRead($http_server_socket);

		$http_server_socket->close();
		undef($http_server_socket);
		$openedport = 0;
	}

	# open new port if specified
	if ($prefs->get('httpport')) {
		Slim::Web::HTTP::openport($prefs->get('httpport'), $::httpaddr);
	}
}

sub connectedSocket {
	return $connected;
}

sub acceptHTTP {
	# try and pull the handle
	my $httpClient = $http_server_socket->accept('Slim::Web::HTTP::ClientConn') || do {

		main::INFOLOG && $log->info("Did not accept connection, accept returned nothing");
		return;
	};

	if (!defined(Slim::Utils::Network::blocking($httpClient, 0))) {
		$log->warn("Cannot set accepted HTTP socket nonblocking: $!");
		$httpClient->close();
		return;
	}

	binmode($httpClient);

	my $peer = $httpClient->peeraddr();

	if ($httpClient->connected() && $peer) {

		$peer = inet_ntoa($peer);

		# Check if source address is valid
		if (!($prefs->get('filterHosts')) ||
		     (Slim::Utils::Network::isAllowedHost($peer))) {

			# Timeout for reads from the client.  HTTP::Daemon in get_request
			# will call select(,,,10) but should not block long
			# as we already know the socket is ready for reading
			$httpClient->timeout(5);

			$peeraddr{$httpClient}    = $peer;
			$openSockets{$httpClient} = 1;

			Slim::Networking::Select::addRead($httpClient, \&processHTTP);
			Slim::Networking::Select::addError($httpClient, \&_closeErroredSocket);

			$connected++;

			Slim::Utils::Timers::setTimer(
				$httpClient,
				time() + KEEPALIVETIMEOUT,
				\&closeHTTPSocket,
				0,
				'idle timeout',
			);

			if ( main::INFOLOG && $log->is_info ) {
				$log->info("Accepted connection $connected from $peeraddr{$httpClient}:" . $httpClient->peerport);
			}

		} else {

			$log->warn("Did not accept HTTP connection from $peer, unauthorized source");

			$httpClient->close();
			undef($httpClient);
		}

	} else {

		$log->warn("Did not accept connection, couldn't get peer addr");
		$httpClient->close();
		undef($httpClient);
	}
}

sub skins {
	$skinMgr->skins(@_);
}

# Handle an HTTP request
sub processHTTP {
	my $httpClient = shift || return;

	my $isDebug = ( main::DEBUGLOG && $log->is_debug ) ? 1 : 0;

### OLD ORDER ###
	# Set the request date (write $request)
	# CSRF auth code management (write $request)
	# Read cookies (write $params)
	# Icy-MetaData (write sendMetaData)
	# Create response (from $request but it's a ref so?)
	# Log headers
	# if get/head/post
	## Icy-MetaData (write sendMetaData)
	## Authorization header (returns if nok)
	## Parse URI (write $params)
	## Skins (write params & path, redirected if nok)
	## More CSRF mgmt (looks at the modified path)
	# else
	## Send bad request
### NEW ORDER ###
	# Create response (from $request but it's a ref so?)
	# Log raw headers
	# if get/head/post
	## Authorization header (returns if nok)
	## Persistent connection (write $response $keepAlive)
	## Set the request date (write $request)
	## CSRF auth code management (write $request)
	## Read cookies (write $params)
	## Icy-MetaData (write sendMetaData)
	## Parse URI (write $params)
	## Skins (write params & path, redirected if nok)
	## More CSRF mgmt (looks at the modified path)
	## Log processed headers
	# else
	## Send bad request

	# Store the time we started processing this request
	$httpClient->start_time( Time::HiRes::time() );

	# Remove keep-alive timeout
	Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );

	main::DEBUGLOG && $isDebug && $log->info("Reading request...");

	# SEC-076: Pre-check raw headers before HTTP::Daemon::get_request() can deduplicate Content-Length lines
	my $rawPreview = $httpClient->can('read_buffer') ? $httpClient->read_buffer() : '';
	$rawPreview = '' unless defined $rawPreview;
	$rawPreview =~ s/^(?:\r?\n)+//;
	if ($rawPreview !~ /\r?\n\r?\n/) {
		my $peek = '';
		my $recvRes;
		while (1) {
			$recvRes = recv($httpClient, $peek, 16384, Socket::MSG_PEEK);
			last if defined($recvRes) || $! != EINTR;
		}
		if (defined($recvRes) && defined($peek) && length($peek)) {
			$rawPreview .= $peek;
			$rawPreview =~ s/^(?:\r?\n)+//;
		}
	}
	if ($rawPreview =~ /\A(.*?)\r?\n\r?\n/s) {
		my $rawHdrs = $1;
		my @clMatches = ($rawHdrs =~ m{^Content-Length[ \t]*:}mig);
		if (@clMatches > 1) {
			$log->warn("Rejected duplicate Content-Length header from " . ($peeraddr{$httpClient} || 'unknown'));
			my $errResp = HTTP::Response->new();
			$errResp->protocol('HTTP/1.1');
			_sendImmediateError($httpClient, $errResp, 400, undef, 'Bad Request', {}, 'duplicate Content-Length header');
			return;
		}
	}

	my $request    = $httpClient->get_request(1); # PERF-002: Parse headers only before reading body
	# socket half-closed from client
	if (!defined $request) {

		my $reason = $httpClient->reason || 'unknown error reading request';

		if ( main::INFOLOG && $log->is_info ) {
			$log->info("Client at $peeraddr{$httpClient}:" . $httpClient->peerport . " disconnected. ($reason)");
		}

		closeHTTPSocket($httpClient, 0, $reason);
		return;
	}


	if ( main::INFOLOG && $log->is_info ) {
		$log->info(
			"HTTP request: from $peeraddr{$httpClient}:" . $httpClient->peerport . " ($httpClient) for " .
			join(' ', ($request->method(), $request->protocol(), $request->uri()))
		);
	}

	my $headerCount = 0;
	$request->headers->scan(sub { $headerCount++ });
	if ($headerCount > MAX_HEADER_COUNT) {
		$log->warn("Too many request headers ($headerCount) from " . ($peeraddr{$httpClient} || 'unknown'));
		my $resp = HTTP::Response->new(431);
		$resp->protocol('HTTP/1.1');
		_applySecurityHeaders($resp);
		$resp->header('Connection' => 'close');
		$httpClient->send_response($resp);
		closeHTTPSocket($httpClient, 0, 'too many headers');
		return;
	}

	if ( main::DEBUGLOG && $isDebug ) {
		$log->debug("Raw request headers: [\n" . _redactedRequestString($request) . "]");
	}

	# this will hold our context and is used to fill templates
	my $params = {};
	$params->{'userAgent'} = $request->header('user-agent');

	# this bundles up all our response headers and content
	my $response = HTTP::Response->new();

	$response->protocol('HTTP/1.1');
	$response->request($request);
	_applySecurityHeaders($response);

	# Reject ambiguous Content-Length / Transfer-Encoding headers (CL/TE smuggling)
	my @teHeaders = $request->header('Transfer-Encoding');
	my @clHeaders = $request->header('Content-Length');
	my $clInvalid = 0;
	if (@teHeaders) {
		$clInvalid = 1;
	} elsif (@clHeaders > 1) {
		$clInvalid = 1;
	} elsif (@clHeaders == 1) {
		my $clVal = $clHeaders[0];
		$clInvalid = 1 if !defined($clVal) || $clVal !~ /^\s*\d+\s*$/;
	}
	if ($clInvalid) {
		$log->warn("Rejected ambiguous Content-Length/Transfer-Encoding from " . ($peeraddr{$httpClient} || 'unknown'));
		_sendImmediateError($httpClient, $response, 400, undef, 'Bad Request', $params, 'invalid CL/TE headers');
		return;
	}

	# PERF-002: Enforce bounded request body size before reading body or dispatching handlers
	my $clBytes = (@clHeaders == 1 && defined($clHeaders[0]) && $clHeaders[0] =~ /^\s*(\d+)\s*$/)
		? ($1 + 0)
		: 0;
	if ($clBytes > MAX_REQUEST_BODY_SIZE) {
		$log->warn("Rejected oversized request body (CL=$clBytes) from " . ($peeraddr{$httpClient} || 'unknown'));
		_sendImmediateError($httpClient, $response, 413, undef, 'Payload Too Large', $params, 'payload too large');
		return;
	}

	for my $e ($request->header('Expect')) {
		if (lc($e) =~ /^\s*100-continue\s*$/) {
			if ($httpClient->can('send_status_line') && $httpClient->can('send_crlf')) {
				$httpClient->send_status_line(100);
				$httpClient->send_crlf;
			}
		} else {
			_sendImmediateError($httpClient, $response, 417, undef, 'Expectation Failed', $params, 'unsupported Expect header');
			return;
		}
	}

	if ($clBytes > 0) {
		use bytes;
		my $buf = '';
		if ($httpClient->can('read_buffer')) {
			my $rbuf = $httpClient->read_buffer('');
			if (defined($rbuf) && length($rbuf)) {
				if (length($rbuf) > $clBytes) {
					$buf = substr($rbuf, 0, $clBytes);
					$httpClient->read_buffer(substr($rbuf, $clBytes));
				} else {
					$buf = $rbuf;
				}
			}
		}

		my $readTimeout = ($httpClient->can('timeout') && $httpClient->timeout()) || 5;
		while (length($buf) < $clBytes) {
			my $remaining = $clBytes - length($buf);
			my $chunkSize = $remaining > 65536 ? 65536 : $remaining;
			my $n = sysread($httpClient, $buf, $chunkSize, length($buf));
			if (!defined $n) {
				next if $! == EINTR;
				if ($! == EWOULDBLOCK || $!{EAGAIN}) {
					my $fd = fileno($httpClient);
					if (!defined $fd) {
						$log->warn("Aborted request body read on invalid descriptor from " . ($peeraddr{$httpClient} || 'unknown'));
						closeHTTPSocket($httpClient, 0, 'invalid body socket');
						return;
					}
					my $rin = '';
					vec($rin, $fd, 1) = 1;
					my $ready;
					while (1) {
						$ready = select($rin, undef, undef, $readTimeout);
						last if defined($ready) || $! != EINTR;
						$rin = '';
						vec($rin, $fd, 1) = 1;
					}
					if (!$ready) {
						my $reason = defined($ready) ? 'request body timeout' : "request body select error: $!";
						$log->warn("Aborted request body read ($reason) from " . ($peeraddr{$httpClient} || 'unknown'));
						closeHTTPSocket($httpClient, 0, $reason);
						return;
					}
					next;
				}
				my $err = "$!";
				$log->warn("Aborted request body sysread error ($err) from " . ($peeraddr{$httpClient} || 'unknown'));
				closeHTTPSocket($httpClient, 0, "body read error: $err");
				return;
			}
			if ($n == 0) {
				$log->warn("Aborted request body read on premature EOF (got " . length($buf) . " of $clBytes bytes) from " . ($peeraddr{$httpClient} || 'unknown'));
				closeHTTPSocket($httpClient, 0, 'premature EOF reading body');
				return;
			}
		}

		$request->content($buf);
	}

	my $bodyBytes = defined($request->content)
		? do { use bytes; length($request->content) }
		: 0;
	if ($bodyBytes > MAX_REQUEST_BODY_SIZE) {
		$log->warn("Rejected oversized request body (CL=$clBytes, body=$bodyBytes) from " . ($peeraddr{$httpClient} || 'unknown'));
		_sendImmediateError($httpClient, $response, 413, undef, 'Payload Too Large', $params, 'payload too large');
		return;
	}

	# handle stuff we know about or abort
	if ($request->method() eq 'GET' || $request->method() eq 'HEAD' || $request->method() eq 'POST'
		|| $request->method() eq 'PUT' || $request->method() eq 'DELETE') {

		# Manage authorization with failure rate limiting
		my $peer       = $peeraddr{$httpClient} || 'unknown';
		my $authorized = !$prefs->get('authorize');

		if ( !$authorized ) {
			if ( my $retryAfter = _isAuthRateLimited($peer) ) {
				$log->warn("Rate-limiting HTTP auth attempts from $peer");
				$response->header('Retry-After' => $retryAfter);
				_sendImmediateError($httpClient, $response, 429, undef, 'Too Many Requests', $params, 'auth rate limited');
				return;
			}

			my $hasCreds = 0;
			if (my ($user, $pass) = $request->authorization_basic()) {
				$hasCreds   = 1;
				$authorized = checkAuthorization($user, $pass, $request);
			}
			elsif ( $request->header('X-Scanner-Token') || $request->header('X-Scanner') ) {
				$hasCreds   = 1;
				$authorized = checkAuthorization('', '', $request);
			}
			elsif ( defined $request->header('Authorization') ) {
				$hasCreds = 1;
			}

			if ($authorized) {
				_clearAuthFailure($peer) if $hasCreds;
			} elsif ($hasCreds) {
				_recordAuthFailure($peer);
			}
		}

		# no Valid authorization supplied!
		if (!$authorized) {
			$response->www_authenticate(sprintf('Basic realm="%s"', string('SQUEEZEBOX_SERVER')));
			_sendImmediateError($httpClient, $response, RC_UNAUTHORIZED, 'html/errors/401.html', 'Unauthorized', $params, 'unauthorized');
			return;
		}


		# HTTP/1.1 Persistent connections or HTTP 1.0 Keep-Alives
		# XXX - MAXKEEPALIVES should be a preference
		# This always add a Connection: close header if we want the connection to be closed.
		my $connHdr = $request->header('Connection');
		$connHdr =~ tr/\r\n\0//d if defined $connHdr;
		my $hasClose     = defined($connHdr) && $connHdr =~ /(?:^|[,\s])close(?:$|[,\s])/i;
		my $hasKeepAlive = defined($connHdr) && $connHdr =~ /(?:^|[,\s])keep-alive(?:$|[,\s])/i;

		if (MAXKEEPALIVES > 0 && defined $keepAlives{$httpClient} && $keepAlives{$httpClient} >= MAXKEEPALIVES) {

			# This will close the client socket & remove the
			# counter in sendResponse()
			delete $keepAlives{$httpClient};
			$response->header('Connection' => 'close');

			main::DEBUGLOG && $isDebug && $log->info("Hit MAXKEEPALIVES, will close connection.");

		} else {

			if ($httpClient->proto_ge('1.1')) {

				# 1.1 defaults to persistent unless Connection: close
				if ($hasClose) {
					delete $keepAlives{$httpClient};
					$response->header('Connection' => 'close');
				} else {
					$keepAlives{$httpClient}++;
					$response->header('Connection' => 'keep-alive') if $hasKeepAlive;
				}

			} else {

				# HTTP/1.0 is persistent ONLY if Connection: keep-alive is requested without close
				if ($hasKeepAlive && !$hasClose) {
					$keepAlives{$httpClient}++;
					$response->header('Connection' => 'keep-alive');
				} else {
					delete $keepAlives{$httpClient};
					$response->header('Connection' => 'close');
				}
			}
		}

		# Set the request time - for If-Modified-Since
		$request->client_date(time());

		# extract the URI and raw path
		# the path is modified below for skins and stuff
		my $uri   = $request->uri();
		my $path  = $uri->path();

		my $csrfProtectionLevel = main::WEBUI && $prefs->get('csrfProtectionLevel');

		if ( main::WEBUI && $csrfProtectionLevel ) {
			# remove our special X-Slim-CSRF header if present
			$request->remove_header("X-Slim-CSRF");

			# store CSRF auth code in fake request header if present
			if ( defined($uri) && ($uri =~ m|^(.*)\;cauth\=([0-9]+:[0-9a-f]{64})$| ) ) {

				my $plainURI = $1;
				my $csrfAuth = $2;

				if ( main::DEBUGLOG && $isDebug ) {
					$log->debug("Found CSRF auth token in URI, resetting request URI to \"$plainURI\"");
				}

				# change the URI so later code doesn't "see" the cauth part
				$request->uri($plainURI);

				# store the cauth code in the request object (headers are handy!)
				$request->push_header("X-Slim-CSRF",$csrfAuth);

				# immediately refresh $uri and $path after stripping ;cauth=
				$uri  = $request->uri();
				$path = $uri->path();
			}
		}

		main::DEBUGLOG && $isDebug && $log->debug("Raw path is [$path]");

		# Route validation and single-pass unescape before rawFunc / protectSettings / skin routing
		my $rawUriPath = defined($uri) ? $uri->path() : '';
		my $unescapedPath = defined($path) ? Slim::Utils::Misc::unescape($path) : '';
		if ( $rawUriPath =~ /(?:%00|%2e|\.\.)/i
			|| $path =~ /[\0\r\n\\]/
			|| $unescapedPath =~ /[\0\r\n\\]|(?:^|\/)\.\.(?:$|\/)/
		) {
			$log->warn("Rejected unsafe request path from " . ($peeraddr{$httpClient} || 'unknown') . ": [$rawUriPath]");
			_sendImmediateError($httpClient, $response, 400, undef, 'Bad Request', $params, 'invalid path');
			return;
		}
		$path = $unescapedPath;

		checkCORS($request, $response);

		# break here for raw HTTP code
		# we hand the $response object only, it contains the almost unmodified request
		# we took care above of basic HTTP stuff and authorization
		# $rawFunc shall call addHTTPResponse
		if (my $rawFunc = Slim::Web::Pages->getRawFunction($path)) {

			# Enforce protectSettings before rawFunc
			my $normRawPath = $path;
			$normRawPath =~ s|^/+||;
			my $rawPageFunc = Slim::Web::Pages->getPageFunction($normRawPath) if $normRawPath !~ m{^imageproxy/};
			if ( main::WEBUI && _isExternalSettingsPeer($httpClient)
				&& ($normRawPath =~ m{^settings(?:/|$)}i || ($rawPageFunc && !ref($rawPageFunc) && $rawPageFunc->isa('Slim::Web::Settings')))
			) {
				my $hostIP = Slim::Utils::IPDetect::IP();
				$log->error("Access to settings pages is restricted to the local network or localhost: $peeraddr{$httpClient} -> $hostIP ($path)");
				_sendImmediateError($httpClient, $response, RC_FORBIDDEN, 'html/errors/403.html', 'Forbidden', $params, 'settings protected');
				return;
			}

			# Enforce non-HEAD gate before rawFunc
			if ( $request->method() eq 'HEAD' ) {
				_sendImmediateError($httpClient, $response, RC_METHOD_NOT_ALLOWED, 'html/errors/405.html', 'Method Not Allowed', $params, 'HEAD on raw function');
				return;
			}

			main::DEBUGLOG && $isDebug && $log->info("Handling [$path] using raw function");

			if (ref($rawFunc) eq 'CODE') {

				# XXX: should this use eval?
				&{$rawFunc}($httpClient, $response);
				return;
			}
		}

		#PUT and DELETE are only considered valid for a raw function, so we will not go any further for these HTTP verbs
		if ( $request->method() eq 'PUT' || $request->method() eq 'DELETE') {

			$log->is_warn && $log->warn("Bad Request: [" . join(' ', ($request->method, $request->uri)) . "]");

			_sendImmediateError($httpClient, $response, RC_METHOD_NOT_ALLOWED, 'html/errors/405.html', 'Method Not Allowed', $params, 'method not allowed');
			return;
		}

		# Dont' process cookies for graphics, stylesheets etc.
		if (shouldHandleCookies($path)) {
			if ( my $cookie = $request->header('Cookie') ) {
				$params->{'cookies'} = { CGI::Cookie->parse($cookie) };

				# define the precacheHiDPIArtwork pref if we're dealing with a high DPI monitor
				if ( $params->{'cookies'}->{'Squeezebox-enableHiDPI'} ) {
					my $hidpi = $params->{'cookies'}->{'Squeezebox-enableHiDPI'}->value;
					if ( defined($hidpi) && $hidpi =~ /^\d+(?:\.\d+)?$/ && $hidpi > 1 && !defined $prefs->get('precacheHiDPIArtwork') ) {
						$prefs->set('precacheHiDPIArtwork', 1);
					}
				}

				foreach (@{PERSIST_COOKIES()}) {
					if ( $params->{'cookies'}->{$_} ) {
						my $cookieVal = $params->{'cookies'}->{$_}->value;
						if ( defined($cookieVal) && length($cookieVal) <= 64 && $cookieVal =~ /^[A-Za-z0-9_.:-]*$/ ) {
							my $curVal = $prefs->get($_);
							$curVal = '' unless defined $curVal;
							if ($cookieVal ne $curVal) {
								$prefs->set($_, $cookieVal);
							}
						}
					}
				}
			}
		}

		# Icy-MetaData
		$sendMetaData{$httpClient} = 0;

		if ($request->header('Icy-MetaData')) {
			$sendMetaData{$httpClient} = 1;
		}

		# parse out URI
		my $query = ($request->method() eq "POST") ? $request->content() : $uri->query();

		$params->{url_query} = $query;

		$params->{content} = $request->content();

		my ($queryWithArgs, $queryToTest, $providedPageAntiCSRFToken);
		# CSRF: make list of params passed by HTTP client
		my %csrfReqParams;

		# XXX - unfortunately Lyrion Music Server uses a query form
		# that can have a key without a value, yet it's
		# differnet from a key with an empty value. So we have
		# to parse out like this.
		if ($query) {

			my $paramCount = 0;

			foreach my $param (split /\&/, $query) {

				if (++$paramCount > MAX_PARAM_COUNT) {
					$log->warn("Too many request parameters ($paramCount) from " . ($peeraddr{$httpClient} || 'unknown'));
					_sendImmediateError($httpClient, $response, 400, undef, 'Bad Request', $params, 'too many parameters');
					return;
				}

				if ($param =~ /([^=]+)=(.*)/) {

					my $name  = Slim::Utils::Misc::unescape($1, 1);
					my $value = Slim::Utils::Misc::unescape($2, 1);

					# We need to turn perl's internal
					# representation of the unescaped
					# UTF-8 string into a "real" UTF-8
					# string with the appropriate magic set.
					if ($value ne '*') {
						$value = Slim::Utils::Unicode::utf8decode($value);
					}

					if ( $RESERVED_PARAMS{$name} ) {
						if ( $name eq 'pageAntiCSRFToken' && !defined $providedPageAntiCSRFToken ) {
							$providedPageAntiCSRFToken = $value;
						}
						next;
					}

					# Ick. It sure would be nice to use
					# CGI or CGI::Lite
					if (ref($params->{$name}) eq 'ARRAY') {

						push @{$params->{$name}}, $value;

					} elsif (exists $params->{$name}) {

						my $old = delete $params->{$name};

						@{$params->{$name}} = ($old, $value);

					} else {

						$params->{$name} = $value;
					}

					main::DEBUGLOG && $isDebug && $log->info("HTTP parameter $name = $value");

					if ( main::WEBUI && $csrfProtectionLevel ) {
						my $csrfName = $name;
						if ( $csrfName eq 'command' ) { $csrfName = 'p0'; }
						if ( $csrfName eq 'subcommand' ) { $csrfName = 'p1'; }
						push @{$csrfReqParams{$csrfName}}, $value;
					}

				} else {

					my $name = Slim::Utils::Misc::unescape($param, 1);

					next if $RESERVED_PARAMS{$name};

					$params->{$name} = 1;

					main::DEBUGLOG && $isDebug && $log->info("HTTP parameter $name = 1");

					if ( main::WEBUI && $csrfProtectionLevel ) {
						my $csrfName = $name;
						if ( $csrfName eq 'command' ) { $csrfName = 'p0'; }
						if ( $csrfName eq 'subcommand' ) { $csrfName = 'p1'; }
						push @{$csrfReqParams{$csrfName}}, 1;
					}
				}
			}
		}

		if ( main::WEBUI && $csrfProtectionLevel ) {
			# for CSRF protection, get the query args in one neat string that
			# looks like a GET querystring value; this should handle GET and POST
			# equally well, only looking at the data that we would act on
			($queryWithArgs, $queryToTest) = Slim::Web::HTTP::CSRF->getQueries($request, \%csrfReqParams);

			# pageAntiCSRFToken is a bare token
			$params->{pageAntiCSRFToken} = Slim::Web::HTTP::CSRF->makePageToken($request);
		}

		# Skins
		if ($path) {

			$params->{'webroot'} = '/';

			$path =~ s|^/+||;

			if ( !main::WEBUI || $path =~ m{^(?:html|music|contributor|plugins|apps|settings|firmware|clixmlbrowser|index\.html|imageproxy)/}i || Slim::Web::Pages->isRawDownload($path) ) {
				# not a skin

			} elsif ($path =~ m|^([a-zA-Z0-9]+)$| && $skinMgr->isaSkin($1)) {

				my $location = '/' . $1 . '/';
				my $rawQuery = defined($uri) ? $uri->query() : undef;
				$location .= '?' . $rawQuery if defined($rawQuery) && $rawQuery ne '';

				main::DEBUGLOG && $isDebug && $log->info("Alternate skin $1 requested, redirecting to $location.");

				$response->code(RC_MOVED_PERMANENTLY);
				$response->header('Location' => $location);
				$response->header('Connection' => 'close');

				$httpClient->send_response($response);

				closeHTTPSocket($httpClient);

				return;

			} elsif ($path =~ m|^(.+?)/.*|) {

				my $desiredskin = $1;

				# Requesting a specific skin, verify and set the skinOverride param
				main::DEBUGLOG && $isDebug && $log->info("Alternate skin $desiredskin requested");

				my $skinname = $skinMgr->isaSkin($desiredskin);

				if ($skinname) {

					main::DEBUGLOG && $isDebug && $log->info("Rendering using $skinname");

					$params->{'skinOverride'} = $skinname;
					$params->{'webroot'} = $params->{'webroot'} . "$skinname/";

					$path =~ s{^.+?/}{/};
					$path =~ s|^/+||;

				} else {

					# throw 404
					my $safeSkin = _htmlEscape($desiredskin);
					$params->{'suggestion'} = qq(There is no "$safeSkin")
						. qq( skin, try ) . Slim::Utils::Network::serverURL() . qq( instead.);

					if ( $log->is_warn ) {
						$log->warn("Invalid skin requested: [" . join(' ', ($request->method, $request->uri)) . "]");
					}

					_sendImmediateError($httpClient, $response, RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', $params, 'invalid skin');
					return;
				}
			}

			$params->{"path"} = $path;
			$params->{"host"} = $request->header('Host');
		}

		if ( main::WEBUI && $csrfProtectionLevel ) {
			# apply CSRF protection logic to "dangerous" commands
			if (!Slim::Web::HTTP::CSRF->testCSRFToken($httpClient, $request, $response, $params, $queryWithArgs, $queryToTest, $providedPageAntiCSRFToken)) {
				return;
			}
		}

		if ( main::DEBUGLOG && $isDebug ) {
			$log->debug("Processed request headers: [\n" . _redactedRequestString($request) . "]");
		}

		# process the command
		processURL($httpClient, $response, $params);

	} else {
		if ( $request->method eq 'OPTIONS' ) {
			checkCORS($request, $response);
			$response->code(RC_OK);
		}

		$response->header('Connection' => 'close');

		if ( !$response->code() ) {
			if ( $log->is_warn ) {
				$log->warn("Bad Request: [" . join(' ', ($request->method, $request->uri)) . "]");
			}

			$response->code(RC_METHOD_NOT_ALLOWED);
			$response->content_type('text/html');
			$response->content_ref(_renderErrorPage(RC_METHOD_NOT_ALLOWED, 'html/errors/405.html', 'Method Not Allowed', $params));
		}

		$httpClient->send_response($response);
		closeHTTPSocket($httpClient);
	}

	# what does our response look like?
	if (main::DEBUGLOG && $isDebug) {

		$response->content("");
		$log->debug("Response Headers: [\n" . $response->as_string . "]");
	}

	if ( main::DEBUGLOG && $isDebug ) {
		$log->debug(
			"End request: keepAlive: [" .
			($keepAlives{$httpClient} || '') .
			"] - waiting for next request for $httpClient on connection = " . ($response->header('Connection') || '') . "\n"
		);
	}
}

# processURL - handles the execution of the HTTP request
sub processURL {
	my ($httpClient, $response, $params) = @_;

	my $output = "";
	my @p = ();
	my $client = undef;
	my $path   = $params->{"path"};

	# Command parameters are query parameters named p0 through pN
	# 	For example:
	#		http://host/status.m3u?p0=playlist&p1=jump&p2=2
	#		http://host/status.m3u?command=playlist&subcommand=jump&p2=2
	# This example jumps to the second song in the playlist and sends a playlist as the response
	#
	# If there are multiple players, then they are specified by the player id
	#   For example:
	#		http://host/status.html?p0=mixer&p1=volume&p2=11&player=10.0.1.203:69

	for (my $i = 0; $i <= scalar keys %{$params}; $i++) {
		last unless defined $params->{"p$i"};
		my $pval = $params->{"p$i"};
		$p[$i] = (!main::ISWINDOWS && Slim::Utils::Unicode::looks_like_utf8($pval))
			? Slim::Utils::Unicode::utf8on($pval)
			: Slim::Utils::Unicode::utf8encode_locale($pval);
	}

	# This is trumped by query parameters 'command' and 'subcommand'.
	# These are passed as the first two command parameters (p0 and p1),
	# while the rest of the query parameters are passed as third (p3).
	if (defined $params->{'command'} && $path !~ /^memoryusage/) {
		$p[0] = $params->{'command'};
		$p[1] = $params->{'subcommand'};
		$p[2] = join '&',
			map  { $_ . '=' . $params->{$_} }
			grep { !$RESERVED_PARAMS{$_} && $_ ne 'command' && $_ ne 'subcommand' && defined $params->{$_} && !ref($params->{$_}) }
			sort keys %{$params};
	}

	if ( main::INFOLOG && $log->is_info ) {
		$log->info("processURL Clients: " . join(" ", Slim::Player::Client::clientIPs()));
	}

	# explicitly specified player (for web browsers or squeezeboxen)
	if (defined($params->{"player"})) {
		$client = Slim::Player::Client::getClient($params->{"player"});

		if ( blessed($client) ) {
			if ( $path =~ m|plugins/UPnP/|i ) {
				# Bug 18053 - we're ignoring upnp requests, as these aren't our clients being active but whatever else
				main::DEBUGLOG && $log->is_debug && $log->debug("Don't update the client's lastActivityTime - this is a UPnP request. ($path)");
			}
			else {
				# Update the client's last activity time, since they did something through the web
				$client->lastActivityTime( Time::HiRes::time() );
			}
		}
	}

	# is this an HTTP stream?
	if (!defined($client) && ($path =~ /(?:stream\.mp3|stream)$/)) {

		# Bug 14825, allow multiple stream.mp3 clients from the same address with a player param
		my $address = $params->{player} || $peeraddr{$httpClient};

		if (!defined($address) || $address eq '') {
			$log->warn("Failed to create HTTP client for unknown address");
			closeHTTPSocket($httpClient, 0, 'client init failed');
			return;
		}

		main::INFOLOG && $log->is_info && $log->info("processURL found HTTP client at address=$address");

		$client = Slim::Player::Client::getClient($address);

		if (!defined($client)) {

			my $paddr = getpeername($httpClient);

			main::INFOLOG && $log->is_info && $log->info("New http client at $address");

			if ($paddr) {
				my $err;
				{
					local $@;
					$client = eval { Slim::Player::HTTP->new($address, $paddr, $httpClient) };
					$err = $@;
				}
				if ($err || !blessed($client)) {
					$log->warn("Failed to create HTTP client for " . (defined $address ? $address : 'unknown') . ($err ? ": $err" : ''));
					closeHTTPSocket($httpClient, 0, 'client init failed');
					return;
				}
				$client->init();

				# Give the streaming player a descriptive name such as "Winamp from x.x.x.x"
				if ( $params->{userAgent} ) {
					my ($agent) = $params->{userAgent} =~ m{([^/]+)};
					if ( $agent eq 'NSPlayer' ) {
						$agent = 'Windows Media Player';
					}
					elsif ( $agent eq 'WinampMPEG' ) {
						$agent = 'Winamp';
					}

					$client->name( $agent . ' ' . string('FROM') . ' ' . $address );
				}

				# Bug 4795
				# If the player has an existing playlist, start playing it without
				# requiring the user to press Play in the web UI
				if ( Slim::Player::Playlist::track($client) &&
					!Slim::Music::Info::isRemoteURL( Slim::Player::Playlist::url($client) )
				) {
					# play if current playlist item is not a remote url
					$client->execute( [ 'play' ] );
				}
			}
		}

		if (blessed($client)) {
			if (defined($params->{'bitrate'})) {
				# must validate 32 40 48 56 64 80 96 112 128 160 192 224 256 320 CBR
				# set to the closest lower value of its not a match
				my $temprate = $params->{'bitrate'};

				foreach my $i (qw(320 256 224 192 160 128 112 96 80 64 56 48 40 32)) {
					$temprate = $i;
					last if ($i <= $params->{'bitrate'});
				}

				$prefs->client($client)->set('transcodeBitrate',$temprate);

				main::INFOLOG && $log->is_info && $log->info("Setting transcode bitrate to $temprate");

			} else {

				$prefs->client($client)->set('transcodeBitrate',undef);
			}
		}
	}

	# player specified from cookie
	if ( !defined $client && $params->{'cookies'} ) {
		if ( my $player = $params->{'cookies'}->{'Squeezebox-player'} ) {
			$client = Slim::Player::Client::getClient( $player->value );
		}
	}

	# if we don't have a player specified, just pick one if there is one...
	$client = Slim::Player::Client::clientRandom() if !defined $client;

	if (blessed($client) && $client->can('id')) {

		$peerclient{$httpClient} = $client->id;
	}

	if ($client && $client->isa("Slim::Player::SLIMP3")) {

		$params->{'playermodel'} = 'slimp3';
	} elsif ($client && $client->isa("Slim::Player::Transporter")) {

		$params->{'playermodel'} = 'transporter';
	} else {

		$params->{'playermodel'} = 'squeezebox';
	}

	my @callbackargs = ($client, $httpClient, $response, $params);

	my $normPath = defined($path) ? $path : '';
	$normPath =~ s|^/+||;
	my $classOrCode = ($normPath ne '' && $normPath !~ m{^imageproxy/})
		? Slim::Web::Pages->getPageFunction($normPath)
		: undef;

	# Enforce protectSettings BEFORE executeRequest and player=* broadcast
	if ( _isExternalSettingsPeer($httpClient)
		&& ( $normPath =~ m{^settings(?:/|$)}i
			|| ($classOrCode && !ref($classOrCode) && $classOrCode->isa('Slim::Web::Settings'))
			|| (defined($p[0]) && $p[0] =~ /^(?:pref|serverpref|stopserver|restartserver)$/i) )
	) {
		my $hostIP = Slim::Utils::IPDetect::IP() || 'unknown';
		$log->error("Access to settings pages/commands is restricted to the local network or localhost: " . ($peeraddr{$httpClient} || 'unknown') . " -> $hostIP ($normPath)");

		delete $keepAlives{$httpClient};
		$response->code(RC_FORBIDDEN);
		$response->content_type('text/html');
		$response->header('Connection' => 'close');

		my $body = _renderErrorPage(RC_FORBIDDEN, 'html/errors/403.html', 'Forbidden', $params);
		prepareResponseForSending($client, $params, $body, $httpClient, $response);
		return;
	}

	# Enforce non-HEAD command gate and route validation BEFORE executeRequest and player=* broadcast
	if (defined($p[0])) {

		if ($response->request() && $response->request()->method() eq 'HEAD') {
			delete $keepAlives{$httpClient};
			$response->code(RC_METHOD_NOT_ALLOWED);
			$response->content_type('text/html');
			$response->header('Connection' => 'close');

			my $body = _renderErrorPage(RC_METHOD_NOT_ALLOWED, 'html/errors/405.html', 'Method Not Allowed', $params);
			prepareResponseForSending($client, $params, $body, $httpClient, $response);
			return;
		}

		my $validCmdRoute = main::WEBUI && ($classOrCode || (defined($path) && ($path =~ /status\.m3u/ || $path =~ /anyurl/)));
		if ($validCmdRoute) {

			if (defined($params->{"player"}) && $params->{"player"} eq "*") {

				for my $client2 (Slim::Player::Client::clients()) {

					next if $client eq $client2;

					$client2->execute(\@p);
				}
			}

			my $responded = 0;
			my $ctrlReq = Slim::Control::Request::executeRequest(
				$client,
				\@p,
				sub {
					$responded = 1;
					generateHTTPResponse(@_);
				},
				\@callbackargs,
			);

			if ( !$responded && (!$ctrlReq || (!$ctrlReq->isStatusDispatchable() && !$ctrlReq->isStatusProcessing())) ) {
				generateHTTPResponse(@callbackargs);
			}

		} else {

			generateHTTPResponse(@callbackargs);
		}

	} else {

		generateHTTPResponse(@callbackargs);
	}
}

=pod

=head1 Send the response to the client

=cut

sub generateHTTPResponse {
	my ($client, $httpClient, $response, $params, $p) = @_;

	# this is a scalar ref because of the potential size of the body.
	# not sure if it actually speeds things up considerably.
	my ($body, $mtime, $inode, $size);

	# default to 200
	$response->code(RC_OK);

	$params->{'player'}   = '';
	$params->{'revision'} = $::REVISION if $::REVISION;
	$params->{'nosetup'}  = 1   if $::nosetup;
	$params->{'noserver'} = 1   if $::noserver;

	# Check for the gallery view cookie.
	if ($params->{'cookies'}->{'Squeezebox-albumView'} &&
		$params->{'cookies'}->{'Squeezebox-albumView'}->value) {

		$params->{'artwork'} = $params->{'cookies'}->{'Squeezebox-albumView'}->value unless defined $params->{'artwork'};
	}

	# Check for the album order cookie.
	if ($params->{'cookies'}->{'Squeezebox-orderBy'} &&
		$params->{'cookies'}->{'Squeezebox-orderBy'}->value) {

		$params->{'orderBy'} = $params->{'cookies'}->{'Squeezebox-orderBy'}->value unless defined $params->{'orderBy'};
	}

	# Check for thumbSize cookie (for Touch, 1-by-1 artwork enlarge/shrink feature)
	if ($params->{'cookies'}->{'Squeezebox-thumbSize'} &&
		$params->{'cookies'}->{'Squeezebox-thumbSize'}->value) {

			$params->{'thumbSize'} = $params->{'cookies'}->{'Squeezebox-thumbSize'}->value unless defined $params->{'thumbSize'};
	}

	if (Slim::Web::Graphics::serverResizesArt()) {
		$params->{'serverResizesArt'} = 1;
	}

	my $path = $params->{"path"};
	my $type = Slim::Music::Info::typeFromSuffix($path, 'htm');

	# some browsers delete cookies aggressively - restore them from prefs
	if (shouldHandleCookies($response->request->uri->path)) {
		foreach (@{PERSIST_COOKIES()}) {
			my $prefVal = $prefs->get($_);
			if (!$params->{'cookies'}->{$_} && defined($prefVal) && length($prefVal) <= 64 && $prefVal =~ /^[A-Za-z0-9_.:-]*$/) {
				$response->headers->push_header( 'Set-Cookie' => _formatSetCookie(
					-name     => $_,
					-value    => $prefVal,
					-path     => '/',
					-samesite => 'Lax',
				) );
			}
		}
	}

	# lots of people need this - always derive from %Slim::Music::Info::types
	my $contentType = $Slim::Music::Info::types{$type};

	if ( Slim::Web::Pages->isRawDownload($path) ) {
		$contentType = 'application/octet-stream';
	}

	if ( $path =~ m{music/[0-9a-f]+/(?:download|cover)} || $path =~ m{contributor/[0-9a-f]+/image} || $path =~ m{^imageproxy/} ) {
		# Avoid generating templates for download URLs
		$contentType = 'application/octet-stream';
	}

	$contentType =~ tr/\r\n\0//d if defined $contentType;

	# setup our defaults
	$response->content_type($contentType) if defined $contentType;
	#$response->expires(0);

	# short-circuit if we don't have a content type to respond to.
	unless (defined($contentType)) {

		$response->code(RC_NOT_FOUND);
		$response->content_type('text/html');

		$body = _renderErrorPage(RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', $params);

		return prepareResponseForSending(
			$client,
			$params,
			$body,
			$httpClient,
			$response,
		);
	}

	my $classOrCode;
	$classOrCode = Slim::Web::Pages->getPageFunction($path) if defined($path) && $path !~ m{^imageproxy/};

	# protect access to settings pages: only allow from local network
	if ( defined($peeraddr{$httpClient})
		&& !Slim::Utils::Network::ip_is_host($peeraddr{$httpClient})
		&& $prefs->get('protectSettings') && !$prefs->get('authorize')
		&& ( (defined($path) && $path =~ m{^/?settings(?:/|$)}i)
			|| ($classOrCode && !ref $classOrCode && $classOrCode->isa('Slim::Web::Settings')) )
		&& ( Slim::Utils::Network::ip_is_gateway($peeraddr{$httpClient}) || Slim::Utils::Network::ip_on_different_network($peeraddr{$httpClient}) )
	) {
		my $hostIP = Slim::Utils::IPDetect::IP();
		$log->error("Access to settings pages is restricted to the local network or localhost: $peeraddr{$httpClient} -> $hostIP ($path)");

		$response->code(RC_FORBIDDEN);
		$response->content_type('text/html');

		$body = _renderErrorPage(RC_FORBIDDEN, 'html/errors/403.html', 'Forbidden', $params);

		return prepareResponseForSending(
			$client,
			$params,
			$body,
			$httpClient,
			$response,
		);
	}

	main::INFOLOG && $log->is_info && $log->info("Generating response for ($type, $contentType) $path");

	if (defined($client) && $classOrCode) {
		$params->{'player'} = $client->id();
		$params->{'myClientState'} = $client;

		if ( $path !~ m{(?:^progress\.|settings/)} ) {
			# save the player id in a cookie
			$response->headers->push_header( 'Set-Cookie' => _formatSetCookie(
				-name     => 'Squeezebox-player',
				-value    => $params->{'player'},
				-expires  => '+1y',
				-path     => '/',
				-samesite => 'Lax',
			) );
		}
	}

	# this might do well to break up into methods
	if ($contentType =~ /(?:image|javascript|css)/ || $path =~ m{^html/}) {

		my $max = 60 * 60;

		# increase expiry to a week for static content, but not cover art
		unless ($contentType =~ /image/ && $path !~ m{^html/}) {
			$max = $max * 24 * 7;
		}

 		# static content should expire from cache in one hour
		$response->expires( time() + $max );
		$response->header('Cache-Control' => 'max-age=' . $max);
	}

	# XXX - this is no longer being used by any of the stock skins
	if ($contentType =~ /text/ && $contentType !~ /(?:css|javascript)/ && $path !~ /(?:json|memoryusage)/) {

		$params->{'params'} = {};

		filltemplatefile('include.html', $params);

		while (my ($key,$value) = each %{$params->{'params'}}) {

			$params->{$key} = $value;
		}

		delete $params->{'params'};
	}

	# Static files handled here, stream them out to the browser to avoid wasting memory
	my $isStatic = 0;
	if ( $path =~ m{(?:^|/)favicon\.ico$}i ) {
		$path = 'html/mypage.ico';
		$isStatic = 1;
	}
	elsif ( $path =~ m{(?:\.(?:css|js)|(?:^|/)robots\.txt)$}i ) {
		$isStatic = 1;
	}
	elsif ( $path =~ m{^html/} && $path !~ $IMAGE_RESIZE_REGEX ) {
		if ( $contentType ne 'text/html' && $contentType ne 'text/xml' && $contentType ne 'application/x-java-jnlp-file' ) {
			$isStatic = 1;
		}
	}

	if ( $isStatic ) {
		($mtime, $inode, $size) = getFileInfoForStaticContent($path, $params);

		if ( !($mtime || $inode || $size || $body) ) {
			$response->code(RC_NOT_FOUND);
			$response->content_type('text/html');
			$body = _renderErrorPage(RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', $params);
		}
		elsif (contentHasBeenModified($response, $mtime, $inode, $size)) {

			$params->{contentAsFh} = 1;

			# $body contains a filehandle for static content
			$body = getStaticContent($path, $params);
			$body = \undef unless defined $body;
		}
	}
	else {
		if ($classOrCode) {

			# if we match one of the page functions as defined above,
			# execute that, and hand it a callback to send the data.

			main::PERFMON && (my $startTime = AnyEvent->time);

			if (ref($classOrCode) eq 'CODE') {

				# XXX: should this use eval?

				$body = &{$classOrCode}(
					$client,
					$params,
					\&prepareResponseForSending,
					$httpClient,
					$response,
				);

			} elsif ($classOrCode->can('handler')) {

				# Pull the player ID out and create a client from it
				# if we need to use it for player settings.
				if (exists $params->{'playerid'} && $classOrCode->needsClient) {

					$client = Slim::Player::Client::getClient($params->{'playerid'});
					if (blessed($client)) {
						$params->{'player'}        = $client->id();
						$params->{'myClientState'} = $client;
						$peerclient{$httpClient}   = $client->id();
					} else {
						$params->{'player'} = '';
						delete $params->{'myClientState'};
					}
				}

				$body = $classOrCode->handler(
					$client,
					$params,
					\&prepareResponseForSending,
					$httpClient,
					$response,
				);
			}

			main::PERFMON && $startTime && Slim::Utils::PerfMon->check('web', AnyEvent->time - $startTime, "Page: $path");

		} elsif ($path =~ /^(?:stream\.mp3|stream)$/o) {
			# Bug 15380, return correct content-type depending on what we're streaming
			if ( blessed($client) && $client->can('controller') && $client->controller() ) {
				if ( my $sc = $client->controller()->songStreamController() ) {
					if ( my $song = $sc->song() ) {
						my $type = $song->streamformat();
						my $ct   = defined($type) ? $Slim::Music::Info::types{$type} : undef;
						$response->content_type($ct) if defined $ct;
					}
				}
			}

			# short circuit here if it's a slim/squeezebox
			if ($sendMetaData{$httpClient}) {
				$response->header("icy-metaint" => METADATAINTERVAL);
				$response->header("icy-name"    => string('WELCOME_TO_SQUEEZEBOX_SERVER'));
			}

			main::INFOLOG && $log->is_info && $log->info("Disabling keep-alive for stream.mp3");
			delete $keepAlives{$httpClient};
			Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );
			$response->header( Connection => 'close' );

			my $headers = _stringifyHeaders($response) . $CRLF;

			$metaDataBytes{$httpClient} = - length($headers);

			addStreamingResponse($httpClient, $headers);

			return 0;

		} elsif ($path =~ m{music/([^/]+)/(cover|thumb)} ||
			$path =~ m{contributor/([^/]+)/image} ||
			$path =~ m{^(?:plugins/cache/icons|imageproxy)} ||
			$path =~ $IMAGE_RESIZE_REGEX
		) {

			main::PERFMON && (my $startTime = AnyEvent->time);

			# Bug 15723, We need to track if we have an async artwork request so
			# we don't return data out of order
			my $async = 0;
			my $sentResponse = 0;

			Slim::Web::Graphics::artworkRequest(
				$client,
				$path,
				$params,
				sub {
					$sentResponse = 1;
					prepareResponseForSending(@_);

					if ( $async ) {
						main::INFOLOG && $log->is_info && $log->info('Async artwork request done, enable read');
						Slim::Networking::Select::addRead($httpClient, \&processHTTP);
					}
				},
				$httpClient,
				$response,
			);

			# If artworkRequest did not directly call the callback, we are in an async request
			if ( !$sentResponse ) {
				main::INFOLOG && $log->is_info && $log->info('Async artwork request pending, pause read');
				Slim::Networking::Select::removeRead($httpClient);
				$async = 1;
			}

			main::PERFMON && $startTime && Slim::Utils::PerfMon->check('web', AnyEvent->time - $startTime, "Page: $path");

			return;

		# return quickly with a 404 if web UI is disabled
		} elsif ( !main::WEBUI && (
			   $path =~ /status\.m3u/
			|| $path =~ /(server|scanner|perfmon|log)\.(?:log|txt)/
			|| $path =~ /^html\//
			|| $path =~ /^\/?settings(?:\/|$)/i
			|| $path =~ /anyurl/
		) ) {
			$response->content_type('text/html');
			$response->code(RC_NOT_FOUND);

			my $msg = "<h1>404 Not Found: " . _htmlEscape($path) . "</h1><p>Lyrion Music Server web UI is not available in --noweb mode.</p>";
			$body = \$msg;

			return prepareResponseForSending(
				$client,
				$params,
				$body,
				$httpClient,
				$response,
			);

		} elsif ($path =~ /music\/(-[0-9a-f]+)\/download/) {
			my $obj = Slim::Schema->find('Track', $1);

			# our download code can't handle the volatile files directly - let's fake a local file here
			if (blessed($obj) && Slim::Music::Info::isVolatile($obj->url)) {
				my $handler = Slim::Player::ProtocolHandlers->handlerForURL($obj->url);

				my $url = Slim::Utils::Misc::fileURLFromPath($handler->pathFromFileURL($obj->url));

				my $tmpObj = Slim::Schema::RemoteTrack->updateOrCreate($url, {
					content_type => Slim::Music::Info::typeFromPath($url)
				});

				main::INFOLOG && $log->is_info && $log->info("Disabling keep-alive for large file download");
				delete $keepAlives{$httpClient};
				Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );
				$response->header( Connection => 'close' );

				if ( blessed($tmpObj) && $tmpObj->id && downloadMusicFile($httpClient, $response, $tmpObj->id) ) {
					return 0;
				}
			}

			# 404 error
			$log->is_warn && $log->warn("unable to find file for path: $path");

			$response->content_type('text/html');
			$response->code(RC_NOT_FOUND);

			$body = _renderErrorPage(RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', $params);

			return prepareResponseForSending(
				$client,
				$params,
				$body,
				$httpClient,
				$response,
			);
		} elsif ($path =~ /music\/([0-9a-f]+)\/download/) {
			# Bug 10730
			my $id = $1;

			if ( $path =~ /music/ ) {
				main::INFOLOG && $log->is_info && $log->info("Disabling keep-alive for large file download");
				delete $keepAlives{$httpClient};
				Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );
				$response->header( Connection => 'close' );
			}

			# Reject bad getContentFeatures requests (DLNA 7.4.26.5)
			if ( my $gcf = $response->request->header('getContentFeatures.dlna.org') ) {
				if ( $gcf ne '1' ) {
					$response->code(400);
					$response->headers->remove_content_headers;
					$httpClient->send_response($response);
					closeHTTPSocket($httpClient);
					return 0;
				}
			}

			if ( $path =~ /music/ ) {
				if ( downloadMusicFile($httpClient, $response, $id) ) {
					return 0;
				}
			}

		} elsif ($path =~ /(server|scanner|perfmon|log)\.(?:log|txt)/) {

			if ( main::WEBUI ) {
				$body = Slim::Web::Pages::Common->logFile($httpClient, $params, $response, $1);

				if ($body) {
					$contentType = 'text/html';
				}
				else {
					# when the full file is requested, then all the streaming is handled in the logFile call. Nothing is returned.
					return 0;
				}
			}

		} elsif ($path =~ /status\.m3u/) {

			if ( main::WEBUI ) {
				my $m3u = Slim::Web::Pages::Common->statusM3u($client);
				$body = defined($m3u) ? \$m3u : \undef;
			}

		} elsif ($path =~ m{^html/}) {

			# content is in the "html" subdirectory within the template directory.
			# if it's HTML then use the template mechanism
			if ($contentType eq 'text/html' || $contentType eq 'text/xml' || $contentType eq 'application/x-java-jnlp-file') {

				# if the path ends with a slash, then server up the index.html file
				$path .= 'index.html' if $path =~ m|/$|;
				$body  = filltemplatefile($path, $params);
				$body  = \undef unless defined $body;

			}

		} elsif ( Slim::Web::Pages->isRawDownload($path) ) {

			# path is for download of known file outside http directory
			my ($file, $ct);

			my $rawFiles = Slim::Web::Pages->getRawFiles();

			for my $key (keys %$rawFiles) {

				if ( $path =~ $key ) {

					my $fileinfo = $rawFiles->{$key};
					$file = ref $fileinfo->{file} eq 'CODE' ? $fileinfo->{file}->($path) : $fileinfo->{file};
					$ct   = ref $fileinfo->{ct}   eq 'CODE' ? $fileinfo->{ct}->($path)   : $fileinfo->{ct};

					if (!-e $file) {
						$file = undef;
					}

					last;
				}
			}

			if ($file) {
				# disable keep-alive for raw files, this is needed to prevent
				# Jive downloads from timing out
				if ( $keepAlives{$httpClient} ) {
					main::INFOLOG && $log->is_info && $log->info("Disabling keep-alive for raw file $file");
					delete $keepAlives{$httpClient};
					Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );
					$response->header( Connection => 'close' );
				}

				# download the file
				main::INFOLOG && $log->is_info && $log->info("serving file: $file for path: $path");
				sendStreamingFile( $httpClient, $response, $ct, $file );
				return 0;

			} else {
				# 404 error
				$log->is_warn && $log->warn("unable to find file for path: $path");

				$response->content_type('text/html');
				$response->code(RC_NOT_FOUND);

				$body = _renderErrorPage(RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', $params);

				return prepareResponseForSending(
					$client,
					$params,
					$body,
					$httpClient,
					$response,
				);
			}

		} elsif ( $path =~ /anyurl/ ) {
			main::DEBUGLOG && $log->is_debug && $log->debug('anyurl - parameters processed, redirect to status page if needed');
			$body = filltemplatefile('xmlbrowser_redirect.html', $params);
			$body = \undef unless defined $body;

		} else {
			# who knows why we're here, we just know that something ain't right
			$body = \undef;
		}
	}

	# if there's a reference to an empty value, then there is no valid page at all
	if (!$response->code() || $response->code() ne RC_NOT_MODIFIED) {

		if (defined $body && ref $body eq 'SCALAR' && !defined $$body) {

			$response->code(RC_NOT_FOUND);
			$contentType = 'text/html';
			$body = _renderErrorPage(RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', $params);
		}

		return 0 unless $body;

	} else {

		# Set the body to nothing, so the length() check won't fail.
		$body = \'';
	}

	# Create an ETag based on the mtime, file size and inode of the
	# content. This will allow us us to send back 304 (Not Modified)
	# headers. Very similar to how Apache does it.
	#
	# ETags can and should get smarter with our dynamic data - because we
	# know when it was updated, we can change the ETag. Until that
	# happens, only enable it for static content - ie: when an mtime exists.
	if (defined $mtime) {

		# for our static content
		$response->last_modified($mtime) if defined $mtime;

		my @etag = ();

		if ( !defined $size ) {
			if ( ref $body eq 'SCALAR' ) {
				$size = length($$body);
			}
			elsif ( ref $body eq 'FileHandle' ) {
				$size = (stat $body)[7];
			}
		}

		push @etag, sprintf('%lx', $inode) if $inode;
		push @etag, sprintf('%lx', $size)  if $size;
		push @etag, sprintf('%lx', $mtime) if $mtime;

		$response->etag(join('-', @etag));
	}

	# treat js.html differently - need the html ending to have it processed by TT,
	# but browser should consider it javascript
	if ( $path =~ /js(?:|-\S*)\.html/i ) {
		$contentType = 'application/x-javascript';
	}

	$contentType =~ tr/\r\n\0//d if defined $contentType;
	$response->content_type($contentType) if defined $contentType;

	#if (defined $params->{'refresh'}) {
	#	$response->header('Refresh', $params->{'refresh'});
	#}

	return 0 unless $body;

	if ( ref $body eq 'FileHandle' ) {
		$response->content_length( $size );

		my $headers = _stringifyHeaders($response) . $CRLF;

		$streamingFiles{$httpClient} = $body;

		# we are not a real streaming session, so we need to avoid sendStreamingResponse using the random $client stored in
		# $peerclient as this will cause streaming to the real client $client to stop.
		delete $peerclient{$httpClient};

		# Disable metadata in case this client sent an Icy-Metadata header
		$sendMetaData{$httpClient} = 0;

		addStreamingResponse($httpClient, $headers);

		return;
	}

	# if the reference to the body is itself undefined, then we've started
	# generating the page in the background
	return prepareResponseForSending($client, $params, $body, $httpClient, $response);
}

sub _sendStreamingError {
	my ($httpClient, $response, $code, $size) = @_;
	$response->protocol('HTTP/1.1') unless $response->protocol();
	_applySecurityHeaders($response);
	$response->code($code);
	$response->headers->remove_content_headers;
	$response->remove_header('Content-Disposition');
	$response->header('Content-Range' => 'bytes *' . chr(47) . (defined $size ? $size : 0))
		if $code == 416;
	$response->content_length(0);
	$response->header('Date' => time2str(time)) unless defined $response->header('Date');
	$response->header('Connection' => 'close');
	my $reftype = Scalar::Util::reftype($httpClient);
	if ($reftype && $reftype eq 'GLOB') {
		if (defined ${*$httpClient}{'httpd_daemon'}) {
			${*$httpClient}{'httpd_client_proto'} = 1001 unless defined ${*$httpClient}{'httpd_client_proto'};
			$httpClient->send_response($response);
		} else {
			use bytes;
			my $raw = _stringifyHeaders($response) . $CRLF;
			my $off = 0;
			my $len = length($raw);
			while ($off < $len) {
				my $w = syswrite($httpClient, $raw, $len - $off, $off);
				last unless defined $w || $! == EINTR;
				$off += $w if defined $w;
			}
		}
	} else {
		$httpClient->send_response($response);
	}
	closeHTTPSocket($httpClient);
}

sub sendStreamingFile {
	my ( $httpClient, $response, $contentType, $file, $objOrHash, $showInBrowser ) = @_;

	use bytes;

	# Send the file down - and hint to the browser
	# the correct filename to save it as.
	my $size = -s $file;

	$contentType = 'application/octet-stream' unless defined $contentType && $contentType ne '';
	$contentType =~ tr/\r\n\0//d;
	my $safeFilename = _sanitizeDispositionFilename(basename($file));

	my $fh = FileHandle->new($file, '<');
	if (!$fh) {
		$log->error("Couldn't open streaming file '$file': $!");
		$response->headers->remove_content_headers;
		_sendImmediateError($httpClient, $response, RC_NOT_FOUND, 'html/errors/404.html', 'Not Found', {}, 'file open failed');
		return;
	}
	binmode($fh);

	$response->content_type( $contentType );
	$response->content_length( $size );
	$response->header('Content-Disposition',
		sprintf('attachment; filename="%s"', $safeFilename)
	) unless $showInBrowser;

	# Range/TimeSeekRange
	my $range   = $response->request->header('Range');
	my $tsrange = $response->request->header('TimeSeekRange.dlna.org');

	# If a Range is already provided, ignore TimeSeekRange
	my $isTSR;
	if ( $tsrange && !$range ) {
		# Translate TimeSeekRange into byte range
		my $valid = 0;

		my $formatClass = blessed($objOrHash) ? Slim::Formats->classForFormat($objOrHash->content_type) : undef;

		# Ignore TimeSeekRange unless we have a valid format class (currently this only supports audio)
		if ( $formatClass && Slim::Formats->loadTagFormatForType($objOrHash->content_type) && $formatClass->can('findFrameBoundaries') ) {
			# Valid is: npt=(start time)-(end time)
			# A time may be either a fractional seconds (sss.fff), or hhh:mm:ss.fff
			# End is optional
			if ( $tsrange =~ /^npt=([^-]+)-([^\s]*)$/ ) {
				my $start = $1 || 0;
				my $end   = $2;

				my $startbytes = 0;
				my $endbytes = $size - 1;

				if ( $start =~ /:/ ) {
					my ($h, $m, $s) = split /:/, $start;
					$start = ($h * 3600) + ($m * 60) + $s;
				}

				if ( $start > 0 ) {
					$startbytes = $formatClass->findFrameBoundaries($fh, undef, $start);
					main::DEBUGLOG && $log->is_debug && $log->debug("TimeSeekRange.dlna.org: Found start byte offset $startbytes for time $start");
				}

				if ( $end ) {
					if ( $end =~ /:/ ) {
						my ($h, $m, $s) = split /:/, $end;
						$end = ($h * 3600) + ($m * 60) + $s;
					}

					$endbytes = $formatClass->findFrameBoundaries($fh, undef, $end);
					main::DEBUGLOG && $log->is_debug && $log->debug("TimeSeekRange.dlna.org: Found end offset $endbytes for time $end");
				}

				if ( $startbytes == -1 && $endbytes == -1 ) {
					# DLNA 7.4.40.8, a valid time range syntax but out of range for the media
					$response->code(416);
				}
				else {
					# If only the end is -1, assume it was seeking too near the end, and set it to $size
					if ($endbytes == -1) {
						$endbytes = $size - 1;
					}
				}

				if ( $startbytes >= 0 && $endbytes >= 0 ) {
					# Create a valid Range request, which will be handled by the below range code
					$range = "bytes=${startbytes}-${endbytes}";
					$isTSR = 1;
					$valid = 1;

					my $duration = $objOrHash->secs;
					$end ||= $duration;
					$response->header( 'TimeSeekRange.dlna.org' => "npt=${start}-${end}/${duration} bytes=${startbytes}-${endbytes}/${size}" );

					# If npt is "0-" don't perform a range request
					if ($start == 0 && $end == $duration) {
						$range = undef;
					}
				}
			}
			else {
				# DLNA 7.4.40.9, bad npt format is a 400 error
				$response->code(400);
			}
		}

		if ( !$valid ) {
			$log->warn("Invalid TimeSeekRange.dlna.org request: $tsrange");
			my $errCode = ($response->code && $response->code >= 400) ? $response->code : 406;
			_sendStreamingError($httpClient, $response, $errCode, $size);
			return;
		}
	}

	# Support Range requests
	if ( $range ) {
		# Only support a single range request, and no support for suffix requests
		if ( $range =~ m/^bytes=(\d+)-(\d+)?$/ ) {
			my $first = $1 + 0;
			my $last  = defined($2) ? ($2 + 0) : ($size - 1);

			if ( !$size || $first >= $size ) {
				# invalid (past end of file)
				_sendStreamingError($httpClient, $response, 416, $size);
				return;
			}

			if ( $last >= $size ) {
				$last = $size - 1;
			}

			my $total = $last - $first + 1;

			if ( $total < 1 ) {
				# invalid (first > last)
				_sendStreamingError($httpClient, $response, 400, $size);
				return;
			}

			main::DEBUGLOG && $log->is_debug && $log->debug("Handling Range request: $first-$last");

			seek $fh, $first, 0;

			if ( $isTSR ) { # DLNA 7.4.40.7 A time seek uses 200 status and doesn't include Content-Range, ugh
				$response->code(200);
			}
			else {
				$response->code( 206 );
				$response->header( 'Content-Range' => "bytes $first-$last/$size" );
			}
			$response->content_length( $total );

			# Save total value for use later in sendStreamingResponse
			${*$fh}{rangeTotal}   = $total;
			${*$fh}{rangeCounter} = 0;
		}
	}

	# Respond to realTimeInfo.dlna.org (DLNA 7.4.72)
	if ( $response->request->header('realTimeInfo.dlna.org') ) {
		$response->header( 'realTimeInfo.dlna.org' => 'DLNA.ORG_TLAG=*' );
	}

	my $headers = _stringifyHeaders($response) . $CRLF;

	# For a range request, reduce rangeCounter to account for header size
	if ( ${*$fh}{rangeTotal} ) {
		${*$fh}{rangeCounter} -= length $headers;
	}

	$streamingFiles{$httpClient} = $fh;

	# we are not a real streaming session, so we need to avoid sendStreamingResponse using the random $client stored in
	# $peerclient as this will cause streaming to the real client $client to stop.
	delete $peerclient{$httpClient};

	# Disable metadata in case this client sent an Icy-Metadata header
	$sendMetaData{$httpClient} = 0;

	addStreamingResponse($httpClient, $headers);
}

sub contentHasBeenModified {
	my $response = shift;
	my $mtime    = shift || $response->last_modified() || 0;

	my $request  = $response->request();
	my $method   = $request->method();

	# From Apache:
	#
	# Check for conditional requests --- note that we only want to do
	# this if we are successful so far and we are not processing a
	# subrequest or an ErrorDocument.
	#
	# The order of the checks is important, since ETag checks are supposed
	# to be more accurate than checks relative to the modification time.

	# If an If-Match request-header field was given
	# AND the field value is not "*" (meaning match anything)
	# AND if our strong ETag does not match any entity tag in that field,
	#     respond with a status of 412 (Precondition Failed).
	my $ifMatch = $request->if_match();
	my $etag    = $response->etag();

	my $ifModified  = $request->if_modified_since();
	my $requestTime = $request->client_date();
	my $isGetOrHead = ($method eq 'GET' || $method eq 'HEAD') ? 1 : 0;

	if ($ifMatch) {

		if ($ifMatch ne '*' && (!$etag || $etag =~ m{^W/}i || $ifMatch =~ m{^W/}i || $etag ne $ifMatch)) {

			main::DEBUGLOG && $log->is_debug && $log->debug("\tifMatch - RC_PRECONDITION_FAILED");
			$response->code(RC_PRECONDITION_FAILED);
		}

	} else {

		# Else if a valid If-Unmodified-Since request-header field was given
		# AND the requested resource has been modified since the time
		# specified in this field, then the server MUST
		#     respond with a status of 412 (Precondition Failed).
		my $ifUnmodified = $request->if_unmodified_since();

		if ($ifUnmodified && $mtime && $mtime > $ifUnmodified) {

			 main::DEBUGLOG && $log->is_debug && $log->debug("\tifUnmodified - RC_PRECONDITION_FAILED");

			 $response->code(RC_PRECONDITION_FAILED);
		}
	}

	# return early.
	if ($response->code() eq RC_PRECONDITION_FAILED) {

		$response->remove_header('Content-Length');
		return 1;
	}

	# If an If-None-Match request-header field was given
	# AND the field value is "*" (meaning match anything)
	#     OR our ETag matches any of the entity tags in that field, fail.
	#
	# If the request method was GET or HEAD, failure means the server
	#    SHOULD respond with a 304 (Not Modified) response.
	# For all other request methods, failure means the server MUST
	#    respond with a status of 412 (Precondition Failed).
	#
	# GET or HEAD allow weak etag comparison, all other methods require
	# strong comparison.  We can only use weak if it's not a range request.
	my $ifNoneMatch = $request->if_none_match();

	if ($ifNoneMatch) {
		my $matched = 0;

		if ($ifNoneMatch eq '*') {
			$matched = 1;
		} elsif ($etag) {
			if ($request->if_range() || !$isGetOrHead) {
				if ($etag !~ m{^W/}i && $ifNoneMatch !~ m{^W/}i && $ifNoneMatch eq $etag) {
					$matched = 1;
				}
			} else {
				(my $weakEtag = $etag) =~ s{^W/}{}i;
				(my $weakIfnm = $ifNoneMatch) =~ s{^W/}{}i;
				if ($weakIfnm eq $weakEtag) {
					$matched = 1;
				}
			}
		}

		if ($matched) {
			if ($isGetOrHead) {
				main::DEBUGLOG && $log->is_debug && $log->debug("\tifNoneMatch matched - returning 304");
				$response->code(RC_NOT_MODIFIED);
			} else {
				main::DEBUGLOG && $log->is_debug && $log->debug("\tifNoneMatch matched on $method - returning 412");
				$response->code(RC_PRECONDITION_FAILED);
				$response->remove_header('Content-Length');
				return 1;
			}
		}

	} elsif ($isGetOrHead) {

		# Else if a valid If-Modified-Since request-header field was given
		# AND it is a GET or HEAD request
		# AND the requested resource has not been modified since the time
		# specified in this field, then the server MUST
		#    respond with a status of 304 (Not Modified).
		# A date later than the server's current request time is invalid.

		if ($ifModified && $requestTime && $mtime) {

			if (($ifModified >= $mtime) && ($ifModified <= $requestTime)) {

				if ( main::INFOLOG && $log->is_info ) {
					$log->info(sprintf("Content at: %s has not been modified - returning 304.", $request->uri));
				}

				$response->code(RC_NOT_MODIFIED);
			}
		}
 	}

 	if ($response->code() eq RC_NOT_MODIFIED) {

 		for my $header (qw(Content-Length Content-Type Last-Modified)) {
 			$response->remove_header($header);
 		}

		return 0;
 	}

	return 1;
}

sub prepareResponseForSending {
	my ($client, $params, $body, $httpClient, $response) = @_;

	use bytes;

	# Trap empty content
	$body ||= \'';

	# Set the Content-Length - valid for either HEAD or GET
	$response->content_length(length($$body));

	# bug 7498: add charset to content type if needed
	# If we're perl 5.8 or above, always send back utf-8
	# Otherwise, send back the charset from the current locale
	my $contentType = $response->header('Content-Type');

	if (defined $contentType && $contentType ne '') {
		$contentType =~ tr/\r\n\0//d;

		if ($contentType =~ m!^text/(?:html|xml)!i && $contentType !~ /;\s*charset\s*=/i) {

			$contentType .= '; charset=utf-8';
		}

		$response->content_type($contentType);
	}

	_applySecurityHeaders($response);
	$response->header( Date => time2str(time) );

	# Only evaluate conditional validators (If-None-Match / If-Modified-Since) on 200 OK responses
	# so 4xx/5xx error responses (403, 404, 405, etc.) cannot be converted to 304 Not Modified.
	if (defined($response->code()) && $response->code() == RC_OK) {

		contentHasBeenModified($response);
	}

	if ($response->code() eq RC_NOT_MODIFIED || $response->code() eq RC_PRECONDITION_FAILED) {
		$response->remove_header('Content-Length');
	}

	# buffer our response, including headers, and we have no more data
	addHTTPResponse($httpClient, $response, $body, 1, 0);

	return 0;
}

# XXX - ick ick
sub _stringifyHeaders {
	my $response = shift;

	_applySecurityHeaders($response);

	my $code = $response->code();
	my $data = '';

	$data .= sprintf("%s %s %s%s", ($response->protocol() || 'HTTP/1.1'), $code, HTTP::Status::status_message($code) || "", $CRLF);

	$data .= "Server: Lyrion Music Server$CRLF";

	$data .= $response->headers_as_string($CRLF);

	# hack to make xmms like the audio better, since it appears to be case sensitive on for headers.
	$data =~ s/^(Icy-.+\:)/\L$1/mg;

	# hack for Reciva Internet Radios which glitch on metadata unless the
	# icy-name header comes before icy-metaint, so make sure icy-metaint
	# is the last of the headers.
	$data =~ s/^(icy-metaint:[^\n]*\n)(.+)/$2$1/ms;

	return $data;
}

# addHTTPResponse
# buffers an HTTP response $response with body $body for $httpClient
#  $response is used to get the headers and the desired chunking/closing behaviour
#  headers are sent if $sendheaders is 1 (the default)
#  if chunking is used, a last chunk is sent if Connection:Close or $more is 0 (the default)

# Example for normal use
#  addHTPPResponse($httpClient, $response, $body, 1, 0)
#   buffers headers and body, chunked or not, closing or not

# Example for chunking use
#  1. addHTTPResponse($client, $response, $body, 1, 1)
#   buffers headers and first body part
#  2. addHTTPResponse($client, $response, $body, 0, 1)
#   buffers more body
#  3. addHTTPResponse($client, $response, $body, 0, 0)
#   buffers more body and last chunk (or close)

sub addHTTPResponse {
	my $httpClient  = shift;
	my $response    = shift;
	my $body        = shift;
	my $sendheaders = shift;
	my $more        = shift || 0;

	# determine our closing/chunking behaviour
	# code above is responsible to set the headers right...
	my $close   = 0;
	my $chunked = 0;

	# if we have more, don't close now!
	if (!$more && $response->header('Connection') && $response->header('Connection') =~ /close/i) {

		$close = 1;
	}

	if ($response->header('Transfer-Encoding') && $response->header('Transfer-Encoding') =~ /chunked/i) {

		$chunked = 1;
	}

	# Force byte semantics on $body and length($$body) - otherwise we'll
	# try to write out multibyte characters with invalid byte lengths in
	# sendResponse() below.
	use bytes;

	# Collect all our output into one chunk, to reduce TCP packets
	my $outbuf = '';

	# First add the headers, if requested
	if (!defined($sendheaders) || $sendheaders == 1) {

		# Add a header displaying the time it took us to serve this request
		my $startTime = (blessed($httpClient) && $httpClient->can('start_time')) ? $httpClient->start_time : undef;
		if (defined $startTime) {
			$response->header( 'X-Time-To-Serve' => ( Time::HiRes::time() - $startTime ) );
		}

		$outbuf .= _stringifyHeaders($response) . $CRLF;
	}

	# And now the body.
	# Don't send back any content on a HEAD or 304 response.
	if ($response->request()->method() ne 'HEAD' &&
		$response->code() ne RC_NOT_MODIFIED &&
		$response->code() ne RC_PRECONDITION_FAILED) {

		# use chunks if we have a transfer-encoding that says so
		if ($chunked) {

			# add non-empty chunk...
			if (length($$body)) {
				$outbuf .= sprintf("%X", length($$body)) . $CRLF . $$body . $CRLF;
			}

			# add a last empty chunk if we're closing the connection or if there's nothing more
			if ($close || !$more) {

				$outbuf .= '0' . $CRLF . $CRLF;
			}

		} else {

			$outbuf .= $$body;
		}
	}

	push @{$outbuf{$httpClient}}, {
		'data'     => \$outbuf,
		'offset'   => 0,
		'length'   => length($outbuf),
		'close'    => $close,
	};

	Slim::Networking::Select::addWrite($httpClient, \&sendResponse);
}

sub addHTTPLastChunk {
	my $httpClient = shift;
	my $close = shift;

	my $emptychunk = "0" . $CRLF . $CRLF;

	push @{$outbuf{$httpClient}}, {
		'data'     => \$emptychunk,
		'offset'   => 0,
		'length'   => length($emptychunk),
		'close'    => $close,
	};

	Slim::Networking::Select::addWrite($httpClient, \&sendResponse);
}

# sendResponse
# callback for write select
# pops a data segment for the given httpclient and sends it
# optionally closes the connection *if* there's no more segments.
# expects segments to be hashrefs with items 'data', 'offset', 'length' and 'close'
sub sendResponse {
	my $httpClient = shift;

	use bytes;

	my $segment    = shift(@{$outbuf{$httpClient}});
	my $sentbytes  = 0;
	my $port       = $httpClient->peerport();

	# abort early if we're not connected
	if (!$httpClient->connected) {

		$log->is_warn && $log->warn("Not connected with $peeraddr{$httpClient}:$port, closing socket");

		closeHTTPSocket($httpClient, 0, 'not connected');
		return;
	}

	# abort early if we don't have anything.
	if (!$segment) {

		main::INFOLOG && $log->is_info && $log->info("No segment to send to $peeraddr{$httpClient}:$port, waiting for next request...");

		# Nothing to send, so we take the socket out of the write list.
		# When we process the next request, it will get put back on.
		Slim::Networking::Select::removeWrite($httpClient);

		return;
	}

	if (defined $segment->{'data'} && defined ${$segment->{'data'}}) {

		$sentbytes = syswrite($httpClient, ${$segment->{'data'}}, $segment->{'length'}, $segment->{'offset'});
	}

	if (!defined $sentbytes && ($! == EWOULDBLOCK || $! == EINTR)) {
		main::INFOLOG && $log->is_info && $log->info("Would block or interrupted while sending. Resetting sentbytes for: $peeraddr{$httpClient}:$port");
		$sentbytes = 0;
	}

	if (!defined($sentbytes)) {

		# Treat $httpClient with suspicion
		main::INFOLOG && $log->is_info && $log->info("Send to $peeraddr{$httpClient}:$port had error ($!), closing and aborting.");

		closeHTTPSocket($httpClient, 0, "$!");

		return;
	}

	# sent incomplete message
	if ($sentbytes < $segment->{'length'}) {

		$segment->{'length'} -= $sentbytes;
		$segment->{'offset'} += $sentbytes;
		unshift @{$outbuf{$httpClient}}, $segment;

	} else {

		main::INFOLOG && $log->is_info && $log->info("Sent $sentbytes to $peeraddr{$httpClient}:$port");

		# sent full message
		if (@{$outbuf{$httpClient}} == 0) {

			# no more messages to send
			main::INFOLOG && $log->is_info && $log->info("No more segments to send to $peeraddr{$httpClient}:$port");


			# close the connection if requested by the higher God pushing segments
			if ($segment->{'close'} && $segment->{'close'} == 1) {

				main::INFOLOG && $log->is_info && $log->info("End request, connection closing for: $peeraddr{$httpClient}:$port");

				closeHTTPSocket($httpClient);
				return;
			}
			else {
				# Check for additional pipelined GET or HEAD requests we need to process
				# We also support pipelined cometd requets, even though this is against the HTTP RFC
				if ( ${*$httpClient}{httpd_rbuf} ) {
					if ( ${*$httpClient}{httpd_rbuf} =~ m{^(?:GET|HEAD|POST /cometd)} ) {
						main::INFOLOG && $log->is_info && $log->info("Pipelined request found, processing");
						processHTTP($httpClient);
						return;
					}
					elsif ( $log->is_info ) {
						main::INFOLOG && $log->info( "Not handling pipelined request:\n" . ${*$httpClient}{httpd_rbuf} );
					}
				}
			}

			# Post-send keep-alive timer once response queue is drained
			Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );
			if ( $keepAlives{$httpClient} ) {
				Slim::Utils::Timers::setTimer(
					$httpClient,
					time() + KEEPALIVETIMEOUT,
					\&closeHTTPSocket,
					0,
					'keep-alive timeout',
				);
			}

		} else {

			main::INFOLOG && $log->is_info && $log->info("More segments to send to $peeraddr{$httpClient}:$port");
		}
	}
}

=pod

=head1 These two routines handle HTTP streaming of audio (a la ShoutCast and IceCast)

=cut

sub addStreamingResponse {
	my $httpClient = shift;
	my $message    = shift;

	use bytes;

	my %segment = (
		'data'   => \$message,
		'offset' => 0,
		'length' => length($message)
	);

	push @{$outbuf{$httpClient}}, \%segment;

	# Set the kernel's send buffer to be higher so that there is less
	# chance of audio skipping if/when we block elsewhere in the code.
	#
	# Check to make sure that our target size isn't smaller than the
	# kernel's default size.
	if (unpack('I', getsockopt($httpClient, SOL_SOCKET, SO_SNDBUF)) < (MAXCHUNKSIZE * 2)) {

		setsockopt($httpClient, SOL_SOCKET, SO_SNDBUF, (MAXCHUNKSIZE * 2));
	}

	# we aren't going to read from this socket anymore so don't select on it...
	Slim::Networking::Select::removeRead($httpClient);

	if (my $client = Slim::Player::Client::getClient($peerclient{$httpClient})) {

		$client->streamingsocket($httpClient);

		my $newpeeraddr = getpeername($httpClient);

		$client->paddr($newpeeraddr) if $newpeeraddr;
	}

	Slim::Networking::Select::addWrite($httpClient, \&sendStreamingResponse, 1);
}

sub clearOutputBuffer {
	my $client = shift;
	return unless defined $client;

	my $key = (blessed($client) && $client->can('streamingsocket'))
		? $client->streamingsocket()
		: $client;

	delete $outbuf{$key} if defined $key;
}

sub sendStreamingResponse {
	my $httpClient = shift;
	my $sentbytes;

	my $client;

	my $isInfo = ( main::INFOLOG && $log->is_info ) ? 1 : 0;

	if ( $peerclient{$httpClient} ) {
		$client = Slim::Player::Client::getClient($peerclient{$httpClient});
	}

	# when we are streaming a file, we may not have a client, rather it might just be going to a web browser.
	# assert($client);

	my $outbuf = $outbuf{$httpClient};
	my $segment = shift(@$outbuf);
	my $streamingFile = $streamingFiles{$httpClient};

	my $silence = 0;

	main::INFOLOG && $isInfo && $log->info("sendStreaming response begun...");

	# Keep track of where we need to stop if this is a range request
	my $rangeTotal;
	my $rangeCounter;
	if ( $streamingFile && ${*$streamingFile}{rangeTotal} ) {
		$rangeTotal   = ${*$streamingFile}{rangeTotal};
		$rangeCounter = ${*$streamingFile}{rangeCounter};

		main::DEBUGLOG && $log->is_debug && $log->debug( "  range request, sending $rangeTotal bytes ($rangeCounter sent)" );
	}

	if (   !$httpClient->connected()
		|| ($client && $client->isa("Slim::Player::Squeezebox")
			&& (   !defined($client->streamingsocket())
			    || $httpClient != $client->streamingsocket()
				|| (!$streamingFile && $client->isStopped()) # XXX is the !$streamingFile test superfluous
				)
			)
		)
	{
		main::INFOLOG && $isInfo &&
			$log->info(($client ? $client->id : ''), " Streaming connection closed");

		closeStreamingSocket($httpClient);
		return undef;
	}

	if (!defined($streamingFile) && $client && $client->isa("Slim::Player::HTTP") &&
		((Slim::Player::Source::playmode($client) ne 'play') || (Slim::Player::Playlist::count($client) == 0))) {

		$silence = 1;
	}

	# if we don't have anything in our queue, then get something
	if (!defined($segment)) {

		# if we aren't playing something, then queue up some silence
		if ($silence) {

			main::INFOLOG && $isInfo && $log->info("(silence)");

			my $bitrate = Slim::Utils::Prefs::maxRate($client);
			my $silence = undef;

			if ($bitrate == 320 || $bitrate == 0) {

				$silence = getStaticContent("html/silence.mp3");

			} else {

				$silence = getStaticContent("html/lbrsilence.mp3");
			}

			if (ref($silence) eq 'SCALAR' && defined $$silence && length($$silence)) {
				my %segment = (
					'data'   => $silence,
					'offset' => 0,
					'length' => length($$silence)
				);

				unshift @$outbuf, \%segment;
			} else {
				$log->warn("Unable to load silence MP3 for streaming client");
				closeStreamingSocket($httpClient);
				return 0;
			}

		} else {
			my $chunkRef;

			if (defined($streamingFile)) {

				my $chunk = undef;
				my $len   = MAXCHUNKSIZE;

				# Reduce len if needed for a range request
				if ( $rangeTotal && ( $rangeCounter + $len > $rangeTotal ) ) {
					$len = $rangeTotal - $rangeCounter;
					main::DEBUGLOG && $log->is_debug && $log->debug( "Reduced read length to $len for range request" );
				}

				if ( $len ) {
					my $n;
					my $retries = 0;
					do {
						$n = $streamingFile->sysread( $chunk, $len );
					} while (!defined($n) && $! == EINTR && ++$retries < 5);

					if (!defined($n)) {
						if ($! == EWOULDBLOCK) {
							return 0;
						}
						$log->warn("Error reading streaming file: $!");
						closeStreamingSocket($httpClient, 1);
						return undef;
					}
				}

				if (defined($chunk) && length($chunk)) {

					$chunkRef = \$chunk;

				} else {

					# we're done streaming this stored file, closing connection.
					closeStreamingSocket($httpClient);

					return 0;
				}

			} else {
				# bug 10534
				if (!$client) {
					closeStreamingSocket($httpClient);
					main::INFOLOG && $log->info("Abandoning orphened streaming connection");
					return 0;
				}

				$chunkRef = $client->nextChunk(MAXCHUNKSIZE, sub {tryStreamingLater(shift, $httpClient);});
			}

			# otherwise, queue up the next chunk of sound
			if ($chunkRef) {

				if (length($$chunkRef)) {

					if ( main::INFOLOG && $isInfo ) {
						$log->info("(audio: " . length($$chunkRef) . " bytes)");
					}

					my %segment = (
						'data'   => $chunkRef,
						'offset' => 0,
						'length' => length($$chunkRef)
					);

					unshift @$outbuf,\%segment;

				} else {
					main::INFOLOG && $log->info("Found an empty chunk on the queue - dropping the streaming connection.");
					forgetClient($client);
					return undef;
				}

			} else {

				# let's try again after RETRY_TIME - not really necessary as we are selecting on source, ...
				my $retry = RETRY_TIME;

				main::INFOLOG && $isInfo && $log->info("Nothing to stream, let's wait for $retry seconds...");

				Slim::Networking::Select::removeWrite($httpClient);

				Slim::Utils::Timers::setTimer($client, Time::HiRes::time() + $retry, \&tryStreamingLater,($httpClient));
			}
		}

		# try again...
		$segment = shift(@$outbuf);
	}

	# try to send metadata, if appropriate
	if ($sendMetaData{$httpClient}) {

		# if the metadata would appear in the middle of this message, just send the bit before
		main::INFOLOG && $isInfo && $log->info("metadata bytes: $metaDataBytes{$httpClient}");

		if ($metaDataBytes{$httpClient} == METADATAINTERVAL) {

			unshift @$outbuf, $segment;

			use bytes;

			my $songIndex = Slim::Player::Source::streamingSongIndex($client);
			my $url = Slim::Player::Playlist::url($client, $songIndex);

			my $title = $url ? Slim::Music::Info::getCurrentTitle($client, $url) : string('WELCOME_TO_SQUEEZEBOX_SERVER');
			$title = '' unless defined $title;
			utf8::encode($title) if utf8::is_utf8($title);
			$title =~ tr/'\r\n\0/ /d;
			if (length($title) > 4065) {
				$title = substr($title, 0, 4065);
			}

			my $metastring = "StreamTitle='" . $title . "';";
			my $pad        = (16 - (length($metastring) % 16)) % 16;
			$metastring   .= chr(0) x $pad;
			my $length     = length($metastring) / 16;

			my $message = pack('C', $length) . $metastring;

			my %segment = (
				'data'   => \$message,
				'offset' => 0,
				'length' => length($message)
			);

			$segment = \%segment;

			$metaDataBytes{$httpClient} = 0;

			if ( main::INFOLOG && $isInfo ) {
				$log->info("sending metadata of length $length: '$metastring' (" . length($message) . " bytes)");
			}

		} elsif (defined($segment) && $metaDataBytes{$httpClient} + $segment->{'length'} > METADATAINTERVAL) {

			my $splitpoint = METADATAINTERVAL - $metaDataBytes{$httpClient};

			# make a copy of the segment, and point to the second half, to be sent later.
			my %splitsegment = %$segment;

			$splitsegment{'offset'} += $splitpoint;
			$splitsegment{'length'} -= $splitpoint;

			unshift @$outbuf, \%splitsegment;

			#only send the first part
			$segment->{'length'} = $splitpoint;

			$metaDataBytes{$httpClient} += $splitpoint;

			main::INFOLOG && $isInfo && $log->info("splitting message for metadata at $splitpoint");

		} elsif (defined $segment) {

			# if it's time to send the metadata, just send the metadata
			$metaDataBytes{$httpClient} += $segment->{'length'};
		}
	}

	if (defined($segment)) {

		use bytes;

		my $prebytes = $segment->{'length'};
		$sentbytes   = syswrite($httpClient, ${$segment->{'data'}}, $segment->{'length'}, $segment->{'offset'});

		if (!defined $sentbytes && ($! == EWOULDBLOCK || $! == EINTR)) {
			$sentbytes = 0;
		}

		if (defined($sentbytes)) {
			if ($sentbytes < $segment->{'length'}) { #sent incomplete message

				if ($sentbytes) {

					main::INFOLOG && $isInfo && $log->info("sent incomplete chunk, requeuing " . ($segment->{'length'} - $sentbytes). " bytes");
				}

				$metaDataBytes{$httpClient} -= $segment->{'length'} - $sentbytes;

				$segment->{'length'} -= $sentbytes;
				$segment->{'offset'} += $sentbytes;

				unshift @$outbuf,$segment;
			}

		} else {

			main::INFOLOG && $isInfo && $log->info("syswrite returned undef: $!");

			closeStreamingSocket($httpClient);

			return undef;
		}

	} else {
		if ( main::INFOLOG && $isInfo ) {
			$log->info("\$httpClient is: $httpClient");
			if (exists $peeraddr{$httpClient}) {
				$log->info("\$peeraddr{\$httpClient} is: $peeraddr{$httpClient}");
				$log->info("Got nothing for streaming data to $peeraddr{$httpClient}");
			} else {
				$log->info("\$peeraddr{\$httpClient} is undefined");
			}
		}
		return 0;
	}

	if ($sentbytes) {

		main::INFOLOG && $isInfo && $log->info("Streamed $sentbytes to $peeraddr{$httpClient}");

		# Update sent counter if this is a range request
		if ( $rangeTotal ) {
			${*$streamingFile}{rangeCounter} += $sentbytes;
		}
	}

	return $sentbytes;
}

sub tryStreamingLater {
	my $client     = shift;
	my $httpClient = shift;

	if ( defined $client->streamingsocket() && $httpClient == $client->streamingsocket() ) {

		# Bug 10085 - This might be a callback for an old connection
		# which we decided to close after establishing the timer, so
		# only kill the timer if we were called for the active streaming connection;
		# otherwise we might kill the timer related to the next connection too.
		Slim::Utils::Timers::killTimers($client, \&tryStreamingLater);
	}

	# Bug 14740 - always call sendStreamingResponse so we ensure the socket gets closed
	Slim::Networking::Select::addWrite($httpClient, \&sendStreamingResponse, 1);
}

sub forgetClient {
	my $client = shift;

	if (defined($client->streamingsocket)) {
		closeStreamingSocket($client->streamingsocket);
	}
}

sub closeHTTPSocket {
	my ( $httpClient, $streaming, $reason ) = @_;

	return unless $httpClient;

	$reason ||= 'closed normally';

	main::INFOLOG && $log->is_info && $log->info("Closing HTTP socket $httpClient with " . ($peeraddr{$httpClient} || 'unknown') . ":" . ($httpClient->peerport || 0). " ($reason)");

	Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );

	Slim::Networking::Select::removeRead($httpClient);
	Slim::Networking::Select::removeWrite($httpClient);
	Slim::Networking::Select::removeError($httpClient);

	if (defined $streamingFiles{$httpClient}) {
		main::INFOLOG && $log->is_info && $log->info("Closing streaming file.");
		close  $streamingFiles{$httpClient};
		delete $streamingFiles{$httpClient};
	}

	my $wasOpen = delete $openSockets{$httpClient};

	# clean up the various caches
	delete($outbuf{$httpClient});
	delete($sendMetaData{$httpClient});
	delete($metaDataBytes{$httpClient});
	delete($peeraddr{$httpClient});
	delete($keepAlives{$httpClient});
	delete($peerclient{$httpClient});

	if ($wasOpen) {
		# heads up to handlers, if any
		for my $func (@closeHandlers) {
			if (ref($func) eq 'CODE') {

				# XXX: should this use eval?
				&{$func}($httpClient);
			}
		}

		# Fix for bug 1289. A close on its own wasn't always actually
		# sending a FIN or RST packet until significantly later for
		# streaming connections. The call to shutdown seems to be a
		# little more assertive about closing the socket. Windows-only
		# for now, but could be considered for other platforms and
		# non-streaming connections.
		if (main::ISWINDOWS) {
			$httpClient->shutdown(2);
		}

		$httpClient->close();
		$connected-- if $connected > 0;
	}
	elsif ($httpClient->opened()) {
		$httpClient->close();
	}

	undef($httpClient);
}

sub closeStreamingSocket {
	my ($httpClient, $force) = @_;

	return unless $httpClient;

	if (defined $streamingFiles{$httpClient}) {

		main::INFOLOG && $log->is_info && $log->info("Closing streaming file.");

		close  $streamingFiles{$httpClient};
		delete $streamingFiles{$httpClient};
	}

	foreach my $client (Slim::Player::Client::clients()) {

		if (defined($client->streamingsocket) && $client->streamingsocket == $httpClient) {
			$client->streamingsocket(undef);
		}
	}

	# Close socket unless it's a healthy keep-alive connection
	if ( !$force && $keepAlives{$httpClient} && $httpClient->connected() ) {
		main::INFOLOG && $log->is_info && $log->info('Keep-alive on streaming socket');

		delete($outbuf{$httpClient});
		delete($sendMetaData{$httpClient});
		delete($metaDataBytes{$httpClient});
		delete($peerclient{$httpClient});

		Slim::Networking::Select::addRead($httpClient, \&processHTTP);
		Slim::Networking::Select::removeWrite($httpClient);

		Slim::Utils::Timers::killTimers( $httpClient, \&closeHTTPSocket );
		Slim::Utils::Timers::setTimer(
			$httpClient,
			time() + KEEPALIVETIMEOUT,
			\&closeHTTPSocket,
			0,
			'keep-alive timeout',
		);
	}
	else {
		main::INFOLOG && $log->is_info && $log->info('Closing streaming socket');
		delete $keepAlives{$httpClient};
		closeHTTPSocket($httpClient, 1, $force ? 'socket error' : 'stream closed');
	}

	return;
}

sub shouldHandleCookies {
	my ($path) = @_;
	return unless $path;
	return $path && $path !~ m/(?:gif|png|jpe?g|css)$/i && $path !~ m{^/(?:music/[a-f\d]+/cover|contributor/[a-f\d]+/image|imageproxy/.*/image)};
}

sub checkAuthorization {
	my $username = shift;
	my $password = shift;
	my $request  = shift;

	# No authorization needed
	if (!$prefs->get('authorize')) {
		return 1;
	}

	# Check ephemeral server token for internal scanner requests (no pass-the-hash)
	if ( $request && defined $SCANNER_AUTH_TOKEN && $SCANNER_AUTH_TOKEN ne '' ) {
		my $hdrToken = $request->header('X-Scanner-Token');
		$hdrToken = $request->header('X-Scanner') unless defined $hdrToken && $hdrToken ne '';
		if ( defined($hdrToken) && $hdrToken ne '' && _constantTimeEq($hdrToken, $SCANNER_AUTH_TOKEN) ) {
			return 1;
		}
	}

	# Require defined non-empty password
	return 0 unless defined($password) && $password ne '';

	my $ok       = 0;
	my $prefUser = $prefs->get('username');
	my $pwd      = $prefs->get('password');

	if ( defined($username) && $username ne ''
		&& defined($prefUser) && $prefUser ne ''
		&& _constantTimeEq($username, $prefUser)
	) {
		return 0 unless defined($pwd) && $pwd ne '';

		$ok = _constantTimeEq(sha1_base64($password), $pwd);

		# bug 11003 - try crypt if sha1 fails, keep backwards compatibility
		# this should be removed some releases after 7.4
		if (!$ok && length($pwd) >= 2) {
			my $salt = substr($pwd, 0, 2);
			$ok = _constantTimeEq(crypt($password, $salt), $pwd);
		}
	} else {
		foreach my $client (Slim::Player::Client::clients()) {
			my $clientPwd = $client->password();
			if (defined($clientPwd) && $clientPwd ne '' && _constantTimeEq($clientPwd, $password)) {
				$ok = 1;
				last;
			}
		}
	}

	return $ok;
}

sub checkCORS {
	my ($request, $response) = @_;

	if ( my $allowedHosts = $prefs->get('corsAllowedHosts') ) {
		$response->header('Vary' => 'Origin') unless defined $response->header('Vary');

		my $host   = $request->header('Host');
		my $origin = $request->header('Origin');

		return $response->header('Access-Control-Allow-Origin')
			unless defined($origin) && $origin ne '' && defined($host) && $host ne '';

		return undef if $origin =~ /[\r\n\0\s]/;

		my $origUri = eval { URI->new($origin) };
		return undef unless $origUri && $origUri->scheme && $origUri->scheme =~ /^https?$/i;
		return undef if defined($origUri->userinfo) || defined($origUri->query) || defined($origUri->fragment);

		my $origPath = $origUri->path();
		return undef if defined($origPath) && $origPath ne '' && $origPath ne '/';

		my $origHost = eval { $origUri->host() };
		my $origPort = eval { $origUri->port() };
		return undef unless defined($origHost) && $origHost ne '' && defined($origPort);

		my $normOrigin = lc($origUri->scheme) . '://' . lc($origHost) . ':' . $origPort;

		$host =~ tr/\r\n\0//d;
		my $reqHostPort = $host =~ m/:\d+$/ ? lc($host) : lc($host) . ':80';

		if ( lc("$origHost:$origPort") ne $reqHostPort ) {
			my $matched = 0;

			for my $entry (split /[,\s]+/, $allowedHosts) {
				next unless defined($entry) && $entry ne '';
				if ($entry eq '*') {
					$response->header('Access-Control-Allow-Origin' => '*');
					$matched = 1;
					last;
				}
				my $entryUri = eval { URI->new($entry) };
				next unless $entryUri && $entryUri->scheme && $entryUri->scheme =~ /^https?$/i && $entryUri->host;
				next if defined($entryUri->userinfo) || defined($entryUri->query) || defined($entryUri->fragment);
				my $entryPath = $entryUri->path();
				next if defined($entryPath) && $entryPath ne '' && $entryPath ne '/';

				my $normEntry = lc($entryUri->scheme) . '://' . lc($entryUri->host) . ':' . $entryUri->port;
				if ($normEntry eq $normOrigin) {
					$response->header('Access-Control-Allow-Origin' => $origin);
					$matched = 1;
					last;
				}
			}

			if (!$matched && main::INFOLOG && $log->is_info) {
				$log->info("CORS is not enabled for $origin");
			}
		}
	}

	return $response->header('Access-Control-Allow-Origin');
}

# addCloseHandler
# defines a function to be called when $httpClient is closed
# prototype: func($httpClient), no return value
sub addCloseHandler{
	my $funcPtr = shift;

	if ( main::DEBUGLOG && $log->is_debug ) {
		my $funcName = Slim::Utils::PerlRunTime::realNameForCodeRef($funcPtr);
		$log->debug("Adding Close handler: $funcName");
	}

	push @closeHandlers, $funcPtr;
}


# Fills the template file specified as $path, using either the currently
# selected skin, or an override. Returns the filled template string
# these are all very similar

sub filltemplatefile {
	return $skinMgr->_generateContentFromFile('fill', @_);
}

sub getStaticContent {
	return $skinMgr->_generateContentFromFile('get', @_);
}

sub getFileInfoForStaticContent {
	return $skinMgr->_generateContentFromFile('mtime', @_);
}

sub getStaticContentForTemplate {
	my $ref = $skinMgr->_generateContentFromFile('get', @_);
	return (ref($ref) eq 'SCALAR' && defined $$ref) ? $$ref : '';
}


sub addTemplateDirectory {
	$skinMgr->addTemplateDirectory(@_);
}

sub fixHttpPath {
	$skinMgr->fixHttpPath(@_);
}

# the following subs have been moved to Slim::Web::Pages in SC 7.4
# backwards compatibility should be removed at some reasonable point
sub addPageFunction {
	logBacktrace("Slim::Web::HTTP::addPageFunction() is deprecated - please use Slim::Web::Pages->addPageFunction() instead");
	Slim::Web::Pages->addPageFunction(@_);
}

sub addRawFunction {
	logBacktrace("Slim::Web::HTTP::addRawFunction() is deprecated - please use Slim::Web::Pages->addRawFunction() instead");
	Slim::Web::Pages->addRawFunction(@_);
}

sub addRawDownload {
	logBacktrace("Slim::Web::HTTP::addRawDownload() is deprecated - please use Slim::Web::Pages->addRawDownload() instead");
	Slim::Web::Pages->addRawDownload(@_);
}

sub removeRawDownload {
	logBacktrace("Slim::Web::HTTP::removeRawDownload() is deprecated - please use Slim::Web::Pages->removeRawDownload() instead");
	Slim::Web::Pages->removeRawDownload(@_);
}

sub protectURI { if ( main::WEBUI ) {
	logBacktrace("Slim::Web::HTTP::protectURI() is deprecated - please use Slim::Web::HTTP::CSRF->protectURI() instead");
	Slim::Web::HTTP::CSRF->protectURI(@_);
} }

sub protectName { if ( main::WEBUI ) {
	logBacktrace("Slim::Web::HTTP::protectName() is deprecated - please use Slim::Web::HTTP::CSRF->protectName() instead");
	Slim::Web::HTTP::CSRF->protectName(@_);
} }

sub protectCommand { if ( main::WEBUI ) {
	logBacktrace("Slim::Web::HTTP::protectCommand() is deprecated - please use Slim::Web::HTTP::CSRF->protectCommand() instead");
	Slim::Web::HTTP::CSRF->protectCommand(@_);
} }

sub protect { if ( main::WEBUI ) {
	logBacktrace("Slim::Web::HTTP::protect() is deprecated - please use Slim::Web::HTTP::CSRF->protect() instead");
	Slim::Web::HTTP::CSRF->protect(@_);
} }

sub downloadMusicFile {
	my ($httpClient, $response, $id) = @_;

	# Support transferMode.dlna.org (DLNA 7.4.49)
	my $tm = $response->request->header('transferMode.dlna.org') || 'Streaming';
	if ( $tm =~ /^(?:Streaming|Background)$/i ) {
		$response->header( 'transferMode.dlna.org' => $tm );
	}
	else {
		_sendStreamingError($httpClient, $response, 406, 0);
		return;
	}

	my $obj = Slim::Schema->find('Track', $id);

	if (blessed($obj) && Slim::Music::Info::isSong($obj) && Slim::Music::Info::isFile($obj->url)) {

		# Bug 8808, support transcoding if a file extension is provided
		my $uri    = $response->request->uri;
		my $isHead = $response->request->method eq 'HEAD';

		if ( my ($outFormat) = $uri =~ m{download\.([^\?]+)} ) {
			# if the file extension itself is no valid content type, try to figure it out
			if ( !Slim::Music::Info::isSong(undef, $outFormat) ) {
				$outFormat = Slim::Music::Info::typeFromSuffix($uri);
			}

			if ( $obj->content_type ne $outFormat ) {
				if ( main::TRANSCODING ) {
					# Also support LAME bitrate/quality
					my ($bitrate) = $uri =~ m{bitrate=(\d+)};
					my ($quality) = $uri =~ m{quality=(\d)};
					$quality = 9 unless defined($quality) && $quality =~ m{^[0-9]$};

					# Use aif because DLNA specifies big-endian format
					$outFormat = 'aif' if $outFormat =~ /^(?:aiff?|wav)$/;

					my ($transcoder, $error) = Slim::Player::TranscodingHelper::getConvertCommand2(
						$obj,
						undef, # content-type will be determined from $obj
						['F'], # File stream mode
						[],
						[],
						$outFormat,
						$bitrate || 0,
					);

					if ( !$transcoder ) {
						$log->error("Couldn't transcode " . $obj->url . " to $outFormat: $error");

						$response->code(400);
						$response->headers->remove_content_headers;
						addHTTPResponse($httpClient, $response, \'', 1, 0);
						return 1;
					}

					my $command = Slim::Player::TranscodingHelper::tokenizeConvertCommand2(
						$transcoder, $obj->path, $obj->url, undef, $quality
					);

					if ( !$command ) {
						$log->error("Couldn't create transcoder command-line for " . $obj->url . " to $outFormat");

						$response->code(400);
						$response->headers->remove_content_headers;
						addHTTPResponse($httpClient, $response, \'', 1, 0);
						return 1;
					}

					my $in;
					my $out;
					my $w;
					my $done = 0;

					if ( !$isHead ) {
						main::INFOLOG && $log->is_info && $log->info("Opening transcoded download (" . $transcoder->{profile} . "), command: $command");

						# Bug: 4318
						# On windows ensure a child window is not opened if $command includes transcode processes
						if (main::ISWINDOWS) {
							Win32::SetChildShowWindow(0);
							$in = FileHandle->new;
							my $pid = $in->open($command);
							if (!$pid) {
								$in->close;
								undef $in;
							}
							Win32::SetChildShowWindow();
						} else {
							$in = FileHandle->new($command);
						}

						if ( !$in ) {
							$log->error("Couldn't open transcoder pipeline for " . $obj->url . ": $!");

							$response->code(500);
							$response->headers->remove_content_headers;
							addHTTPResponse($httpClient, $response, \'', 1, 0);
							return 1;
						}

						Slim::Utils::Network::blocking($in, 0);
					}

					if ($outFormat eq 'aif') {
						# Construct special PCM content-type
						$response->content_type( 'audio/L16;rate=' . $obj->samplerate . ';channels=' . $obj->channels );
					}
					else {
						$response->content_type( $Slim::Music::Info::types{$outFormat} || 'application/octet-stream' );
					}

					# Tell client range requests are not supported
					$response->header( 'Accept-Ranges' => 'none' );

					my $filename = basename(Slim::Utils::Misc::pathFromFileURL($obj->url));
					if ($filename !~ s/\.[^.\/\\]+$/\.$outFormat/) {
						$filename .= ".$outFormat";
					}
					my $safeName = _sanitizeDispositionFilename($filename);
					$response->header('Content-Disposition',
						sprintf('attachment; filename="%s"', $safeName)
					);

					my $is11 = $response->request->protocol eq 'HTTP/1.1';

					if ($is11) {
						# Use chunked TE for HTTP/1.1 clients
						$response->header( 'Transfer-Encoding' => 'chunked' );
					}

					# Add DLNA HTTP header, with ORG_CI to indicate transcoding, and lack of ORG_OP to indicate no seeking
					my $dlna;
					if ( $outFormat eq 'mp3' ) {
						$dlna = 'DLNA.ORG_PN=MP3;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01700000000000000000000000000000';
					}
					elsif ( $outFormat eq 'aif' ) {
						$dlna = 'DLNA.ORG_PN=LPCM;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01700000000000000000000000000000';
					}
					if ($dlna) {
						$response->header( 'contentFeatures.dlna.org' => $dlna );
					}

					my $headers = _stringifyHeaders($response) . $CRLF;

					# Remove Select watchers before handing $httpClient to AnyEvent::Handle
					Slim::Networking::Select::removeRead($httpClient);
					Slim::Networking::Select::removeWrite($httpClient);
					Slim::Networking::Select::removeError($httpClient);

					# non-blocking stream $pipeline to $httpClient
					my $writer; $writer = sub {
						if (defined $headers) {
							my $hdr = $headers;
							undef $headers;
							$out->push_write($hdr);

							if ($isHead) {
								$done = 1;
								return;
							}
						}

						if ($done) {
							undef $w;
							$out && $out->destroy;
							$in && $in->close;
							undef $writer;

							if ( $httpClient->opened() ) {
								closeHTTPSocket($httpClient);
							}
							return;
						}

						if ($in) {
							# Try to read some data from the pipeline
							my $len = sysread $in, my $buf, 32 * 1024;
							if ( !defined $len ) {
								if ($! == EWOULDBLOCK) {
									$w = AnyEvent->io( fh => $in, poll => 'r', cb => sub {
										undef $w;
										$in && !$done && $writer && $writer->();
									} );
								}
								else {
									$done = 1;
									undef $w;
									if ($is11) {
										$out->push_write( '0' . $CRLF . $CRLF );
									}
									else {
										$writer->();
									}
								}
							}
							elsif ( $len == 0 ) {
								$done = 1;
								undef $w;

								if ($is11) {
									# Add last empty chunk
									$out->push_write( '0' . $CRLF . $CRLF );
								}
								else {
									$writer->(); # clean up & close
								}
							}
							else {
								if ($is11) {
									$out->push_write( sprintf("%X", length($buf)) . $CRLF . $buf . $CRLF );
								}
								else {
									$out->push_write($buf);
								}
							}
						}
					};

					$out = AnyEvent::Handle->new(
						fh         => $httpClient,
						linger     => 0,
						timeout    => 300,
						on_timeout => sub {
							main::INFOLOG && $log->is_info && $log->info("Timing out transcoded download for $httpClient");
							undef $w;
							$done = 1;
							$writer && $writer->();
						},
						on_error   => sub {
							my ($hdl, $fatal, $msg) = @_;
							main::INFOLOG && $log->is_info && $log->info("Transcoded download error: $msg");
							undef $w;
							$done = 1;
							$writer && $writer->();
						},
					);

					# Bug 17212, Must add callback after object creation - references to $out within the $writer callback were
					# failing when the on_drain callback was passed as a constructor argument
					$out->on_drain($writer);

					return 1;
				}
				else {
					# Transcoding is not enabled, return 400
					$log->error("Transcoding is not enabled for " . $obj->url . " to $outFormat");

					$response->code(400);
					$response->headers->remove_content_headers;
					addHTTPResponse($httpClient, $response, \'', 1, 0);
					return 1;
				}
			}
		}

		main::INFOLOG && $log->is_info && $log->info("Opening $obj for download...");

		my $ct = $Slim::Music::Info::types{$obj->content_type()} || 'application/octet-stream';

		# Add DLNA HTTP header
		if ( my $pn = $obj->dlna_profile ) {
			my $canseek = ($pn eq 'MP3' || $pn =~ /^WMA/);
			my $dlna = "DLNA.ORG_PN=${pn};DLNA.ORG_OP=" . ($canseek ? '11' : '01') . ";DLNA.ORG_FLAGS=01700000000000000000000000000000";
			$response->header( 'contentFeatures.dlna.org' => $dlna );
		}

		Slim::Web::HTTP::sendStreamingFile( $httpClient, $response, $ct, Slim::Utils::Misc::pathFromFileURL($obj->url), $obj );

		return 1;
	}

	return;
}

1;

__END__

# Local Variables:
# tab-width:4
# indent-tabs-mode:t
# End:
