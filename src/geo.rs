//! 出口地区解析 —— `raycast_locale` / `raycast_timezone` 的自动来源。
//!
//! 真客户端的 locale/timezone 来自运行机器的系统区域与本地时区;网关扮演
//! 的是"某台机器",这两个值必须与出口 IP 的地理一致,否则 locale/
//! timezone/IP 三者交叉就是异常信号。
//!
//! **出口可能是 mihomo/clash 这类混合端口**:同一个代理 URL,每次连接的
//! 出口节点都可能不同——所以不能在启动时探测一次了事。解析是**每次构建
//! 请求体前**做的:
//!
//! 1. config 显式给出 locale+timezone → 原样使用(零探测)。
//! 2. 否则经当前通道(直连或链首代理客户端)实测出口
//!    (`http://ip-api.com/json`,免费无 key,响应自带 IANA timezone),
//!    结果按**出口 IP** 缓存 [`EGRESS_CACHE_TTL`]——mihomo 切节点后出口
//!    IP 变化,缓存自然失效;同节点复用期内零额外开销。
//! 3. 探测失败/超时 → 回落显式值(若给了单个) → 本机身份 → 缺省,绝不
//!    阻塞推理请求;失败结果短暂负缓存,避免每请求都撞超时。

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

/// 一次探测得到(或配置给出)的地区身份。
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

/// 运行期出口地区解析器。`AppState` 持有,请求路径调用 [`Self::resolve`]。
pub struct EgressResolver {
    /// 显式配置;locale 与 timezone **都**给出时完全跳过探测。
    explicit_locale: Option<String>,
    explicit_timezone: Option<String>,
    /// 探测客户端:direct 模式直连;proxy 模式挂链首代理(mihomo 的
    /// 混合端口 URL 即可,出口随当前节点)。
    probe_client: reqwest::Client,
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// 本机身份(direct 探测失败的兜底)。
    machine: Region,
}

impl EgressResolver {
    /// 构造解析器。`machine` 是 direct 模式的兜底身份(启动时读本机区域)。
    pub fn new(
        explicit_locale: Option<String>,
        explicit_timezone: Option<String>,
        probe_client: reqwest::Client,
        machine: Region,
    ) -> Self {
        Self {
            explicit_locale,
            explicit_timezone,
            probe_client,
            cache: Mutex::new(HashMap::new()),
            machine,
        }
    }

    /// 解析当前出口的地区身份。`cache_key` 区分通道(direct / 每个代理
    /// URL 各一条缓存,互不污染)。探测最多 [`PROBE_TIMEOUT`]。
    pub async fn resolve(&self, cache_key: &str) -> Region {
        // 双显式:身份固定,零 IO。
        if let (Some(locale), Some(timezone)) = (&self.explicit_locale, &self.explicit_timezone) {
            return Region {
                locale: locale.clone(),
                timezone: timezone.clone(),
            };
        }

        // 缓存命中(含负缓存)→ 免查。
        {
            let cache = self.cache.lock().expect("geo cache poisoned");
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
        match self.probe().await {
            Some(region) => {
                let merged = Region {
                    locale: self.explicit_locale.clone().unwrap_or(region.locale),
                    timezone: self.explicit_timezone.clone().unwrap_or(region.timezone),
                };
                self.cache.lock().expect("geo cache poisoned").insert(
                    cache_key.to_string(),
                    CacheEntry::Hit {
                        region: merged.clone(),
                        at: Instant::now(),
                    },
                );
                merged
            }
            None => {
                self.cache.lock().expect("geo cache poisoned").insert(
                    cache_key.to_string(),
                    CacheEntry::Miss { at: Instant::now() },
                );
                tracing::warn!("egress geo probe failed; using fallback identity");
                self.fallback()
            }
        }
    }

    /// 兜底身份:显式单值 → 本机身份 → 缺省。
    fn fallback(&self) -> Region {
        Region {
            locale: self
                .explicit_locale
                .clone()
                .unwrap_or_else(|| self.machine.locale.clone()),
            timezone: self
                .explicit_timezone
                .clone()
                .unwrap_or_else(|| self.machine.timezone.clone()),
        }
    }

    /// 经探测客户端查询 ip-api;失败返回 None。
    async fn probe(&self) -> Option<Region> {
        #[derive(serde::Deserialize)]
        struct Reply {
            #[serde(rename = "countryCode")]
            country_code: Option<String>,
            #[serde(rename = "timezone")]
            timezone: Option<String>,
        }
        let reply: Reply = self
            .probe_client
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
    async fn explicit_pair_skips_probing() {
        // 双显式:即使 probe client 指向必败地址,也零 IO 直接返回。
        let resolver = EgressResolver::new(
            Some("en-US".into()),
            Some("America/New_York".into()),
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap(),
            machine_region(),
        );
        let region = resolver.resolve("direct").await;
        assert_eq!(region.locale, "en-US");
        assert_eq!(region.timezone, "America/New_York");
    }

    #[tokio::test]
    async fn unreachable_probe_falls_back_without_blocking() {
        // 探测必败 → 负缓存 → 兜底;两次调用第二次免查。
        let resolver = EgressResolver::new(
            None,
            None,
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
        let first = resolver.resolve("proxy0").await;
        assert_eq!(first.locale, "zh-CN");
        let second = resolver.resolve("proxy0").await;
        assert_eq!(second.timezone, "Asia/Shanghai");
    }

    #[tokio::test]
    async fn explicit_half_config_merges_with_probe_or_fallback() {
        // 只给了 timezone:探测失败时 locale 走本机身份,timezone 固定显式值。
        let resolver = EgressResolver::new(
            None,
            Some("Europe/Berlin".into()),
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap(),
            machine_region(),
        );
        let region = resolver.resolve("proxy1").await;
        assert_eq!(region.timezone, "Europe/Berlin");
        assert!(!region.locale.is_empty());
    }

    #[test]
    fn machine_region_is_well_formed() {
        let region = machine_region();
        assert!(region.locale.contains('-'));
        assert!(region.timezone.contains('/'));
    }
}
