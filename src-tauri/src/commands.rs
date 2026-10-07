//! 命令层：把 xhs-core 的能力暴露给界面。所有命令返回 Result<T, String>，
//! 错误一律转成人话（界面 toast 直接显示）。

use crate::naming;
use crate::state::{AppState, BatchCache, Settings};
use crate::types::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Emitter;
use tauri::Manager;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use xhs_core::download as dl;
use xhs_core::parser::{parse_input, SourceTarget};
use xhs_core::XhsClient;

const TASK_EVENT: &str = "task://update";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

type PauseFlag = Arc<std::sync::atomic::AtomicBool>;

fn flag_set(flag: &PauseFlag) -> bool {
    flag.load(std::sync::atomic::Ordering::Relaxed)
}

/// 把头像下载下来转成 data URL 内嵌进 LoginInfo：
/// CDN 在 WebView2 里偶发加载失败（curl 直连却 200，实测踩过），
/// 后端用 reqwest（已验证可靠）下载一次，前端就与 CDN 彻底解耦。
async fn inline_avatar(client: &XhsClient, face_url: &str) -> String {
    use base64::Engine as _;
    if face_url.is_empty() {
        return String::new();
    }
    let resp = client.http.get(face_url).send().await;
    let Ok(resp) = resp else { return face_url.to_string() };
    if !resp.status().is_success() {
        return face_url.to_string();
    }
    let Ok(bytes) = resp.bytes().await else { return face_url.to_string() };
    // 只内嵌合理大小的图（头像不会超过 1 MiB），异常大文件退回原 URL
    if bytes.len() > 1024 * 1024 {
        return face_url.to_string();
    }
    let mime = if bytes.starts_with(&[0x89, b'P']) {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if bytes.len() > 10 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    };
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    )
}

// ───────────────────────── 应用状态 ─────────────────────────

