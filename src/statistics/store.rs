//! Independent, append-per-attempt statistics; request-log retention never deletes these rows.
use diesel::{connection::SimpleConnection, prelude::*};

use super::{
    CallContext,
    wire::{ListParams, Statistic},
};
use crate::{core::page::Page, db::Db, schema::statistics as s, util::unix_secs};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS statistics (
    id TEXT PRIMARY KEY NOT NULL, request_id TEXT NOT NULL,
    api_key TEXT, api_key_masked TEXT, requested_at INTEGER NOT NULL,
    started_at INTEGER NOT NULL, finished_at INTEGER, request_ip TEXT,
    account_email TEXT NOT NULL, requested_model TEXT NOT NULL,
    upstream_provider TEXT NOT NULL, upstream_model TEXT NOT NULL,
    requested_effort TEXT, requested_thinking_json TEXT, upstream_effort TEXT,
    call_type TEXT NOT NULL CHECK(call_type IN ('inference', 'compact')),
    status TEXT NOT NULL CHECK(status IN ('in_progress', 'completed', 'failed', 'cancelled', 'interrupted')),
    http_status INTEGER, error_message TEXT, usage_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_statistics_requested_at ON statistics(requested_at, id);
CREATE INDEX IF NOT EXISTS idx_statistics_request_id ON statistics(request_id);
CREATE INDEX IF NOT EXISTS idx_statistics_account ON statistics(account_email, requested_at);
CREATE INDEX IF NOT EXISTS idx_statistics_model ON statistics(requested_model, requested_at);
CREATE INDEX IF NOT EXISTS idx_statistics_upstream_model ON statistics(upstream_model, requested_at);
CREATE INDEX IF NOT EXISTS idx_statistics_key ON statistics(api_key, requested_at);
";

pub struct StatisticsStore {
    db: Db,
}

impl StatisticsStore {
    pub fn new(db: Db) -> Self {
        crate::db::blocking(|| db.lock().batch_execute(SCHEMA))
            .expect("failed to initialize statistics schema");
        Self { db }
    }

    pub(crate) fn start(&self, id: &str, call: &CallContext, email: &str) -> Result<(), String> {
        crate::db::blocking(|| {
            diesel::insert_into(s::table)
                .values((
                    s::id.eq(id),
                    s::request_id.eq(&call.request.request_id),
                    s::api_key.eq(&call.request.api_key),
                    s::api_key_masked.eq(&call.request.api_key_masked),
                    s::requested_at.eq(call.request.requested_at),
                    s::started_at.eq(unix_secs() as i64),
                    s::request_ip.eq(&call.request.request_ip),
                    s::account_email.eq(email),
                    s::requested_model.eq(&call.requested_model),
                    s::upstream_provider.eq(&call.upstream_provider),
                    s::upstream_model.eq(&call.upstream_model),
                    s::requested_effort.eq(&call.requested_effort),
                    s::requested_thinking_json.eq(&call.requested_thinking_json),
                    s::upstream_effort.eq(&call.upstream_effort),
                    s::call_type.eq(call.call_type),
                    s::status.eq("in_progress"),
                ))
                .execute(&mut *self.db.lock())
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    pub(crate) fn finish(
        &self,
        id: &str,
        status: &str,
        http_status: Option<i32>,
        error: Option<&str>,
        usage: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        crate::db::blocking(|| {
            let usage = usage.map(serde_json::Value::to_string);
            diesel::update(
                s::table
                    .filter(s::id.eq(id))
                    .filter(s::finished_at.is_null()),
            )
            .set((
                s::status.eq(status),
                s::finished_at.eq(unix_secs() as i64),
                s::http_status.eq(http_status),
                s::error_message.eq(error),
                s::usage_json.eq(usage),
            ))
            .execute(&mut *self.db.lock())
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
    }

    /// Called once during startup, before serving traffic after an unclean shutdown.
    pub fn interrupt_unfinished(&self) -> Result<usize, String> {
        crate::db::blocking(|| {
            diesel::update(s::table.filter(s::finished_at.is_null()))
                .set((
                    s::status.eq("interrupted"),
                    s::finished_at.eq(unix_secs() as i64),
                    s::error_message.eq("gateway restarted before this attempt finished"),
                ))
                .execute(&mut *self.db.lock())
                .map_err(|e| e.to_string())
        })
    }

    pub fn detail(&self, id: &str) -> Result<Option<Statistic>, String> {
        crate::db::blocking(|| {
            s::table
                .find(id)
                .select(Statistic::as_select())
                .first(&mut *self.db.lock())
                .optional()
                .map_err(|e| e.to_string())
        })
    }

    pub fn list(&self, params: &ListParams) -> Result<Page<Statistic>, String> {
        let (page, size) = params.pagination()?;
        crate::db::blocking(|| {
            let filtered = || {
                let mut query = s::table.into_boxed::<diesel::sqlite::Sqlite>();
                if let Some(from) = params.requested_at_from {
                    query = query.filter(s::requested_at.ge(from));
                }
                if let Some(to) = params.requested_at_to {
                    query = query.filter(s::requested_at.le(to));
                }
                if let Some(v) = &params.request_id {
                    query = query.filter(s::request_id.eq(v));
                }
                if let Some(v) = &params.account_email {
                    query = query.filter(s::account_email.eq(v));
                }
                if let Some(v) = &params.requested_model {
                    query = query.filter(s::requested_model.eq(v));
                }
                if let Some(v) = &params.upstream_model {
                    query = query.filter(s::upstream_model.eq(v));
                }
                if let Some(v) = &params.api_key {
                    query = query.filter(s::api_key.eq(v));
                }
                if let Some(v) = &params.call_type {
                    query = query.filter(s::call_type.eq(v));
                }
                if let Some(v) = &params.status {
                    query = query.filter(s::status.eq(v));
                }
                query
            };
            let mut conn = self.db.lock();
            conn.transaction::<_, diesel::result::Error, _>(|conn| {
                let total = filtered().count().get_result::<i64>(conn)?;
                let items = filtered()
                    .order((s::started_at.desc(), s::id.desc()))
                    .limit(i64::from(size))
                    .offset(i64::from(page - 1) * i64::from(size))
                    .select(Statistic::as_select())
                    .load(conn)?;
                Ok(Page::new(items, total as u64, page, size))
            })
            .map_err(|e| e.to_string())
        })
    }
}
