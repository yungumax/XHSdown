//! HTTP 会话层：桌面 UA、访客 Cookie（a1/webId/xsecappid）、按域组发 Cookie。
//!
//! 小红书网页是桌面优先：UA 用桌面 Chrome。未登录时访问首页/探索页仍可拿到
//! acw_tc 等风控 Cookie，但笔记详情会被 302 到登录页——登录态由网页登录
//! （内嵌官方登录页收割 Cookie）提供，本层只负责存储与按域组发。

use crate::error::{Result, XhsError};
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_LANGUAGE, REFERER, USER_AGENT};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// 持久化的会话 Cookie（全部在 .xiaohongshu.com 域上，单桶即可；
/// 结构保留分桶形态（便于日后加域时按域分桶）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainCookies {
    pub web: Vec<(String, String)>,
}

impl DomainCookies {
    pub fn is_empty(&self) -> bool {
        self.web.is_empty()
    }
}

/// 桌面 Chrome UA：小红书网页按桌面端渲染，__INITIAL_STATE__ 数据最全。
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const SITE_REFERER: &str = "https://www.xiaohongshu.com/";

/// Cookie 要装到哪些主机（登录凭据在 .xiaohongshu.com 上，两个主机都要能读到）。
pub const COOKIE_ORIGINS: &[&str] = &["https://www.xiaohongshu.com/", "https://xiaohongshu.com/"];

pub struct XhsClient {
    pub http: Client,
    jar: Arc<Jar>,
}

impl XhsClient {
    pub fn new() -> Result<Self> {
        Self::build(None)
    }

    /// 走指定代理（形如 `http://127.0.0.1:7890`）；传空则直连。
    /// 会话客户端：Cookie 罐只当存储，请求按 export_cookies 显式组头发送。
    pub fn with_proxy(proxy: Option<&str>) -> Result<Self> {
        Self::build(proxy.filter(|value| !value.trim().is_empty()))
    }