#[tauri::command]
pub async fn app_status(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<AppStatus, String> {
    warmup_once(&state).await;
    let settings = state.settings_snapshot();

    // 登录态检测改为**后台延迟**执行。旧逻辑在启动路径上同步等 40~80 秒，期间：
    // 1) 界面一直"未登录"；2) 它导航 xhs-work 窗口去 /me，用户此时点 WEB 就会
    // 互相抢窗口（实测：检测把用户正看的页面抢走、自己也被迫失败）。
    // 现在：启动立即返回（用内存缓存），检测在后台跑——窗口正被用户看着就让路，
    // 等它收起再测；结果出来后刷新 state.login 并推 login://confirmed 给界面。
    let login = state.login.read().unwrap().clone().unwrap_or_default();
    {
        let app2 = app.clone();
        tokio::spawn(async move {
            // 等启动首屏稳住再开始
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut attempts = 0u32;
            loop {
                if attempts >= 4 {
                    xhs_core::login::debug_log("启动恢复登录：多次读取失败，本轮放弃（下次启动再测）");
                    break;
                }
                // 用户正看着网页窗口就让路：等它收起再测（不计入尝试次数）
                if let Some(w) = app2.get_webview_window("xhs-work") {
                    if w.is_visible().unwrap_or(false) {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                }
                attempts += 1;
                let state = app2.state::<crate::state::AppState>();
                match tokio::time::timeout(Duration::from_secs(40), browser_login_check(&app2)).await {
                    Ok(Ok(Some((uid, name, avatar)))) => {
                        match finish_login(&app2, &state, uid, name, avatar).await {
                            Ok(info) => {
                                let _ = app2.emit("login://confirmed", info);
                                xhs_core::login::debug_log("启动恢复登录：已登录");
                            }
                            Err(e) => xhs_core::login::debug_log(&format!("启动恢复登录：收尾失败 {e}")),
                        }
                        break;
                    }
                    Ok(Ok(None)) => {
                        // 确认未登录：清掉内存里的旧态（界面默认就是未登录，不用推事件）
                        *state.login.write().unwrap() = None;
                        xhs_core::login::debug_log("启动恢复登录：未登录");
                        break;
                    }
                    Ok(Err(e)) => {
                        // 读失败 ≠ 未登录（网络慢/风控）：重试，不清登录态
                        xhs_core::login::debug_log(&format!(
                            "启动恢复登录：读取失败（第 {} 次）：{e}",
                            attempts
                        ));
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    }
                    Err(_) => {
                        xhs_core::login::debug_log(&format!(
                            "启动恢复登录：检测超时（第 {} 次）",
                            attempts
                        ));
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    }
                }
            }
        });
    }

    // 自检开关：数据目录里放一个 login_selftest 文件 → 启动后自动探一次登录页
    // （验证 WebView2 里的二维码状态与保活脚本；平时不跑）
    let selftest_marker = settings.data_root().join("login_selftest");
    if selftest_marker.exists() {
        // 标记文件的内容可以是"要额外探测的链接"（为空就只跑常规探测）
        let probe_url = std::fs::read_to_string(&selftest_marker)
            .unwrap_or_default()
            .trim()
            .to_string();
        let _ = std::fs::remove_file(&selftest_marker);
        let app2 = app.clone();
        let settings2 = settings.clone();
        tokio::spawn(async move { login_selftest(app2, settings2, probe_url).await });
    }

    // 笔记自检开关：数据目录里放一个 note_selftest 文件 → 启动后用本会话实测
    // 「笔记详情页是否被弹回首页」（远程判断下载失败是不是风控；平时不跑）。
    // 文件内容可以是"要探测的笔记链接"（为空就自动从探索页抓一张新卡片）
    let note_marker = settings.data_root().join("note_selftest");
    if note_marker.exists() {
        let probe_url = std::fs::read_to_string(&note_marker)
            .unwrap_or_default()
            .trim()
            .to_string();
        let _ = std::fs::remove_file(&note_marker);
        let app2 = app.clone();
        let settings2 = settings.clone();
        tokio::spawn(async move { note_selftest(app2, settings2, probe_url).await });
    }

    Ok(AppStatus {
        version: APP_VERSION.to_string(),
        login,
        output_dir: settings.output_dir.to_string_lossy().to_string(),
        cookies_path: settings.cookies_path().to_string_lossy().to_string(),
    })
}

/// 会话预热：启动后访问一次首页，把访客 Cookie（acw_tc/a1/webId 等）放进会话。
async fn warmup_once(state: &State<'_, AppState>) {
    if state.warmed.swap(true, Ordering::SeqCst) {
        return;
    }
    let client = state.client();
    let _ = client.warmup().await;
}

#[tauri::command]
pub fn app_settings(state: State<'_, AppState>) -> Result<SettingsEnv, String> {
    let settings = state.settings_snapshot();
    Ok(SettingsEnv {
        cookies_path: settings.cookies_path().to_string_lossy().to_string(),
        cookies_saved: state.cookies_saved(),
        version: APP_VERSION.to_string(),
        settings,
    })
}

#[tauri::command]
pub fn update_settings(
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<SettingsEnv, String> {
    let mut next = settings;
    next.clamp();

    let old = state.settings_snapshot();
    let proxy_changed = old.proxy != next.proxy;
    let slots_changed = old.max_concurrent_tasks != next.max_concurrent_tasks;

    if proxy_changed {
        // 换会话时只搬访客 Cookie：登录凭据只活在浏览器窗口里（见 state.rs 的说明）
        let client = XhsClient::with_proxy(Some(&next.proxy)).map_err(err)?;
        let guest: Vec<(String, String)> = state
            .client()
            .export_cookies()
            .into_iter()
            .filter(|(name, _)| {
                matches!(name.as_str(), "a1" | "webId" | "xsecappid" | "webBuild" | "acw_tc")
            })
            .collect();
        client.install_cookies(&guest);
        *state.client.write().unwrap() = Arc::new(client);
    }
    if slots_changed {
        state.rebuild_slots();
    }

    next.save().map_err(err)?;
    *state.settings.lock().unwrap() = next.clone();
    // 数据目录可能变了，Cookie 跟着搬
    state.save_cookies();

    Ok(SettingsEnv {
        cookies_path: next.cookies_path().to_string_lossy().to_string(),
        cookies_saved: true,
        version: APP_VERSION.to_string(),
        settings: next,
    })
}

// ───────────────────────── 来源解析 ─────────────────────────

/// 笔记页地址：带 xsec_token（详情页多数情况必需），无 token 时用裸链接。
/// `source` = 抓这批卡片时的来源上下文（pc_search / pc_user / pc_feed …）：
/// token 与来源相关，写错来源会被站点弹回首页（实测踩过），所以跟着链接一起带着。
fn note_page_url(id: &str, token: &str, source: &str) -> String {
    if token.is_empty() {
        return format!("https://www.xiaohongshu.com/explore/{id}");
    }
    let src = if source.trim().is_empty() { "pc_user" } else { source.trim() };
    format!("https://www.xiaohongshu.com/explore/{id}?xsec_token={token}&xsec_source={src}")
}

/// 抽出来的 "这批卡片来自哪个来源上下文"（抽取脚本从页面 URL 里读的 xsec_source）
fn bundle_source(value: &serde_json::Value) -> String {
    value
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 视频清晰度选项：auto 永远在，hd/sd 按实际有没有给。
fn quality_options(videos: &[(String, String)]) -> Vec<QualityOption> {
    let has_hd = videos
        .iter()
        .any(|(label, _)| label == "1080P" || label == "720P");
    let has_sd = !videos.is_empty();
    vec![
        QualityOption { value: "auto".into(), label: "最佳可用".into(), available: true, hint: String::new() },
        QualityOption { value: "hd".into(), label: "高清".into(), available: has_hd, hint: if has_hd { String::new() } else { "该来源没有高清档".into() } },
        QualityOption { value: "sd".into(), label: "标清".into(), available: has_sd, hint: String::new() },
    ]
}

/// 主页地址：原链接带 token 就原样用（搜索来源的主页必须带），否则按 uid 拼裸链接。
fn user_fetch_url(uid: &str, token: &Option<String>, url: &str) -> String {
    if url.contains("/user/profile/") && url.contains("xsec_token=") {
        return url.to_string();
    }
    match token {
        Some(tok) if !tok.is_empty() => format!(
            "https://www.xiaohongshu.com/user/profile/{uid}?xsec_token={tok}&xsec_source=pc_search"
        ),
        _ => format!("https://www.xiaohongshu.com/user/profile/{uid}"),
    }
}

/// 笔记页地址：原链接已带 xsec_token 就原样用，否则把 token 补进查询串。
fn note_fetch_url(id: &str, token: &Option<String>, url: &str) -> String {
    let mut fetch_url = url.to_string();
    let has_token = fetch_url.contains("xsec_token=");
    match token {
        Some(tok) if !tok.is_empty() && !has_token => {
            let sep = if fetch_url.contains('?') { "&" } else { "?" };
            fetch_url = format!("{fetch_url}{sep}xsec_token={tok}&xsec_source=pc_search");
        }
        _ => {}
    }
    if fetch_url.is_empty() {
        fetch_url = format!("https://www.xiaohongshu.com/explore/{id}");
    }
    fetch_url
}

#[tauri::command]
pub async fn probe_source(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    input: String,
) -> Result<Probe, String> {
    warmup_once(&state).await;
    let client = state.client();

    // 归一输入；xhslink 短链要先跟随 302 解析成最终形态。
    // 原始输入保留一份用于缓存别名：probe_more 拿到的还是这条原始链接。
    let raw_input = input.clone();
    let input = match parse_input(&input) {
        Ok(target) => target,
        Err(e) if e.to_string().contains("NEED_RESOLVE:") => {
            let url = e
                .to_string()
                .split_once("NEED_RESOLVE:")
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_default();
            let resolved = client.resolve_redirect(url.trim()).await.map_err(err)?;
            parse_input(&resolved).map_err(err)?
        }
        Err(e) => return Err(err(e)),
    };

    match input {
        SourceTarget::Note { id, token, url } => {
            let fetch_url = note_fetch_url(&id, &token, &url);
            let note = fetch_note(&app, &client, &fetch_url, 2).await?;
            let entry = PostEntry {
                bid: note.note_id.clone(),
                mid: String::new(),
                title: naming::title_of(&note.title),
                author: note.nickname.clone(),
                created_at: note.created_at,
                pics: note.pics.len() as u32,
                has_video: !note.videos.is_empty(),
                is_audio: false,
                media_url: String::new(),
                page_url: fetch_url.clone(),
                duration: note.duration,
                is_retweet: false,
                media_unknown: false,
            };
            let probe = Probe {
                kind: "post".into(),
                bid: note.note_id.clone(),
                mid: String::new(),
                title: naming::title_of(&note.title),
                author: note.nickname.clone(),
                uid: note.user_id.clone(),
                cover: String::new(),
                note: String::new(),
                total: 1,
                loaded: 1,
                exhausted: true,
                qualities: quality_options(&note.videos),
                items: vec![entry],
            };
            let key = input_key(SourceTarget::Note {
                id: id.clone(),
                token: token.clone(),
                url: url.clone(),
            });
            let cache = BatchCache {
                uid: note.user_id.clone(),
                items: probe.items.clone(),
                exhausted: true,
                url: fetch_url.clone(),
                stale: 0,
            };
            state.cache_batch(key.clone(), cache.clone());
            if raw_input.trim() != key {
                state.cache_batch(raw_input.trim().to_string(), cache);
            }
            Ok(probe)
        }
        SourceTarget::Search { url } => {
            // 搜索结果页：解析结果里那一屏笔记卡片（与推荐流同构）
            let value = webview_fetch(&app, &url).await?;
            let cards = UserBundle::cards_from_web(&value);
            if cards.is_empty() {
                return Err(
                    "搜索结果页没有解析到笔记卡片（可能关键词无结果、被风控或页面还没加载完）——请稍后重试"
                        .into(),
                );
            }
            let entries: Vec<PostEntry> = cards.iter().map(|card| card.entry("")).collect();
            let count = entries.len();
            let probe = Probe {
                kind: "search".into(),
                bid: String::new(),
                mid: String::new(),
                title: "搜索结果".into(),
                author: String::new(),
                uid: String::new(),
                cover: String::new(),
                note: format!(
                    "搜索结果这一屏共 {count} 条。结果是持续加载的，程序只能读到当前这一屏；更精准的话可直接粘单条笔记链接"
                ),
                total: count,
                loaded: count,
                exhausted: true,
                qualities: quality_options(&[]),
                items: entries,
            };
            xhs_core::login::debug_log(&format!("probe search：本屏 {count} 条"));
            let cache = BatchCache {
                uid: String::new(),
                items: probe.items.clone(),
                exhausted: false,
                url: url.clone(),
                stale: 0,
            };
            state.cache_batch("search:last".to_string(), cache);
            Ok(probe)
        }
        SourceTarget::Feed => {
            // 推荐流首页：解析页面里这一屏笔记卡片（都带 token，可直接下载）
            let value = webview_fetch(&app, "https://www.xiaohongshu.com/explore").await?;
            let cards = UserBundle::cards_from_web(&value);
            if cards.is_empty() {
                return Err(
                    "推荐流页面没有解析到笔记卡片（可能被风控或页面还没加载完）——请稍后重试"
                        .into(),
                );
            }
            let entries: Vec<PostEntry> = cards.iter().map(|card| card.entry("")).collect();
            let count = entries.len();
            let probe = Probe {
                kind: "feed".into(),
                bid: String::new(),
                mid: String::new(),
                title: "推荐流".into(),
                author: String::new(),
                uid: String::new(),
                cover: String::new(),
                note: format!(
                    "推荐流这一屏共 {count} 条。推荐流是持续加载的，程序只能读到当前这一屏；手里有具体链接的话，直接粘链接或作者主页更准"
                ),
                total: count,
                loaded: count,
                exhausted: true,
                qualities: quality_options(&[]),
                items: entries,
            };
            xhs_core::login::debug_log(&format!("probe feed：本屏 {count} 条"));
            let cache = BatchCache {
                uid: String::new(),
                items: probe.items.clone(),
                exhausted: false,
                url: "https://www.xiaohongshu.com/explore".to_string(),
                stale: 0,
            };
            let key = "feed:explore".to_string();
            state.cache_batch(key, cache);
            Ok(probe)
        }
        SourceTarget::User { uid, token, url } => {
            let page_url = user_fetch_url(&uid, &token, &url);
            let user = fetch_user(&app, &client, &uid, &page_url).await?;
            let entries: Vec<PostEntry> =
                user.cards.iter().map(|card| card.entry(&user.nickname)).collect();
            // 主页可以边滚边加载，所以这里不算"到底"；继续解析时再滚
            let exhausted = false;
            let note = if entries.is_empty() {
                "没读到这个作者的笔记：列表可能还在加载、或被短时限流。稍等片刻重试一次；也可以直接粘贴单条笔记链接（在 App 里分享复制）试试"
                    .to_string()
            } else {
                // 网页端只内嵌最新一批笔记；更早的要走带签名的翻页接口
                format!(
                    "网页端仅能读取主页最新 {} 条笔记，更早的内容请在 App 里分享具体笔记链接",
                    entries.len()
                )
            };
            // 总数 = 主页声明的笔记总数：界面显示"已加载 N / 总数"
            let probe = Probe {
                kind: "user".into(),
                bid: String::new(),
                mid: String::new(),
                title: user.nickname.clone(),
                author: user.nickname.clone(),
                uid: user.uid.clone(),
                cover: user.avatar.clone(),
                note,
                total: if user.notes_count > 0 {
                    user.notes_count as usize
                } else {
                    entries.len()
                },
                loaded: entries.len(),
                exhausted,
                qualities: quality_options(&[]),
                items: entries,
            };
            xhs_core::login::debug_log(&format!(
                "probe user={uid} 本页 {} 条（可继续解析滚动加载）",
                probe.items.len()
            ));
            let cache = BatchCache {
                uid: user.uid.clone(),
                items: probe.items.clone(),
                exhausted,
                url: page_url.clone(),
                stale: 0,
            };
            let key = format!("user:{uid}");
            state.cache_batch(key.clone(), cache.clone());
            if raw_input.trim() != key {
                state.cache_batch(raw_input.trim().to_string(), cache);
            }
            Ok(probe)
        }
    }
}

fn input_key(target: SourceTarget) -> String {
    match target {
        SourceTarget::Note { id, .. } => format!("post:{id}"),
        SourceTarget::User { uid, .. } => format!("user:{uid}"),
        SourceTarget::Feed => "feed:explore".to_string(),
        SourceTarget::Search { .. } => "search:last".to_string(),
    }
}

#[tauri::command]
pub async fn probe_more(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    input: String,
    want: usize,
) -> Result<ProbeMore, String> {
    let client = state.client();
    // 输入可能是任意链接形态，归一成缓存键
    let key = match parse_input(&input) {
        Ok(target) => input_key(target),
        Err(e) if e.to_string().contains("NEED_RESOLVE:") => {
            let url = e
                .to_string()
                .split_once("NEED_RESOLVE:")
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_default();
            let resolved = client.resolve_redirect(url.trim()).await.map_err(err)?;
            input_key(parse_input(&resolved).map_err(err)?)
        }
        Err(_) => input.trim().to_string(),
    };

    let (url, mut items, mut exhausted, mut stale) = {
        let batches = state.batches.lock().unwrap();
        let Some(cache) = batches.get(&key) else {
            return Err("这个来源的解析缓存已过期，请回到输入页重新解析".into());
        };
        (
            cache.url.clone(),
            cache.items.clone(),
            cache.exhausted,
            cache.stale,
        )
    };

    if exhausted {
        return Ok(ProbeMore {
            items: Vec::new(),
            loaded: items.len(),
            total: 0,
            exhausted: true,
            note: String::new(),
        });
    }

    // 回到来源页面（用户可能已经翻到别的页）——隐式导航，不打扰用户
    let settings = state.settings_snapshot();
    let window = ensure_work_window(&app, &settings).await?;
    let current = window
        .url()
        .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
        .unwrap_or_default();
    if current != url {
        if let Ok(parsed) = url.parse::<tauri::Url>() {
            let _ = window.navigate(parsed);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    // 可见模式（与 DYdown 的可见加载一致）：解析过程就在网页窗口里进行，
    // 用户能看着列表一页页滚出来；不再后台偷偷滚
    if !window.is_visible().unwrap_or(false) {
        let _ = window.show();
        let _ = window.set_focus();
        xhs_core::login::debug_log("继续解析：切到网页窗口（可见模式，滚动加载中）");
    }

    // 滚到底 + 抽取：条数不再增长连续 3 轮即认为到底（参考 DYdown 的判定）
    let mut rounds = 0;
    let mut added: Vec<PostEntry> = Vec::new();
    while rounds < 8 && !exhausted {
        rounds += 1;
        let _ = window.eval(SCROLL_BOTTOM_JS);
        // 节奏放慢一点：滚太快容易触发小红书的风控（验证码）
        tokio::time::sleep(Duration::from_millis(1800)).await;
        let Some(value) = extract_from_page(&window).await else {
            stale += 1;
            if stale >= 3 {
                exhausted = true;
            }
            continue;
        };
        let cards = UserBundle::cards_from_web(&value);
        if cards.len() > items.len() {
            // 页面状态是"累积"的：整份替换即可（多的就是新滚出来的）
            let fresh: Vec<PostEntry> = cards.iter().map(|card| card.entry("")).collect();
            for entry in fresh {
                if !items.iter().any(|old| old.bid == entry.bid) {
                    items.push(entry.clone());
                    added.push(entry);
                }
            }
            stale = 0;
            if added.len() >= want.max(1) {
                break;
            }
        } else {
            stale += 1;
            if stale >= 3 {
                exhausted = true;
            }
        }
    }

    xhs_core::login::debug_log(&format!(
        "继续解析：{key} 新增 {} 条（累计 {}，{}）",
        added.len(),
        items.len(),
        if exhausted { "已到底" } else { "还有更多" }
    ));

    // 写回缓存
    {
        let mut batches = state.batches.lock().unwrap();
        if let Some(cache) = batches.get_mut(&key) {
            cache.items = items.clone();
            cache.exhausted = exhausted;
            cache.stale = stale;
        }
    }

    Ok(ProbeMore {
        items: added,
        loaded: items.len(),
        total: 0,
        exhausted,
        note: String::new(),
    })
}

// ───────────────────────── 下载任务 ─────────────────────────

#[tauri::command]
pub async fn start_download(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    req: DownloadReq,
) -> Result<String, String> {
    let id = state.next_task_id();
    let task = TaskUpdate {
        id: id.clone(),
        title: req.title.clone(),
        quality_label: req.quality_label.clone(),
        status: "queued".into(),
        image_count: req.image_count,
        video_count: req.video_count,
        is_audio: req.is_audio,
        message: "排队中".into(),
        ..Default::default()
    };
    emit_task(&app, &task);

    let pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = tokio::spawn(run_task(app.clone(), req.clone(), id.clone()));
    state.tasks.lock().unwrap().insert(
        id.clone(),
        crate::state::TaskEntry {
            snapshot: Arc::new(Mutex::new(task)),
            abort: Some(handle.abort_handle()),
            req,
            pause,
        },
    );
    Ok(id)
}

#[tauri::command]
pub fn cancel_download(app: tauri::AppHandle, state: State<'_, AppState>, task_id: String) -> Result<(), String> {
    let snapshot = {
        let tasks = state.tasks.lock().unwrap();
        tasks.get(&task_id).map(|e| {
            if let Some(abort) = &e.abort {
                abort.abort();
            }
            e.snapshot.clone()
        })
    };
    if let Some(snapshot) = snapshot {
        let mut task = snapshot.lock().unwrap();
        // 必须写回快照本体：孤儿 reporter 每 500ms 会重播快照，
        // 只改克隆的话「已取消」立刻被覆盖回「下载中」
        if !matches!(task.status.as_str(), "done" | "failed" | "canceled") {
            task.status = "canceled".into();
            task.message = "已取消".into();
            task.speed_bps = 0.0;
        }
        emit_task(&app, &task.clone());
    }
    Ok(())
}

fn emit_task(app: &tauri::AppHandle, task: &TaskUpdate) {
    let _ = app.emit(TASK_EVENT, task);
}

/// 一条笔记的下载流水线：图片 → 视频 → 文案，进度按阶段汇报。
async fn run_task(app: tauri::AppHandle, req: DownloadReq, id: String) {
    let state = app.state::<AppState>();
    let snapshot = state
        .tasks
        .lock()
        .unwrap()
        .get(&id)
        .map(|e| e.snapshot.clone());
    let Some(snapshot) = snapshot else { return };
    let (pause, entry_req) = {
        let tasks = state.tasks.lock().unwrap();
        match tasks.get(&id) {
            Some(entry) => (entry.pause.clone(), Some(entry.req.clone())),
            None => (Arc::new(std::sync::atomic::AtomicBool::new(false)), None),
        }
    };
    let _ = entry_req;
    // 创建后立刻被暂停（如「全部暂停」在排队期间触发）：直接进入暂停态
    if flag_set(&pause) {
        let task = {
            let mut task = snapshot.lock().unwrap();
            task.status = "paused".into();
            task.message = "已暂停".into();
            task.clone()
        };
        emit_task(&app, &task);
        return;
    }

    let settings = state.settings_snapshot();
    let client = state.client();
    let _slot = state.slots().acquire_owned().await;

    // done 与各 progress 闭包各自持有独立克隆，避免借用与 move 冲突
    let app_done = app.clone();
    let snapshot_done = snapshot.clone();

    let done = move |status: &str, message: &str, output: &str| {
        let task = {
            // 必须写回快照本体：只改克隆的话，后端账本停在旧状态，
            // 「全部暂停」的守卫会把已完成任务再翻成已暂停（实测踩过）
            let mut t = snapshot_done.lock().unwrap();
            t.status = status.to_string();
            t.message = message.to_string();
            t.speed_bps = 0.0;
            if !output.is_empty() {
                t.output_path = output.to_string();
            }
            t.clone()
        };
        emit_task(&app_done, &task);
    };

    // 熔断闸门：当前会话正被站点限流时就别逐条撞墙了（任务直接给出明确原因）
    if let Err(reason) = note_block_gate(&app) {
        done("failed", &reason, "");
        return;
    }
    // 取详情（重试几次，拿最新的图片/视频地址）
    let fetch_url = if req.page_url.is_empty() {
        format!("https://www.xiaohongshu.com/explore/{}", req.bid)
    } else {
        req.page_url.clone()
    };
    let note = match fetch_note(&app, &client, &fetch_url, settings.retry_count as usize).await {
        Ok(note) => note,
        Err(e) => {
            done("failed", &e, "");
            return;
        }
    };

    let title = {
        let t = naming::title_of(&note.title);
        if t.is_empty() { req.title.clone() } else { t }
    };

    // 渲染路径：文件夹层级 + 条目名（不带扩展名的公共前缀）
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("title".into(), title.clone());
    vars.insert("author".into(), if req.author.is_empty() { note.nickname.clone() } else { req.author.clone() });
    vars.insert("bid".into(), req.bid.clone());
    vars.insert("uid".into(), req.uid.clone());
    vars.insert("publish_date".into(), req.naming.publish_date.clone());
    vars.insert("date".into(), req.naming.date.clone());
    vars.insert("year".into(), req.naming.year.clone());
    vars.insert("month".into(), req.naming.month.clone());
    vars.insert("source_kind".into(), req.naming.source_kind.clone());
    vars.insert("index".into(), naming::padded_index(req.naming.index, req.naming.index_pad));
    vars.insert("ext".into(), String::new());

    let folder = naming::render(&settings.folder_template, &vars, true);
    let prefix = naming::render(&settings.naming_template, &vars, false);
    let base_dir = settings.output_dir.join(&folder);
    if let Err(e) = tokio::fs::create_dir_all(&base_dir).await {
        done("failed", &format!("创建目录失败：{e}"), "");
        return;
    }

    // 重名处理
    let conflict = settings.rename_conflict.clone();
    let final_path = |name: &str, ext: &str| -> PathBuf {
        let mut path = base_dir.join(format!("{name}.{ext}"));
        if conflict == "auto" {
            let mut seq = 1;
            while path.exists() {
                path = base_dir.join(format!("{name} ({seq}).{ext}"));
                seq += 1;
            }
        }
        path
    };

    // skip 模式：文案或首个媒体已存在 → 整条跳过（不重复下载）
    if conflict == "skip" {
        let text_probe = base_dir.join(format!("{prefix}.txt"));
        let media_probe = if !note.videos.is_empty() {
            base_dir.join(format!("{prefix}.mp4"))
        } else if note.pics.len() == 1 {
            base_dir.join(format!("{prefix}.jpg"))
        } else if !note.pics.is_empty() {
            base_dir.join(format!("{prefix}-1.jpg"))
        } else {
            text_probe.clone()
        };
        if media_probe.exists() || text_probe.exists() {
            // 找出该条目已落盘的主文件（跳过时 output 指向文件而非目录）
            let existing = std::fs::read_dir(&base_dir)
                .ok()
                .and_then(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .find(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().starts_with(&prefix))
                                .unwrap_or(false)
                        })
                });
            {
                let mut task = snapshot.lock().unwrap();
                task.image_pct = 100.0;
                task.video_pct = 100.0;
                // 回填文件大小：跳过的任务不显示「0 B」
                if let Some(size) = existing
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .filter(|s| *s > 0)
                {
                    task.downloaded = size;
                    task.total = size;
                }
            }
            done(
                "done",
                "文件已存在，跳过下载",
                &existing
                    .unwrap_or(base_dir)
                    .to_string_lossy(),
            );
            return;
        }
    }

    // 主文件：完成后「打开」直接打开它（视频 > 首图 > 文案）
    let mut primary: Option<PathBuf> = None;

    // 阶段一：图片
    let image_spec = if req.image.is_empty() { settings.image_format.clone() } else { req.image.clone() };
    let pic_total = note.pics.len();
    let opts = dl::DownloadOptions { retries: settings.retry_count as usize };
    let mut downloaded_total: u64 = 0;

    if pic_total > 0 {
        {
            let mut task = snapshot.lock().unwrap();
            task.status = "downloading".into();
            task.message = "下载图片".into();
        }
        for (index, pic) in note.pics.iter().enumerate() {
            let candidates = dl::image_candidates(pic, &image_spec);
            let ext = candidates.first().map(|u| url_ext(u)).unwrap_or_else(|| "jpg".into());
            let name = if pic_total == 1 { prefix.clone() } else { format!("{prefix}-{}", index + 1) };
            let dest = final_path(&name, &ext);

            let snap = snapshot.clone();
            let app_c = app.clone();
            let base_before = downloaded_total;
            let pic_index = index;
            let pic_total_c = pic_total;
            let progress: dl::ProgressFn = Arc::new(move |p| {
                let mut task = snap.lock().unwrap();
                // 任务已终局（被取消/暂停）：孤儿 reporter 不再把旧进度播回 UI
                if matches!(task.status.as_str(), "done" | "failed" | "canceled" | "paused") {
                    return;
                }
                task.downloaded = base_before + p.downloaded;
                task.total = base_before + p.total;
                task.speed_bps = p.speed_bps;
                task.image_pct = (pic_index as f64 + p.downloaded as f64 / p.total.max(1) as f64)
                    / pic_total_c as f64
                    * 100.0;
                emit_task(&app_c, &task.clone());
            });

            if dest.exists() {
                // 续传：图片已完成，跳过
                let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                downloaded_total += size;
                if primary.is_none() {
                    primary = Some(dest.clone());
                }
                continue;
            }
            if flag_set(&pause) {
                let task = {
                    let mut task = snapshot.lock().unwrap();
                    task.status = "paused".into();
                    task.message = "已暂停".into();
                    task.clone()
                };
                emit_task(&app, &task);
                return;
            }
            // 图片也走断点续传：暂停能在一个分块内停下（普通下载不检查旗标，
            // 会出现「UI 已暂停、底层还在下」），中止也不会留下半个损坏的图片文件。
            // 候选逐个试：某些变体在 CDN 上是死的（403），换同图的另一个变体即可
            let mut last_err = String::from("没有可用的图片地址");
            let mut saved = false;
            for (cand_index, url) in candidates.iter().enumerate() {
                if flag_set(&pause) {
                    let task = {
                        let mut task = snapshot.lock().unwrap();
                        task.status = "paused".into();
                        task.message = "已暂停".into();
                        task.downloaded = downloaded_total;
                        task.speed_bps = 0.0;
                        task.clone()
                    };
                    emit_task(&app, &task);
                    return;
                }
                match dl::download_resumable(
                    &client.http,
                    url,
                    &dest,
                    &opts,
                    progress.clone(),
                    Some(pause.clone()),
                )
                .await
                {
                    Ok(dl::DownloadEnd::Completed(size)) => {
                        downloaded_total += size;
                        if primary.is_none() {
                            primary = Some(dest.clone());
                        }
                        saved = true;
                        break;
                    }
                    Ok(dl::DownloadEnd::Paused(size)) => {
                        let task = {
                            let mut task = snapshot.lock().unwrap();
                            task.status = "paused".into();
                            task.message = "已暂停".into();
                            task.downloaded = downloaded_total + size;
                            task.speed_bps = 0.0;
                            task.clone()
                        };
                        emit_task(&app, &task);
                        return;
                    }
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&dest).await;
                        last_err = format!("{e}");
                        if cand_index + 1 < candidates.len() {
                            xhs_core::login::debug_log(&format!(
                                "图片第 {} 张的候选 {} 失败（{last_err}），换下一个变体：{}",
                                index + 1,
                                cand_index + 1,
                                xhs_core::error::truncate(url, 120)
                            ));
                        }
                    }
                }
            }
            if !saved {
                // 全部候选都失败：把真正试过的地址记进日志，便于复盘
                xhs_core::login::debug_log(&format!(
                    "图片下载失败（已试 {} 个变体）：{}",
                    candidates.len(),
                    candidates
                        .iter()
                        .map(|u| xhs_core::error::truncate(u, 160))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
                done("failed", &format!("图片下载失败：{last_err}"), "");
                return;
            }
        }
        {
            let mut task = snapshot.lock().unwrap();
            task.image_pct = 100.0;
            task.speed_bps = 0.0;
            emit_task(&app, &task.clone());
        }
    }

    // 阶段二：视频
    if !note.videos.is_empty() {
        {
            let mut task = snapshot.lock().unwrap();
            task.status = "downloading".into();
            task.message = "下载视频".into();
            if task.quality_label.is_empty() {
                task.quality_label = "视频".into();
            }
        }
        let (label, url) = pick_video(&note.videos, &req.quality);
        {
            xhs_core::login::debug_log(&format!(
                "视频挑选：quality={} 选「{label}」，候选 {:?}",
                req.quality,
                note.videos.iter().map(|(l, _)| l).collect::<Vec<_>>()
            ));
        }
        let dest = final_path(&prefix, "mp4");

        let snap = snapshot.clone();
        let app_c = app.clone();
        let base_before = downloaded_total;
        let progress: dl::ProgressFn = Arc::new(move |p| {
            let mut task = snap.lock().unwrap();
            if matches!(task.status.as_str(), "done" | "failed" | "canceled" | "paused") {
                return;
            }
            task.downloaded = base_before + p.downloaded;
            task.total = base_before + p.total;
            task.speed_bps = p.speed_bps;
            task.video_pct = if p.total > 0 { p.downloaded as f64 / p.total as f64 * 100.0 } else { 0.0 };
            emit_task(&app_c, &task.clone());
        });

        match dl::download_resumable(&client.http, &url, &dest, &opts, progress, Some(pause.clone())).await
        {
            Ok(dl::DownloadEnd::Completed(size)) => {
                downloaded_total += size;
                primary = Some(dest.clone());
                let mut task = snapshot.lock().unwrap();
                task.video_pct = 100.0;
                task.speed_bps = 0.0;
                if !label.is_empty() {
                    task.quality_label = format!("视频 · {label}");
                }
                emit_task(&app, &task.clone());
            }
            Ok(dl::DownloadEnd::Paused(size)) => {
                let task = {
                    let mut task = snapshot.lock().unwrap();
                    task.status = "paused".into();
                    task.message = "已暂停（点击继续以恢复）".into();
                    task.downloaded = base_before + size;
                    task.speed_bps = 0.0;
                    task.clone()
                };
                emit_task(&app, &task);
                // 暂停后立即返回，流水线停止（.part 保留进度，继续时从断点恢复）
                return;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&dest).await;
                done("failed", &format!("视频下载失败：{e}"), "");
                return;
            }
        }
    }

    // 阶段三：文案
    if settings.download_text {
        {
            let mut task = snapshot.lock().unwrap();
            task.status = "saving".into();
            task.message = "保存文案".into();
            task.video_pct = 100.0;
            emit_task(&app, &task.clone());
        }
        let date_text = if note.created_at > 0 {
            let (y, m, d) = civil_from_unix(note.created_at + 8 * 3600);
            format!("{y}-{m:02}-{d:02}")
        } else {
            String::new()
        };
        let body = format!(
            "{}\n\n——\n作者：{}\n发布时间：{}\n链接：{}\n下载于：XHSdown v{}\n",
            note.desc, note.nickname, date_text, fetch_url, APP_VERSION
        );
        let dest = final_path(&prefix, "txt");
        if let Err(e) = tokio::fs::write(&dest, body).await {
            done("failed", &format!("文案写入失败：{e}"), "");
            return;
        }
    }

    {
        let mut task = snapshot.lock().unwrap();
        task.downloaded = task.total.max(downloaded_total);
    }
    if primary.is_none() && settings.download_text {
        // 纯文字笔记：主文件就是文案
        primary = Some(base_dir.join(format!("{prefix}.txt")));
    }
    xhs_core::login::debug_log(&format!(
        "任务完成：bid={} 主文件={}",
        req.bid,
        primary.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
    ));
    done(
        "done",
        "已完成",
        &primary.unwrap_or(base_dir).to_string_lossy(),
    );
}

/// 清晰度档位排名（越大越高）：1080 > 720 > 540 > 480 > 其余。
fn quality_rank(label: &str) -> u32 {
    let l = label.to_lowercase();
    if l.contains("1080") {
        5
    } else if l.contains("720") {
        4
    } else if l.contains("540") {
        3
    } else if l.contains("480") {
        2
    } else {
        1
    }
}

/// 按清晰度挑播放地址：auto 取最高档，hd 优先 ≥720 的最高档，sd 取最低档。
fn pick_video(videos: &[(String, String)], quality: &str) -> (String, String) {
    if videos.is_empty() {
        return Default::default();
    }
    // 档位按清晰度从高到低排（同档保持原顺序）
    let mut sorted: Vec<&(String, String)> = videos.iter().collect();
    sorted.sort_by(|a, b| quality_rank(&b.0).cmp(&quality_rank(&a.0)));

    match quality {
        "hd" => sorted
            .iter()
            .find(|(label, _)| quality_rank(label) >= 4)
            .or_else(|| sorted.first())
            .map(|&(l, u)| (l.clone(), u.clone()))
            .unwrap_or_default(),
        "sd" => sorted
            .iter()
            .rev()
            .find(|(label, _)| quality_rank(label) <= 2)
            .or_else(|| sorted.last())
            .map(|&(l, u)| (l.clone(), u.clone()))
            .unwrap_or_default(),
        // auto：最高档
        _ => sorted
            .first()
            .map(|&(l, u)| (l.clone(), u.clone()))
            .unwrap_or_default(),
    }
}

fn url_ext(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    // CDN 变体把格式写在 `!规格` 里（如 `!nc_n_webp_mw_1`），路径上没有扩展名；
    // 不解析这一层就只能一律存成 .jpg —— 实际内容是 webp（实测）
    if let Some((_, suffix)) = path.rsplit_once('!') {
        let low = suffix.to_ascii_lowercase();
        for ext in ["avif", "webp", "heic", "png", "jpeg", "jpg"] {
            if low.contains(ext) {
                return ext.to_string();
            }
        }
    }
    match path.rsplit_once('.') {
        Some((_, ext)) if ext.len() <= 5 => ext.to_ascii_lowercase(),
        _ => "jpg".to_string(),
    }
}

/// Unix 秒 → （年, 月, 日）（东八区口径由调用方加好偏移）。
fn civil_from_unix(secs: i64) -> (i64, u32, u32) {
    let days = secs.div_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ───────────────────────── 网页登录（内嵌官方页） ─────────────────────────

// ── 浏览器取数 ─────────────────────────────────────────────
//
// 为什么不让 reqwest 直接抓：小红书会拒绝"复制 Cookie 再来请求"的会话
// （实测：登录小窗里 Cookie 明明齐全，程序自己请求仍被判未登录）。
// 所以取数一律借登录窗口那个真实浏览器的会话——签名与风控由浏览器处理，
// 程序只负责最后一步：下载 CDN 直链（那些地址自带签名，不需要 Cookie）。

/// 唯一的浏览器窗口：**登录与取数都用它**。
///
/// 为什么不分成两个窗口：WebView2 在同一数据目录下的多个环境之间不能可靠共享
/// 会话 Cookie——实测"登录窗关闭后取数窗就变成未登录"，所以干脆一个窗口一个会话，
/// 登录态天然共享，没有跨环境竞态。
async fn ensure_work_window(
    app: &tauri::AppHandle,
    settings: &Settings,
) -> Result<tauri::WebviewWindow, String> {
    if let Some(window) = app.get_webview_window("xhs-work") {
        return Ok(window);
    }
    let dir = crate::state::webview_dir(settings);
    std::fs::create_dir_all(&dir).map_err(err)?;
    let app_for_new_window = app.clone();
    let window = tauri::WebviewWindowBuilder::new(
        app,
        "xhs-work",
        tauri::WebviewUrl::External("about:blank".parse().unwrap()),
    )
    .title("XHSdown")
    .inner_size(1000.0, 760.0)
    .visible(false)
    .data_directory(dir)
    // 站内链接常以"新窗口"打开（target=_blank / window.open）：浏览器里是新标签页，
    // 而我们没有新窗口处理，请求会被静默丢弃——表现就是"点了没反应"。
    // 这里统一改成就地跳转，与浏览器点击行为一致。
    .on_new_window(move |url, _features| {
        let text = url.to_string();
        if text.starts_with("http://") || text.starts_with("https://") {
            if let Some(work) = app_for_new_window.get_webview_window("xhs-work") {
                let _ = work.navigate(url.clone());
            }
        }
        tauri::webview::NewWindowResponse::Deny
    })
    .build()
    .map_err(err)?;
    Ok(window)
}

/// 滚到底 + 点「加载更多」（参考 DYdown 的做法）：
/// 无限列表要靠滚动触发下一页请求；有些页面还用"点击加载更多"按钮。
const SCROLL_BOTTOM_JS: &str = r#"(function () {
  try {
    var all = document.querySelectorAll('*');
    for (var i = 0; i < all.length; i++) {
      var e = all[i];
      try {
        var st = getComputedStyle(e);
        if ((st.overflowY === 'auto' || st.overflowY === 'scroll') && e.scrollHeight > e.clientHeight + 50) {
          // 分两步滚：有些加载器只在"能看到底部"时才请求下一页
          e.scrollTop = Math.max(0, e.scrollHeight - e.clientHeight - 400);
          e.scrollTop = e.scrollHeight - e.clientHeight;
          try { e.dispatchEvent(new Event('scroll', { bubbles: true })); } catch (x) {}
        }
      } catch (x) {}
    }
    try { window.scrollTo(0, (document.body || document.documentElement).scrollHeight); } catch (x) {}
    // 无限列表常用 scroll 事件驱动加载，这里补一发
    try { window.dispatchEvent(new Event('scroll', { bubbles: true })); } catch (x) {}
    // 只认文本明确的按钮，避免误点关注/收藏
    var btns = document.querySelectorAll('button, div[role="button"], span');
    for (var j = 0; j < btns.length; j++) {
      var t = (btns[j].textContent || '').trim();
      if (t.length < 16 && (t.indexOf('加载更多') >= 0 || t.indexOf('点击加载') >= 0 || t.indexOf('查看更多') >= 0)) {
        btns[j].click();
        break;
      }
    }
    return 1;
  } catch (e) { return 0; }
})();"#;

/// 页面提取脚本：把当前页的关键数据（笔记详情 / 用户主页）打包写进 URL 片段。
/// webview 里的远程页面没有 Tauri IPC，URL 片段是唯一能把值带回 Rust 的通道。
const EXTRACT_JS: &str = r#"(function () {
  var report = function (out) {
    try {
      history.replaceState(null, '', '#xhsdownid=' + Date.now() + '~' + encodeURIComponent(JSON.stringify(out)));
    } catch (e) {}
  };
  // 小红书把状态字段包在 Vue 的 ref 里（{__v_isRef:true,_value:…}），必须逐层剥开，
  // 否则读到的永远是包装壳（实测踩过：登录后 userPageData 看着是空对象，其实是 ref）
  var unwrap = function (v) {
    var depth = 0;
    while (v && typeof v === 'object' && v.__v_isRef === true && depth < 6) {
      v = v._value !== undefined ? v._value : v.value;
      depth += 1;
    }
    return v;
  };
  var str = function (v) {
    v = unwrap(v);
    if (typeof v === 'string') return v;
    if (v && typeof v === 'object') {
      var c = ['url', 'imageUrl', 'urlDefault', 'src', 'image'];
      for (var i = 0; i < c.length; i++) {
        var t = unwrap(v[c[i]]);
        if (typeof t === 'string') return t;
      }
    }
    return '';
  };
  var pick = function (o, keys) {
    o = unwrap(o);
    if (!o || typeof o !== 'object') return '';
    for (var i = 0; i < keys.length; i++) {
      var t = str(o[keys[i]]);
      if (t) return t;
    }
    return '';
  };
  var mergeInto = function (target, source) {
    source = unwrap(source);
    if (!source || typeof source !== 'object') return target;
    var keys = Object.keys(source);
    for (var i = 0; i < keys.length; i++) {
      if (target[keys[i]] === undefined) target[keys[i]] = source[keys[i]];
    }
    return target;
  };
  var arr = function (v) {
    v = unwrap(v);
    return Array.isArray(v) ? v : [];
  };
  // 收集循环每轮全量抽取、跨轮按 id 合并，但整包 JSON 塞进 URL 片段在几千条时会撑爆
  // （Chromium 单个 URL 约 2MB，中文 encodeURIComponent 还要再涨 3 倍）。
  // 只回传"新增的那一段"，附 40 条重叠防止序号错位；首轮/换页后 prev=0，自然回全量。
  // 标记按"整页地址"记账：单页应用里换到另一个主页时地址变了，prev 自动归零回全量
  var delta = function (key, list) {
    try {
      var here = location.href.split('#')[0];
      var old = window[key];
      var prev = (old && old.url === here) ? old.n : 0;
      window[key] = { url: here, n: list.length };
      if (list.length > 400 && prev > 0 && list.length >= prev) {
        return list.slice(Math.max(0, prev - 40));
      }
    } catch (e) {}
    return list;
  };

  try {
    var st = window.__INITIAL_STATE__ || null;
    if (!st) {
      report({ ready: false, pageUrl: here });
      return;
    }

    // 先按**当前网址**判断页面类型，再读对应的数据。
    // 不能"状态里有什么就读什么"：小红书是单页应用，状态会残留上一次看过的
    // 笔记/主页数据（实测：在推荐流页面上读到了残留的笔记，登录流程一直等不到昵称）
    // 页面地址一律去掉 #片段：上报时若带上一次 marker，会层层嵌套越滚越长（实测）
    var here = location.href.split('#')[0];
    var urlNote = here.match(/(?:\/explore\/|\/discovery\/item\/)([0-9a-f]{24})/);
    var isNotePage = !!urlNote;
    var isProfilePage = here.indexOf("/user/profile/") >= 0;
    // 裸 /explore（可带查询串）才算推荐流：带笔记 id 的走笔记分支
    var isFeedPage = !urlNote && /\/explore(\/|\?|$)/.test(here);
    // 搜索结果页（/search_result?keyword=…）；判定要放在"未就绪兜底"之前，
    // 否则会被当成无关页面直接挡掉（实测踩过）
    var isSearchPage = here.indexOf("/search_result") >= 0 || /\/search(\/|\?|$)/.test(here);
    if (!isNotePage && !isProfilePage && !isFeedPage && !isSearchPage) {
      report({ ready: false, pageUrl: here });
      return;
    }
    // 页面 URL 里的来源上下文（pc_search / pc_user / pc_feed …）。
    // 卡片 token 与它相关：下载时要用同一个来源去开笔记页，写错来源会被站点弹回首页（实测踩过）
    var pageSource = '';
    try {
      var sm = (location.search || '').match(/[?&]xsec_source=([^&]+)/);
      if (sm) pageSource = decodeURIComponent(sm[1]);
    } catch (e) {}

    // ── 搜索结果页：这一屏结果卡片（与信息流同构，在 search.feeds 下）──
    if (isSearchPage) {
      var searchRoot = unwrap(st.search) || {};
      var searchFeeds = arr(searchRoot.feeds);
      var searchCards = [];
      for (var si = 0; si < searchFeeds.length; si++) {
        var sc = unwrap(searchFeeds[si]) || {};
        var snc = unwrap(sc.noteCard) || {};
        var scu = unwrap(snc.user) || {};
        var sid = sc.id || '';
        if (!sid) continue;
        searchCards.push({
          id: sid,
          token: sc.xsecToken || '',
          title: snc.displayTitle || '',
          type: snc.type || '',
          time: sc.time || snc.time || 0,
          author: scu.nickname || scu.nickName || '',
        });
      }
      report({
        ready: searchCards.length > 0,
        kind: 'search',
        pageUrl: here,
        source: pageSource,
        uid: '', nickname: '搜索结果', avatar: '', desc: '',
        notesCount: searchCards.length,
        notes: delta('__xhsdownSearchN', searchCards),
      });
      return;
    }

    // ── 推荐流首页：这一屏的笔记卡片（都带 token，勾选即可下载）──
    if (isFeedPage) {
      var feedRoot = unwrap(st.feed) || {};
      var feeds = arr(feedRoot.feeds);
      var feedCards = [];
      for (var fi = 0; fi < feeds.length; fi++) {
        var fc = unwrap(feeds[fi]) || {};
        var fnc = unwrap(fc.noteCard) || {};
        var fcu = unwrap(fnc.user) || {};
        var fid = fc.id || "";
        if (!fid) continue;
        feedCards.push({
          id: fid,
          token: fc.xsecToken || "",
          title: fnc.displayTitle || "",
          type: fnc.type || "",
          time: fc.time || fnc.time || 0,
          author: fcu.nickname || fcu.nickName || "",
        });
      }
      report({
        ready: feedCards.length > 0,
        kind: "feed",
        pageUrl: here,
        uid: "",
        nickname: "推荐流",
        avatar: "",
        desc: "",
        source: pageSource,
        notesCount: feedCards.length,
        notes: delta('__xhsdownFeedN', feedCards),
      });
      return;
    }

    // ── 笔记详情页 ──
    // 真实路径可能在 st.noteDetailMap，也可能在 st.note.noteDetailMap（两种都试）
    var noteRoot = unwrap(st.note) || {};
    var map = unwrap(st.noteDetailMap) || unwrap(noteRoot.noteDetailMap) || {};
    var ids = Object.keys(map);
    if (isNotePage && ids.length) {
      var note = unwrap(unwrap(map[ids[0]]) || {}) || {};
      note = unwrap(note.note) || note;
      var pics = [];
      var list = arr(note.imageList);
      for (var i = 0; i < list.length && i < 80; i++) {
        var pic = unwrap(list[i]) || {};
        // 每张图给一串候选（首选在前）：实测同图的不同变体有的在 CDN 上是死的
        // （视频封面 `!nc_n_webp_mw` 403、它的 `_1` 版本正常），只发一个地址会整条失败
        var alts = [];
        var addAlt = function (u) {
          u = str(u);
          if (u && alts.indexOf(u) < 0) alts.push(u);
        };
        var infos = arr(pic.infoList);
        var scenes = [];
        for (var j = 0; j < infos.length; j++) {
          var infoItem = unwrap(infos[j]) || {};
          if (!infoItem.url) continue;
          scenes.push({ scene: String(infoItem.imageScene || ''), url: String(infoItem.url) });
        }
        // WB_DFT（尺寸最大）优先，其次 WB_PRV，其余变体垫后
        scenes.sort(function (a, b) {
          var rank = function (s) { return s === 'WB_DFT' ? 0 : (s === 'WB_PRV' ? 1 : 2); };
          return rank(a.scene) - rank(b.scene);
        });
        for (var j = 0; j < scenes.length; j++) addAlt(scenes[j].url);
        addAlt(pic.urlDefault);
        addAlt(pic.urlPre);
        if (alts.length) pics.push(alts);
      }
      var videos = [];
      var video = unwrap(note.video) || {};
      var media = unwrap(video.media) || {};
      var streams = unwrap(media.stream) || {};
      // 实测：流的编码键名已从 h264/h265/av1 改成 EF4/EF5 这类内部码——
      // 不能再按编码名取，遍历 stream 下**所有**条目，凡是带 masterUrl 的都收进来，
      // 按视频码率从高到低排（auto 取首个=最高画质；同分辨率下 h264 码率通常也更高）
      var pool = [];
      var streamKeys = Object.keys(streams);
      for (var c = 0; c < streamKeys.length; c++) {
        var streamArr = arr(streams[streamKeys[c]]);
        for (var k = 0; k < streamArr.length; k++) {
          var it = unwrap(streamArr[k]) || {};
          var murl = str(it.masterUrl);
          if (!murl) continue;
          var meta = unwrap(it.videoMeta) || {};
          pool.push({
            url: murl,
            height: Number(unwrap(it.height)) || Number(unwrap(meta.height)) || 0,
            duration: Number(unwrap(it.videoDuration)) || Number(unwrap(meta.duration)) || 0,
            bitrate: Number(unwrap(it.videoBitrate)) || 0,
          });
        }
      }
      pool.sort(function (a, b) { return b.bitrate - a.bitrate; });
      for (var v = 0; v < pool.length; v++) {
        videos.push({ url: pool[v].url, height: pool[v].height, duration: pool[v].duration });
      }
      var nu = unwrap(note.user) || {};
      report({
        ready: true, kind: 'note', pageUrl: here,
        noteId: note.noteId || ids[0],
        title: note.title || '',
        desc: String(note.desc || '').slice(0, 20000),
        time: note.time || 0,
        userId: nu.userId || '', nickname: nu.nickname || '', avatar: str(nu.image),
        images: pics, videos: videos
      });
      return;
    }

    // ── 用户主页 ──
    if (!isProfilePage) {
      report({ ready: false, pageUrl: here });
      return;
    }
    var pu = unwrap(st.user) || {};
    var page = unwrap(pu.userPageData) || {};
    if (typeof page !== 'object' || Array.isArray(page)) page = {};
    var info = unwrap(pu.userInfo) || {};
    if (typeof info !== 'object' || Array.isArray(info)) info = {};
    var merged = mergeInto(mergeInto({}, info), page);

    // 访问的是谁：URL 里的 uid 最可靠（/user/profile/{uid}），/me 没有就用状态里的
    var urlUid = '';
    var um = location.href.match(/profile\/([0-9a-f]{24})/);
    if (um) urlUid = um[1];
    var stateUid = pick(merged, ['id', 'userId', 'user_id', 'userid']);

    // 昵称：标题优先（非通用标题即为目标昵称），否则取状态
    var rawTitle = (document.title || '').trim();
    var genericTitle = rawTitle === '小红书' || rawTitle.indexOf('你的生活兴趣社区') >= 0;
    var titleName = genericTitle ? '' : rawTitle.replace(/\s*[-—|]\s*小红书.*$/, '').trim();
    var nameEl = document.querySelector(
      '[class*="user-name"], [class*="nickname"], [class*="user-nickname"]'
    );
    var domName = '';
    if (nameEl) {
      var nt = (nameEl.textContent || '').trim();
      if (nt && nt.length <= 30 && nt.indexOf('小红书') < 0) domName = nt;
    }
    // 状态里的昵称只在"状态 uid 与链接 uid 一致"（或本来就没有 uid，如 /me）时采用，
    // 否则可能把登录者自己的昵称显示成访问对象的（实测：状态里的 userPageData 可能是"我"）
    var stateUidMatches = !urlUid || (!!stateUid && stateUid === urlUid);
    var stateName = stateUidMatches
      ? pick(merged, ['nickname', 'nickName', 'nick_name', 'name', 'userName'])
      : '';
    var nameOut = titleName || stateName || domName;

    // 头像
    var avatarOut = pick(merged, ['headImage', 'image', 'avatar', 'images', 'avatarUrl', 'imageb']);
    if (!avatarOut) {
      var imgs = document.querySelectorAll('img');
      for (var ii = 0; ii < imgs.length && ii < 400; ii++) {
        var isrc = imgs[ii].getAttribute('src') || '';
        if (isrc.indexOf('sns-avatar') >= 0) {
          avatarOut = isrc;
          break;
        }
      }
    }

    // 笔记列表：页面把**访问对象**的主页笔记放在 user.notes 里。
    // 注意结构可能是"分页嵌套"——每一项是一页（20 条）的容器，键名是数字，
    // 必须递归展平；否则一条都读不到（实测：某主页 notes 有 5 项、实际 100 条笔记）
    var flatten = function (value) {
      var out = [];
      var walk = function (node, depth) {
        if (depth > 4 || out.length >= 20000) return;
        var list = arr(node);
        for (var i = 0; i < list.length && out.length < 20000; i++) {
          var item = unwrap(list[i]);
          if (Array.isArray(item)) {
            walk(item, depth + 1);
            continue;
          }
          if (!item || typeof item !== 'object') continue;
          var keys = Object.keys(item);
          var numericOnly =
            keys.length > 0 &&
            keys.every(function (k) {
              return /^[0-9]+$/.test(k);
            });
          if (numericOnly && !item.id && !item.noteId && !item.noteCard) {
            walk(
              keys.map(function (k) {
                return item[k];
              }),
              depth + 1
            );
            continue;
          }
          out.push(item);
        }
      };
      walk(value, 0);
      return out;
    };

    var cards = [];
    var noteList = flatten(pu.notes);
    // 这里**不能**再卡条数：滚动收集是"每轮全量抽取 + 跨轮合并"，
    // 抽取上限一旦低于页面已加载数，合并数就再也不涨 → 被误判成"到底"停住（实测卡在 300）
    for (var n = 0; n < noteList.length; n++) {
      var card = noteList[n];
      var nc = unwrap(card.noteCard) || {};
      var cu = unwrap(nc.user) || {};
      var cid = card.id || card.noteId || nc.noteId || nc.id || '';
      if (!cid) continue;
      cards.push({
        id: cid,
        token: card.xsecToken || cu.xsecToken || nc.xsecToken || '',
        title: nc.displayTitle || nc.title || '',
        type: nc.type || '',
        time: card.time || nc.time || 0,
        author: cu.nickname || cu.nickName || ''
      });
    }

    // 就绪判定：页面自己说"主页抓取完成"（userFetchingStatus=resolved）、或已拿到笔记，
    // 且必须有昵称——笔记列表会先于资料到达，只认昵称会得到"有列表没资料"的半成品
    var fetchStatus = String(unwrap(pu.userFetchingStatus) || '');
    if (!nameOut) {
      report({ ready: false, pageUrl: here });
      return;
    }
    if (fetchStatus !== 'resolved' && cards.length === 0) {
      report({ ready: false, pageUrl: here });
      return;
    }
    // 总数实测在 userPageData.posted（纯数字）；notes/noteCount 是列表字段（可能是数组），当兜底
    var numOf = function (v) {
      v = unwrap(v);
      return typeof v === 'number' && isFinite(v) ? Math.floor(v) : 0;
    };
    var countOut = numOf(merged.posted) || numOf(merged.notes) || numOf(merged.noteCount);
    report({
      ready: true, kind: 'user', pageUrl: here,
      uid: urlUid || stateUid,
      nickname: nameOut,
      avatar: avatarOut,
      desc: String(pick(merged, ['desc', 'description']) || '').slice(0, 3000),
      source: pageSource,
      notesCount: countOut,
      notes: delta('__xhsdownUserN', cards)
    });
    return;
  } catch (e) {
    report({ error: String(e), pageUrl: here });
  }
})();
"#;

