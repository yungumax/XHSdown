//! 网页数据抽取：笔记详情、用户主页、登录态。
//!
//! 全部从页面 HTML 的 `__INITIAL_STATE__` 里按需取值（Value 抽取而不是
//! 完整反序列化）：字段冗多且各端不一致，按需取值对改版最稳
//! （缺字段就是没有，不会整条解析失败）。

use crate::client::XhsClient;
use crate::error::{Result, XhsError};
use serde_json::Value;

/// 一条笔记解析出的干净结构（图片/视频/文案全齐）。
#[derive(Debug, Clone, Default)]
pub struct NoteData {
    pub note_id: String,
    pub xsec_token: String,
    /// 展示标题（title 优先，空则取 desc 前 30 字）
    pub title: String,
    /// 正文全文（下载文案用）
    pub desc: String,
    pub created_at: i64,
    pub user_id: String,
    pub nickname: String,
    /// 图片直链（原图规格）
    pub pics: Vec<String>,
    /// 视频档位 (label, url)，清晰度从高到低排
    pub videos: Vec<(String, String)>,
    pub liked: String,
    pub duration: i64,
}

impl NoteData {
    pub fn has_video(&self) -> bool {
        !self.videos.is_empty()
    }
}

/// 博主信息。
#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    pub uid: String,
    pub nickname: String,
    pub avatar: String,
    pub red_id: String,
    pub description: String,
    /// 笔记总数（接口不给就是 0）
    pub notes_count: u32,
    pub fans: String,
}

impl XhsClient {
    /// 单条笔记详情。`url` 是带 xsec_token 的完整笔记页地址
    /// （分享链接自带；用户主页来的笔记由本库拼好带回）。
    pub async fn note_detail(&self, url: &str) -> Result<NoteData> {
        let html = self
            .fetch_text(url, crate::client::SITE_REFERER)
            .await?;
        let state = XhsClient::parse_initial_state(&html)?;
        let detail = note_from_state(&state).ok_or_else(|| {
            XhsError::Api("笔记可能已被删除、仅自己可见或页面改版（未取到笔记数据）".into())
        })?;
        Ok(detail)
    }

    /// 用户主页（含第一页笔记卡片）。小红书网页端只内嵌最新一批笔记，
    /// 更早的要走带签名的翻页接口，桌面端拿不到——exhausted 由上层据此判定。
    pub async fn user_profile(&self, uid: &str) -> Result<(UserInfo, Vec<NoteData>)> {
        let url = format!("https://www.xiaohongshu.com/user/profile/{uid}");
        let html = self
            .fetch_text(&url, crate::client::SITE_REFERER)
            .await?;
        let state = XhsClient::parse_initial_state(&html)?;
        // userPageData 未登录时实测是空数组；只有拿到对象才算真数据
        let page = state
            .pointer("/user/userPageData")
            .filter(|v| v.is_object())
            .ok_or_else(|| {
                let logged_in = state
                    .pointer("/user/loggedIn")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if logged_in {
                    XhsError::Api("主页数据缺失（可能已注销或被风控）".into())
                } else {
                    XhsError::NeedLogin(
                        "小红书要求登录后浏览用户主页——请先登录再重试".into(),
                    )
                }
            })?;
        let info = UserInfo {
            uid: str_at(page, &["id"]).unwrap_or_else(|| uid.to_string()),
            nickname: str_at(page, &["nickname"]).unwrap_or_default(),
            avatar: str_at(page, &["headImage"]).unwrap_or_default(),
            red_id: str_at(page, &["redId"]).unwrap_or_default(),
            description: str_at(page, &["desc"]).unwrap_or_default(),
            notes_count: page.get("notes").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            fans: str_at(page, &["fans"]).unwrap_or_default(),
        };
        if info.uid.is_empty() {
            return Err(XhsError::Api("这个主页不存在或不可见".into()));
        }

        let mut notes = Vec::new();
        if let Some(cards) = state.pointer("/user/notes").and_then(|v| v.as_array()) {
            for card in cards {
                let Some(note) = card_from_value(card) else { continue };
                notes.push(note);
            }
        }
        Ok((info, notes))
    }

