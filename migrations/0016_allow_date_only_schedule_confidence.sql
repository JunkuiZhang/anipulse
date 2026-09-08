-- SQLite cannot widen an inline CHECK constraint in place. Rename the legacy
-- column, add the widened replacement, and copy all existing values. Keeping
-- the legacy column avoids a table rebuild, which would be risky because many
-- runtime tables reference anime(id) with cascading foreign keys.
ALTER TABLE anime RENAME COLUMN schedule_confidence TO schedule_confidence_legacy;

ALTER TABLE anime ADD COLUMN schedule_confidence TEXT
    CHECK(schedule_confidence IS NULL OR schedule_confidence IN (
        'calibrated', 'stale', 'estimated', 'date_only', 'unavailable'
    ));

UPDATE anime
SET schedule_confidence = schedule_confidence_legacy;
