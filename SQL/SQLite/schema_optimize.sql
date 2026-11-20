--
-- Recreate the contributor_album table to ensure it is accurate (bug 4882).
-- This is done here as it is faster to do in sql than in the server.
--

-- XXX This appears to not be needed anymore as contributors are properly
-- removed by the new scanner

DELETE FROM contributor_album;

INSERT INTO contributor_album (role, contributor, album)
SELECT DISTINCT ct.role, ct.contributor, t.album
FROM contributor_track AS ct
JOIN tracks AS t ON t.id = ct.track
WHERE t.album IS NOT NULL;

-- Analyze is slow but very necessary for SQLite to choose correct indices for queries, especially
-- on the tracks table which has a ton of them. Bound analysis to avoid unbounded full-table scans.
PRAGMA analysis_limit=1000;
PRAGMA optimize;
