//! src/account/store.rs
//! 账号存储:accounts 表的 CRUD、轮询选号与可用性管理。

use std::sync::atomic::{AtomicI64, Ordering};

use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::Text;
use diesel::sqlite::Sqlite;

use crate::account::wire::{AccountInfo, AccountRecord};
use crate::core::page::Page;
use crate::db::Db;
use crate::error::GatewayError;
use crate::schema::accounts;
use crate::util::unix_secs;

/// 一次 upsert 的三态结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
    Unchanged,
}

/// accounts 表的一行(含明文 token,仅在内部与 refresh 使用)。
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub email: String,
    pub tag: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub obtained_at: Option<String>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub has_pro_features: Option<bool>,
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    /// 试用账号每小时请求限额(仅非 Pro 号有)。
    pub trial_requests_per_hour: Option<f64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
    pub info_fetched_at: Option<String>,
    /// 限额冷却截止时间(unix 秒)。轮询跳过冷却中的账号,到点自动恢复;
    /// 与 401 的 `available=0` 摘除不同,冷却不摘号,只是暂时避开。
    pub cooldown_until: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = accounts)]
#[diesel(check_for_backend(Sqlite))]
struct StoredAccount {
    id: i64,
    email: String,
    tag: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    obtained_at: Option<String>,
    available: i32,
    unavailable_reason: Option<String>,
    has_pro_features: Option<i32>,
    tier: Option<String>,
    sub_status: Option<String>,
    trial_days_left: Option<i64>,
    current_period_end: Option<i64>,
    trial_requests_per_hour: Option<f64>,
    remaining_credits: Option<f64>,
    total_credits: Option<f64>,
    remaining_usd: Option<f64>,
    next_credits_at: Option<String>,
    info_fetched_at: Option<String>,
    cooldown_until: Option<i64>,
    created_at: String,
    updated_at: String,
}

impl From<StoredAccount> for AccountRow {
    fn from(row: StoredAccount) -> Self {
        Self {
            email: row.email,
            tag: row.tag,
            access_token: row.access_token,
            refresh_token: row.refresh_token,
            obtained_at: row.obtained_at,
            available: row.available != 0,
            unavailable_reason: row.unavailable_reason,
            has_pro_features: row.has_pro_features.map(|value| value != 0),
            tier: row.tier,
            sub_status: row.sub_status,
            trial_days_left: row.trial_days_left,
            current_period_end: row.current_period_end,
            trial_requests_per_hour: row.trial_requests_per_hour,
            remaining_credits: row.remaining_credits,
            total_credits: row.total_credits,
            remaining_usd: row.remaining_usd,
            next_credits_at: row.next_credits_at,
            info_fetched_at: row.info_fetched_at,
            cooldown_until: row.cooldown_until,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Insertable)]
#[diesel(table_name = accounts)]
struct NewAccount<'a> {
    email: &'a str,
    tag: Option<&'a str>,
    access_token: &'a str,
    refresh_token: Option<&'a str>,
    obtained_at: Option<&'a str>,
    user_json: &'a str,
    created_at: &'a str,
    updated_at: &'a str,
}

pub enum AccountList {
    All(Vec<AccountRow>),
    Page(Page<AccountRow>),
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    email               TEXT NOT NULL UNIQUE,
    tag                 TEXT,
    access_token        TEXT NOT NULL,
    refresh_token       TEXT,
    obtained_at         TEXT,
    user_json           TEXT,
    available           INTEGER NOT NULL DEFAULT 1,
    unavailable_reason  TEXT,
    has_pro_features    INTEGER,
    tier                TEXT,
    sub_status          TEXT,
    trial_days_left     INTEGER,
    current_period_end  INTEGER,
    trial_requests_per_hour REAL,
    remaining_credits   REAL,
    total_credits       REAL,
    remaining_usd       REAL,
    next_credits_at     TEXT,
    info_fetched_at     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
";

/// 既有库的增量迁移:旧 schema 没有 `cooldown_until` / `has_pro_features`
/// 列,`ALTER TABLE` 加列幂等不可行,用 `PRAGMA table_info` 判断。
fn migrate(conn: &mut diesel::sqlite::SqliteConnection) {
    for (column, ddl) in [
        (
            "cooldown_until",
            "ALTER TABLE accounts ADD COLUMN cooldown_until INTEGER;",
        ),
        (
            "has_pro_features",
            "ALTER TABLE accounts ADD COLUMN has_pro_features INTEGER;",
        ),
        (
            "trial_requests_per_hour",
            "ALTER TABLE accounts ADD COLUMN trial_requests_per_hour REAL;",
        ),
    ] {
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            count: i64,
        }
        let has = diesel::sql_query(
            "SELECT COUNT(*) AS count FROM pragma_table_info('accounts') WHERE name = ?",
        )
        .bind::<Text, _>(column)
        .get_result::<Count>(conn)
        .map(|row| row.count > 0)
        .unwrap_or(false);
        if !has {
            conn.batch_execute(ddl)
                .unwrap_or_else(|e| panic!("migrate accounts: add {column}: {e}"));
        }
    }
}

