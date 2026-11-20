package Slim::Player::Pipeline;


# Logitech Media Server Copyright 2001-2024 Logitech.
# Lyrion Music Server Copyright 2024 Lyrion Community.
# This program is free software; you can redistribute it and/or
# modify it under the terms of the GNU General Public License,
# version 2.

use strict;

use base qw(IO::Handle);
use bytes;

use IPC::Open2;
use IO::Handle;
use POSIX qw(:sys_wait_h);

use Slim::Utils::Errno;
use Slim::Utils::Log;
use Slim::Utils::Misc;
use Slim::Utils::Network;
use Slim::Utils::OSDetect;
use Slim::Utils::Timers ();
use Time::HiRes ();

my $log = logger('player.source');

sub new {
	my $class   = shift;
	my $source  = shift;
	my $command = shift;
	my $local = shift;  # flag to indicate that pipeline is used for transcoding of local files and pipeline should not close on pause

	my $self = $class->SUPER::new();
	my ($reader, $writer);

	if (main::ISWINDOWS) {

		my $listenReader = IO::Socket::INET->new(

			LocalAddr => 'localhost',
			Listen    => 5

		) || do {

			logError("Couldn't create listen socket for reader: $!");
			${*$self}{'pipeline_closed'} = 1;
			return undef;
		};

		my $readerPort = $listenReader->sockport;

		my ($listenWriter, $writerPort);

		if ($source) {

			$listenWriter = IO::Socket::INET->new(

				LocalAddr => 'localhost',
				Listen    => 5

			) || do {

				logError("Couldn't create listen socket for writer: $!");
				$listenReader->close();
				${*$self}{'pipeline_closed'} = 1;
				return undef;
			};

			$writerPort = $listenWriter->sockport;
		}

		$command =~ s/(\\*)"/$1$1\\"/g;
		$command =~ s/(\\+)$/$1$1/;

		my $socketwrapper = Slim::Utils::Misc::findbin('socketwrapper');
		if (!$socketwrapper) {
			logError("Couldn't find socketwrapper binary");
			$listenReader->close();
			$listenWriter->close() if $listenWriter;
			${*$self}{'pipeline_closed'} = 1;
			return undef;
		}

		Slim::bootstrap::tryModuleLoad('Win32::Process');
		if ($@) {
			logError("Couldn't load Win32::Process: $@");
			$listenReader->close();
			$listenWriter->close() if $listenWriter;
			${*$self}{'pipeline_closed'} = 1;
			return undef;
		}

		my $newcommand = '"' . $socketwrapper . '" ';

		# Bug 15650, Run transcoders with the same priority as the server
		# XXX this sets the priority of the socketwrapper.exe process but not the actual transcoder process(es).
		my $priority = Win32::Process::NORMAL_PRIORITY_CLASS();

		my $createMode = $priority | Win32::Process::CREATE_NO_WINDOW();

		if ($log->is_info || $log->is_debug) {

			$newcommand .= $log->is_debug ? ' -D ' : ' -d ';       # socketwrapper debugging (-D = verbose)

			$createMode = $priority | Win32::Process::CREATE_NEW_CONSOLE(); # create window so it is seen
		}

		if ($listenWriter) {
			$newcommand .= '-i ' . $writerPort . ' ';
		}

		if (!$local) {
			$newcommand .= ' -w ';                                 # enable checking of stream in socketwrapper
		}

		$newcommand .=  '-o ' . $readerPort . ' -c "' .  $command . '"';

		main::INFOLOG && $log->info("Launching process with command: $newcommand");

		my $processObj;

		if (!Win32::Process::Create(
			$processObj,
			$socketwrapper,
			$newcommand,
			0,
			$createMode,
			".")
		) {

			logError("Couldn't create socketwrapper process");

			$listenReader->close();

			if ($listenWriter) {
				$listenWriter->close();
			}

			${*$self}{'pipeline_closed'} = 1;
			return undef;
		}

		${*$listenReader}{'pipeline'} = $self;
		${*$self}{'pipeline_listen_reader'} = $listenReader;
		Slim::Networking::Select::addRead($listenReader, \&acceptReader);
		Slim::Networking::Select::addError($listenReader, \&selectError);

		if ($listenWriter) {
			${*$listenWriter}{'pipeline'} = $self;
			${*$self}{'pipeline_listen_writer'} = $listenWriter;
			Slim::Networking::Select::addRead($listenWriter, \&acceptWriter);
			Slim::Networking::Select::addError($listenWriter, \&selectError);
		}
	}
	else {
		my ($pids, $pgid);
		($reader, $writer, $pids, $pgid) = _spawnUnixPipeline($command);
		if (!defined($reader)) {
			${*$self}{'pipeline_closed'} = 1;
			return undef;
		}

		${*$self}{'pipeline_pids'} = $pids;
		${*$self}{'pipeline_pgid'} = $pgid;
		${*$self}{'pipeline_pid'}  = $pids->[-1];

		if (!defined(Slim::Utils::Network::blocking($reader, 0))) {

			logError("Cannot set pipe line reader to nonblocking");

			_reapChild(delete ${*$self}{'pipeline_pids'}, delete ${*$self}{'pipeline_pgid'});
			delete ${*$self}{'pipeline_pid'};

			$reader->close();
			$writer->close();
			${*$self}{'pipeline_closed'} = 1;

			return undef;
		}

		if (!defined(Slim::Utils::Network::blocking($writer, 0))) {

			logError("Cannot set pipe line writer to nonblocking");

			_reapChild(delete ${*$self}{'pipeline_pids'}, delete ${*$self}{'pipeline_pgid'});
			delete ${*$self}{'pipeline_pid'};

			$reader->close();
			$writer->close();
			${*$self}{'pipeline_closed'} = 1;

			return undef;
		}

		binmode($reader);
		binmode($writer);
	}

	if (defined($source)) {
		binmode($source);
	}

	${*$self}{'pipeline_reader'} = $reader;
	${*$self}{'pipeline_writer'} = $writer;
	${*$self}{'pipeline_source'} = $source;
	${*$self}{'pipeline_pending_bytes'} = '';
	${*$self}{'pipeline_pending_size'} = 0;
	${*$self}{'pipeline_pending_offset'} = 0;
	${*$self}{'pipeline_error'} = 0;

	return $self;
}

