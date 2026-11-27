//! Stateful CRUD collections (#153b) — a tiny in-memory REST backend behind
//! a `path` a mock declares.
//!
//! A mock with `"collection": {...}` configures one collection. The mock
//! store holds the *declaration*; this module holds the *rows* and serves
//! them: `GET` list, `POST` create (`201` + `Location`), `GET/PUT/PATCH/
//! DELETE` by id (`404` when absent). `POST /admin/reset` returns every
//! collection to its seed.
//!
//! Deliberately *not* a matcher: `handle_request` checks for a collection
//! before mock matching, because a collection isn't a mock shape at all —
//! it's a resource. A mock file that also declares the collection's paths
//! can still be hit for the paths the collection doesn't own (e.g. a
//! sub-path), but the five generated routes always win for their own paths.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::types::CollectionConfig;

/// Live row state per collection name. Lock discipline: one acquire per
/// operation, never across an await.
pub type CollectionStore = Arc<RwLock<HashMap<String, CollectionState>>>;

/// The mutable half of one collection: its rows and the next id to assign.
#[derive(Debug, Clone)]
pub struct CollectionState {
    /// Rows in insertion order — `GET` lists them in the order created,
    /// with the seed first.
    pub rows: Vec<Value>,
    /// The id of the last row created by the server, for assigning new ids
    /// after the seed's maximum.
    pub next_id: serde_json::Number,
}

impl CollectionState {
    /// Start from the config's seed, computing `next_id` past the seed's
    /// largest numeric id.
    pub fn from_seed(config: &CollectionConfig) -> Self {
        let mut state = CollectionState {
            rows: config.seed.clone(),
            next_id: serde_json::Number::from(0u64),
        };
        let max_seen = state
            .rows
            .iter()
            .filter_map(|row| row.get(&config.id_field))
            .filter_map(|id| id.as_u64())
            .max()
            .unwrap_or(0);
        state.next_id = serde_json::Number::from(max_seen);
        state
    }

    /// Assign a fresh id into `row[config.id_field]` when the row doesn't
    /// carry one, and record the row's id as the largest seen so far either
    /// way — a created row bringing `{"id": 50}` pushes the counter past 50.
    /// Returns the id actually stored.
    pub fn assign_id(&mut self, row: &mut Value, config: &CollectionConfig) -> Value {
        let existing = row.get(&config.id_field).cloned();
        match existing {
            Some(Value::Null) | None => {}
            Some(id) => {
                // Bring-your-own id: the counter must skip past it.
                if let Some(as_number) = id.as_u64() {
                    if as_number > self.next_id.as_u64().unwrap_or(0) {
                        self.next_id = serde_json::Number::from(as_number);
                    }
                }
                return id;
            }
        }
        let mut next = self.next_id.as_u64().unwrap_or(0).wrapping_add(1).max(1);
        while self
            .rows
            .iter()
            .any(|row| row_id_matches(row, &config.id_field, &next.to_string()))
        {
            next = next.wrapping_add(1).max(1);
        }
        self.next_id = serde_json::Number::from(next);
        let assigned = Value::Number(serde_json::Number::from(next));
        row.as_object_mut()
            .expect("created rows are JSON objects")
            .insert(config.id_field.clone(), assigned.clone());
        assigned
    }
}

/// Everything one request needs answered from a collection: which
/// collection's routes own this path (if any), and what the operation
/// returns.
pub struct CollectionResponse {
    pub status: u16,
    pub body: Value,
    pub location: Option<String>,
}

/// True when the collection rooted at `base` owns `request_path`: the base
/// itself or one segment deeper (`/users`, `/users/7`) — never `/users/7/x`.
pub fn collection_owns(base_path: &str, request_path: &str) -> bool {
    let base = base_path.trim_end_matches('/');
    let wanted = request_path.trim_end_matches('/');
    if wanted == base {
        return true;
    }
    wanted.starts_with(base)
        && wanted[base.len()..].starts_with('/')
        && !wanted[base.len() + 1..].contains('/')
}