    fn build(proxy: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(UA));
        headers.insert(REFERER, HeaderValue::from_static(SITE_REFERER));
        // 桌面浏览器的 Accept 头：少了它探索页会直接 302 到登录页（实测踩过）
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
            ),
        );
        headers.insert(
            ACCEPT_LANGUAGE,
            HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
        );

        let jar = Arc::new(Jar::default());
        let builder = Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180));

        let mut builder = builder;
        if let Some(addr) = proxy {
            let proxy = reqwest::Proxy::all(addr.trim())
                .map_err(|e| XhsError::InvalidInput(format!("代理地址无效: {e}")))?;
            builder = builder.proxy(proxy);
        }

        Ok(Self {
            http: builder.build()?,
            jar,
        })
    }

    /// 向会话中写入一条 Cookie，后续请求自动携带。
    pub fn add_cookie(&self, cookie: &str, url: &Url) {
        self.jar.add_cookie_str(cookie, url);
    }

    /// 导出某个域的 Cookie（k=v 形式）。
    pub fn export_cookies_for(&self, origin: &str) -> Vec<(String, String)> {
        let Ok(url) = Url::parse(origin) else { return Vec::new() };
        let Some(header) = self.jar.cookies(&url) else { return Vec::new() };
        let Ok(text) = header.to_str() else { return Vec::new() };
        let mut out = Vec::new();
        for pair in text.split(';') {
            if let Some((name, value)) = pair.trim().split_once('=') {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    out.push((name, value.trim().to_string()));
                }
            }
        }
        out
    }

    /// 导出全部 Cookie（单桶：web）。
    pub fn export_cookies(&self) -> Vec<(String, String)> {
        let mut out = self.export_cookies_for("https://www.xiaohongshu.com/");
        for (name, value) in self.export_cookies_for("https://xiaohongshu.com/") {
            if !out.iter().any(|(n, _): &(String, String)| n == &name) {
                out.push((name, value));
            }
        }
        out
    }

    /// 按域恢复 Cookie（两个主机都装）。
    pub fn install_cookies(&self, cookies: &[(String, String)]) {
        for origin in COOKIE_ORIGINS {
            self.install_cookies_for(origin, cookies);
        }
    }

    pub fn install_cookies_for(&self, origin: &str, cookies: &[(String, String)]) {
        let Ok(url) = Url::parse(origin) else { return };
        for (name, value) in cookies {
            self.jar
                .add_cookie_str(&format!("{name}={value}"), &url);
        }
    }

    /// 按请求 URL 的域取 Cookie 头。
    fn cookie_header_for(&self, url: &Url) -> Option<String> {
        let header = self.jar.cookies(url)?;
        let text = header.to_str().ok()?;
        (!text.is_empty()).then(|| text.to_string())
    }

    /// 是否已具备登录凭据（web_session 由官方登录页签发）。
    pub fn has_session(&self) -> bool {
        self.export_cookies()
            .iter()
            .any(|(name, value)| name == "web_session" && !value.is_empty())
    }

    /// 会话预热：访问首页与探索页，把响应里的 Set-Cookie（acw_tc / abRequestId 等
    /// WAF 与指纹 Cookie）收进会话；再补齐浏览器会带的那组指纹 Cookie——
    /// 少了它们探索页都可能被 302 到登录页（实测踩过）。
    ///
    /// 注意：本客户端的 Cookie 罐只当存储用（请求靠显式组头发送），
    /// 所以这里要手工收割 Set-Cookie，而不是靠 cookie_provider 自动收。
    pub async fn warmup(&self) -> Result<()> {
        for url in [SITE_REFERER, "https://www.xiaohongshu.com/explore"] {
            let Ok(resp) = self.http.get(url).send().await else { continue };
            let origin: Url = SITE_REFERER.parse().unwrap();
            for value in resp.headers().get_all(reqwest::header::SET_COOKIE) {
                if let Ok(text) = value.to_str() {
                    // Set-Cookie: name=value; Path=/; ...（只取第一段）
                    if let Some(pair) = text.split(';').next() {
                        self.jar.add_cookie_str(pair.trim(), &origin);
                    }
                }
            }
            let _ = resp.bytes().await;
        }

        let cookies = self.export_cookies();
        let value_of = |name: &str| {
            cookies
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        let mut patch: Vec<(String, String)> = Vec::new();
        let a1 = value_of("a1").unwrap_or_else(guest_a1);
        if value_of("a1").is_none() {
            patch.push(("a1".into(), a1.clone()));
        }
        if value_of("webId").is_none() {
            patch.push(("webId".into(), format!("{a1}-xhsdown-webid")));
        }
        if value_of("xsecappid").is_none() {
            patch.push(("xsecappid".into(), "xhs-pc".into()));
        }
        if value_of("webBuild").is_none() {
            patch.push(("webBuild".into(), "4.63.2".into()));
        }
        if !patch.is_empty() {
            for origin in COOKIE_ORIGINS {
                for (name, value) in &patch {
                    self.add_cookie(&format!("{name}={value}"), &origin.parse().unwrap());
                }
            }
        }
        Ok(())
    }

    /// 跟随重定向拿到最终地址（用于 xhslink.com 短链）。
    pub async fn resolve_redirect(&self, url: &str) -> Result<String> {
        let resp = self.http.get(url).send().await?;
        Ok(resp.url().to_string())
    }

    /// 取一页文本（HTML 解析主通道；Cookie 按域显式组发）。
    pub async fn fetch_text(&self, url: &str, referer: &str) -> Result<String> {
        let mut req = self.http.get(url).header(REFERER, referer);
        if let Ok(parsed) = Url::parse(url) {
            if let Some(cookies) = self.cookie_header_for(&parsed) {
                req = req.header(reqwest::header::COOKIE, cookies);
            }
        }
        let resp = req.send().await?;
        let status = resp.status();
        // 小红书用 302 到 /login 或 /404?error_code=300031 表达"不能看"
        let final_url = resp.url().to_string();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(XhsError::Unavailable(format!(
                "HTTP {status}: {}",
                crate::error::truncate(&text, 200)
            )));
        }
        if final_url.contains("/login?") || final_url.contains("/404?source=") {
            return Err(XhsError::NeedLogin(
                "小红书要求登录后浏览该内容（也可能已被删除或仅自己可见）——请先登录再重试".into(),
            ));
        }
        if text.contains("/login?redirectPath=") && text.len() < 500 {
            return Err(XhsError::NeedLogin(
                "此内容需要登录后查看（小红书未开放匿名访问）".into(),
            ));
        }
        Ok(text)
    }

    /// 解析页面里的 `window.__INITIAL_STATE__`（HTML 内嵌 JSON）。
    /// 里面的裸 `undefined` 值要先换成 null 才是合法 JSON。
    pub fn parse_initial_state(html: &str) -> Result<Value> {
        const MARKER: &str = "window.__INITIAL_STATE__";
        let start = html
            .find(MARKER)
            .ok_or_else(|| XhsError::Decode("页面里没有 __INITIAL_STATE__（可能被风控或改版）".into()))?;
        let json_start = html[start..]
            .find('{')
            .map(|i| start + i)
            .ok_or_else(|| XhsError::Decode("__INITIAL_STATE__ 后没有 JSON 主体".into()))?;
        let end = html[json_start..]
            .find("</script>")
            .map(|i| json_start + i)
            .unwrap_or(html.len());
        let raw = html[json_start..end].trim_end().trim_end_matches(';').trim_end();

        // 裸 JS 字面量 → null（只处理作为 JSON 值出现的位置，不碰字符串内容）：
        // undefined / NaN / Infinity / -Infinity 都不是合法 JSON。
        // 注意替换串要写成 ${1}null：正则引擎会把 $1null 当成组名 "1null"（实测踩过）
        let re = regex::Regex::new(r"([:,\[]\s*)undefined").unwrap();
        let fixed = re.replace_all(raw, "${1}null");
        // 用  收边界（regex crate 不支持前瞻断言）
        let re_ident = regex::Regex::new(r"([:,\[]\s*)(?:NaN|-?Infinity)").unwrap();
        let fixed = re_ident.replace_all(&fixed, "${1}null");
        // 状态里直接内嵌了 JS 构造器：`new Set([])` / `new Map(...)`（实测踩过）
        let re_set = regex::Regex::new(r"new Set\(\s*(\[[^()]*\])?\s*\)").unwrap();
        let fixed = re_set.replace_all(&fixed, "${1}");
        let re_map = regex::Regex::new(r"new Map\(\s*(\{[^()]*\})?\s*\)").unwrap();
        let fixed = re_map.replace_all(&fixed, "${1}");
        // 先按上面的清洗结果解析；仍失败就用通用兜底（剩余的 new Xxx(...) 一律当 null），
        // 并记一条日志——站点改版引入新的内嵌字面量时，这样最不容易整个页面读不出来
        let parsed = match serde_json::from_str(&fixed) {
            Ok(value) => Ok(value),
            Err(first_err) => {
                let re_ctor =
                    regex::Regex::new(r"new\s+[A-Za-z_$][0-9A-Za-z_$]*\s*\([^()]*\)").unwrap();
                let relaxed = re_ctor.replace_all(&fixed, "null");
                match serde_json::from_str(&relaxed) {
                    Ok(value) => {
                        crate::login::debug_log(
                            "状态里有未识别的 JS 字面量，已用通用兜底转换后解析成功",
                        );
                        Ok(value)
                    }
                    Err(_) => Err(first_err),
                }
            }
        };
        parsed.map_err(|e| {
            // 把出错位置附近的内容截出来：站点改版引入新的非法字面量时能一眼看到
            let msg = e.to_string();
            let near = regex::Regex::new(r"column (\d+)")
                .unwrap()
                .captures(&msg)
                .and_then(|c| c.get(1))
                .and_then(|c| c.as_str().parse::<usize>().ok())
                .map(|col| {
                    let lo = col.saturating_sub(80);
                    let hi = (col + 80).min(fixed.len());
                    crate::error::truncate(&fixed[lo..hi], 200)
                })
                .unwrap_or_else(|| crate::error::truncate(&fixed, 200));
            XhsError::Decode(format!("__INITIAL_STATE__ 解析失败：{e}；出错附近：{near}"))
        })
    }
}