/// 在当前页面注入提取脚本并把片段读回来。
async fn extract_from_page(window: &tauri::WebviewWindow) -> Option<serde_json::Value> {
    let _ = window.eval(EXTRACT_JS);
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(160)).await;
        let Ok(url) = window.url() else { continue };
        let text = url.to_string();
        let Some(idx) = text.find("xhsdownid=") else { continue };
        let rest = &text[idx + "xhsdownid=".len()..];
        let Some(tilde) = rest.find('~') else { continue };
        let payload = rest[tilde + 1..].split('&').next().unwrap_or("");
        let decoded = percent_decode(payload);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&decoded) {
            return Some(value);
        }
    }
    None
}

/// 用浏览器会话打开一个页面并取回数据。
/// 串行执行：取数窗口只有一个，并发导航会互相打断。
async fn webview_fetch(app: &tauri::AppHandle, url: &str) -> Result<serde_json::Value, String> {
    webview_fetch_inner(app, url, false).await
}

/// 取数地址 → 期望的页面类型（与抽取脚本上报的 kind 一一对应）。
/// 空串 = 不限类型（不认识的地址就不拦）。
fn expected_page_kind(url: &str) -> &'static str {
    let path = url.split('?').next().unwrap_or(url);
    if path.contains("/search_result") || path.contains("/search/") {
        return "search";
    }
    if path.contains("/user/profile/") {
        return "user";
    }
    if let Some(idx) = path
        .find("/explore/")
        .or_else(|| path.find("/discovery/item/"))
    {
        let tail = path[idx..].trim_end_matches('/').rsplit('/').next().unwrap_or("");
        // /explore/{id} 才是笔记页；裸 /explore 是推荐流
        return if tail.len() >= 20 && tail.chars().all(|c| c.is_ascii_alphanumeric()) {
            "note"
        } else {
            "feed"
        };
    }
    if path.trim_end_matches('/').ends_with("/explore") {
        return "feed";
    }
    ""
}

/// `allow_login_window`：登录流程自己要借这个窗口读资料，那时不能拦。
async fn webview_fetch_inner(
    app: &tauri::AppHandle,
    url: &str,
    allow_login_window: bool,
) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let settings = state.settings_snapshot();
    let _guard = state.fetch_lock.lock().await;

    // 登录窗正开着（用户在登录）就别抢窗口：会把登录页导航走
    if !allow_login_window && state.login_window_open.load(Ordering::SeqCst) {
        return Err(
            "登录窗口还开着：请先完成登录（成功后会自动关闭）或关掉它，再解析".into(),
        );
    }

    let window = ensure_work_window(app, &settings).await?;
    // 取数前窗口是否可见要记下来：隐藏窗口的渲染与定时器会被 WebView2 挂起/节流
    // （见 lib.rs 的启动参数），日志里有这条信息才能一眼分辨"取数失败"是不是窗口被挂起
    let was_visible = window.is_visible().unwrap_or(false);
    if allow_login_window && was_visible {
        // 登录检测让路：窗口正被用户看着（在浏览/登录页在前面），把它导航去 /me
        // 等于当面抢走用户的页面，检测自己也会跟着失败（实测：启动检测与 WEB 点击打架）
        return Err("网页窗口正被使用，登录检测稍后再试".into());
    }
    // 用户正在看这个网页窗口时先收起：取数要导航，别当面把人家页面翻走
    if !allow_login_window && was_visible {
        let _ = window.hide();
        xhs_core::login::debug_log("取数前收起网页窗口（避免把用户正在看的页面导航走）");
    }
    let parsed: tauri::Url = url.parse().map_err(|e| format!("地址无效：{e}"))?;
    // 导航前的地址：轮询里用它判断"导航是否已经落地"（落地了才做快速失败判断，
    // 否则会把还没被替换掉的旧页面误当成"被弹走"）
    let before_url = window
        .url()
        .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
        .unwrap_or_default();
    {
        // 目标地址记进日志（token 只留长度）：取数失败时能一眼看出是链接形态问题还是站点问题
        let (note_id, token) = split_note_url(url);
        xhs_core::login::debug_log(&format!(
            "取数目标：{} （id={note_id} token 长度={}；起始页={}；窗口={}）",
            xhs_core::error::truncate(url.split('?').next().unwrap_or(url), 100),
            token.len(),
            xhs_core::error::truncate(&before_url, 90),
            if was_visible { "可见" } else { "隐藏" }
        ));
    }
    window.navigate(parsed).map_err(err)?;

    // 这次要的是哪类页面：轮询时只有同类型的 ready 才算数。
    // 不校验就会读到**上一个页面**的 ready —— 实测取笔记时导航还没落地，
    // 抽取先把用户主页读成 ready（kind=user），装进 NoteBundle 就成了
    // "页面里没有笔记数据（可能已被删除或仅自己可见）"
    let expect = expected_page_kind(url);
    let started = std::time::Instant::now();
    let mut last_page = String::new();
    let mut stale_kind_logged = false;
    // 浏览器地址每次变化都记一笔：导航有没有落地、被弹到哪儿，全看这条日志
    let mut last_live = before_url.clone();
    // 目标地址是否真的落地过：落过又被弹走 = 站点级风控（不是链接坏）
    let target_url = url.split('#').next().unwrap_or(url).to_string();
    let mut reached_target = false;
    while started.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(700)).await;

        // 导航已落地（浏览器地址变了）时先做快速失败：被站点弹回首页 / 弹到登录页或
        // 404 都不必再等满 30 秒——候选阶梯要靠它把每个失败形态压到一两秒
        let live_url = window
            .url()
            .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
            .unwrap_or_default();
        if !live_url.is_empty() && live_url != last_live {
            xhs_core::login::debug_log(&format!(
                "取数：浏览器地址 → {}",
                xhs_core::error::truncate(&live_url, 120)
            ));
            last_live = live_url.clone();
        }
        if !live_url.is_empty() && live_url == target_url {
            reached_target = true;
        }
        if !live_url.is_empty() && live_url != before_url {
            if live_url.contains("/explore") && !live_url.contains("/explore/") && expect == "note" {
                // 落地过又被弹走：token/链接都是好的（浏览器里同一地址能开），
                // 是这台会话被站点定向限流了——别再逐条撞墙
                let msg = if reached_target {
                    format!(
                        "{NOTE_BLOCK_HINT}：笔记页刚打开就被弹回首页，而主页/解析仍正常。请等几分钟再试，或先用网页窗口手动打开一条笔记确认能正常浏览"
                    )
                } else {
                    "这个链接已失效：页面跳回了首页（token 过期或已被使用过）".to_string()
                };
                return Err(msg);
            }
            if live_url.contains("/404") || live_url.contains("/login") {
                let kind = if live_url.contains("/404") { "404/不可见" } else { "登录页" };
                return Err(format!(
                    "需要登录：小红书要求登录后才能浏览该内容（落地页={kind}；{}）",
                    xhs_core::error::truncate(&live_url, 120)
                ));
            }
        }

        let Some(value) = extract_from_page(&window).await else { continue };
        if let Some(page) = value.get("pageUrl").and_then(|v| v.as_str()) {
            // 去掉 #片段：marker 会层层嵌套，日志会被撑爆（实测踩过）
            last_page = page.split('#').next().unwrap_or(page).to_string();
        }
        if value.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
            let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if expect.is_empty() || kind == expect {
                return Ok(value);
            }
            if !stale_kind_logged {
                stale_kind_logged = true;
                xhs_core::login::debug_log(&format!(
                    "取数：读到的还是上一个页面（要 {expect}，读到 {kind}），继续等导航落地"
                ));
            }
        }
        if let Some(e) = value.get("error").and_then(|v| v.as_str()) {
            xhs_core::login::debug_log(&format!("取数脚本异常：{e}"));
        }
        // 风控验证码页：把网页窗口打开让用户过验证（滑块/验证码由真人完成）
        if last_page.contains("/captcha") || last_page.contains("website-login") {
            if !allow_login_window {
                if let Some(win) = app.get_webview_window("xhs-work") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            return Err(
                "小红书要求完成安全验证：已为您打开网页窗口，请在页面里完成滑块/验证码，完成后回到解析页重试（建议同时登录账号，可减少验证频率）"
                    .into(),
            );
        }

        // 明确的"要登录 / 不可见"页面：不用再等
        if last_page.contains("/login") || last_page.contains("/404") {
            let kind = if last_page.contains("/404") { "404/不可见" } else { "登录页" };
            return Err(format!(
                "需要登录：小红书要求登录后才能浏览该内容（落地页={kind}；{}）",
                xhs_core::error::truncate(&last_page, 120)
            ));
        }
    }
    // 页面跳回首页（/explore 裸页）通常意味着链接里的 token 已失效或被用过
    if last_page.contains("/explore") && !last_page.contains("/explore/") {
        return Err(
            "这个链接已失效：页面跳回了首页（token 过期或已被使用过）。请在 App 里重新分享这条内容、或在用户主页解析出的列表里直接勾选下载"
                .into(),
        );
    }
    Err(format!(
        "页面读取超时（最后停在 {}）——可能是网络慢或被风控",
        if last_page.is_empty() { "未知页面" } else { &last_page }
    ))
}

