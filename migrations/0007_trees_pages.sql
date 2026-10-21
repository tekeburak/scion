-- Trees and path-history pages move from the object store to the metadata
-- database (Phase 10.3): both are small, read many at a time while deriving,
-- logging and cloning, and written a batch at a time by the import. They stay
-- immutable and content-addressed: a row is inserted once and never updated
-- or deleted. Blobs stay in the object store.

-- `bytes` is the tree's canonical encoding; `id` = blake3(bytes).
CREATE TABLE trees (
    repo    BIGINT NOT NULL REFERENCES repos(id),
    id      BYTEA  NOT NULL CHECK (octet_length(id) = 32),
    bytes   BYTEA  NOT NULL,
    PRIMARY KEY (repo, id)
);

-- A page of `path_history` nodes; `id` = blake3(bytes).
CREATE TABLE pages (
    repo    BIGINT NOT NULL REFERENCES repos(id),
    id      BYTEA  NOT NULL CHECK (octet_length(id) = 32),
    bytes   BYTEA  NOT NULL,
    PRIMARY KEY (repo, id)
);
