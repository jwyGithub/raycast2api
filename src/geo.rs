//! 出口地区解析 —— chat 请求 `locale` / `<user-preferences>` 块的来源。
//!
//! 真客户端的 locale/timezone 来自运行机器的系统区域与本地时区;网关扮演
//! 的是"某台机器",这两个值必须与出口 IP 的地理一致,否则 locale/
//! timezone/IP 三者交叉就是异常信号。**不提供手动配置**:运行环境即身份,
//! 排除配置与运行之间的差异性。
//!
//! dev 网关是**账号级代理**:选号发生在请求体构建之后,所以解析挂在
//! dispatcher 的 `for_account` 之后——direct 账号经直连探测,代理账号经
//! 该账号自己的代理探测(`ip-api.com/json`,免费无 key,响应自带 IANA
//! timezone),结果按**账号代理 id** 缓存 [`EGRESS_CACHE_TTL`]:
//!
//! 1. 经当前账号的出口实测;结果按代理 id(或 `direct`)缓存——
//!    mihomo 切节点后出口 IP 变化,缓存自然失效;同节点复用期内零开销。
//! 2. 探测失败/超时 → 本机身份兜底,绝不阻塞推理请求;失败结果短暂
//!    负缓存,避免每请求都撞超时。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 出口探测结果的缓存时长。同出口 IP 的后续请求免查;超过后重新实测
/// (mihomo 节点切换表现为出口 IP 变化,缓存即时失效)。
const EGRESS_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// 探测失败后的负缓存时长:期间不再重试,直接走兜底。
const PROBE_FAILURE_TTL: Duration = Duration::from_secs(60);

/// 单次探测超时(请求路径上,必须小)。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// [`PROBE_TIMEOUT`] 的秒数,供启动时构造探测客户端(connect/read 超时)
/// 使用;请求级超时仍以 [`PROBE_TIMEOUT`] 为准。
pub const PROBE_TIMEOUT_SECS: u64 = 3;

/// 一次探测得到的地区身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub locale: String,
    pub timezone: String,
}

/// 缓存条目:成功缓存地区,失败负缓存时间戳。
#[derive(Debug, Clone)]
enum CacheEntry {
    Hit { region: Region, at: Instant },
    Miss { at: Instant },
}

/// 运行期出口地区解析器。`AppState` 持有,dispatcher 选号后调用
/// [`Self::resolve_for`]。
pub struct EgressResolver {
    /// direct 探测客户端(代理账号传该账号自己的代理客户端)。
    direct_probe_client: reqwest::Client,
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// 探测单飞:缓存过期瞬间可能有大量在途请求同时 miss,全局互斥让
    /// 它们排队共享一次探测结果(探测有 3s 上限,低频发生,全局串行
    /// 足够;同时避免撞 ip-api 免费档 ~45 req/min 的限流)。
    probe_flight: tokio::sync::Mutex<()>,
    /// 本机身份(direct 探测失败的兜底)。
    machine: Region,
}

impl EgressResolver {
    /// 构造解析器。`machine` 是 direct 探测失败的兜底身份(启动时读本机
    /// 区域)。
    pub fn new(direct_probe_client: reqwest::Client, machine: Region) -> Self {
        Self {
            direct_probe_client,
            cache: Mutex::new(HashMap::new()),
            probe_flight: tokio::sync::Mutex::new(()),
            machine,
        }
    }

    /// 本机地理身份(请求体构建时的占位;探测失败时的兜底)。
    pub fn machine_identity(&self) -> Region {
        self.machine.clone()
    }