/// 读当前页面上的账号资料（登录者是本人）。
///
/// 为什么不只读 DOM：小红书把"我的资料"放在全局状态里（`user.userPageData` /
/// `user.userInfo`，可能是 Vue ref），当前页面（推荐流等）通常已经带着它——
/// 直接读就不用导航到主页，既快又不会打扰站点（实测导航过去常被短时限流）。
/// 状态里没有时再退回 DOM：文档标题即昵称、头像图在 sns-avatar CDN。
const IDENTITY_DOM_JS: &str = r#"(function () {
  var report = function (out) {
    try {
      history.replaceState(null, '', '#xhsdownid=' + Date.now() + '~' + encodeURIComponent(JSON.stringify(out)));
    } catch (e) {}
  };
  var unwrap = function (v) {
    var d = 0;
    while (v && typeof v === 'object' && v.__v_isRef === true && d < 6) {
      v = v._value !== undefined ? v._value : v.value;
      d += 1;
    }
    return v;
  };
  var str = function (v) {
    v = unwrap(v);
    if (typeof v === 'string') return v;
    if (v && typeof v === 'object') {
      var c = ['url', 'imageUrl', 'urlDefault', 'src', 'image'];
      for (var i = 0; i < c.length; i++) {
        var t = unwrap(v[c[i]]);
        if (typeof t === 'string') return t;
      }
    }
    return '';
  };
  try {
    var out = { ready: true };
    var st = window.__INITIAL_STATE__ || {};
    var pu = unwrap(st.user) || {};
    var page = unwrap(pu.userPageData);
    if (!page || typeof page !== 'object' || Array.isArray(page)) page = {};
    var info = unwrap(pu.userInfo);
    if (!info || typeof info !== 'object' || Array.isArray(info)) info = {};
    var pick = function (o, keys) {
      for (var i = 0; i < keys.length; i++) {
        var t = str(o[keys[i]]);
        if (t) return t;
      }
      return '';
    };
    out.name =
      pick(page, ['nickname', 'nickName', 'nick_name', 'name']) ||
      pick(info, ['nickname', 'nickName', 'nick_name', 'name']);
    out.uid = pick(page, ['id', 'userId', 'user_id']) || pick(info, ['id', 'userId', 'user_id']);
    out.avatar =
      pick(page, ['headImage', 'image', 'avatar', 'images', 'avatarUrl']) ||
      pick(info, ['headImage', 'image', 'avatar', 'images', 'avatarUrl']);
    // 状态里没有就退回 DOM
    if (!out.name) {
      var raw = (document.title || '').trim();
      var generic = raw === '小红书' || raw.indexOf('你的生活兴趣社区') >= 0;
      if (!generic) out.name = raw.replace(/\s*[-—|]\s*小红书.*$/, '').trim();
    }
    if (!out.name) {
      var el = document.querySelector('[class*="user-name"], [class*="nickname"], [class*="user-nickname"]');
      if (el) {
        var t = (el.textContent || '').trim();
        if (t && t.length <= 30 && t.indexOf('小红书') < 0) out.name = t;
      }
    }
    if (!out.avatar) {
      var imgs = document.querySelectorAll('img');
      for (var i = 0; i < imgs.length && i < 400; i++) {
        var src = imgs[i].getAttribute('src') || '';
        if (src.indexOf('sns-avatar') >= 0) { out.avatar = src; break; }
      }
    }
    report(out);
  } catch (e) {
    report({ error: String(e) });
  }
})();"#;

/// 读取 IDENTITY_DOM_JS 写回的资料。
async fn read_identity(window: &tauri::WebviewWindow) -> Option<(String, String, String)> {
    for _ in 0..24 {
        tokio::time::sleep(Duration::from_millis(220)).await;
        let Ok(url) = window.url() else { continue };
        let text = url.to_string();
        let Some(idx) = text.find("xhsdownid=") else { continue };
        let rest = &text[idx + "xhsdownid=".len()..];
        let Some(tilde) = rest.find('~') else { continue };
        let payload = rest[tilde + 1..].split('&').next().unwrap_or("");
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&percent_decode(payload)) else {
            continue;
        };
        if value.get("error").is_some() {
            return None;
        }
        if !value.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue; // 标题还是站点默认的：稍后再试
        }
        let uid = value.get("uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let name = value.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let avatar = value.get("avatar").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        return Some((uid, name, avatar));
    }
    None
}

/// 让浏览器去读「我的主页」，判断当前是否已登录并尽量取回账号资料。
///
/// 判定依据是**页面有没有被踢到登录页**：未登录时 /user/profile/me 会 302 到 /login
/// （`webview_fetch` 会因此返回"需要登录"错误），登录成功则正常渲染。
/// 资料字段不能作为判据——登录后 userPageData 仍可能是空对象（实测踩过）。
async fn browser_login_check(
    app: &tauri::AppHandle,
) -> Result<Option<(String, String, String)>, String> {
    // 先看当前页面（多数时候已经带着登录者资料，不用导航，快且不打扰站点）
    let value = {
        let mut found: Option<serde_json::Value> = None;
        if let Some(window) = app.get_webview_window("xhs-work") {
            for _ in 0..3 {
                let _ = window.eval(IDENTITY_DOM_JS);
                if let Some((uid, name, avatar)) = read_identity(&window).await {
                    if !name.is_empty() {
                        found = Some(serde_json::json!({
                            "kind": "user", "uid": uid, "nickname": name, "avatar": avatar,
                        }));
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
        }
        match found {
            Some(value) => value,
            None => {
                // 当前页面读不到就导航到「我的主页」读一次
                match webview_fetch_inner(app, "https://www.xiaohongshu.com/user/profile/me", true)
                    .await
                {
                    Ok(value) => value,
                    Err(e) => {
                        // 读不到 ≠ 没登录：网络慢 / 被风控 / 窗口被隐藏时页面读也会失败。
                        // 返回 Err，让调用方区分"确认未登录"和"本次没读成"（启动时据此重试）
                        xhs_core::login::debug_log(&format!("登录检测：{e}"));
                        return Err(e);
                    }
                }
            }
        }
    };

    let mut uid = value.get("uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let mut name = value.get("nickname").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let mut avatar = value.get("avatar").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let notes = value
        .get("notes")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    // 判定门槛：**必须拿到真昵称**。笔记列表单独不算——实测遇到过
    // "笔记列表先到、资料还没渲染"的半就绪状态，那时收工就会得到
    // "已登录但昵称头像全空"的假成功（正是用户看到的怪状态）。
    if name.is_empty() {
        xhs_core::login::debug_log(&format!(
            "登录检测：已登录但资料未就绪（笔记 {notes} 条），继续等待"
        ));
        return Ok(None);
    }

    // 状态里没资料就退回 DOM（文档标题=昵称、头像图）
    if name.is_empty() || avatar.is_empty() {
        if let Some(window) = app.get_webview_window("xhs-work") {
            // 资料异步到达：最多重试 3 轮（每轮内部轮询 ~5 秒）
            for _ in 0..3 {
                let _ = window.eval(IDENTITY_DOM_JS);
                if let Some((_u, _n, _a)) = read_identity(&window).await {
                    break;
                }
            }
            let _ = window.eval(IDENTITY_DOM_JS);
            if let Some((dom_uid, dom_name, dom_avatar)) = read_identity(&window).await {
                if uid.is_empty() {
                    uid = dom_uid;
                }
                if name.is_empty() {
                    name = dom_name;
                }
                if avatar.is_empty() {
                    avatar = dom_avatar;
                }
            }
        }
    }

    xhs_core::login::debug_log(&format!(
        "登录检测：已登录（昵称={:?} uid={:?} 头像={}）",
        name,
        uid,
        if avatar.is_empty() { "无" } else { "有" }
    ));
    Ok(Some((uid, name, avatar)))
}

/// 笔记数据（reqwest 快路径与浏览器取数共用的中间结构）
#[derive(Debug, Clone, Default)]
struct NoteBundle {
    note_id: String,
    title: String,
    desc: String,
    created_at: i64,
    user_id: String,
    nickname: String,
    /// 每张图一条候选链（首选在前，下载失败可回退到同图其它变体）
    pics: Vec<Vec<String>>,
    videos: Vec<(String, String)>,
    duration: i64,
}

impl NoteBundle {
    fn from_web(value: &serde_json::Value) -> Result<Self, String> {
        let note_id = value
            .get("noteId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if note_id.is_empty() {
            return Err("页面里没有笔记数据（可能已被删除或仅自己可见）".into());
        }
        let mut pics: Vec<Vec<String>> = Vec::new();
        if let Some(arr) = value.get("images").and_then(|v| v.as_array()) {
            for item in arr {
                // 新格式：每个元素是同图的候选链（首选在前）；旧格式：单个字符串
                let mut alts: Vec<String> = Vec::new();
                match item {
                    serde_json::Value::Array(list) => {
                        for u in list {
                            if let Some(s) = u.as_str() {
                                if !s.is_empty() {
                                    alts.push(https(s));
                                }
                            }
                        }
                    }
                    serde_json::Value::String(s) if !s.is_empty() => alts.push(https(s)),
                    _ => {}
                }
                if !alts.is_empty() {
                    pics.push(alts);
                }
            }
        }
        let mut videos: Vec<(String, String)> = Vec::new();
        if let Some(arr) = value.get("videos").and_then(|v| v.as_array()) {
            for item in arr {
                let Some(url) = item
                    .get("url")
                    .and_then(|v| v.as_str())
                    .filter(|u| !u.is_empty())
                else {
                    continue;
                };
                let height = item.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                videos.push((height_label(height), https(url)));
            }
        }
        videos.sort_by(|a, b| height_rank(&b.0).cmp(&height_rank(&a.0)));
        videos.dedup_by(|a, b| a.1 == b.1);
        let duration = value
            .get("videos")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("duration"))
            .and_then(|v| v.as_f64())
            .map(|ms| (ms / 1000.0) as i64)
            .unwrap_or(0);
        Ok(Self {
            note_id,
            title: value.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            desc: value.get("desc").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            created_at: value
                .get("time")
                .and_then(|v| v.as_i64())
                .map(|ms| ms / 1000)
                .unwrap_or(0),
            user_id: value.get("userId").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            nickname: value.get("nickname").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            pics,
            videos,
            duration,
        })
    }
}

/// 主页/推荐流卡片
#[derive(Debug, Clone, Default)]
struct CardBundle {
    id: String,
    token: String,
    /// 这批卡片是从哪个来源上下文抓的（pc_search / pc_user / pc_feed …）。
    /// 小红书的 token 跟来源上下文相关：从搜索点进的主页，卡片 token 也带搜索上下文，
    /// 下载时若一律写 pc_user，站点会把链接弹回首页（实测踩过）。
    source: String,
    title: String,
    author: String,
    is_video: bool,
    created_at: i64,
}

impl CardBundle {
    /// 卡片 → 表格条目（媒体数量未知，进详情页才有）
    fn entry(&self, fallback_author: &str) -> PostEntry {
        PostEntry {
            bid: self.id.clone(),
            mid: String::new(),
            title: naming::title_of(&self.title),
            author: if self.author.is_empty() {
                fallback_author.to_string()
            } else {
                self.author.clone()
            },
            created_at: self.created_at,
            pics: 0,
            has_video: self.is_video,
            is_audio: false,
            media_url: String::new(),
            page_url: note_page_url(&self.id, &self.token, &self.source),
            duration: 0,
            is_retweet: false,
            media_unknown: true,
        }
    }
}

/// 用户主页数据
#[derive(Debug, Clone, Default)]
struct UserBundle {
    uid: String,
    nickname: String,
    avatar: String,
    notes_count: u32,
    cards: Vec<CardBundle>,
}

impl UserBundle {
    /// 只解析卡片数组（推荐流没有 uid，不能走 from_web 的归属校验）
    fn cards_from_web(value: &serde_json::Value) -> Vec<CardBundle> {
        let mut cards = Vec::new();
        if let Some(arr) = value.get("notes").and_then(|v| v.as_array()) {
            for item in arr {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                cards.push(CardBundle {
                    id,
                    token: item.get("token").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    source: bundle_source(value),
                    title: item.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    author: item.get("author").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    is_video: item.get("type").and_then(|v| v.as_str()) == Some("video"),
                    created_at: item
                        .get("time")
                        .and_then(|v| v.as_i64())
                        .map(|ms| ms / 1000)
                        .unwrap_or(0),
                });
            }
        }
        cards
    }

    fn from_web(value: &serde_json::Value) -> Result<Self, String> {
        let uid = value.get("uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        if uid.is_empty() {
            return Err("页面里没有主页数据（可能已被注销或需要登录）".into());
        }
        let mut cards = Vec::new();
        if let Some(arr) = value.get("notes").and_then(|v| v.as_array()) {
            for item in arr {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                cards.push(CardBundle {
                    id,
                    token: item.get("token").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    source: bundle_source(value),
                    title: item.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    author: item.get("author").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    is_video: item.get("type").and_then(|v| v.as_str()) == Some("video"),
                    created_at: item
                        .get("time")
                        .and_then(|v| v.as_i64())
                        .map(|ms| ms / 1000)
                        .unwrap_or(0),
                });
            }
        }
        Ok(Self {
            uid,
            nickname: value.get("nickname").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            avatar: value.get("avatar").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            notes_count: value.get("notesCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            cards,
        })
    }
}

/// 取一条笔记：先试 reqwest（Cookie 可用时更快），失败就借浏览器会话（可靠）。
async fn fetch_note(
    app: &tauri::AppHandle,
    _client: &XhsClient,
    url: &str,
    _retries: usize,
) -> Result<NoteBundle, String> {
    // 只走浏览器会话。**不要**用复制的 Cookie 让 reqwest 去请求：
    // 小红书风控会把"同一 session 来自另一个客户端"判成劫持，直接把会话作废——
    // 连带把浏览器里的登录态一起杀掉（实测：登录后 9 秒内会话就没了）。
    xhs_core::login::debug_log("笔记：走浏览器会话取数（不使用复制的 Cookie）");
    let (id, token) = split_note_url(url);
    xhs_core::login::debug_log(&format!(
        "笔记取数：id={id} token 长度={}",
        token.len()
    ));
    // 批量下载降速：两条笔记之间至少隔 3 秒。猛拉详情是最容易触发风控的动作，
    // 降速换来的时间远比"全批被挡、等五分钟冷却"划算。
    // 注意：锁必须在 await 之前放掉（MutexGuard 不能跨 await，否则 future 不是 Send）
    let pace_wait = {
        const NOTE_MIN_INTERVAL: Duration = Duration::from_secs(3);
        let state = app.state::<AppState>();
        let mut last = state.last_note_fetch.lock().unwrap();
        let wait = match *last {
            Some(at) => NOTE_MIN_INTERVAL.saturating_sub(at.elapsed()),
            None => Duration::ZERO,
        };
        *last = Some(std::time::Instant::now() + wait);
        wait
    };
    if !pace_wait.is_zero() {
        tokio::time::sleep(pace_wait).await;
    }

    let candidates = note_url_candidates(url);
    let mut last_err = String::from("笔记取数失败");
    // 最多两轮：被风控挡下时等 30 秒再试一次（短时限流常常自己就过去了）；
    // 仍被挡就记一笔熔断，交给闸门去停手
    for round in 0..2 {
        if round > 0 {
            xhs_core::login::debug_log("笔记取数被风控挡下：等 30 秒再试一次");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        let mut blocked = false;
        for (index, cand) in candidates.iter().enumerate() {
            match webview_fetch(app, cand).await {
                Ok(value) => match NoteBundle::from_web(&value) {
                    Ok(bundle) => {
                        note_block_mark(app, false);
                        return Ok(bundle);
                    }
                    Err(e) => last_err = e,
                },
                Err(e) => last_err = e,
            }
            // 风控挡下：换形态也没用（实测六种形态全被弹回），本轮到此为止
            if last_err.contains(NOTE_BLOCK_HINT) {
                blocked = true;
                break;
            }
            if index + 1 < candidates.len() {
                xhs_core::login::debug_log(&format!(
                    "笔记取数候选 {} 失败（{last_err}），换下一个地址形态",
                    index + 1
                ));
            }
        }
        if !blocked {
            // 不是风控（链接本身的问题）：没有必要等 30 秒再试一遍
            return Err(last_err);
        }
    }
    note_block_mark(app, true);
    Err(last_err)
}

/// 熔断闸门：连续两条笔记被风控挡住后，冷却期内不再逐条硬试
/// （逐条硬试的代价是拿会话去撞墙，站点只会把限制拉得更久）。
fn note_block_gate(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let guard = state.note_block.lock().unwrap();
    let (at, count) = *guard;
    if count >= 2 && at.elapsed() < Duration::from_secs(300) {
        let left = (300 - at.elapsed().as_secs()).max(1);
        return Err(format!(
            "{NOTE_BLOCK_HINT}：已暂停继续尝试，约 {left} 秒后再试；也可以先在网页窗口里手动打开一条笔记确认能正常浏览"
        ));
    }
    Ok(())
}

/// 记一笔熔断状态：被挡（blocked=true）累加次数，取数成功则清零
fn note_block_mark(app: &tauri::AppHandle, blocked: bool) {
    let state = app.state::<AppState>();
    let mut guard = state.note_block.lock().unwrap();
    if blocked {
        let count = if guard.0.elapsed() < Duration::from_secs(300) {
            guard.1 + 1
        } else {
            1
        };
        *guard = (std::time::Instant::now(), count);
    } else {
        *guard = (std::time::Instant::now() - Duration::from_secs(3600), 0);
    }
}

/// 从笔记链接里拆出 id 与 token（token 缺省为空串）。
fn split_note_url(url: &str) -> (String, String) {
    let head = url.split('?').next().unwrap_or(url);
    let id = head.rsplit('/').next().unwrap_or("").to_string();
    let mut token = String::new();
    if let Some((_, query)) = url.split_once('?') {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("xsec_token=") {
                token = value.to_string();
            }
        }
    }
    (id, token)
}

/// 站点把这台会话的"笔记详情"请求挡下时的提示前缀（批量下载的熔断器认它）
const NOTE_BLOCK_HINT: &str = "小红书短时限制了笔记详情请求（风控）";

/// 一条笔记的候选地址链：原样 → 移动版路径。
///
/// 实测结论（别再往下加形态）：同一 token 换 `xsec_source`、换 `/discovery/item/` 路径、
/// 甚至裸链接，在浏览器里都能正常打开；而 App 里被弹回首页时**六种形态无一幸免**——
/// 被弹是"会话被风控"这个层面的问题，不是链接形态问题。多试形态只会拿会话去撞墙，
/// 所以这里只留"原样"和一个移动版兜底。
fn note_url_candidates(url: &str) -> Vec<String> {
    let (id, token) = split_note_url(url);
    let mut out: Vec<String> = vec![url.to_string()];
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()) && !token.is_empty() {
        let variant = format!(
            "https://www.xiaohongshu.com/discovery/item/{id}?xsec_token={token}&xsec_source=pc_user"
        );
        if !out.iter().any(|existing| existing == &variant) {
            out.push(variant);
        }
    }
    out
}

/// 取一个用户主页：同样的两段式。
/// `page_url` 必须带上原始链接里的 token（搜索来源的主页没它会被拒）。
async fn fetch_user(
    app: &tauri::AppHandle,
    _client: &XhsClient,
    uid: &str,
    page_url: &str,
) -> Result<UserBundle, String> {
    // 同上：只走浏览器会话，绝不用复制的 Cookie 发请求（会被判劫持、作废会话）
    xhs_core::login::debug_log("主页：走浏览器会话取数（不使用复制的 Cookie）");
    let bare = format!("https://www.xiaohongshu.com/user/profile/{uid}");
    let mut candidates: Vec<String> = Vec::new();
    if !page_url.trim().is_empty() && page_url.contains("xsec_token=") {
        candidates.push(page_url.to_string());
    }
    candidates.push(bare.clone());
    if page_url.trim() != bare && !candidates.iter().any(|u| u == page_url) {
        candidates.push(page_url.to_string());
    }

    let mut last_err = String::from("主页取数失败");
    for (index, url) in candidates.iter().enumerate() {
        match webview_fetch(app, url).await {
            Ok(value) => {
                if let Ok(mut bundle) = UserBundle::from_web(&value) {
                    // 主页笔记是异步加载的：列表为空时原地多读几次（不导航），
                    // 免得"页面明明有笔记、程序却说 0 条"（实测踩过）
                    if bundle.cards.is_empty() {
                        if let Some(window) = app.get_webview_window("xhs-work") {
                            for _ in 0..4 {
                                tokio::time::sleep(Duration::from_millis(1500)).await;
                                let Some(retry_value) = extract_from_page(&window).await else {
                                    continue;
                                };
                                let Ok(retry) = UserBundle::from_web(&retry_value) else {
                                    continue;
                                };
                                if retry.cards.len() > bundle.cards.len() {
                                    bundle = retry;
                                    if !bundle.cards.is_empty() {
                                        break;
                                    }
                                }
                            }
                            xhs_core::login::debug_log(&format!(
                                "主页笔记：重读后 {} 条",
                                bundle.cards.len()
                            ));
                        }
                    }
                    return Ok(bundle);
                }
            }
            Err(e) => {
                last_err = e.clone();
                if index + 1 < candidates.len() {
                    xhs_core::login::debug_log(&format!("主页取数失败（{e}），换一种链接重试"));
                }
            }
        }
    }
    // 两种链接都失败：复核会话是否还有效，便于区分"会话失效"与"内容/链接问题"
    match webview_fetch(app, "https://www.xiaohongshu.com/user/profile/me").await {
        Ok(value) => {
            let name = value.get("nickname").and_then(|v| v.as_str()).unwrap_or_default();
            xhs_core::login::debug_log(&format!(
                "主页取数复核：会话仍有效（我的主页昵称={name:?}）——问题在目标链接/内容"
            ));
        }
        Err(e) => {
            xhs_core::login::debug_log(&format!("主页取数复核：会话可能已失效（{e}）"));
        }
    }
    Err(last_err)
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

/// 视频高度 → 档位标签
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

/// 档位标签 → 数值高度（排序用）
fn height_rank(label: &str) -> u64 {
    label.trim_end_matches('P').trim().parse().unwrap_or(0)
}

/// 二维码保活脚本最近一次上报（用于"只在变化时打日志"）
fn state_of_qr_log() -> &'static std::sync::Mutex<Option<String>> {
    static LAST: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();
    LAST.get_or_init(|| std::sync::Mutex::new(None))
}

/// 读取登录窗上的二维码状态上报并记日志（只在数值变化时记）。
fn log_qr_state(window: &tauri::WebviewWindow) {
    let Ok(url) = window.url() else { return };
    let text = url.to_string();
    let Some(idx) = text.find("xhsdownqr=") else { return };
    let rest = &text[idx + "xhsdownqr=".len()..];
    let state = rest.split('&').next().unwrap_or("").to_string();
    if state.is_empty() {
        return;
    }
    let mut last = state_of_qr_log().lock().unwrap();
    if last.as_deref() == Some(state.as_str()) {
        return;
    }
    let stage = match state.rsplit(',').next().unwrap_or("0") {
        "1" => "已过期",
        "2" => "等手机确认",
        "3" => "自动刷新已停止（页面反复报过期）",
        _ => "正常",
    };
    xhs_core::login::debug_log(&format!(
        "登录页二维码：点击={} 重载={} 状态={}",
        state.split(',').next().unwrap_or("?"),
        state.split(',').nth(1).unwrap_or("?"),
        stage
    ));
    *last = Some(state);
}

/// 后端登录轮询：定时检查登录页，检测到登录就完成绑定并向前端发事件。
///
/// 为什么放后端：界面窗口被登录窗遮挡时，WebView2 会把页面定时器降频到近乎停摆
/// （实测：前端轮询静默 80+ 秒），登录成功了界面也收不到通知。Rust 侧定时器不受影响。
async fn login_watch(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    let started = std::time::Instant::now();
    // 两个阶段：0 = 等登录页自己跳走；1 = 已进入"读资料"
    // （之前没有阶段概念，每 2.5 秒都重新导航一次"我的主页"：页面刚打开就被打断，
    //   异步资料永远加载不出来，还把站点惹到限速——实测页面报"未连接到服务器"）
    let mut entered = false;
    let mut last_full_fetch = std::time::Instant::now() - Duration::from_secs(600);
    let mut backoff = Duration::from_secs(5);

    loop {
        if started.elapsed() > Duration::from_secs(600) {
            xhs_core::login::debug_log("登录轮询：超过 10 分钟，停止");
            break;
        }
        tokio::time::sleep(Duration::from_millis(2500)).await;

        if !state.login_window_open.load(Ordering::SeqCst) {
            xhs_core::login::debug_log("登录轮询：登录窗已关闭，停止");
            break;
        }
        let Some(window) = app.get_webview_window("xhs-work") else {
            xhs_core::login::debug_log("登录轮询：窗口不存在，停止");
            break;
        };

        // 二维码保活：定时器不降频，不会停在过期态
        let _ = window.eval(QR_KEEPER_JS);
        log_qr_state(&window);

        // 阶段 0：等登录页离开 /login（登录成功的必然结果，边沿触发一次）
        if !entered {
            let page_url = window.url().map(|u| u.to_string()).unwrap_or_default();
            let left_login = page_url.contains("xiaohongshu.com") && !page_url.contains("/login");
            if !left_login {
                continue;
            }
            entered = true;
            // 登录已完成：先把窗口藏起来——接下来要读资料/可能导航到主页，
            // 用户不必看着登录窗被程序翻来翻去（读操作都在隐藏窗口里做）
            let _ = window.hide();
            xhs_core::login::debug_log("登录页已跳走，开始读取账号资料（窗口已隐藏）");
        }

        // 阶段 1：完整取数（含导航）最多每 20 秒一次；其余时间只读当前页面，
        // 让页面的异步资料有机会加载完
        let now = std::time::Instant::now();
        // 优先在当前页面读账号资料（登录者的资料就在全局状态里，不需要任何导航）
        let _ = window.eval(IDENTITY_DOM_JS);
        let mut value = match read_identity(&window).await {
            Some((uid, name, avatar)) if !name.is_empty() => Some(serde_json::json!({
                "kind": "user", "uid": uid, "nickname": name, "avatar": avatar,
            })),
            _ => None,
        };
        // 当前页面读不到（例如状态还没热）：退回到「我的主页」读，
        // 带退避（5s→10s→20s→40s），避免频繁打扰站点被限流
        if value.is_none() && now.duration_since(last_full_fetch) >= backoff {
            last_full_fetch = now;
            match webview_fetch_inner(&app, "https://www.xiaohongshu.com/user/profile/me", true)
                .await
            {
                Ok(v) => {
                    backoff = Duration::from_secs(5);
                    value = Some(v);
                }
                Err(e) => {
                    backoff = (backoff * 2).min(Duration::from_secs(40));
                    xhs_core::login::debug_log(&format!(
                        "读我的主页失败（{e}），{} 秒后重试",
                        backoff.as_secs()
                    ));
                }
            }
        }

        let Some(value) = value else { continue };
        let name = value
            .get("nickname")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            xhs_core::login::debug_log("读到主页数据但还没昵称，继续等待");
            continue;
        }
        let uid = value.get("uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let avatar = value
            .get("avatar")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        xhs_core::login::debug_log(&format!("检测到登录：昵称={:?} uid={:?}", name, uid));
        match finish_login(&app, &state, uid, name, avatar).await {
            Ok(login) => {
                let _ = app.emit("login://confirmed", login);
                break;
            }
            Err(e) => xhs_core::login::debug_log(&format!("登录收尾失败：{e}")),
        }
    }

    state.login_watch_running.store(false, Ordering::SeqCst);
}

#[tauri::command]
pub async fn web_login_watch(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.login_watch_running.swap(true, Ordering::SeqCst) {
        return Ok(()); // 已经在跑
    }
    let app2 = app.clone();
    tokio::spawn(async move { login_watch(app2).await });
    Ok(())
}

/// 页面快照（仅自检用）：标题、正文开头、图片、链接、状态字段全打出来。
const PAGE_SNAPSHOT_JS: &str = r#"(function () {
  var report = function (out) {
    try {
      history.replaceState(null, '', '#xhsdownselftest=' + encodeURIComponent(JSON.stringify(out)));
    } catch (e) {}
  };
  // 状态字段可能是 Vue ref，先解包
  var unwrap = function (v) {
    var d = 0;
    while (v && typeof v === 'object' && v.__v_isRef === true && d < 6) {
      v = v._value !== undefined ? v._value : v.value;
      d += 1;
    }
    return v;
  };
  var prim = function (v) {
    if (v === null || v === undefined) return null;
    if (typeof v === 'string') return v.slice(0, 60);
    if (typeof v === 'number' || typeof v === 'boolean') return v;
    if (Array.isArray(v)) return '[' + v.length + ' 项]';
    return '[' + Object.prototype.toString.call(v).slice(8, -1) + ']';
  };
  var fields = function (obj, limit) {
    var out = {};
    var o = unwrap(obj);
    if (!o || typeof o !== 'object') return out;
    var keys = Object.keys(o).slice(0, limit || 20);
    for (var i = 0; i < keys.length; i++) {
      try { out[keys[i]] = prim(unwrap(o[keys[i]])); } catch (e) { out[keys[i]] = '?'; }
    }
    return out;
  };
  var safe = function (v) {
    try { return JSON.stringify(unwrap(v)).slice(0, 300); } catch (e) { return '[circular]'; }
  };
  var arr = function (v) {
    v = unwrap(v);
    return Array.isArray(v) ? v : [];
  };
  try {
    var out = { url: location.href.split('#')[0], title: document.title };
    var st = window.__INITIAL_STATE__ || {};
    // 勘察：把可能装着"笔记卡片数组"的字段找出来（搜索页/信息流结构未知时用）
    var survey = {};
    var probe = function (label, obj) {
      var o = unwrap(obj);
      if (!o || typeof o !== 'object') {
        survey[label] = '[' + Object.prototype.toString.call(o).slice(8, -1) + ']';
        return;
      }
      var keys = Object.keys(o).slice(0, 18);
      var info = {};
      for (var i = 0; i < keys.length; i++) {
        var v = unwrap(o[keys[i]]);
        if (Array.isArray(v)) info[keys[i]] = '数组' + v.length;
        else if (v && typeof v === 'object') info[keys[i]] = '对象';
        else info[keys[i]] = String(v).slice(0, 20);
      }
      survey[label] = info;
    };
    probe('search', st.search);
    probe('feed', st.feed);
    probe('note', st.note);
    probe('global', st.global);
    out.survey = survey;
    var pu = unwrap(st.user) || {};
    out.userFields = fields(pu, 18);
    var notesArr = arr(unwrap(pu.notes));
    out.notesLen = notesArr.length;
    // 转储第一条笔记的真实结构（主页条目与信息流条目字段名可能不同）
    if (notesArr.length > 0) {
      var firstNote = unwrap(notesArr[0]) || {};
      out.firstNoteKeys = Object.keys(firstNote).slice(0, 20);
      var nk = {};
      var fkeys = Object.keys(firstNote).slice(0, 20);
      for (var ni = 0; ni < fkeys.length; ni++) {
        var nv = unwrap(firstNote[fkeys[ni]]);
        if (Array.isArray(nv)) nk[fkeys[ni]] = '数组' + nv.length;
        else if (nv && typeof nv === 'object') nk[fkeys[ni]] = '对象';
        else nk[fkeys[ni]] = String(nv).slice(0, 40);
      }
      out.firstNoteFields = nk;
      var inner = unwrap(firstNote.noteCard);
      if (inner && typeof inner === 'object') {
        out.noteCardFields = fields(inner, 16);
      }
    }
    out.noteQueriesType = Object.prototype.toString.call(unwrap(pu.noteQueries));
    out.noteQueriesKeys = Object.keys(unwrap(pu.noteQueries) || {}).slice(0, 6);
    out.noteQueriesSample = safe(pu.noteQueries);
    out.pageFields = fields(unwrap(pu.userPageData), 14);
    // DOM：笔记卡片链接（主页网格里是 /user/profile/{uid}/{noteId} 或 /explore/{noteId}）
    var links = document.querySelectorAll('a[href]');
    var seen = {};
    var noteLinks = [];
    for (var i = 0; i < links.length && noteLinks.length < 8; i++) {
      var href = links[i].getAttribute('href') || '';
      var m = href.match(/(?:\/explore\/|\/user\/profile\/[0-9a-f]{24}\/)([0-9a-f]{24})/);
      if (m && !seen[m[1]]) {
        seen[m[1]] = 1;
        noteLinks.push({
          id: m[1],
          href: href.slice(0, 60),
          text: (links[i].textContent || '').replace(/\s+/g, ' ').trim().slice(0, 36),
        });
      }
    }
    out.domNoteLinks = noteLinks;
    out.domNoteLinkCount = links.length;
    report(out);
  } catch (e) {
    report({ error: String(e) });
  }
})();"#;

/// 从探索页状态里抓一张新卡片的 id + token（笔记自检第一步用）
const NOTE_SELFTEST_CARD_JS: &str = r#"(function () {
  var report = function (t) {
    try { history.replaceState(null, '', '#xhsdownselftest=' + encodeURIComponent(t)); } catch (e) {}
  };
  try {
    var unwrap = function (v) {
      var d = 0;
      while (v && typeof v === 'object' && v.__v_isRef === true && d < 6) {
        v = v._value !== undefined ? v._value : v.value;
        d += 1;
      }
      return v;
    };
    var st = window.__INITIAL_STATE__ || {};
    var feeds = unwrap((unwrap(st.feed) || {}).feeds) || [];
    for (var i = 0; i < feeds.length; i++) {
      var o = unwrap(feeds[i]) || {};
      var id = String(o.id || '');
      var token = String(o.xsecToken || '');
      if (/^[0-9a-f]{24}$/.test(id) && token) {
        report('CARD~' + id + '~' + token);
        return;
      }
    }
    report('NOCARD');
  } catch (e) {
    report('ERR~' + e);
  }
})();"#;

