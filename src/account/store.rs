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
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
    pub info_fetched_at: Option<String>,
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
            tier: self.tier.clone(),
            sub_status: self.sub_status.clone(),
            trial_days_left: self.trial_days_left,
            current_period_end: self.current_period_end,
            remaining_credits: self.remaining_credits,
            total_credits: self.total_credits,
            remaining_usd: self.remaining_usd,
            next_credits_at: self.next_credits_at.clone(),
            info_fetched_at: self.info_fetched_at.clone(),
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
    tier                TEXT,
    sub_status          TEXT,
    trial_days_left     INTEGER,
    current_period_end  INTEGER,
    remaining_credits   REAL,
    total_credits       REAL,
    remaining_usd       REAL,
    next_credits_at     TEXT,
    info_fetched_at     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
";

/// 查询列的统一顺序(`list` / `next_available` 共用);`id` 在首位,
/// 供轮询游标使用。
const ROW_COLUMNS: &str = "id, email, tag, access_token, refresh_token, obtained_at, \
    available, unavailable_reason, tier, sub_status, trial_days_left, \
    current_period_end, remaining_credits, total_credits, remaining_usd, \
    next_credits_at, info_fetched_at, created_at, updated_at";

pub struct AccountStore {
    db: Db,
    /// 轮询游标:最后选中的行 id。取 id 更大的下一个可用行(没有则回卷),
    /// 这样摘除某账号后轮询会自然落到它的后继,而不是跳回头部。
    last_id: AtomicI64,
}

impl AccountStore {
    /// 构造并建表(幂等)。
    pub fn new(db: Db) -> Self {
        db.lock()
            .execute_batch(SCHEMA)
            .expect("create accounts schema");
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

    /// 删除;返回是否存在。
    pub fn delete(&self, email: &str) -> Result<bool, GatewayError> {
        let conn = self.db.lock();
        let n = conn
            .execute("DELETE FROM accounts WHERE email = ?1", [email])
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// 轮询取下一个可用账号;全不可用报 [`GatewayError::NoAccount`]。
    pub fn next_available(&self) -> Result<AccountRow, GatewayError> {
        let rows = {
            let conn = self.db.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ROW_COLUMNS} FROM accounts WHERE available = 1 ORDER BY id"
                ))
                .map_err(sql_err)?;
            stmt.query_map([], row_to_account)
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
        let (id, row) = rows
            .iter()
            .find(|(id, _)| *id > last)
            .unwrap_or(&rows[0]);
        self.last_id.store(*id, Ordering::Relaxed);
        Ok(row.clone())
    }

    /// 摘除(401 / 额度耗尽)。
    pub fn mark_unavailable(&self, email: &str, reason: &str) {
        self.set_available(email, false, Some(reason));
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
                tier = ?2, sub_status = ?3, trial_days_left = ?4,
                current_period_end = ?5, remaining_credits = ?6, total_credits = ?7,
                remaining_usd = ?8, next_credits_at = ?9,
                info_fetched_at = ?10, updated_at = ?10
             WHERE email = ?1",
            rusqlite::params![
                email,
                info.tier,
                info.sub_status,
                info.trial_days_left,
                info.current_period_end,
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
            tier: r.get(8)?,
            sub_status: r.get(9)?,
            trial_days_left: r.get(10)?,
            current_period_end: r.get(11)?,
            remaining_credits: r.get(12)?,
            total_credits: r.get(13)?,
            remaining_usd: r.get(14)?,
            next_credits_at: r.get(15)?,
            info_fetched_at: r.get(16)?,
            created_at: r.get(17)?,
            updated_at: r.get(18)?,
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
        assert_eq!(s.upsert(&rec("a@x", "t1")).unwrap(), UpsertOutcome::Inserted);
        // token 相同 → 跳过
        assert_eq!(s.upsert(&rec("a@x", "t1")).unwrap(), UpsertOutcome::Unchanged);
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