pub struct AccountStore {
    renewal_locks: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
    db: Db,
    /// 轮询游标:最后选中的行 id。取 id 更大的下一个可用行(没有则回卷),
    /// 这样摘除某账号后轮询会自然落到它的后继,而不是跳回头部。
    last_id: AtomicI64,
}

impl AccountStore {
    /// 构造并建表(幂等),并做既有库的增量迁移。
    pub fn new(db: Db) -> Self {
        crate::db::blocking(|| {
            {
                let mut conn = db.lock();
                conn.batch_execute(SCHEMA).expect("create accounts schema");
                migrate(&mut conn);
            }
            Self {
                db,
                last_id: AtomicI64::new(0),
                renewal_locks: Default::default(),
            }
        })
    }

    pub(crate) fn renewal_lock(&self, email: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.renewal_locks.lock().expect("renewal locks poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(email).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(email.to_owned(), std::sync::Arc::downgrade(&lock));
        lock
    }

    /// 按 email upsert;三态语义见 [`UpsertOutcome`]。
    pub fn upsert(&self, record: &AccountRecord) -> Result<UpsertOutcome, GatewayError> {
        crate::db::blocking(|| {
            let email = record.email().ok_or_else(|| {
                GatewayError::BadRequest("account entry has no user.email".into())
            })?;
            let user_json = serde_json::to_string(&record.user).unwrap_or_default();
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            let existing = accounts::table
                .filter(accounts::email.eq(&email))
                .select((accounts::access_token, accounts::refresh_token))
                .first::<(String, Option<String>)>(&mut *conn)
                .optional()
                .map_err(sql_err)?;

            match existing {
                None => {
                    diesel::insert_into(accounts::table)
                        .values(&NewAccount {
                            email: &email,
                            tag: record.tag.as_deref(),
                            access_token: &record.access_token,
                            refresh_token: record.refresh_token.as_deref(),
                            obtained_at: record.obtained_at.as_deref(),
                            user_json: &user_json,
                            created_at: &now,
                            updated_at: &now,
                        })
                        .execute(&mut *conn)
                        .map_err(sql_err)?;
                    Ok(UpsertOutcome::Inserted)
                }
                Some((access, refresh))
                    if access != record.access_token || refresh != record.refresh_token =>
                {
                    // token 变了:更新全部业务字段并恢复可用(新 token 可能已复活)。
                    diesel::update(accounts::table.filter(accounts::email.eq(&email)))
                        .set((
                            accounts::tag.eq(record.tag.as_deref()),
                            accounts::access_token.eq(&record.access_token),
                            accounts::refresh_token.eq(record.refresh_token.as_deref()),
                            accounts::obtained_at.eq(record.obtained_at.as_deref()),
                            accounts::user_json.eq(&user_json),
                            accounts::available.eq(1),
                            accounts::unavailable_reason.eq(None::<String>),
                            accounts::updated_at.eq(&now),
                        ))
                        .execute(&mut *conn)
                        .map_err(sql_err)?;
                    Ok(UpsertOutcome::Updated)
                }
                Some(_) => Ok(UpsertOutcome::Unchanged),
            }
        })
    }

    /// 全部账号(含不可用的;refresh 需要它们的 token)。
    pub fn list(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
        status: Option<&str>,
    ) -> Result<AccountList, GatewayError> {
        crate::db::blocking(|| {
            let mut conn = self.db.lock();
            let status = status.filter(|value| !value.is_empty() && *value != "all");
            if status
                .is_some_and(|value| !matches!(value, "available" | "cooldown" | "unavailable"))
            {
                return Err(GatewayError::BadRequest(
                    "status must be one of all, available, cooldown, unavailable".into(),
                ));
            }
            let now = unix_secs() as i64;
            let mut query = accounts::table.into_boxed::<Sqlite>();
            let mut count_query = accounts::table.into_boxed::<Sqlite>();
            match status {
                Some("available") => {
                    let filter = accounts::available.eq(1).and(
                        accounts::cooldown_until
                            .is_null()
                            .or(accounts::cooldown_until.le(now)),
                    );
                    query = query.filter(filter);
                    count_query = count_query.filter(filter);
                }
                Some("cooldown") => {
                    let filter = accounts::available
                        .eq(1)
                        .and(accounts::cooldown_until.gt(now));
                    query = query.filter(filter);
                    count_query = count_query.filter(filter);
                }
                Some("unavailable") => {
                    query = query.filter(accounts::available.eq(0));
                    count_query = count_query.filter(accounts::available.eq(0));
                }
                _ => {}
            }
            if let (Some(page), Some(page_size)) = (page, page_size) {
                let total = count_query
                    .count()
                    .get_result::<i64>(&mut *conn)
                    .map_err(sql_err)? as u64;
                let rows = query
                    .order(accounts::id.asc())
                    .limit(i64::from(page_size))
                    .offset(i64::from(page - 1) * i64::from(page_size))
                    .select(StoredAccount::as_select())
                    .load::<StoredAccount>(&mut *conn)
                    .map_err(sql_err)?
                    .into_iter()
                    .map(AccountRow::from)
                    .collect();
                Ok(AccountList::Page(Page::new(rows, total, page, page_size)))
            } else {
                let rows = query
                    .order(accounts::id.asc())
                    .select(StoredAccount::as_select())
                    .load::<StoredAccount>(&mut *conn)
                    .map_err(sql_err)?
                    .into_iter()
                    .map(AccountRow::from)
                    .collect();
                Ok(AccountList::All(rows))
            }
        })
    }

    pub fn counts(&self) -> Result<(u64, u64, u64, u64), GatewayError> {
        crate::db::blocking(|| {
            let now = unix_secs() as i64;
            let mut conn = self.db.lock();
            let total = accounts::table
                .count()
                .get_result::<i64>(&mut *conn)
                .map_err(sql_err)? as u64;
            let available = accounts::table
                .filter(
                    accounts::available.eq(1).and(
                        accounts::cooldown_until
                            .is_null()
                            .or(accounts::cooldown_until.le(now)),
                    ),
                )
                .count()
                .get_result::<i64>(&mut *conn)
                .map_err(sql_err)? as u64;
            let cooldown = accounts::table
                .filter(
                    accounts::available
                        .eq(1)
                        .and(accounts::cooldown_until.gt(now)),
                )
                .count()
                .get_result::<i64>(&mut *conn)
                .map_err(sql_err)? as u64;
            let unavailable = accounts::table
                .filter(accounts::available.eq(0))
                .count()
                .get_result::<i64>(&mut *conn)
                .map_err(sql_err)? as u64;
            Ok((total, available, cooldown, unavailable))
        })
    }

    /// 导出可直接提交给 POST /v1/accounts/create 的原始账号凭据。
    pub fn export_records(&self) -> Result<Vec<AccountRecord>, GatewayError> {
        crate::db::blocking(|| {
            let mut conn = self.db.lock();
            accounts::table
                .select((
                    accounts::user_json,
                    accounts::tag,
                    accounts::access_token,
                    accounts::refresh_token,
                    accounts::obtained_at,
                ))
                .order(accounts::id.asc())
                .load::<(
                    Option<String>,
                    Option<String>,
                    String,
                    Option<String>,
                    Option<String>,
                )>(&mut *conn)
                .map_err(sql_err)?
                .into_iter()
                .map(
                    |(user_json, tag, access_token, refresh_token, obtained_at)| {
                        let user = serde_json::from_str(user_json.as_deref().unwrap_or("{}"))
                            .map_err(|e| {
                                GatewayError::Internal(format!(
                                    "account store: invalid user_json: {e}"
                                ))
                            })?;
                        Ok(AccountRecord {
                            tag,
                            user,
                            access_token,
                            refresh_token,
                            obtained_at,
                        })
                    },
                )
                .collect()
        })
    }

    /// 按 email 取单个账号;不存在返回 `None`。
    pub fn get(&self, email: &str) -> Result<Option<AccountRow>, GatewayError> {
        crate::db::blocking(|| {
            let mut conn = self.db.lock();
            accounts::table
                .filter(accounts::email.eq(email))
                .select(StoredAccount::as_select())
                .first::<StoredAccount>(&mut *conn)
                .optional()
                .map(|row| row.map(AccountRow::from))
                .map_err(sql_err)
        })
    }

    /// 当前可选(未摘除、不在冷却)的账号数,作为 `send_chat` 换号重试的上限。
    pub fn count_available(&self) -> usize {
        crate::db::blocking(|| {
            let now = unix_secs() as i64;
            let mut conn = self.db.lock();
            accounts::table
                .filter(
                    accounts::available.eq(1).and(
                        accounts::cooldown_until
                            .is_null()
                            .or(accounts::cooldown_until.le(now)),
                    ),
                )
                .count()
                .get_result::<i64>(&mut *conn)
                .map(|n| n as usize)
                .unwrap_or(0)
        })
    }

    /// 按邮箱和/或状态删除;返回删除行数。
    pub fn delete(&self, email: Option<&str>, status: Option<&str>) -> Result<usize, GatewayError> {
        crate::db::blocking(|| {
            if email.is_none() && status.is_none() {
                return Err(GatewayError::BadRequest(
                    "email or status is required".into(),
                ));
            }
            if status
                .is_some_and(|value| !matches!(value, "available" | "cooldown" | "unavailable"))
            {
                return Err(GatewayError::BadRequest(
                    "status must be one of available, cooldown, unavailable".into(),
                ));
            }

            let mut conn = self.db.lock();
            let now = unix_secs() as i64;
            let n = diesel::sql_query(
            "DELETE FROM accounts WHERE (? = 0 OR email = ?) AND (? = 0 OR CASE ? WHEN 'available' THEN available = 1 AND (cooldown_until IS NULL OR cooldown_until <= ?) WHEN 'cooldown' THEN available = 1 AND cooldown_until > ? WHEN 'unavailable' THEN available = 0 ELSE 0 END)",
        )
        .bind::<diesel::sql_types::Bool, _>(email.is_some())
        .bind::<Text, _>(email.unwrap_or_default())
        .bind::<diesel::sql_types::Bool, _>(status.is_some())
        .bind::<Text, _>(status.unwrap_or_default())
        .bind::<diesel::sql_types::BigInt, _>(now)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .execute(&mut *conn)
        .map_err(sql_err)?;
            Ok(n)
        })
    }

    /// 轮询取下一个可用账号(未摘除且不在限额冷却期);全不可用报
    /// [`GatewayError::NoAccount`]。
    pub fn next_available(&self) -> Result<AccountRow, GatewayError> {
        crate::db::blocking(|| {
            let now = unix_secs() as i64;
            let mut conn = self.db.lock();
            let last = self.last_id.load(Ordering::Relaxed);
            let mut selected = None;
            for after in [last, 0] {
                selected = accounts::table
                    .filter(
                        accounts::available.eq(1).and(
                            accounts::cooldown_until
                                .is_null()
                                .or(accounts::cooldown_until.le(now)),
                        ),
                    )
                    .filter(accounts::id.gt(after))
                    .order(accounts::id.asc())
                    .select(StoredAccount::as_select())
                    .first::<StoredAccount>(&mut *conn)
                    .optional()
                    .map_err(sql_err)?;
                if selected.is_some() {
                    break;
                }
            }
            let row = selected
                .ok_or_else(|| GatewayError::NoAccount("no available Raycast account".into()))?;
            self.last_id.store(row.id, Ordering::Relaxed);
            Ok(AccountRow::from(row))
        })
    }

    /// 摘除(401 / 额度耗尽)。
    pub fn mark_unavailable(&self, email: &str, reason: &str) {
        crate::db::blocking(|| {
            self.set_available(email, false, Some(reason));
        })
    }

    pub fn update_tokens(
        &self,
        email: &str,
        access_token: &str,
        refresh_token: Option<&str>,
    ) -> Result<bool, GatewayError> {
        crate::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            let n = if let Some(refresh_token) = refresh_token {
                diesel::update(accounts::table.filter(accounts::email.eq(email)))
                    .set((
                        accounts::access_token.eq(access_token),
                        accounts::refresh_token.eq(refresh_token),
                        accounts::available.eq(1),
                        accounts::unavailable_reason.eq(None::<String>),
                        accounts::updated_at.eq(&now),
                    ))
                    .execute(&mut *conn)
            } else {
                diesel::update(accounts::table.filter(accounts::email.eq(email)))
                    .set((
                        accounts::access_token.eq(access_token),
                        accounts::available.eq(1),
                        accounts::unavailable_reason.eq(None::<String>),
                        accounts::updated_at.eq(&now),
                    ))
                    .execute(&mut *conn)
            }
            .map_err(sql_err)?;
            Ok(n > 0)
        })
    }

    pub fn reinstate(&self, email: &str) -> Result<bool, GatewayError> {
        crate::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            let n = diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set((
                    accounts::available.eq(1),
                    accounts::unavailable_reason.eq(None::<String>),
                    accounts::cooldown_until.eq(None::<i64>),
                    accounts::updated_at.eq(&now),
                ))
                .execute(&mut *conn)
                .map_err(sql_err)?;
            Ok(n > 0)
        })
    }

    pub fn mark_cooldown(&self, email: &str, until_unix_secs: i64, reason: &str) {
        crate::db::blocking(|| {
            let mut conn = self.db.lock();
            let _ = diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set((
                    accounts::cooldown_until.eq(Some(until_unix_secs)),
                    accounts::unavailable_reason.eq(Some(reason)),
                    accounts::updated_at.eq(unix_secs().to_string()),
                ))
                .execute(&mut *conn);
        })
    }

    pub fn set_available(&self, email: &str, available: bool, reason: Option<&str>) {
        crate::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            let _ = diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set((
                    accounts::available.eq(i32::from(available)),
                    accounts::unavailable_reason.eq(reason),
                    accounts::updated_at.eq(now),
                ))
                .execute(&mut *conn);
        })
    }

    pub fn update_info(&self, email: &str, info: &AccountInfo) -> Result<(), GatewayError> {
        crate::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set((
                    accounts::has_pro_features.eq(info.has_pro_features.map(i32::from)),
                    accounts::tier.eq(info.tier.as_deref()),
                    accounts::sub_status.eq(info.sub_status.as_deref()),
                    accounts::trial_days_left.eq(info.trial_days_left),
                    accounts::current_period_end.eq(info.current_period_end),
                    accounts::trial_requests_per_hour.eq(info.trial_requests_per_hour),
                    accounts::remaining_credits.eq(info.remaining_credits),
                    accounts::total_credits.eq(info.total_credits),
                    accounts::remaining_usd.eq(info.remaining_usd),
                    accounts::next_credits_at.eq(info.next_credits_at.as_deref()),
                    accounts::info_fetched_at.eq(&now),
                    accounts::updated_at.eq(&now),
                ))
                .execute(&mut *conn)
                .map_err(sql_err)?;
            Ok(())
        })
    }
}

