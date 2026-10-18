-- "When did X land": a changeset lands as a request's head (a fast-forward)
-- or as its rebased `landed_cs`. Only landed rows are indexed, so a lookup is
-- one probe per changeset in each index.
CREATE INDEX change_requests_landed_cs ON change_requests (repo, landed_cs)
    WHERE status = 'landed';
CREATE INDEX change_requests_landed_head ON change_requests (repo, head_cs)
    WHERE status = 'landed';
