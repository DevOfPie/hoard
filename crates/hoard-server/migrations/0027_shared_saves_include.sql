-- What a shared save consists of: a JSON array of `/`-separated patterns
-- relative to the save's root (`["worlds_local/Alpha.db", ...]`), set when the
-- save is shared and read back by every member, so all of them walk the same
-- files. NULL is everything, which is what every share before this column was.
ALTER TABLE shared_saves ADD COLUMN include_json TEXT;