    /// 当前登录态与账号（`__INITIAL_STATE__.user.loggedIn` 是权威标志；
    /// 账号资料优先取 userPageData，取不到再退回 userInfo 的多套字段名）。
    pub async fn account(&self) -> Result<Option<(String, String, String)>> {
        let url = "https://www.xiaohongshu.com/user/profile/me";
        let html = match self.fetch_text(url, crate::client::SITE_REFERER).await {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        let Ok(state) = XhsClient::parse_initial_state(&html) else {
            return Ok(None);
        };
        let logged_in = state
            .pointer("/user/loggedIn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !logged_in {
            return Ok(None);
        }
        // userPageData（登录后是对象；未登录时实测为空数组）
        let page = state.pointer("/user/userPageData").filter(|v| v.is_object());
        let info = state.pointer("/user/userInfo").filter(|v| v.is_object());
        let pick = |keys: &[&str]| -> String {
            for source in [page, info] {
                if let Some(obj) = source {
                    for key in keys {
                        if let Some(text) = obj.get(key).and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                return text.to_string();
                            }
                        }
                    }
                }
            }
            String::new()
        };
        let uid = pick(&["id", "userId", "user_id"]);
        let nickname = pick(&["nickname", "nickName", "nick_name", "name"]);
        let avatar = pick(&["headImage", "image", "avatar", "images"]);
        if uid.is_empty() && nickname.is_empty() {
            return Ok(None);
        }
        Ok(Some((uid, nickname, avatar)))
    }
}

/// 笔记详情页 __INITIAL_STATE__ → NoteData。
/// noteDetailMap 以笔记 ID 为键（正常只有一个键），取第一个有效 note。
pub fn note_from_state(state: &Value) -> Option<NoteData> {
    let map = state.get("noteDetailMap")?.as_object()?;
    for (_id, wrapper) in map {
        let Some(note) = wrapper.get("note") else { continue };
        if note.get("noteId").is_none() && note.get("desc").is_none() {
            continue;
        }
        return Some(note_from_value(note));
    }
    None
}

/// note 对象 → NoteData。
pub fn note_from_value(note: &Value) -> NoteData {
    let mut pics = Vec::new();
    if let Some(arr) = note.get("imageList").and_then(|v| v.as_array()) {
        for pic in arr {
            // 首选 infoList 里的 WB_DFT（默认大图），退回 urlDefault / urlPre
            let url = pic
                .get("infoList")
                .and_then(|v| v.as_array())
                .and_then(|list| {
                    list.iter()
                        .find(|item| str_at(item, &["imageScene"]).as_deref() == Some("WB_DFT"))
                        .or_else(|| list.first())
                })
                .and_then(|item| str_at(item, &["url"]))
                .filter(|u| !u.is_empty())
                .or_else(|| str_at(pic, &["urlDefault"]))
                .or_else(|| str_at(pic, &["urlPre"]))
                .unwrap_or_default();
            if !url.is_empty() {
                pics.push(https(&url));
            }
        }
    }

    let mut videos = Vec::new();
    let mut duration = 0;
    if let Some(stream) = note.pointer("/video/media/stream") {
        // 优先 h264（兼容性最好）；没有再退 h265
        for key in ["h264", "h265", "av1"] {
            let Some(list) = stream.get(key).and_then(|v| v.as_array()) else { continue };
            for item in list {
                let Some(url) = str_at(item, &["masterUrl"]).filter(|u| !u.is_empty()) else {
                    continue;
                };
                let height = item.pointer("/videoMeta/height").and_then(|v| v.as_u64()).unwrap_or(0);
                let label = height_label(height);
                let dur = item.pointer("/videoMeta/duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if duration == 0 && dur > 0.0 {
                    duration = (dur / 1000.0) as i64;
                }
                videos.push((label, https(&url)));
            }
            if !videos.is_empty() {
                break;
            }
        }
        // 按清晰度从高到低（数字序，不能按标签字典序——"480P" > "1080P" 是陷阱）
        videos.sort_by(|a, b| height_rank(&b.0).cmp(&height_rank(&a.0)));
        videos.dedup_by(|a, b| a.1 == b.1);
    }

    let desc = str_at(note, &["desc"]).unwrap_or_default();
    let title = {
        let raw = str_at(note, &["title"]).unwrap_or_default();
        if raw.trim().is_empty() {
            crate::login::title_preview(&desc, 30)
        } else {
            raw
        }
    };

    NoteData {
        note_id: str_at(note, &["noteId"]).unwrap_or_default(),
        xsec_token: str_at(note, &["xsecToken"]).unwrap_or_default(),
        title,
        desc: strip_html(&desc),
        created_at: note
            .get("time")
            .and_then(|v| v.as_i64())
            .map(|ms| ms / 1000)
            .unwrap_or(0),
        user_id: str_at(note, &["user", "userId"]).unwrap_or_default(),
        nickname: str_at(note, &["user", "nickname"]).unwrap_or_default(),
        pics,
        videos,
        liked: str_at(note, &["interactInfo", "likedCount"]).unwrap_or_default(),
        duration,
    }
}

