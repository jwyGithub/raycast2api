//! src/account/store.rs
//! 账号存储:accounts 表的 CRUD、轮询选号与可用性管理。

use std::sync::atomic::{AtomicI64, Ordering};

use diesel::connection::SimpleConnection;
use diesel::dsl::sql;
use diesel::prelude::*;
use diesel::sql_types::{Bool, Text};
use diesel::sqlite::Sqlite;

use crate::account::wire::{AccountInfo, AccountRecord};
use crate::core::page::Page;
use crate::error::GatewayError;
use crate::storage::db::Db;
use crate::storage::schema::accounts;
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
    /// 管理员开关:`false` 时不参与调度或额度查询,仅允许必要的 token 续期。
    pub enabled: bool,
    /// 调度优先级,数值越大越先用;同档内轮询,高档有可用账号时不用低档。
    pub priority: i32,
    /// 账号专属 device_id(导入时必填并落库);`None` 仅见于迁移前的旧行,
    /// 首次被选中时由 dispatcher 标记摘除并提示重新导入,不回落全局配置。
    pub device_id: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub obtained_at: Option<String>,
    pub access_token_expires_at: Option<i64>,
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

impl AccountRow {
    /// 冷却和停用期间禁止普通上游请求;必要的 token 续期走独立入口。
    pub fn ensure_upstream_allowed(&self, now: i64) -> Result<(), GatewayError> {
        let (code, message) = if !self.enabled {
            (
                "account_disabled",
                format!(
                    "account {} is disabled; enable it before calling upstream",
                    self.email
                ),
            )
        } else if self.cooldown_until.is_some_and(|until| until > now) {
            (
                "account_cooldown",
                format!(
                    "account {} is in cooldown; wait for expiry or restore it before calling upstream",
                    self.email
                ),
            )
        } else {
            return Ok(());
        };
        Err(GatewayError::Admission {
            status: axum::http::StatusCode::CONFLICT,
            code,
            message,
        })
    }

    /// 与 [`AccountStatus`] 的 SQL 谓词等价的内存判定,供接口说明账号的实际状态。
    pub fn status_at(&self, now: i64) -> AccountStatus {
        if !self.enabled {
            AccountStatus::Disabled
        } else if !self.available {
            AccountStatus::Unavailable
        } else if self.cooldown_until.is_some_and(|until| until > now) {
            AccountStatus::Cooldown
        } else {
            AccountStatus::Available
        }
    }
}

#[derive(Queryable, Selectable)]
#[diesel(table_name = accounts)]
#[diesel(check_for_backend(Sqlite))]
struct StoredAccount {
    id: i64,
    email: String,
    tag: Option<String>,
    enabled: i32,
    priority: i32,
    device_id: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    obtained_at: Option<String>,
    access_token_expires_at: Option<i64>,
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
            enabled: row.enabled != 0,
            priority: row.priority,
            device_id: row.device_id,
            access_token: row.access_token,
            refresh_token: row.refresh_token,
            obtained_at: row.obtained_at,
            access_token_expires_at: row.access_token_expires_at,
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
    enabled: i32,
    priority: i32,
    device_id: Option<&'a str>,
    access_token: &'a str,
    refresh_token: Option<&'a str>,
    obtained_at: Option<&'a str>,
    access_token_expires_at: Option<i64>,
    user_json: &'a str,
    created_at: &'a str,
    updated_at: &'a str,
}

/// 只改开关/优先级的变更集;`None` 字段不参与 UPDATE。
#[derive(AsChangeset)]
#[diesel(table_name = accounts)]
struct PolicyChanges<'a> {
    enabled: Option<i32>,
    priority: Option<i32>,
    updated_at: &'a str,
}

/// 账号的调度状态。四个取值互斥,优先级:停用 > 不可用 > 冷却 > 可用。
/// 列表筛选、计数、删除、选号共用这一定义;合法取值与 SQL 谓词同处一个 match,
/// 新增状态时编译器会强制两边同步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Available,
    Cooldown,
    Unavailable,
    Disabled,
}

