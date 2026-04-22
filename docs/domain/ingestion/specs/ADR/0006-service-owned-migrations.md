---
id: cpt-ingestion-adr-service-owned-migrations
status: accepted
date: 2026-04-22
---

# ADR-0006 — Service-owned MariaDB migrations

## Context

Earlier work in PR #214 introduced a **global** MariaDB migration
runner: a bash script (`run-migrations-mariadb.sh`) that applied
`*.sql` / `*.sh` files from a central `src/ingestion/scripts/
migrations/mariadb/` directory and tracked progress in a shared
`schema_migrations` table. The runner was invoked from `init.sh` and
`up.sh` before any backend service started.

This was reviewed and reverted during the same PR. The conclusion:

1. The global runner creates a **cross-cutting coupling**: every team
   shipping MariaDB changes has to edit a directory that lives in
   ingestion tooling, not in the domain's own codebase. Schema and
   the code that depends on it live in two different places.

2. Our one existing precedent — `analytics-api` — had already adopted
   the **service-owned** pattern (SeaORM `Migrator` embedded in the
   Rust service, applied via `Migrator::up()` at startup). Running
   one global bash runner alongside an in-service SeaORM migrator was
   two migration mechanisms for the same MariaDB instance — the topic
   of the (now superseded) ADR-0005 "coexist with seaql_migrations".

3. Database isolation was also shallow: our `persons` table initially
   lived in the `analytics` database shared with analytics-api, then
   moved to a dedicated `identity` database — but schema *authority*
   was still in ingestion, not in the identity-resolution service.

We therefore adopt one consistent rule across all backend services.

## Decision

**Every backend service that owns MariaDB tables:**

1. **Owns its own database** inside the shared MariaDB instance.
   Cross-service access is explicit (cross-database JOINs / separate
   connections), never implicit via shared-schema layout.

2. **Owns its own migrations**, stored inside the service directory
   (`src/backend/services/<name>/src/migration/`), authored with the
   SeaORM migration DSL (raw SQL via `manager.get_connection().
   execute_unprepared(...)` is acceptable when column-level
   properties — charset, collation — are not cleanly expressible in
   the DSL).

3. **Applies its migrations at startup**, via `Migrator::up(db,
   None)` invoked from `main`. A helm `initContainer` using the
   service image's `migrate` CLI subcommand runs the same path
   separately for deploy-time ordering (same pattern as
   `analytics-api`).

4. **Tracks applied versions** in its own `seaql_migrations` table
   inside its own database. Different services' trackers live in
   different databases and never collide.

5. **Excludes one-shot data seeds from the Migrator**. Seeds
   (operator-triggered data bootstrap from external stores like
   ClickHouse) are stand-alone scripts in
   `src/backend/services/<name>/seed/`, invoked explicitly by
   operators after migrations and the source data are in place.
   They are not schema migrations and must not enter the migration
   history.

6. **Is responsible for its schema lifecycle**. `up.sh` and `init.sh`
   provision the **database + user grants** (infra concern) but
   never apply per-service DDL.

## Applied to `persons`

- `identity-resolution` service owns the MariaDB database `identity`.
- Schema defined in
  `src/backend/services/identity/src/migration/m20260421_000001_persons.rs`.
- Migrator registered in
  `src/backend/services/identity/src/migration/mod.rs`.
- Applied on startup via `run_migrations(&db)` in `src/main.rs` +
  through the `migrate` subcommand invoked by the helm initContainer.
- One-shot seed scripts (bash + Python) live at
  `src/backend/services/identity/seed/`.

## Consequences

- `up.sh` creates the `identity` database + grants, then hands off to
  the service. No global migration step remains in `up.sh` / `init.sh`.
- Adding a new service-owned MariaDB table means adding a new
  migration file in that service's `migration/` directory — no
  ingestion-side changes required.
- Cross-service schema dependencies become explicit: if service A
  needs data from service B's table, it either reads via service B's
  API or via an explicit cross-database query. No accidental shared
  table layouts.
- Rust becomes the required toolchain for authoring new schema for
  Rust-backed domains. Non-Rust domains (if any arise) would need to
  pick a different migrator — deliberately out of scope for this ADR.

## Alternatives considered

- **Global bash migration runner** (the pre-revert state). Rejected
  after review: see Context §1 and §2.
- **SeaORM migrations in a shared crate across services**. Rejected:
  defeats the per-service-database decision; all services would end
  up re-importing the same migration registry. Service-local is
  simpler.
- **Rust migration library + SQL files** (no SeaORM DSL). Rejected:
  analytics-api already uses SeaORM; a second pattern doubles the
  mental-model load for no benefit.
- **`schema_migrations` bash runner per service** (a runner copy
  inside each service directory). Rejected: still means a bash
  runner at all, and duplicates logic; SeaORM is the canonical Rust
  path.

## Related

- `docs/components/backend/specs/ADR/` (analytics-api Migrator is
  the source pattern this ADR generalises).
- `src/backend/services/identity/src/migration/` — first service-
  owned migration set under this policy.
- `docs/domain/identity-resolution/specs/ADR/0002-deterministic-person-id-for-seed.md`
  — seed contract, unchanged by this ADR (seed stays one-shot, not
  a migration).
- Superseded: ADR-0004 (global MariaDB migration runner) and
  ADR-0005 (coexistence of two trackers in one DB). Both are deleted
  from the repository in the same commit that introduced this ADR —
  neither was ever relied on outside this PR.
