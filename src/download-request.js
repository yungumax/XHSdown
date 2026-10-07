// 下载请求与命名变量：解析页与内容库共用同一份实现。
//
// 命名规则是"一个条目叫什么"的唯一出处，两个入口必须算出同一个名字 ——
// 所以 batchNaming / singleNaming 只在这里实现一次，谁也别抄第二份。

import * as api from "./api";

export const KIND_LABELS = {
  post: "单条笔记",
  user: "用户主页",
  feed: "推荐流",
  search: "搜索结果",
};

/** 图片规格选项（唯一定义，设置页与下载弹层共用，避免两处文案漂移）。
 *
 * 实测（抽样多条笔记逐张解码 + 量体积）：站点对笔记图片只放出两个变体，
 * **分辨率完全相同**（1080 宽即网页端上限），差别只在压缩质量：
 * 同图原图 84 KB vs 压缩图 18 KB（约 1/4）。所以只有两档可选，
 * 不存在"更清晰"的第三档——网传的换域名/剥后缀取原图技巧实测全部 403。 */
export const IMAGE_CHOICES = [
  { value: "large", label: "原图（最佳画质，最高 1080 宽）" },
  { value: "thumbnail", label: "压缩图（省空间，约 1/4 体积）" },
];

export function localDate(unixSecs) {
  const d = unixSecs ? new Date(unixSecs * 1000) : new Date();
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** 发布年月：文件夹模板 {year}/{month} 用 */
export function yearMonth(unixSecs) {
  const d = unixSecs ? new Date(unixSecs * 1000) : new Date();
  const pad = (n) => String(n).padStart(2, "0");
  return { year: String(d.getFullYear()), month: pad(d.getMonth() + 1) };
}

/** 单条笔记的命名变量 */
export function singleNaming(probe) {
  const entry = probe.items?.[0] ?? {};
  const created = entry.created_at || probe.created_at || 0;
  const { year, month } = yearMonth(created);
  return {
    author: probe.author || entry.author || "",
    // uid 是字符串字段：这里写 `|| 0` 会把"没有 uid"变成整数 0，
    // 后端 NamingVars.uid 是 String，反序列化直接报 invalid type（实测踩过）
    uid: probe.uid || "",
    bid: probe.bid || entry.bid || "",
    mid: probe.mid || entry.mid || "",
    publish_date: localDate(created),
    date: localDate(),
    year,
    month,
    source_kind: KIND_LABELS[probe.kind] ?? "笔记",
    // 单条也带序号（默认模板含 {index}）：固定 1，补一位不出现"00"
    index: 1,
    index_pad: 1,
  };
}

/** 批量条目的命名变量：主页按"由旧到新"给序号（position 由表格算好传入） */
export function batchNaming(batch, entry, position) {
  const { year, month } = yearMonth(entry.created_at);
  return {
    author: entry.author || batch.author || "",
    // 同上：必须是字符串。网页收集来的来源 uid 为空，`|| 0` 会让整条下载请求被拒
    uid: batch.uid || "",
    bid: entry.bid || "",
    mid: entry.mid || "",
    publish_date: localDate(entry.created_at),
    date: localDate(),
    year,
    month,
    source_kind: KIND_LABELS[batch.kind] ?? "笔记",
    // {index} 的补零宽度按来源总数算：20 条补到 2 位、几千条补到 4 位，
    // 这样目录按名称排序才是 01、02 … 10，而不是 1、10、2。
    // 总数拿不到时用本批条数；来源以后继续拉长，已编号的位数也不会变。
    index_pad: String(Math.max(batch.total || batch.items.length, 1)).length,
    index: position,
  };
}

/** 默认视频清晰度：优先取可用档里的最佳（auto），设置里挑了具体档就用那一档 */
export function pickDefaultQuality(probe, settings) {
  const picked = settings?.video_quality && settings.video_quality !== "auto"
    ? settings.video_quality
    : "auto";
  if (picked !== "auto" && probe.qualities?.some((q) => q.value === picked && q.available)) {
    return picked;
  }
  return "auto";
}

/** 序号列的补零宽度与文件名一致：100 条补 3 位，避免表里 "01" 而磁盘上是 "001" */
export function paddedSeq(row) {
  const probe = row.source.probe;
  const width = String(Math.max(probe.total || probe.items.length, 1)).length;
  return String(row.abs).padStart(Math.max(width, 2), "0");
}