    /// 解析**当前账号出口**的地区身份。`cache_key` 是账号代理 id(或
    /// `"direct"`),各出口缓存互不污染。`proxy_client` 为代理账号的代理
    /// 客户端;探测最多 [`PROBE_TIMEOUT`]。
    pub async fn resolve_for(
        &self,
        cache_key: &str,
        proxy_client: Option<&reqwest::Client>,
    ) -> Region {
        // 缓存命中(含负缓存)→ 免查。
        {
            let cache = self.lock_cache();
            match cache.get(cache_key) {
                Some(CacheEntry::Hit { region, at }) if at.elapsed() < EGRESS_CACHE_TTL => {
                    return region.clone();
                }
                Some(CacheEntry::Miss { at }) if at.elapsed() < PROBE_FAILURE_TTL => {
                    return self.fallback();
                }
                _ => {}
            }
        }

        // 单飞探测:排队进入(前一人在探时等它),进去后先复查缓存——
        // 排队期间可能已被前一人写入。
        let _flight = self.probe_flight.lock().await;
        {
            let cache = self.lock_cache();
            match cache.get(cache_key) {
                Some(CacheEntry::Hit { region, at }) if at.elapsed() < EGRESS_CACHE_TTL => {
                    return region.clone();
                }
                Some(CacheEntry::Miss { at }) if at.elapsed() < PROBE_FAILURE_TTL => {
                    return self.fallback();
                }
                _ => {}
            }
        }

        // 实测出口。
        let started = Instant::now();
        match self.probe(proxy_client).await {
            Some(region) => {
                crate::trace_info!(
                    "geo",
                    "egress probed channel={} locale={} tz={} elapsed={}ms",
                    cache_key,
                    region.locale,
                    region.timezone,
                    started.elapsed().as_millis(),
                );
                self.lock_cache().insert(
                    cache_key.to_string(),
                    CacheEntry::Hit {
                        region: region.clone(),
                        at: Instant::now(),
                    },
                );
                region
            }
            None => {
                self.lock_cache().insert(
                    cache_key.to_string(),
                    CacheEntry::Miss { at: Instant::now() },
                );
                crate::trace_warn!(
                    "geo",
                    "egress probe failed channel={} elapsed={}ms; using machine fallback",
                    cache_key,
                    started.elapsed().as_millis(),
                );
                self.fallback()
            }
        }
    }

    /// 兜底身份:本机身份。
    fn fallback(&self) -> Region {
        self.machine.clone()
    }

    /// 锁中毒视为可恢复:guard 区不可能 panic,直接取回内部数据继续,
    /// 不把推理请求变成 panic。
    fn lock_cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 经探测客户端查询 ip-api;失败返回 None。免费档仅提供 HTTP,不做
    /// 敏感数据传输;返回值只影响声明的 locale/timezone。
    async fn probe(&self, proxy_client: Option<&reqwest::Client>) -> Option<Region> {
        #[derive(serde::Deserialize)]
        struct Reply {
            #[serde(rename = "countryCode")]
            country_code: Option<String>,
            #[serde(rename = "timezone")]
            timezone: Option<String>,
        }
        let client = proxy_client.unwrap_or(&self.direct_probe_client);
        let reply: Reply = client
            .get("http://ip-api.com/json?fields=countryCode,timezone")
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let timezone = reply.timezone.filter(|t| !t.trim().is_empty());
        let country = reply.country_code.filter(|c| c.len() == 2);
        match (country.as_deref(), timezone) {
            (Some(code), Some(tz)) => Some(Region {
                locale: country_identity(code).locale,
                timezone: tz,
            }),
            (Some(code), None) => Some(country_identity(code)),
            _ => None,
        }
    }
}

