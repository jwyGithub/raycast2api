//! 模型别名存储:`model_aliases` 表的读写与内存缓存。
//!
//! 别名(客户端请求里写的名字)是主键,所以同一个别名只能映射到一个模型;
//! 一个模型可以有多个别名。请求路径只读内存缓存,写入时同步更新库与缓存。

use std::collections::HashMap;
use std::sync::RwLock;

use diesel::{connection::SimpleConnection, prelude::*};
use serde::Serialize;

use crate::storage::{
    db::{Db, blocking},
    schema::model_aliases as t,
};
use crate::util::unix_secs;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS model_aliases (
    alias TEXT PRIMARY KEY NOT NULL, model_id TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_model_aliases_model ON model_aliases(model_id);
";

/// 新增别名失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum CreateError {
    /// 别名已存在,携带它当前映射到的模型。
    Exists(String),
    Db(String),
}

/// 控制台展示用的别名条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AliasEntry {
    pub alias: String,
    /// 创建时间(Unix 秒)。
    pub created_at: i64,
}

/// 缓存里别名指向的目标。
struct Target {
    model_id: String,
    created_at: i64,
}

pub struct ModelAliasStore {
    db: Db,
    cache: RwLock<HashMap<String, Target>>,
}

impl ModelAliasStore {
    /// 建表(如不存在)并把已有别名加载进缓存。
    pub fn new(db: Db) -> Self {
        let rows = blocking(|| -> QueryResult<Vec<(String, String, i64)>> {
            let mut conn = db.lock();
            conn.batch_execute(SCHEMA)?;
            t::table
                .select((t::alias, t::model_id, t::created_at))
                .load(&mut *conn)
        })
        .expect("failed to initialize model alias table");
        let cache = rows
            .into_iter()
            .map(|(alias, model_id, created_at)| {
                (
                    alias,
                    Target {
                        model_id,
                        created_at,
                    },
                )
            })
            .collect();
        Self {
            db,
            cache: RwLock::new(cache),
        }
    }

    /// 别名 → 目标模型 ID(精确匹配,一跳);不是别名返回 `None`。
    pub fn resolve(&self, alias: &str) -> Option<String> {
        self.cache
            .read()
            .expect("alias cache poisoned")
            .get(alias)
            .map(|target| target.model_id.clone())
    }

    /// 按目标模型分组的别名(组内按别名升序),给控制台模型列表使用。
    pub fn by_model(&self) -> HashMap<String, Vec<AliasEntry>> {
        let mut grouped: HashMap<String, Vec<AliasEntry>> = HashMap::new();
        for (alias, target) in self.cache.read().expect("alias cache poisoned").iter() {
            grouped
                .entry(target.model_id.clone())
                .or_default()
                .push(AliasEntry {
                    alias: alias.clone(),
                    created_at: target.created_at,
                });
        }
        grouped
            .values_mut()
            .for_each(|aliases| aliases.sort_by(|a, b| a.alias.cmp(&b.alias)));
        grouped
    }

    /// 新增别名;已存在则拒绝(不覆盖,要换目标须先删除)。
    pub fn create(&self, alias: &str, model_id: &str) -> Result<(), CreateError> {
        self.create_with_previous_id(alias, model_id, None)
    }

