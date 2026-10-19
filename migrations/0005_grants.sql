-- Who may do what in a repository. A grant gives `principal`
-- (`github:<login>` or `sp:<name>`) `role` on the paths under `path_prefix`
-- (empty: the whole repository). Server-wide grants (creating repositories)
-- are stored under the reserved repository `*`, which no repository can take.

INSERT INTO repos (name) VALUES ('*');

CREATE TABLE grants (
    repo         BIGINT NOT NULL REFERENCES repos(id),
    principal    TEXT   NOT NULL,
    path_prefix  TEXT   NOT NULL,
    role         TEXT   NOT NULL CHECK (role IN ('reader', 'writer', 'admin')),
    PRIMARY KEY (repo, principal, path_prefix)
);
