"""Remove only operator-verified empty episodes from a release-backfill incident.

Dry-run by default. Stop both AniPulse services before applying; this script
does not create backups or infer watch history. Manifest entries contain exact
episode_id, anime_id, episode_no and expected_at from the read-only audit.
"""
import argparse
import datetime as dt
import hashlib
import json
import re
from pathlib import Path
import sqlite3


def timestamp(value):
    # Rust stores nanoseconds; Python 3.10 only accepts 3/6 fractional digits.
    normalized = re.sub(r"\.(\d+)(?=[+-])", lambda match: "." + match[1][:6].ljust(6, "0"), value.replace("Z", "+00:00"))
    parsed = dt.datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        raise ValueError("timestamps must include a timezone")
    return parsed


def protected_digest(connection):
    queries = [
        "SELECT * FROM episode_video ORDER BY id",
        "SELECT * FROM episode WHERE watched_at IS NOT NULL OR confirmed_at IS NOT NULL "
        "OR notified_at IS NOT NULL OR state IN ('confirmed','notified') ORDER BY id",
        "SELECT * FROM uploader_trust ORDER BY anime_id,uploader_mid",
        "SELECT * FROM global_uploader_trust ORDER BY uploader_mid",
    ]
    values = [connection.execute(query).fetchall() for query in queries]
    return hashlib.sha256(json.dumps(values, sort_keys=True).encode()).hexdigest()


def repair(database, manifest, regression_since, apply=False):
    since = timestamp(regression_since)
    ids = [entry["episode_id"] for entry in manifest]
    if not ids or len(set(ids)) != len(ids) or len(ids) > 1000:
        raise ValueError("manifest must contain 1..1000 distinct explicit episode IDs")
    uri = Path(database).resolve().as_uri() + ("?mode=rw" if apply else "?mode=ro")
    connection = sqlite3.connect(uri, uri=True, timeout=5)
    connection.execute("PRAGMA foreign_keys=ON")
    try:
        connection.execute("BEGIN IMMEDIATE" if apply else "BEGIN")
        version = connection.execute(
            "SELECT MAX(version) FROM _sqlx_migrations WHERE success=1"
        ).fetchone()[0]
        if version < 20:
            raise ValueError("apply the tracking-scope migration first")
        before = protected_digest(connection)
        selected, skipped = [], []
        for entry in manifest:
            episode_id = entry["episode_id"]
            row = connection.execute(
                "SELECT anime_id,episode_no,expected_at,state,watched_at,confirmed_at,notified_at "
                "FROM episode WHERE id=?", (episode_id,)
            ).fetchone()
            if row is None:
                skipped.append([episode_id, "already absent"])
                continue
            if row[:2] != (entry["anime_id"], entry["episode_no"]):
                raise ValueError(f"episode identity mismatch: {episode_id}")
            expected = entry["expected_at"]
            unchanged = (row[2] is None and expected is None) or (
                row[2] is not None and expected is not None
                and timestamp(row[2]) == timestamp(expected)
            )
            reason = None
            if not unchanged:
                reason = "schedule changed since audit"
            elif row[3] in ("confirmed", "notified") or any(row[4:]):
                reason = "confirmed or watched history"
            elif connection.execute(
                "SELECT 1 FROM episode_video WHERE episode_id=? LIMIT 1", (episode_id,)
            ).fetchone():
                reason = "has video"
            else:
                start = connection.execute(
                    "SELECT tracking_start_episode FROM anime WHERE id=?", (row[0],)
                ).fetchone()[0]
                last = connection.execute(
                    "SELECT MAX(episode_no) FROM episode WHERE anime_id=? "
                    "AND state IN ('confirmed','notified')", (row[0],)
                ).fetchone()[0]
                frontier = max(start, (last + 1) if last is not None else start)
                if start <= row[1] <= frontier:
                    reason = "within current tracking frontier"
                elif row[1] >= start and connection.execute(
                    "SELECT 1 FROM release_batch WHERE anime_id=? AND last_episode>first_episode "
                    "AND ? BETWEEN first_episode AND last_episode LIMIT 1", row[:2]
                ).fetchone():
                    reason = "belongs to an active multi-episode release"
            if not reason:
                first = connection.execute(
                    "SELECT MIN(first_seen_at) FROM candidate WHERE episode_id=?", (episode_id,)
                ).fetchone()[0]
                if first and timestamp(first) < since:
                    reason = "candidate predates incident"
            if not reason and connection.execute(
                "SELECT 1 FROM management_job WHERE state IN ('queued','running') "
                "AND json_extract(payload_json,'$.episode_id')=? LIMIT 1", (episode_id,)
            ).fetchone():
                reason = "has queued or running user job"
            if not reason and connection.execute(
                "SELECT 1 FROM audit_event WHERE actor_type IN ('admin','cli') "
                "AND outcome='success' AND ("
                "(entity_type='episode' AND entity_id=?) OR "
                "(entity_type='candidate' AND entity_id LIKE ?) OR "
                "json_extract(metadata_json,'$.episode_id')=?) LIMIT 1",
                (str(episode_id), f"{episode_id}:%", episode_id),
            ).fetchone():
                reason = "has explicit user action"
            if reason:
                skipped.append([episode_id, reason])
            else:
                selected.append(episode_id)
        if apply:
            for episode_id in selected:
                connection.execute("DELETE FROM episode WHERE id=?", (episode_id,))
        after = protected_digest(connection)
        if before != after:
            raise RuntimeError("protected video/watch/trust records changed; rolling back")
        result = {"mode": "apply" if apply else "dry-run", "episodes": selected,
                  "skipped": skipped, "protected_unchanged": True,
                  "protected_sha256": before}
        if apply:
            connection.execute(
                "INSERT INTO audit_event(actor_type,action,entity_type,outcome,metadata_json,created_at) "
                "VALUES('system','release_backfill.repair','episode','success',?,?)",
                (json.dumps(result), dt.datetime.now(dt.timezone.utc).isoformat()),
            )
            connection.commit()
        else:
            connection.rollback()
        return result
    except Exception:
        connection.rollback()
        raise
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", required=True)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--regression-since", required=True)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--run-as", help="read staging inputs as root, then drop privileges before opening SQLite")
    args = parser.parse_args()
    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    if args.run_as:
        import os
        import pwd
        account = pwd.getpwnam(args.run_as)
        if os.geteuid() != 0 or account.pw_uid == 0:
            raise ValueError("--run-as requires root and a non-root destination user")
        os.setgroups([])
        os.setgid(account.pw_gid)
        os.setuid(account.pw_uid)
        os.umask(0o077)
    print(json.dumps(repair(args.database, manifest, args.regression_since, args.apply)))


if __name__ == "__main__":
    main()