/// 笔记自检：用本会话实测「笔记详情页是否被弹回首页」。
/// 由数据目录里的 note_selftest 标记文件触发（平时不跑），用来远程判断
/// 下载失败是不是会话被风控——先探后答，别让用户当测试员。
///
/// 标记文件为空：从探索页抓一张新卡片试开（会话级体检）。
/// 标记文件是一条笔记链接：把它按**来源/路径变体矩阵**逐一试开，最后再用
/// 探索页的新鲜 token 做对照——用来分辨「链接形态被拒」还是「会话被风控」。
async fn note_selftest(app: tauri::AppHandle, settings: Settings, custom_url: String) {
    // 等 40 秒：让启动时的登录检测先跑完（它也导航这个窗口，别互相抢）
    tokio::time::sleep(Duration::from_secs(40)).await;
    let window = match ensure_work_window(&app, &settings).await {
        Ok(w) => w,
        Err(e) => {
            xhs_core::login::debug_log(&format!("笔记自检：窗口不可用 {e}"));
            return;
        }
    };
    let _ = window.hide();

    // 通道对比实验：A=不动；C=location.replace 只写一次；D=history.replaceState 高频；
    // E=Cookie 高频。哪个稳住就用哪个通道回传数据。
    if custom_url == "injecttest" {
        let mut results: Vec<String> = Vec::new();

        for (label, channel) in [
            ("A(不注入)", Channel::None),
            ("C(replace只写1次)", Channel::ReplaceOnce),
            ("D(replaceState高频)", Channel::ReplaceState),
            ("E(Cookie高频)", Channel::Cookie),
        ] {
            let (id, tok) = fresh_feed_card(&window).await;
            if id.is_empty() {
                xhs_core::login::debug_log(&format!("笔记自检·{label}：没抓到卡片，跳过"));
                continue;
            }
            let url =
                format!("https://www.xiaohongshu.com/explore/{id}?xsec_token={tok}&xsec_source=pc_feed");
            let verdict = channel_leg(&window, &url, channel).await;
            xhs_core::login::debug_log(&format!("笔记自检·{label}：{verdict}"));
            results.push(format!("{label}={verdict}"));
        }
        let d_ok = results
            .iter()
            .any(|r| r.starts_with("D(") && r.contains("稳定"));
        let e_ok = results
            .iter()
            .any(|r| r.starts_with("E(") && r.contains("稳定"));
        let c_ok = results
            .iter()
            .any(|r| r.starts_with("C(") && r.contains("稳定"));
        let conclusion = if d_ok {
            "结论：改用 history.replaceState 回传（不发事件、不加历史记录）"
        } else if e_ok {
            "结论：改用 Cookie 回传"
        } else if c_ok {
            "结论：location.replace 只写一次可用——改成仅就绪时上报"
        } else {
            "结论：所有写通道都会触发弹走——需要走 ExecuteScript 直读（无页面改动）"
        };
        xhs_core::login::debug_log(&format!("笔记自检：{conclusion}"));
        return;
    }

    let (id, token) = if custom_url.is_empty() {
        (String::new(), String::new())
    } else {
        split_note_url(&custom_url)
    };

    if !id.is_empty() && !token.is_empty() {
        // ── 变体矩阵：同一个 id+token，逐个形态试 ──
        xhs_core::login::debug_log(&format!("笔记自检：变体矩阵开始（id={id}）"));
        let variants: Vec<(&str, String)> = vec![
            ("原样", custom_url.clone()),
            ("pc_user", format!("https://www.xiaohongshu.com/explore/{id}?xsec_token={token}&xsec_source=pc_user")),
            ("pc_search", format!("https://www.xiaohongshu.com/explore/{id}?xsec_token={token}&xsec_source=pc_search")),
            ("pc_feed", format!("https://www.xiaohongshu.com/explore/{id}?xsec_token={token}&xsec_source=pc_feed")),
            ("discovery/item", format!("https://www.xiaohongshu.com/discovery/item/{id}?xsec_token={token}&xsec_source=pc_user")),
            ("裸链接", format!("https://www.xiaohongshu.com/explore/{id}")),
        ];
        let mut any_ok = false;
        for (label, url) in variants {
            let verdict = probe_note_page(&window, &url).await;
            xhs_core::login::debug_log(&format!("笔记自检·{label}：{verdict}"));
            if verdict.contains("稳定") {
                any_ok = true;
                break;
            }
        }
        // ── 对照组：探索页的新鲜 token（昨天通过的形态）──
        let control = fresh_feed_note_probe(&window).await;
        xhs_core::login::debug_log(&format!(
            "笔记自检·对照(新鲜token)：{control}"
        ));
        let conclusion = if any_ok {
            "结论：链接形态问题——矩阵里有能用的形态，按它改下载链接的构建方式"
        } else if control.contains("稳定") {
            "结论：这类 token 整体被拒（对照的新鲜 token 能开）——token 与来源上下文绑定，下载前要先回原页面上下文"
        } else {
            "结论：会话当前开不了任何笔记页（对照也不行）——会话被风控中，等几分钟或退出重登"
        };
        xhs_core::login::debug_log(&format!("笔记自检：{conclusion}"));
        return;
    }

    // ── 会话级体检：新鲜卡片直接开 ──
    if !custom_url.is_empty() {
        let verdict = probe_note_page(&window, &custom_url).await;
        xhs_core::login::debug_log(&format!("笔记自检·指定链接：{verdict}"));
        return;
    }
    let control = fresh_feed_note_probe(&window).await;
    xhs_core::login::debug_log(&format!("笔记自检：结论 → {control}"));
}

/// 从探索页抓一张新卡片（只返回 id+token，不试开）
async fn fresh_feed_card(window: &tauri::WebviewWindow) -> (String, String) {
    if let Ok(parsed) = "https://www.xiaohongshu.com/explore".parse::<tauri::Url>() {
        let _ = window.navigate(parsed);
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = window.eval(NOTE_SELFTEST_CARD_JS);
    let card = read_selftest(window).await.unwrap_or_default();
    let mut parts = card.splitn(3, '~');
    let kind = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("").to_string();
    let token = parts.next().unwrap_or("").to_string();
    if kind != "CARD" {
        return (String::new(), String::new());
    }
    (id, token)
}

/// 从探索页抓一张新卡片并试开（会话级体检的标准动作）
async fn fresh_feed_note_probe(window: &tauri::WebviewWindow) -> String {
    let (id, token) = fresh_feed_card(window).await;
    if id.is_empty() {
        return "探索页没取到新卡片".into();
    }
    xhs_core::login::debug_log(&format!("笔记自检：对照卡片 id={id}"));
    let url = format!(
        "https://www.xiaohongshu.com/explore/{id}?xsec_token={token}&xsec_source=pc_feed"
    );
    probe_note_page(window, &url).await
}

/// 通道对比实验的写通道
#[derive(Clone, Copy, PartialEq)]
enum Channel {
    /// 只看不写（对照组）
    None,
    /// location.replace 只写一次（页面加载 2 秒后）
    ReplaceOnce,
    /// history.replaceState 每 700ms 写（带 2KB 载荷模拟真实数据量）
    ReplaceState,
    /// document.cookie 每 700ms 写（带 2KB 载荷）
    Cookie,
}

/// 通道对比实验的一条腿：导航到笔记页，按指定通道**写满整个观察窗**（模拟真实
/// 取数的持续注入），中途被弹走才算失败——只看 1.4 秒会得出假阳性。
async fn channel_leg(window: &tauri::WebviewWindow, url: &str, channel: Channel) -> String {
    if let Ok(parsed) = url.parse::<tauri::Url>() {
        let _ = window.navigate(parsed);
    }
    let note_prefix = "https://www.xiaohongshu.com/explore/".to_string();
    let mut writes = 0u32;
    for tick in 0..12 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let script = match channel {
            Channel::None => None,
            Channel::ReplaceOnce => {
                if tick != 3 {
                    None
                } else {
                    Some("location.replace('#xhsdownid='+Date.now()+'~'+encodeURIComponent(JSON.stringify({ready:false,filler:'x'.repeat(2048)})))".to_string())
                }
            }
            Channel::ReplaceState => Some(
                "try{history.replaceState(null,'','#xhsdownid='+Date.now()+'~'+encodeURIComponent(JSON.stringify({ready:false,filler:'x'.repeat(2048)})))}catch(e){}".to_string(),
            ),
            Channel::Cookie => Some(
                "try{document.cookie='xhsdown_t='+Date.now()+'~'+'x'.repeat(2048)+';path=/'}catch(e){}".to_string(),
            ),
        };
        if let Some(js) = script {
            let _ = window.eval(&js);
            writes += 1;
        }
        let live = window
            .url()
            .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
            .unwrap_or_default();
        if live.contains("/explore") && !live.contains("/explore/")
            || live.contains("/404")
            || live.contains("/login")
            || live.contains("/search_result")
        {
            return format!(
                "被弹走（写了 {writes} 次后；落地={}）",
                xhs_core::error::truncate(&live, 90)
            );
        }
    }
    format!("笔记页稳定打开，写满观察窗（写了 {writes} 次）没被弹")
}

