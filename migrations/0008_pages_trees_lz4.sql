-- A path log reads one history page per change, each about 12 kB and stored
-- compressed out of line. lz4 decompresses them faster than the default pglz
-- at about the same size (Phase 10.7). Rows already stored keep the
-- compression they were written with; both read the same.
ALTER TABLE pages ALTER COLUMN bytes SET COMPRESSION lz4;
ALTER TABLE trees ALTER COLUMN bytes SET COMPRESSION lz4;