/// 主页/探索页的笔记卡片 → 轻量 NoteData（标题/作者/类型；媒体要再进详情页）。
pub fn card_from_value(card: &Value) -> Option<NoteData> {
    let id = str_at(card, &["id"]).filter(|s| !s.is_empty())?;
    let note_card = card.get("noteCard").cloned().unwrap_or_default();
    let token = ["xsecToken"]
        .iter()
        .find_map(|key| str_at(card, &[key]))
        .or_else(|| str_at(&note_card, &["xsecToken"]))
        .or_else(|| str_at(&note_card, &["user", "xsecToken"]))
        .unwrap_or_default();
    let is_video = str_at(&note_card, &["type"]).as_deref() == Some("video");
    let title = {
        let raw = str_at(&note_card, &["displayTitle"]).unwrap_or_default();
        if raw.trim().is_empty() {
            "（无标题笔记）".to_string()
        } else {
            raw
        }
    };
    let created_at = card
        .get("time")
        .or_else(|| note_card.get("time"))
        .and_then(|v| v.as_i64())
        .map(|ms| ms / 1000)
        .unwrap_or(0);

    Some(NoteData {
        note_id: id,
        xsec_token: token,
        title,
        desc: String::new(),
        created_at,
        user_id: str_at(&note_card, &["user", "userId"]).unwrap_or_default(),
        nickname: str_at(&note_card, &["user", "nickname"])
            .or_else(|| str_at(&note_card, &["user", "nickName"]))
            .unwrap_or_default(),
        pics: Vec::new(),
        videos: if is_video {
            vec![("视频".to_string(), String::new())]
        } else {
            Vec::new()
        },
        liked: str_at(&note_card, &["interactInfo", "likedCount"]).unwrap_or_default(),
        duration: 0,
    })
}

/// 视频清晰度标签：按高度取整档。
fn height_label(height: u64) -> String {
    if height >= 1080 {
        "1080P".into()
    } else if height >= 720 {
        "720P".into()
    } else if height >= 540 {
        "540P".into()
    } else if height >= 480 {
        "480P".into()
    } else if height > 0 {
        format!("{height}P")
    } else {
        "视频".into()
    }
}

/// 清晰度标签 → 数值高度（排序用；无法解析的当 0）。
fn height_rank(label: &str) -> u64 {
    label.trim_end_matches('P').trim().parse().unwrap_or(0)
}

fn str_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    match current {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

pub fn strip_html(html: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(html, "");
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_note_state() {
        let state: Value = serde_json::from_str(
            r#"{
              "noteDetailMap": {
                "66b7d3a7000000001e01c8b3": {
                  "note": {
                    "noteId": "66b7d3a7000000001e01c8b3",
                    "type": "normal",
                    "title": "测试标题",
                    "desc": "正文内容",
                    "time": 1759291200000,
                    "user": { "userId": "58c8014a6a6a690dbb0c84c1", "nickname": "示例作者" },
                    "interactInfo": { "likedCount": "1234" },
                    "imageList": [
                      {
                        "urlDefault": "https://sns-webpic-qc.xhscdn.com/2025/abc!nd_dft_wlteh-webp_3",
                        "infoList": [
                          { "imageScene": "WB_PRV", "url": "https://sns-webpic-qc.xhscdn.com/2025/abc!nd_dft_prv-webp_3" },
                          { "imageScene": "WB_DFT", "url": "https://sns-webpic-qc.xhscdn.com/2025/abc!nd_dft_wlteh-webp_3" }
                        ]
                      }
                    ]
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let note = note_from_state(&state).unwrap();
        assert_eq!(note.note_id, "66b7d3a7000000001e01c8b3");
        assert_eq!(note.title, "测试标题");
        assert_eq!(note.nickname, "示例作者");
        assert_eq!(note.pics.len(), 1);
        assert_eq!(note.created_at, 1759291200);
        assert!(note.videos.is_empty());
    }

    #[test]
    fn parses_video_streams() {
        let note: Value = serde_json::from_str(
            r#"{
              "noteId": "n1",
              "type": "video",
              "title": "视频笔记",
              "desc": "",
              "video": {
                "media": {
                  "stream": {
                    "h264": [
                      { "masterUrl": "https://sns-video.xhscdn.com/stream/low", "videoMeta": { "height": 480, "duration": 30000 } },
                      { "masterUrl": "https://sns-video.xhscdn.com/stream/high", "videoMeta": { "height": 1080, "duration": 30000 } }
                    ]
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let data = note_from_value(&note);
        assert!(data.has_video());
        assert_eq!(data.videos[0].0, "1080P", "高清晰度应排在前：{:?}", data.videos);
        assert_eq!(data.title, "视频笔记");
        assert_eq!(data.duration, 30);
    }
}
