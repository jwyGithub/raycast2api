//! 页面管理的代理配置、延迟及账号引用，存入共享 SQLite。
use std::{collections::HashMap, sync::Mutex, time::Duration};

use crate::{
    error::GatewayError,
    storage::db::{Db, blocking},
    util::{new_ulid, unix_secs},
};
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    sql_types::{BigInt, Integer, Nullable, Text},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

// 免费 IP-API 不支持 HTTPS；查询请求也必须经过待测试代理。
const EXIT_API_URL: &str = "http://ip-api.com/json";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyInput {
    #[serde(rename = "type")]
    pub kind: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub password: String,
}

impl ProxyInput {
    fn validated(mut self) -> Result<Self, GatewayError> {
        self.host = self.host.trim().to_owned();
        if !matches!(self.kind.as_str(), "http" | "https" | "socks5")
            || self.port == 0
            || self.host.is_empty()
            || self.host.contains(['/', '@', '?', '#'])
        {
            return Err(GatewayError::BadRequest(
                "代理类型、HOST 或 PORT 无效".into(),
            ));
        }
        self.url()?;
        Ok(self)
    }

    fn url(&self) -> Result<reqwest::Url, GatewayError> {
        let mut url = reqwest::Url::parse(&format!("{}://localhost", self.kind))
            .map_err(|_| GatewayError::BadRequest("代理类型无效".into()))?;
        url.set_host(Some(&self.host))
            .map_err(|_| GatewayError::BadRequest("HOST 无效".into()))?;
        url.set_port(Some(self.port))
            .map_err(|_| GatewayError::BadRequest("PORT 无效".into()))?;
        Ok(url)
    }

    fn client(
        &self,
        timeout: Duration,
        transport: super::Transport,
    ) -> Result<Client, GatewayError> {
        let mut proxy = reqwest::Proxy::all(self.url()?)
            .map_err(|_| GatewayError::BadRequest("代理配置无效".into()))?;
        if !self.user_name.is_empty() || !self.password.is_empty() {
            // SOCKS credentials must be encoded in the URL; basic_auth is HTTP-only.
            if self.kind == "socks5" {
                let mut url = self.url()?;
                url.set_username(&self.user_name)
                    .map_err(|_| GatewayError::BadRequest("USER_NAME 无效".into()))?;
                url.set_password(Some(&self.password))
                    .map_err(|_| GatewayError::BadRequest("PASSWORD 无效".into()))?;
                proxy = reqwest::Proxy::all(url)
                    .map_err(|_| GatewayError::BadRequest("代理配置无效".into()))?;
            } else {
                proxy = proxy.basic_auth(&self.user_name, &self.password);
            }
        }
        super::base_builder(timeout, transport)
            .no_proxy()
            .proxy(proxy)
            .build()
            .map_err(|_| GatewayError::BadRequest("代理客户端构造失败".into()))
    }
}

#[derive(Clone, Serialize, QueryableByName)]
pub struct ProxyRow {
    #[diesel(sql_type = Text)]
    pub id: String,
    #[serde(rename = "type")]
    #[diesel(sql_type = Text)]
    pub kind: String,
    #[diesel(sql_type = Text)]
    pub host: String,
    #[diesel(sql_type = Integer)]
    pub port: i32,
    #[diesel(sql_type = Text)]
    pub user_name: String,
    #[diesel(sql_type = Text)]
    pub password: String,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub latency_ms: Option<i64>,
    /// 最近一次测试完成时间（Unix 秒），包含失败的测试。
    #[diesel(sql_type = Nullable<BigInt>)]
    pub tested_at: Option<i64>,
    #[diesel(sql_type = Nullable<Text>)]
    pub exit_ip: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub exit_location: Option<String>,
    /// 保存 IP-API 原始 JSON，接口按对象返回，避免丢失扩展字段。
    #[serde(serialize_with = "serialize_exit_details")]
    #[diesel(sql_type = Nullable<Text>)]
    pub exit_details: Option<String>,
    /// 创建时间（Unix 秒）；旧记录无法恢复时为空。
    #[diesel(sql_type = Nullable<BigInt>)]
    pub created_at: Option<i64>,
    #[serde(skip)]
    #[diesel(sql_type = Text)]
    pub revision: String,
}

