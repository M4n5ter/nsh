---
title: 终端
description: 在终端运行 Native Shell 应用，打包控制台程序，并在 Ratatui 应用中嵌入独立的 NSH 界面。
order: 15.75
group: platforms
---

# 终端

Native Shell 使用 Ratatui 和 Crossterm 在 macOS、Windows、Linux 的终端中绘制应用。终端界面背后仍是同一套
TSX 和 Rust 组件、响应式图、Taffy 弹性与网格布局、虚拟列表、网络功能和沙箱。终端应用可以在自己管理的任意
矩形区域中嵌入独立的 `AppView`。

## 运行应用

```sh
nsh run my-app --target tui
```

宿主使用终端的备用屏幕、键盘、鼠标和括号粘贴模式，并在应用关闭时恢复终端。TSX 修改通过图形平台也使用的
开发服务器送达。Ctrl+Q 关闭应用。发行包将宿主放在 `runtime/tui/`；源码检出中的命令会按需构建 `nsh-tui-host`。

运维控制台示例包含侧栏导航、搜索和状态筛选、服务指标、详情检查器、画布图表、活动时间线和部署对话框。
紧凑的 120×34 布局可同时显示七条完整服务记录、指标和图表；160×50 还会显示检查器和活动时间线。更矮的终端
会收起次要内容，为服务列表保留空间：

```sh
nsh run examples/terminal-workbench --target tui
```

宿主也可以直接加载构建好的包：

```sh
nsh build my-app --release -o build
dist/nsh/runtime/tui/nsh-tui-host --bundle build/bundle.nshc --size 120x34
```

`--size` 使用列数和行数。交互会话的视口由终端当前大小决定；脚本则使用指定大小作为初始视口。

## 布局与绘制

NSH 保持以 point 布局。默认情况下，一个终端单元格代表 **8×16 point**，因此 120×40 的终端向 Taffy 提供
960×640 point 的视口。内边距、弹性与网格布局、滚动偏移、图片大小和指针命中测试共用这一坐标系。终端大小
变化会将新视口传给引擎，让应用重新布局。

文本按终端列宽换行：中文和许多 emoji 占两列，组合字符序列保持完整。文本片段保留颜色、粗体、斜体、下划线
和删除线；双向段落按显示顺序排列文本。字体形状与字形大小由终端字体决定。显式行高和字间距会取整为终端的
行与列。

