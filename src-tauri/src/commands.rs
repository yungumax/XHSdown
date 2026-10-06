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

    // 登录态：让浏览器读一次「我的主页」——登录态存在浏览器数据目录里，
    // 所以跨重启也能恢复。超时或失败一律当未登录，不卡界面。
    let mut login = LoginInfo::default();
    // 给足 40 秒：页面加载慢时（风控/网络）也能等到结果，不要提前放弃
    let check = tokio::time::timeout(Duration::from_secs(40), browser_login_check(&app)).await;
    match check {
        Ok(Ok(Some((uid, name, avatar)))) => {
            let client = state.client();
            login = LoginInfo {
                logged_in: true,
                uname: name,
                face: inline_avatar(&client, &avatar).await,
                mid: uid,
                ..Default::default()
            };
            *state.login.write().unwrap() = Some(login.clone());
            xhs_core::login::debug_log("启动恢复登录：已登录");
        }
        Ok(Ok(None)) => xhs_core::login::debug_log("启动恢复登录：未登录"),
        Ok(Err(e)) => xhs_core::login::debug_log(&format!("启动恢复登录失败：{e}")),
        Err(_) => xhs_core::login::debug_log("启动恢复登录：检测超时"),
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
fn note_page_url(id: &str, token: &str) -> String {
    if token.is_empty() {
        format!("https://www.xiaohongshu.com/explore/{id}")
    } else {
        format!("https://www.xiaohongshu.com/explore/{id}?xsec_token={token}&xsec_source=pc_user")
    }
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
            };
            state.cache_batch(key.clone(), cache.clone());
            if raw_input.trim() != key {
                state.cache_batch(raw_input.trim().to_string(), cache);
            }
            Ok(probe)
        }
        SourceTarget::User { uid, token, url } => {
            let page_url = user_fetch_url(&uid, &token, &url);
            let user = fetch_user(&app, &client, &uid, &page_url).await?;
            let entries: Vec<PostEntry> = user
                .cards
                .iter()
                .map(|card| PostEntry {
                    bid: card.id.clone(),
                    mid: String::new(),
                    title: naming::title_of(&card.title),
                    author: user.nickname.clone(),
                    created_at: card.created_at,
                    pics: 0,
                    has_video: card.is_video,
                    is_audio: false,
                    media_url: String::new(),
                    page_url: note_page_url(&card.id, &card.token),
                    duration: 0,
                    is_retweet: false,
                    media_unknown: true,
                })
                .collect();
            let exhausted = true;
            let note = if entries.is_empty() {
                "这个作者没有可解析的笔记（或仅对粉丝可见）".to_string()
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
                "probe user={uid} 本页 {} 条",
                probe.items.len()
            ));
            let cache = BatchCache {
                uid: user.uid.clone(),
                items: probe.items.clone(),
                exhausted,
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
    }
}

