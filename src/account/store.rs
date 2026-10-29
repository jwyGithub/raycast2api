//! src/account/store.rs
//! 账号存储:accounts 表的 CRUD、轮询选号与可用性管理。

use std::sync::atomic::{AtomicI64, Ordering};

use crate::account::wire::{AccountInfo, AccountRecord, AccountView};
use crate::db::Db;
use crate::error::GatewayError;
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

impl AccountRow {
    /// 转为对外视图:token 掩码。
    pub fn to_view(&self) -> AccountView {
        AccountView {
            email: self.email.clone(),
            tag: self.tag.clone(),
            access_token: crate::account::wire::mask_token(&self.access_token),
            available: self.available,
            unavailable_reason: self.unavailable_reason.clone(),
            obtained_at: self.obtained_at.clone(),
            has_pro_features: self.has_pro_features,
            tier: self.tier.clone(),
            sub_status: self.sub_status.clone(),
            trial_days_left: self.trial_days_left,
            current_period_end: self.current_period_end,
            trial_requests_per_hour: self.trial_requests_per_hour,
            remaining_credits: self.remaining_credits,
            total_credits: self.total_credits,
            remaining_usd: self.remaining_usd,
            next_credits_at: self.next_credits_at.clone(),
            info_fetched_at: self.info_fetched_at.clone(),
            cooldown_until: self.cooldown_until,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
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
fn migrate(conn: &rusqlite::Connection) {
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
        let has: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('accounts') WHERE name = ?1",
                [column],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !has {
            conn.execute_batch(ddl)
                .unwrap_or_else(|e| panic!("migrate accounts: add {column}: {e}"));
        }
    }
}

/// 查询列的统一顺序(`list` / `next_available` 共用);`id` 在首位,
/// 供轮询游标使用。
const ROW_COLUMNS: &str = "id, email, tag, access_token, refresh_token, obtained_at, \
    available, unavailable_reason, has_pro_features, tier, sub_status, trial_days_left, \
    current_period_end, trial_requests_per_hour, remaining_credits, total_credits, remaining_usd, \
    next_credits_at, info_fetched_at, cooldown_until, created_at, updated_at";

pub struct AccountStore {
    db: Db,
    /// 轮询游标:最后选中的行 id。取 id 更大的下一个可用行(没有则回卷),
    /// 这样摘除某账号后轮询会自然落到它的后继,而不是跳回头部。
    last_id: AtomicI64,
}

impl AccountStore {
    /// 构造并建表(幂等),并做既有库的增量迁移。
    pub fn new(db: Db) -> Self {
        {
            let conn = db.lock();
            conn.execute_batch(SCHEMA).expect("create accounts schema");
            migrate(&conn);
        }
        Self {
            db,
            last_id: AtomicI64::new(0),
        }
    }

    /// 按 email upsert;三态语义见 [`UpsertOutcome`]。
    pub fn upsert(&self, record: &AccountRecord) -> Result<UpsertOutcome, GatewayError> {
        let email = record
            .email()
            .ok_or_else(|| GatewayError::BadRequest("account entry has no user.email".into()))?;
        let user_json = serde_json::to_string(&record.user).unwrap_or_default();
        let now = unix_secs().to_string();
        let conn = self.db.lock();

        let existing: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT access_token, refresh_token FROM accounts WHERE email = ?1",
                [&email],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        match existing {
            None => {
                conn.execute(
                    "INSERT INTO accounts
                        (email, tag, access_token, refresh_token, obtained_at,
                         user_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    rusqlite::params![
                        email,
                        record.tag,
                        record.access_token,
                        record.refresh_token,
                        record.obtained_at,
                        user_json,
                        now,
                    ],
                )
                .map_err(sql_err)?;
                Ok(UpsertOutcome::Inserted)
            }
            Some((access, refresh))
                if access != record.access_token || refresh != record.refresh_token =>
            {
                // token 变了:更新全部业务字段并恢复可用(新 token 可能已复活)。
                conn.execute(
                    "UPDATE accounts SET
                        tag = ?2, access_token = ?3, refresh_token = ?4,
                        obtained_at = ?5, user_json = ?6,
                        available = 1, unavailable_reason = NULL, updated_at = ?7
                     WHERE email = ?1",
                    rusqlite::params![
                        email,
                        record.tag,
                        record.access_token,
                        record.refresh_token,
                        record.obtained_at,
                        user_json,
                        now,
                    ],
                )
                .map_err(sql_err)?;
                Ok(UpsertOutcome::Updated)
            }
            Some(_) => Ok(UpsertOutcome::Unchanged),
        }
    }

    /// 全部账号(含不可用的;refresh 需要它们的 token)。
    pub fn list(&self) -> Result<Vec<AccountRow>, GatewayError> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare(&format!("SELECT {ROW_COLUMNS} FROM accounts ORDER BY id"))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], row_to_account)
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        Ok(rows.into_iter().map(|(_, row)| row).collect())
    }

