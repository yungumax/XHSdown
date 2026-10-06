<script setup>
// 登录对话框（小红书版）：账号密码/短信/扫码全部在官方登录页完成，
// 应用只负责开窗与收割 Cookie——没有可裸调的登录接口。
import { onUnmounted, ref, watch } from "vue";
import * as api from "../api";
import Icon from "./Icon.vue";

const props = defineProps({
  login: { type: Object, default: () => ({ logged_in: false }) },
});
const emit = defineEmits(["close", "logout", "web-confirmed"]);

const webBusy = ref(false);
const statusText = ref("");

// 轮询代际：退出登录/关闭弹窗时 +1，旧循环发现代际变了自动终止——
// 否则 webview 里残留的登录 Cookie 会让轮询把用户"重新登录"回去（实测踩过）
const webGen = ref(0);
function cancelWebPoll() {
  webGen.value += 1;
  webBusy.value = false;
  statusText.value = "";
}

// 已登录 → 未登录的瞬间（退出登录）终止进行中的轮询
watch(
  () => props.login.logged_in,
  (on, was) => {
    if (was && !on) cancelWebPoll();
  }
);

// 组件卸载（关闭弹窗）时终止网页登录轮询，避免后台幽灵循环
onUnmounted(cancelWebPoll);

/** 打开官方登录窗口 */
async function openWebLogin() {
  try {
    await api.webLoginOpen();
    statusText.value = "已打开小红书官方登录窗口（独立小窗），登录完成后回到这里点「我已登录，完成绑定」。";
  } catch (error) {
    statusText.value = String(error);
  }
}

/** 检测登录并收割 Cookie：成功即通知主应用 */
async function bindLogin() {
  if (webBusy.value) return;
  webBusy.value = true;
  const gen = ++webGen.value;
  statusText.value = "正在检测登录状态…";
  try {
    await api.webLoginOpen().catch(() => {});
    // 轮询 Cookie：登录成功（web_session 落地）即收割并通知主应用
    for (let i = 0; i < 90; i++) {
      if (gen !== webGen.value) return;
      try {
        const login = await api.webLoginCookies();
        if (gen !== webGen.value) return;
        emit("web-confirmed", login);
        return;
      } catch (error) {
        // 后端 logout 会清数据并关窗：这轮轮询作废
        if (String(error).includes("登录窗口已关闭")) {
          statusText.value = String(error);
          return;
        }
        // 其余错误（还没登录）继续等
        if (i >= 88) statusText.value = String(error);
      }
      await new Promise((r) => setTimeout(r, 2000));
    }
  } finally {
    if (gen === webGen.value) webBusy.value = false;
  }
}

async function closeWebLogin() {
  cancelWebPoll();
  try {
    await api.webLoginClose();
  } catch {}
  emit("close");
}
</script>

<template>
  <div class="backdrop" @click.self="closeWebLogin">
    <div class="dialog pop-in" role="dialog" aria-modal="true">
      <header class="head">
        <h2>{{ login.logged_in ? "账号" : "登录小红书" }}</h2>
        <button class="close" title="关闭" @click="closeWebLogin">✕</button>
      </header>

      <template v-if="login.logged_in">
        <div class="account">
          <img v-if="login.face" class="avatar" :src="login.face" referrerpolicy="no-referrer" alt="" />
          <Icon v-else name="user" class="avatar-fallback" />
          <p class="uname">{{ login.uname }}</p>
          <p class="sub">
            <span class="num">ID {{ login.mid }}</span>
          </p>
        </div>
        <p class="tip">
          已登录状态下可解析笔记、读取自己的主页笔记；部分仅粉丝可见的内容需关注关系。
        </p>
        <div class="actions">
          <button class="ghost danger" @click="emit('logout')">退出登录</button>
          <button class="primary" @click="closeWebLogin">完成</button>
        </div>
      </template>

      <template v-else>
        <div class="web-login">
          <Icon name="lock" class="big-icon" />
          <p class="hint">
            登录在**小红书官方登录页**内完成（独立小窗）——输入手机号/验证码/扫码都可以，
            本应用不接触你的密码。
          </p>
          <p class="hint dim">
            登录成功后回到这里点「我已登录，完成绑定」，应用只保留登录凭据（Cookie），
            保存在本机数据目录里。
          </p>
          <p v-if="statusText" class="status">{{ statusText }}</p>
        </div>
        <div class="actions">
          <button class="ghost" :disabled="webBusy" @click="openWebLogin">
            打开登录窗口
          </button>
          <button class="primary" :disabled="webBusy" @click="bindLogin">
            {{ webBusy ? "检测登录中…" : "我已登录，完成绑定" }}
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
