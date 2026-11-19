-- Revert v27 indexes and restore pruned leftmost-prefix single-column indexes

DROP INDEX IF EXISTS tracks_album_disc_tracknum;
DROP INDEX IF EXISTS playlist_track_pos;
DROP INDEX IF EXISTS albumsContributorIndex;
DROP INDEX IF EXISTS contributor_track_track_role_contrib;
DROP INDEX IF EXISTS contributor_album_album_role;
DROP INDEX IF EXISTS genre_track_track_genre;

CREATE INDEX IF NOT EXISTS trackAlbumIndex ON tracks (album);
CREATE INDEX IF NOT EXISTS playlistIndex ON playlist_track (playlist);
CREATE INDEX IF NOT EXISTS contributor_trackTrackIndex ON contributor_track (track);
CREATE INDEX IF NOT EXISTS contributor_trackRoleIndex ON contributor_track (role);
CREATE INDEX IF NOT EXISTS contributor_albumAlbumIndex ON contributor_album (album);
CREATE INDEX IF NOT EXISTS contributor_albumRoleIndex ON contributor_album (role);
CREATE INDEX IF NOT EXISTS genre_trackGenreIndex ON genre_track (genre);
CREATE INDEX IF NOT EXISTS genre_trackTrackIndex ON genre_track (track);
