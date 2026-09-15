-- Blob and chunk stores of a group: twins of `blobs` (0013) and `chunks`
-- (0014) keyed by group instead of user.
--
-- The per-account dedup rule of 0013 stands: content existence must not leak
-- across accounts. A shared save cannot live under its owner's key without
-- letting members probe the owner's other saves, so a group is its own
-- namespace, on disk at blobs/group/<group_id>/… and chunks/group/<group_id>/….
-- Existence then leaks only inside the group, whose members can read the save
-- anyway.
--
-- refcount semantics are those of 0013 and 0014: every referencing
-- snapshot_files (or snapshot_file_chunks) row, live or trashed, and GC at 0.
CREATE TABLE IF NOT EXISTS group_blobs (
    group_id   TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    sha256     TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    refcount   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (group_id, sha256)
);

CREATE TABLE IF NOT EXISTS group_chunks (
    group_id   TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    sha256     TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    refcount   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (group_id, sha256)
);
