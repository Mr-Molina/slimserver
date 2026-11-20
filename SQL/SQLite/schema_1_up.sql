
BEGIN TRANSACTION;

-- Drop dependent child tables before parent tables to satisfy foreign key constraints
DROP TABLE IF EXISTS library_track;
DROP TABLE IF EXISTS library_album;
DROP TABLE IF EXISTS library_contributor;
DROP TABLE IF EXISTS library_genre;
DROP TABLE IF EXISTS comments;
DROP TABLE IF EXISTS genre_track;
DROP TABLE IF EXISTS contributor_album;
DROP TABLE IF EXISTS contributor_track;
DROP TABLE IF EXISTS playlist_track;
DROP TABLE IF EXISTS tracks;
DROP TABLE IF EXISTS works;
DROP TABLE IF EXISTS albums;
DROP TABLE IF EXISTS contributors;
DROP TABLE IF EXISTS genres;
DROP TABLE IF EXISTS years;
DROP TABLE IF EXISTS unreadable_tracks;
DROP TABLE IF EXISTS rescans;
DROP TABLE IF EXISTS pluginversion;

CREATE TABLE IF NOT EXISTS metainformation (
  name  varchar(255) NOT NULL DEFAULT '',
  value varchar(255) NOT NULL DEFAULT '',
  PRIMARY KEY (name)
);

--
-- Table: rescans
--
CREATE TABLE rescans (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  files_scanned int(10),
  files_to_scan int(10),
  start_time int(10),
  end_time int(10)
);

--
-- Table: unreadable_tracks
--
CREATE TABLE unreadable_tracks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  rescan int(10),
  url text NOT NULL COLLATE NOCASE,
  reason text NOT NULL
);
CREATE INDEX IF NOT EXISTS unreadableRescanIndex ON unreadable_tracks (rescan);
CREATE INDEX IF NOT EXISTS unreadableUrlIndex ON unreadable_tracks (url);

--
-- Table: years
--
CREATE TABLE years (
  id smallint(5),
  PRIMARY KEY (id)
);

--
-- Table: albums
--
CREATE TABLE albums (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title blob,
  titlesort text COLLATE NOCASE,
  titlesearch text COLLATE NOCASE,
  customsearch text COLLATE NOCASE,
  compilation bool,
  year  smallint(5),
  artwork char(8), -- pointer to a track coverid that contains artwork
  disc  tinyint(1),
  discc  tinyint(1),
  replay_gain float,
  replay_peak float,
  musicbrainz_id varchar(40) COLLATE NOCASE,	-- musicbrainz uuid (36 bytes of text)
  musicmagic_mixable bool,
  contributor int(10)
);
CREATE INDEX IF NOT EXISTS albumsTitleIndex ON albums (title);
CREATE INDEX IF NOT EXISTS albumsSortIndex ON albums (titlesort);
CREATE INDEX IF NOT EXISTS albumsSearchIndex ON albums (titlesearch);
CREATE INDEX IF NOT EXISTS albumsCustomSearchIndex ON albums (customsearch);
CREATE INDEX IF NOT EXISTS compilationSortIndex ON albums (compilation);
CREATE INDEX IF NOT EXISTS albumsYearIndex ON albums (year);
CREATE INDEX IF NOT EXISTS albumsDiscIndex ON albums (disc);
CREATE INDEX IF NOT EXISTS albumsDiscCountIndex ON albums (discc);
CREATE INDEX IF NOT EXISTS albumsArtworkIndex ON albums (artwork);
CREATE INDEX IF NOT EXISTS albumsContributorIndex ON albums (contributor);
CREATE INDEX IF NOT EXISTS albumsMusicBrainzIndex ON albums (musicbrainz_id);

--
-- Table: contributors
--
CREATE TABLE contributors (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name blob,
  namesort text COLLATE NOCASE,
  namesearch text COLLATE NOCASE,
  customsearch text COLLATE NOCASE,
  musicbrainz_id varchar(40) COLLATE NOCASE,	-- musicbrainz uuid (36 bytes of text)
  musicmagic_mixable bool
);
CREATE INDEX IF NOT EXISTS contributorsNameIndex ON contributors (name);
CREATE INDEX IF NOT EXISTS contributorsSortIndex ON contributors (namesort);
CREATE INDEX IF NOT EXISTS contributorsSearchIndex ON contributors (namesearch);
CREATE INDEX IF NOT EXISTS contributorsCustomSearchIndex ON contributors (customsearch);
CREATE INDEX IF NOT EXISTS contributorsMusicBrainzIndex ON contributors (musicbrainz_id);

--
-- Table: genres
--
CREATE TABLE genres (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name blob,
  namesort text COLLATE NOCASE,
  namesearch text COLLATE NOCASE,
  customsearch text COLLATE NOCASE,
  musicmagic_mixable bool
);
CREATE INDEX IF NOT EXISTS genreNameIndex ON genres (name);
CREATE INDEX IF NOT EXISTS genreSortIndex ON genres (namesort);
CREATE INDEX IF NOT EXISTS genreSearchIndex ON genres (namesearch);
CREATE INDEX IF NOT EXISTS genreCustomSearchIndex ON genres (customsearch);