/// 列表额外返回绑定账号数量，实时统计而非保存计数。
#[derive(Serialize)]
pub struct ProxyListItem {
    #[serde(flatten)]
    pub proxy: ProxyRow,
    pub account_count: i64,
}

#[derive(Serialize)]
pub struct ProxyProbeResult {
    pub latency_ms: i64,
    pub exit_ip: Option<String>,
    pub exit_location: Option<String>,
    pub exit_details: Option<serde_json::Value>,
    pub exit_error: Option<String>,
}

#[derive(Serialize)]
pub struct ProxyTestResult {
    #[serde(flatten)]
    pub probe: ProxyProbeResult,
    pub tested_at: i64,
}

fn serialize_exit_details<S: serde::Serializer>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .serialize(serializer)
}

struct ProxyExit {
    ip: String,
    location: String,
    details: serde_json::Value,
}

impl ProxyRow {
    fn input(&self) -> ProxyInput {
        ProxyInput {
            kind: self.kind.clone(),
            host: self.host.clone(),
            port: self.port as u16,
            user_name: self.user_name.clone(),
            password: self.password.clone(),
        }
    }
}

type Clients = (Client, Client);
pub struct ProxyStore {
    db: Db,
    clients: Mutex<HashMap<(String, String, Duration), Clients>>,
}

fn sql_error(error: diesel::result::Error) -> GatewayError {
    GatewayError::Internal(format!("proxy store: {error}"))
}

/// 旧代理 ID 由 new_ulid 生成，前 10 位包含创建时的毫秒时间戳。
fn creation_time_from_id(id: &str) -> Option<i64> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    if id.len() != 26 || !id.bytes().all(|byte| ALPHABET.contains(&byte)) {
        return None;
    }
    let millis = id.bytes().take(10).try_fold(0u64, |value, byte| {
        let digit = ALPHABET.iter().position(|candidate| *candidate == byte)?;
        Some((value << 5) | digit as u64)
    })?;
    (millis <= 0xFFFF_FFFF_FFFF).then_some((millis / 1000) as i64)
}

