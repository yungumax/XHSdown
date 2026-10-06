# XHSdown

小红书图片 / 视频 / 文案下载工具（Windows 桌面端）。界面一比一沿用
[WeBodown](https://github.com/yungumax/WeBodown) 的设计体系，主题色换成小红书红。

- **技术栈**：Tauri 2 · Rust · Vue 3（前端 Vite 构建）
- **下载能力**：图片原图 / 压缩图 · 视频 mp4 直链（1080P / 720P / 540P）·
  文案 txt · 并发下载 · 暂停与断点续传（`.part` 续传）
- **内容来源**：笔记链接（含 App 分享文本整段粘贴）· 用户主页 · xhslink 短链 ·
  登录后读取自己的主页笔记
- **自动更新**：`tauri-plugin-updater` + GitHub Releases（签名校验，
  端点 `releases/latest/download/latest.json`）

## 使用说明

1. 安装后首次打开进入引导页，设置保存目录与媒体选项。
2. 主界面「解析」页粘贴笔记链接（或 App 的分享文本）→ 开始解析 → 勾选内容 → 下载。
3. 未登录时部分内容会被小红书风控拦下；点右上角「未登录」→「打开登录窗口」，
   在官方登录页完成登录后点「我已登录，完成绑定」即可。

登录凭据只保存在本机数据目录（`%APPDATA%\xhsdown\cookies.json`），不会上传。

## 开发与构建

```bash
npm install                 # 前端依赖
npm run build               # 构建前端到 dist/
# 编译桌面端（需要 MSVC 链接环境；Git Bash 下先 source 便携工具链）
source scripts/msvc-env.sh
cargo build --features custom-protocol          # 调试版 → target/debug/xhsdown.exe
npx tauri build                                  # 发布版 → NSIS 安装包 + 更新包
```

注意构建顺序：**先 `npm run build` 再 `cargo build`**（反了会把旧 dist 嵌进去）。

Windows PowerShell / cmd 用户可直接用 `dev.bat`（自动装配便携 MSVC 链接环境）。

## 发布与自动更新

仓库发布流程（`.github/workflows/release.yml`）：推送 `v*` 标签 →
CI 构建 NSIS 安装包 → 用私钥签名 → 建 Release 并上传
`setup.exe` / `.sig` / `latest.json`。

应用内「设置 · 应用更新」检测的就是该 Release 的 `latest.json`；安装包有签名校验，
密钥不匹配会拒绝安装。

**首次配置（只需一次）**：在 GitHub 仓库 `Settings → Secrets and variables → Actions`
添加 `TAURI_SIGNING_PRIVATE_KEY`，值为本地签名私钥文件内容：
`D:\Zcode\_data\tauri-keys\xhsdown.key`（**该私钥不入库**，丢失后已发布的更新包将无法被旧版本接受）。
公钥已写入 `src-tauri/tauri.conf.json`。

## 项目结构

```
crates/xhs-core/     # 核心库：网页会话、__INITIAL_STATE__ 解析、下载引擎
src-tauri/           # Tauri 壳与命令层（commands.rs 是前后端唯一契约）
src/                 # Vue 3 前端（pages/ 各栏目，components/ 通用件）
scripts/             # 图标生成、便携 MSVC 链接环境、页面结构验证脚本
```

## 免责声明

仅供个人学习与离线收藏使用。请遵守小红书用户协议，不要用于传播或商业用途。