--
-- Table: tracks
--
CREATE TABLE tracks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  url text NOT NULL COLLATE NOCASE,
  title blob,
  titlesort text COLLATE NOCASE,
  titlesearch text COLLATE NOCASE,
  customsearch text COLLATE NOCASE,
  album int(10),
  tracknum int(10),
  content_type varchar(255) COLLATE NOCASE,
  timestamp int(10),
  filesize int(10),
  audio_size int(10),
  audio_offset int(10),
  year smallint(5),
  secs float,
  cover blob,
  vbr_scale varchar(255),
  bitrate float,
  samplerate int(10),
  samplesize int(10),
  channels tinyint(1),
  block_alignment int(10),
  endian  bool,
  bpm smallint(5),
  tagversion varchar(255),
  drm bool,
  disc tinyint(1),
  audio bool,
  remote bool,
  lossless bool,
  lyrics text COLLATE NOCASE, -- needs to be text so that searches are case insensitive.
  musicbrainz_id varchar(40) COLLATE NOCASE,	-- musicbrainz uuid (36 bytes of text)
  musicmagic_mixable bool,
  replay_gain float,
  replay_peak float,
  extid varchar(64) COLLATE NOCASE,
  FOREIGN KEY (`album`) REFERENCES `albums` (`id`) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS trackTitleIndex ON tracks (title);
CREATE INDEX IF NOT EXISTS ctSortIndex ON tracks (content_type);
CREATE INDEX IF NOT EXISTS trackSortIndex ON tracks (titlesort);
CREATE INDEX IF NOT EXISTS trackSearchIndex ON tracks (titlesearch);
CREATE INDEX IF NOT EXISTS trackCustomSearchIndex ON tracks (customsearch);
CREATE INDEX IF NOT EXISTS trackBitrateIndex ON tracks (bitrate);
CREATE INDEX IF NOT EXISTS trackDiscIndex ON tracks (disc);
CREATE INDEX IF NOT EXISTS trackFilesizeIndex ON tracks (filesize);
CREATE INDEX IF NOT EXISTS trackTimestampIndex ON tracks (timestamp);
CREATE INDEX IF NOT EXISTS trackTracknumIndex ON tracks (tracknum);
CREATE INDEX IF NOT EXISTS trackYearIndex ON tracks (year);
CREATE INDEX IF NOT EXISTS trackAudioIndex ON tracks (audio);
CREATE INDEX IF NOT EXISTS trackLyricsIndex ON tracks (lyrics);
CREATE INDEX IF NOT EXISTS trackRemoteIndex ON tracks (remote);
CREATE INDEX IF NOT EXISTS trackLosslessIndex ON tracks (lossless);
CREATE INDEX IF NOT EXISTS urlIndex ON tracks (url);
CREATE INDEX IF NOT EXISTS trackExtId ON tracks (extid);
CREATE INDEX IF NOT EXISTS trackMusicBrainzIndex ON tracks (musicbrainz_id);
CREATE INDEX IF NOT EXISTS tracks_album_disc_tracknum ON tracks (album, disc, tracknum);

--
-- Table: playlist_track
--
CREATE TABLE playlist_track (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  position  int(10),
  playlist  int(10),
  track  int(10),
  FOREIGN KEY (`track`) REFERENCES `tracks` (`id`) ON DELETE CASCADE,
  FOREIGN KEY (`playlist`) REFERENCES `tracks` (`id`) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS trackIndex ON playlist_track (track);
CREATE INDEX IF NOT EXISTS positionIndex ON playlist_track (position);
CREATE INDEX IF NOT EXISTS playlist_track_pos ON playlist_track (playlist, position, track);

--
-- Table: contributor_track
--
CREATE TABLE contributor_track (
  role  int(10),
  contributor  int(10),
  track  int(10),
  PRIMARY KEY (role,contributor,track),
  FOREIGN KEY (`track`) REFERENCES `tracks` (`id`) ON DELETE CASCADE,
  FOREIGN KEY (`contributor`) REFERENCES `contributors` (`id`) ON DELETE CASCADE 
);
CREATE INDEX IF NOT EXISTS contributor_trackContribIndex ON contributor_track (contributor);
CREATE INDEX IF NOT EXISTS contributor_track_track_role_contrib ON contributor_track (track, role, contributor);

--
-- Table: contributor_album
--
CREATE TABLE contributor_album (
  role  int(10),
  contributor  int(10),
  album  int(10),
  PRIMARY KEY (role,contributor,album),
  FOREIGN KEY (`album`) REFERENCES `albums` (`id`) ON DELETE CASCADE,
  FOREIGN KEY (`contributor`) REFERENCES `contributors` (`id`) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS contributor_albumContribIndex ON contributor_album (contributor);
CREATE INDEX IF NOT EXISTS contributor_album_album_role ON contributor_album (album, role, contributor);

--
-- Table: genre_track
--
CREATE TABLE genre_track (
  genre  int(10),
  track  int(10),
  PRIMARY KEY (genre,track),
  FOREIGN KEY (`track`) REFERENCES `tracks` (`id`) ON DELETE CASCADE,
  FOREIGN KEY (`genre`) REFERENCES `genres` (`id`) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS genre_track_track_genre ON genre_track (track, genre);

--
-- Table: comments
--
CREATE TABLE comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  track  int(10),
  value text COLLATE NOCASE, -- needs to be text so that searches are case insensitive.
  FOREIGN KEY (`track`) REFERENCES `tracks` (`id`) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS comments_trackIndex ON comments (track);
CREATE INDEX IF NOT EXISTS commentsValueIndex ON comments (value);

--
-- Table: pluginversion
--
CREATE TABLE pluginversion (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name varchar(255) COLLATE NOCASE,
  version  int(10)
);
CREATE INDEX IF NOT EXISTS pluginversionNameIndex ON pluginversion (name);

COMMIT;
