package Slim::Utils::SQLHelper;


=head1 NAME

Slim::Utils::SQLHelper

=head1 DESCRIPTION

Utility functions to handle reading of SQL files and executing them on the DB.

This may be replaced by DBIx::Class's deploy functionality.

=head1 METHODS

=head2 executeSQLFile( $driver, $dbh, $sqlFile )

Run the commands as specified in the sqlFile.

Valid commands are:

ANALYZE, ALTER, CREATE, USE, SET, INSERT, UPDATE, DELETE, DROP, SELECT,
OPTIMIZE, TRUNCATE, UNLOCK, START, COMMIT, PRAGMA

=head1 SEE ALSO

L<DBIx::Class>

L<DBIx::Migration>

=cut

use strict;
use File::Spec::Functions qw(catdir file_name_is_absolute);

use Slim::Utils::OSDetect;
use Slim::Utils::Log;

sub executeSQLFile {
	my $class  = shift;
	my $driver = shift;
	my $dbh    = shift;
	my $file   = shift;

	my $sqlFile = $file;

	if (!file_name_is_absolute($file)) {
		$sqlFile = catdir(scalar Slim::Utils::OSDetect::dirsFor('SQL'), $driver, $file);
	}

	main::INFOLOG && logger('database.sql')->info("Executing SQL file $sqlFile");

	open(my $fh, $sqlFile) or do {

		logError("Couldn't open file [$sqlFile] : $!");
		return 0;
	};

	my $statement   = '';
	my $inStatement = 0;
	my $success     = 1;

	for my $line (<$fh>) {
		chomp $line;

		# skip and strip comments & empty lines
		$line =~ s/\s*--.*?$//o;
		$line =~ s/^\s*//o;

		next if $line =~ /^--/;
		next if $line =~ /^\s*$/;

		if ($line =~ /^\s*(?:ANALYZE|ALTER|CREATE|USE|SET|INSERT|UPDATE|DELETE|DROP|SELECT|OPTIMIZE|TRUNCATE|UNLOCK|START|COMMIT|PRAGMA)\b/oi) {
			$inStatement = 1;
		}

		if ($line =~ /;/ && $inStatement) {

			$statement .= ($statement ne '' ? ' ' : '') . $line;

			main::INFOLOG && logger('database.sql')->info("Executing SQL: [$statement]");

			eval { $dbh->do($statement) };

			if ($@) {
				my $err = $@;
				logError("Couldn't execute SQL statement: [$statement] : [$err]");
				if ($dbh && !$dbh->{AutoCommit}) {
					close $fh;
					die $err;
				}
				$success = 0;
			}

			$statement   = '';
			$inStatement = 0;
			next;
		}

		$statement .= ($statement ne '' ? ' ' : '') . $line if $inStatement;
	}

	close $fh;

	return $success;
}

1;

__END__
