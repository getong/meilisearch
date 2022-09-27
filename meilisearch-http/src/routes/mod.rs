use std::collections::BTreeMap;

use actix_web::web::Data;
use actix_web::{web, HttpRequest, HttpResponse};
use index_scheduler::{IndexScheduler, Query, Status};
use log::debug;
use serde::{Deserialize, Serialize};

use serde_json::json;
use time::OffsetDateTime;

use index::{Settings, Unchecked};
use meilisearch_types::error::ResponseError;
use meilisearch_types::star_or::StarOr;

use crate::analytics::Analytics;
use crate::extractors::authentication::{policies::*, GuardedData};

use self::indexes::{IndexStats, IndexView};

mod api_key;
mod dump;
pub mod indexes;
mod tasks;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(web::scope("/tasks").configure(tasks::configure))
        .service(web::resource("/health").route(web::get().to(get_health)))
        .service(web::scope("/keys").configure(api_key::configure))
        .service(web::scope("/dumps").configure(dump::configure))
        .service(web::resource("/stats").route(web::get().to(get_stats)))
        .service(web::resource("/version").route(web::get().to(get_version)))
        .service(web::scope("/indexes").configure(indexes::configure));
}

/// Extracts the raw values from the `StarOr` types and
/// return None if a `StarOr::Star` is encountered.
pub fn fold_star_or<T, O>(content: impl IntoIterator<Item = StarOr<T>>) -> Option<O>
where
    O: FromIterator<T>,
{
    content
        .into_iter()
        .map(|value| match value {
            StarOr::Star => None,
            StarOr::Other(val) => Some(val),
        })
        .collect()
}

const PAGINATION_DEFAULT_LIMIT: fn() -> usize = || 20;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pagination {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "PAGINATION_DEFAULT_LIMIT")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaginationView<T> {
    pub results: Vec<T>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
}

impl Pagination {
    /// Given the full data to paginate, returns the selected section.
    pub fn auto_paginate_sized<T>(
        self,
        content: impl IntoIterator<Item = T> + ExactSizeIterator,
    ) -> PaginationView<T>
    where
        T: Serialize,
    {
        let total = content.len();
        let content: Vec<_> = content
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        self.format_with(total, content)
    }

    /// Given an iterator and the total number of elements, returns the selected section.
    pub fn auto_paginate_unsized<T>(
        self,
        total: usize,
        content: impl IntoIterator<Item = T>,
    ) -> PaginationView<T>
    where
        T: Serialize,
    {
        let content: Vec<_> = content
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .collect();
        self.format_with(total, content)
    }

    /// Given the data already paginated + the total number of elements, it stores
    /// everything in a [PaginationResult].
    pub fn format_with<T>(self, total: usize, results: Vec<T>) -> PaginationView<T>
    where
        T: Serialize,
    {
        PaginationView {
            results,
            offset: self.offset,
            limit: self.limit,
            total,
        }
    }
}

impl<T> PaginationView<T> {
    pub fn new(offset: usize, limit: usize, total: usize, results: Vec<T>) -> Self {
        Self {
            offset,
            limit,
            results,
            total,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "name")]
pub enum UpdateType {
    ClearAll,
    Customs,
    DocumentsAddition {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    DocumentsPartial {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    DocumentsDeletion {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    Settings {
        settings: Settings<Unchecked>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    pub duration: f64, // in seconds
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub processed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    pub error: ResponseError,
    pub duration: f64, // in seconds
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub processed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueuedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub started_processing_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum UpdateStatusResponse {
    Enqueued {
        #[serde(flatten)]
        content: EnqueuedUpdateResult,
    },
    Processing {
        #[serde(flatten)]
        content: EnqueuedUpdateResult,
    },
    Failed {
        #[serde(flatten)]
        content: FailedUpdateResult,
    },
    Processed {
        #[serde(flatten)]
        content: ProcessedUpdateResult,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexUpdateResponse {
    pub update_id: u64,
}

impl IndexUpdateResponse {
    pub fn with_id(update_id: u64) -> Self {
        Self { update_id }
    }
}

/// Always return a 200 with:
/// ```json
/// {
///     "status": "Meilisearch is running"
/// }
/// ```
pub async fn running() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "status": "Meilisearch is running" }))
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub database_size: u64,
    #[serde(serialize_with = "time::serde::rfc3339::option::serialize")]
    pub last_update: Option<OffsetDateTime>,
    pub indexes: BTreeMap<String, IndexStats>,
}

async fn get_stats(
    index_scheduler: GuardedData<ActionPolicy<{ actions::STATS_GET }>, Data<IndexScheduler>>,
    req: HttpRequest,
    analytics: web::Data<dyn Analytics>,
) -> Result<HttpResponse, ResponseError> {
    analytics.publish(
        "Stats Seen".to_string(),
        json!({ "per_index_uid": false }),
        Some(&req),
    );
    let search_rules = &index_scheduler.filters().search_rules;

    let mut last_task: Option<OffsetDateTime> = None;
    let mut indexes = BTreeMap::new();
    let mut database_size = 0;
    let processing_task = index_scheduler.get_tasks(
        Query::default()
            .with_status(Status::Processing)
            .with_limit(1),
    )?;
    let processing_index = processing_task
        .first()
        .and_then(|task| task.index_uid.clone());

    for index in index_scheduler.indexes()? {
        if !search_rules.is_index_authorized(&index.name) {
            continue;
        }

        database_size += index.size()?;

        let stats = IndexStats {
            number_of_documents: index.number_of_documents()?,
            is_indexing: processing_index
                .as_deref()
                .map_or(false, |index_name| index.name == index_name),
            field_distribution: index.field_distribution()?,
        };

        let updated_at = index.updated_at()?;
        last_task = last_task.map_or(Some(updated_at), |last| Some(last.max(updated_at)));

        indexes.insert(index.name.clone(), stats);
    }

    let stats = Stats {
        database_size,
        last_update: last_task,
        indexes,
    };

    debug!("returns: {:?}", stats);
    Ok(HttpResponse::Ok().json(stats))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionResponse {
    commit_sha: String,
    commit_date: String,
    pkg_version: String,
}

async fn get_version(
    _index_scheduler: GuardedData<ActionPolicy<{ actions::VERSION }>, Data<IndexScheduler>>,
) -> HttpResponse {
    let commit_sha = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    let commit_date = option_env!("VERGEN_GIT_COMMIT_TIMESTAMP").unwrap_or("unknown");

    HttpResponse::Ok().json(VersionResponse {
        commit_sha: commit_sha.to_string(),
        commit_date: commit_date.to_string(),
        pkg_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[derive(Serialize)]
struct KeysResponse {
    private: Option<String>,
    public: Option<String>,
}

pub async fn get_health() -> Result<HttpResponse, ResponseError> {
    Ok(HttpResponse::Ok().json(serde_json::json!({ "status": "available" })))
}