    /// 按 email 取单个账号;不存在返回 `None`。
    pub fn get(&self, email: &str) -> Result<Option<AccountRow>, GatewayError> {
        let conn = self.db.lock();
        match conn.query_row(
            &format!("SELECT {ROW_COLUMNS} FROM accounts WHERE email = ?1"),
            [email],
            row_to_account,
        ) {
            Ok((_, row)) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(sql_err(e)),
        }
    }

    /// 当前可选(未摘除、不在冷却)的账号数,作为 `send_chat` 换号重试的上限。
    pub fn count_available(&self) -> usize {
        let now = unix_secs() as i64;
        self.db
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM accounts
                 WHERE available = 1 AND (cooldown_until IS NULL OR cooldown_until <= ?1)",
                [now],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// 删除;返回是否存在。
    pub fn delete(&self, email: &str) -> Result<bool, GatewayError> {
        let conn = self.db.lock();
        let n = conn
            .execute("DELETE FROM accounts WHERE email = ?1", [email])
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// 轮询取下一个可用账号(未摘除且不在限额冷却期);全不可用报
    /// [`GatewayError::NoAccount`]。
    pub fn next_available(&self) -> Result<AccountRow, GatewayError> {
        let now = unix_secs() as i64;
        let rows = {
            let conn = self.db.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ROW_COLUMNS} FROM accounts
                     WHERE available = 1
                       AND (cooldown_until IS NULL OR cooldown_until <= ?1)
                     ORDER BY id"
                ))
                .map_err(sql_err)?;
            stmt.query_map([now], row_to_account)
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?
        };
        if rows.is_empty() {
            return Err(GatewayError::NoAccount(
                "no available Raycast account; add or refresh one via POST /v1/accounts".into(),
            ));
        }
        let last = self.last_id.load(Ordering::Relaxed);
        let (id, row) = rows.iter().find(|(id, _)| *id > last).unwrap_or(&rows[0]);
        self.last_id.store(*id, Ordering::Relaxed);
        Ok(row.clone())
    }

    /// 摘除(401 / 额度耗尽)。
    pub fn mark_unavailable(&self, email: &str, reason: &str) {
        self.set_available(email, false, Some(reason));
    }

    /// 原子写回刷新后的 token 对并恢复可用。refresh_token 是一次一换:
    /// 刷新成功必须同时落盘新 access_token 与新 refresh_token,否则旧的
    /// refresh_token 已作废、账号就死了。返回是否确实有更新。
    pub fn update_tokens(
        &self,
        email: &str,
        access_token: &str,
        refresh_token: Option<&str>,
    ) -> Result<bool, GatewayError> {
        let now = unix_secs().to_string();
        let conn = self.db.lock();
        let n = conn
            .execute(
                "UPDATE accounts SET
                    access_token = ?2,
                    refresh_token = COALESCE(?3, refresh_token),
                    available = 1,
                    unavailable_reason = NULL,
                    updated_at = ?4
                 WHERE email = ?1",
                rusqlite::params![email, access_token, refresh_token, now],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// 立即恢复账号(清冷却 + 置可用)。用于用户处理完(如确认邮箱)后
    /// 手动恢复,不等冷却自然到期。返回是否存在该账号。
    pub fn reinstate(&self, email: &str) -> Result<bool, GatewayError> {
        let now = unix_secs().to_string();
        let conn = self.db.lock();
        let n = conn
            .execute(
                "UPDATE accounts SET
                    available = 1,
                    unavailable_reason = NULL,
                    cooldown_until = NULL,
                    updated_at = ?2
                 WHERE email = ?1",
                rusqlite::params![email, now],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// 限额冷却:账号保持 `available=1`,但轮询在 `until`(unix 秒)前
    /// 跳过它。到期自动恢复,无需显式清除。
    pub fn mark_cooldown(&self, email: &str, until_unix_secs: i64, reason: &str) {
        let conn = self.db.lock();
        let _ = conn.execute(
            "UPDATE accounts
                SET cooldown_until = ?2,
                    unavailable_reason = ?3,
                    updated_at = ?4
             WHERE email = ?1",
            rusqlite::params![email, until_unix_secs, reason, unix_secs().to_string()],
        );
    }

    /// 设置可用性;`available = true` 时清空原因。
    pub fn set_available(&self, email: &str, available: bool, reason: Option<&str>) {
        let now = unix_secs().to_string();
        let conn = self.db.lock();
        let _ = conn.execute(
            "UPDATE accounts
                SET available = ?2,
                    unavailable_reason = ?3,
                    updated_at = ?4
             WHERE email = ?1",
            rusqlite::params![email, available, reason, now],
        );
    }

    /// 写入额度/订阅信息与 `info_fetched_at`。
    pub fn update_info(&self, email: &str, info: &AccountInfo) -> Result<(), GatewayError> {
        let now = unix_secs().to_string();
        let conn = self.db.lock();
        conn.execute(
            "UPDATE accounts SET
                has_pro_features = ?2, tier = ?3, sub_status = ?4, trial_days_left = ?5,
                current_period_end = ?6, trial_requests_per_hour = ?7,
                remaining_credits = ?8, total_credits = ?9,
                remaining_usd = ?10, next_credits_at = ?11,
                info_fetched_at = ?12, updated_at = ?12
             WHERE email = ?1",
            rusqlite::params![
                email,
                info.has_pro_features.map(|b| b as i64),
                info.tier,
                info.sub_status,
                info.trial_days_left,
                info.current_period_end,
                info.trial_requests_per_hour,
                info.remaining_credits,
                info.total_credits,
                info.remaining_usd,
                info.next_credits_at,
                now,
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

/// 行映射(列顺序与 `ROW_COLUMNS` 一致;首列是行 `id`,供轮询游标用)。
fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, AccountRow)> {
    Ok((
        r.get(0)?,
        AccountRow {
            email: r.get(1)?,
            tag: r.get(2)?,
            access_token: r.get(3)?,
            refresh_token: r.get(4)?,
            obtained_at: r.get(5)?,
            available: r.get::<_, i64>(6)? != 0,
            unavailable_reason: r.get(7)?,
            has_pro_features: r.get::<_, Option<i64>>(8)?.map(|n| n != 0),
            tier: r.get(9)?,
            sub_status: r.get(10)?,
            trial_days_left: r.get(11)?,
            current_period_end: r.get(12)?,
            trial_requests_per_hour: r.get(13)?,
            remaining_credits: r.get(14)?,
            total_credits: r.get(15)?,
            remaining_usd: r.get(16)?,
            next_credits_at: r.get(17)?,
            info_fetched_at: r.get(18)?,
            cooldown_until: r.get(19)?,
            created_at: r.get(20)?,
            updated_at: r.get(21)?,
        },
    ))
}

/// SQL 错误统一进 `GatewayError::Internal`。
fn sql_err(e: rusqlite::Error) -> GatewayError {
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
        assert_eq!(s.list().unwrap()[0].access_token, "t2");
    }

    #[test]
    fn update_resets_availability() {
        let s = store();
        s.upsert(&rec("a@x", "t1")).unwrap();
        s.mark_unavailable("a@x", "token 失效 (401)");
        assert!(!s.list().unwrap()[0].available);
        // 带 token 更新 → 恢复
        s.upsert(&rec("a@x", "t9")).unwrap();
        let row = s.list().unwrap()[0].clone();
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
        let a = &s.list().unwrap()[0];
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
        assert!(s.delete("a@x").unwrap());
        assert!(!s.delete("a@x").unwrap());
        assert!(s.list().unwrap().is_empty());
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
        let row = s.list().unwrap()[0].clone();
        assert_eq!(row.tier.as_deref(), Some("pro"));
        assert_eq!(row.remaining_credits, Some(5.5));
        assert!(row.info_fetched_at.is_some());

        // set_available 独立操作
        s.set_available("a@x", false, Some("额度耗尽"));
        assert!(!s.list().unwrap()[0].available);
        s.set_available("a@x", true, None);
        assert!(s.list().unwrap()[0].available);
    }
}
