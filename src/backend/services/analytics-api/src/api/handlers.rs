//! Route handlers.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use sea_orm::{ActiveModelTrait, ColumnTrait, Condition, EntityTrait, NotSet, QueryFilter, Set};
use std::sync::Arc;
use uuid::Uuid;

use super::AppState;
use crate::auth::SecurityContext;
use crate::domain::metric::{
    CreateMetricRequest, Metric, MetricSummary, TableColumn, UpdateMetricRequest,
};
use crate::domain::query::{PageInfo, QueryRequest, QueryResponse};
use crate::domain::threshold::{
    self, CreateThresholdRequest, Threshold, UpdateThresholdRequest,
};
use crate::infra::db::entities;

// ── Health ──────────────────────────────────────────────────

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "healthy" }))
}

// ── Metrics CRUD ────────────────────────────────────────────

pub async fn list_metrics(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
) -> Result<impl IntoResponse, StatusCode> {
    let rows = entities::metrics::Entity::find()
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .filter(entities::metrics::Column::IsEnabled.eq(true))
        .all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to list metrics");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items: Vec<MetricSummary> = rows.into_iter().map(model_to_metric_summary).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

pub async fn get_metric(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    let row = entities::metrics::Entity::find_by_id(id)
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to get metric");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(model_to_metric(row)))
}

pub async fn create_metric(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(req): Json<CreateMetricRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let id = Uuid::now_v7();

    let model = entities::metrics::ActiveModel {
        id: Set(id),
        insight_tenant_id: Set(ctx.insight_tenant_id),
        name: Set(req.name),
        description: Set(req.description),
        query_ref: Set(req.query_ref),
        is_enabled: Set(true),
        created_at: NotSet,
        updated_at: NotSet,
    };

    entities::metrics::Entity::insert(model)
        .exec(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to create metric");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let row = entities::metrics::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(model_to_metric(row))))
}

pub async fn update_metric(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateMetricRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = entities::metrics::Entity::find_by_id(id)
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut model: entities::metrics::ActiveModel = existing.into();

    if let Some(name) = req.name {
        model.name = Set(name);
    }
    if let Some(desc) = req.description {
        model.description = Set(Some(desc));
    }
    if let Some(query_ref) = req.query_ref {
        model.query_ref = Set(query_ref);
    }
    if let Some(enabled) = req.is_enabled {
        model.is_enabled = Set(enabled);
    }

    let updated = model.update(&state.db).await.map_err(|e| {
        tracing::error!(error = %e, "failed to update metric");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(model_to_metric(updated)))
}

pub async fn delete_metric(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = entities::metrics::Entity::find_by_id(id)
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut model: entities::metrics::ActiveModel = existing.into();
    model.is_enabled = Set(false);
    model.update(&state.db).await.map_err(|e| {
        tracing::error!(error = %e, "failed to soft-delete metric");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::NO_CONTENT)
}

// ── Query ───────────────────────────────────────────────────

pub async fn query_metric(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<QueryRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // 1. Load metric definition
    let metric = entities::metrics::Entity::find_by_id(id)
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .filter(entities::metrics::Column::IsEnabled.eq(true))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // 2. Load thresholds for this metric
    let thresholds = entities::thresholds::Entity::find()
        .filter(entities::thresholds::Column::MetricId.eq(id))
        .filter(entities::thresholds::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .all(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // 3. Validate $top
    let top = req.top.min(200).max(1);

    // 4. Build ClickHouse query from query_ref + security filters + OData filters
    //
    // TODO: Full implementation should:
    // - Wrap query_ref as subquery
    // - Parse OData $filter expression into parameterized WHERE clauses
    // - Validate org_unit_id from $filter against AccessScope (IDOR prevention)
    // - Resolve person_ids via Identity Resolution API
    // - Parse $orderby and validate columns against metric schema
    // - Parse $select to restrict returned columns
    // - Implement cursor-based pagination (decode $skip → keyset)
    //
    // Current: demonstrate the query building pipeline with direct table query.

    let query_ref = &metric.query_ref;
    let mut sql = format!("SELECT * FROM ({query_ref}) AS _q WHERE insight_tenant_id = ?");
    let mut params: Vec<String> = vec![ctx.insight_tenant_id.to_string()];

    // Parse OData $filter (simplified — production needs a proper OData parser)
    if let Some(ref filter) = req.filter {
        // Extract date filters
        if let Some(date_from) = extract_odata_value(filter, "metric_date", "ge") {
            sql.push_str(" AND metric_date >= ?");
            params.push(date_from);
        }
        if let Some(date_to) = extract_odata_value(filter, "metric_date", "lt") {
            sql.push_str(" AND metric_date < ?");
            params.push(date_to);
        }
    }

    // Apply $orderby
    if let Some(ref orderby) = req.orderby {
        // TODO: Validate column names against metric schema
        sql.push_str(&format!(" ORDER BY {orderby}"));
    }

    // Apply pagination (fetch top+1 to detect has_next)
    sql.push_str(&format!(" LIMIT {}", top + 1));

    tracing::debug!(sql = %sql, metric_id = %id, "executing metric query");

    // TODO: Execute the query against ClickHouse.
    // For dynamic metrics (columns vary per metric), we need either:
    // - A generic row type that deserializes any column set
    // - Raw query execution returning serde_json::Value rows
    //
    // Placeholder response with debug info.
    let mut items: Vec<serde_json::Value> = vec![serde_json::json!({
        "_debug_sql": sql,
        "_debug_params": params,
        "_note": "query execution not yet implemented — need dynamic row deserialization"
    })];

    // 5. Evaluate thresholds on each result row
    for item in &mut items {
        if let Some(obj) = item.as_object_mut() {
            let mut threshold_results = serde_json::Map::new();
            for t in &thresholds {
                if let Some(val) = obj.get(&t.field_name).and_then(|v| v.as_f64()) {
                    if threshold::threshold_matches(val, &t.operator, t.value) {
                        // Keep highest severity: critical > warning > good
                        let current = threshold_results
                            .get(&t.field_name)
                            .and_then(|v| v.as_str());
                        if should_upgrade_level(current, &t.level) {
                            threshold_results.insert(
                                t.field_name.clone(),
                                serde_json::Value::String(t.level.clone()),
                            );
                        }
                    }
                }
            }
            obj.insert(
                "_thresholds".to_owned(),
                serde_json::Value::Object(threshold_results),
            );
        }
    }

    let response = QueryResponse {
        items,
        page_info: PageInfo {
            has_next: false,
            cursor: None,
        },
    };

    Ok(Json(response))
}

/// Returns true if `new_level` is higher severity than `current`.
fn should_upgrade_level(current: Option<&str>, new_level: &str) -> bool {
    let rank = |l: &str| match l {
        "critical" => 3,
        "warning" => 2,
        "good" => 1,
        _ => 0,
    };
    match current {
        Some(c) => rank(new_level) > rank(c),
        None => true,
    }
}

/// Simplified OData value extractor.
/// Extracts value from patterns like `field_name ge 'value'`.
fn extract_odata_value(filter: &str, field: &str, op: &str) -> Option<String> {
    let pattern = format!("{field} {op} '");
    if let Some(start) = filter.find(&pattern) {
        let rest = &filter[start + pattern.len()..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_owned());
        }
    }
    None
}

// ── Thresholds CRUD ─────────────────────────────────────────

pub async fn list_thresholds(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(metric_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    // Verify metric exists and belongs to tenant
    entities::metrics::Entity::find_by_id(metric_id)
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let rows = entities::thresholds::Entity::find()
        .filter(entities::thresholds::Column::MetricId.eq(metric_id))
        .filter(entities::thresholds::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to list thresholds");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items: Vec<Threshold> = rows.into_iter().map(model_to_threshold).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

pub async fn create_threshold(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(metric_id): Path<Uuid>,
    Json(req): Json<CreateThresholdRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Verify metric exists and belongs to tenant
    entities::metrics::Entity::find_by_id(metric_id)
        .filter(entities::metrics::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Validate operator and level
    threshold::validate_threshold(&req.operator, &req.level).map_err(|e| {
        tracing::warn!(error = %e, "invalid threshold");
        StatusCode::BAD_REQUEST
    })?;

    let id = Uuid::now_v7();

    let model = entities::thresholds::ActiveModel {
        id: Set(id),
        insight_tenant_id: Set(ctx.insight_tenant_id),
        metric_id: Set(metric_id),
        field_name: Set(req.field_name),
        operator: Set(req.operator),
        value: Set(req.value),
        level: Set(req.level),
        created_at: NotSet,
        updated_at: NotSet,
    };

    entities::thresholds::Entity::insert(model)
        .exec(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to create threshold");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let row = entities::thresholds::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(model_to_threshold(row))))
}

pub async fn update_threshold(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((metric_id, tid)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateThresholdRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = entities::thresholds::Entity::find_by_id(tid)
        .filter(entities::thresholds::Column::MetricId.eq(metric_id))
        .filter(entities::thresholds::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut model: entities::thresholds::ActiveModel = existing.into();

    if let Some(field_name) = req.field_name {
        model.field_name = Set(field_name);
    }
    if let Some(operator) = req.operator {
        if !threshold::VALID_OPERATORS.contains(&operator.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        model.operator = Set(operator);
    }
    if let Some(value) = req.value {
        model.value = Set(value);
    }
    if let Some(level) = req.level {
        if !threshold::VALID_LEVELS.contains(&level.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        model.level = Set(level);
    }

    let updated = model.update(&state.db).await.map_err(|e| {
        tracing::error!(error = %e, "failed to update threshold");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(model_to_threshold(updated)))
}

pub async fn delete_threshold(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((metric_id, tid)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, StatusCode> {
    let existing = entities::thresholds::Entity::find_by_id(tid)
        .filter(entities::thresholds::Column::MetricId.eq(metric_id))
        .filter(entities::thresholds::Column::InsightTenantId.eq(ctx.insight_tenant_id))
        .one(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    entities::thresholds::Entity::delete_by_id(existing.id)
        .exec(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to delete threshold");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(StatusCode::NO_CONTENT)
}

// ── Columns ─────────────────────────────────────────────────

pub async fn list_columns(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
) -> Result<impl IntoResponse, StatusCode> {
    let columns = entities::table_columns::Entity::find()
        .filter(
            Condition::any()
                .add(entities::table_columns::Column::InsightTenantId.is_null())
                .add(entities::table_columns::Column::InsightTenantId.eq(ctx.insight_tenant_id)),
        )
        .all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to list columns");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items: Vec<TableColumn> = columns.into_iter().map(model_to_column).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

pub async fn list_columns_for_table(
    State(state): State<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(table): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let columns = entities::table_columns::Entity::find()
        .filter(entities::table_columns::Column::ClickhouseTable.eq(&table))
        .filter(
            Condition::any()
                .add(entities::table_columns::Column::InsightTenantId.is_null())
                .add(entities::table_columns::Column::InsightTenantId.eq(ctx.insight_tenant_id)),
        )
        .all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to list columns for table");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items: Vec<TableColumn> = columns.into_iter().map(model_to_column).collect();
    Ok(Json(serde_json::json!({ "items": items })))
}

// ── Mappers ─────────────────────────────────────────────────

fn model_to_metric(m: entities::metrics::Model) -> Metric {
    Metric {
        id: m.id,
        insight_tenant_id: m.insight_tenant_id,
        name: m.name,
        description: m.description,
        query_ref: m.query_ref,
        is_enabled: m.is_enabled,
        created_at: m.created_at.naive_utc(),
        updated_at: m.updated_at.naive_utc(),
    }
}

fn model_to_metric_summary(m: entities::metrics::Model) -> MetricSummary {
    MetricSummary {
        id: m.id,
        name: m.name,
        description: m.description,
    }
}

fn model_to_threshold(m: entities::thresholds::Model) -> Threshold {
    Threshold {
        id: m.id,
        insight_tenant_id: m.insight_tenant_id,
        metric_id: m.metric_id,
        field_name: m.field_name,
        operator: m.operator,
        value: m.value,
        level: m.level,
        created_at: m.created_at.naive_utc(),
        updated_at: m.updated_at.naive_utc(),
    }
}

fn model_to_column(m: entities::table_columns::Model) -> TableColumn {
    TableColumn {
        id: m.id,
        insight_tenant_id: m.insight_tenant_id,
        clickhouse_table: m.clickhouse_table,
        field_name: m.field_name,
        field_description: m.field_description,
    }
}
