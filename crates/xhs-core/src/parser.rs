//! 输入解析：把用户粘贴的小红书分享文本/链接归一成来源目标。
//!
//! 小红书的分享形态：App「分享-复制链接」给出的是一段文本
//! （`78 FYx2:/ 复制打开小红书，看看【xx】的作品！... https://www.xiaohongshu.com/explore/{id}?xsec_token=...`），
//! 用户主页是 `https://www.xiaohongshu.com/user/profile/{uid}`，
//! 短链是 `http://xhslink.com/{code}`（需要网络解析）。

use crate::error::{Result, XhsError};

/// 一条输入归一后的来源目标。
#[derive(Debug, Clone, PartialEq)]
pub enum SourceTarget {
    /// 推荐流首页（https://www.xiaohongshu.com/explore，没有具体笔记 id）：
    /// 解析页面里那一屏笔记卡片，勾选后即可下载
    Feed,
    /// 搜索结果页（https://www.xiaohongshu.com/search_result?keyword=…）：
    /// 解析搜索结果里那一屏笔记卡片
    Search { url: String },
    /// 单条笔记（id = 24 位十六进制；token = xsec_token，详情页多数情况必需）
    Note { id: String, token: Option<String>, url: String },
    /// 用户主页（uid = 24 位十六进制）
    ///
    /// token / url：从搜索结果复制来的主页链接带 `xsec_token`，**必须带上**，
    /// 否则小红书会拒绝该主页（实测：丢了 token 就解析失败）。
    User {
        uid: String,
        token: Option<String>,
        url: String,
    },
}

