use super::project_resolution::ResolvedProject;
use super::{ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use crate::db::{
    canonicalize_memory_tags, memory_catalog_revision, valid_memory_catalog_revision,
    validate_memory_query, validate_memory_scope_id, MemoryPrincipalAttribution, MemoryPriority,
    MemoryScopeAttribution, MemorySetInput, MemoryStoreError, ProjectMemoryRecord,
    ProjectMemoryScopeRecord, MAX_MEMORIES_GLOBAL, MAX_MEMORY_BOOTSTRAP_BYTES,
    MAX_MEMORY_SCOPE_LIST_LIMIT, MAX_MEMORY_SEARCH_LIMIT, MAX_MEMORY_SEARCH_RESULT_BYTES,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const DEFAULT_MEMORY_SEARCH_LIMIT: usize = 20;
/// Independent cap for lifecycle inventory projection. Project summaries are
/// separately capped by the global Memory row ceiling below; exceeding either
/// bound makes status unknown rather than turning a partial view into deletion
/// authority.
const MAX_MEMORY_SCOPE_INVENTORY_CLIENTS: usize = 1_024;

fn memory_hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn memory_scope_id_from_parts(project_runtime_id: &str, client_id: &str, root: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-project-memory-scope-v1\0");
    for value in [
        project_runtime_id.as_bytes(),
        client_id.as_bytes(),
        root.as_bytes(),
    ] {
        memory_hash_field(&mut hasher, value);
    }
    format!("wc_memscope_{:x}", hasher.finalize())
}

pub(crate) fn memory_scope_id(project: &ResolvedProject) -> String {
    memory_scope_id_from_parts(
        &project.resolved_id,
        &project.config.client_id,
        &project.config.path,
    )
}

pub(crate) fn memory_root_fingerprint(root: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-project-memory-root-v1\0");
    memory_hash_field(&mut hasher, root.as_bytes());
    format!("wc_memroot_{:x}", hasher.finalize())
}

fn memory_scope_attribution(project: &ResolvedProject) -> MemoryScopeAttribution {
    MemoryScopeAttribution {
        project_runtime_id: project.resolved_id.clone(),
        runner_client_id: project.config.client_id.clone(),
        root_fingerprint: memory_root_fingerprint(&project.config.path),
    }
}

pub(crate) fn memory_principal_attribution(
    auth: Option<&AuthContext>,
) -> Result<MemoryPrincipalAttribution, &'static str> {
    let (kind, stable_principal_id) = super::session_context::runtime_observation_principal(auth)
        .map_err(|_| "memory_principal_unavailable")?;
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-project-memory-principal-v1\0");
    memory_hash_field(&mut hasher, kind.as_bytes());
    memory_hash_field(&mut hasher, stable_principal_id.as_bytes());
    Ok(MemoryPrincipalAttribution {
        kind,
        principal_digest: format!("wc_memprincipal_{:x}", hasher.finalize()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryScopeCurrentStatus {
    Current,
    NotCurrent,
    Unknown,
}

impl MemoryScopeCurrentStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::NotCurrent => "not_current",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Default)]
struct MemoryInventoryObservation {
    current_projects: BTreeMap<String, String>,
    client_inventory_complete: BTreeMap<String, bool>,
}

fn memory_inventory_observation(
    views: Option<&[crate::runner_http::RunnerSemanticView]>,
) -> MemoryInventoryObservation {
    let Some(views) = views else {
        return MemoryInventoryObservation::default();
    };
    let mut current_projects = BTreeMap::new();
    let mut client_inventory_complete = BTreeMap::new();
    for semantic in views {
        let client_id = &semantic.view.client_id;
        let complete = semantic
            .view
            .project_inventory
            .as_ref()
            .is_some_and(|status| status.sync_state == "complete");
        client_inventory_complete.insert(client_id.clone(), complete);
        for project in &semantic.view.projects {
            let runtime_id =
                super::project_resolution::runner_project_runtime_id(client_id, &project.id);
            let scope = memory_scope_id_from_parts(&runtime_id, client_id, &project.path);
            if current_projects.insert(scope, runtime_id).is_some() {
                // A domain collision or duplicated authoritative identity makes
                // negative lifecycle conclusions unsafe for every affected scope.
                client_inventory_complete.insert(client_id.clone(), false);
            }
        }
    }
    MemoryInventoryObservation {
        current_projects,
        client_inventory_complete,
    }
}

fn memory_scope_current_status(
    scope: &ProjectMemoryScopeRecord,
    inventory: &MemoryInventoryObservation,
) -> MemoryScopeCurrentStatus {
    if inventory
        .current_projects
        .contains_key(&scope.memory_scope_id)
    {
        return MemoryScopeCurrentStatus::Current;
    }
    let Some(client_id) = scope.runner_client_id.as_deref() else {
        // Current persisted scopes are always attributed. Keep this defensive
        // projection fail-closed if malformed in-memory state reaches it.
        return MemoryScopeCurrentStatus::Unknown;
    };
    if inventory.client_inventory_complete.get(client_id) == Some(&true) {
        MemoryScopeCurrentStatus::NotCurrent
    } else {
        // Missing Runner records, pending/degraded inventory, and Server restart
        // gaps are not evidence of permanent detachment.
        MemoryScopeCurrentStatus::Unknown
    }
}

fn memory_lifecycle_error(kind: &str, extra: Value) -> ToolResult {
    let mut output = json!({
        "error_kind": kind,
        "state_changed": false,
    });
    if let (Some(target), Some(extra)) = (output.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    ToolResult::err_with_output(kind.to_string(), output)
}

fn memory_lifecycle_store_error(error: MemoryStoreError) -> ToolResult {
    let mut extra = json!({});
    if let (Some(revision), Some(object)) =
        (error.current_catalog_revision(), extra.as_object_mut())
    {
        object.insert("current_catalog_revision".to_string(), json!(revision));
    }
    memory_lifecycle_error(error.code(), extra)
}

fn memory_descriptor(
    record: &ProjectMemoryRecord,
    matched_fields: Option<Vec<&'static str>>,
) -> Value {
    let mut value = json!({
        "memory_id": record.memory_id,
        "memory_key": record.memory_key,
        "summary": record.summary,
        "priority": record.priority.as_str(),
        "bootstrap": record.bootstrap,
        "tags": record.tags,
        "revision": record.revision,
    });
    if let (Some(fields), Some(object)) = (matched_fields, value.as_object_mut()) {
        object.insert("matched_fields".to_string(), json!(fields));
    }
    value
}

fn query_matches(record: &ProjectMemoryRecord, query: &str) -> Option<Vec<&'static str>> {
    let query = query.trim();
    if query.is_empty() {
        return Some(Vec::new());
    }
    let query = query.to_lowercase();
    let mut fields = Vec::new();
    if record.memory_key.to_lowercase().contains(&query) {
        fields.push("memory_key");
    }
    if record.summary.to_lowercase().contains(&query) {
        fields.push("summary");
    }
    if record.body.to_lowercase().contains(&query) {
        fields.push("body");
    }
    if record
        .tags
        .iter()
        .any(|tag| tag.to_lowercase().contains(&query))
    {
        fields.push("tags");
    }
    (!fields.is_empty()).then_some(fields)
}

fn memory_error(project: &ResolvedProject, error: MemoryStoreError) -> ToolResult {
    let mut output = json!({
        "project": project.resolved_id,
        "error_kind": error.code(),
        "state_changed": false,
    });
    if let (Some(revision), Some(object)) = (error.current_revision(), output.as_object_mut()) {
        object.insert("current_revision".to_string(), json!(revision));
    }
    ToolResult::err_with_output(error.code().to_string(), output)
}

fn memory_simple_error(project: &ResolvedProject, kind: &str, extra: Value) -> ToolResult {
    let mut output = json!({
        "project": project.resolved_id,
        "error_kind": kind,
        "state_changed": false,
    });
    if let (Some(target), Some(extra)) = (output.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    ToolResult::err_with_output(kind.to_string(), output)
}

impl ToolRuntime {
    fn memory_database(&self) -> Result<&crate::Database, &'static str> {
        self.memory_db.as_deref().ok_or("memory_store_unavailable")
    }

    pub(crate) fn memory_search(
        &self,
        project: &ResolvedProject,
        query: Option<String>,
        tags: Option<Vec<String>>,
        offset: Option<usize>,
        limit: Option<usize>,
        expected_catalog_revision: Option<String>,
    ) -> ToolResult {
        let query = query.unwrap_or_default();
        if let Err(error) = validate_memory_query(&query) {
            return memory_error(project, error);
        }
        let tags = match canonicalize_memory_tags(tags.unwrap_or_default()) {
            Ok(tags) => tags,
            Err(error) => return memory_error(project, error),
        };
        let limit = limit.unwrap_or(DEFAULT_MEMORY_SEARCH_LIMIT);
        if !(1..=MAX_MEMORY_SEARCH_LIMIT).contains(&limit) {
            return memory_error(project, MemoryStoreError::InvalidLimit);
        }
        let Some(db) = self.memory_db.as_deref() else {
            return memory_simple_error(project, "memory_store_unavailable", json!({}));
        };
        let scope = memory_scope_id(project);
        let records = match db.list_project_memories(&scope) {
            Ok(records) => records,
            Err(error) => return memory_error(project, error),
        };
        let catalog_revision = memory_catalog_revision(&records);
        if let Some(expected) = expected_catalog_revision.as_deref() {
            if !valid_memory_catalog_revision(expected) {
                return memory_simple_error(
                    project,
                    "memory_catalog_revision_invalid",
                    json!({"catalog_revision": catalog_revision}),
                );
            }
            if expected != catalog_revision {
                return memory_simple_error(
                    project,
                    "memory_catalog_changed",
                    json!({"catalog_revision": catalog_revision}),
                );
            }
        }

        let query_present = !query.trim().is_empty();
        let mut matches = records
            .iter()
            .filter(|record| {
                tags.iter()
                    .all(|tag| record.tags.iter().any(|value| value == tag))
            })
            .filter_map(|record| {
                query_matches(record, &query)
                    .map(|fields| memory_descriptor(record, query_present.then_some(fields)))
            })
            .collect::<Vec<_>>();
        let total_count = matches.len();
        let offset = offset.unwrap_or(0).min(total_count);
        let mut returned = Vec::new();
        for descriptor in matches.drain(offset..).take(limit) {
            let mut candidate = returned.clone();
            candidate.push(descriptor.clone());
            let probe = json!({
                "project": project.resolved_id,
                "catalog_revision": catalog_revision,
                "total_count": total_count,
                "returned_count": candidate.len(),
                "offset": offset,
                "next_offset": offset + candidate.len(),
                "truncated": true,
                "memories": candidate,
            });
            if serde_json::to_vec(&probe)
                .map(|bytes| bytes.len() <= MAX_MEMORY_SEARCH_RESULT_BYTES)
                .unwrap_or(false)
            {
                returned.push(descriptor);
            } else {
                break;
            }
        }
        let next = offset + returned.len();
        let truncated = next < total_count;
        ToolResult::ok(json!({
            "project": project.resolved_id,
            "catalog_revision": catalog_revision,
            "total_count": total_count,
            "returned_count": returned.len(),
            "offset": offset,
            "next_offset": truncated.then_some(next),
            "truncated": truncated,
            "memories": returned,
        }))
    }

    pub(crate) fn memory_read(
        &self,
        project: &ResolvedProject,
        memory_key: String,
        expected_revision: Option<String>,
    ) -> ToolResult {
        let Some(db) = self.memory_db.as_deref() else {
            return memory_simple_error(project, "memory_store_unavailable", json!({}));
        };
        let scope = memory_scope_id(project);
        let record = match db.get_project_memory(&scope, &memory_key) {
            Ok(Some(record)) => record,
            Ok(None) => return memory_error(project, MemoryStoreError::NotFound),
            Err(error) => return memory_error(project, error),
        };
        if let Some(expected) = expected_revision.as_deref() {
            if crate::db::validate_memory_revision(expected).is_err() {
                return memory_error(project, MemoryStoreError::InvalidRevision);
            }
            if expected != record.revision {
                return memory_simple_error(
                    project,
                    "memory_changed",
                    json!({"memory_key": record.memory_key, "current_revision": record.revision}),
                );
            }
        }
        ToolResult::ok(json!({
            "project": project.resolved_id,
            "memory_id": record.memory_id,
            "memory_key": record.memory_key,
            "summary": record.summary,
            "body": record.body,
            "priority": record.priority.as_str(),
            "bootstrap": record.bootstrap,
            "tags": record.tags,
            "revision": record.revision,
            "created_at_unix_ms": record.created_at_unix_ms,
            "updated_at_unix_ms": record.updated_at_unix_ms,
            "provenance": {
                "created_by_kind": record.created_by_kind,
                "updated_by_kind": record.updated_by_kind,
            },
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn memory_set(
        &self,
        project: &ResolvedProject,
        memory_key: String,
        summary: String,
        body: Option<String>,
        priority: Option<String>,
        bootstrap: Option<bool>,
        tags: Option<Vec<String>>,
        expected_revision: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let Some(db) = self.memory_db.as_deref() else {
            return memory_simple_error(project, "memory_store_unavailable", json!({}));
        };
        let scope = memory_scope_id(project);
        let scope_attribution = memory_scope_attribution(project);
        let principal = match memory_principal_attribution(auth) {
            Ok(principal) => principal,
            Err(kind) => return memory_simple_error(project, kind, json!({})),
        };
        // memory_set is a full canonical definition update for the required
        // summary. Omitted optional fields preserve current values only on an
        // explicit CAS update; without expected_revision they retain create
        // defaults even if the key now exists. That keeps a response-lost create
        // retry from silently absorbing a concurrent optional-field update. The
        // subsequent DB transaction still owns the authoritative CAS, so this
        // pre-read cannot turn a concurrent update into last-write-wins.
        let preserve_optional_fields = expected_revision.is_some();
        let existing = match db.get_project_memory(&scope, &memory_key) {
            Ok(existing) => existing,
            Err(error) => return memory_error(project, error),
        };
        let priority = match priority {
            Some(value) => match MemoryPriority::parse(&value) {
                Ok(priority) => priority,
                Err(error) => return memory_error(project, error),
            },
            None if preserve_optional_fields => existing
                .as_ref()
                .map(|record| record.priority)
                .unwrap_or(MemoryPriority::Normal),
            None => MemoryPriority::Normal,
        };
        let body = match body {
            Some(body) => body,
            None if preserve_optional_fields => existing
                .as_ref()
                .map(|record| record.body.clone())
                .unwrap_or_default(),
            None => String::new(),
        };
        let bootstrap = match bootstrap {
            Some(bootstrap) => bootstrap,
            None if preserve_optional_fields => existing
                .as_ref()
                .map(|record| record.bootstrap)
                .unwrap_or(false),
            None => false,
        };
        let tags = match tags {
            Some(tags) => tags,
            None if preserve_optional_fields => existing
                .as_ref()
                .map(|record| record.tags.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let outcome = match db.set_project_memory_attributed(
            &scope,
            &scope_attribution,
            &principal,
            MemorySetInput {
                memory_key,
                summary,
                body,
                priority,
                bootstrap,
                tags,
                expected_revision,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => return memory_error(project, error),
        };
        ToolResult::ok(json!({
            "project": project.resolved_id,
            "memory_id": outcome.record.memory_id,
            "memory_key": outcome.record.memory_key,
            "old_revision": outcome.old_revision,
            "revision": outcome.record.revision,
            "created": outcome.created,
            "state_changed": outcome.state_changed,
        }))
    }

    pub(crate) fn memory_delete(
        &self,
        project: &ResolvedProject,
        memory_key: String,
        expected_revision: String,
    ) -> ToolResult {
        let Some(db) = self.memory_db.as_deref() else {
            return memory_simple_error(project, "memory_store_unavailable", json!({}));
        };
        let scope = memory_scope_id(project);
        let scope_attribution = memory_scope_attribution(project);
        let outcome = match db.delete_project_memory_attributed(
            &scope,
            &scope_attribution,
            &memory_key,
            &expected_revision,
        ) {
            Ok(outcome) => outcome,
            Err(error) => return memory_error(project, error),
        };
        ToolResult::ok(json!({
            "project": project.resolved_id,
            "memory_id": outcome.memory_id,
            "memory_key": memory_key,
            "revision": outcome.revision,
            "deleted": outcome.deleted,
            "state_changed": outcome.state_changed,
        }))
    }

    pub(crate) async fn memory_scope_list(
        &self,
        auth: Option<&AuthContext>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> ToolResult {
        let Some(db) = self.memory_db.as_deref() else {
            return memory_lifecycle_error("memory_store_unavailable", json!({}));
        };
        let offset = offset.unwrap_or(0);
        let limit = limit.unwrap_or(50);
        if !(1..=MAX_MEMORY_SCOPE_LIST_LIMIT).contains(&limit) {
            return memory_lifecycle_store_error(MemoryStoreError::InvalidLimit);
        }
        let (total_count, snapshots) = match db.list_project_memory_scopes(offset, limit) {
            Ok(value) => value,
            Err(error) => return memory_lifecycle_store_error(error),
        };
        let views = self
            .runner_registry
            .list_bounded_runner_semantic_views_for_auth(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                MAX_MEMORY_SCOPE_INVENTORY_CLIENTS,
                MAX_MEMORIES_GLOBAL,
            )
            .await;
        let inventory = memory_inventory_observation(views.as_deref());
        let normalized_offset = offset.min(total_count);
        let mut scopes = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let status = memory_scope_current_status(&snapshot.scope, &inventory);
            let catalog_revision = memory_catalog_revision(&snapshot.memories);
            let memory_count = snapshot.memories.len();
            let bootstrap_count = snapshot
                .memories
                .iter()
                .filter(|memory| memory.bootstrap)
                .count();
            let oldest_memory_created_at_unix_ms = snapshot
                .memories
                .iter()
                .map(|memory| memory.created_at_unix_ms)
                .min()
                .expect("scope snapshots are non-empty");
            let latest_memory_updated_at_unix_ms = snapshot
                .memories
                .iter()
                .map(|memory| memory.updated_at_unix_ms)
                .max()
                .expect("scope snapshots are non-empty");
            let current_project_runtime_id = inventory
                .current_projects
                .get(&snapshot.scope.memory_scope_id)
                .cloned();
            scopes.push(json!({
                "memory_scope_id": snapshot.scope.memory_scope_id,
                "identity_state": snapshot.scope.identity_state,
                "current_status": status.as_str(),
                "project_runtime_id": snapshot.scope.project_runtime_id,
                "runner_client_id": snapshot.scope.runner_client_id,
                "root_fingerprint": snapshot.scope.root_fingerprint,
                "current_project_runtime_id": current_project_runtime_id,
                "memory_count": memory_count,
                "bootstrap_count": bootstrap_count,
                "catalog_revision": catalog_revision,
                "oldest_memory_created_at_unix_ms": oldest_memory_created_at_unix_ms,
                "latest_memory_updated_at_unix_ms": latest_memory_updated_at_unix_ms,
                "scope_created_at_unix_ms": snapshot.scope.created_at_unix_ms,
                "scope_last_mutated_at_unix_ms": snapshot.scope.last_mutated_at_unix_ms,
            }));
        }
        let next = normalized_offset + scopes.len();
        let truncated = next < total_count;
        ToolResult::ok(json!({
            "total_count": total_count,
            "returned_count": scopes.len(),
            "offset": normalized_offset,
            "next_offset": truncated.then_some(next),
            "truncated": truncated,
            "scopes": scopes,
        }))
    }

    pub(crate) async fn memory_scope_purge(
        &self,
        auth: Option<&AuthContext>,
        memory_scope_id: String,
        expected_catalog_revision: String,
        confirm: bool,
    ) -> ToolResult {
        if !confirm {
            return memory_lifecycle_error(
                "memory_scope_confirmation_required",
                json!({"memory_scope_id": memory_scope_id}),
            );
        }
        if let Err(error) = validate_memory_scope_id(&memory_scope_id) {
            return memory_lifecycle_store_error(error);
        }
        if !valid_memory_catalog_revision(&expected_catalog_revision) {
            return memory_lifecycle_error(
                "memory_catalog_revision_invalid",
                json!({"memory_scope_id": memory_scope_id}),
            );
        }
        let Some(db) = self.memory_db.as_deref() else {
            return memory_lifecycle_error("memory_store_unavailable", json!({}));
        };
        let scope_record = match db.get_project_memory_scope(&memory_scope_id) {
            Ok(None) => {
                return ToolResult::ok(json!({
                    "memory_scope_id": memory_scope_id,
                    "catalog_revision": Value::Null,
                    "purged_count": 0,
                    "purged": false,
                    "state_changed": false,
                }))
            }
            Ok(Some(scope)) => scope,
            Err(error) => return memory_lifecycle_store_error(error),
        };

        self.runner_registry
            .with_bounded_runner_semantic_views_for_auth_locked(
                crate::runner_http::runner_access_from_auth(auth).as_ref(),
                MAX_MEMORY_SCOPE_INVENTORY_CLIENTS,
                MAX_MEMORIES_GLOBAL,
                |views| {
                    let inventory = memory_inventory_observation(views.as_deref());
                    match memory_scope_current_status(&scope_record.scope, &inventory) {
                        MemoryScopeCurrentStatus::Current => {
                            return memory_lifecycle_error(
                                "memory_scope_current",
                                json!({"memory_scope_id": memory_scope_id}),
                            )
                        }
                        MemoryScopeCurrentStatus::Unknown => {
                            return memory_lifecycle_error(
                                "memory_scope_status_unknown",
                                json!({"memory_scope_id": memory_scope_id}),
                            )
                        }
                        MemoryScopeCurrentStatus::NotCurrent => {}
                    }
                    match db
                        .purge_project_memory_scope(&memory_scope_id, &expected_catalog_revision)
                    {
                        Ok(outcome) => ToolResult::ok(json!({
                            "memory_scope_id": outcome.memory_scope_id,
                            "catalog_revision": outcome.catalog_revision,
                            "purged_count": outcome.purged_count,
                            "purged": outcome.purged,
                            "state_changed": outcome.state_changed,
                        })),
                        Err(error) => memory_lifecycle_store_error(error),
                    }
                },
            )
            .await
    }

    pub(crate) fn memory_bootstrap_context_projection(
        &self,
        project: &ResolvedProject,
    ) -> Result<Value, &'static str> {
        let db = self.memory_database()?;
        let scope = memory_scope_id(project);
        let records = db
            .list_project_memories(&scope)
            .map_err(|_| "memory_store_unavailable")?;
        let catalog_revision = memory_catalog_revision(&records);
        let mut bootstrap = records
            .iter()
            .filter(|record| record.bootstrap)
            .collect::<Vec<_>>();
        bootstrap.sort_by(|a, b| {
            a.priority
                .bootstrap_rank()
                .cmp(&b.priority.bootstrap_rank())
                .then_with(|| a.memory_key.cmp(&b.memory_key))
        });
        let total_count = bootstrap.len();
        let mut memories = Vec::new();
        for record in bootstrap {
            let descriptor = memory_descriptor(record, None);
            let mut candidate = memories.clone();
            candidate.push(descriptor.clone());
            let projection = json!({
                "project": project.resolved_id,
                "catalog_revision": catalog_revision,
                "total_count": total_count,
                "returned_count": candidate.len(),
                "truncated": candidate.len() < total_count,
                "memories": candidate,
            });
            if serde_json::to_vec(&projection)
                .map(|bytes| bytes.len() <= MAX_MEMORY_BOOTSTRAP_BYTES)
                .unwrap_or(false)
            {
                memories.push(descriptor);
            } else {
                break;
            }
        }
        let truncated = memories.len() < total_count;
        let projection = json!({
            "project": project.resolved_id,
            "catalog_revision": catalog_revision,
            "total_count": total_count,
            "returned_count": memories.len(),
            "truncated": truncated,
            "memories": memories,
        });
        debug_assert!(
            serde_json::to_vec(&projection)
                .map(|bytes| bytes.len() <= MAX_MEMORY_BOOTSTRAP_BYTES)
                .unwrap_or(false),
            "memory.bootstrap projection must remain independently bounded"
        );
        Ok(projection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_identity_changes_with_runner_or_registered_root_without_exposure() {
        fn resolved(client: &str, path: &str) -> ResolvedProject {
            ResolvedProject {
                input: "demo".to_string(),
                resolved_id: format!("agent:{client}:demo"),
                config: crate::projects::ProjectConfig {
                    path: path.to_string(),
                    client_id: client.to_string(),
                    allow_patch: true,
                },
            }
        }
        let a = memory_scope_id(&resolved("runner", "/registered/a"));
        let b = memory_scope_id(&resolved("runner", "/registered/b"));
        let c = memory_scope_id(&resolved("other", "/registered/a"));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("wc_memscope_"));
        assert!(!a.contains("registered"));
        assert!(!a.contains("runner"));
    }

    #[test]
    fn incomplete_bounded_inventory_never_proves_not_current() {
        let attributed = ProjectMemoryScopeRecord {
            memory_scope_id: format!("wc_memscope_{}", "a".repeat(64)),
            identity_state: "attributed".to_string(),
            project_runtime_id: Some("agent:runner:demo".to_string()),
            runner_client_id: Some("runner".to_string()),
            root_fingerprint: Some(format!("wc_memroot_{}", "b".repeat(64))),
            created_at_unix_ms: 1,
            last_mutated_at_unix_ms: 1,
        };
        let incomplete = memory_inventory_observation(None);
        assert_eq!(
            memory_scope_current_status(&attributed, &incomplete),
            MemoryScopeCurrentStatus::Unknown
        );
    }

    #[test]
    fn body_match_is_discoverable_without_body_projection() {
        let record = ProjectMemoryRecord {
            memory_id: "wc_mem_0123456789abcdef0123456789abcdef".to_string(),
            memory_key: "policy".to_string(),
            summary: "release guidance".to_string(),
            body: "Use hidden canary phrase".to_string(),
            priority: MemoryPriority::Normal,
            bootstrap: false,
            tags: Vec::new(),
            definition_hash: format!("wc_memdef_{}", "a".repeat(64)),
            created_by_kind: "test".to_string(),
            created_by_principal_digest: Some(format!("wc_memprincipal_{}", "1".repeat(64))),
            updated_by_kind: "test".to_string(),
            updated_by_principal_digest: Some(format!("wc_memprincipal_{}", "1".repeat(64))),
            generation: 1,
            revision: format!("wc_memrev_{}", "a".repeat(64)),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        assert_eq!(query_matches(&record, "canary"), Some(vec!["body"]));
        let serialized = memory_descriptor(&record, Some(vec!["body"])).to_string();
        assert!(!serialized.contains("hidden canary phrase"));
        assert!(serialized.contains("release guidance"));
    }
}
