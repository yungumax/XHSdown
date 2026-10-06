<script setup>
// 首启引导：安装后第一次打开时引导设置保存目录与媒体选项。
// 「开始使用」保存全部设置并置 onboarded=true；「跳过」只置标记不改配置。
import { ref } from "vue";
import Icon from "./Icon.vue";
import * as api from "../api";

const props = defineProps({
  settings: { type: Object, required: true },
});
const emit = defineEmits(["done", "skip"]);

const outputDir = ref(props.settings.output_dir || "");
const imageFormat = ref(props.settings.image_format || "large");
const videoQuality = ref(props.settings.video_quality || "auto");
const downloadText = ref(props.settings.download_text ?? true);
const busy = ref(false);

const IMAGE_CHOICES = [
  { value: "large", label: "原图（最佳画质）" },
  { value: "thumbnail", label: "压缩图（体积小）" },
];
const VIDEO_QUALITIES = [
  { value: "auto", label: "最佳可用" },
  { value: "hd", label: "高清" },
  { value: "sd", label: "标清" },
];

async function chooseDir() {
  const dir = await api.chooseOutputDir().catch(() => "");
  if (dir) outputDir.value = dir;
}

function finish() {
  if (busy.value) return;
  busy.value = true;
  emit("done", {
    ...props.settings,
    output_dir: outputDir.value,
    image_format: imageFormat.value,
    video_quality: videoQuality.value,
    download_text: downloadText.value,
    onboarded: true,
  });
}

function skip() {
  emit("done", { ...props.settings, onboarded: true });
}
</script>

<template>
  <div class="onboard-backdrop">
    <div class="onboard-card pop-in">
      <div class="identity">
        <Icon name="download" class="mark" />
        <div>
          <h1>欢迎使用 XHSdown</h1>
          <p class="sub num">v{{ props.settings.version || "" }}</p>
        </div>
      </div>

      <div class="fields">
        <div class="field full">
          <label>保存目录</label>
          <div class="row-flex">
            <input :value="outputDir" spellcheck="false" readonly />
            <button class="ghost" @click="chooseDir">选择</button>
          </div>
          <p class="note">下载的图片/视频/文案都保存在这里（按作者与年月自动分层）。</p>
        </div>

        <div class="grid2">
          <div class="field">
            <label>图片规格</label>
            <select v-model="imageFormat">
              <option v-for="item in IMAGE_CHOICES" :key="item.value" :value="item.value">
                {{ item.label }}
              </option>
            </select>
          </div>
          <div class="field">
            <label>视频清晰度</label>
            <select v-model="videoQuality">
              <option v-for="item in VIDEO_QUALITIES" :key="item.value" :value="item.value">
                {{ item.label }}
              </option>
            </select>
          </div>
        </div>
        <p class="note">
          图片规格决定多图笔记下载原图还是压缩图；视频清晰度不可用时自动取最佳可用档
          （网页端常见 1080P / 720P）。
        </p>

        <label class="check card-check full">
          <input type="checkbox" v-model="downloadText" />
          <span>下载文案（独立 .txt，与媒体文件放在一起）</span>
        </label>
      </div>

      <div class="actions">
        <button class="primary" :disabled="busy" @click="finish">
          <Icon name="check" />
          开始使用
        </button>
        <button class="ghost" @click="skip">跳过，稍后在设置中配置</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.onboard-backdrop {
  position: fixed;
  inset: 0;
  display: grid;
  place-items: center;
  background: var(--app-bg);
  z-index: 60;
}

.onboard-card {
  width: min(520px, calc(100vw - 60px));
  padding: 24px 26px;
  background: var(--card);
  border: 1px solid var(--line);
  border-radius: var(--radius-lg);
}

.identity {
  display: flex;
  align-items: center;
  gap: 13px;
  margin-bottom: 18px;
}

.mark {
  width: 44px;
  height: 44px;
  color: var(--accent);
}

h1 {
  margin: 0;
  font-size: 21px;
  font-weight: 700;
  letter-spacing: -0.2px;
}

.sub {
  margin: 2px 0 0;
  font-size: 12px;
  color: var(--faint);
}

.fields {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}

.field label {
  font-size: 12.5px;
  font-weight: 600;
}

.grid2 {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 14px;
}

.full {
  width: 100%;
}

.row-flex {
  display: flex;
  gap: 8px;
  align-items: center;
}

.row-flex input {
  flex: 1;
  padding: 8px 11px;
  color: var(--text);
  background: var(--field);
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
  font-size: 13px;
}

input:not([type="checkbox"]),
select {
  padding: 8px 11px;
  color: var(--text);
  background: var(--field);
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
  font-size: 13px;
}

select:focus,
input:focus {
  outline: none;
  border-color: var(--accent-line);
  box-shadow: 0 0 0 3px var(--accent-soft);
}

.note {
  margin: 0;
  font-size: 11.5px;
  color: var(--faint);
  line-height: 1.6;
}

.check {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12.5px;
  cursor: pointer;
  padding: 11px 13px;
  background: var(--raised);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius-sm);
}

.check input {
  flex: none;
}

.actions {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 20px;
}

.actions .ghost,
.actions .primary {
  flex: 1;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

.primary {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 7px;
  padding: 8px 20px;
  font-weight: 600;
  color: #fff;
  background: var(--accent);
  border-radius: var(--radius-sm);
}

.primary:hover {
  background: var(--accent-dark);
}

.primary svg {
  width: 17px;
  height: 17px;
}

.ghost {
  padding: 8px 13px;
  font-size: 12.5px;
  color: var(--muted);
  background: var(--field);
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
}

.ghost:hover {
  color: var(--text);
  border-color: var(--accent-line);
  background: var(--raised);
}
</style>
