//! 真实网络冒烟测试（匿名路径）：
//! 验证 Rust 客户端的请求头 / 访客 Cookie 组装能拿到真实页面并解析出数据。
//!
//! 运行：cargo test -p xhs-core --test network -- --nocapture
//! 说明：探索页是**匿名可见**的入口，用它验证客户端；笔记详情与用户主页
//! 需要登录态（2026 起小红书对未登录访问 302 登录页 / 404 拦截页），
//! 这里只断言「被明确判定为需要登录」而不是解析崩溃。

use xhs_core::XhsClient;
use xhs_core::XhsError;

#[tokio::test]
async fn anonymous_explore_page_parses() {
    let client = XhsClient::new().expect("构建客户端");
    client.warmup().await.expect("预热首页");

    let html = client
        .fetch_text("https://www.xiaohongshu.com/explore", "https://www.xiaohongshu.com/")
        .await
        .expect("抓取探索页");

    let state = XhsClient::parse_initial_state(&html).expect("解析 __INITIAL_STATE__");
    let feeds = state
        .pointer("/feed/feeds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    println!("探索页 feeds: {} 条", feeds.len());
    assert!(!feeds.is_empty(), "探索页应返回笔记卡片");

    let first = &feeds[0];
    let id = first.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    assert_eq!(id.len(), 24, "卡片 id 应为 24 位十六进制：{id}");
    assert!(
        first.pointer("/noteCard/displayTitle").is_some(),
        "卡片应有标题字段"
    );
    // 未登录时 loggedIn 必须为 false（账户检测的权威标志）
    let logged_in = state
        .pointer("/user/loggedIn")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(!logged_in, "匿名会话不应被判为已登录");
    println!(
        "首条：{} / {}",
        id,
        first
            .pointer("/noteCard/displayTitle")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    );
}

#[tokio::test]
async fn note_detail_without_login_reports_need_login() {
    let client = XhsClient::new().expect("构建客户端");
    client.warmup().await.expect("预热首页");

    // 未登录访问笔记详情：应当得到明确的「需要登录」，而不是解析崩溃
    let err = client
        .note_detail("https://www.xiaohongshu.com/explore/6a90eb4f000000001803e1cb")
        .await
        .expect_err("未登录时不应成功");
    println!("未登录访问笔记详情的返回：{err}");
    assert!(
        matches!(err, XhsError::NeedLogin(_)),
        "应判定为需要登录，实际：{err}"
    );
}

#[tokio::test]
async fn user_profile_without_login_reports_need_login() {
    let client = XhsClient::new().expect("构建客户端");
    client.warmup().await.expect("预热首页");

    // 匿名访问用户主页：站点行为不稳定——有时 302 到登录页（NeedLogin），
    // 有时直接给出页面（能解析出昵称）。两种都算正常，这里只要求"不崩、给明确结果"。
    match client.user_profile("58c8014a6a6a690dbb0c84c1").await {
        Ok((info, _notes)) => {
            println!("未登录也能读到主页：昵称={:?} uid={:?}", info.nickname, info.uid);
        }
        Err(e @ XhsError::NeedLogin(_)) => {
            println!("未登录访问用户主页被要求登录：{e}");
        }
        Err(e) => panic!("应是可解析或明确的登录提示，实际：{e}"),
    }
}

#[tokio::test]
async fn account_without_login_is_none() {
    let client = XhsClient::new().expect("构建客户端");
    client.warmup().await.expect("预热首页");
    let account = client.account().await.expect("账户检测不应报错");
    println!("匿名 account(): {account:?}");
    assert!(account.is_none(), "匿名会话不应返回账号");
}
