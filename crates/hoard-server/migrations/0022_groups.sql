-- Groups: several accounts on one self-hosted server sharing saves.
--
-- A group is owned by one user, who pays for the storage of everything shared
-- into it (`storage_used_bytes` is the display mirror; the charge lands on the
-- owner's `users.storage_used_bytes`). Members are added by invite only: the
-- owner mints a one-time link, and the link's token is stored hashed, exactly
-- like `api_tokens.token_hash`, so a leaked database cannot be turned into a
-- membership.
--
-- `role` is 'owner' for the creator's own membership row and 'member' for
-- everybody else. The owner has a row too so "the groups I belong to" is one
-- join with no special case.
CREATE TABLE IF NOT EXISTS groups (
    id                 TEXT PRIMARY KEY NOT NULL,
    name               TEXT NOT NULL,
    owner_user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    storage_used_bytes INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

CREATE TABLE IF NOT EXISTS group_members (
    group_id  TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role      TEXT NOT NULL CHECK (role IN ('owner', 'member')),
    joined_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    PRIMARY KEY (group_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_group_members_user ON group_members(user_id);

CREATE TABLE IF NOT EXISTS group_invites (
    id         TEXT PRIMARY KEY NOT NULL,
    group_id   TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_by TEXT NOT NULL REFERENCES users(id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at TEXT NOT NULL,
    used_by    TEXT REFERENCES users(id),
    used_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_group_invites_group ON group_invites(group_id);