fn sql_err(e: diesel::result::Error) -> GatewayError {
    GatewayError::Internal(format!("account store: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::wire::AccountRecord;
    use crate::db::Db;

    fn rec(email: &str, token: &str) -> AccountRecord {
        serde_json::from_value(serde_json::json!({
            "tag": "t",
            "user": { "email": email },
            "access_token": token,
            "refresh_token": "r1"
        }))
        .unwrap()
    }

    fn store() -> AccountStore {
        AccountStore::new(Db::open(":memory:").unwrap())
    }

    fn all(store: &AccountStore) -> Vec<AccountRow> {
        match store.list(None, None, None).unwrap() {
            AccountList::All(rows) => rows,
            AccountList::Page(_) => unreachable!("unpaged list returns all rows"),
        }
    }

    #[test]
    fn concurrent_selection_advances_cursor_inside_connection_lock() {
        let store = std::sync::Arc::new(AccountStore::new(Db::open(":memory:").unwrap()));
        for email in ["a@test", "b@test"] {
            let record = serde_json::from_value(
                serde_json::json!({"user":{"email":email},"access_token":"rca_test"}),
            )
            .unwrap();
            store.upsert(&record).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.next_available().unwrap().email
                })
            })
            .collect();
        barrier.wait();
        let emails: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(emails.iter().filter(|email| *email == "a@test").count(), 4);
    }

    #[test]
    fn upsert_three_states() {
        let s = store();
        // 插入
        assert_eq!(
            s.upsert(&rec("a@x", "t1")).unwrap(),
            UpsertOutcome::Inserted
        );
        // token 相同 → 跳过
        assert_eq!(
            s.upsert(&rec("a@x", "t1")).unwrap(),
            UpsertOutcome::Unchanged
        );
        // token 变化 → 更新
        assert_eq!(s.upsert(&rec("a@x", "t2")).unwrap(), UpsertOutcome::Updated);
        assert_eq!(all(&s)[0].access_token, "t2");
    }

    #[test]
    fn update_resets_availability() {
        let s = store();
        s.upsert(&rec("a@x", "t1")).unwrap();
        s.mark_unavailable("a@x", "token 失效 (401)");
        assert!(!all(&s)[0].available);
        // 带 token 更新 → 恢复
        s.upsert(&rec("a@x", "t9")).unwrap();
        let row = all(&s)[0].clone();
        assert!(row.available);
        assert_eq!(row.unavailable_reason, None);
    }

    #[test]
    fn get_single_account() {
        let s = store();
        // 不存在 → None
        assert!(s.get("a@x").unwrap().is_none());
        s.upsert(&rec("a@x", "t1")).unwrap();
        let row = s.get("a@x").unwrap().expect("should exist");
        assert_eq!(row.access_token, "t1");
        assert_eq!(row.has_pro_features, None);
        // 带 has_pro 的完整信息写入后再读出
        s.update_info(
            "a@x",
            &crate::account::wire::AccountInfo {
                has_pro_features: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.get("a@x").unwrap().unwrap().has_pro_features, Some(true));
    }

    #[test]
    fn upsert_without_email_is_bad_request() {
        let s = store();
        let bad: AccountRecord = serde_json::from_str(r#"{ "access_token": "t" }"#).unwrap();
        assert!(matches!(
            s.upsert(&bad).unwrap_err(),
            GatewayError::BadRequest(_)
        ));
    }

    #[test]
    fn rotation_cycles_and_skips_unavailable() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.upsert(&rec("b@x", "tb")).unwrap();
        s.upsert(&rec("c@x", "tc")).unwrap();
        assert_eq!(s.next_available().unwrap().email, "a@x");
        assert_eq!(s.next_available().unwrap().email, "b@x");
        // 摘除 b 后,轮到 c,再循环 a、c
        s.mark_unavailable("b@x", "额度耗尽");
        assert_eq!(s.next_available().unwrap().email, "c@x");
        assert_eq!(s.next_available().unwrap().email, "a@x");
        assert_eq!(s.next_available().unwrap().email, "c@x");
    }

    #[test]
    fn cooldown_skips_account_until_expiry() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.upsert(&rec("b@x", "tb")).unwrap();

        // a 进入冷却:轮询只剩 b。
        s.mark_cooldown("a@x", unix_secs() as i64 + 600, "限额 25/h");
        assert_eq!(s.next_available().unwrap().email, "b@x");
        assert_eq!(s.next_available().unwrap().email, "b@x");
        let a = &all(&s)[0];
        assert!(a.available, "冷却不摘号");
        assert!(a.cooldown_until.is_some());

        // 冷却到期自动恢复。
        s.mark_cooldown("a@x", unix_secs() as i64 - 1, "已过期");
        assert_eq!(s.next_available().unwrap().email, "a@x");
    }

    #[test]
    fn no_available_account_errors() {
        let s = store();
        assert!(matches!(
            s.next_available().unwrap_err(),
            GatewayError::NoAccount(_)
        ));
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.mark_unavailable("a@x", "x");
        assert!(matches!(
            s.next_available().unwrap_err(),
            GatewayError::NoAccount(_)
        ));
    }

    #[test]
    fn delete_reports_existence() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        assert_eq!(s.delete(Some("a@x"), None).unwrap(), 1);
        assert_eq!(s.delete(Some("a@x"), None).unwrap(), 0);
        assert!(all(&s).is_empty());
    }

    #[test]
    fn update_info_writes_quota_and_availability() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        let info = AccountInfo {
            tier: Some("pro".into()),
            sub_status: Some("trialing".into()),
            remaining_credits: Some(5.5),
            total_credits: Some(10.0),
            remaining_usd: Some(0.55),
            ..AccountInfo::default()
        };
        s.update_info("a@x", &info).unwrap();
        let row = all(&s)[0].clone();
        assert_eq!(row.tier.as_deref(), Some("pro"));
        assert_eq!(row.remaining_credits, Some(5.5));
        assert!(row.info_fetched_at.is_some());

        // set_available 独立操作
        s.set_available("a@x", false, Some("额度耗尽"));
        assert!(!all(&s)[0].available);
        s.set_available("a@x", true, None);
        assert!(all(&s)[0].available);
    }
}
