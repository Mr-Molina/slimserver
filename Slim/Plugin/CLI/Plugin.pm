package Slim::Plugin::CLI::Plugin;

# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.


use strict;
use IO::Socket qw(SOMAXCONN SO_KEEPALIVE SOL_SOCKET);
use Socket qw(:crlf inet_ntoa);
use Scalar::Util qw(blessed);

if ( main::WEBUI ) {
 	require Slim::Plugin::CLI::Settings;
}

use Slim::Control::Request;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Timers;
use Slim::Utils::Unicode;
use Slim::Utils::Prefs;

use constant MAX_INBUFF         => 65536;
use constant MAX_OUTBUFF        => 1024;
use constant CLI_AUTH_TIMEOUT   => 30;
use constant CLI_IDLE_TIMEOUT   => 300;
use constant LOGIN_MAX_FAILURES => 3;
use constant LOGIN_LOCKOUT_BASE => 5;
use constant LOGIN_LOCKOUT_MAX  => 300;

# This plugin provides a command-line interface to the server via a TCP/IP port.
# See cli-api.html for documentation.

# Queries and commands handled by this module:
#  can
#  exit
#  login
#  listen
#  shutdown
#  subscribe
# Other CLI queries/commands are handled through Request.pm
#
# This module also handles parameter "subscribe"

my $cli_socket;             # server socket
my $cli_socket_port = 0;    # CLI port on which socket is opened

my $cli_busy = 0;           # 1 if CLI is processing command
my $cli_subscribed = 0;     # 1 if CLI is subscribed to the notification system

our %connections;           # hash indexed by client_sock value
                            # each element is a hash with following keys
                            # .. id:            "IP:PORT" for debug
                            # .. peeraddr:      client IP string
                            # .. socket:        the socket (a hash key is *not* an
                            #                   object, but the value is...)
                            # .. inbuff:        input buffer
                            # .. outbuff:       output buffer (array)
                            # .. auth:          1 if connection authenticated (login)
                            # .. terminator:    terminator last used by client, we
                            #                   use it when replying
                            # .. subscribe:     undef if the client is not listening
                            #                   to anything, otherwise see below.
                            # .. last_activity: epoch timestamp of last activity

our %pending;

our %disconnectHandlers;

our %login_failures;

my $log = Slim::Utils::Log->addLogCategory({
	'category'     => 'plugin.cli',
	'defaultLevel' => 'ERROR',
	'description'  => 'PLUGIN_CLI',
});

my $prefs = preferences('plugin.cli');

$prefs->migrate(1, sub {
	require Slim::Utils::Prefs::OldPrefs;
	$prefs->set('cliport', Slim::Utils::Prefs::OldPrefs->get('cliport') || 9090); 1;
});

$prefs->setValidate({
	'validator' => sub {
		defined $_[1] && $_[1] =~ /^\d+$/ && ($_[1] == 0 || ($_[1] >= 1024 && $_[1] <= 65535));
	}
}, 'cliport');
$prefs->setChange(\&Slim::Plugin::CLI::Plugin::cli_socket_change, 'cliport');

my $prefsServer = preferences('server');

################################################################################
# PLUGIN CODE
################################################################################

# plugin: initialize the command line interface server
sub initPlugin {

	main::INFOLOG && $log->info("Initializing");

	if ( main::WEBUI ) {
		Slim::Plugin::CLI::Settings->new;
	}

	# register our functions

#        |requires Client
#        |  |is a Query
#        |  |  |has Tags
#        |  |  |  |Function to call
#        C  Q  T  F

    Slim::Control::Request::addDispatch(['can', '_p1', '_p2', '_p3', '_p4', '_p5', '?'],
        [0, 1, 0, \&canQuery]);
    Slim::Control::Request::addDispatch(['listen',    '_newvalue'],
        [0, 0, 0, \&listenCommand]);
    Slim::Control::Request::addDispatch(['listen',    '?'],
        [0, 1, 0, \&listenQuery]);
    Slim::Control::Request::addDispatch(['subscribe', '_functions'],
        [0, 0, 0, \&subscribeCommand]);

	# open our socket
	cli_socket_change();

	# register handlers for discovery packets
	Slim::Networking::Discovery::addTLVHandler({
		'CLIA' => sub { (defined $::cliaddr && length $::cliaddr) ? $::cliaddr : '127.0.0.1' }, # cli address
		'CLIP' => sub { $cli_socket_port },                                                       # cli port
	});
}

# plugin: name of our plugin
sub getDisplayName {
	return 'PLUGIN_CLI';
}

