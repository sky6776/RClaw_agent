/**
 * WebView 83（vivo 等 Android 10 设备）缺失 API 的定向补丁。
 *
 * vite 的 target "chrome83" 只转译语法、不补运行时 API：react-markdown 依赖链
 * （unified/remark 生态）直接调用 Object.hasOwn（Chrome 93+），缺失时整个
 * React 树启动即崩，表现为整屏白屏。此处仅补依赖实际用到的缺失项；
 * 桌面端（Chromium 120+）已有这些 API，垫片为空操作、零影响。
 */

// Object.hasOwn —— Chrome 93+（react-markdown / hast-util-raw 使用）。
// 项目 tsconfig 的 lib 低于 es2022，须经类型断言引用。
const objectCtor = Object as unknown as {
  hasOwn?: (object: object, property: PropertyKey) => boolean;
};
if (typeof objectCtor.hasOwn !== "function") {
  Object.defineProperty(Object, "hasOwn", {
    value: (object: object, property: PropertyKey) =>
      Object.prototype.hasOwnProperty.call(object, property),
    writable: true,
    configurable: true,
    enumerable: false,
  });
}

// flex 容器 gap 兜底开关 —— flex 布局的 gap 要 Chrome 84+（WebView 83 没有），
// 失效时所有 flex 间距归零（"局促"的主因）。检测到不支持就在 html 上挂
// .no-flex-gap，index.css 据此用相邻兄弟 margin 模拟（桌面探测通过不挂类）。
const gapProbe = document.createElement("div");
gapProbe.style.cssText = "display:flex;gap:8px;position:absolute;visibility:hidden";
const gapA = document.createElement("div");
const gapB = document.createElement("div");
gapProbe.append(gapA, gapB);
// 挂 body 探测：UA 样式表里 head{display:none}，挂 head 的元素不参与布局、
// 两个子 div 的 rect 恒为 0，探测会永远判「不支持」——桌面端也被挂上
// no-flex-gap，兜底 margin 与原生 gap 叠加成全局双倍间距。
document.body.appendChild(gapProbe);
const flexGapOk =
  gapB.getBoundingClientRect().left - gapA.getBoundingClientRect().left >= 8;
gapProbe.remove();
if (!flexGapOk) {
  document.documentElement.classList.add("no-flex-gap");
}