/// 导航到一条笔记页并盯最多 8 秒：稳在笔记页 = 能开；被弹走 = 被拒。
/// 返回给日志看的结论文字。
async fn probe_note_page(window: &tauri::WebviewWindow, url: &str) -> String {
    if let Ok(parsed) = url.parse::<tauri::Url>() {
        let _ = window.navigate(parsed);
    }
    let note_prefix = "https://www.xiaohongshu.com/explore/".to_string();
    let discovery_prefix = "https://www.xiaohongshu.com/discovery/item/".to_string();
    let mut stable = 0u32;
    let mut landed = String::new();
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let live = window
            .url()
            .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
            .unwrap_or_default();
        if live.is_empty() {
            continue;
        }
        if (live.starts_with(&note_prefix) && live.len() > note_prefix.len() + 10)
            || (live.starts_with(&discovery_prefix)
                && live.len() > discovery_prefix.len() + 10)
        {
            landed = live;
            stable += 1;
            if stable >= 2 {
                return "笔记页稳定打开（能开）".into();
            }
        } else if live.contains("/explore")
            || live.contains("/404")
            || live.contains("/login")
            || live.contains("/search_result")
        {
            return format!("被弹走（落地={}）", xhs_core::error::truncate(&live, 90));
        }
    }
    if landed.is_empty() {
        "8 秒内没有任何落页（网络慢？）".into()
    } else {
        "没等到稳定也没被弹走".into()
    }
}

/// 登录页自检：隐藏窗口打开官方登录页，探测「二维码状态 + 保活脚本是否生效」并写日志。
/// 由数据目录里的 login_selftest 标记文件触发（平时不跑），用于远程诊断。
async fn login_selftest(app: tauri::AppHandle, settings: Settings, probe_url: String) {
    // 探针一：登录页（二维码状态）——共用唯一窗口
    let window = match ensure_work_window(&app, &settings).await {
        Ok(window) => window,
        Err(e) => {
            xhs_core::login::debug_log(&format!("自检：窗口不可用 {e}"));
            return;
        }
    };
    {
        if let Ok(parsed) = "https://www.xiaohongshu.com/login".parse::<tauri::Url>() {
            let _ = window.navigate(parsed);
        }
    }
    tokio::time::sleep(Duration::from_secs(10)).await;
    let _ = window.eval(LOGIN_SELFTEST_JS);
    xhs_core::login::debug_log(&format!(
        "自检·注入前：{}",
        read_selftest(&window).await.unwrap_or_else(|| "未取到".into())
    ));
    let _ = window.eval(QR_KEEPER_JS);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = window.eval(LOGIN_SELFTEST_JS);
    xhs_core::login::debug_log(&format!(
        "自检·注入后：{}",
        read_selftest(&window).await.unwrap_or_else(|| "未取到".into())
    ));
    let _ = window.close();

    // 探针三：转储资料可能藏身的位置（主页 + 推荐流页头）
    for url in [
        "https://www.xiaohongshu.com/user/profile/me",
        "https://www.xiaohongshu.com/user/profile/58c8014a6a6a690dbb0c84c1",
    ] {
        if let Ok(window) = ensure_work_window(&app, &settings).await {
            if let Ok(parsed) = url.parse::<tauri::Url>() {
                let _ = window.navigate(parsed);
            }
            // 页面加载 + hydration 需要时间：反复注入直到拿到结果
            let mut dump = None;
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = window.eval(PAGE_SNAPSHOT_JS);
                if let Some(text) = read_selftest(&window).await {
                    dump = Some(text);
                    break;
                }
            }
            xhs_core::login::debug_log(&format!(
                "自检·转储[{url}]：{}",
                dump.unwrap_or_else(|| "未取到".into())
            ));
        }
    }

    // 探针五：主页"笔记为空"的归因对比（带 token / 不带 token / 页面快照）
    if !probe_url.is_empty() && probe_url.contains("/user/profile/") {
        let plain = probe_url.split('?').next().unwrap_or("").to_string();
        for (label, url) in [("带token", probe_url.clone()), ("不带token", plain.clone())] {
            match webview_fetch(&app, &url).await {
                Ok(value) => {
                    let name = value.get("nickname").and_then(|v| v.as_str()).unwrap_or("");
                    let notes = value.get("notes").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                    xhs_core::login::debug_log(&format!(
                        "自检·主页[{label}]：昵称={name:?} 笔记={notes} 条"
                    ));
                }
                Err(e) => {
                    xhs_core::login::debug_log(&format!("自检·主页[{label}]：失败 {e}"));
                }
            }
            // 再取一次页面快照，看到底是"没登录"还是"页面没给数据"
            if let Ok(window) = ensure_work_window(&app, &settings).await {
                tokio::time::sleep(Duration::from_secs(6)).await;
                let _ = window.eval(PAGE_SNAPSHOT_JS);
                if let Some(text) = read_selftest(&window).await {
                    xhs_core::login::debug_log(&format!("自检·结构[{label}]：{text}"));
                }
            }
        }
    }

    // 探针四：指定链接（标记文件里给的那条），走真实解析路径
    if !probe_url.is_empty() {
        let client = app.state::<AppState>().client();
        match parse_input(&probe_url) {
            Ok(SourceTarget::User { uid, token, url }) => {
                let page_url = user_fetch_url(&uid, &token, &url);
                let _ = (&client, &uid);
                xhs_core::login::debug_log(&format!(
                    "自检·主页链接：uid={uid} 带token={} 取数地址={page_url}",
                    token.is_some()
                ));
                // 先解析首屏，再连跑几轮"继续解析"：验证滚动加载能把整列拉出来
                match probe_source(app.clone(), app.state::<AppState>(), url.clone()).await {
                    Ok(first) => {
                        let mut total = first.items.len();
                        xhs_core::login::debug_log(&format!("自检·主页首屏：{total} 条"));
                        for round in 1..=8 {
                            match probe_more(
                                app.clone(),
                                app.state::<AppState>(),
                                url.clone(),
                                20,
                            )
                            .await
                            {
                                Ok(more) => {
                                    total += more.items.len();
                                    xhs_core::login::debug_log(&format!(
                                        "自检·主页滚动：第 {round} 轮 新增 {} 累计 {} {}",
                                        more.items.len(),
                                        total,
                                        if more.exhausted { "（已到底）" } else { "" }
                                    ));
                                    if more.exhausted {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    xhs_core::login::debug_log(&format!(
                                        "自检·主页滚动：第 {round} 轮失败 {e}"
                                    ));
                                    break;
                                }
                            }
                        }
                        // 滚完之后把页面提示文字打出来：区分"被登录墙挡住"与"确实到底"
                        if let Ok(window) = ensure_work_window(&app, &settings).await {
                            // 不用正则，避免 Rust 字符串里的反斜杠转义问题
                            let _ = window.eval(
                                "(function(){var t=document.body.innerText||'';t=t.split(String.fromCharCode(10)).join(' ').split('  ').join(' ');history.replaceState(null,'','#xhsdownselftest='+encodeURIComponent(t.slice(0,400)));})()",
                            );
                            if let Some(text) = read_selftest(&window).await {
                                xhs_core::login::debug_log(&format!("自检·页尾提示：{text}"));
                            }
                        }
                    }
                    Err(e) => xhs_core::login::debug_log(&format!("自检·主页失败：{e}")),
                }
            }
            Ok(SourceTarget::Search { url }) => {
                xhs_core::login::debug_log(&format!("自检·搜索链接：{url}"));
                match probe_source(app.clone(), app.state::<AppState>(), url).await {
                    Ok(probe) => xhs_core::login::debug_log(&format!(
                        "自检·搜索结果：{} 条",
                        probe.items.len()
                    )),
                    Err(e) => xhs_core::login::debug_log(&format!("自检·搜索失败：{e}")),
                }
            }
            Ok(SourceTarget::Feed) => {
                xhs_core::login::debug_log("自检·推荐流链接：走推荐流解析");
                let client = app.state::<AppState>().client();
                let _ = client;
                match probe_source(
                    app.clone(),
                    app.state::<AppState>(),
                    "https://www.xiaohongshu.com/explore".to_string(),
                )
                .await
                {
                    Ok(probe) => xhs_core::login::debug_log(&format!(
                        "自检·推荐流结果：{} 条",
                        probe.items.len()
                    )),
                    Err(e) => xhs_core::login::debug_log(&format!("自检·推荐流失败：{e}")),
                }
            }
            Ok(SourceTarget::Note { id, token, url }) => {
                let fetch_url = note_fetch_url(&id, &token, &url);
                xhs_core::login::debug_log(&format!("自检·笔记链接：{id} 带token={}", token.is_some()));
                match fetch_note(&app, &client, &fetch_url, 1).await {
                    Ok(note) => xhs_core::login::debug_log(&format!(
                        "自检·笔记结果：标题={:?} 图={} 视频档={:?}",
                        note.title,
                        note.pics.len(),
                        note.videos.iter().map(|(l, _)| l.clone()).collect::<Vec<_>>()
                    )),
                    Err(e) => xhs_core::login::debug_log(&format!("自检·笔记失败：{e}")),
                }
            }
            Err(e) => xhs_core::login::debug_log(&format!("自检·链接无法识别：{e}")),
        }
    }

    // 探针二：/user/profile/me —— 登录判定的权威来源，把原始数据打出来
    match webview_fetch(&app, "https://www.xiaohongshu.com/user/profile/me").await {
        Ok(value) => {
            let summary = serde_json::json!({
                "kind": value.get("kind"),
                "uid": value.get("uid"),
                "nickname": value.get("nickname"),
                "notesCount": value.get("notesCount"),
                "pageUrl": value.get("pageUrl"),
                "notes": value.get("notes").and_then(|v| v.as_array()).map(|a| a.len()),
            });
            xhs_core::login::debug_log(&format!("自检·我的主页：{summary}"));
        }
        Err(e) => {
            xhs_core::login::debug_log(&format!("自检·我的主页：读取失败 {e}"));
        }
    }
}

/// 读取自检脚本写进 URL 片段的结果。
async fn read_selftest(window: &tauri::WebviewWindow) -> Option<String> {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(url) = window.url() {
            let text = url.to_string();
            if let Some(idx) = text.find("xhsdownselftest=") {
                let rest = &text[idx + "xhsdownselftest=".len()..];
                return Some(percent_decode(rest.split('&').next().unwrap_or("")));
            }
        }
    }
    None
}

/// 百分号解码（只处理 %XX，其余原样保留）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(hex) = input.get(i + 1..i + 3) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 二维码保活脚本：小红书登录页的二维码过期后会出现「点击刷新」。
/// 这里替用户点掉；点不动（8 秒仍是过期态）就直接重载登录页拿新码。
/// 每轮把「点击次数,重载次数,是否过期」写进 URL 片段，Rust 侧读出来记日志。
const QR_KEEPER_JS: &str = r#"(function () {
  if (window.__xhsdownQrKeeper) return;
  window.__xhsdownQrKeeper = true;
  window.__xhsdownQrClicks = 0;
  window.__xhsdownQrReloads = 0;
  window.__xhsdownQrLastClick = 0;
  window.__xhsdownQrLastReport = '';
  window.__xhsdownQrExpiredSeen = 0;
  var MAX_CLICKS = 5;
  var report = function (state) {
    // 只在状态变化时才写 URL：每 2.5 秒改一次地址栏会打扰页面自身逻辑
    if (state === window.__xhsdownQrLastReport) return;
    window.__xhsdownQrLastReport = state;
    try { history.replaceState(null, '', '#xhsdownqr=' + state); } catch (e) {}
  };
  var tick = function () {
    var text = document.body.innerText || '';
    var expired = text.indexOf('二维码已过期') >= 0;
    // 正在扫码/等待手机确认时**绝不动**：这时换码会让手机确认的是旧码，
    // 页面会报 "fail to login"（实测踩过）
    var scanning =
      text.indexOf('扫描成功') >= 0 ||
      text.indexOf('请在手机上确认') >= 0 ||
      text.indexOf('确认登录') >= 0 ||
      text.indexOf('已扫描') >= 0;
    if (scanning) {
      window.__xhsdownQrExpiredSeen = 0;
      report(window.__xhsdownQrClicks + ',' + window.__xhsdownQrReloads + ',2');
      return;
    }

    // 过期判定要"连续两次成立"：页面偶发地在过期/正常之间横跳，
    // 只认一次就会把二维码刷个不停（实测踩过）
    window.__xhsdownQrExpiredSeen = expired ? window.__xhsdownQrExpiredSeen + 1 : 0;
    var stable = window.__xhsdownQrExpiredSeen >= 2;

    if (!stable) {
      report(window.__xhsdownQrClicks + ',' + window.__xhsdownQrReloads + ',0');
      return;
    }
    // 点击有冷却，且有次数上限：反复横跳时停下来，别把用户晾在"刷码"里
    if (window.__xhsdownQrClicks >= MAX_CLICKS) {
      report(window.__xhsdownQrClicks + ',' + window.__xhsdownQrReloads + ',3');
      return;
    }
    if (Date.now() - window.__xhsdownQrLastClick < 15000) {
      report(window.__xhsdownQrClicks + ',' + window.__xhsdownQrReloads + ',1');
      return;
    }

    var nodes = document.querySelectorAll('a, button, span, div, p');
    var leaf = null;
    var first = null;
    for (var i = 0; i < nodes.length; i++) {
      var el = nodes[i];
      var t = (el.textContent || '').replace(/\s+/g, '');
      if (t === '点击刷新' || t === '刷新二维码') {
        if (!first) first = el;
        // 优先点最内层元素：外层容器点了不生效（事件挂在叶子节点上）
        if (!leaf && el.children.length === 0) leaf = el;
      }
    }
    var target = leaf || first;
    if (target) {
      target.click();
      window.__xhsdownQrClicks += 1;
      window.__xhsdownQrLastClick = Date.now();
    }
    report(window.__xhsdownQrClicks + ',' + window.__xhsdownQrReloads + ',1');
  };
  window.__xhsdownQrTick = tick;
  setInterval(function () {
    try { tick(); } catch (e) {}
  }, 2500);
  // 窗口被切走时系统会降频：回到前台/可见时立刻补一次检查
  window.addEventListener('focus', function () { try { tick(); } catch (e) {} });
  document.addEventListener('visibilitychange', function () {
    if (!document.hidden) { try { tick(); } catch (e) {} }
  });
})();"#;

/// 自检脚本：把登录页的真实状态（有没有二维码、是否过期、刷新元素、手机号输入框、
/// 保活脚本是否装上）写进 URL 片段，供 Rust 侧记日志。
const LOGIN_SELFTEST_JS: &str = r#"(function () {
  try {
    var text = document.body.innerText || '';
    var imgs = document.querySelectorAll('img');
    var qr = 0;
    for (var i = 0; i < imgs.length; i++) {
      if ((imgs[i].src || '').indexOf('data:image') === 0) qr += 1;
    }
    var refresh = 0;
    var nodes = document.querySelectorAll('a, button, span, div, p');
    for (var j = 0; j < nodes.length; j++) {
      var t = (nodes[j].textContent || '').replace(/\s+/g, '');
      if (t === '点击刷新' || t === '刷新二维码') { refresh = 1; break; }
    }
    var phone = document.querySelectorAll('input[type="tel"], input[placeholder*="手机号"]').length;
    var out = {
      url: location.href.slice(0, 120),
      qrImgs: qr,
      expired: text.indexOf('二维码已过期') >= 0,
      refreshBtn: refresh,
      phoneInputs: phone,
      keeper: !!window.__xhsdownQrKeeper,
      clicks: window.__xhsdownQrClicks || 0,
      reloads: window.__xhsdownQrReloads || 0
    };
    history.replaceState(null, '', '#xhsdownselftest=' + encodeURIComponent(JSON.stringify(out)));
  } catch (e) {
    history.replaceState(null, '', '#xhsdownselftest=' + encodeURIComponent(JSON.stringify({ error: String(e) })));
  }
})();"#;

/// 浏览器 Cookie 指纹：只看「名字=值」的集合，用来判断登录状态有没有变化。
fn cookie_fingerprint(window: &tauri::WebviewWindow) -> String {
    let mut parts: Vec<String> = harvest_webview_cookies(window)
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    parts.sort();
    parts.join(";")
}

/// 从登录小窗收割 Cookie（HttpOnly 也能拿到）。
/// 只是备份一份给 reqwest 快路径用——登录态本身靠浏览器数据目录共享。
fn harvest_webview_cookies(window: &tauri::WebviewWindow) -> Vec<(String, String)> {
    let mut cookies: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for origin in xhs_core::client::COOKIE_ORIGINS {
        let Ok(url) = tauri::Url::parse(origin) else { continue };
        let list = match window.cookies_for_url(url) {
            Ok(list) => list,
            Err(e) => {
                xhs_core::login::debug_log(&format!("cookies_for_url({origin}) 失败：{e}"));
                continue;
            }
        };
        for c in list {
            let pair = (c.name().to_string(), c.value().to_string());
            if seen.insert(pair.0.clone()) {
                cookies.push(pair);
            }
        }
    }
    xhs_core::login::debug_log(&format!("浏览器 Cookie 备份：{} 个", cookies.len()));
    cookies
}

/// 登录成功后收尾：备份 Cookie、内嵌头像、写登录态、关登录小窗。
async fn finish_login(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    uid: String,
    name: String,
    avatar: String,
) -> Result<LoginInfo, String> {
    let settings = state.settings_snapshot();
    if let Some(window) = app.get_webview_window("xhs-work") {
        // 只记录一份备份（诊断用），**不装到 reqwest 客户端**：
        // 用复制的 Cookie 发请求会被风控判成会话劫持，进而作废登录态（实测踩过）
        let cookies = harvest_webview_cookies(&window);
        xhs_core::login::debug_log(&format!(
            "登录凭据：浏览器侧 {} 个 Cookie（不复制给 reqwest 使用）",
            cookies.len()
        ));
        let _ = &settings;
    }
    let client = state.client();
    let face = inline_avatar(&client, &avatar).await;
    let login = LoginInfo {
        logged_in: true,
        uname: name,
        face,
        mid: uid,
        ..Default::default()
    };
    *state.login.write().unwrap() = Some(login.clone());
    xhs_core::login::debug_log(&format!(
        "登录完成：昵称={:?} uid={:?}",
        login.uname, login.mid
    ));
    if let Some(window) = app.get_webview_window("xhs-work") {
        let _ = window.hide();
    }
    state.login_window_open.store(false, Ordering::SeqCst);
    Ok(login)
}

#[tauri::command]
pub async fn web_login_open(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let settings = state.settings_snapshot();
    // 与取数共用一个窗口：登录态天然共享（分两个窗口会出现跨环境会话丢失，实测踩过）
    let window = ensure_work_window(&app, &settings).await?;
    let url: tauri::Url = "https://www.xiaohongshu.com/login".parse().unwrap();
    window.navigate(url).map_err(err)?;
    let _ = window.set_title("小红书登录");
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(520.0, 720.0)));
    let _ = window.center();
    let _ = window.show();
    let _ = window.set_focus();
    let _ = window.eval(QR_KEEPER_JS);
    state.login_window_open.store(true, Ordering::SeqCst);
    Ok(())
}

