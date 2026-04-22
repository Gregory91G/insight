#!/usr/bin/env python3
"""
One-time seed: identity_inputs (ClickHouse) -> persons (MariaDB).

Groups identity_inputs rows by source-account (one connector user
instance) and assigns a deterministic person_id (UUIDv5, keyed on
tenant + normalised email) per unique email. Writes every observation
into MariaDB persons via INSERT IGNORE. Re-running is idempotent:
same (tenant, email) always yields the same person_id, and the
uq_person_observation UNIQUE KEY skips already-written rows.
See ADR-0002 (deterministic-person-id-for-seed).

Prerequisites:
  - ClickHouse identity_inputs view exists (run dbt first)
  - MariaDB persons table exists (the identity-resolution service
    applies it at startup via its own SeaORM Migrator; see ADR-0006)
  - Environment: CLICKHOUSE_URL, CLICKHOUSE_USER, CLICKHOUSE_PASSWORD
  - Environment: MARIADB_URL (mysql://user:pass@host:port/identity)

Usage:
  # From host with port-forwards:
  export CLICKHOUSE_URL=http://localhost:30123
  export CLICKHOUSE_USER=default
  export CLICKHOUSE_PASSWORD=<from secret>
  export MARIADB_URL=mysql://insight:insight-pass@localhost:3306/identity

  python3 src/backend/services/identity/seed/seed-persons-from-identity-input.py

  # Or via kubectl port-forward for MariaDB:
  kubectl -n insight port-forward svc/insight-mariadb 3306:3306 &
"""

import base64
import json
import os
import urllib.parse
import urllib.request
import uuid
from collections import defaultdict
from datetime import datetime, timezone
from urllib.parse import unquote, urlparse

# MariaDB driver -- pymysql preferred, mysql.connector fallback. For
# BINARY(16) columns we pass `uuid.UUID.bytes` (16 raw bytes) rather than
# the UUID object itself: both drivers would otherwise fall back to
# str(UUID) -- a 36-char text form -- which BINARY(16) silently
# truncates to the first 16 ASCII bytes, corrupting the column.
try:
    import pymysql as _mysql_driver  # type: ignore[import-not-found]
except ImportError:
    import mysql.connector as _mysql_driver  # type: ignore[import-not-found,no-redef]

# -- Schema constraints (mirror src/backend/services/identity/src/migration/
# m20260421_000001_persons.rs -- the authoritative DDL is now in the Rust
# service's SeaORM Migrator; see ADR-0006)
# VARCHAR(512) for alias_value -- longer values are rejected rather than
# silently truncated by INSERT IGNORE.
MAX_ALIAS_VALUE_LEN = 512

# -- ClickHouse connection ------------------------------------------------
CH_URL = os.environ.get("CLICKHOUSE_URL", "http://localhost:30123")
CH_USER = os.environ.get("CLICKHOUSE_USER", "default")
CH_PASSWORD = os.environ["CLICKHOUSE_PASSWORD"]
# Hard cap on the ClickHouse HTTP query. A stalled endpoint otherwise
# hangs the whole one-shot seed indefinitely.
CH_TIMEOUT_SEC = int(os.environ.get("CLICKHOUSE_TIMEOUT_SEC", "60"))

# Guard urllib against file:// and other non-HTTP schemes -- CH_URL is read
# from env and fed to urlopen; a mistaken value should error, not open a
# local file (Bandit B310).
if urllib.parse.urlparse(CH_URL).scheme not in ("http", "https"):
    raise ValueError(
        f"CLICKHOUSE_URL must use http:// or https:// scheme; got {CH_URL!r}"
    )


def ch_query(sql: str) -> list[dict]:
    """Execute ClickHouse query, return list of dicts."""
    params = urllib.parse.urlencode({"query": sql + " FORMAT JSONEachRow"})
    url = f"{CH_URL}/?{params}"
    req = urllib.request.Request(url)
    creds = base64.b64encode(f"{CH_USER}:{CH_PASSWORD}".encode()).decode()
    req.add_header("Authorization", f"Basic {creds}")
    with urllib.request.urlopen(req, timeout=CH_TIMEOUT_SEC) as resp:  # noqa: S310 -- scheme validated above
        lines = resp.read().decode().strip().split("\n")
        return [json.loads(line) for line in lines if line.strip()]


