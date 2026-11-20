package Slim::Web::JSONRPC;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

# This class provides a JSON-RPC 1.0 API over HTTP to the Slim::Control::Request
# mechanism (sometimes referred to as CLI)

use strict;

use HTTP::Status qw(RC_OK RC_BAD_REQUEST RC_FORBIDDEN RC_METHOD_NOT_ALLOWED RC_REQUEST_ENTITY_TOO_LARGE RC_UNSUPPORTED_MEDIA_TYPE);
use JSON::XS qw(decode_json encode_json);
use Scalar::Util qw(blessed);

use Slim::Web::HTTP;
use Slim::Web::HTTP::CSRF;
use Slim::Utils::Compress;
use Slim::Utils::Log;
use Slim::Utils::Network;
use Slim::Utils::Prefs;

use constant MAX_JSONRPC_PAYLOAD_BYTES => 65536;
use constant MAX_JSONRPC_DEPTH         => 16;

my $log = logger('network.jsonrpc');
my $prefs = preferences('server');

# this holds a context for each connection, to enable asynchronous commands as well
# as subscriptions.
our %contexts = ();

# this array provides a function for each supported JSON method
my %methods = (
	'slim.request'        => \&requestMethod,
);

my $allLanguages = Slim::Utils::Strings::languageOptions();

# init
# initializes the JSON RPC code
sub init {
	# register our URI handler
	Slim::Web::Pages->addRawFunction('jsonrpc.js', \&handleURI);

	# register our close handler
	# we want to be called if a connection closes to clear our context
	Slim::Web::HTTP::addCloseHandler(\&handleClose);
}


# handleClose
# deletes any internal references to the $httpClient
sub handleClose {
	my $httpClient = shift || return;

	if (defined $contexts{$httpClient}) {
		main::DEBUGLOG && $log->debug("Closing any subscriptions for $httpClient");

		# remove any subscription management
		Slim::Control::Request::unregisterAutoExecute($httpClient);

		# delete the context
		delete $contexts{$httpClient};
	}
}


sub _sendErrorAndClose {
	my ($httpClient, $httpResponse, $code) = @_;

	$httpResponse->code($code);
	$httpResponse->content_type('application/json');
	$httpResponse->header('Connection' => 'close');
	$httpResponse->content('');

	$httpClient->send_response($httpResponse);
	Slim::Web::HTTP::closeHTTPSocket($httpClient);
}


