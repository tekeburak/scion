-- The land worker's start-up query finds repositories with a pending or
-- landing request; the index covers both so neither it nor the claim scans.
DROP INDEX change_requests_pending;
CREATE INDEX change_requests_open ON change_requests (repo, submitted_at)
    WHERE status IN ('pending', 'landing');