#[tauri::command]
pub async fn probe_more(
    state: State<'_, AppState>,
    input: String,
    _want: usize,
) -> Result<ProbeMore, String> {
    // 小红书网页端的翻页接口要 x-s 签名，桌面端拿不到：
    // 主页解析只给最新一批，这里如实返回 exhausted，由界面隐藏「继续解析」。
    let client = state.client();
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
    let (loaded, uid, exhausted) = {
        let batches = state.batches.lock().unwrap();
        match batches.get(&key) {
            Some(cache) => (cache.items.len(), cache.uid.clone(), cache.exhausted),
            None => (0, String::new(), true),
        }
    };
    let _ = uid;
    Ok(ProbeMore {
        items: Vec::new(),
        loaded,
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
            let url = dl::image_variant(pic, &image_spec);
            let ext = url_ext(&url);
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
            // 会出现「UI 已暂停、底层还在下」），中止也不会留下半个损坏的图片文件
            match dl::download_resumable(&client.http, &url, &dest, &opts, progress, Some(pause.clone())).await
            {
                Ok(dl::DownloadEnd::Completed(size)) => {
                    downloaded_total += size;
                    if primary.is_none() {
                        primary = Some(dest.clone());
                    }
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
                    done("failed", &format!("图片下载失败：{e}"), "");
                    return;
                }
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
    let window = tauri::WebviewWindowBuilder::new(
        app,
        "xhs-work",
        tauri::WebviewUrl::External("about:blank".parse().unwrap()),
    )
    .title("XHSdown")
    .inner_size(1000.0, 760.0)
    .visible(false)
    .data_directory(dir)
    .build()
    .map_err(err)?;
    Ok(window)
}

/// 页面提取脚本：把当前页的关键数据（笔记详情 / 用户主页）打包写进 URL 片段。
/// webview 里的远程页面没有 Tauri IPC，URL 片段是唯一能把值带回 Rust 的通道。
const EXTRACT_JS: &str = r#"(function () {
  var report = function (out) {
    try {
      location.replace('#xhsdownid=' + Date.now() + '~' + encodeURIComponent(JSON.stringify(out)));
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

  try {
    var st = window.__INITIAL_STATE__ || null;
    if (!st) {
      report({ ready: false, pageUrl: location.href });
      return;
    }

    // 先按**当前网址**判断页面类型，再读对应的数据。
    // 不能"状态里有什么就读什么"：小红书是单页应用，状态会残留上一次看过的
    // 笔记/主页数据（实测：在推荐流页面上读到了残留的笔记，登录流程一直等不到昵称）
    var here = location.href;
    var urlNote = here.match(/(?:\/explore\/|\/discovery\/item\/)([0-9a-f]{24})/);
    var isNotePage = !!urlNote;
    var isProfilePage = here.indexOf("/user/profile/") >= 0;
    if (!isNotePage && !isProfilePage) {
      report({ ready: false, pageUrl: here });
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
        var best = '';
        var infos = arr(pic.infoList);
        for (var j = 0; j < infos.length; j++) {
          var infoItem = unwrap(infos[j]) || {};
          if (infoItem.imageScene === 'WB_DFT' && infoItem.url) {
            best = infoItem.url;
            break;
          }
        }
        if (!best && infos.length) best = str(infos[0]);
        if (!best) best = str(pic.urlDefault) || str(pic.urlPre);
        if (best) pics.push(best);
      }
      var videos = [];
      var video = unwrap(note.video) || {};
      var media = unwrap(video.media) || {};
      var streams = unwrap(media.stream) || {};
      var codecs = ['h264', 'h265', 'av1'];
      for (var c = 0; c < codecs.length && !videos.length; c++) {
        var streamArr = arr(streams[codecs[c]]);
        for (var k = 0; k < streamArr.length; k++) {
          var it = unwrap(streamArr[k]) || {};
          var meta = unwrap(it.videoMeta) || {};
          if (it.masterUrl) {
            videos.push({ url: it.masterUrl, height: meta.height || 0, duration: meta.duration || 0 });
          }
        }
      }
      var nu = unwrap(note.user) || {};
      report({
        ready: true, kind: 'note', pageUrl: location.href,
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

    // 笔记列表：页面把**访问对象**的主页笔记放在 user.notes 里（实测）
    var cards = [];
    var noteList = arr(pu.notes);
    for (var n = 0; n < noteList.length && n < 300; n++) {
      var card = unwrap(noteList[n]) || {};
      var nc = unwrap(card.noteCard) || {};
      var cu = unwrap(nc.user) || {};
      var cid = card.id || '';
      if (!cid) continue;
      cards.push({
        id: cid,
        token: card.xsecToken || cu.xsecToken || '',
        title: nc.displayTitle || '',
        type: nc.type || '',
        time: card.time || nc.time || 0
      });
    }

    // 就绪判定：页面自己说"主页抓取完成"（userFetchingStatus=resolved）、或已拿到笔记，
    // 且必须有昵称——笔记列表会先于资料到达，只认昵称会得到"有列表没资料"的半成品
    var fetchStatus = String(unwrap(pu.userFetchingStatus) || '');
    if (!nameOut) {
      report({ ready: false, pageUrl: location.href });
      return;
    }
    if (fetchStatus !== 'resolved' && cards.length === 0) {
      report({ ready: false, pageUrl: location.href });
      return;
    }
    var countRaw = unwrap(merged.notes) !== undefined ? unwrap(merged.notes) : unwrap(merged.noteCount);
    var countOut = typeof countRaw === 'number' ? countRaw : 0;
    report({
      ready: true, kind: 'user', pageUrl: location.href,
      uid: urlUid || stateUid,
      nickname: nameOut,
      avatar: avatarOut,
      desc: String(pick(merged, ['desc', 'description']) || '').slice(0, 3000),
      notesCount: countOut,
      notes: cards
    });
    return;
  } catch (e) {
    report({ error: String(e), pageUrl: location.href });
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
    let parsed: tauri::Url = url.parse().map_err(|e| format!("地址无效：{e}"))?;
    window.navigate(parsed).map_err(err)?;

    let started = std::time::Instant::now();
    let mut last_page = String::new();
    while started.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let Some(value) = extract_from_page(&window).await else { continue };
        if let Some(page) = value.get("pageUrl").and_then(|v| v.as_str()) {
            last_page = page.to_string();
        }
        if value.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(value);
        }
        if let Some(e) = value.get("error").and_then(|v| v.as_str()) {
            xhs_core::login::debug_log(&format!("取数脚本异常：{e}"));
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
      location.replace('#xhsdownid=' + Date.now() + '~' + encodeURIComponent(JSON.stringify(out)));
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
                        xhs_core::login::debug_log(&format!("登录检测：{e}"));
                        return Ok(None);
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
    pics: Vec<String>,
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
        let mut pics = Vec::new();
        if let Some(arr) = value.get("images").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(url) = item.as_str() {
                    if !url.is_empty() {
                        pics.push(https(url));
                    }
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

/// 主页卡片
#[derive(Debug, Clone, Default)]
struct CardBundle {
    id: String,
    token: String,
    title: String,
    is_video: bool,
    created_at: i64,
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
                    title: item.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
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
    // 小红书风控会把"同一会话来自另一个客户端"判成劫持，直接把会话作废——
    // 连带把浏览器里的登录态一起杀掉（实测：登录后 9 秒内会话就没了）。
    xhs_core::login::debug_log("笔记：走浏览器会话取数（不使用复制的 Cookie）");
    let value = webview_fetch(app, url).await?;
    NoteBundle::from_web(&value)
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
                if let Ok(bundle) = UserBundle::from_web(&value) {
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
      location.replace('#xhsdownselftest=' + encodeURIComponent(JSON.stringify(out)));
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
  try {
    var out = { url: location.href, title: document.title };
    var st = window.__INITIAL_STATE__ || {};
    var pu = unwrap(st.user) || {};
    out.userFields = fields(pu, 18);
    out.notesLen = ((unwrap(pu.notes) || []).length);
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

    // 探针四：指定链接（标记文件里给的那条），走真实解析路径
    if !probe_url.is_empty() {
        let client = app.state::<AppState>().client();
        match parse_input(&probe_url) {
            Ok(SourceTarget::User { uid, token, url }) => {
                let page_url = user_fetch_url(&uid, &token, &url);
                xhs_core::login::debug_log(&format!(
                    "自检·主页链接：uid={uid} 带token={} 取数地址={page_url}",
                    token.is_some()
                ));
                match fetch_user(&app, &client, &uid, &page_url).await {
                    Ok(user) => xhs_core::login::debug_log(&format!(
                        "自检·主页结果：昵称={:?} uid={:?} 笔记={} 总数={}",
                        user.nickname,
                        user.uid,
                        user.cards.len(),
                        user.notes_count
                    )),
                    Err(e) => xhs_core::login::debug_log(&format!("自检·主页失败：{e}")),
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
    try { location.replace('#xhsdownqr=' + state); } catch (e) {}
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
    location.replace('#xhsdownselftest=' + encodeURIComponent(JSON.stringify(out)));
  } catch (e) {
    location.replace('#xhsdownselftest=' + encodeURIComponent(JSON.stringify({ error: String(e) })));
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
            url: note_page_url(&card.id, &card.token),
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