/// True when `path` is either a collection base or `<base>/<id>` — owned.
/// (Used by the handler against each declared collection; the find lives in
/// the handler so the matched config is what names the collection in logs.)
///
/// Serve one request against one known collection: the five routes plus the
/// per-path 405. The write lock is held across the whole operation — rows
/// are small, and a read-modify-write must not interleave.
pub async fn answer_collection(
    store: &CollectionStore,
    config: &CollectionConfig,
    path: &str,
    method: &str,
    body: Option<&[u8]>,
) -> Option<CollectionResponse> {
    let base = config.path.trim_end_matches('/');
    let rest = path.trim_end_matches('/').strip_prefix(base)?;
    // The path is either the base itself (`/users`), or `<base>/<id>`.
    // Anything deeper (`/users/1/posts`) is none of the collection's business.
    let id_part = match rest {
        "" | "/" => None,
        other if !other.starts_with('/') => return None,
        other => {
            let id = &other[1..];
            if id.is_empty() || id.contains('/') {
                return None;
            }
            Some(urlencoding::decode(id).ok()?.into_owned())
        }
    };

    let not_found = || CollectionResponse {
        status: 404,
        body: json!({
            "error": "not found",
            "collection": config.name,
            "id": id_part,
        }),
        location: None,
    };

    let method = method.to_ascii_uppercase();
    let mut rows = store.write().await;

    // Make sure the collection exists in the store (lazy init off the seed).
    rows.entry(config.name.clone())
        .or_insert_with(|| CollectionState::from_seed(config));

    let state = rows.get_mut(&config.name)?;

    let operation = match (method.as_str(), id_part.is_some()) {
        ("GET", false) => config.operations.list.as_ref(),
        ("GET", true) => config.operations.get.as_ref(),
        ("POST", false) => config.operations.create.as_ref(),
        ("PUT", true) => config.operations.replace.as_ref(),
        ("PATCH", true) => config.operations.patch.as_ref(),
        ("DELETE", true) => config.operations.delete.as_ref(),
        _ => None,
    };
    let response = match (method.as_str(), id_part.as_deref()) {
        // GET list
        ("GET", None) => Some(CollectionResponse {
            status: 200,
            body: json!(state.rows),
            location: None,
        }),

        // POST create
        ("POST", None) => {
            let mut new_row = match parse_row(body, operation) {
                Ok(row) => row,
                Err(response) => return Some(response),
            };
            if let Some(id) = new_row.get(&config.id_field).filter(|id| !id.is_null()) {
                if !id.is_number() && !id.as_str().is_some_and(|id| !id.is_empty()) {
                    return Some(collection_error(
                        400,
                        "id must be a non-empty string or a number",
                    ));
                }
                let requested = id_text(id);
                if state
                    .rows
                    .iter()
                    .any(|row| row_id_matches(row, &config.id_field, &requested))
                {
                    return Some(collection_error(409, "id already exists"));
                }
            }
            let id = state.assign_id(&mut new_row, config);
            state.rows.push(new_row.clone());
            let id_string = urlencoding::encode(&id_text(&id)).into_owned();
            Some(CollectionResponse {
                status: 201,
                body: new_row,
                location: Some(format!("{}/{id_string}", config.path.trim_end_matches('/'))),
            })
        }

        // GET one
        ("GET", Some(id)) => state
            .rows
            .iter()
            .find(|row| row_id_matches(row, &config.id_field, id))
            .map(|row| CollectionResponse {
                status: 200,
                body: row.clone(),
                location: None,
            })
            .or_else(|| Some(not_found())),

        // PUT/PATCH replace-merge
        ("PUT" | "PATCH", Some(id)) => {
            let Some(position) = state
                .rows
                .iter()
                .position(|row| row_id_matches(row, &config.id_field, id))
            else {
                return Some(not_found());
            };
            let mut incoming = match parse_row(body, operation) {
                Ok(row) => row,
                Err(response) => return Some(response),
            };
            let existing_id = state.rows[position]
                .get(&config.id_field)
                .cloned()
                .unwrap_or(Value::Null);
            if incoming
                .get(&config.id_field)
                .is_some_and(|claimed| claimed != &existing_id)
            {
                return Some(CollectionResponse {
                    status: 409,
                    body: json!({"error": "id mismatch", "detail": "cannot change the resource id"}),
                    location: None,
                });
            }
            if method == "PUT" {
                incoming[&config.id_field] = existing_id;
                state.rows[position] = incoming;
            } else if let (Some(existing), Some(patch)) = (
                state.rows[position].as_object_mut(),
                incoming.as_object_mut(),
            ) {
                existing.append(patch);
            }
            Some(CollectionResponse {
                status: 200,
                body: state.rows[position].clone(),
                location: None,
            })
        }

        // DELETE
        ("DELETE", Some(id)) => {
            let Some(position) = state
                .rows
                .iter()
                .position(|row| row_id_matches(row, &config.id_field, id))
            else {
                return Some(not_found());
            };
            state.rows.remove(position);
            Some(CollectionResponse {
                status: 204,
                body: Value::Null,
                location: None,
            })
        }

        // A method the collection can't answer on the base path is a plain
        // 405, not a fall-through: the path IS the collection's.
        (_, None) | (_, Some(_)) => {
            let allowed = if id_part.is_some() {
                "GET, PUT, PATCH, DELETE"
            } else {
                "GET, POST"
            };
            Some(CollectionResponse {
                status: 405,
                body: json!({
                    "error": "method not allowed",
                    "collection": config.name,
                    "allow": allowed,
                }),
                location: None,
            })
        }
    };
    response.map(|mut response| {
        if (200..300).contains(&response.status) {
            if let Some(operation) = operation {
                response.status = operation.status.unwrap_or(response.status);
                if let Some(body) = &operation.response {
                    response.body = body.clone();
                }
            }
        }
        response
    })
}

