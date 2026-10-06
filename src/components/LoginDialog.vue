<script setup>
// 登录对话框（小红书版）：打开官方登录页（手机号验证码 / 账号密码 / 扫码都在里面），
// 登录成功后自动检测、自动关闭，并把昵称与头像同步到标题栏。
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import * as api from "../api";
import Icon from "./Icon.vue";

const props = defineProps({
  login: { type: Object, default: () => ({ logged_in: false }) },
});
const emit = defineEmits(["close", "logout", "web-confirmed"]);

const busy = ref(false);
const status = ref("正在打开小红书官方登录窗口…");
const windowOpen = ref(false);

// 轮询代际：退出登录/关闭弹窗时 +1，旧循环发现代际变了自动终止——
// 否则 webview 里残留的登录 Cookie 会让轮询把用户"重新登录"回去（实测踩过）
const gen = ref(0);
let alive = true;

// 已登录 → 未登录的瞬间（退出登录）终止进行中的轮询
watch(
  () => props.login.logged_in,
  (on, was) => {
    if (was && !on) stop();
  }
);

onUnmounted(() => {
  alive = false;
  gen.value += 1;
});

function stop() {
  gen.value += 1;
  busy.value = false;
  status.value = "";
}

/** 打开官方登录窗口（已开着就聚焦） */
async function openWindow() {
  try {
    await api.webLoginOpen();
    windowOpen.value = true;
    return true;
  } catch (error) {
    status.value = `打开登录窗口失败：${error}`;
    return false;
  }
}

/** 轮询登录状态：一旦识别出登录，就自动完成绑定并关闭弹窗 */
async function waitForLogin() {
  if (busy.value) return;
  busy.value = true;
  const mine = ++gen.value;
  status.value = "请在小窗里完成登录（手机号验证码 / 账号密码 / 扫码都可以），登录成功后此处会自动完成绑定。";
  try {
    await openWindow();
    const limit = 150; // 150 × 2s = 5 分钟
    for (let i = 0; i < limit; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      if (!alive || mine !== gen.value) return;
      try {
        const info = await api.webLoginCookies();
        if (!alive || mine !== gen.value) return;
        emit("web-confirmed", info);
        return;
      } catch (error) {
        const text = String(error);
        if (text.includes("登录窗口已关闭")) {
          windowOpen.value = false;
          status.value = "登录窗口已关闭。点下方按钮重新打开，或直接在此关闭。";
          return;
        }
        // 还没登录：继续等（超时才把最后一次的错误显示出来）
        if (i === limit - 1) status.value = text;
      }
    }
    status.value = "等待超时。可以关掉登录窗口重开一次，或点下方「重新打开登录窗口」。";
  } finally {
    if (alive && mine === gen.value) busy.value = false;
  }
}

/** 手动查一次（自动轮询之外的手动兜底） */
async function checkNow() {
  if (busy.value) return;
  const mine = ++gen.value;
  busy.value = true;
  status.value = "正在检测登录状态…";
  try {
    const info = await api.webLoginCookies();
    if (!alive || mine !== gen.value) return;
    emit("web-confirmed", info);
  } catch (error) {
    status.value = String(error);
  } finally {
    if (alive && mine === gen.value) busy.value = false;
  }
}

async function closeAll() {
  stop();
  try {
    await api.webLoginClose();
  } catch {}
  emit("close");
}

const idText = computed(() => {
  const id = props.login.mid || "";
  if (!id) return "";
  return id.length > 16 ? `${id.slice(0, 8)}…${id.slice(-4)}` : id;
});

onMounted(() => {
  if (!props.login.logged_in) waitForLogin();
});
</script>

