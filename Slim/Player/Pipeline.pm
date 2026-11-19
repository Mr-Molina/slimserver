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
				return undef;
			};

			$writerPort = $listenWriter->sockport;
		}

		$command =~ s/(\\*)"/$1$1\\"/g;
		$command =~ s/(\\+)$/$1$1/;

		my $newcommand = '"' . Slim::Utils::Misc::findbin('socketwrapper') .  '" ';

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

		Slim::bootstrap::tryModuleLoad('Win32::Process');

		if ($@ || !Win32::Process::Create(
			$processObj,
			Slim::Utils::Misc::findbin("socketwrapper"),
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
		$reader = IO::Handle->new();
		$writer = IO::Handle->new();

		my @cmd = _splitSimpleCommand($command);
		my $pid = (@cmd > 1 || (@cmd == 1 && $cmd[0] !~ /[\s"'\\\$`|&;<>(){}*?\[\]~#]/))
			? open2($reader, $writer, @cmd)
			: open2($reader, $writer, $command);
		${*$self}{'pipeline_pid'} = $pid;

		if (!defined(Slim::Utils::Network::blocking($reader, 0))) {

			logError("Cannot set pipe line reader to nonblocking");

			_reapChild(delete ${*$self}{'pipeline_pid'});

			$reader->close();
			$writer->close();

			return undef;
		}

		if (!defined(Slim::Utils::Network::blocking($writer, 0))) {

			logError("Cannot set pipe line writer to nonblocking");

			_reapChild(delete ${*$self}{'pipeline_pid'});

			$reader->close();
			$writer->close();

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

sub acceptReader {
	if (main::ISWINDOWS) {
		my $listener = shift;
		my $pipeline = ${*$listener}{'pipeline'};

		my $reader = $listener->accept();

		if (!defined($reader)) {

			logError("Accepting on reader listener: $!");

			${*$pipeline}{'pipeline_error'} = 1;

			return;
		}

		if (!defined(Slim::Utils::Network::blocking($reader, 0))) {

			logError("Cannot set pipe line reader to nonblocking");

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

		my $writer = $listener->accept();

		if (!defined($writer)) {

			logError("Accepting on writer listener: $!");

			${*$pipeline}{'pipeline_error'} = 1;

			return;
		}

		if (!defined(Slim::Utils::Network::blocking($writer, 0))) {

			logError("Cannot set pipe line writer to nonblocking");

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

	${*$pipeline}{'pipeline_error'} = 1;
}

sub sysread {
	my $self = $_[0];
	my $chunksize = $_[2];
	my $readlen;

	my $error = ${*$self}{'pipeline_error'};

	if ($error) {
		$! = -1;
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

			if ($! != EWOULDBLOCK) {
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

		while ($i < $len) {
			$ch = substr($command, $i, 1);

			if ($in_double) {
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
				elsif ($ch =~ /[\|\&\;\<\>\(\)\{\}\*\?\[\]\~\$\`\'\\\#\r\n]/) {
					return ();
				}
				else {
					$arg .= $ch;
					$i++;
				}
			}
		}

		return () if $in_double;
		push @args, $arg;
	}

	return @args;
}

sub _reapKilledChild {
	my ($pid, $retries) = @_;
	return if waitpid($pid, POSIX::WNOHANG()) != 0;
	return if (${^GLOBAL_PHASE} // '') eq 'DESTRUCT';
	return if $retries >= 10;

	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.05, sub {
		_reapKilledChild($pid, $retries + 1);
	});
}

sub _reapChild {
	my $pid = shift;
	return unless defined $pid;
	return if waitpid($pid, POSIX::WNOHANG()) != 0;

	kill('TERM', $pid);
	return if waitpid($pid, POSIX::WNOHANG()) != 0;

	if ((${^GLOBAL_PHASE} // '') eq 'DESTRUCT') {
		kill('KILL', $pid);
		waitpid($pid, 0);
		return;
	}

	Slim::Utils::Timers::setTimer(undef, Time::HiRes::time() + 0.05, sub {
		return if waitpid($pid, POSIX::WNOHANG()) != 0;

		kill('KILL', $pid);
		return if waitpid($pid, POSIX::WNOHANG()) != 0;

		_reapKilledChild($pid, 0);
	});
}

sub close {
	my $self = shift;
	my $inDestruct = (${^GLOBAL_PHASE} // '') eq 'DESTRUCT';

	my $reader = ${*$self}{'pipeline_reader'};

	if (defined($reader)) {
		if (!$inDestruct) {
			Slim::Networking::Select::removeRead($reader);
			Slim::Networking::Select::removeError($reader);
		}
		$reader->close();
	}

	my $writer = ${*$self}{'pipeline_writer'};

	if (defined($writer)) {
		if (!$inDestruct) {
			Slim::Networking::Select::removeWrite($writer);
		}
		$writer->close();
	}

	if (main::ISWINDOWS) {

		my $listenReader = ${*$self}{'pipeline_listen_reader'};

		if (defined($listenReader)) {

			if (!$inDestruct) {
				Slim::Networking::Select::removeRead($listenReader);
				Slim::Networking::Select::removeError($listenReader);
			}

			${*$listenReader}{'pipeline'} = undef;

			$listenReader->close();
		}

		my $listenWriter = ${*$self}{'pipeline_listen_writer'};

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
		my $pid = delete ${*$self}{'pipeline_pid'};
		_reapChild($pid) if defined $pid;
	}

	my $source = ${*$self}{'pipeline_source'};

	if (defined($source) && !$inDestruct) {
		$source->close();
	}
}

sub DESTROY {
	my $self = shift;

	if ((${^GLOBAL_PHASE} // '') eq 'DESTRUCT') {
		my $pid = delete ${*$self}{'pipeline_pid'};
		_reapChild($pid) if defined $pid;
		return;
	}

	$self->close();
}

1;

__END__