/// 常见国家/地区 → (BCP47 locale, IANA timezone)。覆盖主流住宅代理
/// 出口国;未收录国家回落 `en-US` / `UTC`。时区取该国主体时区。
pub fn country_identity(code: &str) -> Region {
    let (locale, timezone) = match code {
        "US" => ("en-US", "America/New_York"),
        "GB" => ("en-GB", "Europe/London"),
        "CA" => ("en-CA", "America/Toronto"),
        "AU" => ("en-AU", "Australia/Sydney"),
        "NZ" => ("en-NZ", "Pacific/Auckland"),
        "IE" => ("en-IE", "Europe/Dublin"),
        "DE" => ("de-DE", "Europe/Berlin"),
        "FR" => ("fr-FR", "Europe/Paris"),
        "ES" => ("es-ES", "Europe/Madrid"),
        "IT" => ("it-IT", "Europe/Rome"),
        "NL" => ("nl-NL", "Europe/Amsterdam"),
        "BE" => ("nl-BE", "Europe/Brussels"),
        "AT" => ("de-AT", "Europe/Vienna"),
        "CH" => ("de-CH", "Europe/Zurich"),
        "SE" => ("sv-SE", "Europe/Stockholm"),
        "NO" => ("nb-NO", "Europe/Oslo"),
        "DK" => ("da-DK", "Europe/Copenhagen"),
        "FI" => ("fi-FI", "Europe/Helsinki"),
        "PL" => ("pl-PL", "Europe/Warsaw"),
        "PT" => ("pt-PT", "Europe/Lisbon"),
        "CZ" => ("cs-CZ", "Europe/Prague"),
        "RO" => ("ro-RO", "Europe/Bucharest"),
        "GR" => ("el-GR", "Europe/Athens"),
        "TR" => ("tr-TR", "Europe/Istanbul"),
        "UA" => ("uk-UA", "Europe/Kyiv"),
        "JP" => ("ja-JP", "Asia/Tokyo"),
        "KR" => ("ko-KR", "Asia/Seoul"),
        "CN" => ("zh-CN", "Asia/Shanghai"),
        "HK" => ("zh-HK", "Asia/Hong_Kong"),
        "TW" => ("zh-TW", "Asia/Taipei"),
        "SG" => ("en-SG", "Asia/Singapore"),
        "MY" => ("ms-MY", "Asia/Kuala_Lumpur"),
        "TH" => ("th-TH", "Asia/Bangkok"),
        "VN" => ("vi-VN", "Asia/Ho_Chi_Minh"),
        "PH" => ("en-PH", "Asia/Manila"),
        "ID" => ("id-ID", "Asia/Jakarta"),
        "IN" => ("en-IN", "Asia/Kolkata"),
        "AE" => ("ar-AE", "Asia/Dubai"),
        "SA" => ("ar-SA", "Asia/Riyadh"),
        "IL" => ("he-IL", "Asia/Jerusalem"),
        "BR" => ("pt-BR", "America/Sao_Paulo"),
        "MX" => ("es-MX", "America/Mexico_City"),
        "AR" => ("es-AR", "America/Argentina/Buenos_Aires"),
        "CL" => ("es-CL", "America/Santiago"),
        "CO" => ("es-CO", "America/Bogota"),
        "ZA" => ("en-ZA", "Africa/Johannesburg"),
        _ => ("en-US", "UTC"),
    };
    Region {
        locale: locale.to_string(),
        timezone: timezone.to_string(),
    }
}

/// 启动时读本机身份(direct 模式主身份 + 探测失败兜底)。
pub fn machine_region() -> Region {
    let locale = sys_locale::get_locale()
        .map(|raw| raw.split('.').next().unwrap_or(&raw).replace('_', "-"))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "zh-CN".to_string());
    let timezone = iana_time_zone::get_timezone()
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Asia/Shanghai".to_string());
    Region { locale, timezone }
}

/// 集成测试用的固定地区身份(双显式语义不再存在,但测试需要确定性的
/// locale/timezone 断言值;探测在测试环境不可用,直接给定)。
pub fn test_region() -> Region {
    Region {
        locale: "zh-CN".to_string(),
        timezone: "Asia/Shanghai".to_string(),
    }
}

/// 同上,英文/纽约变体(user-preferences 替换断言用不同值区分新旧)。
pub fn test_region_en() -> Region {
    Region {
        locale: "en-US".to_string(),
        timezone: "America/New_York".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_identity_covers_common_proxies() {
        assert_eq!(country_identity("US").locale, "en-US");
        assert_eq!(country_identity("US").timezone, "America/New_York");
        assert_eq!(country_identity("JP").locale, "ja-JP");
        assert_eq!(country_identity("CN").timezone, "Asia/Shanghai");
        // 未收录国家回落 en-US/UTC。
        assert_eq!(country_identity("ZZ").locale, "en-US");
        assert_eq!(country_identity("ZZ").timezone, "UTC");
    }

    #[tokio::test]
    async fn unreachable_probe_falls_back_without_blocking() {
        // 探测必败 → 负缓存 → 兜底;两次调用第二次免查。
        let resolver = EgressResolver::new(
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap(),
            Region {
                locale: "zh-CN".into(),
                timezone: "Asia/Shanghai".into(),
            },
        );
        let first = resolver.resolve_for("proxy0", None).await;
        assert_eq!(first.locale, "zh-CN");
        let second = resolver.resolve_for("proxy0", None).await;
        assert_eq!(second.timezone, "Asia/Shanghai");
    }

    #[test]
    fn machine_region_is_well_formed() {
        let region = machine_region();
        assert!(region.locale.contains('-'));
        assert!(region.timezone.contains('/'));
    }
}
