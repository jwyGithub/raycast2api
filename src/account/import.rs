//! 启动导入:account.json → 账号表。

use crate::account::store::{AccountStore, UpsertOutcome};
use crate::account::wire::AccountRecord;
use crate::error::GatewayError;

/// 一次导入的计数。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// email 缺失等被跳过的条目数。
    pub skipped: usize,
}

/// 从 `path` 导入账号;文件缺失静默返回零计数。
pub fn import_file(store: &AccountStore, path: &str) -> Result<ImportSummary, GatewayError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        // 没有账号文件是合法状态(账号可以只来自 POST /v1/accounts)。
        Err(_) => return Ok(ImportSummary::default()),
    };

    let records: Vec<AccountRecord> = serde_json::from_str(&content)
        .map_err(|e| GatewayError::Config(format!("failed to parse '{path}': {e}")))?;

    let mut summary = ImportSummary::default();
    for record in records {
        if record.email().is_none() {
            tracing::warn!(tag = ?record.tag, "account entry has no user.email, skipped");
            summary.skipped += 1;
            continue;
        }
        match store.upsert(&record)? {
            UpsertOutcome::Inserted => summary.inserted += 1,
            UpsertOutcome::Updated => summary.updated += 1,
            UpsertOutcome::Unchanged => summary.unchanged += 1,
        }
    }
    tracing::info!(
        inserted = summary.inserted,
        updated = summary.updated,
        unchanged = summary.unchanged,
        skipped = summary.skipped,
        "account import from '{path}'"
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::path::PathBuf;

    fn tmp_file(name: &str, content: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("r2a-import-test-{name}-{}.json", std::process::id()));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn imports_and_upserts_by_email() {
        let store = AccountStore::new(Db::open(":memory:").unwrap());
        let file = tmp_file(
            "basic",
            r#"[{ "tag": "a", "user": { "email": "a@x" }, "access_token": "t1" },
                { "tag": "b", "user": { "email": "b@x" }, "access_token": "t2" }]"#,
        );
        let s = import_file(&store, file.to_str().unwrap()).unwrap();
        assert_eq!(s, ImportSummary { inserted: 2, ..Default::default() });

        // token 变化 → 更新;相同 → 跳过;无 email → skipped
        let file2 = tmp_file(
            "second",
            r#"[{ "tag": "a", "user": { "email": "a@x" }, "access_token": "t9" },
                { "tag": "b", "user": { "email": "b@x" }, "access_token": "t2" },
                { "tag": "bad", "access_token": "t3" }]"#,
        );
        let s = import_file(&store, file2.to_str().unwrap()).unwrap();
        assert_eq!(
            s,
            ImportSummary { inserted: 0, updated: 1, unchanged: 1, skipped: 1 }
        );
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&file2);
    }

    #[test]
    fn missing_file_is_silent_noop() {
        let store = AccountStore::new(Db::open(":memory:").unwrap());
        let s = import_file(&store, "/nonexistent/account.json").unwrap();
        assert_eq!(s, ImportSummary::default());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn malformed_file_is_config_error() {
        let store = AccountStore::new(Db::open(":memory:").unwrap());
        let file = tmp_file("bad", "not json");
        assert!(matches!(
            import_file(&store, file.to_str().unwrap()).unwrap_err(),
            GatewayError::Config(_)
        ));
        let _ = std::fs::remove_file(&file);
    }
}
