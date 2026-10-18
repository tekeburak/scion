-- Listing requests reads the newest of a repository, of one status or of
-- any, so each listing walks an index for `limit` rows and stops. The status
-- index also serves the land worker's claim and start-up query, which the
-- partial `change_requests_open` index served before.
DROP INDEX change_requests_open;
CREATE INDEX change_requests_submitted ON change_requests (repo, submitted_at, id);
CREATE INDEX change_requests_status ON change_requests (repo, status, submitted_at, id);