# plugin: shutdown the CLI
sub shutdownPlugin {
	my $exiting = shift;

	main::INFOLOG && $log->info("Shutting down..");

	# close all connections
	foreach my $key (keys %connections) {

		# retrieve the socket object
		my $client_socket = $connections{$key}{'socket'};
		next unless defined $client_socket;

		# close the connection
		client_socket_close($client_socket);
	}

	%pending = ();

	# close the socket
	cli_socket_close();
}

# plugin strings at the end of the file


################################################################################
# SOCKETS
################################################################################

sub _is_login_token {
	my $tok = shift;
	return 0 unless defined $tok;
	(my $clean = $tok) =~ s/%([0-9A-Fa-f]{2})/chr(hex($1))/ge;
	$clean =~ s/[\x00-\x1F\x7F]//g;
	return lc($clean) eq 'login';
}

sub _sanitize_log {
	my $str = shift;
	return '' unless defined $str;
	my @lines = split /[\r\n\x00]+/, $str;
	for my $line (@lines) {
		$line =~ s{[\x01-\x08\x0B\x0C\x0E-\x1F\x7F]}{}g;
		if ( $line =~ m{^(\s*)(\S+)(\s+\S+\s+)(.+)$} && _is_login_token($2) ) {
			$line = "${1}${2}${3}******";
		}
		elsif ( $line =~ m{^(\s*\S+\s+)(\S+)(\s+\S+\s+)(.+)$} && _is_login_token($2) ) {
			$line = "${1}${2}${3}******";
		}
		elsif ( $line =~ m{^(\s*)(\S+)(\s+)(\S+)\s*$} && _is_login_token($2) ) {
			$line = "${1}${2}${3}******";
		}
		$line =~ s{[\x00-\x1F\x7F]}{}g;
	}
	return join(' ', grep { length } @lines);
}

sub _client_ip {
	my $client_socket = shift || return;
	if (exists $connections{$client_socket}) {
		return $connections{$client_socket}{'peeraddr'}
			if defined $connections{$client_socket}{'peeraddr'};
		if (defined $connections{$client_socket}{'id'}) {
			my $ip = $connections{$client_socket}{'id'};
			$ip =~ s/:[^:]*$//;
			return $ip if length $ip;
		}
	}
	if (blessed($client_socket) && $client_socket->can('peeraddr')) {
		my $raw = $client_socket->peeraddr;
		return inet_ntoa($raw) if defined $raw && length($raw) == 4;
	}
	return;
}

sub _is_localhost {
	my $client_socket = shift || return 0;
	my $peer = _client_ip($client_socket);
	return (defined $peer && ($peer eq '127.0.0.1' || $peer eq '::1')) ? 1 : 0;
}

sub _idle_timeout_for {
	my $client_socket = shift;
	return CLI_AUTH_TIMEOUT unless exists $connections{$client_socket} && $connections{$client_socket}{'auth'};
	return CLI_IDLE_TIMEOUT;
}

sub _schedule_idle_timer {
	my $client_socket = shift;
	return unless defined $client_socket && exists $connections{$client_socket};
	Slim::Utils::Timers::killTimers($client_socket, \&_cli_idle_timeout);
	# Subscribed connections intentionally stay idle waiting for notifications
	return if $connections{$client_socket}{'auth'}
		&& $connections{$client_socket}{'subscribe'}
		&& keys %{$connections{$client_socket}{'subscribe'}};
	Slim::Utils::Timers::setTimer(
		$client_socket,
		time() + _idle_timeout_for($client_socket),
		\&_cli_idle_timeout,
	);
}

sub _cli_idle_timeout {
	my $client_socket = shift;
	return unless defined $client_socket && exists $connections{$client_socket};
	return if $connections{$client_socket}{'auth'}
		&& $connections{$client_socket}{'subscribe'}
		&& keys %{$connections{$client_socket}{'subscribe'}};

	my $timeout = _idle_timeout_for($client_socket);
	my $elapsed = time() - ($connections{$client_socket}{'last_activity'} || 0);
	if ($elapsed >= $timeout) {
		main::INFOLOG && $log->info("Closing idle connection with " . ($connections{$client_socket}{'id'} || 'unknown'));
		client_socket_close($client_socket);
		cli_subscribe_manage();
	} else {
		Slim::Utils::Timers::setTimer(
			$client_socket,
			time() + ($timeout - $elapsed),
			\&_cli_idle_timeout,
		);
	}
}

