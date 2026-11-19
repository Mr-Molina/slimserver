-- Add new Rust-Experiment indexes for existing databases and drop redundant leftmost-prefix indexes

CREATE INDEX IF NOT EXISTS tracks_album_disc_tracknum ON tracks (album, disc, tracknum);
CREATE INDEX IF NOT EXISTS playlist_track_pos ON playlist_track (playlist, position, track);
CREATE INDEX IF NOT EXISTS albumsContributorIndex ON albums (contributor);
CREATE INDEX IF NOT EXISTS contributor_track_track_role_contrib ON contributor_track (track, role, contributor);
CREATE INDEX IF NOT EXISTS contributor_album_album_role ON contributor_album (album, role, contributor);
CREATE INDEX IF NOT EXISTS genre_track_track_genre ON genre_track (track, genre);

DROP INDEX IF EXISTS trackAlbumIndex;
DROP INDEX IF EXISTS playlistIndex;
DROP INDEX IF EXISTS contributor_trackTrackIndex;
DROP INDEX IF EXISTS contributor_trackRoleIndex;
DROP INDEX IF EXISTS contributor_albumAlbumIndex;
DROP INDEX IF EXISTS contributor_albumRoleIndex;
DROP INDEX IF EXISTS genre_trackGenreIndex;
DROP INDEX IF EXISTS genre_trackTrackIndex;
