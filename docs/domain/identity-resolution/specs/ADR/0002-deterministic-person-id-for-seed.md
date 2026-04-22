---
id: cpt-ir-adr-deterministic-person-id
status: accepted
date: 2026-04-21
---

# ADR-0002 — Deterministic `person_id` for persons initial seed

## Context

The `persons` table (MariaDB — see
`cpt-insightspec-ir-dbtable-persons-mariadb`) is populated initially
from ClickHouse `identity.identity_inputs` via a one-time seed script
(`src/backend/services/identity/seed/seed-persons-from-identity-input.py`).
Each unique email within a tenant becomes one person.

Two requirements shape this seed:

1. **Safe accidental re-runs** — running the seed twice must not
   duplicate rows and must not require the operator to `TRUNCATE` the
   table.
2. **Cross-source joining from day one** — observations of the same
   email from different connectors (BambooHR, Zoom, Cursor, …) must
   land on the same `person_id` without a separate resolution pass.

A random `uuid4` per email would satisfy neither: the second run would
mint a fresh random id for the same email, and different connectors
would never share one.

## Decision

1. **`person_id` is deterministic**: generated as
   `uuid5(uuid.NAMESPACE_URL, f"insight:person:{insight_tenant_id}:{lower(trim(email))}")`.
   The same `(tenant, email)` pair always produces the same
   `person_id`. Uses RFC 4122's predefined `NAMESPACE_URL` (7fc30aed-…)
   — the same namespace convention as `oidc-authn-plugin`
   (`Uuid::new_v5(&Uuid::NAMESPACE_URL, input.as_bytes())`). No custom
   "project-level magic UUID" to track.

2. **`persons` has a UNIQUE constraint** on the natural key
   `(insight_tenant_id, person_id, insight_source_type,
   insight_source_id, alias_type, alias_value)` — one observation of
   one field for one person from one source instance is unique.

3. **Seed uses `INSERT IGNORE`** — re-runs silently skip observations
   that already exist and add only genuinely new ones. No abort, no
   `TRUNCATE`, no destruction of operator-authored rows.

4. **The script never issues `TRUNCATE`** — wiping the table remains
   an explicit operator action outside the script.

## Rationale

- **No data loss on re-run**: operator-authored rows and prior seed
  results survive. The script is safe to re-run after a new connector
  sync to pick up newly-observed accounts.
- **No duplicates on re-run**: the UNIQUE key enforces it at the
  database level; `INSERT IGNORE` handles it at the statement level.
  Correctness does not depend on the script being run exactly once.
- **Cross-source joining works from day one**: `person_id` is shared
  across all source-accounts of the same email — exactly the effect
  that random UUIDs would have required an additional resolution pass
  to achieve.
- **No project-specific magic constant**: `uuid.NAMESPACE_URL` is a
  predefined RFC 4122 namespace, stable across every UUID library.
  The input-string scheme (`insight:person:…`) makes the namespace
  intent self-documenting.

## Consequences

- `person_id` values are **stable** across seed runs — downstream
  systems can reference them safely.
- The input-string format
  (`insight:person:{insight_tenant_id}:{lower(trim(email))}`) is
  itself the stable interface. Changing the format would re-assign
  every `person_id` and break downstream references; document any
  future change via a new ADR.
- The UNIQUE index on the observation tuple enforces the natural key.
  Any future column addition that should be part of the identity of
  an observation must update this index.
- The approach assumes `email` is the sole bootstrap key. Accounts
  without an email are silently skipped. This ADR does not change
  that.

## Alternatives considered

- **Custom project-level namespace UUID**. Rejected: introduces a
  magic constant that has to be preserved through every environment
  migration, while `NAMESPACE_URL` is a cross-library RFC-defined
  value and the disambiguation of the intent goes into the input
  string instead.
- **Random `person_id` per run**. Rejected: not idempotent,
  re-running would duplicate rows.
- **`person_id` as auto-increment from MariaDB**. Rejected: the
  column type across domains is UUID (glossary convention); we want
  `person_id` assignable purely on the connector / seed side without
  a MariaDB round-trip per account.

## Related

- `cpt-insightspec-ir-dbtable-persons-mariadb` — the persons table
  definition
- `cpt-ir-fr-persons-initial-seed` — functional requirement for the
  seed
- `src/backend/plugins/oidc-authn-plugin/src/domain/service.rs` —
  prior art for `NAMESPACE_URL`-based UUIDv5 in this project
- `docs/shared/glossary/ADR/0001-uuidv7-primary-key.md` — UUID types
  across the project