# start our listener
sub cli_socket_open {
	my $listenerport = shift;

	main::DEBUGLOG && $log->debug("Opening on $listenerport");

	if ($listenerport) {
		my $bind_addr = (defined $::cliaddr && length $::cliaddr) ? $::cliaddr : '127.0.0.1';

		$cli_socket = IO::Socket::INET->new(
			Proto     => 'tcp',
			LocalPort => $listenerport,
			LocalAddr => $bind_addr,
			Listen    => SOMAXCONN,
			ReuseAddr => 1,
			Reuse     => 1,
			Timeout   => 0.001

		) or $log->logdie("Can't setup the listening port $listenerport: $!");

		$cli_socket_port = $listenerport;

		Slim::Networking::Select::addRead($cli_socket, \&cli_socket_accept);

		main::INFOLOG && $log->info("Now accepting connections on $bind_addr:$listenerport");
	}
}

# open or change our socket
sub cli_socket_change {

	main::DEBUGLOG && $log->debug("Begin Function");

	# get the port we must use
	my $newport = $prefs->get('cliport');
	return unless defined $newport && $newport =~ /^\d+$/ && ($newport == 0 || ($newport >= 1024 && $newport <= 65535));

	# if the port changed...
	if ($cli_socket_port != $newport) {

		# if we've already opened a socket, let's close it
		# (this is false the first time through)
		if ($cli_socket_port) {
			cli_socket_close();
		}

		# if we've got an command line interface port specified, open it up!
		if ($newport) {
			cli_socket_open($newport);
		}
	}
}


# stop our listener on cli_socket_port
sub cli_socket_close {

	main::DEBUGLOG && $log->debug("Begin Function");

	if ($cli_socket_port) {

		main::INFOLOG && $log->info("Closing socket $cli_socket_port");

		Slim::Networking::Select::removeRead($cli_socket);
		$cli_socket->close();
		$cli_socket_port = 0;
		Slim::Control::Request::unsubscribe(\&cli_request_notification);
	}
}


# accept new connection!
sub cli_socket_accept {

	main::DEBUGLOG && $log->debug("Begin Function");

	my $client_socket = $cli_socket->accept();

	if ($client_socket && $client_socket->connected && $client_socket->peeraddr) {

		# Check max connections
		if ( scalar keys %connections >= $prefsServer->get('tcpConnectMaximum') ) {

			$log->error("Warning: Closing connection: too many connections open! (" . scalar( keys %connections ) . ")" );

			$client_socket->close();

			return;
		}

		my $tmpaddr = inet_ntoa($client_socket->peeraddr);

		# Reject IPs currently under login backoff lockout
		if ($login_failures{$tmpaddr} && ($login_failures{$tmpaddr}{'locked_until'} || 0) > time()) {
			$log->warn("Rejecting CLI connection from locked-out IP $tmpaddr");
			$client_socket->close();
			return;
		}

		# Check allowed hosts
		if ( !Slim::Utils::Network::ip_is_host($tmpaddr)
			&& $prefsServer->get('protectSettings') && !$prefsServer->get('authorize')
			&& ( Slim::Utils::Network::ip_is_gateway($tmpaddr) || Slim::Utils::Network::ip_on_different_network($tmpaddr) )
		) {
			$log->error("Access to CLI is restricted to the local network or localhost: $tmpaddr");
			$client_socket->close;
		}
		elsif (!($prefsServer->get('filterHosts')) || (Slim::Utils::Network::isAllowedHost($tmpaddr))) {

			Slim::Networking::Select::addRead($client_socket, \&client_socket_read);
			Slim::Networking::Select::addError($client_socket, \&client_socket_close);

			$connections{$client_socket}{'socket'} = $client_socket;
			$connections{$client_socket}{'peeraddr'} = $tmpaddr;
			$connections{$client_socket}{'id'} = $tmpaddr.':'.$client_socket->peerport;
			$connections{$client_socket}{'inbuff'} = '';
			$connections{$client_socket}{'outbuff'} = [];
			$connections{$client_socket}{'auth'} = !$prefsServer->get('authorize');
			$connections{$client_socket}{'terminator'} = $LF;
			$connections{$client_socket}{'last_activity'} = time();

			$client_socket->setsockopt(SOL_SOCKET, SO_KEEPALIVE, 1);
			_schedule_idle_timer($client_socket);

			if ( main::INFOLOG && $log->is_info ) {
				$log->info("Accepted connection from $connections{$client_socket}{'id'} (" . (keys %connections) . " active connections)");
			}
		}
		else {

			main::INFOLOG && $log->info("Did not accept connection from $tmpaddr: unauthorized source");
			$client_socket->close;
		}

	} else {

		$log->error("Warning: Could not accept connection: $!");
	}
}