# -- MariaDB connection ---------------------------------------------------
def get_mariadb_conn():
    """Connect to MariaDB. Requires pymysql or mysql-connector-python."""
    mariadb_url = os.environ.get(
        "MARIADB_URL", "mysql://insight:insight-pass@localhost:3306/identity"
    )
    # seed-persons.sh URL-encodes user/password via urllib.parse.quote() so
    # that passwords containing ':', '@', '/', or '%' do not break URL
    # parsing. urlparse returns the values still-encoded -- we unquote here
    # before handing them to the driver.
    parsed = urlparse(mariadb_url)
    user = unquote(parsed.username) if parsed.username else "insight"
    password = unquote(parsed.password) if parsed.password else ""
    host = parsed.hostname or "localhost"
    port = parsed.port or 3306
    database = parsed.path.lstrip("/") or "identity"

    return _mysql_driver.connect(
        host=host, port=port, user=user, password=password,
        database=database, charset="utf8mb4", autocommit=False,
    )


# -- Main -----------------------------------------------------------------
def main():
    print("=== Seed: identity_inputs -> MariaDB persons ===")

    # 1. Read all identity_inputs rows from ClickHouse.
    #    ORDER BY _synced_at DESC inside each source-account so the email
    #    anchor picked in step 3 is deterministically the latest observation
    #    (ADR-0002 requires stable person_id across re-runs).
    print("  Reading identity_inputs from ClickHouse...")
    rows = ch_query("""
        SELECT
            toString(insight_tenant_id)     AS insight_tenant_id,
            toString(insight_source_id)     AS insight_source_id,
            insight_source_type,
            source_account_id,
            alias_type,
            alias_value,
            _synced_at
        FROM identity.identity_inputs
        WHERE operation_type = 'UPSERT'
          AND alias_value IS NOT NULL
          AND alias_value != ''
        ORDER BY
            insight_tenant_id,
            insight_source_type,
            insight_source_id,
            source_account_id,
            _synced_at DESC,
            alias_type,
            alias_value
    """)
    print(f"  Read {len(rows)} rows")

    if not rows:
        print("  No data -- nothing to seed.")
        return

    # 2. Group by source triple + source_account_id, find emails
    #    Key: (tenant, source_type, source_id, source_account_id) -> list of observations
    accounts: dict[tuple, list[dict]] = defaultdict(list)
    for r in rows:
        key = (
            r["insight_tenant_id"],
            r["insight_source_type"],
            r["insight_source_id"],
            r["source_account_id"],
        )
        accounts[key].append(r)

    # 3. Assign deterministic person_id per unique email (within tenant).
    #    UUIDv5 over RFC 4122 NAMESPACE_URL with a self-documenting input
    #    string -- the same (tenant, email) pair always produces the same
    #    UUID, so re-running the seed never mints a new person_id for an
    #    existing person. Matches the NAMESPACE_URL + string-input pattern
    #    used by oidc-authn-plugin. See ADR-0002.
    email_to_person: dict[tuple[str, str], uuid.UUID] = {}
    account_person: dict[tuple, uuid.UUID] = {}

    for key, obs_list in accounts.items():
        tenant_id = key[0]
        email = None
        for obs in obs_list:
            if obs["alias_type"] == "email":
                email = obs["alias_value"].strip().lower()
                break
        if not email:
            continue  # no email -- skip account (email is the sole person key)

        email_key = (tenant_id, email)
        if email_key not in email_to_person:
            email_to_person[email_key] = uuid.uuid5(
                uuid.NAMESPACE_URL, f"insight:person:{tenant_id}:{email}"
            )
        account_person[key] = email_to_person[email_key]

    print(f"  Unique persons (by email): {len(email_to_person)}")
    print(f"  Accounts with email: {len(account_person)}")

    # 4. Build INSERT rows for MariaDB.
    #    Skip observations whose alias_value exceeds VARCHAR(512) -- without
    #    this check MariaDB may silently truncate (depends on SQL mode) and
    #    the truncated value would corrupt uq_person_observation uniqueness.
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S.000")
    insert_rows = []
    oversized = 0
    for key, obs_list in accounts.items():
        person_id = account_person.get(key)
        if person_id is None:
            continue  # skipped (no email)
        tenant_str, source_type, source_id_str, _ = key
        # tenant_id and insight_source_id come from identity.identity_inputs,
        # where ClickHouse types both columns as UUID -- toString() on the
        # wire always yields a valid UUID string. An invalid value here is
        # an ingestion-pipeline bug; fail loudly with uuid.UUID's native
        # ValueError rather than silently dropping the observation.
        # Bind as 16-byte raw (UUID.bytes) so BINARY(16) gets the real
        # binary value, not the 36-char text form truncated to 16 ASCII
        # bytes.
        tenant_bin = uuid.UUID(tenant_str).bytes
        source_bin = uuid.UUID(source_id_str).bytes
        person_bin = person_id.bytes
        for obs in obs_list:
            alias_value = obs["alias_value"]
            # VARCHAR(512) utf8mb4 caps at 512 *characters* (up to ~2048
            # bytes), so we compare character length, not byte length;
            # otherwise non-ASCII values (IDN emails, accented display
            # names) would be dropped even though MariaDB would accept
            # them.
            if len(alias_value) > MAX_ALIAS_VALUE_LEN:
                oversized += 1
                continue
            insert_rows.append((
                obs["alias_type"],
                source_type,
                source_bin,
                tenant_bin,
                alias_value,
                person_bin,
                person_bin,  # author = self for initial seed
                "",          # reason
                now,
            ))

    print(f"  Rows to insert (pre-dedup): {len(insert_rows)}")
    if oversized:
        print(f"  Rows skipped -- alias_value > {MAX_ALIAS_VALUE_LEN} characters: {oversized}")

    # 5. Write to MariaDB via INSERT IGNORE.
    #    The uq_person_observation UNIQUE KEY guarantees identical
    #    observations are skipped -- re-running is idempotent. No TRUNCATE
    #    anywhere in this script. To wipe and re-seed, operator must
    #    manually TRUNCATE outside this script.
    print("  Connecting to MariaDB...")
    conn = get_mariadb_conn()
    cursor = conn.cursor()

    cursor.execute("SELECT COUNT(*) FROM persons")
    existing_before = cursor.fetchone()[0]
    print(f"  Existing rows before seed: {existing_before}")

    print(f"  Upserting {len(insert_rows)} rows (INSERT IGNORE)...")
    cursor.executemany(
        """INSERT IGNORE INTO persons
           (alias_type, insight_source_type, insight_source_id, insight_tenant_id,
            alias_value, person_id, author_person_id, reason, created_at)
           VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)""",
        insert_rows,
    )
    conn.commit()

    cursor.execute("SELECT COUNT(*) FROM persons")
    existing_after = cursor.fetchone()[0]
    added = existing_after - existing_before
    skipped = len(insert_rows) - added
    print(f"  Added: {added}, skipped as duplicates: {skipped}, total: {existing_after}")

    # Summary
    cursor.execute("""
        SELECT alias_type, COUNT(*) AS cnt
        FROM persons
        GROUP BY alias_type
        ORDER BY alias_type
    """)
    print("\n  Summary:")
    for row in cursor.fetchall():
        print(f"    {row[0]}: {row[1]}")

    cursor.execute("SELECT COUNT(DISTINCT person_id) FROM persons")
    print(f"    Total unique persons: {cursor.fetchone()[0]}")

    conn.close()
    print("\n=== Seed complete ===")


if __name__ == "__main__":
    main()
