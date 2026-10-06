# XHSdown 项目规则（对任何代理/会话永久生效）

## 发布规则（用户明确要求）

- **禁止自动发布装机版**：不得在未经用户明确确认的情况下创建 GitHub Release、
  打 tag 触发发布工作流、或对外分发任何安装包。
- 构建（cargo build / tauri build）仅在本地产出，属开发行为，不受此限。
- 用户确认发布后再执行：打 tag → 推送 → 等待 CI 产出安装包 → 核对 Release 内容。
- **签名私钥不入库**：私钥在 `D:\Zcode\_data\tauri-keys\xhsdown.key`；
  CI 用 GitHub Secret `TAURI_SIGNING_PRIVATE_KEY`。丢失私钥 = 旧版本无法接受新更新包。

## 测试版规则

- **每轮代码修改完成后，默认重新构建并打开调试版（测试版）exe** 供用户验收：
  `cmd //c start "" "D:\Zcode\XHSdown\target\debug\xhsdown.exe"`
- 构建顺序：先 `npm run build` 再 `cargo build --features custom-protocol`（反了会嵌旧 dist）。
- 构建环境：先 `source scripts/msvc-env.sh`（便携 MSVC 工具链，见脚本注释；
  本仓库无 `.lldbin` 时自动借用 `D:\Zcode\WeBodown\.lldbin`）。

## 小红书接口的坑（改代码前先读）

- **登录是硬门槛**：2026 起笔记详情页与用户主页对未登录访问直接 302 到
  `/login`（有时是 `/404?error_code=300031`）。没有可裸调的登录接口（都要 x-s 签名），
  登录统一走「内嵌官方登录页」（`web_login_*` 命令）。
- **取数一律借浏览器会话**：小红书会拒绝「复制 Cookie 再用 reqwest 请求」的会话
  （实测：登录窗里 Cookie 齐全，程序自己请求仍被判未登录）。所以解析走隐藏的
  `xhs-fetch` 窗口（与登录窗共用浏览器数据目录），reqwest 只负责下载 CDN 直链
  （那些地址自带签名，不需要 Cookie）。入口：`webview_fetch()` / `fetch_note()` / `fetch_user()`。
- **登录判定**：让浏览器去读 `/user/profile/me` —— 未登录会被 302 到 `/login`
  （`webview_fetch` 因此报「需要登录」），能正常渲染就算已登录（`browser_login_check()`）。
  页面里的 `loggedIn` 字段**对匿名访客也是 true**，不能用来判定（实测踩过）；
  资料字段也可能为空（异步加载），所以昵称/头像再退回 DOM：文档标题即昵称、
  头像图在 `sns-avatar` CDN、页头头像链接 `/user/profile/{uid}` 就是本人 uid。
  实测结果：`nickname=允骨 uid=618c64cd… notes=5`（自己的主页能读到笔记列表）。
- **登录态存在 %APPDATA%\xhsdown\webview**（浏览器数据目录），跨重启保留；
  退出登录时删该目录，删不掉就留 `webview.logout` 标记下次启动清——否则会
  「点了退出登录、重启又自动登录回去」。
- **访客指纹 Cookie**：无 `a1` / `webId` / `xsecappid` 时，连探索页都会被 302。
  `XhsClient::warmup()` 会补一组访客值——这组值只够过首页（reqwest 快路径用）。
- **`__INITIAL_STATE__` 里的字段是 Vue ref，必须解包**：小红书把状态挂在 Vue 的
  `ref` 上（`{__v_isRef:true, _rawValue, _value}`），直接读字段只会拿到包装壳
  ——实测"登录后 userPageData 看着是空对象"就是这个原因，白排查了好几轮。
  `EXTRACT_JS` 里的 `unwrap()` 会逐层剥 `_value`，**所有状态读取都要过它**。
- **登录页二维码不会自动续期**：实测（在真实浏览器里盯了十几分钟）二维码图片从头到尾
  不变，过期后会显示「二维码已过期」（刷新元素是叶子节点
  `<div class="status-desc refresh">点击刷新</div>`，点它确实能换出新码——已验证）。
  另外 **WebView2/Chromium 在窗口被遮挡时会把页面定时器降频（实测前端轮询静默 80+ 秒）**，
  所以"等登录"这件事由**后端** `login_watch` 定时器做（不受降频影响），
  检测到登录后用 `login://confirmed` 事件推给前端。
- **登录页自检开关**：在数据目录放一个空的 `login_selftest` 文件再启动应用，
  它会自动开一个隐藏窗口打开官方登录页，把「二维码图片数 / 是否过期 / 刷新元素 /
  手机号输入框 / 保活脚本是否装上」写进 `xhs_debug.log`（前缀 `自检·注入前/后`），
  探完自动关窗。远程排查登录页问题用这个，别让用户当测试员。
- **xsec_token**：笔记链接必须带 `xsec_token`（App 分享链接自带；用户主页解析出的
  条目由 `note_page_url()` 拼好）。文件：`crates/xhs-core/src/parser.rs`。
- **数据在 `__INITIAL_STATE__`**：笔记详情在 `noteDetailMap[id].note`；
  用户主页在 `user.userPageData` + `user.notes[]`。里面的裸 `undefined` 不是合法 JSON，
  必须替换成 null（替换串写 `${1}null`，`$1null` 会被正则当组名——踩过）。
- **无翻页接口**：用户主页只内嵌最新一批笔记，`probe_more` 恒 `exhausted=true`，
  界面据此隐藏「继续解析」。要更早的内容只能分享具体笔记链接。
- **图片规格**：CDN 地址 `!` 后是缩放指令，去掉即原图（`download::image_variant`）。
- **视频清晰度**：`video.media.stream.h264[].masterUrl`，"1080P / 480P" 这类标签排序
  必须按数值比（字典序下 "480P" > "1080P"，踩过）。

## 其他

- 用户数据/设置在 `%APPDATA%\xhsdown\`；调试日志 `xhs_debug.log` 同目录。
- 前后端契约在 `src-tauri/src/commands.rs` 与 `src/api.js`——两边字段名必须同步改。
- 界面基线来自 WeBodown（一比一），改版式时保持两侧一致；主题色变量在
  `src/styles.css` 的 `--accent*` 一组。