impl AccountStatus {
    /// 解析 API 的状态名;`all`、空串等非状态值返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "available" => Self::Available,
            "cooldown" => Self::Cooldown,
            "unavailable" => Self::Unavailable,
            "disabled" => Self::Disabled,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Cooldown => "cooldown",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }

    /// 对应的 SQL 谓词。`now` 为内部时钟的 unix 秒,不接受用户输入。
    fn predicate(self, now: i64) -> String {
        let clause = match self {
            Self::Disabled => "enabled = 0".into(),
            Self::Unavailable => "enabled = 1 AND available = 0".into(),
            Self::Cooldown => format!("enabled = 1 AND available = 1 AND cooldown_until > {now}"),
            Self::Available => format!(
                "enabled = 1 AND available = 1 AND (cooldown_until IS NULL OR cooldown_until <= {now})"
            ),
        };
        format!("({clause})")
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    email               TEXT NOT NULL UNIQUE,
    tag                 TEXT,
    device_id           TEXT,
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
/// 调用方应在 `IMMEDIATE` 事务内执行,使探测与加列原子完成。
fn migrate(conn: &mut diesel::sqlite::SqliteConnection) -> Result<(), diesel::result::Error> {
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
        (
            "device_id",
            "ALTER TABLE accounts ADD COLUMN device_id TEXT;",
        ),
        (
            "enabled",
            "ALTER TABLE accounts ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;",
        ),
        (
            "priority",
            "ALTER TABLE accounts ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;",
        ),
        (
            "access_token_expires_at",
            "ALTER TABLE accounts ADD COLUMN access_token_expires_at BIGINT;",
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
        .get_result::<Count>(conn)?
        .count
            > 0;
        if !has {
            conn.batch_execute(ddl)?;
        }
    }
    Ok(())
}

pub struct AccountStore {
    renewal_locks: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
    /// 按凭据摘要记录轮换退避,避免网络结果不明时反复重放 OAuth。
    renewal_attempts: std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>,
    db: Db,
    /// 轮询游标:最后选中的行 id。取 id 更大的下一个可用行(没有则回卷),
    /// 这样摘除某账号后轮询会自然落到它的后继,而不是跳回头部。
    last_id: AtomicI64,
    /// 上一次选号所在的优先级档;档位变化时游标从档首重新开始。
    last_tier: AtomicI64,
}

impl AccountStore {
    /// 构造并建表(幂等),并做既有库的增量迁移。
    pub fn new(db: Db) -> Result<Self, GatewayError> {
        crate::storage::db::blocking(|| {
            {
                let mut conn = db.lock();
                conn.batch_execute(SCHEMA)
                    .map_err(|e| GatewayError::Internal(format!("create accounts schema: {e}")))?;
                conn.immediate_transaction(migrate)
                    .map_err(|e| GatewayError::Internal(format!("migrate accounts schema: {e}")))?;
            }
            Ok(Self {
                db,
                last_id: AtomicI64::new(0),
                last_tier: AtomicI64::new(i64::MIN),
                renewal_locks: Default::default(),
                renewal_attempts: Default::default(),
            })
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

    /// 调用方持有账号续期锁时申请一次轮换,相同凭据失败后短暂退避。
    pub(crate) fn claim_renewal(&self, email: &str, credentials: &str, now: i64) -> bool {
        let mut attempts = self.renewal_attempts.lock().expect("renewal attempts lock");
        if attempts
            .get(email)
            .is_some_and(|(key, retry_at)| key == credentials && *retry_at > now)
        {
            return false;
        }
        attempts.insert(email.into(), (credentials.into(), now.saturating_add(300)));
        true
    }

    pub(crate) fn reject_renewal(&self, email: &str, credentials: &str) {
        self.renewal_attempts
            .lock()
            .expect("renewal attempts lock")
            .insert(email.into(), (credentials.into(), i64::MAX));
    }

    /// 按 email upsert;三态语义见 [`UpsertOutcome`]。
    pub fn upsert(&self, record: &AccountRecord) -> Result<UpsertOutcome, GatewayError> {
        let mut outcomes = self.upsert_batch(std::slice::from_ref(record))?;
        Ok(outcomes.remove(0))
    }

    /// 批量 upsert:先校验全部条目,再在同一 `IMMEDIATE` 事务内顺序写入。
    /// 任一条目非法或写入失败则整批回滚,不会留下部分导入的账号。
    pub fn upsert_batch(
        &self,
        records: &[AccountRecord],
    ) -> Result<Vec<UpsertOutcome>, GatewayError> {
        let prepared = records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let label = if records.len() > 1 {
                    match record.email() {
                        Some(email) => format!("entries[{index}] ({email})"),
                        None => format!("entries[{index}]"),
                    }
                } else {
                    String::new()
                };
                let with_label = |error: GatewayError| match error {
                    GatewayError::BadRequest(message) if !label.is_empty() => {
                        GatewayError::BadRequest(format!("{label}: {message}"))
                    }
                    other => other,
                };
                record.validate().map_err(with_label)?;
                let email = record.email().ok_or_else(|| {
                    with_label(GatewayError::BadRequest(
                        "account entry has no user.email".into(),
                    ))
                })?;
                let priority = record
                    .priority
                    .map(crate::account::wire::priority_i32)
                    .transpose()
                    .map_err(with_label)?;
                Ok((record, email, priority))
            })
            .collect::<Result<Vec<_>, GatewayError>>()?;
        crate::storage::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            conn.immediate_transaction::<Vec<UpsertOutcome>, diesel::result::Error, _>(|conn| {
                prepared
                    .iter()
                    .map(|(record, email, priority)| {
                        upsert_tx(conn, record, email, *priority, &now)
                    })
                    .collect()
            })
            .map_err(sql_err)
        })
    }

    /// 全部账号(含不可用的;refresh 需要它们的 token)。
    pub fn list_all(&self) -> Result<Vec<AccountRow>, GatewayError> {
        crate::storage::db::blocking(|| {
            Ok(accounts::table
                .order(accounts::id.asc())
                .select(StoredAccount::as_select())
                .load::<StoredAccount>(&mut *self.db.lock())
                .map_err(sql_err)?
                .into_iter()
                .map(AccountRow::from)
                .collect())
        })
    }

    /// 分页查询,可按 `available` / `cooldown` / `unavailable` 过滤(`all` 或空串不过滤)。
    pub fn list_page(
        &self,
        page: u32,
        page_size: u32,
        status: Option<&str>,
    ) -> Result<Page<AccountRow>, GatewayError> {
        crate::storage::db::blocking(|| {
            let mut conn = self.db.lock();
            let status = match status.filter(|value| !value.is_empty() && *value != "all") {
                Some(value) => Some(AccountStatus::parse(value).ok_or_else(|| {
                    GatewayError::BadRequest(
                        "status must be one of all, available, cooldown, unavailable, disabled"
                            .into(),
                    )
                })?),
                None => None,
            };
            let now = unix_secs() as i64;
            let mut query = accounts::table.into_boxed::<Sqlite>();
            let mut count_query = accounts::table.into_boxed::<Sqlite>();
            if let Some(status) = status {
                let predicate = status.predicate(now);
                query = query.filter(sql::<Bool>(&predicate));
                count_query = count_query.filter(sql::<Bool>(&predicate));
            }
            let total = count_query
                .count()
                .get_result::<i64>(&mut *conn)
                .map_err(sql_err)? as u64;
            let rows = query
                .order(accounts::id.asc())
                .limit(i64::from(page_size))
                .offset(i64::from(page.saturating_sub(1)) * i64::from(page_size))
                .select(StoredAccount::as_select())
                .load::<StoredAccount>(&mut *conn)
                .map_err(sql_err)?
                .into_iter()
                .map(AccountRow::from)
                .collect();
            Ok(Page::new(rows, total, page, page_size))
        })
    }

    /// 导出可直接提交给 POST /v1/accounts/create 的原始账号凭据。
    pub fn export_records(&self) -> Result<Vec<AccountRecord>, GatewayError> {
        crate::storage::db::blocking(|| {
            let mut conn = self.db.lock();
            accounts::table
                .select((
                    accounts::user_json,
                    accounts::tag,
                    accounts::device_id,
                    accounts::access_token,
                    accounts::refresh_token,
                    accounts::obtained_at,
                    accounts::enabled,
                    accounts::priority,
                    accounts::access_token_expires_at,
                ))
                .order(accounts::id.asc())
                .load::<(
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    String,
                    Option<String>,
                    Option<String>,
                    i32,
                    i32,
                    Option<i64>,
                )>(&mut *conn)
                .map_err(sql_err)?
                .into_iter()
                .map(
                    |(
                        user_json,
                        tag,
                        device_id,
                        access_token,
                        refresh_token,
                        obtained_at,
                        enabled,
                        priority,
                        access_token_expires_at,
                    )| {
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
                            device_id,
                            enabled: Some(enabled != 0),
                            priority: Some(i64::from(priority)),
                            access_token_expires_at,
                        })
                    },
                )
                .collect()
        })
    }

    /// 按 email 取单个账号;不存在返回 `None`。
    pub fn get(&self, email: &str) -> Result<Option<AccountRow>, GatewayError> {
        crate::storage::db::blocking(|| {
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

    /// 发起上游请求前重新读取状态,避免使用停用或冷却前的快照。
    pub fn get_for_upstream(&self, email: &str) -> Result<AccountRow, GatewayError> {
        let row = self
            .get(email)?
            .ok_or_else(|| GatewayError::NotFound(format!("no account with email {email}")))?;
        row.ensure_upstream_allowed(unix_secs() as i64)?;
        self.ensure_upstream_priority(&row)?;
        Ok(row)
    }

    /// 高档仍有可调度账号时,低档只作为备用,不主动查询上游。
    pub fn ensure_upstream_priority(&self, row: &AccountRow) -> Result<(), GatewayError> {
        let top = crate::storage::db::blocking(|| {
            accounts::table
                .filter(sql::<Bool>(
                    &AccountStatus::Available.predicate(unix_secs() as i64),
                ))
                .select(diesel::dsl::max(accounts::priority))
                .first::<Option<i32>>(&mut *self.db.lock())
                .map_err(sql_err)
        })?;
        if top.is_some_and(|priority| priority > row.priority) {
            return Err(GatewayError::Admission {
                status: axum::http::StatusCode::CONFLICT,
                code: "account_standby",
                message: format!(
                    "account {} is on standby while a higher-priority account is available",
                    row.email
                ),
            });
        }
        Ok(())
    }

    /// 当前可选(未摘除、不在冷却)的账号数,作为 `AccountDispatcher::send_chat` 换号重试的上限。
    pub fn count_available(&self) -> usize {
        crate::storage::db::blocking(|| {
            let now = unix_secs() as i64;
            let mut conn = self.db.lock();
            accounts::table
                .filter(sql::<Bool>(&AccountStatus::Available.predicate(now)))
                .count()
                .get_result::<i64>(&mut *conn)
                .map(|n| n as usize)
                .unwrap_or_else(|e| {
                    // 查询失败与"池空"语义不同,记录下来便于排查。
                    tracing::error!(error = %e, "count_available query failed");
                    0
                })
        })
    }

    /// 池全空时,最早已恢复可用的冷却账号还有多少秒可用(`None` = 没有在冷却的
    /// 账号,即全被摘除或池本身为空)。供 [`GatewayError::NoAccount`] 给客户端
    /// 一个可行动的恢复时间,而不是一句笼统的"没有可用账号"。
    ///
    /// [`GatewayError::NoAccount`]: crate::error::GatewayError::NoAccount
    pub fn earliest_cooldown_secs(&self) -> Option<i64> {
        crate::storage::db::blocking(|| {
            let now = unix_secs() as i64;
            let mut conn = self.db.lock();
            accounts::table
                .filter(sql::<Bool>(&AccountStatus::Cooldown.predicate(now)))
                .select(diesel::dsl::min(accounts::cooldown_until))
                .first::<Option<i64>>(&mut *conn)
                .map(|earliest| earliest.map(|until| until - now))
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, "earliest_cooldown_secs query failed");
                    None
                })
        })
    }

    /// 按邮箱和/或状态删除;返回删除行数。
    ///
    /// 状态取 `available` / `cooldown` / `unavailable` / `disabled` 之一,与列表一致,
    /// 同时停用又不可用的账号只归入 `disabled`。空串视为未传。`all` 不接受。
    pub fn delete(&self, email: Option<&str>, status: Option<&str>) -> Result<usize, GatewayError> {
        crate::storage::db::blocking(|| {
            let status = status.filter(|value| !value.is_empty());
            if email.is_none() && status.is_none() {
                return Err(GatewayError::BadRequest(
                    "email or status is required".into(),
                ));
            }
            let status = match status {
                Some(value) => Some(AccountStatus::parse(value).ok_or_else(|| {
                    GatewayError::BadRequest(
                        "status must be one of available, cooldown, unavailable, disabled".into(),
                    )
                })?),
                None => None,
            };

            let mut conn = self.db.lock();
            let now = unix_secs() as i64;
            // 谓词只来自 AccountStatus 的固定字符串;用户输入仅经 bind 进入 SQL。
            let predicate = status.map_or_else(|| "1 = 1".to_string(), |s| s.predicate(now));
            let n = diesel::sql_query(format!(
                "DELETE FROM accounts WHERE (? = 0 OR email = ?) AND (? = 0 OR ({predicate}))"
            ))
            .bind::<diesel::sql_types::Bool, _>(email.is_some())
            .bind::<Text, _>(email.unwrap_or_default())
            .bind::<diesel::sql_types::Bool, _>(status.is_some())
            .execute(&mut *conn)
            .map_err(sql_err)?;
            Ok(n)
        })
    }

    /// 选取下一个账号:只在**最高优先级档**内轮询(未停用、未摘除、不在限额冷却);
    /// 该档全部不可用时才落到更低档。全不可用报 [`GatewayError::NoAccount`]。
    pub fn next_available(&self) -> Result<AccountRow, GatewayError> {
        crate::storage::db::blocking(|| {
            let now = unix_secs() as i64;
            let mut conn = self.db.lock();
            let schedulable = AccountStatus::Available.predicate(now);
            let top: Option<i32> = accounts::table
                .filter(sql::<Bool>(&schedulable))
                .select(diesel::dsl::max(accounts::priority))
                .first::<Option<i32>>(&mut *conn)
                .map_err(sql_err)?;
            let Some(top) = top else {
                return Err(GatewayError::NoAccount(
                    "no available Raycast account".into(),
                ));
            };
            let last = self.last_id.load(Ordering::Relaxed);
            // 档位变化(如高档恢复)时从档首轮询,避免沿用低档游标跳过高档靠前的账号。
            let last_tier = self.last_tier.swap(i64::from(top), Ordering::Relaxed);
            let cursor = if last_tier == i64::from(top) { last } else { 0 };
            let mut selected = None;
            for after in [cursor, 0] {
                selected = accounts::table
                    .filter(sql::<Bool>(&schedulable))
                    .filter(accounts::priority.eq(top))
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

    /// 修改开关和/或优先级;两者都为 `None` 时只刷新 `updated_at`。返回是否命中账号。
    /// `priority` 取 `i32` 全范围,越大越优先。
    pub fn update_policy(
        &self,
        email: &str,
        enabled: Option<bool>,
        priority: Option<i32>,
    ) -> Result<bool, GatewayError> {
        crate::storage::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            let n = diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set(&PolicyChanges {
                    enabled: enabled.map(i32::from),
                    priority,
                    updated_at: &now,
                })
                .execute(&mut *conn)
                .map_err(sql_err)?;
            Ok(n > 0)
        })
    }

    /// 摘除(401 / 额度耗尽)。
    pub fn mark_unavailable(&self, email: &str, reason: &str) {
        crate::storage::db::blocking(|| {
            self.set_available(email, false, Some(reason));
        })
    }

    pub fn update_tokens(
        &self,
        email: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at: Option<i64>,
    ) -> Result<bool, GatewayError> {
        crate::storage::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            let n = if let Some(refresh_token) = refresh_token {
                diesel::update(accounts::table.filter(accounts::email.eq(email)))
                    .set((
                        accounts::access_token.eq(access_token),
                        accounts::refresh_token.eq(refresh_token),
                        accounts::access_token_expires_at.eq(expires_at),
                        accounts::updated_at.eq(&now),
                    ))
                    .execute(&mut *conn)
            } else {
                diesel::update(accounts::table.filter(accounts::email.eq(email)))
                    .set((
                        accounts::access_token.eq(access_token),
                        accounts::access_token_expires_at.eq(expires_at),
                        accounts::updated_at.eq(&now),
                    ))
                    .execute(&mut *conn)
            }
            .map_err(sql_err)?;
            Ok(n > 0)
        })
    }

    pub fn reinstate(&self, email: &str) -> Result<bool, GatewayError> {
        crate::storage::db::blocking(|| {
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
        crate::storage::db::blocking(|| {
            let mut conn = self.db.lock();
            if let Err(error) = diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set((
                    accounts::cooldown_until.eq(Some(until_unix_secs)),
                    accounts::unavailable_reason.eq(Some(reason)),
                    accounts::updated_at.eq(unix_secs().to_string()),
                ))
                .execute(&mut *conn)
            {
                tracing::error!(%error, email, "failed to persist account cooldown");
            }
        })
    }

    pub fn set_available(&self, email: &str, available: bool, reason: Option<&str>) {
        crate::storage::db::blocking(|| {
            let now = unix_secs().to_string();
            let mut conn = self.db.lock();
            if let Err(error) = diesel::update(accounts::table.filter(accounts::email.eq(email)))
                .set((
                    accounts::available.eq(i32::from(available)),
                    accounts::unavailable_reason.eq(reason),
                    accounts::updated_at.eq(now),
                ))
                .execute(&mut *conn)
            {
                tracing::error!(%error, email, available, "failed to persist account availability");
            }
        })
    }

    pub fn update_info(&self, email: &str, info: &AccountInfo) -> Result<(), GatewayError> {
        crate::storage::db::blocking(|| {
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

/// 单条 upsert,调用方负责事务边界与校验。
fn upsert_tx(
    conn: &mut diesel::sqlite::SqliteConnection,
    record: &AccountRecord,
    email: &str,
    priority: Option<i32>,
    now: &str,
) -> Result<UpsertOutcome, diesel::result::Error> {
    // `Value` 的序列化对 string-key 映射不会失败;若失败说明内存结构已损坏,直接 panic。
    let user_json = serde_json::to_string(&record.user).expect("serializing a Value cannot fail");
    let existing = accounts::table
        .filter(accounts::email.eq(email))
        .select((
            accounts::access_token,
            accounts::refresh_token,
            accounts::enabled,
            accounts::priority,
            accounts::device_id,
            accounts::tag,
            accounts::obtained_at,
            accounts::user_json,
            accounts::access_token_expires_at,
        ))
        .first::<(
            String,
            Option<String>,
            i32,
            i32,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        )>(conn)
        .optional()?;

    let Some((
        access,
        refresh,
        enabled,
        current_priority,
        current_device,
        current_tag,
        current_obtained,
        current_user,
        current_expiry,
    )) = existing
    else {
        diesel::insert_into(accounts::table)
            .values(&NewAccount {
                email,
                tag: record.tag.as_deref(),
                enabled: i32::from(record.enabled.unwrap_or(true)),
                priority: priority.unwrap_or(0),
                device_id: record.device_id.as_deref(),
                access_token: &record.access_token,
                refresh_token: record.refresh_token.as_deref(),
                obtained_at: record.obtained_at.as_deref(),
                access_token_expires_at: record.access_token_expires_at,
                user_json: &user_json,
                created_at: now,
                updated_at: now,
            })
            .execute(conn)?;
        return Ok(UpsertOutcome::Inserted);
    };

    // 请求里省略的 refresh_token / obtained_at / tag 保留库里的值,
    // 避免只带 access_token 的导入把已有的续期凭据清空。
    let next_tag = record.tag.clone().or(current_tag.clone());
    let next_refresh = record.refresh_token.clone().or(refresh.clone());
    let next_obtained = record.obtained_at.clone().or(current_obtained.clone());
    // 同一 access token 可保留已知有效期;换 token 时不能沿用旧有效期。
    let next_expiry = record.access_token_expires_at.or_else(|| {
        (access == record.access_token)
            .then_some(current_expiry)
            .flatten()
    });
    let token_changed = access != record.access_token || refresh != next_refresh;
    // device_id 必填(validate 已保证非空),与库里不同即视为需要刷新:
    // 迁移前的旧行 device_id 为 NULL,重新导入必须能把它补上。
    let device_changed = record.device_id != current_device;
    let meta_changed = next_tag != current_tag
        || next_obtained != current_obtained
        || next_expiry != current_expiry
        || current_user.as_deref() != Some(user_json.as_str());
    // 开关/优先级只在请求里显式给出时才改写,缺省保留库里的值。
    let policy_changed = record.enabled.is_some_and(|value| value != (enabled != 0))
        || priority.is_some_and(|value| value != current_priority);
    if !token_changed && !device_changed && !meta_changed && !policy_changed {
        return Ok(UpsertOutcome::Unchanged);
    }
    if token_changed || device_changed || meta_changed {
        diesel::update(accounts::table.filter(accounts::email.eq(email)))
            .set((
                accounts::tag.eq(next_tag.as_deref()),
                accounts::device_id.eq(record.device_id.as_deref()),
                accounts::access_token.eq(&record.access_token),
                accounts::refresh_token.eq(next_refresh.as_deref()),
                accounts::obtained_at.eq(next_obtained.as_deref()),
                accounts::access_token_expires_at.eq(next_expiry),
                accounts::user_json.eq(&user_json),
                accounts::updated_at.eq(now),
            ))
            .execute(conn)?;
    }
    if token_changed {
        // 凭据变了才恢复健康状态;仅换 device_id 不改变调度状态。
        // 限流冷却属于账号,更新凭据也须保留,到期或显式恢复后才重新参与调度。
        diesel::update(accounts::table.filter(accounts::email.eq(email)))
            .set((
                accounts::available.eq(1),
                accounts::unavailable_reason.eq(None::<String>),
            ))
            .execute(conn)?;
    }
    if policy_changed {
        diesel::update(accounts::table.filter(accounts::email.eq(email)))
            .set(&PolicyChanges {
                enabled: record.enabled.map(i32::from),
                priority,
                updated_at: now,
            })
            .execute(conn)?;
    }
    Ok(UpsertOutcome::Updated)
}

fn sql_err(e: diesel::result::Error) -> GatewayError {
    GatewayError::Internal(format!("account store: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::wire::AccountRecord;
    use crate::storage::db::Db;

    /// 测试用的合法 device_id(64 位小写 hex)。
    const TEST_DEVICE_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn rec(email: &str, token: &str) -> AccountRecord {
        serde_json::from_value(serde_json::json!({
            "tag": "t",
            "user": { "email": email },
            "device_id": TEST_DEVICE_ID,
            "access_token": token,
            "refresh_token": "r1"
        }))
        .unwrap()
    }

    fn store() -> AccountStore {
        AccountStore::new(Db::open(":memory:").unwrap()).unwrap()
    }

    fn all(store: &AccountStore) -> Vec<AccountRow> {
        store.list_all().unwrap()
    }

    #[test]
    fn concurrent_selection_advances_cursor_inside_connection_lock() {
        let store = std::sync::Arc::new(AccountStore::new(Db::open(":memory:").unwrap()).unwrap());
        for email in ["a@test", "b@test"] {
            let record = serde_json::from_value(
                serde_json::json!({"user":{"email":email},"device_id":"1111111111111111111111111111111111111111111111111111111111111111","access_token":"rca_test"}),
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
    fn token_expiry_round_trips_and_never_follows_an_unrelated_access_token() {
        let s = store();
        let mut record = rec("expiry@x", "old");
        record.access_token_expires_at = Some(1_900_000_000);
        s.upsert(&record).unwrap();
        let exported = s.export_records().unwrap().remove(0);
        assert_eq!(
            exported.access_token_expires_at,
            record.access_token_expires_at
        );
        record.access_token_expires_at = None;
        s.upsert(&record).unwrap();
        assert_eq!(
            s.get("expiry@x").unwrap().unwrap().access_token_expires_at,
            Some(1_900_000_000)
        );
        record.access_token = "new".into();
        s.upsert(&record).unwrap();
        assert_eq!(
            s.get("expiry@x").unwrap().unwrap().access_token_expires_at,
            None
        );
        record.access_token_expires_at = Some(-1);
        assert!(matches!(
            s.upsert(&record),
            Err(GatewayError::BadRequest(_))
        ));
    }

    #[test]
    fn renewal_backoff_releases_after_timeout_or_credential_change() {
        let s = store();
        assert!(s.claim_renewal("a@x", "old-digest", 100));
        assert!(!s.claim_renewal("a@x", "old-digest", 101));
        assert!(s.claim_renewal("a@x", "old-digest", 400));
        s.reject_renewal("a@x", "old-digest");
        assert!(!s.claim_renewal("a@x", "old-digest", 100_000));
        assert!(s.claim_renewal("a@x", "new-digest", 100_000));
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
    fn importing_credentials_preserves_account_cooldown() {
        let s = store();
        let mut high = rec("high@x", "access-old");
        high.refresh_token = None;
        high.priority = Some(10);
        s.upsert(&high).unwrap();
        s.upsert(&rec("low@x", "access-low")).unwrap();
        let until = unix_secs() as i64 + 3600;
        s.mark_cooldown("high@x", until, "限额冷却");

        // 补充、更换 refresh_token 或更换 access_token 都不能提前解除账号限流。
        for (access, refresh) in [
            ("access-old", "refresh-first"),
            ("access-old", "refresh-next"),
            ("access-new", "refresh-next"),
        ] {
            high.access_token = access.into();
            high.refresh_token = Some(refresh.into());
            assert_eq!(s.upsert(&high).unwrap(), UpsertOutcome::Updated);
            let row = s.get("high@x").unwrap().unwrap();
            assert_eq!(row.access_token, access);
            assert_eq!(row.refresh_token.as_deref(), Some(refresh));
            assert_eq!(row.cooldown_until, Some(until));
            assert_eq!(row.status_at(unix_secs() as i64), AccountStatus::Cooldown);
            assert_eq!(s.count_available(), 1);
            assert_eq!(s.next_available().unwrap().email, "low@x");
        }
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
    fn disabled_account_is_never_selected() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.upsert(&rec("b@x", "tb")).unwrap();
        s.update_policy("a@x", Some(false), None).unwrap();
        for _ in 0..3 {
            assert_eq!(s.next_available().unwrap().email, "b@x");
        }
        assert_eq!(s.count_available(), 1);
        assert!(!all(&s)[0].enabled);
        // 重新启用后恢复参与轮询。
        s.update_policy("a@x", Some(true), None).unwrap();
        assert_eq!(s.count_available(), 2);
    }

    #[test]
    fn higher_priority_tier_is_used_until_exhausted() {
        let s = store();
        let with_priority = |email: &str, token: &str, priority: i64| {
            let mut r = rec(email, token);
            r.priority = Some(priority);
            r
        };
        s.upsert(&with_priority("low@x", "tl", 0)).unwrap();
        s.upsert(&with_priority("hi1@x", "th1", 10)).unwrap();
        s.upsert(&with_priority("hi2@x", "th2", 10)).unwrap();

        // 高档内轮询,不碰低档。
        assert_eq!(s.next_available().unwrap().email, "hi1@x");
        assert_eq!(s.next_available().unwrap().email, "hi2@x");
        assert_eq!(s.next_available().unwrap().email, "hi1@x");

        // 高档全部冷却 → 落到低档。
        s.mark_cooldown("hi1@x", unix_secs() as i64 + 600, "限额");
        s.mark_cooldown("hi2@x", unix_secs() as i64 + 600, "限额");
        assert_eq!(s.next_available().unwrap().email, "low@x");
        assert_eq!(s.count_available(), 1);

        // 高档恢复后立即回到高档。
        s.reinstate("hi2@x").unwrap();
        assert_eq!(s.next_available().unwrap().email, "hi2@x");
    }

    #[test]
    fn disabled_high_priority_account_falls_back_to_lower_tier() {
        let s = store();
        let mut high = rec("hi@x", "th");
        high.priority = Some(5);
        s.upsert(&high).unwrap();
        s.upsert(&rec("lo@x", "tl")).unwrap();
        assert_eq!(s.next_available().unwrap().email, "hi@x");
        s.update_policy("hi@x", Some(false), None).unwrap();
        assert_eq!(s.next_available().unwrap().email, "lo@x");
    }

    #[test]
    fn upstream_priority_follows_the_highest_schedulable_tier() {
        let s = store();
        let mut high = rec("hi@x", "hi");
        high.priority = Some(10);
        s.upsert(&high).unwrap();
        s.upsert(&rec("lo@x", "lo")).unwrap();
        let standby = || {
            assert!(matches!(
                s.get_for_upstream("lo@x"),
                Err(GatewayError::Admission {
                    code: "account_standby",
                    ..
                })
            ))
        };
        standby();
        s.mark_cooldown("hi@x", unix_secs() as i64 + 600, "limit");
        assert!(s.get_for_upstream("lo@x").is_ok());
        s.reinstate("hi@x").unwrap();
        standby();
        s.update_policy("hi@x", Some(false), None).unwrap();
        assert!(s.get_for_upstream("lo@x").is_ok());
        s.update_policy("hi@x", Some(true), None).unwrap();
        standby();
        s.mark_unavailable("hi@x", "expired");
        assert!(s.get_for_upstream("lo@x").is_ok());
        s.reinstate("hi@x").unwrap();
        standby();
        s.update_policy("lo@x", None, Some(10)).unwrap();
        assert!(s.get_for_upstream("lo@x").is_ok());
    }

    #[test]
    fn update_policy_changes_only_given_fields() {
        let s = store();
        let mut r = rec("a@x", "ta");
        r.priority = Some(3);
        s.upsert(&r).unwrap();
        assert!(s.update_policy("a@x", None, Some(7)).unwrap());
        let row = all(&s)[0].clone();
        assert!(row.enabled);
        assert_eq!(row.priority, 7);
        assert!(!s.update_policy("missing@x", Some(false), None).unwrap());
    }

    #[test]
    fn upsert_keeps_policy_unless_explicitly_given() {
        let s = store();
        let mut r = rec("a@x", "ta");
        r.enabled = Some(false);
        r.priority = Some(9);
        s.upsert(&r).unwrap();
        // 同 token 再导入、不带字段 → Unchanged,策略保留。
        assert_eq!(
            s.upsert(&rec("a@x", "ta")).unwrap(),
            UpsertOutcome::Unchanged
        );
        let row = all(&s)[0].clone();
        assert!(!row.enabled);
        assert_eq!(row.priority, 9);
        // 显式改优先级 → Updated,启停不变。
        let mut again = rec("a@x", "ta");
        again.priority = Some(1);
        assert_eq!(s.upsert(&again).unwrap(), UpsertOutcome::Updated);
        let row = all(&s)[0].clone();
        assert!(!row.enabled);
        assert_eq!(row.priority, 1);
    }

    #[test]
    fn status_filter_and_delete_cover_disabled() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.upsert(&rec("b@x", "tb")).unwrap();
        s.update_policy("a@x", Some(false), None).unwrap();
        s.mark_unavailable("a@x", "x");
        // 停用优先于不可用:a 只出现在 disabled 里。
        assert_eq!(s.list_page(1, 10, Some("disabled")).unwrap().total, 1);
        assert_eq!(s.list_page(1, 10, Some("unavailable")).unwrap().total, 0);
        assert_eq!(s.list_page(1, 10, Some("available")).unwrap().total, 1);
        assert_eq!(s.delete(None, Some("disabled")).unwrap(), 1);
        assert!(s.get("a@x").unwrap().is_none());
        assert!(s.get("b@x").unwrap().is_some());
    }

    #[test]
    fn migrates_legacy_accounts_table_to_enabled_and_priority() {
        // 旧库没有 enabled/priority 列:迁移后既有账号应保持启用、优先级 0,且可重复迁移。
        let db = Db::open(":memory:").unwrap();
        db.lock().batch_execute(SCHEMA).unwrap();
        diesel::sql_query(
            "INSERT INTO accounts (email, access_token, user_json, created_at, updated_at) \
             VALUES ('old@x', 'rca_old', '{\"email\":\"old@x\"}', '1', '1')",
        )
        .execute(&mut *db.lock())
        .unwrap();

        let s = AccountStore::new(db.clone()).unwrap();
        let row = s.get("old@x").unwrap().expect("legacy row survives");
        assert!(row.enabled);
        assert_eq!(row.priority, 0);
        assert_eq!(s.next_available().unwrap().email, "old@x");

        // 再次构造(模拟重启)不报错,数据不变。
        let again = AccountStore::new(db);
        let row = again.expect("reopen").get("old@x").unwrap().unwrap();
        assert!(row.enabled);
        assert_eq!(row.priority, 0);
    }

    #[test]
    fn tier_recovery_restarts_from_tier_head() {
        let s = store();
        let with_priority = |email: &str, priority: i64| {
            let mut r = rec(email, "t");
            r.priority = Some(priority);
            r
        };
        s.upsert(&with_priority("hi1@x", 10)).unwrap();
        s.upsert(&with_priority("hi2@x", 10)).unwrap();
        s.upsert(&with_priority("lo@x", 0)).unwrap();
        s.upsert(&with_priority("hi3@x", 10)).unwrap();

        assert_eq!(s.next_available().unwrap().email, "hi1@x");
        assert_eq!(s.next_available().unwrap().email, "hi2@x");
        // 高档全部冷却 → 落到低档,游标停在低档 id 上。
        for email in ["hi1@x", "hi2@x", "hi3@x"] {
            s.mark_cooldown(email, unix_secs() as i64 + 600, "限额");
        }
        assert_eq!(s.next_available().unwrap().email, "lo@x");
        // 高档恢复:应从高档档首(hi1)开始,而不是沿用低档游标跳到 hi3。
        for email in ["hi1@x", "hi2@x", "hi3@x"] {
            s.reinstate(email).unwrap();
        }
        assert_eq!(s.next_available().unwrap().email, "hi1@x");
    }

    #[test]
    fn batch_upsert_rolls_back_on_invalid_entry() {
        let s = store();
        let mut bad = rec("bad@x", "tb");
        bad.priority = Some(3_000_000_000);
        let err = s.upsert_batch(&[rec("ok@x", "to"), bad]).unwrap_err();
        assert!(matches!(err, GatewayError::BadRequest(_)));
        assert!(s.list_all().unwrap().is_empty());
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
