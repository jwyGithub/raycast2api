//! 共享 SQLite 基础设施:连接管理,不含任何业务表知识。
//!
//! 各业务模块(如 `account`)的 store 持有一个 [`Db`],自己执行各自的
//! `CREATE TABLE IF NOT EXISTS`。新增使用方不需要改动本文件。

use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

/// 一个共享的 SQLite 连接(引用计数 + 互斥)。
///
/// 网关流量下 SQLite 操作是微秒级,一把互斥锁足够;若未来需要异步
/// 驱动,只替换本文件,业务 store 的 `&Db` 签名不变。
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// 打开(或创建)数据库文件;`:memory:` 用于测试。
    /// WAL 与 busy timeout 让多线程/多进程读写更稳。
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path)
            .map_err(|e| format!("failed to open sqlite db '{path}': {e}"))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
            .map_err(|e| format!("failed to configure sqlite db '{path}': {e}"))?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// 锁住并访问底层连接。锁中毒(某处 panic)视为致命错误。
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().expect("db connection lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory_and_runs_sql() {
        let db = Db::open(":memory:").unwrap();
        db.lock()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        let x: i64 = db.lock().query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 1);
    }

    #[test]
    fn missing_file_directory_is_an_error() {
        assert!(Db::open("/nonexistent-dir/x.db").is_err());
    }
}
