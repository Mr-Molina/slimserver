package Slim::Web::HTTP::CSRF;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use Digest::SHA qw(hmac_sha256_hex);
use HTTP::Status qw(RC_FORBIDDEN);
use URI;

use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Network;
use Slim::Utils::Prefs;
use Slim::Utils::Strings qw(string);
use Slim::Web::HTTP;

use constant CSRF_TOKEN_TTL => 3600;

my $log = logger('network.http');
my $prefs = preferences('server');

my %dangerousCommands;


sub _escapeHTML {
	my ($str) = @_;
	return '' unless defined $str;

	$str =~ s/&/&amp;/g;
	$str =~ s/</&lt;/g;
	$str =~ s/>/&gt;/g;
	$str =~ s/"/&quot;/g;
	$str =~ s/'/&#39;/g;

	return $str;
}

sub _constantTimeEquals {
	my ($a, $b) = @_;

	return 0 unless defined($a) && defined($b);
	return 0 if length($a) != length($b);

	my $diff = 0;
	for (my $i = 0; $i < length($a); $i++) {
		$diff |= ord(substr($a, $i, 1)) ^ ord(substr($b, $i, 1));
	}

	return $diff == 0 ? 1 : 0;
}

sub _normalizeURIPath {
	my ($raw) = @_;

	return '/' unless defined $raw;

	my $path = "$raw";
	$path =~ s/[?#].*$//;
	$path = Slim::Utils::Misc::unescape($path);
	$path =~ tr{\\}{/};
	$path =~ s{/+}{/}g;

	my @parts;
	for my $seg (split m{/}, $path, -1) {
		next if $seg eq '' || $seg eq '.';
		if ($seg eq '..') {
			pop @parts if @parts;
		} else {
			push @parts, $seg;
		}
	}

	return '/' . join('/', @parts);
}

sub _normalizeRequestTarget {
	my ($raw) = @_;

	return '/' unless defined $raw;

	my $str = "$raw";
	$str =~ s/#.*$//;
	my ($rawPath, $query) = split /\?/, $str, 2;
	my $normPath = _normalizeURIPath($rawPath);

	return defined($query) ? "$normPath?$query" : $normPath;
}

sub _getValidSecret {
	my $secret = $prefs->get('securitySecret');

	if ( !defined($secret) || $secret !~ m|^[0-9a-f]{32,}$|i ) {
		return undef;
	}

	return $secret;
}

sub _parseHostPort {
	my ($rawHost, $defaultPort) = @_;

	return unless defined($rawHost) && length($rawHost);
	return if $rawHost =~ /[\s\@\/\\]/;

	my $h = lc($rawHost);
	my ($host, $port);

	if ( $h =~ m|^\[([^\[\]]+)\](?::(\d+))?$| ) {
		$host = $1;
		$port = $2;
	} elsif ( $h =~ m|^([^:]+)(?::(\d+))?$| ) {
		$host = $1;
		$port = $2;
	} else {
		return;
	}

	$host =~ s/\.+$//;
	return unless length($host);

	if ( defined($port) ) {
		return if $port < 1 || $port > 65535;
		$port = int($port);
	} else {
		$port = $defaultPort;
	}

	return ($host, $port);
}

sub _isHostInServerAllowlist {
	my ($scheme, $host, $port) = @_;

	return 0 unless defined($scheme) && defined($host) && defined($port);

	my %allowedHosts = (
		'localhost' => 1,
		'127.0.0.1' => 1,
		'::1'       => 1,
	);
	my %allowedOrigins;

	eval {
		for my $candidate (
			eval { Slim::Utils::Network::serverAddr() },
			eval { Slim::Utils::Network::hostAddr() },
			eval { Slim::Utils::Network::hostName() },
			$main::httpaddr,
			$main::advertiseaddr,
		) {
			next unless defined($candidate) && length($candidate);
			my $lcHost = lc($candidate);
			$lcHost =~ s/^\[|\]$//g;
			$lcHost =~ s/\.+$//;
			$allowedHosts{$lcHost} = 1 if length($lcHost);
		}

		if ( my $corsAllowed = $prefs->get('corsAllowedHosts') ) {
			for my $entry (split /[,\s]+/, $corsAllowed) {
				next unless defined($entry) && length($entry);
				if ( $entry =~ m|^https?://|i ) {
					my $u = URI->new($entry);
					if ( $u && $u->scheme && $u->host ) {
						my $eh = lc($u->host);
						$eh =~ s/^\[|\]$//g;
						$eh =~ s/\.+$//;
						$allowedOrigins{ lc($u->scheme) . "://$eh:" . $u->port } = 1;
					}
				} else {
					my ($eh) = _parseHostPort($entry, $port);
					$allowedHosts{$eh} = 1 if defined $eh;
				}
			}
		}

		if ( my $ah = $prefs->get('allowedHosts') ) {
			for my $entry (split /,/, $ah) {
				$entry =~ s/^\s+|\s+$//g;
				next if !length($entry) || $entry =~ /[\*\-]/;
				$allowedHosts{lc($entry)} = 1;
			}
		}
	};
	return 0 if $@;

	return 1 if $allowedOrigins{"$scheme://$host:$port"};

	return 0 unless $allowedHosts{$host};

	my $httpport = $prefs->get('httpport');
	if ( defined($httpport) && $httpport =~ /^\d+$/ && $httpport > 0 ) {
		return 1 if $port == int($httpport) || $port == 80 || $port == 443;
		return 0;
	}

	return 1;
}

sub _validateSameOriginHeader {
	my ($headerVal, $reqScheme, $reqHost, $reqPort) = @_;

	return 0 unless defined($headerVal) && length($headerVal);
	return 0 if lc($headerVal) eq 'null' || $headerVal =~ /[\r\n]/;

	my $ok = eval {
		my $u = URI->new($headerVal);
		return 0 unless $u && defined($u->scheme) && defined($u->host);
		return 0 if defined($u->userinfo);

		my $oScheme = lc($u->scheme);
		return 0 unless $oScheme eq 'http' || $oScheme eq 'https';

		my $oHost = lc($u->host);
		$oHost =~ s/^\[|\]$//g;
		$oHost =~ s/\.+$//;
		return 0 unless length($oHost);

		my $oPort = $u->port;
		return 0 unless defined($oPort) && $oPort =~ /^\d+$/;

		return ($oScheme eq $reqScheme && $oHost eq $reqHost && int($oPort) == int($reqPort)) ? 1 : 0;
	};

	return 0 if $@ || !$ok;
	return 1;
}

sub getQueries {
	my ($class, $request, $csrfReqParams) = @_;

	my ($queryWithArgs, $queryToTest);

	if ($prefs->get('csrfProtectionLevel')) {

		$queryWithArgs = _normalizeURIPath($request->uri()) . '?';

		foreach my $n (sort keys %{$csrfReqParams || {}}) {
			my $vals = ref($csrfReqParams->{$n}) eq 'ARRAY' ? $csrfReqParams->{$n} : [ $csrfReqParams->{$n} ];
			foreach my $val (@$vals) {
				next unless defined $val;
				$queryWithArgs .= Slim::Utils::Misc::escape($n) . '=' . Slim::Utils::Misc::escape($val) . '&';
			}
		}

		# scrub some harmless args
		$queryToTest = $queryWithArgs;
		$queryToTest =~ s/\bplayer=.*?\&//g;
		$queryToTest =~ s/\bplayerid=.*?\&//g;
		$queryToTest =~ s/\bajaxUpdate=\d\&//g;
		$queryToTest =~ s/\?\?/\?/;
	}

	return ($queryWithArgs, $queryToTest);
}


sub testCSRFToken {
	if ($prefs->get('csrfProtectionLevel')) {

		my ($class, $httpClient, $request, $response, $params, $queryWithArgs, $queryToTest, $providedPageAntiCSRFToken) = @_;

		foreach my $dregexp ( keys %dangerousCommands ) {
			if ( defined($queryToTest) && $queryToTest =~ m|$dregexp| ) {
				if ( !$class->isRequestCSRFSafe($request, $response, $params, $providedPageAntiCSRFToken) ) {

					$log->error("Client requested dangerous function/arguments and failed CSRF Referer/token test, sending 403 denial");

					$class->throwCSRFError($httpClient, $request, $response, $params, $queryWithArgs);
					return;
				}
			}
		}
	}

	return 1;
}


# makePageToken: anti-CSRF token at the page level, e.g. token to
# protect use of /settings/server/basic.html
sub makePageToken {
	my ($class, $req, $now) = @_;

	my $secret = _getValidSecret();

	if ( !defined($secret) ) {
		# invalid secret!
		# Prefs.pm should have set this!
		$log->warn("Server unable to verify CRSF auth code due to missing or invalid securitySecret server pref");
		return '';
	}

	# strip the querystring before unescaping and collapse dot segments
	my $uri = _normalizeURIPath(defined($req) ? $req->uri() : '/');
	my $ts  = defined($now) ? int($now) : time();
	my $mac = hmac_sha256_hex("page\0$uri\0$ts", $secret);

	return "$ts:$mac";
}

sub isRequestCSRFSafe {
	my ($class, $request, $response, $params, $providedPageAntiCSRFToken) = @_;

	return 0 unless $request;

	my ($hostHeader, $origin, $referer, $reqUri);
	my $hdrOk = eval {
		$hostHeader = $request->header('Host');
		$origin     = $request->header('Origin');
		$referer    = $request->header('Referer');
		$reqUri     = $request->uri();
		1;
	};
	if ( $@ || !$hdrOk ) {
		$log->warn("Failed reading request headers during CSRF validation: $@");
		return 0;
	}

	# Validate Origin and Referer when either is present (no X-Requested-With bypass)
	if ( defined($origin) || defined($referer) ) {
		my $reqScheme = 'http';
		eval {
			if ( defined($reqUri) && "$reqUri" =~ m|^(https?)://|i ) {
				$reqScheme = lc($1);
			} elsif ( defined($origin) && $origin =~ m|^https://|i ) {
				# Only allow https scheme match when server has SSL / insecureHTTPS not forcing http
				$reqScheme = 'https' if Slim::Networking::Async::HTTP->hasSSL() && !$prefs->get('insecureHTTPS');
			}
		};
		return 0 if $@;

		my $defaultPort = ($reqScheme eq 'https') ? 443 : 80;
		my ($reqHost, $reqPort) = _parseHostPort($hostHeader, $defaultPort);
		if ( !defined($reqHost) || !_isHostInServerAllowlist($reqScheme, $reqHost, $reqPort) ) {
			$params->{'suggestion'} = "Invalid Host header or origin." if ref($params) eq 'HASH';
			$log->warn("Invalid Host during CSRF validation: [" . ($hostHeader // '') . "]");
			return 0;
		}

		if ( defined($origin) && !_validateSameOriginHeader($origin, $reqScheme, $reqHost, $reqPort) ) {
			$params->{'suggestion'} = "Invalid Origin header." if ref($params) eq 'HASH';
			$log->warn("Invalid Origin: [" . join(' ', ($request->method // '', $reqUri // '', $origin)) . "]");
			return 0;
		}

		if ( defined($referer) && !_validateSameOriginHeader($referer, $reqScheme, $reqHost, $reqPort) ) {
			$params->{'suggestion'} = "Invalid Referer header." if ref($params) eq 'HASH';
			$log->warn("Invalid referer: [" . join(' ', ($request->method // '', $reqUri // '')) . "]");
			return 0;
		}

		return 1;
	}

	if ( !$class->isCsrfAuthCodeValid($request, $providedPageAntiCSRFToken) ) {
		$params->{'suggestion'} = "Invalid referrer and no valid cauth code." if ref($params) eq 'HASH';

		if ( $log->is_warn ) {
			$log->warn("No valid CSRF auth code: [" .
				join(' ', ($request->method // '', $reqUri // '', $request->header('X-Slim-CSRF') // ''))
			. "]");
		}

		return 0;
	}

	return 1;
}

sub isCsrfAuthCodeValid {
	my ($class, $request, $providedPageAntiCSRFToken, $now) = @_;

	my $csrfProtectionLevel = $prefs->get('csrfProtectionLevel');

	if (! defined($csrfProtectionLevel) ) {

		# Prefs.pm should have set this!
		$log->warn("Warning: Server unable to determine CRSF protection level due to missing server pref");

		return 0;
	}

	# no protection, so we don't care
	return 1 if ( !$csrfProtectionLevel);

	my $uri  = $request ? $request->uri() : undef;
	my $code = $request ? $request->header("X-Slim-CSRF") : undef;

	if ( ! defined($uri) ) {
		return 0;
	}

	my $secret = _getValidSecret();

	if ( !defined($secret) ) {

		# invalid secret!
		$log->warn("Server unable to verify CRSF auth code due to missing or invalid securitySecret server pref");

		return 0;
	}

	$now = defined($now) ? int($now) : time();
	my $normTarget = _normalizeRequestTarget($uri);
	my $normPath   = _normalizeURIPath($uri);

	# Validate HMAC-SHA256 X-Slim-CSRF token with TTL window (no static Level 1/2 bypass)
	if ( defined($code) && $code =~ m|^(\d+):([0-9a-f]{64})$| ) {
		my ($ts, $mac) = (int($1), $2);
		if ( $ts <= $now && ($now - $ts) <= CSRF_TOKEN_TTL ) {
			my $expectedTarget = hmac_sha256_hex("cauth\0$normTarget\0$ts", $secret);
			return 1 if _constantTimeEquals($mac, $expectedTarget);

			my $expectedPath = hmac_sha256_hex("cauth\0$normPath\0$ts", $secret);
			return 1 if _constantTimeEquals($mac, $expectedPath);
		}
	}

	# Validate HMAC-SHA256 pageAntiCSRFToken with TTL window
	if ( defined($providedPageAntiCSRFToken) && $providedPageAntiCSRFToken =~ m|^(\d+):([0-9a-f]{64})$| ) {
		my ($ts, $mac) = (int($1), $2);
		if ( $ts <= $now && ($now - $ts) <= CSRF_TOKEN_TTL ) {
			my $expectedPage = hmac_sha256_hex("page\0$normPath\0$ts", $secret);
			return 1 if _constantTimeEquals($mac, $expectedPage);
		}
	}

	return 0;

}



# CSRF: allow code to indicate it needs protection
#
# The HTML template for protected actions needs to embed an anti-CSRF token. The easiest way
# to do that is include the following once inside each <form>:
# 	<input type="hidden" name="pageAntiCSRFToken" value="[% pageAntiCSRFToken %]">
#
# To protect the settings within the module that handles that page, use the "protect" APIs:
# sub name {
# 	return Slim::Web::HTTP::CSRF->protectName('BASIC_SERVER_SETTINGS');
# }
# sub page {
# 	Slim::Web::HTTP::CSRF->protectURI('settings/server/basic.html');
# }
#
# protectURI: takes the same string that a function's page() method returns
sub protectURI {
	my ($class, $uri) = @_;

	my $quoted = quotemeta($uri);
	my $regexp = "/${quoted}\\b.*\\=";
	$dangerousCommands{$regexp} = 1;

	return $uri;
}

# protectName: takes the same string that a function's name() method returns
sub protectName {
	my ($class, $name) = @_;

	my $quoted = quotemeta($name);
	my $regexp = "\\bpage=${quoted}\\b";
	$dangerousCommands{$regexp} = 1;

	return $name;
}


# normal Lyrion Music Server commands can be accessed with URLs like
#   http://localhost:9000/status.html?p0=pause&player=00%3A00%3A00%3A00%3A00%3A00
#   http://localhost:9000/status.html?command=pause&player=00%3A00%3A00%3A00%3A00%3A00
# Use the protectCommand() API to prevent CSRF attacks on commands -- including commands
# not intended for use via the web interface!
#
# protectCommand: takes an array of commands, e.g.
# protectCommand('play')			# protect any command with 'play' as the first command
# protectCommand('playlist', ['add', 'delete'])	# protect the "playlist add" and "playlist delete" commands
# protectCommand('mixer','volume','\d{1,}');	# protect changing the volume (3rd arg has digit) but allow "?" query in 3rd pos
sub protectCommand {
	my $class = shift;
	my @commands = @_;

	my $regexp = '';
	for (my $pos = 0; $pos < scalar(@commands); ++$pos) {

		my $paramPattern = ($pos == 0) ? '(?:p0|command)' : ($pos == 1) ? '(?:p1|subcommand)' : "p${pos}";
		my $rePart;

		if ( ref($commands[$pos]) eq 'ARRAY' ) {

			$rePart = '\b(';
			my $add = '';

			foreach my $c ( @{$commands[$pos]} ) {
				$rePart .= "${add}${paramPattern}=$c\\b";
				$add = '|';
			}

			$rePart .= ')';
		}

		else {
			$rePart = "\\b${paramPattern}=$commands[$pos]\\b";
		}

		$regexp .= "${rePart}.*?";
	}

	$dangerousCommands{$regexp} = 1;
}

# protect: takes an exact regexp, in case you need more fine-grained protection
#
# Example querystring for server settings:
# /status.html?audiodir=/music&language=EN&page=BASIC_SERVER_SETTINGS&playlistdir=/playlists&rescan=&rescantype=1rescan&saveSettings=Save Settings&useAJAX=1&
sub protect {
	my $regexp = shift;
	$dangerousCommands{$regexp} = 1;
}


sub throwCSRFError {
	my ($class, $httpClient, $request, $response, $params, $queryWithArgs) = @_;

	# throw 403, do not reflect raw Host or mint pre-signed ;cauth= links
	$params->{'suggestion'} = "Invalid Referer and no valid CSRF auth code." if ref($params) eq 'HASH';

	my $blockedPath = _normalizeURIPath($request ? $request->uri() : '/');
	my $escapedPath = _escapeHTML($blockedPath);

	# add a long SGML comment so Internet Explorer displays the page
	my $msg = "<!--" . ( '.' x 500 ) . "-->\n<p>";

	$msg .= string('CSRF_ERROR_INFO');
	$msg .= "</p>\n<p>Blocked URI: <code>${escapedPath}</code></p>";

	$params->{'validURL'} = $msg;

	$response->code(RC_FORBIDDEN);
	$response->content_type('text/html');
	$response->header('Connection' => 'close');
	$response->content_ref(Slim::Web::HTTP::filltemplatefile('html/errors/403.html', $params));

	$httpClient->send_response($response);
	Slim::Web::HTTP::closeHTTPSocket($httpClient);
}

sub makeAuthorizedURI {
	my ($class, $uri, $queryWithArgs, $now) = @_;

	my $secret = _getValidSecret();

	if ( !defined($secret) ) {

		# invalid secret!
		$log->warn("Server unable to compute CRSF auth code URL due to missing or invalid securitySecret server pref");

		return undef;
	}

	my $csrfProtectionLevel = $prefs->get('csrfProtectionLevel');

	if (! defined($csrfProtectionLevel) ) {

		# Prefs.pm should have set this!
		$log->warn("Server unable to determine CRSF protection level due to missing server pref");

		return undef;
	}

	if ( !defined($queryWithArgs) ) {
		return undef;
	}

	my $normTarget = _normalizeRequestTarget(defined($uri) ? $uri : $queryWithArgs);
	my $ts         = defined($now) ? int($now) : time();
	my $mac        = hmac_sha256_hex("cauth\0$normTarget\0$ts", $secret);

	return $queryWithArgs . ';cauth=' . $ts . ':' . $mac;
}



1;