sub _closeListener {
	my ($listener, $pipeline, $key) = @_;

	if (defined $listener) {
		Slim::Networking::Select::removeRead($listener);
		Slim::Networking::Select::removeError($listener);
		${*$listener}{'pipeline'} = undef;
		$listener->close();
	}

	if (defined $pipeline && defined $key) {
		delete ${*$pipeline}{$key};
	}
}

sub acceptReader {
	if (main::ISWINDOWS) {
		my $listener = shift;
		my $pipeline = ${*$listener}{'pipeline'};

		if (!defined($pipeline)) {
			_closeListener($listener);
			return;
		}

		my $reader = $listener->accept();
		_closeListener($listener, $pipeline, 'pipeline_listen_reader');

		if (!defined($reader)) {

			logError("Accepting on reader listener: $!");

			${*$pipeline}{'pipeline_error'} = 1;

			return;
		}

		if (!defined(Slim::Utils::Network::blocking($reader, 0))) {

			logError("Cannot set pipe line reader to nonblocking");

			$reader->close();
			${*$pipeline}{'pipeline_error'} = 1;

			return;
		}

		main::INFOLOG && $log->info("Pipeline reader connected");

		binmode($reader);

		${*$pipeline}{'pipeline_reader'} = $reader;
	}
}

sub acceptWriter {
	if (main::ISWINDOWS) {
		my $listener = shift;
		my $pipeline = ${*$listener}{'pipeline'};

		if (!defined($pipeline)) {
			_closeListener($listener);
			return;
		}

		my $writer = $listener->accept();
		_closeListener($listener, $pipeline, 'pipeline_listen_writer');

		if (!defined($writer)) {

			logError("Accepting on writer listener: $!");

			${*$pipeline}{'pipeline_error'} = 1;

			return;
		}

		if (!defined(Slim::Utils::Network::blocking($writer, 0))) {

			logError("Cannot set pipe line writer to nonblocking");

			$writer->close();
			${*$pipeline}{'pipeline_error'} = 1;

			return;
		}

		main::INFOLOG && $log->info("Pipeline writer connected");

		binmode($writer);

		${*$pipeline}{'pipeline_writer'} = $writer;
	}
}