背景、边框和裁剪遵循保留的视图树。交互宿主在启动时探测图像协议和终端单元格的实际像素尺寸。终端提供
图像能力时，图片和记录好的画布绘图优先通过
[`ratatui-image`](https://docs.rs/ratatui-image/11.1.0/ratatui_image/) 使用 Kitty、Sixel 或 iTerm2 图像协议。
编码在图形工作线程中完成。彩色上下半块字符提供通用绘制路径，也用于保持文字、浮层与图形重叠时的合成效果。
终端决定如何将 RGB 颜色映射到自己的调色板。

`NSH_IMAGE_PROTOCOL` 可选 `auto`（默认）、`halfblocks`、`kitty`、`sixel` 或 `iterm2`。`halfblocks` 直接使用
常规文本绘制。选择图像协议时会使用终端报告的像素尺寸。无界面脚本绘制可复现的文本快照。嵌入宿主通过
`AppView::set_image_protocol` 显式选择协议；创建嵌入视图时，终端 IO 由宿主掌控。

现有 [`nsh/ui/mobile`](../components/index.md) 组件通过这些基础元素绘制。为交互行提供至少一个终端行的
高度，使用清晰的颜色对比，并让重要标签在终端列宽内保持可读。

画布路径、渐变、裁剪、变换、合成和图片绘制由工作线程栅格化；画布文本使用经过塑形的系统字体轮廓。普通
`Text` 保持为终端字形。图片支持 PNG、JPEG、GIF 和 WebP，也支持解码器资源上限内的 GIF、WebP 动画。

## 键盘与鼠标

| 按键 | 操作 |
|---|---|
| Tab、Shift+Tab | 在最新模态界面内聚焦下一个或上一个输入框、可按控件或滚动视图 |
| Enter、Space | 激活聚焦的可按控件 |
| Escape | 离开输入框；其他情况下返回或请求关闭模态界面 |
| 方向键、Page Up / Down、Home / End | 滚动聚焦的滚动视图或其可滚动祖先 |
| Ctrl+Q | 关闭终端宿主 |
| Ctrl+C | 复制输入框选区；输入框之外关闭宿主 |

点击可以聚焦或激活控件。鼠标滚轮垂直滚动，Shift+滚轮水平滚动。指针位置从单元格坐标转换为布局使用的同一
虚拟 point 坐标。

输入框支持按字素移动光标和删除、Shift 选择、Ctrl/Alt+方向键按词移动、Home / End、Ctrl+Home / End、全选、
撤销与重做，以及界面实例自己的剪贴板。Ctrl+A/C/X/V 分别全选、复制、剪切和粘贴；Ctrl+Z 与 Ctrl+Shift+Z 或
Ctrl+Y 撤销和重做。安全输入框隐藏内容并保护复制与剪切。输入法通过终端输入已提交的文本；括号粘贴将文本
作为一次编辑输入。Enter 遵循 `submitBehavior`，Shift/Alt+Enter 为多行输入框插入换行。

应用的 [`nsh/keymap`](../modules/nsh-keymap.md) 绑定先于默认处理执行。终端模拟器决定哪些按键和修饰键会传给
应用；请选择目标终端能够转发的快捷键。

可选择文本支持鼠标选择和 Ctrl+A/C，行内文本链接通过指针命中测试激活。原生提示框和操作表使用 Tab 或方向键
选择按钮，Enter 激活按钮。

## 打包

```sh
nsh package my-app --target tui
nsh package my-app --target tui --run
```

控制台运行程序及资源一起分发：

```text
out/<Name>/
  <Name>                 可执行文件；Windows 上为 <Name>.exe
  nsh/bundle.nshc        编译后的 TSX 应用
  nsh/assets/…           导入的资源
  nsh/app.json           应用元数据
  nsh/capabilities.json  配置了能力时的应用能力声明
```

同一目录在 macOS 和 Linux 上归档为 `<Name>.tar.gz`，在 Windows 上归档为 `<Name>.zip`。请在可执行文件将运行的
操作系统上打包，并将 `nsh/` 目录保留在可执行文件旁边。终端运行时使用 Rust 库和操作系统的控制台设施。
`--run` 在当前终端中启动应用，并等待应用关闭。Rust 根应用使用同样的包结构，根组件编译进运行程序。

## 嵌入 Ratatui 应用

`nsh-platform-tui` 提供具有独立引擎、保留界面和输入状态的 `AppView`。宿主管理终端、事件循环及其他 Ratatui
控件。加载包后，轮询提交、推进动画、转发事件，并绘制到分配给界面的区域：

```rust
use nsh_platform_tui::{AppOptions, AppView, Bundle};

let mut view = AppView::new(AppOptions::default());
let code = std::fs::read_to_string("app/bundle.js")?;
view.load(Bundle::source(code).into())?;

// In the host's event loop:
view.poll()?;
view.tick(start.elapsed().as_secs_f64())?;
view.resize(area.width, area.height);
view.render(area, frame.buffer_mut());
```

使用 `view.handle_event(event)` 转发键盘和粘贴事件。转发鼠标事件前，将坐标转换为相对界面区域的位置；仅向
应接收事件的界面转发事件。释放界面或调用 `stop()` 会结束其引擎。

`AppOptions::asset_base` 从资源目录或 HTTP 基地址解析 `asset:/` URI。`AppOptions::limits` 配置虚拟机限制；
嵌入默认使用 `Limits::default()`，独立宿主使用 `Limits::standalone_desktop()`。`AppOptions::metrics` 接受验证过的
`CellMetrics`，供宿主选择其他虚拟 point 比例。参见[沙箱与能力](./sandbox-and-capabilities.md)。

使用[原生组件](./native-components.md)时，实现 `NativeComponent`，并在加载应用前通过
`AppView::register_component(name, factory)` 注册工厂。每个挂载叶子节点都有自己的实例，接收属性和命令，
在裁剪区域内绘制，并可以向 TSX 返回原生事件。宿主可以为地图、浏览器页面、媒体控制等内容提供终端呈现。
内置 `WebView` 和 `Video` 元素显示说明区域，并报告错误事件供应用处理。

## 可复现的交互

宿主的 `--script` 在内存中的 Ratatui 缓冲区上运行，并保存 UTF-8 文本快照。步骤用分号或换行分隔：

```sh
nsh-tui-host --bundle build/bundle.nshc --size 120x40 \
  --script "wait 300; shot home.txt; key Tab; type 你好 👋; resize 100 35; wait 300; shot resized.txt; quit"
```

| 步骤 | 操作 |
|---|---|
| `wait <ms>` | 在指定时间内处理提交与动画 |
| `shot <file.txt>` | 保存当前文本缓冲区 |
| `tap <column> <row>` | 点击单元格 |
| `hover <column> <row>` | 将指针移到单元格 |
| `wheel <column> <row> <dx> <dy>` | 在单元格位置按滚轮刻度滚动 |
| `type <text>` | 将 UTF-8 文本粘贴进聚焦输入框 |
| `key <name>` | 发送按键，例如 `Tab`、`Enter`、`Esc`、`Ctrl+a` 或 `Shift+Left` |
| `selectall` | 全选聚焦输入框的文本 |
| `resize <columns> <rows>` | 改变视口并重新布局 |
| `dark`、`light` | 设置界面的外观 |
| `expect <text>` | 断言快照包含文本 |
| `expect-focus` | 断言某个节点具有键盘焦点 |
| `quit` | 停止应用 |

快照保留字符和布局。颜色与半块图形可在交互终端中查看。

## 另见

- [CLI](./cli.md)
- [打包](./packaging.md)
- [图片与资源](./images-and-assets.md)
- [Canvas](./canvas.md)
- [平台概览](./platforms.md)