# handleURI ($httpClient, $httpResponse)
# This is the callback from the HTTP code if the URI of the request matches the
# one we've registered. We're passed the HTTP client object and a HTTP response
# object.
# Decode the request as JSON-RPC 1.0.
sub handleURI {
	my ($httpClient, $httpResponse) = @_;

	main::DEBUGLOG && $log->debug("handleURI($httpClient)");

	# make sure we're connected
	if (!$httpClient || !$httpClient->connected()) {
		$log->warn("Aborting, client not connected: " . ($httpClient // 'undef'));
		return;
	}

	my $request = $httpResponse->request();
	if (!$request) {
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	# Require POST + Content-Type application/json (reject GET and form-encoded CSRF mutations)
	my $httpMethod = uc($request->method() || '');
	if ($httpMethod ne 'POST') {
		$log->warn("Rejecting non-POST JSON-RPC request: [$httpMethod]");
		_sendErrorAndClose($httpClient, $httpResponse, RC_METHOD_NOT_ALLOWED);
		return;
	}

	my $contentType = $request->header('Content-Type') // '';
	if ($contentType !~ m{^\s*application/json(?:\s*;|\s*$)}i) {
		$log->warn("Rejecting JSON-RPC request with invalid Content-Type: [$contentType]");
		_sendErrorAndClose($httpClient, $httpResponse, RC_UNSUPPORTED_MEDIA_TYPE);
		return;
	}

	# Validate Origin/Referer via CSRF->isRequestCSRFSafe when either is present;
	# permit non-browser API clients (both Origin and Referer absent) without wildcard CORS.
	my ($hostHeader, $origin, $referer);
	my $hdrOk = eval {
		$hostHeader = $request->header('Host');
		$origin     = $request->header('Origin');
		$referer    = $request->header('Referer');
		1;
	};
	if ($@ || !$hdrOk) {
		$log->warn("Failed reading request headers, failing closed: $@");
		_sendErrorAndClose($httpClient, $httpResponse, RC_FORBIDDEN);
		return;
	}

	if (defined($hostHeader)) {
		my $hostValid = eval {
			my ($h, $p) = Slim::Web::HTTP::CSRF::_parseHostPort($hostHeader, 80);
			defined($h) && Slim::Web::HTTP::CSRF::_isHostInServerAllowlist('http', $h, $p);
		};
		if ($@ || !$hostValid) {
			$log->warn("Rejecting JSON-RPC request with invalid Host: [$hostHeader]");
			_sendErrorAndClose($httpClient, $httpResponse, RC_FORBIDDEN);
			return;
		}
	}

	if (defined($origin) || defined($referer)) {
		my $safe = eval {
			Slim::Web::HTTP::CSRF->isRequestCSRFSafe($request, $httpResponse, {});
		};
		if ($@ || !$safe) {
			$log->warn("Not allowed from Origin/Referer: [" . ($origin // $referer // '') . "]");
			_sendErrorAndClose($httpClient, $httpResponse, RC_FORBIDDEN);
			return;
		}
	}

	# get the request data (POST body only)
	my $input = $request->content();

	if (!defined($input) || !length($input)) {
		$log->warn("No POST data found => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	my $inputBytes = do { use bytes; length($input) };
	if ($inputBytes > MAX_JSONRPC_PAYLOAD_BYTES) {
		$log->warn("JSON-RPC payload too large ($inputBytes bytes > " . MAX_JSONRPC_PAYLOAD_BYTES . ")");
		_sendErrorAndClose($httpClient, $httpResponse, RC_REQUEST_ENTITY_TOO_LARGE);
		return;
	}

	main::INFOLOG && $log->is_info && $log->info("POST data length: [$inputBytes bytes]");

	# Parse the input safely inside eval with max_depth(16) and HASH root
	my $procedure = eval {
		my $decoder = JSON::XS->new->max_depth(MAX_JSONRPC_DEPTH);
		$decoder->utf8(1) unless utf8::is_utf8($input);
		$decoder->decode($input);
	};

	if ($@ || ref($procedure) ne 'HASH') {
		$log->warn("Cannot parse POST data ($inputBytes bytes) into Perl hash => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	if ( main::DEBUGLOG && $log->is_debug ) {
		$log->debug( "JSON parsed procedure ($inputBytes bytes)" );
	}

	# we must have a method
	my $method = $procedure->{'method'};

	if (!$method || ref($method)) {
		$log->warn("Request has no valid method => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	# figure out the method wanted
	my $funcPtr = $methods{$method};

	if (!$funcPtr) {
		$log->warn("Unknown method $method => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	} elsif (ref($funcPtr) ne 'CODE') {
		$log->error("Procedure $method refers to non CODE ??? => closing connection");
		Slim::Web::HTTP::closeHTTPSocket($httpClient);
		return;
	}

	# parse and validate the parameters: [$playerScalar, [$cmdScalar, ...]]
	my $params = $procedure->{'params'};

	if (ref($params) ne 'ARRAY' || scalar(@$params) < 2) {
		$log->warn("Procedure $method has invalid params structure => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	if (defined($params->[0]) && ref($params->[0])) {
		$log->warn("Procedure $method has non-scalar player parameter => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	if (ref($params->[1]) ne 'ARRAY' || !scalar(@{$params->[1]}) || !defined($params->[1]->[0]) || ref($params->[1]->[0])) {
		$log->warn("Procedure $method has invalid command array parameter => closing connection");
		_sendErrorAndClose($httpClient, $httpResponse, RC_BAD_REQUEST);
		return;
	}

	# block access to "pref" & "serverpref" & server control commands from external or proxied hosts
	my $peeraddr = $Slim::Web::HTTP::peeraddr{$httpClient};
	if ( $prefs->get('protectSettings') && !$prefs->get('authorize')
		&& $params->[1]->[0] =~ /^(?:pref|serverpref|stopserver|restartserver)/i
	) {
		my $isProxied = eval {
			defined($request->header('X-Forwarded-For'))
			|| defined($request->header('Forwarded'))
			|| defined($request->header('X-Real-IP'))
			|| defined($request->header('Via'))
			? 1 : 0;
		};
		$isProxied = 1 if $@;

		my $isExternal = !$peeraddr
			|| $isProxied
			|| ( !Slim::Utils::Network::ip_is_host($peeraddr)
				&& ( Slim::Utils::Network::ip_is_gateway($peeraddr) || Slim::Utils::Network::ip_on_different_network($peeraddr) ) );

		if ($isExternal) {
			$log->error("Access to settings is restricted to direct local network or localhost: " . ($peeraddr // 'unknown') . " ($inputBytes bytes)");
			_sendErrorAndClose($httpClient, $httpResponse, RC_FORBIDDEN);
			return;
		}
	}

	# cancel any previous subscription on this connection cleanly
	my $prevContext = delete $contexts{$httpClient};
	if (Slim::Control::Request::unregisterAutoExecute($httpClient) && $prevContext && $prevContext->{'x-jive'}) {
		Slim::Web::HTTP::addHTTPLastChunk($httpClient, 0);
	}

	# create a hash to store our context
	my $context = {};
	$context->{'httpClient'} = $httpClient;
	$context->{'httpResponse'} = $httpResponse;
	$context->{'procedure'} = $procedure;

	# Detect the language the client wants content returned in
	if ( my $langHeader = $request->header('Accept-Language') ) {
		# split locales from the Accept-Language header, sort by priority (q), pick first value, convert to upper case
		# https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Accept-Language
		my ($lang) = map {
			uc($_->{l})
		} sort {
			$b->{q} <=> $a->{q}
		} map {
			my $item = $_;
			$item =~ s/^\s+|\s+$//g;
			if ($item =~ /^([\w-]+)(?:;q=(\d+(?:\.\d+)?))?$/i) {
				{
					l => $1,
					q => defined($2) ? $2 : 1,
				};
			} else {
				();
			}
		} split(',', $langHeader);

		if (defined($lang) && length($lang)) {
			# transform locale to language code (eg. en_GB to EN_GB), fall back to EN if needed
			if ($lang =~ s/-/_/) {
				$lang =~ s/^([A-Z]+)_.*/$1/ if !$allLanguages->{$lang};
			}

			$context->{lang} = $lang;
		}
	}

	if ( my $ua = ( $request->header('X-User-Agent') || $request->header('User-Agent') ) ) {
		$context->{ua} = $ua;
	}

	# Check our operational mode using our X-Jive header
	# We must be dealing with a 1.1 client because X-Jive uses chunked transfers
	# We must not be closing the connection
	my $respConn = $httpResponse->header('Connection') // '';
	my $reqConn  = $request->header('Connection') // '';
	if (defined(my $xjive = $request->header('X-Jive')) &&
		$httpClient->proto_ge('1.1') &&
		$respConn !~ /close/i &&
		$reqConn !~ /close/i) {

		main::INFOLOG && $log->info("Operating in x-jive mode for procedure $method and client $httpClient");
		$context->{'x-jive'} = 1;
		$httpResponse->header('X-Jive' => 'Jive');
	}

	# remember we need to send headers. We'll reset this once sent.
	$context->{'sendheaders'} = 1;

	# store our context. It'll get erased by the callback in HTTP.pm through handleClose
	$contexts{$httpClient} = $context;

	# jump to the code handling desired method. It is responsible to send a suitable output
	eval { &{$funcPtr}($context); };

	if ($@) {
		if ( $log->is_error ) {
			my $funcName = main::DEBUGLOG ? Slim::Utils::PerlRunTime::realNameForCodeRef($funcPtr) : 'unk';
			$log->error("While trying to run function coderef [$funcName]: [$@]");
		}
		handleClose($httpClient);
		Slim::Web::HTTP::closeHTTPSocket($httpClient);
		return;
	}
}


# writeResponse()
# Writes an JSON RPC response to the httpClient
sub writeResponse {
	my $context = shift;
	my $responseRef = shift;

	my $isDebug = main::DEBUGLOG && $log->is_debug;

	my $httpClient   = $context->{'httpClient'};
	my $httpResponse = $context->{'httpResponse'};

	if ( main::DEBUGLOG && $isDebug ) {
		# Deep-copy responseRef prior to dumping its content because dump()
		# will convert int members into strings permanently
		$log->debug( "JSON response: " . Data::Dump::dump(Storable::dclone($responseRef)) );
	}

	# Don't waste CPU cycles if we're not connected
	if (!$httpClient->connected()) {
		$log->warn("Client disconnected in writeResponse!");
		return;
	}

	# convert Perl object into JSON
	my $jsonResponse = encode_json($responseRef);

	main::DEBUGLOG && $isDebug && $log->info("JSON raw response length: [" . do { use bytes; length($jsonResponse) } . " bytes]");

	$httpResponse->code(RC_OK);

	# set a content type to 1.1 proposed value. Should work with 1.0 as it is not specified
	$httpResponse->content_type('application/json');

	use bytes;

	# send the headers only once
	my $sendheaders = $context->{'sendheaders'};
	if ($sendheaders) {
		$context->{'sendheaders'} = 0;
	}

	# in xjive mode, use chunked mode without a last chunk (i.e. we always have $more)
	my $xjive = $context->{'x-jive'};

	if ($xjive) {
		$httpResponse->header('Transfer-Encoding' => 'chunked');
	} else {
		# gzip if requested (unless debugging or less than 150 bytes)
		if ( !$isDebug && Slim::Utils::Compress::hasZlib() && (my $ae = $httpResponse->request->header('Accept-Encoding')) ) {
			my $len = length($jsonResponse);
			if ( $ae =~ /gzip/ && $len > 150 ) {
				my $output = '';
				if ( Slim::Utils::Compress::gzip( { in => \$jsonResponse, out => \$output } ) ) {
					$jsonResponse = $output;
					$httpResponse->header( 'Content-Encoding' => 'gzip' );
					$httpResponse->header( Vary => 'Accept-Encoding' );
				}
			}
		}

		$httpResponse->content_length(length($jsonResponse));
	}

	if ($sendheaders) {

		if ( main::DEBUGLOG && $isDebug ) {
			$log->debug("Response headers: [\n" . $httpResponse->as_string . "]");
		}
	}

	Slim::Web::HTTP::addHTTPResponse($httpClient, $httpResponse, \$jsonResponse, $sendheaders, $xjive);
}


# genreateJSONResponse

sub generateJSONResponse {
	my $context = shift;
	my $result = shift;

	main::DEBUGLOG && $log->debug("generateJSONResponse()");

	# create an object for the response
	my $response = {};

	# add ID if we have it
	if (defined(my $id = $context->{'procedure'}->{'id'})) {
		$response->{'id'} = $id;
	}

	# add result
	$response->{'result'} = $result;

	# while not strictly allowed, the JSON specs does not forbid to add the
	# request data to the response...
	$response->{'params'} = $context->{'procedure'}->{'params'};
	$response->{'method'} = $context->{'procedure'}->{'method'};

	writeResponse($context, $response);
}


# requestMethod
# Handles 'slim.request' calls. Creates a request object and executes it.
sub requestMethod {
	my $context = shift;

	# get the JSON-RPC params
	my $reqParams = $context->{'procedure'}->{'params'};

	if ( main::DEBUGLOG && $log->is_debug ) {
		$log->debug( "requestMethod(" . Data::Dump::dump($reqParams) . ")" );
	}

	# current style : [<player>, [cmd]]
	# proposed style: [{player:xxx, cmd:[xxx], params:{xxx}}]
	# benefit: more than one command in single request
	# HOW DOES RECEIVER PARSE???

	my $commandargs = $reqParams->[1];

	if (!$commandargs || ref($commandargs) ne 'ARRAY') {

		$log->error("commandargs undef or not an array!");
		Slim::Web::HTTP::closeHTTPSocket($context->{'httpClient'});
		return;
	}

	if (defined($reqParams->[0]) && ref($reqParams->[0])) {
		$log->error("playername is not a scalar!");
		Slim::Web::HTTP::closeHTTPSocket($context->{'httpClient'});
		return;
	}

	my $playername = $reqParams->[0];
	my $client     = Slim::Player::Client::getClient($playername);
	my $clientid = blessed($client) ? $client->id() : undef;

	if ($clientid) {
		# bug 16988 - need to update lastActivityTime in jsonrpc too
		$client->lastActivityTime( Time::HiRes::time() );

		main::INFOLOG && $log->info("Parsing command: Found client [$clientid]");
	}

	# create a request
	my $request = Slim::Control::Request->new($clientid, $commandargs);

	if ($request->isStatusDispatchable) {

		# Set language override for this request
		my $lang = $context->{lang};
		my $ua   = $context->{ua};

		my $finish;

		if ( $client ) {
			my $prevLang         = $client->languageOverride();
			my $prevControlledBy = $client->controlledBy();
			my $prevUA           = $client->controllerUA();

			$finish = sub {
				$client->languageOverride($prevLang);
				$client->controlledBy($prevControlledBy);
				$client->controllerUA($prevUA);
			};
		}

		my $asyncPending = 0;
		eval {
			if ( $client && $lang ) {
				# Only override if the currently selected language is not a regional version of the client language
				if ( ! Slim::Utils::Strings::isLanguageRegionalVersion($lang) ) {
					$client->languageOverride($lang);
				}
				$client->controlledBy('squeezeplay');
			}
			elsif ( $lang ) {
				$request->setLanguageOverride($lang);
			}

			if ( $ua && $client ) {
				$client->controllerUA($ua);
			}

			# fix the encoding and/or manage charset param
			$request->fixEncoding();

			# remember we're the source and the $httpClient
			$request->source('JSONRPC');
			$request->connectionID($context->{'httpClient'});

			if ($context->{'x-jive'}) {
				# set this in case the query can be subscribed to
				$request->autoExecuteCallback(\&requestWrite);
			}

			main::INFOLOG && $log->info("Dispatching...");

			$request->execute();

			if ($request->isStatusError()) {
				$finish->() if $finish;

				if ( $log->is_error ) {
					$log->error("Request failed with error: " . $request->getStatusText);
				}

				Slim::Web::HTTP::closeHTTPSocket($context->{'httpClient'});
				return;

			} else {

				# handle async commands
				if ($request->isStatusProcessing()) {

					main::INFOLOG && $log->info("Request is async: will be back");

					# add our write routine as a callback
					$request->callbackParameters( sub {
						eval { requestWrite(@_); 1; };
						my $cbErr = $@;
						$finish->() if $finish;
						die $cbErr if $cbErr;
					} );
					$asyncPending = 1;
					return;
				}

				$finish->() if $finish;

				# the request was successful and is not async, send results back to caller!
				requestWrite($request, $context->{'httpClient'}, $context);
			}
			1;
		};
		if (my $err = $@) {
			$finish->() if $finish && !$asyncPending;
			die $err;
		}

	} else {
		$clientid ||= $playername;
		$log->warn(($clientid ? "$clientid: " : '') . "request not dispatchable!");
		Slim::Web::HTTP::closeHTTPSocket($context->{'httpClient'});
		return;
	}
}


# requestWrite( $request $httpClient, $context)
# Writes a request downstream. $httpClient and $context are retrieved if not
# provided (from the request->connectionID and from the contexts array, respectively)
sub requestWrite {
	my $request = shift;
	my $httpClient = shift;
	my $context = shift;

	main::DEBUGLOG && $log->debug("requestWrite()");

	if (!$httpClient) {

		# recover our http client
		$httpClient = $request->connectionID();
	}

	if (!$context) {

		# recover our beloved context
		$context = $contexts{$httpClient};

		if (!$context) {
			$log->error("Context not found in requestWrite!!!!");
			return;
		}
	} else {

		if (!$httpClient) {
			$log->error("httpClient not found in requestWrite!!!!");
			return;
		}
	}

	# this should never happen, we've normally been forwarned by the closeHandler
	if (!$httpClient->connected()) {
		main::INFOLOG && $log->info("Client no longer connected in requestWrite");
		handleClose($httpClient);
		return;
	}

	generateJSONResponse($context, $request->{'_results'});
}


1;
