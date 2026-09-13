-- Which group a save is shared into.
--
-- One row per shared save and one group per save, on purpose: authorization is
-- then a single join (`saves ⋈ shared_saves ⋈ group_members`) and the blob
-- namespace of a save is a single lookup (0024). A save shared into two groups
-- would need a namespace per group and a copy of every blob in each.
--
-- Deleting the save or the group drops the row; the owner's save itself is
-- never deleted by unsharing.
CREATE TABLE IF NOT EXISTS shared_saves (
    save_id   TEXT PRIMARY KEY NOT NULL REFERENCES saves(id) ON DELETE CASCADE,
    group_id  TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    shared_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_shared_saves_group ON shared_saves(group_id);
