-- The principal that submitted a request: the land worker lands only the
-- paths it may change. Requests from before authentication have none, so
-- they land nothing.
ALTER TABLE change_requests ADD COLUMN submitter TEXT NOT NULL DEFAULT '';
ALTER TABLE change_requests ALTER COLUMN submitter DROP DEFAULT;