<template>
  <div class="backdrop" @click.self="closeAll">
    <div class="dialog pop-in" role="dialog" aria-modal="true">
      <header class="head">
        <h2>{{ login.logged_in ? "账号" : "登录小红书" }}</h2>
        <button class="close" title="关闭" @click="closeAll">✕</button>
      </header>

      <template v-if="login.logged_in">
        <div class="account">
          <img v-if="login.face" class="avatar" :src="login.face" referrerpolicy="no-referrer" alt="" />
          <Icon v-else name="user" class="avatar-fallback" />
          <p class="uname">{{ login.uname || "已登录的小红书账号" }}</p>
          <p v-if="idText" class="sub"><span class="num">ID {{ idText }}</span></p>
        </div>
        <p class="tip">
          已登录状态下可解析笔记、读取自己的主页笔记；部分仅粉丝可见的内容需关注关系。
        </p>
        <div class="actions">
          <button class="ghost danger" @click="emit('logout')">退出登录</button>
          <button class="primary" @click="closeAll">完成</button>
        </div>
      </template>

      <template v-else>
        <div class="web-login">
          <Icon name="lock" class="big-icon" />
          <p class="hint">
            已打开**小红书官方登录页**（独立小窗）——手机号验证码、账号密码、扫码都可以，
            本应用不接触你的密码。
          </p>
          <p class="hint dim">
            登录成功后这里会**自动完成绑定**并关闭弹窗，昵称与头像会显示在右上角。
            登录凭据只保存在本机数据目录。
          </p>
          <p v-if="status" class="status">{{ status }}</p>
          <p v-if="busy" class="waiting">
            <span class="dot-pulse"></span>
            正在等待登录…
          </p>
        </div>
        <div class="actions">
          <button class="ghost" @click="openWindow">打开登录窗口</button>
          <button class="primary" :disabled="busy" @click="checkNow">
            {{ busy ? "检测中…" : "我已登录，立即绑定" }}
          </button>
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  display: grid;
  place-items: center;
  background: var(--shade);
  z-index: 20;
}

.dialog {
  width: 356px;
  padding: 20px;
  background: var(--card);
  border-radius: 16px;
  box-shadow: 0 18px 48px rgba(66, 24, 32, 0.18);
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 14px;
}

h2 {
  margin: 0;
  font-size: 15px;
  font-weight: 700;
}

.close {
  width: 30px;
  height: 30px;
  font-size: 12px;
  color: var(--faint);
  border-radius: var(--radius-sm);
}

.close:hover {
  background: var(--raised);
  color: var(--text);
}

.web-login {
  padding: 16px 14px;
  text-align: center;
  background: var(--raised);
  border-radius: var(--radius);
}

.big-icon {
  width: 34px;
  height: 34px;
  color: var(--accent);
}

.hint {
  margin: 10px 0 0;
  font-size: 12.5px;
  line-height: 1.7;
  text-align: left;
  color: var(--muted);
}

.hint.dim {
  color: var(--faint);
  font-size: 11.5px;
}

.status {
  margin: 10px 0 0;
  padding: 8px 10px;
  font-size: 11.5px;
  line-height: 1.6;
  text-align: left;
  color: var(--accent-ink);
  background: var(--accent-soft);
  border-radius: var(--radius-sm);
}

.waiting {
  display: flex;
  align-items: center;
  gap: 7px;
  margin: 9px 0 0;
  font-size: 11.5px;
  color: var(--muted);
}

.dot-pulse {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--accent);
  animation: dot-pulse 1.4s var(--ease-out) infinite;
}

@keyframes dot-pulse {
  50% {
    opacity: 0.3;
    transform: scale(0.85);
  }
}

.account {
  padding: 16px;
  text-align: center;
  background: var(--raised);
  border-radius: var(--radius);
}

.avatar {
  width: 56px;
  height: 56px;
  object-fit: cover;
  background: var(--thumb);
  border: 1px solid var(--line);
  border-radius: 50%;
}

.avatar-fallback {
  width: 40px;
  height: 40px;
  color: var(--accent-ink);
}

.uname {
  margin: 10px 0 0;
  font-size: 16px;
  font-weight: 700;
}

.sub {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 9px;
  margin: 6px 0 0;
  font-size: 12px;
  color: var(--muted);
  word-break: break-all;
}

.tip {
  margin: 12px 0 0;
  font-size: 11.5px;
  line-height: 1.65;
  color: var(--faint);
  text-align: center;
}

.actions {
  display: flex;
  gap: 10px;
  margin-top: 16px;
}

.actions button {
  flex: 1;
}

.primary {
  padding: 9px;
  font-weight: 600;
  color: #fff;
  background: var(--accent);
  border-radius: var(--radius-sm);
}

.primary:hover:not(:disabled) {
  background: var(--accent-dark);
}

.primary:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}

.ghost {
  padding: 9px 14px;
  font-size: 12.5px;
  color: var(--text);
  background: var(--field);
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
}

.ghost:hover:not(:disabled) {
  border-color: var(--line);
  background: var(--raised);
}

.ghost:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}

.ghost.danger:hover {
  color: var(--err);
  border-color: var(--fail-line);
  background: var(--fail-bg);
}
</style>