impl ProxyStore {
    pub fn new(db: Db) -> Result<Self, String> {
        blocking(|| -> QueryResult<()> {
            let mut conn = db.lock();
            conn.batch_execute(
                "CREATE TABLE IF NOT EXISTS proxies (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, host TEXT NOT NULL, port INTEGER NOT NULL,
                user_name TEXT NOT NULL, password TEXT NOT NULL, latency_ms INTEGER,
                created_at INTEGER, tested_at INTEGER, exit_ip TEXT, exit_location TEXT, exit_details TEXT,
                revision TEXT NOT NULL
            );",
            )?;
            #[derive(QueryableByName)]
            struct Column {
                #[diesel(sql_type = Text)]
                name: String,
            }
            let columns =
                diesel::sql_query("PRAGMA table_info(proxies)").load::<Column>(&mut *conn)?;
            if !columns.iter().any(|column| column.name == "tested_at") {
                conn.batch_execute("ALTER TABLE proxies ADD COLUMN tested_at INTEGER;")?;
            }
            if !columns.iter().any(|column| column.name == "exit_ip") {
                conn.batch_execute("ALTER TABLE proxies ADD COLUMN exit_ip TEXT;")?;
            }
            if !columns.iter().any(|column| column.name == "exit_location") {
                conn.batch_execute("ALTER TABLE proxies ADD COLUMN exit_location TEXT;")?;
            }
            if !columns.iter().any(|column| column.name == "exit_details") {
                conn.batch_execute("ALTER TABLE proxies ADD COLUMN exit_details TEXT;")?;
            }
            if !columns.iter().any(|column| column.name == "created_at") {
                conn.transaction::<_, diesel::result::Error, _>(|conn| {
                    conn.batch_execute("ALTER TABLE proxies ADD COLUMN created_at INTEGER;")?;
                    let rows = diesel::sql_query("SELECT * FROM proxies").load::<ProxyRow>(conn)?;
                    for row in rows {
                        if let Some(created_at) = creation_time_from_id(&row.id) {
                            diesel::sql_query("UPDATE proxies SET created_at=? WHERE id=?")
                                .bind::<BigInt, _>(created_at)
                                .bind::<Text, _>(&row.id)
                                .execute(conn)?;
                        }
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })
        .map_err(|e| format!("create proxies: {e}"))?;
        Ok(Self {
            db,
            clients: Mutex::new(HashMap::new()),
        })
    }

    /// 账号卡片的代理地址，批量查询，不包含代理认证凭据。
    pub fn connection_labels(&self) -> Result<HashMap<String, String>, GatewayError> {
        #[derive(QueryableByName)]
        struct Label {
            #[diesel(sql_type = Text)]
            id: String,
            #[diesel(sql_type = Text)]
            kind: String,
            #[diesel(sql_type = Text)]
            host: String,
            #[diesel(sql_type = Integer)]
            port: i32,
        }
        blocking(|| {
            let rows = diesel::sql_query("SELECT id,kind,host,port FROM proxies")
                .load::<Label>(&mut *self.db.lock())
                .map_err(sql_error)?;
            Ok(rows
                .into_iter()
                .map(|row| {
                    let host = if row.host.contains(':') && !row.host.starts_with('[') {
                        format!("[{}]", row.host)
                    } else {
                        row.host
                    };
                    (row.id, format!("{}://{}:{}", row.kind, host, row.port))
                })
                .collect())
        })
    }

    pub fn list(&self) -> Result<Vec<ProxyListItem>, GatewayError> {
        blocking(|| {
            let mut conn = self.db.lock();
            let proxies = diesel::sql_query("SELECT * FROM proxies ORDER BY id")
                .load::<ProxyRow>(&mut *conn)
                .map_err(sql_error)?;
            #[derive(QueryableByName)]
            struct AccountCount {
                #[diesel(sql_type = Text)]
                proxy: String,
                #[diesel(sql_type = BigInt)]
                count: i64,
            }
            let counts =
                diesel::sql_query("SELECT proxy, COUNT(*) AS count FROM accounts GROUP BY proxy")
                    .load::<AccountCount>(&mut *conn)
                    .map_err(sql_error)?
                    .into_iter()
                    .map(|row| (row.proxy, row.count))
                    .collect::<HashMap<_, _>>();
            Ok(proxies
                .into_iter()
                .map(|proxy| ProxyListItem {
                    account_count: counts.get(&proxy.id).copied().unwrap_or(0),
                    proxy,
                })
                .collect())
        })
    }

    pub fn get(&self, id: &str) -> Result<ProxyRow, GatewayError> {
        blocking(|| {
            diesel::sql_query("SELECT * FROM proxies WHERE id = ?")
                .bind::<Text, _>(id)
                .get_result(&mut *self.db.lock())
                .optional()
                .map_err(sql_error)?
                .ok_or_else(|| GatewayError::NotFound("代理不存在".into()))
        })
    }

    pub fn save(&self, id: Option<&str>, input: ProxyInput) -> Result<ProxyRow, GatewayError> {
        let input = input.validated()?;
        // Validate construction before committing; credentials never enter error text.
        input.client(Duration::from_secs(20), super::Transport::ChatFetch)?;
        let key = id.map(str::to_owned).unwrap_or_else(new_ulid);
        let revision = new_ulid();
        blocking(|| {
            let mut conn = self.db.lock();
            let sql = if id.is_some() {
                "UPDATE proxies SET kind=?,host=?,port=?,user_name=?,password=?,revision=?,latency_ms=NULL,tested_at=NULL,exit_ip=NULL,exit_location=NULL,exit_details=NULL WHERE id=?"
            } else {
                "INSERT INTO proxies (kind,host,port,user_name,password,revision,id,created_at) VALUES (?,?,?,?,?,?,?,?)"
            };
            let query = diesel::sql_query(sql)
                .bind::<Text, _>(&input.kind)
                .bind::<Text, _>(&input.host)
                .bind::<Integer, _>(i32::from(input.port))
                .bind::<Text, _>(&input.user_name)
                .bind::<Text, _>(&input.password)
                .bind::<Text, _>(&revision)
                .bind::<Text, _>(&key);
            let n = if id.is_some() {
                query.execute(&mut *conn)
            } else {
                query
                    .bind::<BigInt, _>(unix_secs() as i64)
                    .execute(&mut *conn)
            }
            .map_err(sql_error)?;
            if n == 0 {
                return Err(GatewayError::NotFound("代理不存在".into()));
            }
            Ok(())
        })?;
        self.clients
            .lock()
            .expect("proxy clients lock")
            .retain(|(id, _, _), _| id != &key);
        self.get(&key)
    }

    pub fn delete(&self, id: &str) -> Result<(), GatewayError> {
        blocking(|| {
            let mut conn = self.db.lock();
            #[derive(QueryableByName)]
            struct Count {
                #[diesel(sql_type = BigInt)]
                count: i64,
            }
            // Same connection lock as binding: checking and deleting cannot race.
            let count = diesel::sql_query("SELECT COUNT(*) AS count FROM accounts WHERE proxy = ?")
                .bind::<Text, _>(id)
                .get_result::<Count>(&mut *conn)
                .map_err(sql_error)?
                .count;
            if count > 0 {
                return Err(GatewayError::BadRequest(
                    "代理正在被账号使用，请先修改账号代理设置".into(),
                ));
            }
            if diesel::sql_query("DELETE FROM proxies WHERE id=?")
                .bind::<Text, _>(id)
                .execute(&mut *conn)
                .map_err(sql_error)?
                == 0
            {
                return Err(GatewayError::NotFound("代理不存在".into()));
            }
            Ok(())
        })?;
        self.clients
            .lock()
            .expect("proxy clients lock")
            .retain(|(key, _, _), _| key != id);
        Ok(())
    }

    pub fn set_account_proxy(&self, email: &str, proxy: &str) -> Result<(), GatewayError> {
        self.set_account_proxy_options(email, proxy, None)
    }

    pub fn set_account_proxy_options(
        &self,
        email: &str,
        proxy: &str,
        fallback_direct: Option<bool>,
    ) -> Result<(), GatewayError> {
        blocking(|| {
            let mut conn = self.db.lock();
            if proxy != "direct"
                && diesel::sql_query("SELECT * FROM proxies WHERE id=?")
                    .bind::<Text, _>(proxy)
                    .get_result::<ProxyRow>(&mut *conn)
                    .optional()
                    .map_err(sql_error)?
                    .is_none()
            {
                return Err(GatewayError::BadRequest("代理不存在".into()));
            }
            if diesel::sql_query("UPDATE accounts SET proxy=?, proxy_fallback_direct=CASE WHEN ?='direct' THEN 0 ELSE COALESCE(?, proxy_fallback_direct) END, updated_at=? WHERE email=?")
                .bind::<Text, _>(proxy)
                .bind::<Text, _>(proxy)
                .bind::<Nullable<Integer>, _>(fallback_direct.map(i32::from))
                .bind::<Text, _>(crate::util::unix_secs().to_string())
                .bind::<Text, _>(email)
                .execute(&mut *conn)
                .map_err(sql_error)?
                == 0
            {
                return Err(GatewayError::NotFound("账号不存在".into()));
            }
            Ok(())
        })
    }

    pub fn clients(&self, id: &str, timeout: Duration) -> Result<Clients, GatewayError> {
        let row = self.get(id)?;
        let key = (row.id.clone(), row.revision.clone(), timeout);
        let mut cache = self.clients.lock().expect("proxy clients lock");
        if let Some(clients) = cache.get(&key) {
            return Ok(clients.clone());
        }
        let input = row.input();
        let clients = (
            input.client(timeout, super::Transport::Got)?,
            input.client(timeout, super::Transport::ChatFetch)?,
        );
        cache.insert(key, clients.clone());
        Ok(clients)
    }

    /// 测试尚未保存的表单配置，仅发起网络请求，不访问数据库。
    pub async fn test_input(
        input: ProxyInput,
        timeout: Duration,
    ) -> Result<ProxyProbeResult, GatewayError> {
        let input = input.validated()?;
        Self::probe_url(
            &input,
            "unsaved",
            timeout,
            crate::config::RAYCAST_API_URL,
            EXIT_API_URL,
        )
        .await
    }

    pub async fn test(&self, id: &str, timeout: Duration) -> Result<ProxyTestResult, GatewayError> {
        self.test_url(id, timeout, crate::config::RAYCAST_API_URL, EXIT_API_URL)
            .await
    }

    async fn test_url(
        &self,
        id: &str,
        timeout: Duration,
        url: &str,
        exit_url: &str,
    ) -> Result<ProxyTestResult, GatewayError> {
        let row = self.get(id)?;
        let result = Self::probe_url(&row.input(), id, timeout, url, exit_url).await;
        let tested_at = unix_secs() as i64;
        let exit_details = result
            .as_ref()
            .ok()
            .and_then(|probe| probe.exit_details.as_ref())
            .map(serde_json::Value::to_string);
        // 连通性失败仅更新测试时间；成功时保存本次出口，查询失败时出口为空。
        let n = blocking(|| {
            let mut conn = self.db.lock();
            match &result {
                Ok(probe) => diesel::sql_query("UPDATE proxies SET latency_ms=?,exit_ip=?,exit_location=?,exit_details=?,tested_at=? WHERE id=? AND revision=?")
                    .bind::<BigInt, _>(probe.latency_ms)
                    .bind::<Nullable<Text>, _>(probe.exit_ip.as_deref())
                    .bind::<Nullable<Text>, _>(probe.exit_location.as_deref())
                    .bind::<Nullable<Text>, _>(exit_details.as_deref())
                    .bind::<BigInt, _>(tested_at)
                    .bind::<Text, _>(id)
                    .bind::<Text, _>(&row.revision)
                    .execute(&mut *conn),
                Err(_) => diesel::sql_query("UPDATE proxies SET tested_at=? WHERE id=? AND revision=?")
                    .bind::<BigInt, _>(tested_at)
                    .bind::<Text, _>(id)
                    .bind::<Text, _>(&row.revision)
                    .execute(&mut *conn),
            }.map_err(sql_error)
        }).inspect_err(|error| {
            crate::trace_warn!("proxy-test", "测试结果保存失败 proxy={} error={}", id, error);
        })?;
        if n == 0 {
            crate::trace_warn!(
                "proxy-test",
                "测试期间配置已修改或删除，未保存结果 proxy={}",
                id
            );
            return Err(GatewayError::BadRequest(
                "测试期间代理已修改或删除，请重新测试".into(),
            ));
        }
        let probe = result?;
        crate::trace_info!(
            "proxy-test",
            "测试完成，延迟、出口及测试时间已保存 proxy={} latency={}ms tested_at={}",
            id,
            probe.latency_ms,
            tested_at
        );
        Ok(ProxyTestResult { probe, tested_at })
    }

    /// 新建连接测量上游延迟，出口查询耗时不计入延迟，不回退直连。
    async fn probe_url(
        input: &ProxyInput,
        id: &str,
        timeout: Duration,
        url: &str,
        exit_url: &str,
    ) -> Result<ProxyProbeResult, GatewayError> {
        let request_id = crate::log::context::current_request_id();
        crate::trace_info!(
            "proxy-test",
            "测试开始 proxy={} type={} host={}:{} upstream={} timeout={}ms",
            id,
            input.kind,
            input.host,
            input.port,
            url,
            timeout.as_millis()
        );
        let client = input
            .client(timeout, super::Transport::ChatFetch)
            .inspect_err(|error| {
                crate::trace_warn!(
                    "proxy-test",
                    "测试客户端构造失败 proxy={} error={}",
                    id,
                    error
                );
            })?;
        let started = std::time::Instant::now();
        let response = client
            .post(url)
            .header("User-Agent", crate::config::USER_AGENT_POST)
            .json(&serde_json::json!({}))
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                crate::trace_warn!(
                    "proxy-test",
                    "测试请求失败 proxy={} elapsed={}ms is_timeout={} is_connect={} error={}",
                    id,
                    started.elapsed().as_millis(),
                    error.is_timeout(),
                    error.is_connect(),
                    crate::error::error_chain(&error)
                );
                GatewayError::Upstream {
                    status: 502,
                    message: "代理测试连接失败或超时".into(),
                }
            })?;
        let status = response.status().as_u16();
        let latency_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        if request_id != "-" {
            crate::log::context::record_channel(&request_id, id);
        }
        crate::trace_info!(
            "proxy-test",
            "收到上游响应 proxy={} status={} latency={}ms",
            id,
            status,
            latency_ms
        );
        // 未提供账号凭据，400/401/405/422 属于预期响应。
        if !(response.status().is_success() || matches!(status, 400 | 401 | 405 | 422)) {
            crate::trace_warn!(
                "proxy-test",
                "响应状态不符合预期 proxy={} status={}",
                id,
                status
            );
            return Err(GatewayError::Upstream {
                status: 502,
                message: format!("代理测试失败：HTTP {status}"),
            });
        }
        drop(response);
        let (exit_ip, exit_location, exit_details, exit_error) =
            match Self::query_exit(&client, id, timeout, exit_url).await {
                Ok(exit) => (Some(exit.ip), Some(exit.location), Some(exit.details), None),
                Err(error) => {
                    crate::trace_warn!(
                        "proxy-test",
                        "代理出口查询失败 proxy={} error={}",
                        id,
                        error
                    );
                    (None, None, None, Some(error.to_string()))
                }
            };
        Ok(ProxyProbeResult {
            latency_ms,
            exit_ip,
            exit_location,
            exit_details,
            exit_error,
        })
    }

    async fn query_exit(
        client: &Client,
        id: &str,
        timeout: Duration,
        url: &str,
    ) -> Result<ProxyExit, GatewayError> {
        crate::trace_info!(
            "proxy-test",
            "代理出口查询开始 proxy={} exit_api={}",
            id,
            url
        );
        let response = client
            .get(url)
            .query(&[("lang", "zh-CN")])
            .timeout(timeout.min(Duration::from_secs(10)))
            .send()
            .await
            .map_err(|_| GatewayError::Upstream {
                status: 502,
                message: "出口查询连接失败或超时".into(),
            })?;
        if !response.status().is_success() {
            return Err(GatewayError::Upstream {
                status: 502,
                message: if response.status().as_u16() == 429 {
                    "出口查询被限流，请稍后重试".into()
                } else {
                    format!("出口查询失败：HTTP {}", response.status().as_u16())
                },
            });
        }
        #[derive(Deserialize)]
        struct IpApiResponse {
            status: String,
            #[serde(default)]
            query: String,
            #[serde(default)]
            country: String,
            #[serde(default, rename = "regionName")]
            region: String,
            #[serde(default)]
            city: String,
        }
        let details =
            response
                .json::<serde_json::Value>()
                .await
                .map_err(|_| GatewayError::Upstream {
                    status: 502,
                    message: "出口查询响应格式无效".into(),
                })?;
        let data = serde_json::from_value::<IpApiResponse>(details.clone()).map_err(|_| {
            GatewayError::Upstream {
                status: 502,
                message: "出口查询响应格式无效".into(),
            }
        })?;
        if data.status != "success" {
            return Err(GatewayError::Upstream {
                status: 502,
                message: "出口查询返回失败".into(),
            });
        }
        let ip = data
            .query
            .parse::<std::net::IpAddr>()
            .map_err(|_| GatewayError::Upstream {
                status: 502,
                message: "出口查询未返回有效 IP".into(),
            })?
            .to_string();
        let mut parts = Vec::new();
        for part in [&data.country, &data.region, &data.city] {
            let part = part.trim();
            if !part.is_empty() && !parts.contains(&part) {
                parts.push(part);
            }
        }
        let location = if parts.is_empty() {
            "未知地区".into()
        } else {
            parts.join(" / ")
        };
        crate::trace_info!(
            "proxy-test",
            "代理出口查询完成 proxy={} exit_ip={} exit_location={}",
            id,
            ip,
            location
        );
        Ok(ProxyExit {
            ip,
            location,
            details,
        })
    }

    /// 优先返回测试快照；旧记录按已保存的 IP 补查详情，避免轮换出口导致 IP 不一致。
    pub async fn exit_details(
        &self,
        id: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, GatewayError> {
        let row = self.get(id)?;
        if let Some(details) = row
            .exit_details
            .as_deref()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        {
            return Ok(details);
        }
        let ip = row
            .exit_ip
            .as_deref()
            .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
            .ok_or_else(|| GatewayError::BadRequest("暂无出口 IP，请先测试代理".into()))?
            .to_string();
        let client = row.input().client(timeout, super::Transport::ChatFetch)?;
        let exit = Self::query_exit(&client, id, timeout, &format!("{EXIT_API_URL}/{ip}")).await?;
        if exit.ip != ip {
            return Err(GatewayError::Upstream {
                status: 502,
                message: "出口详情返回的 IP 与查询 IP 不一致".into(),
            });
        }
        let n = blocking(|| {
            diesel::sql_query("UPDATE proxies SET exit_details=? WHERE id=? AND revision=? AND exit_ip=? AND exit_details IS ?")
                .bind::<Text, _>(exit.details.to_string())
                .bind::<Text, _>(id)
                .bind::<Text, _>(&row.revision)
                .bind::<Text, _>(row.exit_ip.as_deref().unwrap_or_default())
                .bind::<Nullable<Text>, _>(row.exit_details.as_deref())
                .execute(&mut *self.db.lock())
                .map_err(sql_error)
        })?;
        if n == 0 {
            return Err(GatewayError::BadRequest(
                "出口信息已更新，请重新打开详情".into(),
            ));
        }
        Ok(exit.details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{store::AccountStore, wire::AccountRecord};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn input(port: u16) -> ProxyInput {
        ProxyInput {
            kind: "http".into(),
            host: "127.0.0.1".into(),
            port,
            user_name: "user".into(),
            password: "pass".into(),
        }
    }
    fn record(email: &str) -> AccountRecord {
        serde_json::from_value(serde_json::json!({"user":{"email":email},"device_id":"1".repeat(64),"access_token":"fixture"})).unwrap()
    }

    #[test]
    fn imports_disable_rotation_and_settings_survive_reopen() {
        let path = std::env::temp_dir().join(format!("r2a-proxies-{}.db", new_ulid()));
        {
            let db = Db::open(path.to_str().unwrap()).unwrap();
            let accounts = AccountStore::new(db.clone());
            let proxies = ProxyStore::new(db).unwrap();
            accounts.upsert(&record("a@test")).unwrap();
            let account = accounts.get("a@test").unwrap().unwrap();
            assert_eq!(account.use_status, "disable");
            assert_eq!(account.proxy, "direct");
            assert_eq!(accounts.count_available(), 0);
            assert!(accounts.next_available().is_err());
            let proxy = proxies.save(None, input(8080)).unwrap();
            assert_eq!(proxy.latency_ms, None);
            assert!(proxies.set_account_proxy("a@test", "missing").is_err());
            proxies.set_account_proxy("a@test", &proxy.id).unwrap();
            accounts.set_use_status("a@test", "enable").unwrap();
            assert_eq!(accounts.next_available().unwrap().proxy, proxy.id);
            assert!(proxies.delete(&proxy.id).is_err());
            accounts.mark_unavailable("a@test", "token invalid");
            accounts.set_use_status("a@test", "disable").unwrap();
            accounts.reinstate("a@test").unwrap();
            assert_eq!(accounts.count_available(), 0);
            accounts.upsert(&record("a@test")).unwrap();
            assert_eq!(
                accounts.get("a@test").unwrap().unwrap().use_status,
                "disable"
            );
        }
        {
            let db = Db::open(path.to_str().unwrap()).unwrap();
            let accounts = AccountStore::new(db.clone());
            let proxies = ProxyStore::new(db).unwrap();
            let proxy = proxies.list().unwrap().pop().unwrap().proxy;
            assert_eq!(accounts.get("a@test").unwrap().unwrap().proxy, proxy.id);
            proxies.set_account_proxy("a@test", "direct").unwrap();
            proxies.delete(&proxy.id).unwrap();
            assert!(proxies.list().unwrap().is_empty());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn old_accounts_migrate_as_enabled_and_new_imports_are_disabled() {
        let db = Db::open(":memory:").unwrap();
        let accounts = AccountStore::new(db.clone());
        accounts.upsert(&record("old@test")).unwrap();
        // Recreate the pre-migration shape, retaining an existing row.
        db.lock().batch_execute("ALTER TABLE accounts DROP COLUMN use_status; ALTER TABLE accounts DROP COLUMN proxy;").unwrap();
        let accounts = AccountStore::new(db);
        assert_eq!(
            accounts.get("old@test").unwrap().unwrap().use_status,
            "enable"
        );
        accounts.upsert(&record("new@test")).unwrap();
        assert_eq!(
            accounts.get("new@test").unwrap().unwrap().use_status,
            "disable"
        );
    }

    #[tokio::test]
    async fn test_uses_selected_proxy_persists_latency_and_rejects_stale_results() {
        let path = std::env::temp_dir().join(format!("r2a-latency-{}.db", new_ulid()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let db = Db::open(path.to_str().unwrap()).unwrap();
        let store = std::sync::Arc::new(ProxyStore::new(db).unwrap());
        let proxy = store.save(None, input(port)).unwrap();
        let (entered_send, entered_receive) = tokio::sync::oneshot::channel();
        let (release_send, release_receive) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut entered_send = Some(entered_send);
            let mut release_receive = Some(release_receive);
            for (index, status) in [401, 401, 407].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]);
                assert!(
                    request.starts_with("POST http://chat.invalid/chat"),
                    "{request}"
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("proxy-authorization: basic dxnlcjpwyxnz")
                );
                if index == 1 {
                    entered_send.take().unwrap().send(()).unwrap();
                    release_receive.take().unwrap().await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                if status == 401 {
                    // 出口接口也使用本地代理夹具，避免既有测试访问公共服务。
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let n = socket.read(&mut bytes).await.unwrap();
                    let request = String::from_utf8_lossy(&bytes[..n]);
                    assert!(
                        request.starts_with("GET http://ip.invalid/json?"),
                        "{request}"
                    );
                    let body = r#"{"status":"success","query":"203.0.113.1","country":"Singapore","regionName":"Singapore","city":"Singapore"}"#;
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            }
        });
        let latency = store
            .test_url(
                &proxy.id,
                Duration::from_secs(1),
                "http://chat.invalid/chat",
                "http://ip.invalid/json",
            )
            .await
            .unwrap()
            .probe
            .latency_ms;
        assert!(latency >= 20);
        assert_eq!(store.get(&proxy.id).unwrap().latency_ms, Some(latency));
        let reopened = ProxyStore::new(Db::open(path.to_str().unwrap()).unwrap()).unwrap();
        assert_eq!(reopened.get(&proxy.id).unwrap().latency_ms, Some(latency));
        let task_store = store.clone();
        let id = proxy.id.clone();
        let testing = tokio::spawn(async move {
            task_store
                .test_url(
                    &id,
                    Duration::from_secs(1),
                    "http://chat.invalid/chat",
                    "http://ip.invalid/json",
                )
                .await
        });
        entered_receive.await.unwrap();
        store.save(Some(&proxy.id), input(port)).unwrap();
        release_send.send(()).unwrap();
        assert!(testing.await.unwrap().is_err());
        assert_eq!(store.get(&proxy.id).unwrap().latency_ms, None);
        assert!(
            store
                .test_url(
                    &proxy.id,
                    Duration::from_secs(1),
                    "http://chat.invalid/chat",
                    "http://ip.invalid/json"
                )
                .await
                .is_err()
        );
        assert_eq!(store.get(&proxy.id).unwrap().latency_ms, None);
        server.await.unwrap();
        drop(store);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }
}