sub selectError {
	my $listener = shift;
	my $pipeline = ${*$listener}{'pipeline'};

	logError("From select on pipeline listeners.");

	if (defined $pipeline) {
		my $key;
		if (defined ${*$pipeline}{'pipeline_listen_reader'} && ${*$pipeline}{'pipeline_listen_reader'} == $listener) {
			$key = 'pipeline_listen_reader';
		}
		elsif (defined ${*$pipeline}{'pipeline_listen_writer'} && ${*$pipeline}{'pipeline_listen_writer'} == $listener) {
			$key = 'pipeline_listen_writer';
		}
		_closeListener($listener, $pipeline, $key);
		${*$pipeline}{'pipeline_error'} = 1;
	}
	else {
		_closeListener($listener);
	}
}

sub sysread {
	my $self = $_[0];
	my $chunksize = $_[2];
	my $readlen;

	my $error = ${*$self}{'pipeline_error'};

	if ($error) {
		$! = EBADF;
		return undef;
	}

	my $reader = ${*$self}{'pipeline_reader'};
	my $writer = ${*$self}{'pipeline_writer'};
	my $source = ${*$self}{'pipeline_source'};

	unless (defined($reader) && (!defined($source) || defined($writer))) {
		$! = EWOULDBLOCK;
		return undef;
	}

	# First try to stuff the pipe
	my $chunks = 0;
	while (defined($source) && ++$chunks < 4) {

		my $pendingBytes  = ${*$self}{'pipeline_pending_bytes'};
		my $pendingSize   = ${*$self}{'pipeline_pending_size'};
		my $pendingOffset = ${*$self}{'pipeline_pending_offset'} || 0;

		if (!$pendingSize) {

			main::DEBUGLOG && $log->debug("Pipeline doesn't have pending bytes - trying to get some from source");

			my $socketReadlen = $source->sysread($pendingBytes, $chunksize);

			if (!$socketReadlen) {
				if (defined $socketReadlen) {
					# EOF
					main::INFOLOG && $log->info("EOF on source stream");
					$source->close();
					Slim::Networking::Select::removeWrite($writer);
					$writer->close();
					delete ${*$self}{'pipeline_source'};
					delete ${*$self}{'pipeline_writer'};
					last;
				} elsif ($! == EWOULDBLOCK || $! == EINTR) {
					last;
				} else {
					return undef; # reflect error to caller
				}
			}

			$pendingSize = $socketReadlen;
			$pendingOffset = 0;
		}

		main::DEBUGLOG && $log->debug("Attempting to write to pipeline writer");

		my $writelen = syswrite($writer, $pendingBytes, $pendingSize - $pendingOffset, $pendingOffset);

		if ($writelen) {

			main::DEBUGLOG && $log->debug("Wrote $writelen bytes to pipeline writer");

			$pendingOffset += $writelen;

			if ($pendingOffset < $pendingSize) {
				${*$self}{'pipeline_pending_bytes'}  = $pendingBytes;
				${*$self}{'pipeline_pending_size'}   = $pendingSize;
				${*$self}{'pipeline_pending_offset'} = $pendingOffset;
			}
			else {
				${*$self}{'pipeline_pending_bytes'}  = '';
				${*$self}{'pipeline_pending_size'}   = 0;
				${*$self}{'pipeline_pending_offset'} = 0;
			}
		}
		else {

			${*$self}{'pipeline_pending_bytes'}  = $pendingBytes;
			${*$self}{'pipeline_pending_size'}   = $pendingSize;
			${*$self}{'pipeline_pending_offset'} = $pendingOffset;

			if ($! != EWOULDBLOCK && $! != EINTR) {
				return undef;	# reflect error to caller
			}

			last;
		}

	}

	return $reader->sysread($_[1], $chunksize);
}

sub sysseek {
	my $self = shift;

	return 0;
}

