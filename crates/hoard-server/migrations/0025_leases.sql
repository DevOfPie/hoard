-- Who is hosting a shared save right now.
--
-- One row per save, rewritten on every acquire: the member holding it, the
-- device it is held from, and the version it was taken at. The holder renews
-- by heartbeat; `pushed_since` flips when the holder commits a version, which
-- is what stops a takeover from discarding play that already reached the
-- server.
--
-- There is no `active` column. A lease is live when `released_at IS NULL` and
-- `renewed_at` is within the TTL, computed on read. Storing the flag would
-- require the holder to clear it, and a machine that dies mid-session clears
-- nothing (the same reasoning as `online` in 0019).
CREATE TABLE IF NOT EXISTS save_leases (
    save_id          TEXT PRIMARY KEY NOT NULL REFERENCES saves(id) ON DELETE CASCADE,
    holder_user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    holder_device_fp TEXT,
    acquired_at      TEXT NOT NULL,
    renewed_at       TEXT NOT NULL,
    base_version     INTEGER NOT NULL,
    pushed_since     INTEGER NOT NULL DEFAULT 0,
    released_at      TEXT
);