/// 注入到网页里的浮动地址栏（后退/前进/刷新/地址/复制/解析/收起）。
/// 为什么用注入而不是窗口内嵌工具条：Tauri 的 add_child 需要 unstable 特性，
/// 而注入只影响页面本身，不引入第二个浏览器环境（多环境会丢登录态，实测踩过）。
/// 幂等：重复注入只在页面里补一条，不会叠出多条。
const PAGE_BAR_JS: &str = r#"(function () {
  var Z = '2147483000';
  // 站内点击常走"新窗口"（window.open / target=_blank）：浏览器里弹新标签页，
  // 我们这里没有新窗口，请求会被丢弃——点了看起来没反应。
  // 统一改成就地跳转（与浏览器点击体验一致），只做一次。
  if (!window.__xhsdownOpenPatched) {
    window.__xhsdownOpenPatched = true;
    try {
      window.open = function (url) {
        try {
          if (url) location.href = String(url);
        } catch (e) {}
        return null;
      };
    } catch (e) {}
    document.addEventListener(
      'click',
      function (ev) {
        try {
          var el = ev.target;
          while (el && el !== document.documentElement) {
            if (el.tagName === 'A') break;
            el = el.parentElement;
          }
          if (!el || el.tagName !== 'A') return;
          var href = el.getAttribute('href') || '';
          if (!href || href.charAt(0) === '#') return;
          if (el.getAttribute('target') === '_blank') {
            ev.preventDefault();
            ev.stopPropagation();
            location.href = el.href;
          }
        } catch (e) {}
      },
      true
    );
  }
  // 展开状态**只从元素自己身上实时读**，绝不留闭包快照——
  // 脚本可能被后端多次注入，每份闭包各带一份旧状态，会互相把地址栏拉来拉去
  // （表现为"反复上下跳、点不到"，实测踩过）
  var isOpen = function () {
    var bar = document.getElementById('xhsdown-bar');
    if (bar && bar.dataset) return bar.dataset.pinned === '1';
    var handle = document.getElementById('xhsdown-handle');
    if (handle && handle.dataset) return handle.dataset.pinned === '1';
    return false;
  };
  var markPinned = function (on) {
    var bar = document.getElementById('xhsdown-bar');
    var handle = document.getElementById('xhsdown-handle');
    if (bar && bar.dataset) bar.dataset.pinned = on ? '1' : '0';
    if (handle && handle.dataset) handle.dataset.pinned = on ? '1' : '0';
  };
  var apply = function () {
    var bar = document.getElementById('xhsdown-bar');
    if (!bar) return;
    var want = isOpen() ? 'translateY(0)' : 'translateY(-102%)';
    if (bar.style.transform !== want && bar.style.transform !== want.replace('0)', '0px)')) {
      bar.style.transform = want;
    }
  };
  var setOpen = function (next) {
    markPinned(next);
    apply();
  };

  var ensure = function () {
    var handle = document.getElementById('xhsdown-handle');
    var bar = document.getElementById('xhsdown-bar');
    if (!handle) {
      handle = document.createElement('div');
      handle.id = 'xhsdown-handle';
      handle.dataset.pinned = '0';
      handle.textContent = '⌃';
      handle.title = '地址栏（移上去展开，展开后不会自动收起）';
      handle.style.cssText = 'position:fixed;left:8px;bottom:10px;width:20px;height:20px;z-index:' + Z + ';display:flex;align-items:center;justify-content:center;background:#ff2442;color:#fff;border-radius:6px;font:11px/1 sans-serif;cursor:pointer;opacity:.4;transition:opacity .15s;box-shadow:0 2px 8px rgba(0,0,0,.28);';
      handle.onmouseenter = function () {
        handle.style.opacity = '1';
        setOpen(true);
      };
      handle.onmouseleave = function () { handle.style.opacity = '.4'; };
      handle.onclick = function () { setOpen(true); };
      (document.documentElement || document.body).appendChild(handle);
    }
    if (!bar) {
      var startOpen = isOpen();
      bar = document.createElement('div');
      bar.id = 'xhsdown-bar';
      bar.dataset.pinned = startOpen ? '1' : '0';
      // 创建时就带上当前状态的位置（否则会先闪现在顶部再滑上去），过渡推迟到下一帧
      bar.style.cssText = 'position:fixed;left:0;right:0;top:0;height:34px;z-index:' + Z + ';display:flex;align-items:center;gap:6px;padding:0 10px;background:#17181a;color:#e8e9ea;border-bottom:1px solid #282a2d;font:12px/1 "Microsoft YaHei UI","Microsoft YaHei",system-ui,sans-serif;box-sizing:border-box;transform:' + (startOpen ? 'translateY(0)' : 'translateY(-102%)') + ';';
      var btn = function (label, title, fn) {
        var b = document.createElement('button');
        b.textContent = label;
        b.title = title;
        b.style.cssText = 'height:22px;min-width:26px;padding:0 8px;color:#a2a6ab;background:#1e2022;border:1px solid #282a2d;border-radius:6px;cursor:pointer;font:inherit;';
        b.onclick = function (e) { e.stopPropagation(); e.preventDefault(); fn(); };
        bar.appendChild(b);
      };
      btn('◀', '后退', function () { history.back(); });
      btn('▶', '前进', function () { history.forward(); });
      btn('⟳', '刷新', function () { location.reload(); });
      var input = document.createElement('input');
      input.id = 'xhsdown-url';
      input.spellcheck = false;
      input.style.cssText = 'flex:1;min-width:0;height:22px;padding:0 8px;color:#e8e9ea;background:#131416;border:1px solid #282a2d;border-radius:6px;font:inherit;';
      input.value = location.href.split('#')[0];
      input.addEventListener('keydown', function (e) {
        e.stopPropagation();
        if (e.key === 'Enter') {
          var v = input.value.trim();
          if (v) location.href = v;
        }
      });
      bar.appendChild(input);
      btn('复制', '复制当前地址', function () {
        var url = location.href.split('#')[0];
        input.value = url;
        input.select();
        try { document.execCommand('copy'); } catch (e) {}
        try { navigator.clipboard.writeText(url); } catch (e) {}
        input.blur();
      });
      btn('开始解析', '在本页滚动收集笔记（数字会实时增长）', function () {
        history.replaceState(null, '', '#xhsdowncollect=start');
      });
      btn('完成解析', '停止收集并把整份列表送进软件', function () {
        history.replaceState(null, '', '#xhsdowncollect=stop');
      });
      var count = document.createElement('span');
      count.id = 'xhsdown-count';
      count.style.cssText = 'flex:none;font:11px/1 sans-serif;color:#a2a6ab;min-width:64px;text-align:right;';
      count.textContent = '';
      bar.appendChild(count);
      btn('收起', '收起地址栏（收回到左下角小方块）', function () { setOpen(false); });
      (document.documentElement || document.body).appendChild(bar);
      requestAnimationFrame(function () {
        try { bar.style.transition = 'transform .16s ease'; } catch (e) {}
      });
    }
    apply();
  };

  ensure();

  // ref 解包 / 数组归一：计数与总数两处共用（原先是嵌在 countCards 里的）
  var unwrapAll = function (v) {
    var d = 0;
    while (v && typeof v === 'object' && v.__v_isRef === true && d < 6) {
      v = v._value !== undefined ? v._value : v.value;
      d += 1;
    }
    return v;
  };
  var arrOf = function (v) {
    v = unwrapAll(v);
    return Array.isArray(v) ? v : [];
  };
  var countCards = function () {
    try {
      var st = window.__INITIAL_STATE__ || {};
      var flattenCount = function (value) {
        var n = 0;
        var walk = function (node, depth) {
          if (depth > 4) return;
          var list = arrOf(node);
          for (var i = 0; i < list.length; i++) {
            var item = unwrapAll(list[i]);
            if (Array.isArray(item)) { walk(item, depth + 1); continue; }
            if (!item || typeof item !== 'object') continue;
            var keys = Object.keys(item);
            var numericOnly = keys.length > 0 && keys.every(function (k) { return /^[0-9]+$/.test(k); });
            if (numericOnly && !item.id && !item.noteId && !item.noteCard) {
              walk(keys.map(function (k) { return item[k]; }), depth + 1);
              continue;
            }
            n += 1;
          }
        };
        walk(value, 0);
        return n;
      };
      var here = location.href;
      var u = unwrapAll(st.user) || {};
      if (here.indexOf('/user/profile/') >= 0) return flattenCount(u.notes);
      var search = unwrapAll(st.search) || {};
      if (here.indexOf('/search_result') >= 0) return arrOf(search.feeds).length;
      var feed = unwrapAll(st.feed) || {};
      return arrOf(feed.feeds).length;
    } catch (e) {
      return 0;
    }
  };

  // 主页自己写着"共多少条笔记"：一并显示，一眼看出是"到底了"还是"被站点挡住了"
  var totalCards = function () {
    try {
      if (location.href.indexOf('/user/profile/') < 0) return 0;
      var st = window.__INITIAL_STATE__ || {};
      var u = unwrapAll(st.user) || {};
      var num = function (v) {
        v = unwrapAll(v);
        if (typeof v === 'number' && isFinite(v)) return Math.floor(v);
        if (typeof v === 'string' && /^[0-9]+$/.test(v)) return parseInt(v, 10);
        return 0;
      };
      var page = unwrapAll(u.userPageData) || {};
      var info = unwrapAll(u.userInfo) || {};
      // 实测：总数在 userPageData.posted（纯数字，=该账号已发布笔记数）；
      // notes/noteCount 是列表字段（可能是数组），只能当兜底
      return num(page.posted) || num(page.notes) || num(page.noteCount) || num(info.notes) || num(info.noteCount);
    } catch (e) {
      return 0;
    }
  };

  // 全局只留一个定时器：脚本被反复注入时不会叠加出一堆互斗的定时器
  if (!window.__xhsdownBarTimer) {
    window.__xhsdownBarTimer = setInterval(function () {
      try {
        ensure();
        var input = document.getElementById('xhsdown-url');
        if (input && document.activeElement !== input) {
          input.value = location.href.split('#')[0];
        }
        var count = document.getElementById('xhsdown-count');
        if (count) {
          var n = countCards();
          var total = totalCards();
          if (n <= 0) count.textContent = '';
          else if (total > n) count.textContent = '已加载 ' + n + ' / 共 ' + total + ' 条';
          else if (total > 0) count.textContent = '已收全 ' + n + ' 条';
          else count.textContent = '已加载 ' + n + ' 条';
        }
      } catch (e) {}
    }, 800);
  }
})();
"#;

/// 网页显示期间：注入地址栏、处理页面里的按钮信号、按需滚动收集。
/// 页面里的「开始解析 / 完成解析」通过 URL 片段发信号过来（远程页面没有 IPC 通道）。
fn spawn_page_bar(app: tauri::AppHandle) {
    tokio::spawn(async move {
        let mut last_url = String::new();
        let mut last_inject = std::time::Instant::now() - Duration::from_secs(600);
        let mut collecting = false;
        let mut collected: Vec<PostEntry> = Vec::new();
        // 去重集合与 collected 同步维护：几千条时逐条 any() 扫描是 O(n²)，滚久了会卡
        let mut seen_bids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut stale: u32 = 0;

        let clear_hash = |window: &tauri::WebviewWindow| {
            let _ = window.eval("history.replaceState(null,'',location.pathname+location.search)");
        };
        // 从解析栏读数里抠出主页总数：「已收全 625 条」或「已加载 320 / 共 625 条」
        let parse_total = |text: &str| -> u32 {
            let pick = |after: &str| -> u32 {
                text.split(after)
                    .nth(1)
                    .and_then(|rest| rest.split(' ').next())
                    .and_then(|num| num.parse::<u32>().ok())
                    .unwrap_or(0)
            };
            let n = pick("已收全 ");
            if n > 0 {
                return n;
            }
            pick("共 ")
        };
        let emit_done = |app: &tauri::AppHandle, collected: &[PostEntry], auto: bool, total: u32| {
            if collected.is_empty() {
                xhs_core::login::debug_log("网页收集：没有收集到内容");
                return;
            }
            // 有总数就说得明白：到底收够了没有（用户看到"停在 N 条"时最需要这个答案）
            let hint = if total == 0 {
                String::new()
            } else if total as usize <= collected.len() {
                "（已收全）".to_string()
            } else {
                format!(
                    "（主页共 {total} 条，还差 {} 条；未登录时小红书会拦住后续笔记，登录后再收集）",
                    total as usize - collected.len()
                )
            };
            xhs_core::login::debug_log(&format!(
                "网页收集：完成，共 {} 条{auto_hint}{hint}",
                collected.len(),
                auto_hint = if auto { "（自动判定到底）" } else { "" }
            ));
            let _ = app.emit(
                "collect://done",
                serde_json::json!({ "items": collected, "auto": auto, "total": total }),
            );
        };

        loop {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let Some(window) = app.get_webview_window("xhs-work") else {
                break;
            };
            if !window.is_visible().unwrap_or(false) {
                if collecting {
                    emit_done(&app, &collected, false, 0);
                    collecting = false;
                }
                break;
            }

            let url = window
                .url()
                .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
                .unwrap_or_default();
            if url != last_url || last_inject.elapsed() > Duration::from_secs(5) {
                last_url = url.clone();
                last_inject = std::time::Instant::now();
                let _ = window.eval(PAGE_BAR_JS);
            }

            // 页面按钮信号
            if let Ok(current) = window.url() {
                let text = current.to_string();
                if let Some(idx) = text.find("xhsdowncollect=") {
                    let rest = &text[idx + "xhsdowncollect=".len()..];
                    let value = rest.split('&').next().unwrap_or("").to_string();
                    clear_hash(&window);
                    if value == "start" {
                        collecting = true;
                        collected.clear();
                        seen_bids.clear();
                        stale = 0;
                        // 抽取脚本只回传"上次之后的增量"，重新开始收集必须清零标记，
                        // 否则同一页面上第二次收集只会收到增量（基本是空的）
                        let _ = window.eval(
                            "window.__xhsdownUserN=0;window.__xhsdownFeedN=0;window.__xhsdownSearchN=0;",
                        );
                        xhs_core::login::debug_log("网页收集：开始（可见模式，边滚边收集）");
                        let _ = app.emit("collect://update", serde_json::json!({ "count": 0 }));
                    } else if value == "stop" {
                        collecting = false;
                        // 手动点「完成解析」：顺手把页面读一遍拿总数，好告诉用户收够了没有
                        let total = {
                            let _ = window.eval(
                                "(function(){var c=document.getElementById('xhsdown-count');history.replaceState(null,'','#xhsdownselftest='+encodeURIComponent(c?c.textContent:''));})()",
                            );
                            read_selftest(&window)
                                .await
                                .map(|text| parse_total(&text))
                                .unwrap_or(0)
                        };
                        emit_done(&app, &collected, false, total);
                    }
                } else if let Some(idx) = text.find("xhsdownparse=") {
                    let rest = &text[idx + "xhsdownparse=".len()..];
                    let payload = rest.split('&').next().unwrap_or("");
                    let target = percent_decode(payload);
                    clear_hash(&window);
                    if !target.is_empty() {
                        xhs_core::login::debug_log(&format!("网页地址栏送解析：{target}"));
                        let _ = app.emit("web://to-parse", serde_json::json!({ "url": target }));
                    }
                }
            }

            // 收集：滚动 → 抽取 → 合并；条数连续 3 轮不增长即判到底
            if !collecting {
                continue;
            }
            if stale >= 3 {
                collecting = false;
                // 把页面尾部提示记下来：区分"站点说没有更多了"与"被登录墙/风控挡住"
                let _ = window.eval(
                    "(function(){var t=document.body.innerText||'';t=t.split(String.fromCharCode(10)).join(' ');var tail=t.slice(-260);var c=document.getElementById('xhsdown-count');var n=c?c.textContent:'';history.replaceState(null,'','#xhsdownselftest='+encodeURIComponent(n+' ||| '+tail));})()",
                );
                let mut total = 0;
                if let Some(tail) = read_selftest(&window).await {
                    // 读数里前半是解析栏自己的计数（已加载 N / 共 M / 已收全 N），后半是页尾文字：
                    // 一起记下来，才分得清"真到底了"和"被站点挡住了"
                    xhs_core::login::debug_log(&format!("网页收集：自动停止时的页面读数 = {tail}"));
                    total = parse_total(tail.split("|||").next().unwrap_or(""));
                }
                let _ = window.eval("history.replaceState(null,'',location.pathname+location.search)");
                emit_done(&app, &collected, true, total);
                continue;
            }
            let _ = window.eval(SCROLL_BOTTOM_JS);
            tokio::time::sleep(Duration::from_millis(1600)).await;
            match extract_from_page(&window).await {
                Some(value) => {
                    let cards = UserBundle::cards_from_web(&value);
                    let before = collected.len();
                    for card in cards {
                        let entry = card.entry("");
                        if seen_bids.insert(entry.bid.clone()) {
                            collected.push(entry);
                        }
                    }
                    if collected.len() > before {
                        stale = 0;
                        let _ = app.emit(
                            "collect://update",
                            serde_json::json!({ "count": collected.len() }),
                        );
                    } else {
                        stale += 1;
                    }
                }
                None => stale += 1,
            }
        }
    });
}

/// 地址栏状态：当前网页地址 + 窗口是否可见（主界面的网页工具栏据此显示）
#[tauri::command]
pub fn web_bar_state(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let window = app.get_webview_window("xhs-work");
    let visible = window
        .as_ref()
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    let url = window
        .and_then(|w| w.url().ok())
        .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
        .unwrap_or_default();
    // 窗口可见时持续跟踪"用户最后在看的页面"：之后点 WEB 恢复的就是它。
    // 取数/收起期间窗口不可见，不会把取数残留页误记成用户的页面；
    // /me、登录页、404 也不是用户自己浏览的页面（登录检测/风控落的），同样不记
    let user_browsing = visible
        && url.starts_with("http")
        && !url.contains("/user/profile/me")
        && !url.contains("/login")
        && !url.contains("/404");
    if user_browsing {
        *state.last_web_url.lock().unwrap() = url.clone();
    }
    Ok(serde_json::json!({ "url": url, "visible": visible }))
}

/// 地址栏：跳转（只允许 http/https）
#[tauri::command]
pub fn web_bar_go(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("只支持 http/https 地址".into());
    }
    let window = app
        .get_webview_window("xhs-work")
        .ok_or_else(|| "网页窗口不存在".to_string())?;
    let parsed: tauri::Url = trimmed.parse().map_err(|e| format!("地址无效：{e}"))?;
    window.navigate(parsed).map_err(err)?;
    Ok(())
}

/// 地址栏：后退 / 前进 / 刷新（在网页里执行对应脚本）
#[tauri::command]
pub fn web_bar_cmd(app: tauri::AppHandle, cmd: String) -> Result<(), String> {
    let window = app
        .get_webview_window("xhs-work")
        .ok_or_else(|| "网页窗口不存在".to_string())?;
    let script = match cmd.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        "reload" => "location.reload()",
        other => return Err(format!("不认识的指令：{other}")),
    };
    window.eval(script).map_err(err)?;
    Ok(())
}

/// 地址栏：收起网页窗口
#[tauri::command]
pub fn web_bar_hide(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("xhs-work") {
        let _ = window.hide();
    }
    Ok(())
}

/// 网页窗口显示期间，把当前地址同步到窗口标题（用户一眼看到自己在哪一页）。
/// 用后端定时器：界面窗口被遮挡时前端定时器会被降频，后端不受影响。
fn spawn_title_sync(app: tauri::AppHandle) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(window) = app.get_webview_window("xhs-work") else {
                break;
            };
            if !window.is_visible().unwrap_or(false) {
                break;
            }
            if let Ok(url) = window.url() {
                let text = url.to_string().split('#').next().unwrap_or("").to_string();
                let title = if text.is_empty() {
                    "小红书（网页）".to_string()
                } else {
                    format!("小红书（网页）— {}", xhs_core::error::truncate(&text, 120))
                };
                let _ = window.set_title(&title);
            }
        }
    });
}

/// 打开网页窗口：登录了就开当前账号的主页，没登录就开推荐流。
/// 用途：在软件里直接看/复制链接（分享出来的链接 token 才是新鲜的）。
#[tauri::command]
pub async fn open_web_home(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let settings = state.settings_snapshot();
    // 等正在进行的取数/登录检测完成再动窗口：它们的导航被打断会双双失败
    // （锁在函数结束前释放——浏览本身不该持锁，也不该阻塞后续下载）
    let _fetch_guard = state.fetch_lock.lock().await;
    // 窗口是不是这次调用新建的：新建才做尺寸/居中，别每次点 WEB 都把用户调好的窗口拍回去
    let existed = app.get_webview_window("xhs-work").is_some();
    let window = ensure_work_window(&app, &settings).await?;

    // 去哪儿：本会话第一次点 WEB → 发现页；之后 → 恢复用户最后在看的页面
    // （取数会把窗口导航走，恢复逻辑保证点 WEB 永远回到用户自己的页面）
    let saved = state.last_web_url.lock().unwrap().clone();
    let current = window
        .url()
        .map(|u| u.to_string().split('#').next().unwrap_or("").to_string())
        .unwrap_or_default();
    if saved.is_empty() || !current.starts_with("http") {
        let parsed: tauri::Url = "https://www.xiaohongshu.com/explore"
            .parse()
            .map_err(|e| format!("地址无效：{e}"))?;
        window.navigate(parsed).map_err(err)?;
    } else if current != saved {
        let parsed: tauri::Url = saved.parse().map_err(|e| format!("地址无效：{e}"))?;
        window.navigate(parsed).map_err(err)?;
    }
    if !existed {
        let _ = window.set_title("小红书（网页）");
        let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize::new(1000.0, 760.0)));
        let _ = window.center();
    }
    let _ = window.show();
    let _ = window.set_focus();
    // 显示期间把当前地址同步到窗口标题（后端定时器，不受界面降频影响）
    spawn_title_sync(app.clone());
    // 并在网页里注入浮动地址栏（后退/前进/刷新/地址/复制/解析）
    spawn_page_bar(app.clone());
    Ok(())
}