sub _splitSimpleCommand {
	my $command = shift;
	return () unless defined $command;

	my @args;
	my $len = length($command);
	my $i = 0;

	while ($i < $len) {
		my $ch = substr($command, $i, 1);
		if ($ch eq ' ' || $ch eq "\t") {
			$i++;
			next;
		}

		my $arg = '';
		my $in_double = 0;
		my $in_single = 0;

		while ($i < $len) {
			$ch = substr($command, $i, 1);

			if ($in_single) {
				if ($ch eq "'") {
					$in_single = 0;
					$i++;
				}
				else {
					$arg .= $ch;
					$i++;
				}
			}
			elsif ($in_double) {
				if ($ch eq '"') {
					$in_double = 0;
					$i++;
				}
				elsif ($ch eq '\\') {
					return () if $i + 1 >= $len;
					my $next = substr($command, $i + 1, 1);
					if ($next eq '"' || $next eq '\\' || $next eq '$' || $next eq '`') {
						$arg .= $next;
					}
					elsif ($next eq "\n") {
						return ();
					}
					else {
						$arg .= '\\' . $next;
					}
					$i += 2;
				}
				elsif ($ch eq '$' || $ch eq '`' || $ch eq "\n") {
					return ();
				}
				else {
					$arg .= $ch;
					$i++;
				}
			}
			else {
				if ($ch eq ' ' || $ch eq "\t") {
					last;
				}
				elsif ($ch eq '"') {
					$in_double = 1;
					$i++;
				}
				elsif ($ch eq "'") {
					$in_single = 1;
					$i++;
				}
				elsif ($ch eq '\\') {
					return () if $i + 1 >= $len;
					my $next = substr($command, $i + 1, 1);
					return () if $next eq "\n" || $next eq "\r";
					$arg .= $next;
					$i += 2;
				}
				elsif ($ch =~ /[\|\&\;\<\>\(\)\{\}\*\?\[\]\~\$\`\#\r\n]/) {
					return ();
				}
				else {
					$arg .= $ch;
					$i++;
				}
			}
		}

		return () if $in_double || $in_single;
		push @args, $arg;
	}

	return @args;
}

sub _splitPipelineStages {
	my $command = shift;
	return () unless defined $command;

	my @rawStages;
	my $len = length($command);
	my $start = 0;
	my $in_double = 0;
	my $in_single = 0;
	my $i = 0;

	while ($i < $len) {
		my $ch = substr($command, $i, 1);

		if ($in_single) {
			$in_single = 0 if $ch eq "'";
			$i++;
		}
		elsif ($in_double) {
			if ($ch eq '\\') {
				return () if $i + 1 >= $len;
				$i += 2;
			}
			elsif ($ch eq '"') {
				$in_double = 0;
				$i++;
			}
			else {
				$i++;
			}
		}
		else {
			if ($ch eq '\\') {
				return () if $i + 1 >= $len;
				$i += 2;
			}
			elsif ($ch eq '"') {
				$in_double = 1;
				$i++;
			}
			elsif ($ch eq "'") {
				$in_single = 1;
				$i++;
			}
			elsif ($ch eq '|') {
				my $next = ($i + 1 < $len) ? substr($command, $i + 1, 1) : '';
				return () if $next eq '|' || $next eq '&';
				push @rawStages, substr($command, $start, $i - $start);
				$i++;
				$start = $i;
			}
			else {
				$i++;
			}
		}
	}

	return () if $in_double || $in_single;
	push @rawStages, substr($command, $start);

	my @stages;
	for my $stage (@rawStages) {
		my @args = _splitSimpleCommand($stage);
		return () unless @args;
		push @stages, \@args;
	}

	return @stages;
}

sub _spawnUnixPipeline {
	my $command = shift;
	return () unless defined $command;

	my $reader_r = IO::Handle->new();
	my $reader_w = IO::Handle->new();
	my $writer_r = IO::Handle->new();
	my $writer_w = IO::Handle->new();

	if (!pipe($reader_r, $reader_w) || !pipe($writer_r, $writer_w)) {
		logError("Couldn't create pipeline pipes: $!");
		$reader_r->close() if defined fileno($reader_r);
		$reader_w->close() if defined fileno($reader_w);
		$writer_r->close() if defined fileno($writer_r);
		$writer_w->close() if defined fileno($writer_w);
		return ();
	}

	my @stages = _splitPipelineStages($command);
	my @pids;
	my $pgid = 0;

	if (@stages) {
		my $prev_r;
		for my $idx (0 .. $#stages) {
			my $stage_in  = ($idx == 0) ? $writer_r : $prev_r;
			my ($next_r, $next_w);

			if ($idx < $#stages) {
				$next_r = IO::Handle->new();
				$next_w = IO::Handle->new();
				if (!pipe($next_r, $next_w)) {
					logError("Couldn't create inter-stage pipe: $!");
					$reader_r->close() if defined fileno($reader_r);
					$reader_w->close() if defined fileno($reader_w);
					$writer_r->close() if defined fileno($writer_r);
					$writer_w->close() if defined fileno($writer_w);
					$prev_r->close()   if defined $prev_r && defined fileno($prev_r);
					$next_r->close()   if defined fileno($next_r);
					$next_w->close()   if defined fileno($next_w);
					_reapChild(\@pids, $pgid) if @pids;
					return ();
				}
			}

			my $stage_out = ($idx == $#stages) ? $reader_w : $next_w;
			my $pid = fork();

			if (!defined($pid)) {
				logError("Couldn't fork pipeline stage: $!");
				$reader_r->close() if defined fileno($reader_r);
				$reader_w->close() if defined fileno($reader_w);
				$writer_r->close() if defined fileno($writer_r);
				$writer_w->close() if defined fileno($writer_w);
				$prev_r->close()   if defined $prev_r && defined fileno($prev_r);
				$next_r->close()   if defined $next_r && defined fileno($next_r);
				$next_w->close()   if defined $next_w && defined fileno($next_w);
				_reapChild(\@pids, $pgid) if @pids;
				return ();
			}

			if ($pid == 0) {
				eval { POSIX::setpgid(0, $pgid); };
				open(STDIN,  '<&', $stage_in)  or POSIX::_exit(127);
				open(STDOUT, '>&', $stage_out) or POSIX::_exit(127);

				$reader_r->close() if defined fileno($reader_r);
				$reader_w->close() if defined fileno($reader_w);
				$writer_r->close() if defined fileno($writer_r);
				$writer_w->close() if defined fileno($writer_w);
				$prev_r->close()   if defined $prev_r && defined fileno($prev_r);
				$next_r->close()   if defined $next_r && defined fileno($next_r);
				$next_w->close()   if defined $next_w && defined fileno($next_w);

				my @args = @{ $stages[$idx] };
				{ exec { $args[0] } @args; }
				POSIX::_exit(127);
			}

			$pgid ||= $pid;
			eval { POSIX::setpgid($pid, $pgid); };
			push @pids, $pid;

			$stage_in->close()  if defined fileno($stage_in);
			$stage_out->close() if defined fileno($stage_out);
			$prev_r = $next_r;
		}
	}
	else {
		my $pid = fork();
		if (!defined($pid)) {
			logError("Couldn't fork shell pipeline: $!");
			$reader_r->close() if defined fileno($reader_r);
			$reader_w->close() if defined fileno($reader_w);
			$writer_r->close() if defined fileno($writer_r);
			$writer_w->close() if defined fileno($writer_w);
			return ();
		}

		if ($pid == 0) {
			eval { POSIX::setpgid(0, 0); };
			open(STDIN,  '<&', $writer_r) or POSIX::_exit(127);
			open(STDOUT, '>&', $reader_w) or POSIX::_exit(127);

			$reader_r->close() if defined fileno($reader_r);
			$reader_w->close() if defined fileno($reader_w);
			$writer_r->close() if defined fileno($writer_r);
			$writer_w->close() if defined fileno($writer_w);

			{ exec('/bin/sh', '-c', $command); }
			POSIX::_exit(127);
		}

		$pgid = $pid;
		eval { POSIX::setpgid($pid, $pgid); };
		@pids = ($pid);
		$writer_r->close() if defined fileno($writer_r);
		$reader_w->close() if defined fileno($reader_w);
	}

	return ($reader_r, $writer_w, \@pids, $pgid);
}

sub _reapKilledChild {
	my ($pidsOrPid, $retries, $pgid) = @_;
	my @pids = ref($pidsOrPid) eq 'ARRAY' ? @$pidsOrPid : (defined $pidsOrPid ? ($pidsOrPid) : ());
	$retries ||= 0;

	my @remaining = grep { waitpid($_, POSIX::WNOHANG()) == 0 } @pids;
	return unless @remaining;

	if ((${^GLOBAL_PHASE} // '') eq 'DESTRUCT') {
		kill('KILL', -$pgid) if defined $pgid && $pgid > 0;
		kill('KILL', @remaining);
		waitpid($_, 0) for @remaining;
		return;
	}

	# Re-assert SIGKILL periodically if a child lingers beyond 0.5s, never abandon zombies
	if ($retries >= 10 && ($retries % 5) == 0) {
		kill('KILL', -$pgid) if defined $pgid && $pgid > 0;
		kill('KILL', @remaining);
		@remaining = grep { waitpid($_, POSIX::WNOHANG()) == 0 } @remaining;
		return unless @remaining;
	}

	my $delay = $retries < 10 ? 0.05 : ($retries < 20 ? 0.25 : 1.0);
	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + $delay, sub {
		_reapKilledChild(\@remaining, $retries + 1, $pgid);
	});
}

sub _reapChild {
	my ($pidsOrPid, $pgid) = @_;
	my @pids = ref($pidsOrPid) eq 'ARRAY' ? @$pidsOrPid : (defined $pidsOrPid ? ($pidsOrPid) : ());
	return unless @pids || (defined $pgid && $pgid > 0);

	my $pgidAlive = (defined $pgid && $pgid > 0 && kill(0, -$pgid)) ? 1 : 0;
	my @remaining = grep { waitpid($_, POSIX::WNOHANG()) == 0 } @pids;
	return unless @remaining || $pgidAlive;

	kill('TERM', -$pgid) if defined $pgid && $pgid > 0;
	kill('TERM', @remaining) if @remaining;

	@remaining = grep { waitpid($_, POSIX::WNOHANG()) == 0 } @remaining;
	$pgidAlive = (defined $pgid && $pgid > 0 && kill(0, -$pgid)) ? 1 : 0;
	return unless @remaining || $pgidAlive;

	if ((${^GLOBAL_PHASE} // '') eq 'DESTRUCT') {
		kill('KILL', -$pgid) if defined $pgid && $pgid > 0;
		kill('KILL', @remaining) if @remaining;
		waitpid($_, 0) for @remaining;
		return;
	}

	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.05, sub {
		@remaining = grep { waitpid($_, POSIX::WNOHANG()) == 0 } @remaining;
		my $stillGroup = (defined $pgid && $pgid > 0 && kill(0, -$pgid)) ? 1 : 0;
		return unless @remaining || $stillGroup;

		kill('KILL', -$pgid) if defined $pgid && $pgid > 0;
		kill('KILL', @remaining) if @remaining;

		@remaining = grep { waitpid($_, POSIX::WNOHANG()) == 0 } @remaining;
		return unless @remaining;

		_reapKilledChild(\@remaining, 0, $pgid);
	});
}

sub close {
	my $self = shift;
	return if ${*$self}{'pipeline_closed'};
	${*$self}{'pipeline_closed'} = 1;

	my $inDestruct = (${^GLOBAL_PHASE} // '') eq 'DESTRUCT';

	my $reader = delete ${*$self}{'pipeline_reader'};

	if (defined($reader)) {
		if (!$inDestruct) {
			Slim::Networking::Select::removeRead($reader);
			Slim::Networking::Select::removeError($reader);
		}
		$reader->close();
	}

	my $writer = delete ${*$self}{'pipeline_writer'};

	if (defined($writer)) {
		if (!$inDestruct) {
			Slim::Networking::Select::removeWrite($writer);
		}
		$writer->close();
	}

	if (main::ISWINDOWS) {

		my $listenReader = delete ${*$self}{'pipeline_listen_reader'};

		if (defined($listenReader)) {

			if (!$inDestruct) {
				Slim::Networking::Select::removeRead($listenReader);
				Slim::Networking::Select::removeError($listenReader);
			}

			${*$listenReader}{'pipeline'} = undef;

			$listenReader->close();
		}

		my $listenWriter = delete ${*$self}{'pipeline_listen_writer'};

		if (defined($listenWriter)) {

			if (!$inDestruct) {
				Slim::Networking::Select::removeRead($listenWriter);
				Slim::Networking::Select::removeError($listenWriter);
			}

			${*$listenWriter}{'pipeline'} = undef;

			$listenWriter->close();
		}
	}
	else {
		my $pids = delete ${*$self}{'pipeline_pids'};
		my $pgid = delete ${*$self}{'pipeline_pgid'};
		my $pid  = delete ${*$self}{'pipeline_pid'};
		_reapChild($pids || $pid, $pgid) if $pids || defined $pid || (defined $pgid && $pgid > 0);
	}

	my $source = delete ${*$self}{'pipeline_source'};

	if (defined($source) && !$inDestruct) {
		$source->close();
	}
}

sub DESTROY {
	my $self = shift;
	return if ${*$self}{'pipeline_closed'};

	if ((${^GLOBAL_PHASE} // '') eq 'DESTRUCT') {
		${*$self}{'pipeline_closed'} = 1;
		my $pids = delete ${*$self}{'pipeline_pids'};
		my $pgid = delete ${*$self}{'pipeline_pgid'};
		my $pid  = delete ${*$self}{'pipeline_pid'};
		_reapChild($pids || $pid, $pgid) if $pids || defined $pid || (defined $pgid && $pgid > 0);
		return;
	}

	$self->close();
}

1;

__END__