/// 解析一行输入。支持：
/// - `https://www.xiaohongshu.com/explore/{id}?xsec_token=…`（含前后混排的分享文本）
/// - `https://www.xiaohongshu.com/discovery/item/{id}?…`
/// - `https://www.xiaohongshu.com/user/profile/{uid}`
/// - `http://xhslink.com/{code}` 短链（返回 NEED_RESOLVE 哨兵，上层先 302 解析）
/// - 裸 24 位十六进制笔记 ID（无 token，登录态下多数可开）
pub fn parse_input(input: &str) -> Result<SourceTarget> {
    let text = input.trim();
    if text.is_empty() {
        return Err(XhsError::InvalidInput("链接是空的".into()));
    }

    // 从整段文本里抽 URL（分享文本里链接常混在文案中间）
    let url_re = regex::Regex::new(r#"https?://[^\s"'<>，,；;）)】\]]+"#).unwrap();
    let candidate = url_re
        .find(text)
        .map(|m| m.as_str())
        .unwrap_or(text.trim());

    // xhslink 短链：目标无法离线判断，交由上层 resolve_redirect 后重试
    if candidate.contains("xhslink.com/") {
        return Err(XhsError::InvalidInput(format!("NEED_RESOLVE:{candidate}")));
    }

    let lowered = candidate.to_lowercase();
    let is_xhs = lowered.contains("xiaohongshu.com/") || lowered.contains("xhslink.com");
    let path_part = candidate
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(candidate);
    let (path, query) = path_part.split_once('?').unwrap_or((path_part, ""));
    let segments: Vec<&str> = path
        .split('#').next().unwrap_or(path)
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // 从查询串里抽 xsec_token（分享链接有时用 share_token 同值出现）
    let token = query_token(query, &["xsec_token", "share_token"]);

    // 用户主页 /user/profile/{uid}（"me" 也走这条，交给上层处理登录态）
    if let Some(i) = segments.iter().position(|s| *s == "profile") {
        if let Some(uid) = segments.get(i + 1) {
            if is_xhs && looks_like_id(uid) {
                return Ok(SourceTarget::User {
                    uid: uid.to_string(),
                    token: token.clone(),
                    url: candidate.to_string(),
                });
            }
        }
    }
    // 搜索结果页：/search_result?keyword=…（保留完整地址，重新按同一关键词搜）
    if segments.iter().any(|s| *s == "search_result" || *s == "search") {
        if is_xhs {
            return Ok(SourceTarget::Search {
                url: candidate.to_string(),
            });
        }
    }

    // /explore/{id}
    if let Some(i) = segments.iter().position(|s| *s == "explore") {
        match segments.get(i + 1) {
            Some(id) if looks_like_id(id) => {
                return Ok(SourceTarget::Note {
                    id: id.to_string(),
                    token,
                    url: candidate.to_string(),
                });
            }
            // /explore 本身（信息流首页）：解析这一屏笔记
            None if is_xhs => return Ok(SourceTarget::Feed),
            _ => {}
        }
    }
    // /discovery/item/{id}（老版分享形态）
    if let Some(i) = segments.iter().position(|s| *s == "item") {
        if let Some(id) = segments.get(i + 1) {
            if looks_like_id(id) {
                return Ok(SourceTarget::Note {
                    id: id.to_string(),
                    token,
                    url: candidate.to_string(),
                });
            }
        }
    }

    // 裸 24 位十六进制笔记 ID
    let bare = text.trim();
    if looks_like_id(bare) {
        return Ok(SourceTarget::Note {
            id: bare.to_string(),
            token: None,
            url: format!("https://www.xiaohongshu.com/explore/{bare}"),
        });
    }

    Err(XhsError::InvalidInput(format!(
        "认不出这个链接：{}（支持笔记分享链接、用户主页或 xhslink 短链）",
        crate::error::truncate(text, 60)
    )))
}

/// 从查询串里按多个候选键取第一个非空值。
fn query_token(query: &str, keys: &[&str]) -> Option<String> {
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let key = key.trim();
            if keys.contains(&key) && !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// 小红书的笔记 ID / 用户 ID 都是 24 位十六进制。
fn looks_like_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_share_text_with_token() {
        let input = r#"78 FYx2:/ 复制打开小红书，看看【示例】的作品！ https://www.xiaohongshu.com/explore/66b7d3a7000000001e01c8b3?xsec_token=ABX_mbwtysIdP_je5fcmHAEFOSHivdM-hsTp59bVTbINI=&xsec_source=pc_feed 复制本条信息"#;
        match parse_input(input).unwrap() {
            SourceTarget::Note { id, token, url } => {
                assert_eq!(id, "66b7d3a7000000001e01c8b3");
                assert_eq!(
                    token.as_deref(),
                    Some("ABX_mbwtysIdP_je5fcmHAEFOSHivdM-hsTp59bVTbINI=")
                );
                assert!(url.contains("explore/66b7d3a7"));
            }
            other => panic!("应为 Note，实际 {other:?}"),
        }
    }

    #[test]
    fn parses_discovery_item_and_profile() {
        match parse_input("https://www.xiaohongshu.com/discovery/item/66b7d3a7000000001e01c8b3?xsec_token=ABC=").unwrap() {
            SourceTarget::Note { id, token, .. } => {
                assert_eq!(id, "66b7d3a7000000001e01c8b3");
                assert_eq!(token.as_deref(), Some("ABC="));
            }
            other => panic!("应为 Note，实际 {other:?}"),
        }
        match parse_input("https://www.xiaohongshu.com/user/profile/58c8014a6a6a690dbb0c84c1").unwrap() {
            SourceTarget::User { uid, token, .. } => {
                assert_eq!(uid, "58c8014a6a6a690dbb0c84c1");
                assert!(token.is_none());
            }
            other => panic!("应为 User，实际 {other:?}"),
        }
        // 带 token 的主页链接：token 必须保留（搜索来源的主页没它会被拒）
        match parse_input(
            "https://www.xiaohongshu.com/user/profile/66f3c481000000001c002a76?xsec_token=ABQkKpAAA2GD&xsec_source=pc_search",
        )
        .unwrap()
        {
            SourceTarget::User { uid, token, url } => {
                assert_eq!(uid, "66f3c481000000001c002a76");
                assert_eq!(token.as_deref(), Some("ABQkKpAAA2GD"));
                assert!(url.contains("xsec_token="));
            }
            other => panic!("应为 User，实际 {other:?}"),
        }
    }

    #[test]
    fn search_result_is_search() {
        match parse_input(
            "https://www.xiaohongshu.com/search_result?keyword=%25E6%2597%25A0&source=web_explore_feed",
        )
        .unwrap()
        {
            SourceTarget::Search { url } => assert!(url.contains("keyword=")),
            other => panic!("应为 Search，实际 {other:?}"),
        }
    }

    #[test]
    fn explore_home_is_feed() {
        assert_eq!(
            parse_input("https://www.xiaohongshu.com/explore").unwrap(),
            SourceTarget::Feed
        );
        assert_eq!(
            parse_input("https://www.xiaohongshu.com/explore/").unwrap(),
            SourceTarget::Feed
        );
    }

    #[test]
    fn shortlink_needs_resolve() {
        let err = parse_input("http://xhslink.com/a1B2c3").unwrap_err();
        assert!(err.to_string().contains("NEED_RESOLVE:"), "{err}");
    }

    #[test]
    fn bare_note_id() {
        match parse_input("66b7d3a7000000001e01c8b3").unwrap() {
            SourceTarget::Note { id, token, .. } => {
                assert_eq!(id, "66b7d3a7000000001e01c8b3");
                assert!(token.is_none());
            }
            other => panic!("应为 Note，实际 {other:?}"),
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_input("hello world").is_err());
        assert!(parse_input("").is_err());
    }
}
