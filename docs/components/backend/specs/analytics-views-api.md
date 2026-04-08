---
status: proposed
date: 2026-04-08
authors: ["insight-front team"]
---

# Analytics View Endpoints — Backend Proposal

<!-- toc -->

- [1. Overview](#1-overview)
- [2. Motivation: View Endpoints vs. OData](#2-motivation-view-endpoints-vs-odata)
- [3. Data Sources Inventory](#3-data-sources-inventory)
- [4. Endpoints](#4-endpoints)
  - [4.1 Executive View](#41-executive-view)
  - [4.2 Team View](#42-team-view)
  - [4.3 IC Dashboard](#43-ic-dashboard)
  - [4.4 Drill Endpoints](#44-drill-endpoints)
- [5. Shared Types](#5-shared-types)
- [6. Data Availability Envelope](#6-data-availability-envelope)
- [7. Period Granularity Contract](#7-period-granularity-contract)
- [8. CI Connector Roadmap](#8-ci-connector-roadmap)
- [9. Implementation Checklist](#9-implementation-checklist)

<!-- /toc -->

---

## 1. Overview

This document proposes five pre-aggregated view endpoints for the Analytics API service.
They are consumed by the `insight-front` frontend (React) and replace the need for the
frontend to construct complex queries against the generic OData endpoint.

**Base path:** `GET /api/v1/analytics/views/...`  
**Auth:** `Authorization: Bearer <token>` + `X-Tenant-ID: <tenantId>`

---

## 2. Motivation: View Endpoints vs. OData

The backend DESIGN exposes `POST /api/v1/analytics/metrics/query` (OData).
The frontend does not use it directly. Screen-specific view endpoints are proposed
on top as thin compositions over the OData/ClickHouse layer for three reasons:

1. **Pre-aggregated visual positioning** — bullet chart metrics require `bar_left_pct`,
   `median_left_pct`, and `bar_width_pct` computed server-side from P5/P50/P95
   distribution data. This data is not available to the client.

2. **Single RTT per screen** — each view screen needs 15–40 fields in one response.
   OData would require multiple queries and client-side joins that break on auth boundaries.

3. **Config co-location** — `TeamViewConfig` (alert thresholds, column thresholds)
   must travel with the data so the frontend remains fully data-driven with no
   hardcoded threshold values.

The view endpoints are stateless — they accept `period` and return a fully computed
snapshot. The backend implements them as query plans over existing Silver/Gold ClickHouse
tables.

---

## 3. Data Sources Inventory

| Source | Silver Table | Status | Connector PR |
|---|---|---|---|
| GitHub | `class_git_commits`, `class_git_pull_requests`, `class_git_pull_requests_reviewers` | ✓ available | #57 merged |
| Bitbucket Cloud | same `class_git_*` tables (data_source discriminator) | ⚠ in progress | #58 draft |
| Bitbucket Server | same `class_git_*` tables | ⚠ configured | — |
| Claude Team / Code | `class_ai_dev_usage` | ✓ available | #50 merged |
| Cursor | `class_ai_dev_usage` | ✓ available | merged |
| BambooHR | `class_people` | ✓ available | #47 merged |
| Zoom | `class_comms_events` | ✓ available | #61 merged |
| M365 | `class_comms_events` | ✓ available | merged |
| Slack | `class_comms_events` | ✓ available | #48 merged |
| Jira | `class_tasks` (TBD) | ⚠ pending | #62 open |
| GitHub Actions (CI) | `class_ci_runs` (proposed) | ❌ no connector | see §8 |

---

## 4. Endpoints

### 4.1 Executive View

```
GET /api/v1/analytics/views/executive?period={week|month|quarter|year}
```

Returns org-wide team table with KPI cards. One request per period change.

**Response shape: `ExecViewData`**

```ts
type ExecTeamRow = {
  team_id:            string;         // [identity]  org_units.id
  team_name:          string;         // [identity]  org_units.name
  headcount:          number;         // [hr]        COUNT(class_people WHERE org_unit AND active)
  tasks_closed:       number | null;  // [tasks]     COUNT(class_tasks WHERE done AND type!='Bug') — null until PR #62
  bugs_fixed:         number | null;  // [tasks]     COUNT(class_tasks WHERE done AND type='Bug')  — null until PR #62
  build_success_pct:  number | null;  // [ci]        successful_runs/total_runs*100 — null until CI connector
  focus_time_pct:     number;         // [comms]     (work_h - meeting_h) / work_h * 100
  ai_adoption_pct:    number;         // [ai-code]   active_ai_users / headcount * 100
  ai_loc_share_pct:   number;         // [ai-code]   SUM(ai_lines_added) / total_loc * 100
  pr_cycle_time_h:    number;         // [git]       AVG(merged_at - created_at) in hours
  status:             'good' | 'warn' | 'bad';  // computed from ExecViewConfig.column_thresholds
};

type OrgKpis = {
  avgBuildSuccess:    number | null;  // [ci]        null until CI connector
  avgAiAdoption:      number;         // [ai-code]
  avgFocus:           number;         // [comms]
  bugResolutionScore: number | null;  // [tasks]     bugs_fixed / (bugs_opened + 1) * 100 — null until PR #62
  prCycleScore:       number;         // [git]       score derived from AVG(pr_cycle_time_h) vs threshold
};

type ExecViewConfig = {
  column_thresholds: Array<{ metric_key: string; threshold: number }>;
  // stored in Analytics API MariaDB (Dashboard entity)
};

type ExecViewData = {
  teams:             ExecTeamRow[];
  orgKpis:           OrgKpis;
  config:            ExecViewConfig;
  data_availability: DataAvailability;  // see §6
};
```

---

### 4.2 Team View

```
GET /api/v1/analytics/views/team?period={week|month|quarter|year}
```

Returns per-person member table, bullet benchmark sections, hero KPI strip,
and config. Team is resolved from the authenticated user's `org_unit_id`.

**Response shape: `TeamViewData`**

```ts
type TeamMember = {
  person_id:          string;         // [identity]
  period:             PeriodValue;
  name:               string;         // [hr]   class_people.display_name
  seniority:          string;         // [hr]   custom_str_attrs['seniority'] or job_title mapping
  tasks_closed:       number;         // [tasks] 0 until PR #62
  bugs_fixed:         number;         // [tasks] 0 until PR #62
  dev_time_h:         number;         // [comms] work_h - SUM(meeting_duration / 3600)
  prs_merged:         number;         // [git]
  build_success_pct:  number | null;  // [ci]   null until CI connector
  focus_time_pct:     number;         // [comms]
  ai_tools:           string[];       // [ai-code] ARRAY_AGG(DISTINCT source)
  ai_loc_share_pct:   number;         // [ai-code]
  trend_label?:       string;         // [git]  rolling comparison, e.g. "3 months declining"
};

type TeamKpi = {
  metric_key: string;
  label:      string;
  value:      string;
  unit:       string;
  sublabel?:  string;
  chipLabel?: string;
  status:     'good' | 'warn' | 'bad';
  section:    string;
};

// KPI derivation (backend computes from members + config):
//   at_risk_count  = COUNT(members) WHERE any alert_threshold triggered
//   focus_gte_60   = COUNT(members) WHERE focus_time_pct >= 60
//   not_using_ai   = COUNT(members) WHERE ai_tools IS EMPTY
//   avg_pr_cycle   = AVG(pr_cycle_time_h) across team
//   total_loc      = SUM(additions - deletions FROM class_git_commits WHERE author IN team)

type AlertThreshold = {
  metric_key: string;
  trigger:    number;   // at-risk if value < trigger
  bad:        number;   // 'bad' severity if value < bad
  reason:     string;
};

type ColumnThreshold = {
  metric_key:       string;
  good:             number;
  warn:             number;
  higher_is_better: boolean;
};

type TeamViewConfig = {
  alert_thresholds:  AlertThreshold[];   // drives "Attention Needed" section
  column_thresholds: ColumnThreshold[];  // drives MembersTable column coloring
  // stored in Analytics API MariaDB; returned with view response
};

type TeamViewData = {
  teamName:          string;
  teamKpis:          TeamKpi[];
  members:           TeamMember[];
  bulletSections:    BulletSection[];
  config:            TeamViewConfig;
  data_availability: DataAvailability;
};
```

**Bullet section positions** are precomputed from org-unit P5/P50/P95 distribution:
- `bar_left_pct   = (value - range_min) / (range_max - range_min) * 100`
- `median_left_pct = (median - range_min) / (range_max - range_min) * 100`

---

### 4.3 IC Dashboard

```
GET /api/v1/analytics/views/ic/{personId}?period={week|month|quarter|year}
```

Returns individual contributor data for the given person.

```ts
type IcKpi = {
  period:      PeriodValue;
  metric_key:  string;
  label:       string;
  value:       string;
  unit:        string;
  sublabel:    string;    // data source label, e.g. "Bitbucket"
  description?: string;
  delta:       string;    // vs. previous period, e.g. "+12%"
  delta_type:  'good' | 'warn' | 'bad' | 'neutral';
};

// IcKpi metric_key catalog:
//   loc               [git]      SUM(additions - deletions FROM class_git_commits)
//   ai_loc_share_pct  [ai-code]  SUM(lines_added) / loc * 100
//   prs_merged        [git]      COUNT(class_git_pull_requests WHERE merged)
//   pr_cycle_time_h   [git]      AVG(merged_at - created_at) in hours
//   focus_time_pct    [comms]    (work_h - meeting_h) / work_h * 100
//   tasks_closed      [tasks]    ⚠ pending — 0 until PR #62
//   bugs_fixed        [tasks]    ⚠ pending — 0 until PR #62
//   build_success_pct [ci]       ❌ missing — null until CI connector
//   ai_sessions       [ai-code]  SUM(session_count FROM class_ai_dev_usage)

type LocDataPoint     = { label: string; aiLoc: number; codeLoc: number; specLines: number };
type DeliveryDataPoint = { label: string; commits: number; prsMerged: number; tasksDone: number };

type IcChartsData = {
  locTrend:      LocDataPoint[];      // see §7 for period granularity
  deliveryTrend: DeliveryDataPoint[];
};

type TimeOffNotice = {
  days:        number;
  dateRange:   string;
  bambooHrUrl: string;   // [hr-leave]
};

type IcDashboardData = {
  person:            PersonData;
  kpis:              IcKpi[];
  bulletMetrics:     BulletMetric[];
  charts:            IcChartsData;
  timeOffNotice:     TimeOffNotice | null;
  drills:            Record<string, DrillData>;
  data_availability: DataAvailability;
};
```

---

### 4.4 Drill Endpoints

```
GET /api/v1/analytics/views/ic/{personId}/drill/{drillId}
GET /api/v1/analytics/views/team/drill/{drillId}?period={period}
```

Returns tabular row data for a single metric. The drill table columns and
rows are fully determined by the backend — the frontend renders them generically.

```ts
type DrillData = {
  title:    string;
  source:   string;       // e.g. "Jira", "Bitbucket"
  srcClass: string;       // tailwind bg class for the source badge
  value:    string;       // formatted aggregate, e.g. "12" or "94%"
  filter:   string;       // human-readable filter description shown to user
  columns:  string[];
  rows:     Array<Record<string, string | number>>;
};
```

---

## 5. Shared Types

```ts
type PeriodValue = 'week' | 'month' | 'quarter' | 'year';

type PersonData = {
  person_id: string;   // [identity]
  name:      string;   // [hr]
  role:      string;   // [identity]
  seniority: string;   // [hr]
};

type BulletMetric = {
  period:          PeriodValue;
  section:         string;
  metric_key:      string;
  label:           string;
  sublabel?:       string;
  value:           string;
  unit:            string;
  range_min:       string;       // P5 of org-unit distribution
  range_max:       string;       // P95 of org-unit distribution
  median:          string;       // P50
  median_label:    string;
  bar_left_pct:    number;       // precomputed
  bar_width_pct:   number;       // precomputed
  median_left_pct: number;       // precomputed
  status:          'good' | 'warn' | 'bad';
  drill_id:        string;       // empty string if no drill
};

type BulletSection = { id: string; title: string; metrics: BulletMetric[] };
```

---

## 6. Data Availability Envelope

Each view response includes a `data_availability` object so the frontend
can show "Not configured" states instead of misleading zeros.

```ts
type DataAvailability = {
  git:   'available' | 'no-connector' | 'syncing';
  tasks: 'available' | 'no-connector' | 'syncing';
  ci:    'available' | 'no-connector' | 'syncing';
  comms: 'available' | 'no-connector' | 'syncing';
  hr:    'available' | 'no-connector' | 'syncing';
  ai:    'available' | 'no-connector' | 'syncing';
};
```

Fields from missing/pending sources return:
- `number | null` fields → `null`
- `number` fields (no null variant) → `0`
- `string[]` fields → `[]`

When `data_availability.ci = 'no-connector'`, the frontend shows "—" instead
of `0%` for all CI-sourced metrics.

---

## 7. Period Granularity Contract

Chart data is returned at period-correct granularity. The frontend renders
`label` as the x-axis key directly — no client-side aggregation.

| `period`  | Points | Labels                          |
|-----------|--------|---------------------------------|
| `week`    | 5      | Mon, Tue, Wed, Thu, Fri         |
| `month`   | 4      | W1, W2, W3, W4                  |
| `quarter` | 3      | Jan, Feb, Mar (quarter months)  |
| `year`    | 4      | Q1, Q2, Q3, Q4                  |

---

## 8. CI Connector Roadmap

`build_success_pct` returns `null` in v1 — no CI connector exists yet.
This section proposes the implementation path.

### Proposed `class_ci_runs` Silver table

```sql
class_ci_runs (
  tenant_id        UUID,
  source_id        String,        -- 'insight_github', 'insight_bitbucket_cloud'
  unique_key       String,
  run_id           String,
  pipeline_name    String,
  branch           String,
  commit_sha       String,        -- joins class_git_commits
  repo_name        String,
  triggered_by     String,        -- person email or 'scheduler'
  status           LowCardinality(String),  -- 'success' | 'failure' | 'cancelled'
  started_at       DateTime,
  finished_at      DateTime,
  duration_seconds UInt32,
  person_id        UUID NULL      -- resolved via identity resolution
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(started_at)
ORDER BY (tenant_id, source_id, run_id);
```

### Step 1 — GitHub Actions stream

Add `workflow_runs` stream to the GitHub connector (follow-up to PR #57):

- Endpoint: `GET /repos/{owner}/{repo}/actions/runs`
- Incremental by `updated_at`
- Bronze table: `bronze_github.workflow_runs`
- Fields: `id`, `name`, `head_branch`, `head_sha`, `status`, `conclusion`,
  `created_at`, `run_started_at`, `triggering_actor.login`

### Step 2 — Bitbucket Pipelines stream

Add `pipelines` stream to the Bitbucket Cloud connector (add before PR #58 merge):

- Endpoint: `GET /repositories/{workspace}/{repo}/pipelines/`
- Incremental by `created_on`
- Bronze table: `bronze_bitbucket_cloud.pipelines`
- Fields: `uuid`, `state.name`, `state.result.name`, `target.ref_name`,
  `target.commit.hash`, `trigger.name`, `created_on`, `completed_on`, `duration_in_seconds`

### Step 3 — dbt Silver model

Create `class_ci_runs` unifying both sources via `union_by_tag`:
- Map `conclusion='success'` / `state.result.name='SUCCESSFUL'` → `status='success'`
- Map `head_sha` / `target.commit.hash` → `commit_sha`

### Step 4 — Identity resolution

Register `triggering_actor.login` (GitHub) and `trigger.name` (Bitbucket)
as `alias_type = 'username'` in `bootstrap_inputs`.

### Impact

Once `class_ci_runs` is available:
- `build_success_pct` changes from `null` to real values in all three views
- `data_availability.ci` changes from `'no-connector'` to `'available'`
- No frontend type changes needed — `number | null` already handles both states

---

## 9. Implementation Checklist

### Backend — view endpoints

- [ ] `GET /views/executive?period=` — ExecViewData
- [ ] `GET /views/team?period=` — TeamViewData (team resolved from auth context)
- [ ] `GET /views/ic/{personId}?period=` — IcDashboardData
- [ ] `GET /views/ic/{personId}/drill/{drillId}` — DrillData
- [ ] `GET /views/team/drill/{drillId}?period=` — DrillData
- [ ] Return `data_availability` envelope in all view responses
- [ ] Store `TeamViewConfig` and `ExecViewConfig` in MariaDB (Dashboard entity)
- [ ] Return config with each view response (no separate config endpoint)

### Backend — Jira connector (unblocks `tasks_closed`, `bugs_fixed`)

- [ ] Confirm `class_tasks` Silver schema with Jira connector author (PR #62)
- [ ] Add `[tasks]` source tag to Silver model
- [ ] Switch null → real values once connector ships; set `data_availability.tasks = 'available'`

### Backend — CI connector (unblocks `build_success_pct`)

- [ ] GitHub Actions stream (§8 Step 1)
- [ ] Bitbucket Pipelines stream (§8 Step 2)
- [ ] `class_ci_runs` Silver dbt model (§8 Step 3)
- [ ] Identity resolution for CI actors (§8 Step 4)
- [ ] Switch `build_success_pct` null → real values; set `data_availability.ci = 'available'`

### Frontend — already implemented (insight-front)

- [x] `DataAvailability` type and nullable field types (`number | null`)
- [x] `data_availability` field on `ExecViewData`, `TeamViewData`, `IcDashboardData`
- [x] `TeamsTable` / `MembersTable` render `null` as "—"
- [x] `OrgKpiCards` shows "Not configured" description when `avgBuildSuccess` is null
- [x] `METRIC_KEYS` catalog in `types/index.ts`