/// 生成访客 a1 Cookie（"18" + 16 位随机小写字母数字，形态与真机一致）。
fn guest_a1() -> String {
    let chars = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::from("18");
    // 简易熵源：时间 + 地址哈希（无需引入 rand 依赖）
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x2545F4914F6CDD1D)
        ^ (&out as *const _ as u64);
    for _ in 0..16 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(chars[(state % 36) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_initial_state_with_undefined() {
        let html = r#"<html><script>window.__INITIAL_STATE__={"a":1,"b":undefined,"c":[1,undefined],"d":"text undefined kept"};</script></html>"#;
        let value = XhsClient::parse_initial_state(html).unwrap();
        assert_eq!(value["a"], 1);
        assert!(value["b"].is_null());
        assert!(value["c"][1].is_null());
        assert_eq!(value["d"], "text undefined kept");
    }

    #[test]
    fn parses_initial_state_with_js_constructors() {
        // 小红书的状态里会内嵌 new Set([]) / new Map(...)（实测：selectedIds 字段）
        let html = r#"<html><script>window.__INITIAL_STATE__={"ids":new Set([]),"m":new Map({}),"ok":1};</script></html>"#;
        let value = XhsClient::parse_initial_state(html).unwrap();
        assert!(value["ids"].is_array());
        assert!(value["m"].is_object());
        assert_eq!(value["ok"], 1);
    }

    #[test]
    fn guest_a1_shape() {
        let a1 = guest_a1();
        assert_eq!(a1.len(), 18);
        assert!(a1.starts_with("18"));
    }
}
