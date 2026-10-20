import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from repair_release_backfill import protected_digest, repair, timestamp


SINCE = "2026-10-20T11:11:47Z"
EXPECTED = "2026-10-03T13:00:00Z"


class RepairTests(unittest.TestCase):
    def test_rust_nanoseconds_are_normalized_for_python_310(self):
        self.assertEqual(timestamp("2026-10-20T11:12:50.216505206+00:00"),timestamp("2026-10-20T11:12:50.216505Z"))
        self.assertEqual(timestamp("2026-10-20T11:12:50.1Z"),timestamp("2026-10-20T11:12:50.100000+00:00"))

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.database = Path(self.directory.name) / "test.db"
        self.manifest = [{"episode_id": 151, "anime_id": 1,
                          "episode_no": 1, "expected_at": EXPECTED}]
        self.connection = sqlite3.connect(self.database)
        self.addCleanup(self.connection.close)
        self.connection.executescript("""
            PRAGMA foreign_keys=ON;
            CREATE TABLE _sqlx_migrations(version INTEGER,success INTEGER);
            INSERT INTO _sqlx_migrations VALUES(20,1);
            CREATE TABLE anime(id INTEGER PRIMARY KEY,tracking_start_episode INTEGER);
            INSERT INTO anime VALUES(1,3);
            CREATE TABLE episode(id INTEGER PRIMARY KEY,anime_id INTEGER,episode_no INTEGER,
                expected_at TEXT,state TEXT,watched_at TEXT,confirmed_at TEXT,notified_at TEXT);
            CREATE TABLE episode_video(id INTEGER PRIMARY KEY,episode_id INTEGER
                REFERENCES episode(id) ON DELETE CASCADE,bvid TEXT);
            CREATE TABLE candidate(id INTEGER PRIMARY KEY,episode_id INTEGER
                REFERENCES episode(id) ON DELETE CASCADE,first_seen_at TEXT);
            CREATE TABLE uploader_trust(anime_id INTEGER,uploader_mid INTEGER,confirmed_count INTEGER);
            CREATE TABLE global_uploader_trust(uploader_mid INTEGER);
            CREATE TABLE release_batch(anime_id INTEGER,first_episode INTEGER,last_episode INTEGER);
            CREATE TABLE management_job(state TEXT,payload_json TEXT);
            CREATE TABLE audit_event(id INTEGER PRIMARY KEY,actor_type TEXT,action TEXT,
                entity_type TEXT,entity_id TEXT,outcome TEXT,metadata_json TEXT,created_at TEXT);
            INSERT INTO episode VALUES(151,1,1,'2026-10-03T13:00:00Z','waiting',NULL,NULL,NULL);
            INSERT INTO episode VALUES(2,1,2,NULL,'notified','2026-10-13T00:00:00Z',
                '2026-10-12T00:00:00Z','2026-10-12T00:00:00Z');
            INSERT INTO episode VALUES(3,1,3,NULL,'waiting',NULL,NULL,NULL);
            INSERT INTO candidate VALUES(1,151,'2026-10-20T11:13:00Z');
            INSERT INTO episode_video VALUES(1,2,'BVknown');
            INSERT INTO uploader_trust VALUES(1,100,4);
            INSERT INTO global_uploader_trust VALUES(100);
        """)

    def run_repair(self, apply=False):
        self.connection.commit()
        return repair(self.database, self.manifest, SINCE, apply)

    def test_dry_run_apply_and_repeat_preserve_known_history_and_trust(self):
        before = protected_digest(self.connection)
        self.assertEqual(self.run_repair()["episodes"], [151])
        self.assertIsNotNone(self.connection.execute("SELECT 1 FROM episode WHERE id=151").fetchone())
        self.assertEqual(self.run_repair(True)["episodes"], [151])
        self.assertEqual(protected_digest(self.connection), before)
        self.assertEqual(self.connection.execute("SELECT COUNT(*) FROM candidate").fetchone()[0], 0)
        self.assertEqual(self.run_repair(True)["episodes"], [])

    def test_explicit_user_action_is_never_removed(self):
        self.connection.execute("INSERT INTO audit_event(actor_type,entity_type,entity_id,outcome,metadata_json) VALUES('admin','candidate','151:BVmanual','success',?)", (json.dumps({"episode_id": 151}),))
        result = self.run_repair(True)
        self.assertEqual(result["episodes"], [])
        self.assertIn("user action", result["skipped"][0][1])

    def test_candidates_predating_the_incident_are_not_removed(self):
        self.connection.execute("UPDATE candidate SET first_seen_at='2026-10-19T00:00:00Z'")
        result = self.run_repair(True)
        self.assertEqual(result["episodes"], [])
        self.assertIn("predates", result["skipped"][0][1])

    def test_new_video_or_watched_history_is_protected(self):
        self.connection.execute("INSERT INTO episode_video VALUES(2,151,'BVnew')")
        self.assertEqual(self.run_repair(True)["episodes"], [])
        self.connection.execute("DELETE FROM episode_video WHERE id=2")
        self.connection.execute("UPDATE episode SET watched_at='2026-10-20T12:00:00Z' WHERE id=151")
        self.assertEqual(self.run_repair(True)["episodes"], [])

    def test_current_frontier_and_active_batch_cannot_be_cleaned(self):
        self.manifest[0]["episode_no"] = 3
        self.connection.execute("DELETE FROM episode WHERE id=3")
        self.connection.execute("UPDATE episode SET episode_no=3 WHERE id=151")
        self.assertEqual(self.run_repair(True)["episodes"], [])
        self.manifest[0]["episode_no"] = 5
        self.connection.execute("UPDATE episode SET episode_no=5 WHERE id=151")
        self.connection.execute("INSERT INTO release_batch VALUES(1,1,13)")
        self.assertEqual(self.run_repair(True)["episodes"], [])

    def test_identity_mismatch_rolls_back_the_entire_repair(self):
        self.manifest.append({"episode_id": 3, "anime_id": 99,
                              "episode_no": 3, "expected_at": None})
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            self.run_repair(True)
        self.assertIsNotNone(self.connection.execute("SELECT 1 FROM episode WHERE id=151").fetchone())

    def test_pending_user_import_is_protected(self):
        self.connection.execute("INSERT INTO management_job VALUES('queued',?)", (json.dumps({"episode_id":151}),))
        self.assertEqual(self.run_repair(True)["episodes"], [])


if __name__ == "__main__":
    unittest.main()