fn parse_row(
    body: Option<&[u8]>,
    operation: Option<&crate::types::CollectionOperation>,
) -> Result<Value, CollectionResponse> {
    let value = match body.filter(|body| !body.is_empty()) {
        Some(body) => serde_json::from_slice(body)
            .map_err(|error| collection_error(400, &error.to_string()))?,
        None => json!({}),
    };
    let object = value
        .as_object()
        .ok_or_else(|| collection_error(400, "the body must be a JSON object"))?;
    if let Some(operation) = operation {
        for field in &operation.required_fields {
            if !object.contains_key(field) {
                return Err(collection_error(
                    400,
                    &format!("missing required field: {field}"),
                ));
            }
        }
    }
    Ok(value)
}

fn collection_error(status: u16, detail: &str) -> CollectionResponse {
    CollectionResponse {
        status,
        body: json!({"error": "invalid request", "detail": detail}),
        location: None,
    }
}

fn id_text(id: &Value) -> String {
    match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn row_id_matches(row: &Value, id_field: &str, requested: &str) -> bool {
    row.get(id_field).is_some_and(|id| id_text(id) == requested)
}

/// Return every live collection to its seed. Returns how many collections
/// were reset, for `/admin/reset`'s report.
pub async fn reset_collections(store: &CollectionStore, configs: &[&CollectionConfig]) -> usize {
    let mut rows = store.write().await;
    for config in configs {
        rows.insert(config.name.clone(), CollectionState::from_seed(config));
    }
    // Collections that no longer have a config (their mock was deleted)
    // stay in the map — they hold no live route anymore, and a stale row
    // is harmless — but they do not count as reset.
    rows.shrink_to_fit();
    configs.len()
}
