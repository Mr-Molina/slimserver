-- schema_16 recreated tracks but forgot to recreate all the indicies

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
CREATE INDEX IF NOT EXISTS trackAudioIndex ON tracks (audio);
CREATE INDEX IF NOT EXISTS trackLyricsIndex ON tracks (lyrics);
CREATE INDEX IF NOT EXISTS trackRemoteIndex ON tracks (remote);
CREATE INDEX IF NOT EXISTS trackLosslessIndex ON tracks (lossless);
CREATE INDEX IF NOT EXISTS urlIndex ON tracks (url);
CREATE INDEX IF NOT EXISTS trackExtId ON tracks (extid);
CREATE INDEX IF NOT EXISTS urlmd5Index ON tracks (urlmd5);
CREATE INDEX IF NOT EXISTS coveridIndex ON tracks (coverid);
CREATE INDEX IF NOT EXISTS tracks_album_disc_tracknum ON tracks (album, disc, tracknum);