    /// 创建映射，或将同一模型的原始 ID 更新为当前目录 ID。
    pub(crate) fn create_with_previous_id(
        &self,
        alias: &str,
        model_id: &str,
        previous_id: Option<&str>,
    ) -> Result<(), CreateError> {
        // 写锁覆盖整个"检查 + 落库 + 更新缓存",写入本来就稀少,换来无竞态。
        let mut cache = self.cache.write().expect("alias cache poisoned");
        if let Some(existing) = cache.get_mut(alias) {
            let old_id = existing.model_id.as_str();
            if old_id == model_id || previous_id != Some(old_id) {
                return Err(CreateError::Exists(old_id.to_owned()));
            }
            let updated = blocking(|| {
                diesel::update(
                    t::table
                        .filter(t::alias.eq(alias))
                        .filter(t::model_id.eq(old_id)),
                )
                .set(t::model_id.eq(model_id))
                .execute(&mut *self.db.lock())
            })
            .map_err(|error| CreateError::Db(error.to_string()))?;
            if updated == 0 {
                return Err(CreateError::Db(format!(
                    "alias '{alias}' changed in storage"
                )));
            }
            existing.model_id = model_id.to_owned();
            return Ok(());
        }
        let created_at = unix_secs() as i64;
        blocking(|| {
            diesel::insert_into(t::table)
                .values((
                    t::alias.eq(alias),
                    t::model_id.eq(model_id),
                    t::created_at.eq(created_at),
                ))
                .execute(&mut *self.db.lock())
        })
        .map_err(|e| CreateError::Db(e.to_string()))?;
        cache.insert(
            alias.to_owned(),
            Target {
                model_id: model_id.to_owned(),
                created_at,
            },
        );
        Ok(())
    }

    /// 删除别名,返回它此前是否存在。
    pub fn delete(&self, alias: &str) -> Result<bool, String> {
        let mut cache = self.cache.write().expect("alias cache poisoned");
        let deleted = blocking(|| {
            diesel::delete(t::table.filter(t::alias.eq(alias))).execute(&mut *self.db.lock())
        })
        .map_err(|e| e.to_string())?;
        cache.remove(alias);
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Db, ModelAliasStore) {
        let db = Db::open(":memory:").unwrap();
        let store = ModelAliasStore::new(db.clone());
        (db, store)
    }

    #[test]
    fn one_alias_maps_to_one_model_but_a_model_can_have_many_aliases() {
        let (_, s) = store();
        s.create("claude-sonnet-5-5", "anthropic-claude-fable-5-2")
            .unwrap();
        // 同一别名换目标、或重复添加,都不允许,且不改动已有映射。
        for target in ["anthropic-claude-fable-5-1", "anthropic-claude-fable-5-2"] {
            assert_eq!(
                s.create("claude-sonnet-5-5", target),
                Err(CreateError::Exists("anthropic-claude-fable-5-2".into()))
            );
        }
        assert_eq!(
            s.resolve("claude-sonnet-5-5").as_deref(),
            Some("anthropic-claude-fable-5-2")
        );
        // 不同别名可以指向同一个模型。
        s.create("a-second-alias", "anthropic-claude-fable-5-2")
            .unwrap();
        let names: Vec<_> = s.by_model()["anthropic-claude-fable-5-2"]
            .iter()
            .map(|entry| entry.alias.clone())
            .collect();
        assert_eq!(names, ["a-second-alias", "claude-sonnet-5-5"]);
    }

    #[test]
    fn delete_frees_the_alias_for_a_new_target() {
        let (_, s) = store();
        s.create("x", "m1").unwrap();
        assert!(s.delete("x").unwrap());
        assert!(!s.delete("x").unwrap());
        assert_eq!(s.resolve("x"), None);
        s.create("x", "m2").unwrap();
        assert_eq!(s.resolve("x").as_deref(), Some("m2"));
    }

    #[test]
    fn aliases_survive_reopen() {
        let (db, s) = store();
        s.create("keep", "m1").unwrap();
        s.create("gone", "m2").unwrap();
        s.delete("gone").unwrap();
        // 模拟重启:同一个库重新加载,缓存应与库一致。
        let reopened = ModelAliasStore::new(db);
        assert_eq!(reopened.resolve("keep").as_deref(), Some("m1"));
        assert_eq!(reopened.resolve("gone"), None);
        // 创建时间随库持久化,重启后不丢。
        assert_eq!(reopened.by_model()["m1"], s.by_model()["m1"]);
        assert!(reopened.by_model()["m1"][0].created_at > 0);
    }
}