# close connection
sub client_socket_close {
	my $client_socket = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	return unless defined $client_socket;

	my $client_id = (exists $connections{$client_socket} && $connections{$client_socket}{'id'}) || 'unknown';

	Slim::Utils::Timers::killTimers($client_socket, \&_cli_idle_timeout);
	Slim::Networking::Select::removeWrite($client_socket);
	Slim::Networking::Select::removeRead($client_socket);
	Slim::Networking::Select::removeError($client_socket);

	if (blessed($client_socket) && $client_socket->can('close')) {
		$client_socket->close();
	} elsif (ref($client_socket) eq 'GLOB') {
		close $client_socket;
	}

	delete $pending{$client_socket};
	delete $connections{$client_socket};
	Slim::Control::Request::unregisterAutoExecute($client_socket);

	# Notify anyone who wants to know about this disconnection
	if ( my $handler = delete $disconnectHandlers{$client_socket} ) {
		$handler->( $client_socket );
	}

	if ( main::INFOLOG && $log->is_info ) {
		$log->info("Closed connection with $client_id (" . (keys %connections) . " active connections)");
	}
}

# data from connection
sub client_socket_read {
	my $client_socket = shift;
	use bytes;

	main::DEBUGLOG && $log->debug("Begin Function");

	# handle various error cases
	if (!defined($client_socket) || !exists $connections{$client_socket}) {

		$log->warn("Warning: client_socket undefined or unknown in client_socket_read()!");

		return;
	}

	if (!($client_socket->connected())) {

		main::INFOLOG && $log->info("Connection with $connections{$client_socket}{'id'} closed by peer");

		client_socket_close($client_socket);
		return;
	}

	# attempt to read data from the stream
	my $bytes_to_read = 4096;
	my $indata = '';
	my $bytes_read = $client_socket->sysread($indata, $bytes_to_read);

	if (!defined($bytes_read) || ($bytes_read == 0)) {

		main::INFOLOG && $log->info("Connection with $connections{$client_socket}{'id'} half-closed by peer");

		client_socket_close($client_socket);
		return;
	}

	# Cap input buffer to prevent unbounded memory growth
	if (length($connections{$client_socket}{'inbuff'} // '') + length($indata) > MAX_INBUFF) {

		$log->error("Closing connection with $connections{$client_socket}{'id'}: input buffer exceeded " . MAX_INBUFF . " bytes");

		client_socket_close($client_socket);
		cli_subscribe_manage();
		return;
	}

	# buffer the data
	$connections{$client_socket}{'inbuff'} .= $indata;
	$connections{$client_socket}{'last_activity'} = time();
	_schedule_idle_timer($client_socket);

	main::DEBUGLOG && $log->is_debug && $log->debug("$connections{$client_socket}{'id'} - Buffered [" . _sanitize_log($indata) . "]");

	# only parse when we're not busy
	if ($connections{$client_socket}{'busy'}) {

		# manage a stack of connections requiring processing
		$pending{$client_socket} = $client_socket;

		my $numpending = scalar keys %pending;

		$log->warn("Warning: $connections{$client_socket}{'id'} - BUSY!!!!! ($numpending pending)");

	} else {

		# parse and process
		# if the underlying code ever calls Idle, there is a chance
		# we get called again for the same connection (or another)
		client_socket_buf_parse($client_socket);

		# handle any pending items...
		while (scalar keys %pending) {

			main::INFOLOG && $log->info("Found pending reads");

			foreach my $key (keys %pending) {

				my $socket = delete $pending{$key};
				next unless defined $socket && exists $connections{$socket};

				my $sock_obj = $connections{$socket}{'socket'};
				next unless defined $sock_obj;
				client_socket_buf_parse($sock_obj);
			}
		}
	}
}

# parse buffer data
sub client_socket_buf_parse {
	my $client_socket = shift;

	return unless defined $client_socket && exists $connections{$client_socket};

	main::DEBUGLOG && $log->debug($connections{$client_socket}{'id'});

	if (length($connections{$client_socket}{'inbuff'} // '') > MAX_INBUFF) {
		$log->error("Closing connection with $connections{$client_socket}{'id'}: input buffer exceeded " . MAX_INBUFF . " bytes");
		client_socket_close($client_socket);
		cli_subscribe_manage();
		return;
	}

	# parse our buffer linearly to find LF, CR, CRLF, LFCR, or NUL
	while (exists $connections{$client_socket} && length($connections{$client_socket}{'inbuff'} // '')) {

		if ( $connections{$client_socket}{'inbuff'} =~ s/\A([^\r\n\x00]*)(\r\n|\n\r|\r|\n|\x00)// ) {

			my ($command, $terminator) = ($1, $2);

			# Remember the terminator used
			if ($connections{$client_socket}{'terminator'} ne $terminator) {

				$connections{$client_socket}{'terminator'} = $terminator;

				if (main::DEBUGLOG && $log->is_debug) {
					$log->debug('Using terminator ' . Data::Dump::dump($terminator) . " for $connections{$client_socket}{'id'}");
				}
			}

			next unless length $command;

			# Process the command
			# Indicate busy so that any incoming data is buffered and not parsed
			# during command processing
			$connections{$client_socket}{'busy'} = 1;

			my $exit = cli_process($client_socket, $command);

			if (exists $connections{$client_socket} && $exit != 2) {
				$connections{$client_socket}{'busy'} = 0;
			}

			if ($exit == 1) {

				client_socket_write($client_socket) if exists $connections{$client_socket};
				client_socket_close($client_socket);

				# cancel our subscription if we can
				cli_subscribe_manage();
				return;
			}
		}
		else {
			# there's data in our buffer but it doesn't match
			# so wait for more data...
			last;
		}
	}
}


# data to connection
sub client_socket_write {
	my $client_socket = shift;

	return unless defined $client_socket && exists $connections{$client_socket};

	main::DEBUGLOG && $log->debug($connections{$client_socket}{'id'});

	return unless ref $connections{$client_socket}{'outbuff'} eq 'ARRAY';
	my $message = shift(@{$connections{$client_socket}{'outbuff'}});
	my $sentbytes;

	return unless defined $message && length $message;

	if (main::INFOLOG && $log->is_info) {

		my $msg = substr($message, 0, 100);
		$msg =~ s/[\r\n\x00]+$//;

		$log->info("$connections{$client_socket}{'id'} - Sending response [" . _sanitize_log($msg) . "...]");
	}

	$sentbytes = send($client_socket, $message, 0);

	unless (defined($sentbytes)) {

		# Treat $clientsock with suspicion
		$log->error("Error: While sending to: $connections{$client_socket}{'id'}");

		client_socket_close($client_socket);

		return;
	}

	if ($sentbytes < length($message)) {

		# sent incomplete message
		unshift @{$connections{$client_socket}{'outbuff'}}, substr($message, $sentbytes);

	} else {

		# sent full message
		if (@{$connections{$client_socket}{'outbuff'}} == 0) {

			# no more messages to send
			main::INFOLOG && $log->info("Sent response to $connections{$client_socket}{'id'}");

			Slim::Networking::Select::removeWrite($client_socket);

		} else {

			main::INFOLOG && $log->info("More to send to $connections{$client_socket}{'id'}");
		}
	}
}


# buffer a response
sub client_socket_buffer {
	my $client_socket = shift;
	my $message = shift;

	return unless defined $client_socket && exists $connections{$client_socket};

	main::DEBUGLOG && $log->debug($connections{$client_socket}{'id'});

	# we're no longer busy, this is atomic
	$connections{$client_socket}{'busy'} = 0;

	$connections{$client_socket}{'outbuff'} ||= [];
	if (scalar(@{$connections{$client_socket}{'outbuff'}}) >= MAX_OUTBUFF) {
		$log->warn("Closing slow connection with $connections{$client_socket}{'id'}: output buffer exceeded " . MAX_OUTBUFF . " entries");
		client_socket_close($client_socket);
		cli_subscribe_manage();
		return;
	}

	# add the message to the buffer
	push @{$connections{$client_socket}{'outbuff'}}, $message;

	# signal select we got something to write
	Slim::Networking::Select::addWrite($client_socket, \&client_socket_write);
}

################################################################################
# COMMAND PROCESSING
################################################################################


# process command
sub cli_process {
	my($client_socket, $command) = @_;

	return 1 unless defined $client_socket && exists $connections{$client_socket};
	return 0 unless defined $command;

	my $safe_log = (main::DEBUGLOG && $log->is_debug) ? _sanitize_log($command) : undef;

	# Strip ASCII control characters from incoming CLI line
	$command =~ s/[\x00-\x1F\x7F]//g;

	main::DEBUGLOG && $log->is_debug && $log->debug($safe_log);

	$connections{$client_socket}{'last_activity'} = time();
	_schedule_idle_timer($client_socket);

	# do we close the connection after this command
	my $exit = 0;

	# Enforce auth gate BEFORE Comet forwarding, full parsing, lastActivityTime, clientRandom, and cli_request_write
	if ( !$connections{$client_socket}{'auth'} ) {

		if ( $command =~ /^\[/ || $command !~ /^\s*login(?:\s|$)/i ) {

			main::INFOLOG && $log->info("Connection requires authentication, bye!");

			logError("Connections require authentication, check login command.");
			logError("Disconnecting: " . ($connections{$client_socket}{'id'} || 'unknown'));

			return 1;
		}

		my (undef, $arrayRef) = Slim::Control::Stdio::string_to_array($command);
		if ( !defined $arrayRef || !$arrayRef->[0] || lc($arrayRef->[0]) ne 'login' ) {
			logError("Connections require authentication, check login command.");
			logError("Disconnecting: " . ($connections{$client_socket}{'id'} || 'unknown'));
			return 1;
		}

		my $request = Slim::Control::Request->new(undef, $arrayRef, 1);
		return 1 if !defined $request;

		$request->fixEncoding();
		$request->source('CLI');
		$request->connectionID($client_socket);

		$exit = cli_cmd_login($client_socket, $request);
		return $exit if $exit;

		cli_request_write($request);
		return 0;
	}

	# Pass-through authenticated Comet JSON requests to the Comet module
	if ( $command =~ /^\[/ ) {
		Slim::Web::Cometd::cliHandler( $client_socket, $command );
		return 0;
	}

	# parse the command
	my ($client, $arrayRef) = Slim::Control::Stdio::string_to_array($command);

	if (!defined $arrayRef) {
		return 0;
	}

	my $clientid = blessed($client) ? $client->id() : undef;

	# Special case, allow menu requests with a disconnected client
	if ( !$clientid && $arrayRef->[1] && $arrayRef->[1] eq 'menu' ) {
		# set the clientid anyway, will trigger special handling in S::C::Request to store as diconnected clientid
		$clientid = shift @{$arrayRef};
	}

	if ($client) {

		main::INFOLOG && $log->info("Parsing command: Found client [$clientid]");

		# Update the client's last activity time, since they sent something through the CLI
		$client->lastActivityTime( Time::HiRes::time() );
	}

	# create a request
	my $request = Slim::Control::Request->new($clientid, $arrayRef, 1);

	return 0 if !defined $request;

	# fix the encoding
	$request->fixEncoding();

	# remember we're the source and the $client_socket
	$request->source('CLI');
	$request->connectionID($client_socket);
	# set this in case the query can be subscribed to
	$request->autoExecuteCallback(\&cli_request_write);

	my $cmd = $request->getRequest(0);

	# if a command cannot be found in the dispatch table, then the request
	# name is partial or even empty. In this last case, consider the first
	# element of the array as the command
	if (!defined $cmd && $request->isStatusNotDispatchable()) {
		$cmd = $arrayRef->[0];
	}

	# give the command a client if it misses one
	if ($request->isStatusNeedsClient()) {
		$client = Slim::Player::Client::clientRandom();
		$clientid = blessed($client) ? $client->id() : undef;
		$request->clientid($clientid);

		if (main::INFOLOG && $log->is_info) {

			if (defined $client) {
				$log->info("Request [$cmd] requires client, allocated $clientid");
			} else {
				$log->warn("Request [$cmd] requires client, none found!");
			}
		}
	}

	main::INFOLOG && $log->info("Processing request [$cmd]");

	if ($cmd eq 'login') {
		$exit = cli_cmd_login($client_socket, $request);
	}

	elsif ($cmd eq 'exit') {
		$exit = 1;
	}

	elsif ($cmd eq 'shutdown' || $cmd eq 'restartserver') {
		if (!$connections{$client_socket}{'auth'} || !_is_localhost($client_socket)) {
			$log->warn("Rejected [$cmd] from unauthorized or non-localhost client: " . ($connections{$client_socket}{'id'} || 'unknown'));
			$request->setStatusBadParams();
		}
		elsif ($cmd eq 'shutdown') {
			# delay execution so we have time to reply...
			Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.2,
				\&main::stopServer);
			$exit = 1;
		}
		elsif ($request->isStatusDispatchable) {
			main::INFOLOG && $log->info("Dispatching [$cmd]");
			$request->execute();
		}
	}

	elsif ($request->isStatusDispatchable) {

		main::INFOLOG && $log->info("Dispatching [$cmd]");

		$request->execute();

		if ($request->isStatusError()) {

			if ( $log->is_error ) {
				$log->error("Request [$cmd] failed with error: " . $request->getStatusText);
			}

		} else {

			# handle async commands
			if ($request->isStatusProcessing()) {

				main::INFOLOG && $log->info("Request [$cmd] is async: will be back");

				# add our write routine as a callback
				$request->callbackParameters(\&cli_request_write);

				# return async info to caller
				return 2;
			}
		}
	}

	else {

		$log->warn("Request [$cmd] unknown or missing client -- will echo as is...");
	}

	cli_request_write($request);

	return $exit;
}

# generate a string output from a request
sub cli_request_write {
	my $request = shift;
	my $client_socket = shift;

	return unless defined $request;

	# Handle Comet JSON output data
	if ( !ref $request && $request =~ /^\[/ ) {
		return unless defined $client_socket && exists $connections{$client_socket};
		client_socket_buffer(
			$client_socket,
			$request . ($connections{$client_socket}{'terminator'} // $LF)
		);

		return;
	}

	if ( main::DEBUGLOG && $log->is_debug ) {
		$log->debug(_sanitize_log($request->getRequestString));
	}

	$client_socket = $request->connectionID() unless defined $client_socket;


	my @elements = $request->renderAsArray();

	my $output = Slim::Control::Stdio::array_to_string($request->clientid(), \@elements);

	if (defined $client_socket && exists $connections{$client_socket}) {

		client_socket_buffer($client_socket, $output . ($connections{$client_socket}{'terminator'} // $LF));

	} else {

		logError("client_socket undef or disconnected!!!");
	}
}

# callers can subscribe to disconnect events
sub addDisconnectHandler {
	my ( $socket, $callback ) = @_;

	$disconnectHandlers{$socket} = $callback;
}

################################################################################
# CLI commands & queries
################################################################################

# handles the "login" command
sub cli_cmd_login {
	my $client_socket = shift;
	my $request = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	my $login = $request->getParam('_p1');
	my $pwd   = $request->getParam('_p2');

	# Replace _p2 with ***** in all cases...
	$request->addParam('_p2', '******');

	return 1 unless defined $client_socket && exists $connections{$client_socket};

	# if we're not authorized yet, try to be...
	if ($connections{$client_socket}{'auth'} == 0) {

		my $ip  = _client_ip($client_socket) || 'unknown';
		my $now = time();

		# Prune stale unlocked entries if table grows large
		if (scalar keys %login_failures > 4096) {
			for my $k (keys %login_failures) {
				my $entry = $login_failures{$k};
				if (($entry->{'locked_until'} || 0) <= $now && ($now - ($entry->{'last_attempt'} || 0)) > 3600) {
					delete $login_failures{$k};
				}
			}
		}

		if ($login_failures{$ip} && ($login_failures{$ip}{'locked_until'} || 0) > $now) {
			logError("CLI login locked out for $ip; rejecting attempt.");
			return 1;
		}

		if (Slim::Web::HTTP::checkAuthorization($login, $pwd)) {

			main::INFOLOG && $log->info("Connection requires authentication: authorized!");

			delete $login_failures{$ip};
			$connections{$client_socket}{'auth'} = 1;
			return 0;
		}

		my $entry = $login_failures{$ip} ||= { count => 0, locked_until => 0, last_attempt => $now };
		$entry->{'count'}++;
		$entry->{'last_attempt'} = $now;
		if ($entry->{'count'} >= LOGIN_MAX_FAILURES) {
			my $exp = $entry->{'count'} - LOGIN_MAX_FAILURES;
			$exp = 10 if $exp > 10;
			my $delay = LOGIN_LOCKOUT_BASE * (2 ** $exp);
			$delay = LOGIN_LOCKOUT_MAX if $delay > LOGIN_LOCKOUT_MAX;
			$entry->{'locked_until'} = $now + $delay;
		}

		logError("Connections require authentication, wrong credentials received.");
		logError("Disconnecting: " . ($connections{$client_socket}{'id'} || 'unknown'));

		return 1;
	}

	return 0;
}

# handles the "can" query
sub canQuery {
	my $request = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	if ($request->isNotQuery([['can']])) {
		$request->setStatusBadDispatch();
		return;
	}

	my @array = ();

	# get all parameters in the array - we stored up to 5 params
	for (my $i = 1; $i <= 5; $i++) {

		my $elem = $request->getParam("_p$i");

		if (!defined $elem || $elem eq '?') {

			# remove empty and ? entries so we don't echo these back
			$request->deleteParam("_p$i");

		} else {

			# add the term to our array
			push @array, $elem;
		}
	}

	if ($array[0] eq 'login' || $array[0] eq 'shutdown' || $array[0] eq 'exit' ) {

		# these do not go through the normal mechanism and are always available
		$request->addResult('_can', 1);

	} else {

		# create a request with the array...
		my $testrequest = Slim::Control::Request->new(undef, \@array, 1);

		# ... and return if we found a func for it or not
		$request->addResult('_can', ($testrequest->isStatusNotDispatchable ? 0 : 1));

		undef $testrequest;
	}

	$request->setStatusDone();
}

# handles the "listen" command
sub listenCommand {
	my $request = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	if ($request->isNotCommand([['listen']])) {
		$request->setStatusBadDispatch();
		return;
	}

	my $param = $request->getParam('_newvalue');
	my $client_socket = $request->connectionID();

	if (!defined $client_socket || !exists $connections{$client_socket}) {
		$request->setStatusBadParams();
		return;
	}

	if (!defined $param) {
		$param = defined($connections{$client_socket}{'subscribe'}{'listen'}) ? 0 : 1;
	}

	if ($param !~ /^[01]$/) {
		$request->setStatusBadParams();
		return;
	}

	if ($param == 0) {
		cli_subscribe_terms_none($client_socket);
	}
	elsif ($param == 1) {
		cli_subscribe_terms_all($client_socket);
	}

	_schedule_idle_timer($client_socket);

	$request->setStatusDone();
}

# handles the "listen" query
sub listenQuery {
	my $request = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	if ($request->isNotQuery([['listen']])) {
		$request->setStatusBadDispatch();
		return;
	}

	my $client_socket = $request->connectionID();

	if (!defined $client_socket || !exists $connections{$client_socket}) {
		$request->setStatusBadParams();
		return;
	}

	$request->addResult('_listen',  defined($connections{$client_socket}{'subscribe'}{'listen'}) ? 1 : 0);

	$request->setStatusDone();
}

# handles the "subscribe" command
sub subscribeCommand {
	my $request = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	if ($request->isNotCommand([['subscribe']])) {
		$request->setStatusBadDispatch();
		return;
	}

	my $param = $request->getParam('_functions');
	my $client_socket = $request->connectionID();

	if (!defined $client_socket || !exists $connections{$client_socket}) {
		$request->setStatusBadParams();
		return;
	}

	if (defined $param && length $param) {
		my @elems = split(/,/, $param);
		cli_subscribe_terms($client_socket, \@elems);
	} else {
		cli_subscribe_terms_none($client_socket);
	}

	_schedule_idle_timer($client_socket);

	$request->setStatusDone();
}

################################################################################
# Subscription management
################################################################################

# subscribe hash:
# .. listen:      * to get everything
#                 array ref containing array ref containing list of cmds to get
#                 (in Request->isCommand form, i.e. [['cmd1', 'cmd2', 'cmd3']])


# cancels all subscriptions
sub cli_subscribe_terms_none {
	my $client_socket = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	delete $connections{$client_socket}{'subscribe'}{'listen'};

	cli_subscribe_manage();
}

# monitor all things happening on server
sub cli_subscribe_terms_all {
	my $client_socket = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	$connections{$client_socket}{'subscribe'}{'listen'} = '*';

	cli_subscribe_manage();
}

# monitor only certain commands
sub cli_subscribe_terms {
	my $client_socket = shift;
	my $array_ref = shift;

	main::DEBUGLOG && $log->debug("Begin Function");

	$connections{$client_socket}{'subscribe'}{'listen'} = [$array_ref];

	cli_subscribe_manage();
}


# subscribes or unsubscribes to the Request notification system
sub cli_subscribe_manage {

	main::DEBUGLOG && $log->debug("Begin Function");

	# do we need to subscribe?
	my $subscribe = 0;
	foreach my $client_socket (keys %connections) {

		if (keys(%{$connections{$client_socket}{'subscribe'}})) {

			$subscribe++;
			last;
		}
	}

	# subscribe
	if ($subscribe && !$cli_subscribed) {

		Slim::Control::Request::subscribe(\&cli_subscribe_notification);
		$cli_subscribed = 1;

		# force request objects to always order results so we can send on the cli
		Slim::Control::Request::alwaysOrder(1);

	# unsubscribe
	} elsif (!$subscribe && $cli_subscribed) {

		Slim::Control::Request::unsubscribe(\&cli_subscribe_notification);
		$cli_subscribed = 0;

		# turn off ordering as it is expensive and we have no cli listeners
		Slim::Control::Request::alwaysOrder(0);
	}
}

# handles notifications
sub cli_subscribe_notification {
	my $request = shift;

	if ( main::INFOLOG && $log->is_info ) {
		$log->info($request->getRequestString);
	}

	# iterate over each connection, we have a single notification handler
	# for all connections
	foreach my $key (keys %connections) {

		# don't send if unsubscribed or unauthenticated
		next if !defined($connections{$key}{'subscribe'});
		next if !$connections{$key}{'auth'};

		# retrieve the socket object
		my $client_socket = $connections{$key}{'socket'};
		next unless defined $client_socket && exists $connections{$client_socket};

		# remember & decide if we send the echo
		my $sent = 0;

		# handle sending unique commands
		if (defined $connections{$client_socket}{'subscribe'}{'listen'}) {

			# don't echo twice to the sender
			if (!($request->source() && $request->source() eq 'CLI' &&
				  $request->connectionID() eq $client_socket)) {

				# assume no array in {'listen'}: we send everything
				$sent = 1;

				# if we have an array in {'listen'}...
				if (ref $connections{$client_socket}{'subscribe'}{'listen'}
					eq 'ARRAY') {

					# check the command matches the list of wanted commands
					$sent = $request->isCommand($connections{$client_socket}{'subscribe'}{'listen'});
				}

				# send if needed
				if ($sent) {

					# write request
					cli_request_write($request, $client_socket);
				}
			}
		}
	}
}

1;