#[tauri::command]
pub async fn web_login_cookies(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<LoginInfo, String> {
    let not_yet = || {
        "还没有检测到登录状态——请在登录页完成登录（扫码，或手机号 + 短信验证码；小红书网页端不提供账号密码登录）"
            .to_string()
    };

    // 顺手把过期的二维码刷掉（过期后自动点刷新，点不动就重载页面）
    if let Some(window) = app.get_webview_window("xhs-work") {
        let _ = window.eval(QR_KEEPER_JS);
        log_qr_state(&window);
    }

    // 便宜预检：Cookies 没变化、且距上次权威检测不到 12 秒 → 直接回答"还没登录"，
    // 省掉一次主页请求（轮询每 2 秒一次，全量请求太密）
    let fingerprint = app
        .get_webview_window("xhs-work")
        .map(|window| cookie_fingerprint(&window))
        .unwrap_or_default();
    let (changed, due) = {
        let mut baseline = state.login_cookie_baseline.lock().unwrap();
        let changed = match baseline.as_ref() {
            Some(saved) => saved != &fingerprint,
            None => {
                // 第一次轮询：记下基线（此刻用户还没登录）
                *baseline = Some(fingerprint.clone());
                true
            }
        };
        let last = *state.last_login_check.lock().unwrap();
        let due = last
            .map(|t| t.elapsed() > Duration::from_secs(12))
            .unwrap_or(true);
        (changed, due)
    };
    if !changed && !due {
        return Err(not_yet());
    }
    *state.last_login_check.lock().unwrap() = Some(std::time::Instant::now());

    // 权威检测：让浏览器自己读「我的主页」，拿到真实资料才算登录。
    // （页面里的 loggedIn 字段对匿名访客也是 true，不能用来判定——实测踩过）
    let Some((uid, name, avatar)) = browser_login_check(&app).await? else {
        return Err(not_yet());
    };
    finish_login(&app, &state, uid, name, avatar).await
}

#[tauri::command]
pub async fn web_login_close(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // 只隐藏、不关闭：窗口一关会销毁 WebView2 环境，会话 Cookie 可能来不及落盘
    if let Some(window) = app.get_webview_window("xhs-work") {
        let _ = window.hide();
    }
    state.login_window_open.store(false, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
pub async fn logout(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<LoginInfo, String> {
    let settings = state.settings_snapshot();
    // 关掉登录窗与取数窗：浏览器数据目录被占用时删不掉
    for label in ["xhs-work"] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.close();
        }
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 清掉浏览器数据目录（登录态真正所在），删不掉就留标记下次启动再删——
    // 不这样做会出现"点了退出登录，重启又自动登录回去"
    let dir = crate::state::webview_dir(&settings);
    if dir.exists() && std::fs::remove_dir_all(&dir).is_err() {
        let _ = std::fs::create_dir_all(settings.data_root());
        let _ = std::fs::write(crate::state::webview_logout_marker(&settings), b"1");
        xhs_core::login::debug_log("logout：浏览器目录被占用，已留标记下次启动清理");
    }
    let _ = std::fs::remove_file(settings.cookies_path());
    xhs_core::login::debug_log("logout：已清除登录态");

    let client = XhsClient::with_proxy(Some(&settings.proxy)).map_err(err)?;
    *state.client.write().unwrap() = Arc::new(client);
    *state.login.write().unwrap() = None;
    *state.login_cookie_baseline.lock().unwrap() = None;
    *state.last_login_check.lock().unwrap() = None;
    Ok(LoginInfo::default())
}

// ───────────────────────── 内容库（我的笔记） ─────────────────────────

#[tauri::command]
pub async fn library_folders(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<LibraryFolders, String> {
    let client = state.client();
    let login = state.login.read().unwrap().clone().unwrap_or_default();
    let uid = if !login.mid.is_empty() {
        login.mid.clone()
    } else {
        browser_login_check(&app)
            .await?
            .map(|(uid, _, _)| uid)
            .ok_or_else(|| "读取我的笔记需要先登录".to_string())?
    };

    // 自己的主页用 /user/profile/me：它不需要 token（别人的主页才需要）
    let user = fetch_user(&app, &client, &uid, "https://www.xiaohongshu.com/user/profile/me").await?;
    let created: Vec<FavItem> = user
        .cards
        .iter()
        .enumerate()
        .map(|(index, card)| FavItem {
            id: index as u64 + 1,
            bid: card.id.clone(),
            title: naming::title_of(&card.title),
            created_at: card.created_at,
            pics: 0,
            video: card.is_video,
            url: note_page_url(&card.id, &card.token, &card.source),
            kind: "fav".into(),
        })
        .collect();
    let fav_note = if created.is_empty() {
        "这个账号还没有公开笔记，或笔记仅自己可见".to_string()
    } else {
        format!("网页端仅能读取主页最新 {} 条笔记", created.len())
    };

    Ok(LibraryFolders {
        mid: uid,
        created,
        subscribed: Vec::new(),
        fav_note,
        follow_note: String::new(),
    })
}

/// 资源管理器定位：打开文件所在文件夹并选中该文件（目录则直接打开目录）。
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    // 规范化并剥掉 canonicalize 加的 \\?+\\ 前缀：
    // 不剥的话资源管理器解析不了（回退打开"此电脑"，实测踩过）
    let mut target = std::fs::canonicalize(&path)
        .unwrap_or_else(|_| std::path::PathBuf::from(&path));
    let s = target.to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\\UNC\\") {
        target = std::path::PathBuf::from(format!(r"\\{rest}"));
    } else if let Some(rest) = s.strip_prefix(r"\\?\\") {
        target = std::path::PathBuf::from(rest.to_string());
    }
    xhs_core::login::debug_log(&format!(
        "reveal_path：{} → {}",
        path,
        target.display()
    ));
    if !target.exists() {
        return Err(format!("路径不存在：{}", target.display()));
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if target.is_dir() {
            std::process::Command::new("explorer.exe")
                .arg(&target)
                .creation_flags(0x08000000)
                .spawn()
                .map_err(err)?;
        } else {
            std::process::Command::new("explorer.exe")
                .args(["/select,", &target.to_string_lossy()])
                .creation_flags(0x08000000)
                .spawn()
                .map_err(err)?;
        }
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open")
            .arg(target.parent().unwrap_or(&target))
            .spawn()
            .map_err(err)?;
        Ok(())
    }
}

// ───────────────────────── 暂停 / 继续 ─────────────────────────

fn set_paused(app: &tauri::AppHandle, state: &State<'_, AppState>, task_id: &str) -> Result<(), String> {
    let tasks = state.tasks.lock().unwrap();
    let Some(entry) = tasks.get(task_id) else {
        return Err("任务不存在或已结束".into());
    };
    entry.pause.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut task = entry.snapshot.lock().unwrap();
    if matches!(task.status.as_str(), "queued" | "downloading" | "saving") {
        task.status = "paused".into();
        task.message = "已暂停".into();
        task.speed_bps = 0.0;
        // 排队中的任务没有下载循环替我们发事件，这里立即广播暂停态
        emit_task(app, &task.clone());
    }
    Ok(())
}

#[tauri::command]
pub fn pause_download(app: tauri::AppHandle, state: State<'_, AppState>, task_id: String) -> Result<(), String> {
    set_paused(&app, &state, &task_id)
}

#[tauri::command]
pub fn pause_all_downloads(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<usize, String> {
    let tasks = state.tasks.lock().unwrap();
    let mut count = 0;
    for entry in tasks.values() {
        let mut task = entry.snapshot.lock().unwrap();
        if matches!(task.status.as_str(), "queued" | "downloading" | "saving") {
            entry.pause.store(true, std::sync::atomic::Ordering::Relaxed);
            task.status = "paused".into();
            task.message = "已暂停".into();
            task.speed_bps = 0.0;
            emit_task(&app, &task.clone());
            count += 1;
        }
    }
    Ok(count)
}

/// 从暂停态恢复：用保存的请求重跑流水线（视频/图片走 .part 断点续传）。
/// 必须是 async：同步命令跑在主线程，tokio::spawn 在主线程会 panic（应用闪退）。
#[tauri::command]
pub async fn resume_download(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    task_id: String,
) -> Result<(), String> {
    let (req, snapshot) = {
        let tasks = state.tasks.lock().unwrap();
        let Some(entry) = tasks.get(&task_id) else {
            return Err("任务不存在或已结束".into());
        };
        if entry.snapshot.lock().unwrap().status != "paused" {
            return Err("任务不在暂停状态".into());
        }
        (entry.req.clone(), entry.snapshot.clone())
    };
    {
        let mut task = snapshot.lock().unwrap();
        task.status = "queued".into();
        task.message = "排队中".into();
        // 立即广播：恢复的任务可能要等并发槽，不发事件 UI 会一直停在「已暂停」
        emit_task(&app, &task.clone());
    }
    // 先换新旗标再 spawn：重跑的 run_task 起始就读旗标，
    // 旧旗标还停在 true 时会立即再次暂停（表现为「点了继续没反应」）
    let fresh_pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut tasks = state.tasks.lock().unwrap();
        let Some(entry) = tasks.get_mut(&task_id) else {
            return Err("任务不存在或已结束".into());
        };
        entry.pause = fresh_pause;
        // 先中止旧的 run_task 再 spawn：旗标只让下载循环在分块边界退出，
        // 旧任务的收尾窗口里还占着并发槽，不中止会让新任务一直停在排队中
        if let Some(old) = entry.abort.take() {
            old.abort();
        }
    }
    let handle = tokio::spawn(run_task(app, req, task_id.clone()));
    if let Some(entry) = state.tasks.lock().unwrap().get_mut(&task_id) {
        entry.abort = Some(handle.abort_handle());
    }
    Ok(())
}

#[tauri::command]
pub async fn resume_all_downloads(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    let paused: Vec<String> = {
        let tasks = state.tasks.lock().unwrap();
        tasks
            .iter()
            .filter(|(_, e)| e.snapshot.lock().unwrap().status == "paused")
            .map(|(id, _)| id.clone())
            .collect()
    };
    let count = paused.len();
    for task_id in paused {
        let _ = resume_download(app.clone(), state.clone(), task_id).await;
    }
    Ok(count)
}

/// 一次重命名动作的结果（预演与实操共用）。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamePlan {
    pub renamed: usize,
    pub skipped: usize,
    pub missing: usize,
    pub details: Vec<String>,
    pub dry_run: bool,
}

/// 按当前命名规则，把已下载的条目重命名成新编号。
///
/// 只改"条目名"（编号 + 标题），**不动目录层级**：层级由「文件夹」规则决定。
/// 匹配方式：在输出目录里递归找"去掉数字前缀后与条目标题相同"的文件，
/// 命中就原地改名；找不到的条目记进 missing，不做任何猜测。
#[tauri::command]
pub async fn rename_downloaded(
    state: State<'_, AppState>,
    input: String,
    total: usize,
    dry_run: bool,
) -> Result<RenamePlan, String> {
    let settings = state.settings_snapshot();
    let key = match parse_input(&input) {
        Ok(SourceTarget::User { uid, .. }) => format!("user:{uid}"),
        _ => input.trim().to_string(),
    };
    let cache = {
        let batches = state.batches.lock().unwrap();
        batches.get(&key).cloned()
    };
    let Some(cache) = cache else {
        return Err("这个来源的解析结果已过期，请重新解析后再试".into());
    };
    let count = cache.items.len();
    let pad = format!("{total}").len().max(2);
    let _ = count;

    let mut plan = RenamePlan { dry_run, ..Default::default() };

    for (position, item) in cache.items.iter().enumerate() {
        let index = if total > 0 { std::cmp::max(total - position, 1) } else { position + 1 };
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("title".into(), item.title.clone());
        vars.insert("author".into(), item.author.clone());
        vars.insert("bid".into(), item.bid.clone());
        vars.insert("uid".into(), cache.uid.clone());
        let (y, m, d) = {
            let secs = item.created_at + 8 * 3600;
            civil_from_unix(secs)
        };
        vars.insert("publish_date".into(), format!("{y}-{m:02}-{d:02}"));
        vars.insert("year".into(), format!("{y}"));
        vars.insert("month".into(), format!("{m:02}"));
        vars.insert("date".into(), format!("{y}-{m:02}-{d:02}"));
        vars.insert("index".into(), naming::padded_index(index as u32, pad));

        // 条目名：视频是文件名，图文是文件夹名（这里都是文件）
        let wanted_stem = naming::render(&settings.naming_template, &vars, false);

        let found =
            find_downloaded_by_title(&settings.output_dir, &item.title, &wanted_stem).await;
        match found {
            Some(found) => {
                // 扩展名沿用原文件（按封装去猜会改错）
                let wanted = match found.extension() {
                    Some(ext) if !ext.is_empty() => {
                        format!(
                            "{}.{}",
                            std::path::Path::new(&wanted_stem)
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default(),
                            ext.to_string_lossy()
                        )
                    }
                    _ => wanted_stem.clone(),
                };
                if found
                    .file_name()
                    .map(|s| s.to_string_lossy() == wanted)
                    .unwrap_or(false)
                {
                    plan.skipped += 1;
                    continue;
                }
                let target = found.with_file_name(&wanted);
                if target.exists() {
                    plan.skipped += 1;
                    plan.details.push(format!("跳过（同名已存在）：{wanted}"));
                    continue;
                }
                let from = found
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                plan.details.push(format!("{from}  →  {wanted}"));
                if !dry_run {
                    tokio::fs::rename(&found, &target)
                        .await
                        .map_err(|e| format!("重命名失败 {from}: {e}"))?;
                }
                plan.renamed += 1;
            }
            None => {
                plan.missing += 1;
            }
        }
    }
    Ok(plan)
}

/// 在输出目录里递归找"去掉数字前缀后与标题相同"的文件。
///
/// 只认两种形态：`标题` 与 `数字前缀 + 标题（可带扩展名）`，其余一律不动——
/// 宁可少改，也不猜错。
async fn find_downloaded_by_title(
    root: &std::path::Path,
    title: &str,
    wanted: &str,
) -> Option<std::path::PathBuf> {
    let wanted_stem = std::path::Path::new(wanted)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| wanted.to_string());
    let wanted_title = strip_number_prefix(&wanted_stem);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                stack.push(path.clone());
                continue;
            }
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let bare = strip_number_prefix(&stem);
            if bare == wanted_title || bare == title || stem == wanted_stem {
                return Some(path);
            }
        }
    }
    None
}

/// 去掉开头的"编号 + 分隔符"：`001 标题` / `12-标题` / `3 标题` 都还原成标题。
fn strip_number_prefix(name: &str) -> String {
    let trimmed = name.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return trimmed.trim().to_string();
    }
    trimmed[digits.len()..]
        .trim_start_matches([' ', '-', '_', '.', '、', '·'])
        .trim()
        .to_string()
}

// ───────────────────────── 应用更新 ─────────────────────────

/// 更新源仓库：发布 Release 后此处即可在线检测（tag 形如 v0.2.0）。
const UPDATE_REPO: &str = "yungumax/XHSdown";

/// 三段式版本号比较：a >= b 返回 true（逐段数值比较，缺段按 0）。
fn version_at_least(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..3 {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    true
}

#[tauri::command]
pub async fn check_updates() -> Result<UpdateCheck, String> {
    let current = APP_VERSION.to_string();
    let url = format!("https://api.github.com/repos/{UPDATE_REPO}/releases/latest");
    let http = reqwest::Client::builder()
        .user_agent(format!("XHSdown/{APP_VERSION}"))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(err)?;
    let resp = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(err)?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(UpdateCheck {
            current,
            latest: String::new(),
            up_to_date: true,
            error: format!("仓库 {UPDATE_REPO} 尚未发布 Release"),
        });
    }
    if !status.is_success() {
        return Ok(UpdateCheck {
            current,
            latest: String::new(),
            up_to_date: false,
            error: format!("检测失败 HTTP {status}"),
        });
    }
    let value: serde_json::Value = resp.json().await.map_err(err)?;
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let latest = tag.trim_start_matches('v').to_string();
    if latest.is_empty() {
        return Ok(UpdateCheck {
            current,
            latest: String::new(),
            up_to_date: true,
            error: "Release 未打版本标签".into(),
        });
    }
    let up_to_date = version_at_least(&current, &latest);
    Ok(UpdateCheck {
        current,
        latest,
        up_to_date,
        error: String::new(),
    })
}

// ───────────────────────── 命名预览 ─────────────────────────

/// 界面日志：前端把关键状态写进同一个调试日志，排查时不用猜界面拿到了什么。
#[tauri::command]
pub fn ui_log(text: String) {
    xhs_core::login::debug_log(&format!("[UI] {text}"));
}

#[tauri::command]
pub fn naming_variables() -> Vec<NamingVar> {
    naming::VARIABLES
        .iter()
        .map(|(token, label, section, hint)| NamingVar {
            token: token.to_string(),
            label: label.to_string(),
            section: section.to_string(),
            hint: hint.to_string(),
        })
        .collect()
}

fn preview_vars(date: &str, publish_date: &str, ext: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    vars.insert("title".into(), "示例笔记文案".into());
    vars.insert("author".into(), "示例作者".into());
    vars.insert("bid".into(), "66b7d3a7000000001e01c8b3".into());
    vars.insert("uid".into(), "58c8014a6a6a690dbb0c84c1".into());
    vars.insert("date".into(), date.to_string());
    vars.insert("publish_date".into(), publish_date.to_string());
    let (year, month) = publish_date
        .split_once('-')
        .map(|(y, rest)| (y.to_string(), rest.split('-').next().unwrap_or("01").to_string()))
        .unwrap_or((date[..4.min(date.len())].to_string(), "01".to_string()));
    vars.insert("year".into(), year);
    vars.insert("month".into(), month);
    vars.insert("source_kind".into(), "用户主页".into());
    vars.insert("index".into(), "07".into());
    vars.insert("ext".into(), ext.to_string());
    vars
}

#[tauri::command]
pub fn preview_naming(
    template: String,
    date: Option<String>,
    publish_date: Option<String>,
    ext: Option<String>,
    dir: Option<bool>,
) -> Result<String, String> {
    let today = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (y, m, d) = civil_from_unix(now + 8 * 3600);
        format!("{y}-{m:02}-{d:02}")
    };
    let vars = preview_vars(
        &date.unwrap_or_else(|| today.clone()),
        &publish_date.unwrap_or_else(|| "2025-10-01".into()),
        &ext.unwrap_or_else(|| if dir.unwrap_or(false) { String::new() } else { "jpg".into() }),
    );
    Ok(naming::render(&template, &vars, dir.unwrap_or(false)))
}

#[tauri::command]
pub fn preview_names(
    items: Vec<serde_json::Value>,
    ext: Option<String>,
) -> Result<Vec<String>, String> {
    let today = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (y, m, d) = civil_from_unix(now + 8 * 3600);
        format!("{y}-{m:02}-{d:02}")
    };
    let mut out = Vec::new();
    for item in items {
        let mut vars = preview_vars(&today, &publish_of(&item), &ext.clone().unwrap_or_default());
        if let Some(title) = item.get("title").and_then(|v| v.as_str()) {
            vars.insert("title".into(), naming::title_of(title));
        }
        if let Some(naming) = item.get("naming").and_then(|v| v.as_object()) {
            for (key, value) in naming {
                if let Some(text) = value.as_str() {
                    if !text.is_empty() {
                        vars.insert(key.clone(), text.to_string());
                    }
                } else if let Some(num) = value.as_u64() {
                    if *key == "index" {
                        let pad = item
                            .pointer("/naming/index_pad")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(2) as usize;
                        vars.insert("index".into(), naming::padded_index(num as u32, pad));
                    } else {
                        vars.insert(key.clone(), num.to_string());
                    }
                }
            }
        }
        out.push(naming::render(
            &crate::state::Settings::load().naming_template,
            &vars,
            false,
        ));
    }
    Ok(out)
}

fn publish_of(item: &serde_json::Value) -> String {
    item.pointer("/naming/publish_date")
        .and_then(|v| v.as_str())
        .unwrap_or("2025-10-01")
        .to_string()
}

// ───────────────────────── 系统交互与维护 ─────────────────────────

#[tauri::command]
pub async fn choose_output_dir(app: tauri::AppHandle) -> Result<String, String> {
    let folder = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .map(|path| path.to_string());
    match folder {
        Some(path) => Ok(path),
        // 用户取消不报错：返回当前设置的原值由前端兜底
        None => Err("已取消选择".into()),
    }
}

#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    // 规范化：统一反斜杠 + 解析成绝对路径（混入的正斜杠会让资源管理器行为异常）
    let mut target = std::fs::canonicalize(&path).unwrap_or_else(|_| std::path::PathBuf::from(&path));
    // canonicalize 会加 Win32 命名空间前缀 \\?\：剥掉它，普通路径形式对 explorer 更友好
    let s = target.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(r"\\?\UNC\") {
        target = PathBuf::from(format!(r"\\{stripped}"));
    } else if let Some(stripped) = s.strip_prefix(r"\\?\") {
        target = PathBuf::from(stripped.to_string());
    }
    xhs_core::login::debug_log(&format!("open_path：{} → {}", path, target.display()));
    if !target.exists() {
        return Err(format!("路径不存在：{}", target.display()));
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const NO_WINDOW: u32 = 0x0800_0000;
        if target.is_dir() {
            // 目录：资源管理器打开目录
            std::process::Command::new("explorer.exe")
                .arg(&target)
                .creation_flags(NO_WINDOW)
                .spawn()
                .map_err(err)?;
        } else {
            // 文件：系统默认应用打开（视频→播放器、图片→看图器）
            std::process::Command::new("cmd")
                .args(["/C", "start", "", &target.to_string_lossy()])
                .creation_flags(NO_WINDOW)
                .spawn()
                .map_err(err)?;
        }
        return Ok(());
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("xdg-open").arg(&target).spawn().map_err(err)?;
        Ok(())
    }
}

#[tauri::command]
pub fn resume_pending() -> Result<u32, String> {
    // 小红书媒体是小文件直下，任务不落盘跨会话续传；保留接口与前端对齐。
    Ok(0)
}

#[tauri::command]
pub fn cleanup_temp(state: State<'_, AppState>) -> Result<u32, String> {
    let settings = state.settings_snapshot();
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(&settings.output_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".xhstmp") {
                if std::fs::remove_dir_all(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

#[tauri::command]
pub fn export_diagnostics(state: State<'_, AppState>) -> Result<String, String> {
    let settings = state.settings_snapshot();
    let dir = settings.data_root();
    std::fs::create_dir_all(&dir).map_err(err)?;
    let cookies = state.client().export_cookies();
    let names: Vec<&str> = cookies.iter().map(|(n, _)| n.as_str()).collect();
    let text = format!(
        "XHSdown v{}\n时间：{}\n\n设置：\n{}\n\n会话 Cookie 名称：{:?}\n",
        APP_VERSION,
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let (y, m, d) = civil_from_unix(now + 8 * 3600);
            format!("{y}-{m:02}-{d:02}")
        },
        serde_json::to_string_pretty(&settings).unwrap_or_default(),
        names,
    );
    let path = dir.join("diagnostics.txt");
    std::fs::write(&path, text).map_err(err)?;
    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 候选阶梯保持精简：原样 → 移动版路径。
    /// （实测：被风控时六种形态无一幸免，多列形态只会拿会话去撞墙）
    #[test]
    fn note_url_candidates_keep_it_minimal() {
        let url = "https://www.xiaohongshu.com/explore/6a1f0000000000000000abcd?xsec_token=ABtok=&xsec_source=pc_search";
        let list = note_url_candidates(url);
        assert_eq!(list.len(), 2, "原样 + 移动版各一条");
        assert_eq!(list[0], url);
        assert!(list[1].contains("/discovery/item/"));
        // 没有 token 时只有一个候选（换路径没有意义）
        let bare =
            note_url_candidates("https://www.xiaohongshu.com/explore/6a1f0000000000000000abcd");
        assert_eq!(bare.len(), 1);
    }

    #[test]
    fn split_note_url_reads_id_and_token() {
        let (id, token) = split_note_url(
            "https://www.xiaohongshu.com/explore/6a1f0000000000000000abcd?xsec_token=ABtok%3D&xsec_source=pc_user",
        );
        assert_eq!(id, "6a1f0000000000000000abcd");
        assert_eq!(token, "ABtok%3D");
        let (id2, token2) =
            split_note_url("https://www.xiaohongshu.com/discovery/item/6a1f0000000000000000abcd");
        assert_eq!(id2, "6a1f0000000000000000abcd");
        assert!(token2.is_empty());
    }

    /// 来源上下文必须跟着链接走：写错来源会被站点弹回首页（实测踩过）
    #[test]
    fn note_page_url_keeps_source() {
        assert!(note_page_url("id", "tok", "pc_search").contains("xsec_source=pc_search"));
        assert!(note_page_url("id", "tok", "").contains("xsec_source=pc_user"));
        assert_eq!(
            note_page_url("id", "", "pc_search"),
            "https://www.xiaohongshu.com/explore/id"
        );
    }

    #[test]
    fn expected_kind_by_url() {
        assert_eq!(
            expected_page_kind("https://www.xiaohongshu.com/explore/6a1f0000000000000000abcd?x=1"),
            "note"
        );
        assert_eq!(expected_page_kind("https://www.xiaohongshu.com/explore"), "feed");
        assert_eq!(
            expected_page_kind("https://www.xiaohongshu.com/user/profile/66f3c481000000001c002a76"),
            "user"
        );
        assert_eq!(
            expected_page_kind("https://www.xiaohongshu.com/search_result?keyword=x"),
            "search"
        );
        assert_eq!(
            expected_page_kind("https://www.xiaohongshu.com/discovery/item/6a1f0000000000000000abcd"),
            "note"
        );
    }

    /// 扩展名要从 CDN 的 `!规格` 里认（路径上没有扩展名）
    #[test]
    fn url_ext_reads_cdn_variant_format() {
        assert_eq!(
            url_ext("https://sns-webpic-qc.xhscdn.com/2026/1040g!nc_n_webp_mw_1"),
            "webp"
        );
        assert_eq!(url_ext("https://sns-webpic-qc.xhscdn.com/2026/1040g.jpg"), "jpg");
        assert_eq!(url_ext("https://sns-webpic-qc.xhscdn.com/2026/1040g"), "jpg");
    }
}
