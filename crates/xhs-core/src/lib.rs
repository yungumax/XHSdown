//! XHSdown 核心库：小红书网页会话、来源解析、下载引擎。
//!
//! 数据全部来自笔记页/用户页 HTML 里的 `window.__INITIAL_STATE__`：
//! 小红书 2026 起对笔记详情与用户主页要求登录态（web_session），
//! 未登录会被 302 到登录页——所以会话要么带登录 Cookie，要么提示登录。

pub mod api;
pub mod client;
pub mod download;
pub mod error;
pub mod login;
pub mod parser;

pub use client::XhsClient;
pub use error::{Result, XhsError};
