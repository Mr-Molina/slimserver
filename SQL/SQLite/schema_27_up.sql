-- Add new Rust-Experiment indexes for existing databases and drop redundant leftmost-prefix indexes

BEGIN TRANSACTION;

CREATE INDEX IF NOT EXISTS tracks_album_disc_tracknum ON tracks (album, disc, tracknum);
CREATE INDEX IF NOT EXISTS playlist_track_pos ON playlist_track (playlist, position, track);
CREATE INDEX IF NOT EXISTS albumsContributorIndex ON albums (contributor);
CREATE INDEX IF NOT EXISTS contributor_track_track_role_contrib ON contributor_track (track, role, contributor);
CREATE INDEX IF NOT EXISTS contributor_album_album_role ON contributor_album (album, role, contributor);
CREATE INDEX IF NOT EXISTS genre_track_track_genre ON genre_track (track, genre);

CREATE INDEX IF NOT EXISTS trackYearIndex ON tracks (year);
CREATE INDEX IF NOT EXISTS trackMusicBrainzIndex ON tracks (musicbrainz_id);
CREATE INDEX IF NOT EXISTS albumsMusicBrainzIndex ON albums (musicbrainz_id);
CREATE INDEX IF NOT EXISTS contributorsMusicBrainzIndex ON contributors (musicbrainz_id);
CREATE INDEX IF NOT EXISTS commentsValueIndex ON comments (value);

CREATE INDEX IF NOT EXISTS trackSearchNocaseIndex ON tracks (titlesearch COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS trackSortNocaseIndex ON tracks (titlesort COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS albumsSearchNocaseIndex ON albums (titlesearch COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS albumsSortNocaseIndex ON albums (titlesort COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS contributorsSearchNocaseIndex ON contributors (namesearch COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS contributorsSortNocaseIndex ON contributors (namesort COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS genreSearchNocaseIndex ON genres (namesearch COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS genreSortNocaseIndex ON genres (namesort COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS commentsValueNocaseIndex ON comments (value COLLATE NOCASE);

DROP INDEX IF EXISTS trackAlbumIndex;
DROP INDEX IF EXISTS playlistIndex;
DROP INDEX IF EXISTS contributor_trackTrackIndex;
DROP INDEX IF EXISTS contributor_trackRoleIndex;
DROP INDEX IF EXISTS contributor_albumAlbumIndex;
DROP INDEX IF EXISTS contributor_albumRoleIndex;
DROP INDEX IF EXISTS genre_trackGenreIndex;
DROP INDEX IF EXISTS genre_trackTrackIndex;

COMMIT;
