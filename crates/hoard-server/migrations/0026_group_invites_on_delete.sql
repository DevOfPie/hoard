-- `group_invites.created_by` and `used_by` referenced `users` with no ON DELETE
-- (0022), so with foreign_keys=ON deleting any user who had minted or redeemed
-- an invite failed on the constraint. SQLite cannot alter a foreign key in
-- place: the table is rebuilt, rows copied, the old one dropped, the index
-- recreated. The minter's invites go with them (their group does too); a
-- redeemed invite outlives the redeemer with `used_by` cleared, since it is
-- spent either way.
CREATE TABLE group_invites_new (
    id         TEXT PRIMARY KEY NOT NULL,
    group_id   TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_by TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at TEXT NOT NULL,
    used_by    TEXT REFERENCES users(id) ON DELETE SET NULL,
    used_at    TEXT
);

INSERT INTO group_invites_new (id, group_id, token_hash, created_by, created_at, expires_at, used_by, used_at)
SELECT id, group_id, token_hash, created_by, created_at, expires_at, used_by, used_at
FROM group_invites;

DROP TABLE group_invites;
ALTER TABLE group_invites_new RENAME TO group_invites;

CREATE INDEX IF NOT EXISTS idx_group_invites_group ON group_invites(group_id);
