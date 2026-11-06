# TextLens 开发手册

本手册面向继续开发 TextLens 的贡献者与迁移工作区，涵盖本地开发环境、可迁移源码导出，以及版本演进记录。产品简介、功能说明与安装步骤见 [README.md](./README.md)。

## 本地开发

两个平台共用同一套 renderer、设置、动作、模型请求和流式输出；差异集中在选区捕获、剪贴板、原生窗口与系统集成。依赖目录 `node_modules`、网页构建目录 `dist` 和 Rust 编译目录 `src-tauri/target` 不应提交或放入源码快照。

### Windows

环境要求：

- Windows 10/11 x64
- Visual Studio Build Tools，包含 MSVC x64 C++ 工具链和 Windows SDK
- Rust stable 与 `x86_64-pc-windows-msvc` target
- Node.js 22.12+、pnpm 11+ 和 Microsoft Edge WebView2 Runtime

安装并启动：

    pnpm install
    rustup target add x86_64-pc-windows-msvc
    pnpm dev:windows

`pnpm dev:windows` 会像正式应用一样隐藏启动设置窗口并显示短暂提示。通过通知区打开设置页；测试完整退出时，使用通知区菜单，或将设置窗口关闭行为改为“退出 TextLens”。

常用命令：

    pnpm typecheck                 # 检查 TypeScript
    pnpm test                      # 运行 renderer/shared 单元测试
    pnpm test:rust:windows         # 运行 Windows Rust 测试
    pnpm check:rust:windows        # 检查 Windows Rust 后端
    pnpm verify:windows            # 完整 Windows 自动化验证
    pnpm package:windows           # 构建并核验未签名 NSIS 安装包

### macOS

环境要求：

- Apple Silicon Mac，macOS 12 或更高版本
- Xcode Command Line Tools
- Rust stable，并安装 aarch64-apple-darwin target
- Node.js 22.12+ 与 pnpm 11+

安装并启动：

    pnpm install
    rustup target add aarch64-apple-darwin
    pnpm dev

常用命令：

    pnpm dev              # 启动 arm64 Tauri 开发应用
    pnpm typecheck        # 检查 TypeScript
    pnpm test             # 运行 renderer/shared 单元测试
    pnpm check:rust       # 检查 arm64 Rust 后端
    pnpm verify           # 类型、测试、网页构建和 Rust 检查
    pnpm package:app      # 只构建 arm64 TextLens.app
    pnpm package:dmg      # 构建并核验 arm64 TextLens.app 和 DMG

构建产物：

    src-tauri/target/aarch64-apple-darwin/release/bundle/macos/TextLens.app
    src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/TextLens_<version>_aarch64.dmg

## 导出可迁移开发源码

如需把当前开发环境迁移到另一台机器继续开发，可在仓库根目录执行：

    pnpm export:dev-source

该命令会生成一个新的干净目录：

    portable-dev-sources/TextLens-<version>-dev-source

例如版本 0.3.56 会导出为：

    portable-dev-sources/TextLens-0.3.56-dev-source

也可将上述目录复制到仓库外，作为独立的 `TextLens-<version>-dev-source` 文件夹或压缩包分发；包内只含源码与文档，便于整夹拷贝到其他机器继续开发。

### 导出内容

会保留：

- 共享前端源码 `src/`
- 平台代码与资源 `apps/`、`assets/`
- Rust/Tauri 后端 `src-tauri/`（不含编译产物）
- 脚本、锁文件、配置（包括 `.github/` 与 `deny.toml`）、README（中/英）、本开发手册、许可证与第三方说明

不会包含：

- 已编译安装包或 `release/` 发布快照
- 前端构建产物 `dist/`
- 依赖目录 `node_modules/`
- Rust 编译目录 `src-tauri/target/`
- 嵌套的 `portable-dev-sources/`、缓存、日志和 `.tsbuildinfo`
- `.env` 等环境文件（保留 `.env.example`）、证书/私钥文件和符号链接

导出时会对结果运行密钥扫描，扫描失败会删除未通过验证的导出目录。导出目录没有 `.git` 时，`pnpm check:secrets` 会扫描源码目录中的文件并跳过构建缓存；即使源码目录位于另一个 Git 仓库内，也不会误扫描外层仓库。该扫描只覆盖部分密钥格式，正式分享源码前仍应检查自定义配置是否包含敏感信息。

导出与迁移脚本的独立验证：`node --test scripts/test-portable-dev-source.mjs`。

### 迁移到其他电脑后

1. 把整个 `TextLens-<version>-dev-source` 文件夹（或已重命名的 `TextLens` 工作区）复制到目标机器。
2. 安装 Node.js 22+、pnpm 11+，以及对应平台的 Rust / 平台工具链。
3. 在源码根目录执行：

       pnpm install

4. 验证：

       # Windows
       pnpm verify:windows

       # Apple Silicon macOS
       pnpm verify

5. 开发或打包：

       # Windows 开发
       pnpm dev:windows

       # Windows 安装包
       pnpm package:windows

       # macOS 开发 / 打包
       pnpm dev
       pnpm package

默认设置中只会预置 OpenAI 的 Base URL，API Key 为空；不会把开发机上的密钥或私有配置编译进应用。迁移后的环境与命令说明见本手册「本地开发」与上文迁移步骤。

## 版本演进与开发记录

以下记录根据本项目连续开发需求与当前实现整理。部分小版本是内部迭代，重点记录功能变化，不等同于正式公开发行说明。

TextLens 以 macOS 0.3.16 为功能基线，逐步完成 Windows 10/11 x64 版本并继续迭代。内容覆盖界面变化、设置调整、功能对齐、跨应用选区兼容、窗口生命周期、性能与稳定性修复，以及桌面端构建发布链。

这段开发并不是一次简单的平台移植。macOS 版本依赖 Accessibility API、Event Tap、Core Graphics 和 AppKit；Windows 需要重新处理 UI Automation、Win32 输入 Hook、OLE 剪贴板、WebView2、混合 DPI、窗口激活规则和通知区域生命周期。项目始终保留同一套 React renderer、设置结构、动作服务、模型请求和流式输出，仅在必须依赖操作系统的部分使用平台实现。

### 开发阶段概览

| 阶段 | 版本 | 工作重点 | 结果 |
| --- | --- | --- | --- |
| macOS 稳定基线 | 0.3.16 | 固化划词、动作、结果窗、定位和发布产物 | 形成 Windows 对齐所依据的功能基准 |
| Windows 首次对齐 | 0.3.17 | UIA、剪贴板、Win32 窗口、通知区域、单实例和 NSIS | Windows 10/11 x64 首次具备完整工作链路 |
| 桌面应用化 | 0.3.18–0.3.20 | 主窗口生命周期、退出行为、启动提示、工具栏和结果窗体验 | 从纯通知区域工具演进为有完整设置和退出规则的桌面软件 |
| WebView2 生命周期修复 | 0.3.21–0.3.24 | 动态结果窗创建、显示握手、焦点判断和工具栏收尾 | 解决结果窗不显示、闪烁、显示前关闭和点击即消失等问题 |
| 捕获链路稳定性审计 | 0.3.25–0.3.30 | 原子窗口操作、helper 隔离、Hook 自愈、前台事件状态机、跨应用兼容和共享 renderer 收尾 | 降低延迟，避免 provider 卡死、“隔次划词失败”和忙态视觉回归 |
| 菜单栏与文档收尾 | 0.3.31–0.3.33 | 结果窗关闭按钮、LSUIElement 菜单栏代理、选区兼容、复制反馈、测试与文档同步 | macOS 默认不进入 Dock，并完成一轮发布前稳定性复查 |
| 设置体验与搜索引擎 | 0.3.34 | 搜索引擎列表整合、API Key 显隐、模型拖拽排序、schema v9 迁移 | 设置页更易管理搜索与模型，配置可安全升级 |

### 版本摘要

| 版本 | 主要变化 |
| --- | --- |
| 0.3.2 | API Key 改为应用本地 AES-256-GCM 加密存储，不再访问系统钥匙串；移除划词工具栏外层矩形阴影。 |
| 0.3.3–0.3.4 | 修复工具栏和结果窗复制；压缩结果窗标题栏；重做底部关闭、重试、复制按钮；加入继续提问和多轮上下文，并确定不携带 system 与原始任务提示。 |
| 0.3.5 | 修复设置页动作拖动排序；整理稳定的 macOS 基线；开始 Windows 10/11 x64 适配。 |
| 0.3.6 | 暂停 Windows 发布，集中优化 macOS；工具栏取消默认/上次动作底色；后端与前端加入轻量流式缓冲，改善首字等待和输出流畅度。 |
| 0.3.7–0.3.9 | 优化结果窗等待阶段和滚动条；反复修正工具栏 hover，最终通过 macOS 原生全局指针跟踪实现“无需按下鼠标，移入即高亮、移出即消失”。 |
| 0.3.10 | 清理开发测试调用与正式构建边界，避免本机测试工具路径进入发布行为；继续加固工具栏指针状态隔离。 |
| 0.3.11–0.3.13 | 更新专业翻译与格式修复默认提示词；提示词支持修改和一键重置；结果窗加入按服务商分组的模型切换并支持原窗口重新生成；优化标题栏语言缩写和空间分配。 |
| 0.3.14 | 扩展微信、WPS 等自绘文本区域的选区捕获，加入临时复制并恢复剪贴板的兼容路径。 |
| 0.3.15 | 结果框改为以鼠标为中心定位；安全 Markdown 增加数学公式解析和 KaTeX 渲染。 |
| 0.3.16 | 最终定位调整为相对鼠标中心向右 10%、向下 30%，保留屏幕边缘钳制；整理 Apple Silicon 源码、TextLens.app、DMG 和校验文件。 |
| 0.3.17 | 完成 Windows 10/11 x64 对齐：UI Automation 与剪贴板兼容捕获、完整剪贴板恢复、非激活 hover、混合 DPI、多屏窗口、通知区、单实例、WebView2、NSIS 和 Windows 产物校验。 |
| 0.3.18 | 引入 Windows 任务栏主窗口、可配置的关闭行为、设置页退出入口和有界 UIA/OLE 关闭流程，并修复原生窗口操作的重复投递。 |
| 0.3.19 | 将 AI 动作的结果窗口创建事务移出同步 Tauri IPC 主线程，修复 0.3.18 中点击翻译等动作仍可能一直转圈的问题；增加设置页关闭监听就绪握手，消除启动后立即关闭时的退出请求丢失。 |
| 0.3.20 | Windows 普通启动改为非激活轻提示；修复冷启动首个 AI 结果窗闪烁；结果窗恢复干净圆角、八方向缩放和尺寸记忆；缩小并重新定位划词工具栏；加固 UIA、剪贴板竞争保护和常见自绘应用兼容性。 |
| 0.3.21 | 尝试在 detached 动态结果窗创建后轮询原生 HWND，以缓解过早访问窗口句柄导致的 WebView 消息错误，但该方案未证明 WebView2 renderer 已真正就绪，未能彻底解决现场创建失败；同时将搜索改为异步 Windows Shell 直接打开，通过经验证的父子进程关系支持 Cherry Studio 等 Electron 应用，并将启动提示改为屏幕底部居中。 |
| 0.3.22 | 取消 detached build 后的 HWND 轮询，改为等待结果 renderer 的 prepare IPC，再一次完成原生样式、透明隐藏、定位和尺寸准备，commit 时恢复用户透明度与焦点；Windows 工具栏加入 selection ID 保护的 prepare/present 握手，以最终 DOM 尺寸原子定位并显示，消除旧尺寸闪烁；复用预热的 STA/UIA 通道，缩短选区稳定等待和重试间隔，减少进程树快照与 Runtime ID 去重开销，并确保 UIA COM 接口在 OLE apartment 撤销前完成释放。 |
| 0.3.23 | 修复隐藏结果 WebView 默认取得焦点后触发 blur、导致窗口在 reveal 前被关闭的生命周期竞态；结果窗口的焦点失效关闭仅在 committed 后启用；pending 会话清理返回明确原因，避免把准备阶段的关闭误报为普通创建失败。 |
| 0.3.24 | Windows 结果窗口失焦改为延迟检查真实前台 HWND，WebView2 文档、模型下拉、窗口拖拽和缩放不再被误判为外部失焦；工具栏改为 UI 线程同步原生隐藏，并在 renderer 中销毁已消费选区，避免结果出现后工具栏残留或被旧布局再次显示。 |
| 0.3.25 | 完成 Windows 稳定性与响应速度审计：工具栏 stage/present/hide/resize 在 UI 线程按 selection 原子提交且不跨线程持共享锁；原生 HWND 指针采样取代 60 Hz Tauri 窗口查询；旧 Dismiss 不再清除新选区；UIA 瞬时失败可重试，捕获预算为 worker 回复留出余量，剪贴板事务固定输入 generation 并复用进程快照；renderer 隔离非关键初始化错误、合并并发重试、稳定全局键盘监听并删除首次重复尺寸 IPC；加固 Windows 并发首次创建本地加密密钥。 |
| 0.3.26 | 统一翻译结果标题栏的语言方向字号、字重和中心线；Windows 结果窗继续跳过任务栏，标题栏固定提供置顶和关闭按钮。将第三方 UIA/OLE provider 的不可信捕获迁移到同一程序的隔离 helper 进程，超时、崩溃或协议异常后可终止并重建；增加低级输入 Hook 异常检测和恢复能力，避免捕获资源被卡死的 provider 永久耗尽。 |
| 0.3.27 | 修复 Foreground 事件在鼠标 Down/Up 之间清空有效手势、导致划词隔次失败的问题，并在无害的前台 generation 变化后将 pending capture rebase 到最新 generation；删除每 30 秒盲目重装 Hook 的逻辑，改由 instance token 和线程退出状态只替换失效实例；启动后在后台预热选区 helper，降低首次捕获延迟；彻底删除结果窗最小化 IPC、状态、图标和任务栏恢复链路，保留置顶与关闭。 |
| 0.3.28 | 删除来源应用 HWND/PID 与捕获时前台窗口必须一致的不可靠硬门控，仅在 TextLens 自身仍占前台或暂时没有有效前台窗口时进行有界等待；工具栏对 present、监听和设置 IPC 的瞬时失败进行有限重试，并可重建失效 WebView 后恢复最新选区；补充 Down/Up、前台切换、helper 回传及 renderer 恢复的状态机与事件交付测试。 |
| 0.3.29 | 修复结果窗或搜索切换前台后第一轮划词被错误外部窗口捕获、以及迟到 Foreground 关闭新工具栏的问题；捕获前使用 root/PID/进程族进行软关联等待，保留最近成功 generation 过滤同源迟到事件。Windows UIA 增加 focused element 备选，放宽不可靠祖先密码属性对 UIA 文本的阻断，并为 ChatGPT/Codex、EmEditor 及已验证文本控件启用安全剪贴板回退。 |
| 0.3.30 | 修复 Windows 点击翻译、解释、总结后、结果窗出现前短暂显示系统“🚫”光标的问题；保留现有 busyActionId 单飞锁与 spinner 时序，只在工具栏作用域内覆盖 disabled 光标为中性状态，并为忙态单飞与禁用光标增加前端回归测试。 |
| 0.3.31 | 为结果窗标题栏补齐手动关闭按钮；macOS 和 Windows 的标题栏按钮顺序统一为“置顶、关闭”，避免用户只能依赖失焦或底部关闭入口。 |
| 0.3.32 | 将 macOS 调整为默认不显示在 Dock 的菜单栏代理应用，并完成一轮源码稳定性复查：收敛结果窗延迟加载测试噪音、清理 Apple Silicon 编译警告、同步文档与可迁移源码导出说明。 |
| 0.3.33 | 为 macOS Codex/ChatGPT 输出区增加受限辅助功能与剪贴板兼容回退；复制成功后保留工具栏并临时显示 `clipboard-check`；安全迁移旧标识目录中的模型与加密密钥；模型未配置时提供可操作提示；单一搜索动作默认使用谷歌，搜索引擎可在设置页切换为必应中国版或百度；统一原生桥 TextLens 命名并补充跨平台回归测试。 |
| 0.3.34 | 整合搜索引擎与地址模板（默认 Google / Bing / 百度，支持自定义添加）；API Key 输入支持显示/隐藏；模型列表支持拖动手柄排序；设置 schema 升级至 v9 并兼容迁移旧配置。 |
| 0.3.35 | 搜索引擎改为绑定「搜索」动作（三选一下拉，无模板/自定义）；设置页分区重排（AI 服务商在工具栏动作之前）；已保存 API Key 以黑点显示并支持小眼睛明文回显；设置 schema 升级至 v10。 |
| 0.3.36 | 未配置 AI 模型时点翻译等动作自动打开设置并定位到工具栏动作，短暂提示配置服务商/模型；快捷键触发模式下保持选区监听以支持点工具栏外关闭。 |
| 0.3.37 | 设置顶部提示统一为绿/红、约 2 秒自动消失，悬浮不透明 toast 避免与正文叠字；流式合批与显示平滑参数收紧以改善首字与跟手。 |
| 0.3.38 | 进一步优化翻译/解释/总结等流式首字与跟手：前段立即下发与显示，短增量实时跟上，大块仍平滑；HTTP 开启 TCP_NODELAY。 |
| 0.3.39 | Review 加固与 CodeG 划词兼容：API Key 点击查看、删除旧 IPC_CHANNELS、CodeG 剪贴板回退白名单、macOS changeCount 复查、流式 sessionId/输出上限、取消 legacy session、Markdown 非主键导航抑制、API Key 写入同锁。 |
| 0.3.40 | 流式首字再加速（加宽 SSE 首包与前端即时字符/字素预算）；同步模型思考档位，AI 动作可选手动档位或关闭思考（默认 off），请求注入 `reasoning_effort`。 |
| 0.3.41 | 设置页服务商模型列表改为紧凑 chip（仅名称、一行多个），去掉每模型思考档位徽章；思考档位仍仅在工具栏动作编辑中选择。 |
| 0.3.42 | 复制成功后 Dismiss 强制 hide 并同步前端；流式 SSE/store/播放预算再收紧。 |
| 0.3.43 | 复制成功后抑制同一选区文本的短时重捕获，避免工具栏跳到点击处并需点两次才消失。 |
| 0.3.44 | 复制成功后把键盘焦点还给划词来源应用，避免第一次外点只用于切回窗口。 |
| 0.3.45 | 复制后无感交还焦点（避免窗口闪动）；结果框底部继续提问/重试/复制仅悬停下沿时显示。 |
| 0.3.46 | 翻译/解释等结果窗关闭后抑制原选区重弹工具栏，与复制后一次外点逻辑一致。 |
| 0.3.47 | 关闭结果时尽量取消宿主划词高亮；流式首字近零延迟（document 就绪前置、hydrate 后即时预算、streaming 直通）。 |
| 0.3.48 | 消除翻译流式卡顿：SSE 逐段即时 emit、store 取消 rAF 合并、playback 同步直通（跳过全文 Segmenter）。 |
| 0.3.49 | 同步源码包版本与文档，导出可迁移开发源码快照。 |
| 0.3.50 | 将默认「引用」动作升级为「问AI」：打开结果窗多轮提问（首轮可只带上下文）；多轮 transcript UI；SSE content delta 立即下发、前端同步直通，降低首字延迟；设置 schema 迁移。 |
| 0.3.51 | 结果窗自适应打字机式流式显示（store 仍即时收齐逻辑文本）；thinking/reasoning 增量端到端透传；action flusher 按批 yield 降低 IPC 抖动。 |
| 0.3.52 | 设置页改为左侧导航 + 右侧分区内容；精简翻译/总结/解释默认提示词并同步 Rust 默认值与未改动内置项迁移；结果窗 Markdown 视觉与明暗主题优化，无公式内容跳过 math 插件。 |
| 0.3.53 | 结果关闭后延迟清除宿主选区，清除完成后释放 same-text 抑制，避免下次划词工具栏缺失；结果正文再次划词后，在结果框内点击工具栏外区域可正确隐藏工具栏；服务商模型改为 `list_provider_models` 获取 → 多选合并 → 纵向拖拽排序；设置页与结果窗交互打磨。 |
| 0.3.54 | Windows 划词：allowlist 应用在 `IsPassword` 未知时允许剪贴板回退；WPS/微信/QQ 等进程族 UIA 关联；UIA 无目标时前台 Ctrl+C 路径；短时重试后及时 break。macOS：剪贴板 allowlist 大小写不敏感，拓宽金山/WPS 识别。结果 Markdown：句末单换行段落、中文列表标记与块边界规范化；思考区字号约为结果设置的 0.82 倍。 |
| 0.3.55 | 服务商 `enabled` 启停；动作模型选择改为按服务商分组的统一 optgroup 列表，并按模型名自动推断思考档位（含「关闭思考」）；翻译目标语言扩展为 zh/en/ja/ko/ru/de/fr，结果标题栏短码切换；默认其他语种→中文、中文→英文；收紧翻译与解释默认提示词。 |
| 0.3.56 | 修复 macOS 主线程死锁（持 WindowState 锁等待 orderFront 导致托盘无响应）；结果窗关闭前 blur 原生 select；文档改为 DEVELOPMENT_HANDBOOK 开发手册并精简 README。 |
| 0.4.0 | 双端划词兼容拓宽、AX/UIA 有界捕获、Windows 跨段与长划词回退、工具栏 stage-present 与悬停交互、Markdown 稳定渲染、解释提示词和快捷键注册加固。 |
| 0.4.1 | 补回并升级 macOS 通用剪贴板兼容捕获（denylist 门禁、安全输入检查、合成事件标记、代际可取消、changeCount 复查恢复与迟到认领），修复 WPS 与浏览器 PDF 划词无工具栏及空白处双击误弹；macOS 补齐工具栏 WebView 恢复路径；权限轮询自适应与工具栏设置加载自愈；清理重复命令与死代码。 |
| 0.4.2 | 稳定性与响应加固（阶段 0–6）、设置页七分区重构与设计 token、键盘可达性补齐；与宿主应用共存：剪贴板快照不完整即放弃临时复制（截图内容不再被擦）、点动作后折叠宿主高亮且按宿主应用定位、划词重复弹出改为选区身份判定（残留选区不再被误点反复弹出，双击空白与 Cmd+A 不能复用旧选区）。 |

跨版本累计完成的其他能力包括：多服务商多模型、自定义动作、HTTP 服务地址、URL/IP 直达、结果窗拖动与尺寸记忆、置顶与多种自动关闭方式、结果正文再次划词、安全 Markdown、模型重试与临时切换。

### 0.3.16：macOS 功能基线

0.3.16 是本轮 Windows 开发的参照版本。它已经具备一套完整的划词工作流：

- 在其他应用中拖选或通过快捷键取得文本。
- 弹出不抢焦点的工具栏，执行复制、搜索、翻译、总结、解释、润色和自定义 AI 动作。
- 支持多个 OpenAI-compatible 服务商和模型，每个动作可以独立选择服务商、模型、提示词和图标。
- AI 结果以流式方式输出，支持安全 Markdown、GFM 和 KaTeX 数学公式。
- 结果窗口支持拖动、置顶、自动关闭、复制、重试、继续提问和临时切换模型。
- 结果正文可以再次划词，形成连续工作流。
- API Key 使用应用本地 AES-256-GCM 加密存储。
- 针对微信、WPS 等自绘文本区域提供临时复制兼容路径，并在捕获后恢复原剪贴板。

0.3.16 还确定了结果窗口的跟随鼠标定位规则：先让结果框中心对齐动作点击时的鼠标位置，再向右偏移结果框宽度的 10%，向下偏移高度的 30%，接近屏幕边缘时限制在当前显示器工作区内。

这一版整理了 Apple Silicon 源码、`TextLens.app`、DMG 和 SHA-256 文件。它既是功能基线，也是可回归的 macOS 发布快照。

### 0.3.17：完成 Windows 10/11 x64 首次对齐

0.3.17 建立了 Windows 版本的主体能力。

选区捕获采用 UI Automation 快速路径，并为 UIA 无法读取的自绘应用增加受限的 `Ctrl+C` 兼容捕获。兼容捕获通过 OLE 保存原剪贴板对象，支持空剪贴板和多格式内容；只有确认剪贴板仍属于本次 TextLens 捕获时才恢复，避免覆盖用户在捕获期间主动复制的新内容。

Windows 工具栏使用 `WS_EX_NOACTIVATE + WS_EX_TOOLWINDOW + TOPMOST`，显示和点击时不抢走来源应用焦点。全局鼠标与键盘监听负责触发、悬停和关闭，TextLens 注入的按键带有专属标记，Hook 会忽略这些输入，避免自己取消自己的捕获。

同一版本还完成了：

- 物理桌面像素、WebView2 CSS 像素和混合 DPI 之间的坐标转换。
- 多显示器、负坐标副屏和工作区边缘定位。
- 通知区域菜单、启停划词、打开设置和退出。
- 单实例及重复启动处理。
- Windows WebView2 Runtime 集成。
- current-user NSIS 安装、WebView2 bootstrapper、中英文安装资源和产物校验。

macOS 与 Windows 继续共享 `SelectionPayload`、`SelectionMethod`、动作定义、设置数据、模型请求和 renderer。平台差异被限制在选区、剪贴板、原生窗口和系统集成层。

### 0.3.18：补齐 Windows 桌面应用生命周期

0.3.18 的重点是解决“能运行，但不像完整软件”的问题。

Windows 引入可出现在任务栏的设置主窗口，并增加“关闭主窗口时”设置：

- `hide-to-tray`：默认行为，点击 X 后隐藏到通知区域，后台划词继续工作。
- `quit`：点击 X 后执行完整退出。

设置 schema 升级到 v6，旧设置自动迁移到 `hide-to-tray`，服务商、模型、动作和加密 API Key 保持不变。退出链路开始统一处理新任务禁止、请求取消、快捷键注销、Hook 停止和 UIA/OLE worker 结束。

这一版还修复了原生窗口操作重复投递和 UI 主线程重入风险，并把 UIA/OLE 关闭改为有界流程，避免退出时无限等待。不过，结果窗口创建仍有同步 IPC 主线程阻塞问题，后续版本继续处理。

### 0.3.19：修复 AI 动作一直转圈

0.3.18 的实机测试暴露出翻译、解释和总结点击后一直转圈、结果窗口不显示的问题。根因不是模型请求本身，而是动态结果窗口创建事务仍可能在同步 Tauri IPC 主线程中发生重入等待，消息循环因此无法继续处理 WebView 和退出消息。

0.3.19 将结果窗口创建移出同步 IPC 主线程，使 `run_action` 能快速返回，模型请求继续在异步任务中执行。动作启动失败时会停止工具栏旋转、释放当前选区锁并返回明确错误。

设置窗口也增加关闭监听就绪握手，避免启动后立即关闭时丢失退出请求。

### 0.3.20：启动方式、工具栏和结果窗体验重做

普通启动不再自动打开设置窗口，改为显示非激活轻提示。提示不会进入任务栏或通知中心，短暂停留后自动淡出；重复启动显示“TextLens 已在运行”。后续又将提示位置调整为鼠标所在屏幕底部居中。

设置页删除独立的“退出 TextLens”按钮，退出入口保留在通知区域菜单。通知区域菜单同时移除了“选区访问：可用/不可用”状态项，Windows 选区诊断仍保留在设置页。

保存设置后，“设置已保存”改为一次性提示：短暂保持后淡出并卸载，连续保存只重置同一个提示的计时器；保存失败则保留错误信息。

工具栏在这一版缩小约 10%，并调整为：

- 横向以鼠标位置为中心。
- 顶部与鼠标位置对齐。
- 在当前显示器工作区内保留安全边距，底部空间不足时向上钳制。
- “仅图标”模式使用更紧凑的尺寸。
- 工具栏末尾不再显示 X，依靠外部点击、键盘、滚轮和前台切换关闭。

结果窗口修复了圆角外仍套着直角原生窗口的问题，恢复八方向调整大小和尺寸记忆。冷启动第一次执行翻译、解释或总结时的白帧、方角壳体和闪烁也开始通过隐藏准备、视觉就绪和可见提交处理。

### 0.3.21：第一次修复 WebView2 创建失败，并优化搜索

现场仍出现 `failed to receive message from webview`。0.3.21 曾尝试在 detached 动态结果窗创建后轮询原生 HWND，希望等待窗口句柄可用后再继续配置。这个办法能缓解过早访问句柄，却不能证明 WebView2 renderer 已经真正就绪，因此没有彻底解决创建失败。

这一版的有效改动包括：

- 搜索改为异步 Windows Shell 直接打开，避免同步进程启动造成一到两秒延迟。
- URL、域名和 IP 直接打开，普通文字按用户设置的搜索模板编码后跳转。
- 通过经验证的父子进程关系支持 Cherry Studio 等 Electron 应用。
- 启动提示固定在屏幕底部居中，而不是左下角。

### 0.3.22：用 prepare/commit 取代 HWND 轮询

0.3.22 撤销了未解决根因的 HWND 轮询，改为 renderer 与原生窗口之间的两阶段握手。

结果窗口的流程变为：

1. 创建动态 WebView，但保持原生透明度为 0。
2. renderer 加载设置、会话数据并完成首个 DOM 提交。
3. renderer 发送 prepare/视觉就绪 IPC。
4. 后端一次完成原生样式、定位、尺寸和隐藏准备。
5. renderer 等待渲染帧后 commit，后端恢复用户透明度与焦点。

ready、prepare 和 commit 都校验窗口标签及 session ID，过期或重复调用没有副作用。超时会关闭隐藏窗口、取消请求并清理会话，防止选区锁和旋转状态残留。

工具栏也加入带 selection ID 的 prepare/present 握手。renderer 先得到真实 DOM 尺寸，后端再原子完成定位、缩放和显示，避免使用旧尺寸先闪一下再移动。

选区侧复用预热的 STA/UIA 通道，缩短稳定等待和重试间隔，减少进程树快照及 Runtime ID 去重开销，并确保 UIA COM 对象在 OLE apartment 结束前释放。

### 0.3.23：修复“结果窗口在显示前已关闭”

隐藏准备阶段的 WebView2 可能先获得焦点，随后在原生窗口样式和焦点切换时触发 blur。旧逻辑把这个内部焦点波动当成用户失焦，于是窗口在 reveal 之前就被关闭，并报告“结果窗口在显示前已关闭”。

0.3.23 将结果窗口生命周期分为 pending、prepared、committed 和 failed。只有 committed 后才启用失焦关闭。pending 会话清理返回明确原因，不再把准备阶段的关闭统一误报为普通创建失败。

### 0.3.24：修复点击结果窗即消失和工具栏残留

结果窗口显示后，点击正文、模型下拉、拖拽边缘或调整大小仍可能被 WebView2 的瞬时 blur 误判为外部失焦。0.3.24 不再单凭前端 blur 关闭窗口，而是延迟检查真实前台 HWND，确认用户确实切换到外部应用后才关闭。

工具栏则改为在 UI 线程同步执行原生隐藏，并在 renderer 中销毁已经消费的 selection。这样可以避免动作结果已经出现，工具栏却仍留在屏幕上，或者旧的异步布局回调又把工具栏显示出来。

### 0.3.25：稳定性和响应速度全面审计

0.3.25 对窗口、捕获和 renderer 热路径做了一次系统审查：

- 工具栏 stage、present、hide 和 resize 在 UI 线程按 selection ID 原子提交。
- 跨线程调度期间不再持有共享锁，避免主线程重入死锁。
- 原生 HWND 指针采样取代每秒 60 次的 Tauri 窗口查询，降低 IPC 和锁竞争。
- 同一毫秒内 Selection 优先于 Dismiss，排队的旧 Dismiss 不再清除新选区。
- UIA 瞬时失败可以重试，捕获预算为 helper/worker 回复保留余量。
- 剪贴板事务固定输入 generation，并复用一次进程树快照。
- renderer 隔离非关键初始化错误，合并并发重试，稳定全局键盘监听。
- 删除首次显示时重复提交窗口尺寸的 IPC。
- 加固 Windows 上并发首次创建本地加密密钥的流程。

这轮优化的目标不是增加新按钮，而是减少捕获延迟、窗口闪烁、重复事件和长时间运行后的随机失效。

### 0.3.26：结果标题栏统一，并隔离不可信 UIA/OLE provider

翻译结果标题栏中的源语言、箭头和目标语言统一为相同字号、字重、行高和固定高度。目标语言由普通元素绘制显示文本，并用透明原生 select 覆盖，既保留下拉、键盘和无障碍能力，也避免原生 select 文本基线不一致。标题字号和控件间距同时缩小。

Windows 结果窗口标题栏最终保留“置顶”和“关闭”按钮，结果窗继续跳过任务栏。关闭按钮走统一会话清理链路，窗口空白区域仍可拖动。开发过程中曾加入最小化及任务栏恢复方案，但后来发现它会和自动关闭、置顶、会话替换及流式请求状态产生额外冲突，最终在 0.3.27 完整删除。

这一版更重要的后端变化是把 UIA/OLE 捕获从主进程线程迁移到内部 helper 进程。helper 仍使用同一个可执行文件，通过私有启动参数进入捕获模式，因此安装包不增加第二个程序。主进程通过继承的匿名管道与 helper 通信，不开放端口。

第三方 UIA provider 如果永久卡死，主进程可以终止并重建 helper，不再让不可回收的 COM 线程占满捕获槽位。协议包含版本、长度、request ID 和 generation；超时、崩溃、协议损坏及过期回复都能被识别。Hook 也增加异常退出检测和恢复能力。

### 0.3.27：第一次集中修复“隔次划词失败”

测试中出现“第一次成功，下一次必然失败，再下一次恢复”的规律。一个根因是 Foreground 事件可能在鼠标 Down/Up 之间到达，旧状态机会把正常的应用激活变化当成外部切换，提前清空有效手势。

0.3.27 保留跨 root 窗口变化的外部鼠标手势，并在无害的前台 generation 变化后把 pending capture rebase 到最新 generation。新鼠标或键盘输入仍然会取消旧捕获，避免返回过期文本。

Hook 自愈也从每 30 秒盲目重装改为 instance token 和线程退出状态驱动。只有当前 Hook 被确认失效或替换时才安装新实例，防止多个监听器并存后制造重复事件。启动后在后台预热 helper，降低第一次划词的进程启动开销。

同一版本彻底删除结果窗口最小化 IPC、状态、图标和任务栏恢复代码，当前 Windows 结果窗只保留置顶和关闭。

### 0.3.28：撤销过严的来源窗口硬门控

0.3.27 后仍出现完全不弹工具栏的问题。审查发现，捕获前新增的来源 HWND/PID 硬匹配要求鼠标按下时的窗口和捕获时前台窗口完全一致。这个条件在 Electron 壳窗口、浏览器 renderer、多进程子窗口和窗口重建场景中并不成立，500 ms 后选区会被静默删除，helper 和工具栏都收不到事件。

0.3.28 删除该硬门控。只有 TextLens 自身仍占前台或暂时没有有效前台窗口时才进行有界等待；正常外部应用可以快速进入 helper，最终安全性继续由 HWND、PID、generation、进程树、密码控件、UIPI 和指针边界校验保证。

工具栏对设置读取、事件监听和 present IPC 的瞬时失败增加有限重试。如果 singleton WebView2 renderer 已经失效，可以重建工具栏；新 renderer 通过 `toolbar_ready` 取回内存中的最新 selection，而不是永久失去后续工具栏。

### 0.3.29：修复结果或搜索后的首轮捕获失败

0.3.28 放宽硬门控后，又暴露出另一个方向的问题：AI 结果窗或搜索会改变前台应用。用户关闭结果窗或从浏览器回到原应用后立即拖选时，鼠标目标已经是应用 A，但 Windows 报告的前台窗口可能短暂仍是应用 B。旧逻辑只要看到任意外部前台就立即捕获，于是 helper 对 B 执行 UIA，元素却来自 A，安全校验失败。第一次手势虽然丢失，却完成了 A 的激活，所以下一次又会成功。复制动作不改变前台，因此不会触发这个规律。

0.3.29 引入“软关联等待”：

- 优先比较鼠标目标 root HWND 和当前前台 root HWND。
- 接受相同 PID，以及明确的父子进程关系。
- 对明显无关的旧前台短暂等待，相关外部窗口出现后立即捕获。
- 不恢复 exact HWND 的硬匹配，Electron renderer 和浏览器多进程窗口仍可工作。
- 进程树快照按一次手势缓存，避免每 8 ms 重试都重新枚举系统进程。

helper 等待期间排队的 Foreground 事件还可能在 SelectionEvent 发出后到达，并错误发送 Dismiss。0.3.29 保存最近成功捕获的 generation 和来源上下文，忽略迟到的同源事件，避免新工具栏刚出现就被清除。

跨应用兼容路径继续扩展：

- 鼠标捕获先尝试 `ElementFromPoint`，没有文本时再尝试 focused element。
- Control View 和 Raw View 祖先用于查找 TextPattern。
- 明确 `IsPassword=true` 仍然拒绝；无法可靠读取祖先密码属性（`None`）时，UIA 文本仍可读取，但合成 Ctrl+C 仅对已列入复制兼容白名单的应用放行，其他应用一律拒绝——未知密码状态不得静默开启剪贴板路径。
- UIA 元素与前台应用的关系允许父子进程的两个方向，并为同一可执行文件的 renderer sibling 提供受限支持。
- UIA 已得到有效文本但矩形不可靠时，丢弃错误边界并使用鼠标释放位置显示工具栏，不再丢弃文本。
- ChatGPT/Codex、EmEditor、常见浏览器和明确的 Text、Document、Edit、Hyperlink 控件加入安全剪贴板兼容判断。

这些 profile 表示代码已经具备兼容路径，不等于所有应用、版本和权限组合都已完成实机认证。

### 0.3.30：修复 Windows 工具栏忙态短暂显示“🚫”光标

0.3.29 之后，结果窗口创建链路已经恢复正常，但用户点击翻译、解释或总结后，结果框真正出现前仍会短暂看到 Windows 系统的“禁止”光标。审查确认根因不在后端 IPC、结果窗口握手或 WebView2，而在共享 toolbar renderer 的忙态实现：

- 为了防止重复点击，工具栏会在动作启动后立刻把按钮设为 `disabled`。
- 全局表单样式把所有禁用控件的光标统一设为 `not-allowed`。
- 工具栏在结果窗口 reveal 完成前会短暂停留在原位置，因此 Windows 会渲染系统“🚫”光标。

0.3.30 保留现有 `busyActionId` 单飞锁、spinner 和隐藏时序，只在工具栏作用域内覆盖禁用控件光标为中性状态，不影响设置页等真正需要 `not-allowed` 反馈的表单。这样既不改变动作接口，也不触碰 Windows 捕获、结果窗口或 helper 的原生链路。

同一轮改动还增加了两项前端回归测试：

- 忙态期间只能有一个 AI 动作在飞，不能重复触发。
- 工具栏禁用控件不再继承全局 `not-allowed` 光标。

共享源码、Tauri/Cargo/package 版本在本轮统一升级为 0.3.30，并为后续 0.3.31–0.3.32 的 macOS 标题栏、菜单栏代理与文档收尾提供共同基线。

### 0.3.31：补齐结果窗标题栏手动关闭按钮

0.3.31 回到共享结果窗口 renderer，补齐了一个影响日常可控性的交互缺口。此前 macOS 结果窗主要依赖失焦关闭、鼠标移出延迟关闭，或在手动关闭模式下使用底部关闭入口；当用户希望在标题栏就直接结束本次结果时，操作路径不够直观。

这一版将标题栏按钮统一为“置顶、关闭”：

- macOS 与 Windows 都在置顶按钮右侧提供 `x` 关闭按钮。
- 关闭动作继续走统一的会话清理链路，不额外分叉结果状态。
- 手动关闭模式下底部关闭入口仍保留，兼顾键盘与长文滚动场景。

这次调整不触碰模型请求、窗口握手和原生捕获链路，重点是补齐可预测的手动退出路径，减少用户只能依赖自动关闭策略的情况。

### 0.3.32：切换为默认不显示 Dock 的菜单栏代理，并完成稳定性复查

0.3.32 聚焦在 macOS 应用形态与收尾质量。

首先，应用明确切换为菜单栏代理：

- `Info.plist` 设置 `LSUIElement=true`，默认不在 Dock 显示。
- 运行时继续保持 `ActivationPolicy::Accessory`，确保应用以菜单栏工具形态常驻。
- 打包校验脚本同步把 `LSUIElement=true` 作为 macOS 产物验收条件，避免回归成普通 Dock 应用。

其次，这一版对共享源码做了稳定性复查并修正了几处收尾问题：

- 结果窗测试等待延迟 Markdown 资源稳定后再做选区交互，消除了 React `act(...)` 噪音，避免测试通过但日志持续报警。
- Apple Silicon `cargo check` 中仅在非 Windows 目标出现的未使用导入、未使用参数和死代码警告被清理，减少真正问题被告警淹没的风险。
- README、可迁移源码导出说明和开发记录全部同步到 0.3.32，并把导出文档中的旧 `artifacts/` 表述更新为当前使用的 `release/`。

0.3.32 的价值不在新增大功能，而在于把 macOS 菜单栏工具的定位、测试信号和发布文档统一到一致状态，为后续继续迭代提供更干净的基线。

### 0.3.33：增强 Codex/ChatGPT 兼容与复制成功反馈

0.3.33 聚焦在跨应用选区兼容和工具栏操作反馈。

- macOS 继续优先使用 Accessibility API；只有 AX 无法读取、应用属于受限的 Codex/ChatGPT profile、应用仍在前台且焦点控件不是受保护内容时，才进入现有临时复制回退。
- 普通应用的事件 tap、AX 遍历和 generation 状态机没有增加轮询或固定等待；OpenAI 自绘输出区复用既有剪贴板完整快照、变化检测与安全恢复路径。
- 复制动作成功后不再隐藏工具栏，原复制图标临时切换为 `clipboard-check`，1.5 秒后恢复；复制失败、新选区和退出工具栏会清理成功状态，搜索及 AI 动作保持原有隐藏时序。
- 产品标识从历史名称迁移到 `com.local.textlens` 后，旧目录中的服务商、模型绑定和 API Key 曾未被读取，表现为复制与搜索可用、AI 动作无法创建结果窗。启动初始化现会在新配置仍为默认空白状态时一次性迁移旧设置，并将旧密钥解密后使用 TextLens AAD 重新加密；已配置的新目录不会被覆盖。
- AI 动作缺少服务商、模型或 API Key 时，工具栏显示带图标的双行配置提示，并提供“打开设置”按钮；普通网络和执行错误仍保留原错误信息。
- 设置格式保持 v8，工具栏保留一个搜索动作，默认搜索引擎为谷歌；搜索引擎在设置页持久化选择，可切换为必应中国版和百度，旧 v7 的额外搜索动作会安全收敛为该单一入口。
- macOS 原生桥的活动 C ABI 从早期 `SB`/`sb_` 命名统一为 `TextLens`/`textlens_`。历史加密 AAD 字面值继续保留，只用于读取旧版本 API Key。
- Windows 继续使用现有 UIA、focused element、helper 隔离和安全剪贴板回退，并补充 `ChatGPT.exe`、`codex.exe` 及 OpenAI.Codex 安装路径的回归测试。

本轮没有采用可能增加热路径堆分配的大 enum 装箱等机械优化，优先保持已经验证的响应速度和捕获状态机边界。

### 界面演进

#### 启动、设置窗口和通知区域

Windows 版本最初以通知区域为主要入口。0.3.18 曾让设置窗口成为普通启动时显示的任务栏主窗口，以建立清晰的桌面应用生命周期。根据后续使用反馈，0.3.20 又改为普通启动只显示短暂轻提示，设置窗口由通知区域的“打开设置”进入。

当前行为如下：

- 普通启动立即启用后台划词，但不自动打开设置。
- 启动提示位于当前屏幕底部居中，保持后淡出，不抢焦点。
- 重复启动由单实例逻辑接管，只提示 TextLens 已在运行。
- 设置窗口可以最小化和调整大小，关闭 X 的行为由 Windows 专属设置决定。
- 默认关闭设置窗口时隐藏到通知区域，也可以改为退出 TextLens。
- 设置页不再提供单独的退出按钮，通知区域菜单保留退出入口。
- 最小化设置窗口不停止后台划词。

#### 划词工具栏

Windows 工具栏始终按工具窗口处理，不进入任务栏，也不激活来源应用之外的焦点。它经历了几轮明显调整：

- 从较宽的图标加文字布局缩小约 10%。
- 增加“图标和文字”与“仅图标”两种显示方式，设置页实时预览同步尺寸。
- 横向以鼠标为中心，顶部对齐鼠标位置，靠近屏幕边缘时钳制。
- 删除工具栏末尾 X，依靠外部输入和来源应用前台变化关闭。
- 使用原生指针采样实现“移入动作才高亮”，避免非激活 WebView2 hover 不可靠。
- 通过 selection ID 保护 prepare、present、resize 和 hide，旧异步任务不能覆盖新选区。
- renderer 或 IPC 瞬时失败时有限重试，必要时重建 singleton WebView。

#### 结果窗口

结果窗口的最终外观和交互经过多次修正：

- 透明原生窗口与圆角 WebView 配合，去除圆角外的直角背景。
- 支持拖动和八方向调整大小。
- 可以记住用户调整后的逻辑尺寸，下次沿用，也可以在设置页重置尺寸。
- 支持跟随鼠标、默认置顶、窗口透明度、结果字体大小和三种关闭方式。
- 标题栏固定显示置顶和关闭；Windows 不提供最小化，且跳过任务栏。
- 翻译语言方向使用统一的小字号、字重、行高和中心线。
- 模型下拉、正文交互、拖拽和 resize 不会再触发错误的失焦关闭。
- 结果窗口在 renderer 完成首帧后才提交可见，减少冷启动白帧和窗口壳闪烁。
- 结果正文仍可再次划词；置顶窗口不会被新的外部选区自动关闭。

### 设置变化

Windows 适配没有另起一套设置文件。现有 macOS 服务商、模型、动作、提示词和加密 API Key 可以直接沿用。当前设置 schema 为 v13，新增能力通过原字段扩展、默认值和迁移阶梯完成。写入使用临时文件 + 原子替换，替换前尽力保留上一份 `settings.json.bak`；反序列化有意忽略未知字段，以便旧版本回退时不会因新版字段而拒绝整份配置，已知字段仍经过 normalize/validate。

| 设置区域 | 当前能力 | 开发过程中的变化 |
| --- | --- | --- |
| 基础开关 | 启用或停用划词助手 | 通知区域和设置页保持一致 |
| 触发方式 | 划词自动触发或全局快捷键触发 | Windows 增加原生全局快捷键注册与诊断 |
| 工具栏显示 | 图标和文字、仅图标 | 0.3.20 缩小尺寸并同步实时预览 |
| 关闭主窗口时 | 隐藏到通知区域、退出 TextLens | 0.3.18 加入，v5 设置迁移后默认隐藏 |
| 搜索地址 | 自定义包含 `{{text}}` 的 URL 模板 | Windows 搜索后来改为异步 Shell 打开，URL/域名/IP 可直接访问 |
| 应用过滤 | 全部、白名单、黑名单 | Windows 按可执行文件名匹配，macOS 按应用名或 Bundle ID 匹配 |
| 服务商与模型 | 多个 OpenAI-compatible 服务、同步或手动添加模型 | API Key 继续单独加密，不通过公开 settings IPC 返回 |
| 翻译语言 | 默认语言与翻译目标可配置，其余语种译为翻译目标 | 结果标题栏可以临时切换目标语言并重新生成 |
| 结果窗口 | 跟随鼠标、记住尺寸、默认置顶、字体、透明度和关闭方式 | Windows 原生窗口在 prepare/commit 后应用这些设置 |
| 工具栏动作 | 启用、停用、拖拽排序、自定义提示词和模型 | 动作与 renderer 在两平台共享，不为 Windows 分叉 |

保存成功提示在 0.3.20 后改为 1.5 秒内完成显示和淡出；连续保存不会堆叠多个提示。保存错误保留在页面中，便于用户处理。

### Windows 选区捕获与兼容性

#### 捕获顺序

0.3.29 的自动捕获链路可以概括为：

1. 低级鼠标/键盘 Hook 记录原始输入序号、generation、坐标和来源窗口。
2. 主进程等待必要的前台窗口关联，但不使用 exact HWND 硬门控。
3. 内部 helper 初始化 STA、UIA 和 OLE。
4. 鼠标触发先从指针位置取 UIA element，必要时尝试 focused element；键盘触发优先 focused element。
5. 沿 Control View 和 Raw View 祖先查找 TextPattern，读取文本和可用边界。
6. UIA 不支持时，只对明确安全的应用或文本控件尝试临时 `Ctrl+C`。
7. 检查前台窗口、PID/进程族、generation、UIPI、密码属性和用户输入竞争。
8. 返回 `SelectionMethod::Accessibility` 或 `SelectionMethod::Clipboard`，两种结果进入同一 renderer 接口。

#### 安全边界

兼容性增强没有取消以下限制：

- TextLens 不读取明确的密码控件。
- 密码状态无法确认时，不使用自动剪贴板复制。
- 标准权限 TextLens 不读取管理员权限应用中的选区。
- Alt/Win 组合键、UIPI 注入失败和前台应用不匹配时不注入 `Ctrl+C`。
- Terminal、PowerShell、cmd、密码管理器和远程控制程序不启用自动复制回退。
- 捕获期间发生真实用户输入、前台切换或无法归因的剪贴板变化时放弃本次事务。
- 只有剪贴板仍由本次 TextLens 捕获修改时才恢复原始多格式内容。
- 剪贴板携带位图类格式（CF_BITMAP/CF_DIB/CF_TIFF/CF_ENHMETAFILE 等）而快照未能物化其中任一可读格式时，整体放弃本次回退：`EmptyClipboard` 之后的恢复只会写回已捕获的格式，未被捕获的截图位图不可逆丢失。Windows 恢复不追求逐格式等价——已捕获 CF_DIB 时系统会按需重建 CF_BITMAP，因此单独的句柄类格式缺失不算丢失。
- 选中文本最多读取 1,000,000 个字符，避免异常 provider 导致无界分配。

#### 兼容范围

已建立兼容 profile 或 UIA 支持路径的应用类别包括：

- Windows 原生编辑器和常见 Text/Edit/Document 控件。
- Edge、Chrome、Firefox 等浏览器。
- Office、WPS 和常见 PDF 阅读器。
- 微信、企业微信、QQ、飞书、钉钉。
- Teams、Slack、Telegram、Notion、Obsidian。
- VS Code、Cursor、Cherry Studio、ChatGPT/Codex 等 Electron/WebView2 应用。
- EmEditor、Notepad++、Sublime Text、Typora 等编辑器。

具体应用是否可用仍取决于版本、渲染方式、完整性级别和 UIA provider 质量。未完成实机测试的应用不能标为已认证。

### 关键 Bug 与根因

| 现象 | 根因 | 最终处理 |
| --- | --- | --- |
| 翻译、解释、总结一直转圈 | 同步 IPC 中创建窗口导致 UI 主线程重入等待 | 0.3.19 将窗口创建移出同步 IPC，失败时释放旋转和选区状态 |
| TextLens 无法正常退出 | UI 线程等待 Hook/UIA/OLE worker 无界 join | 统一幂等退出控制器并使用有界关闭 |
| 冷启动首个结果窗闪烁 | WebView2 首帧、原生透明度和窗口样式提交时机不一致 | 0.3.22 使用 prepare/commit 两阶段显示 |
| `failed to receive message from webview` | 仅有 HWND 不代表 renderer IPC 已就绪 | 撤销 HWND 轮询，等待 renderer 主动握手 |
| 提示“结果窗口在显示前已关闭” | 隐藏 WebView 的内部焦点变化触发 blur 关闭 | 只有 committed 后启用失焦关闭 |
| 结果窗点击、切换模型或 resize 后消失 | WebView2 瞬时 blur 被当成外部应用切换 | 延迟核验真实前台 HWND |
| 结果窗出现后工具栏不消失 | hide 异步任务与旧 renderer 布局回调竞争 | UI 线程同步隐藏并消费 selection ID |
| 工具栏偶发闪烁或旧位置跳动 | 显示时使用估算尺寸，随后 renderer 再上报真实尺寸 | prepare/present 以最终 DOM 尺寸原子提交 |
| 使用一段时间后捕获永久失效 | 第三方 UIA/OLE provider 卡死并耗尽进程内线程槽位 | 0.3.26 将捕获迁到可终止、可重建 helper 进程 |
| 划词隔次成功、隔次失败 | Foreground 事件在 Down/Up 或 helper 等待期间错误清空手势 | 保存 pending/recent capture 上下文并过滤迟到同源事件 |
| 0.3.28 完全不弹工具栏 | exact HWND/PID 硬门控误杀 Electron 和跨进程窗口 | 删除硬门控，保留 helper 终检 |
| 结果或搜索后的下一次划词必然失败 | 鼠标已进入新应用，但 Windows 前台仍短暂指向旧应用 | 0.3.29 使用窗口/进程族软关联等待 |
| 浏览器、ChatGPT/Codex、EmEditor 无法捕获 | point UIA、进程关系、密码属性和边界检查过严，且缺少兼容 profile | 增加 focused 备选、双向进程关系、安全 fallback 和边界降级 |
| 搜索延迟一到两秒 | 同步启动浏览器阻塞动作链路 | 使用异步 Windows Shell 调度 |

### 保留、撤销和替换的方案

开发过程中特别需要记录几项曾经实现或讨论、但最终没有保留的设计：

- 普通启动自动打开设置窗口：0.3.18 引入，0.3.20 改为后台启动加轻提示。
- 设置页“退出 TextLens”按钮：早期桌面应用化阶段加入，后按使用反馈删除，退出由通知区域菜单负责。
- 工具栏关闭 X：缩小工具栏时删除，当前通过全局输入和前台变化关闭。
- 结果窗口最小化：曾实现标准任务栏最小化和恢复，0.3.27 删除所有相关 IPC、状态和界面。
- detached 窗口创建后的 HWND 轮询：0.3.21 尝试，因不能证明 WebView2 renderer 就绪而在 0.3.22 被 prepare/commit 替代。
- 每 30 秒重装 Hook：会制造重叠监听和重复事件，0.3.27 改为 instance token 驱动的精确替换。
- 来源窗口 exact HWND/PID 硬匹配：会误杀多进程 UI，0.3.28 删除；0.3.29 使用进程族软关联和 helper 终检取代。

这些撤销说明 Windows 的可靠性不能只靠增加等待时间或更严格地匹配窗口。WebView2、Electron 和 UI Automation 都有异步、多进程和第三方 provider 行为，最终方案必须允许短暂的不一致，同时用 generation、session ID 和安全边界拒绝真正过期或越权的结果。

### 架构复用情况

从 0.3.16 到 0.3.38，项目没有复制一套 Windows 前端。两平台继续共享：

- React 设置页、工具栏和结果 renderer。
- Zod/TypeScript 设置 schema 和 IPC 数据结构。
- 动作管理、自定义提示词、服务商与模型配置。
- OpenAI-compatible 请求、SSE 流式解析、取消和重试。
- Markdown、GFM、KaTeX 和安全链接处理。
- 继续提问、会话状态和模型临时切换。
- AES-256-GCM 本地密钥及 API Key 存储格式。

平台层分别负责：

| 范围 | macOS | Windows |
| --- | --- | --- |
| 选区监听 | Accessibility、Event Tap | UI Automation、低级输入 Hook、内部 helper |
| 兼容捕获 | AppKit/Accessibility 临时复制 | OLE 剪贴板事务、带标记的 `SendInput` |
| 窗口 | AppKit/WKWebView 原生行为 | Win32/WebView2、NOACTIVATE、物理像素和 DPI 转换 |
| 系统入口 | 菜单栏 | 通知区域、单实例和 Windows 启动提示 |
| 安装发布 | Apple Silicon `.app`/DMG | x64 current-user NSIS |

### 构建、测试和发布链

Windows 构建目标是 `x86_64-pc-windows-msvc`，要求 MSVC x64 Build Tools、Windows SDK、Rust stable、Node.js、pnpm 和 WebView2 Runtime。

项目建立了以下验证链：

- TypeScript 配置和 renderer 类型检查。
- Vitest renderer/shared 单元测试。
- Vite 生产构建。
- Windows Rust 全目标单元测试和 `cargo check --locked`。
- NSIS 安装包、PE 架构、版本号、安装模式、图标和唯一产物检查。
- SHA-256 校验文件生成。

0.3.34 在既有验证链上保留 0.3.33 的复制状态、OpenAI 桌面应用 profile、旧配置与密钥迁移、模型缺失提示和区域搜索路由回归，并补充搜索引擎列表迁移、模板解析、API Key 显隐与模型拖拽排序相关测试。Apple Silicon Rust、原生 Objective-C++ 和 macOS 实机兼容仍需在 macOS 主机执行最终验收。

Windows 安装包使用 current-user 模式，不要求管理员权限；禁止降级，缺少 WebView2 时通过 bootstrapper 安装，并包含简体中文和英文安装资源。当前安装包没有 Authenticode 签名，可能触发 Microsoft Defender SmartScreen。

### 0.3.34：搜索引擎整合、API Key 显隐与模型拖拽排序

0.3.34 聚焦设置页可管理性与配置演进。

- 将原先独立的「搜索引擎」与「搜索地址模板」整合为可管理列表：默认 Google、Bing（展示名，内置 id 仍为 `bing-china`，模板继续使用 `cn.bing.com`）、百度；内置引擎模板可编辑并可「恢复默认」；支持添加/删除自定义引擎，删除当前激活引擎时回落到 Google。
- 设置 schema 从 v8 升级到 v9：旧 `searchEngine` / `searchTemplate` 迁移为 `searchEngines` + `activeSearchEngineId`；旧展示名「必应中国版」规范为 Bing。
- AI 服务商 API Key 输入框右侧提供显示/隐藏（小眼睛），仅对当前输入草稿生效；已保存密钥仍不回传、不可回显。
- 模型列表支持拖动手柄调整顺序，顺序写入 `provider.models` 并随设置持久化；与工具栏动作拖拽使用独立上下文。
- 产品版本统一为 0.3.34（package / Cargo / tauri 配置与文档），并导出可迁移开发源码包。

### 0.3.35：搜索绑动作、API Key 回显与设置页理顺

0.3.35 聚焦设置信息架构与搜索配置归属。

- 取消独立「搜索引擎」设置与自定义/模板编辑；Google / Bing / 百度 三个固定引擎写在代码常量中。
- 每个「搜索」动作在编辑对话框中选择默认搜索引擎，配置随动作持久化；运行时按该动作引擎打开搜索。
- 设置 schema 升级至 v10：迁移旧 `activeSearchEngineId` 到搜索动作；丢弃自定义引擎。
- API Key：设置页可从本地加密存储加载已保存密钥，默认黑点隐藏，小眼睛切换明文；通用 getSettings 仍不返回密钥。
- 设置页分区顺序：通用 → 权限 → AI 服务商与模型 → 工具栏动作 → 翻译语言 → 结果窗口 → 应用过滤。

### 0.3.35 当前状态

0.3.34 在 0.3.33 能力之上，进一步统一搜索引擎管理体验，并改善服务商密钥与模型列表的设置交互。应用仍具备与 macOS 0.3.16 对齐的核心划词、动作、设置和结果输出能力，以及 Windows 原生窗口、多屏 DPI、通知区域、单实例、选区 helper、自愈 Hook 和 NSIS 交付链；macOS 保持 Codex/ChatGPT 兼容与菜单栏代理形态，搜索动作使用当前选中引擎的地址模板打开浏览器。

仍需明确以下边界：

- Windows 安全模型决定标准权限进程无法读取高权限应用的选区。
- 自绘应用、Electron 应用和浏览器会随版本改变 UIA 树，兼容 profile 需要持续回归。
- 自动化测试能够覆盖状态机和协议，但不能代替真实鼠标、真实前台切换、多屏 DPI 和第三方应用实机测试。
- ChatGPT/Codex、EmEditor 等在 0.3.29 已加入兼容代码路径，但只有完成对应版本的实机测试后才能标记为已认证。
- 当前范围不包括 Windows ARM64、自动更新、正式代码签名和公开发行。


### 0.3.36：未配置 AI 引导设置 + 快捷键模式外点关闭

0.3.36 聚焦首次使用与快捷键模式下的工具栏体验：

- 未配置服务商/模型/密钥时点击翻译等 AI 动作，自动打开设置并滚动到「工具栏动作」，顶部短暂提示「请先配置 AI 服务商与模型，再为工具栏动作选择模型」。
- 快捷键触发模式下仍保持选区监听，仅在「划词后」模式才因划词自动弹出工具栏；点工具栏外可正常关闭。
- 产品版本升至 0.3.36。

### 0.3.37：设置提示统一可见 + 流式跟手优化

0.3.37 聚焦设置页反馈可读性与流式输出体感：

- 顶部提示仅使用绿色（正常/引导）与红色（失败）；两类均约 2 秒后淡出消失。
- 提示改为悬浮不透明 toast，避免与设置正文叠字。
- 后端流式合批改为约 8ms / 128 字节；前端显示平滑缓冲下调，保留首包立即展示。
- 产品版本升至 0.3.37；设置 schema 仍为 v10。

### 0.3.38：流式首字与跟手再优化

0.3.38 在 0.3.37 基础上进一步压低翻译/解释/总结等动作的感知延迟：

- 后端：流的前约 96 字节立即下发，其后约 4ms / 48 字节合批；HTTP 客户端开启 TCP_NODELAY 并提高连接复用。
- 前端事件：每请求前约 64 字符立即应用到结果状态，其后仍按动画帧合并。
- 显示层：短增量（约 16 字以内）立即显示，仅较大突发再平滑；完成排空更快。
- 产品版本升至 0.3.38。

### 0.3.38 当前状态

当前推荐验证路径：

1. 全新设置下点翻译 → 进入设置工具栏动作区并看到短暂横幅。
2. 配置模型后 AI 动作正常。
3. 快捷键模式唤起工具栏后点外部关闭；划词模式回归正常。

### 后续开发建议

后续版本应优先保持当前状态机边界，避免再用固定延迟或 exact HWND 匹配解决竞态。每次修改选区链路时，至少回归以下序列：

- 连续划词，不执行动作。
- 划词后复制，再立即划词。
- 划词后搜索，切回原应用立即划词。
- 划词后翻译、解释或总结，关闭结果窗后立即划词。
- 结果窗置顶、关闭及正文再次划词。
- helper 超时、崩溃和重建后的下一次捕获。
- 闲置十分钟后继续划词。
- 浏览器、Office/WPS、Electron 应用和原生编辑器连续操作。
- 100%、125%、150% 和 200% 缩放及负坐标副屏。

发布前还需要完成真实应用兼容矩阵、安装与卸载验收、Windows 代码签名、第三方许可证复核，以及 macOS Apple Silicon 回归。

### 资料依据

本文档的版本演进部分根据以下项目内容整理：

- 历次 README / 开发记录中的版本摘要（现已并入本手册）。
- `src/shared/schemas.ts` 中的设置 schema 和迁移边界。
- `src/renderer/settings`、`toolbar`、`result` 中的界面与测试。
- `src-tauri/src/runtime.rs`、`windows.rs` 和 `apps/windows/src/selection.rs` 中的窗口、事件及捕获实现。
- `src-tauri/tauri.windows.conf.json` 和 `apps/windows/scripts/verify-artifacts.mjs` 中的 Windows 打包与产物规则。

### 0.3.39：Review 加固与 CodeG 划词兼容

（整理日期：2026-07-21）

在 0.3.38 之后根据代码审查与实测兼容问题做小版本加固：

- **文档：** README「隐私与安全」改为准确描述：常规设置只暴露 `keyConfigured`；设置窗支持用户**点击查看**已保存 API Key（settings-only IPC）。**不改写** 0.3.16–0.3.38 历史条目。
- **设置：** API Key 默认不预加载明文，用户点击「显示」时再拉取（保留查看/编辑能力）。
- **死代码：** 删除未使用的旧 `IPC_CHANNELS` 表，改为与 Tauri 一致的命令/事件名映射。
- **选区：** macOS/Windows 将 CodeG（`app.codeg` / `codeg.exe`）纳入剪贴板回退白名单，修复内容显示区划词不弹工具栏、输入框正常的问题。
- **安全/正确性：** macOS 剪贴板恢复前复查 `changeCount`；流式事件强制 `sessionId` 与输出上限；结果窗取消 `legacy` session 回退；Markdown 链接抑制非主键导航；API Key 写入与 provider 删除同锁。
- 产品版本升至 0.3.39。

### 0.3.40：流式首字加速与模型思考档位

（整理日期：2026-07-21）

- **流式性能：** 加宽 SSE 首包即时窗口、缩短合并间隔；结果窗 store/播放层提高即时字符与字素预算、收紧追赶延迟，改善翻译/解释/总结/润色等场景的首字与流畅度。
- **思考档位：** 同步模型时写入各模型支持的思考强度（API 元数据优先，模型 ID 启发式回退）；AI 动作可手动选择档位或「关闭思考」（默认关闭以优先速度）；请求体注入 `reasoning_effort` 等 OpenAI 兼容字段。
- 产品版本升至 0.3.40。

### 0.3.41：服务商模型列表紧凑展示

（整理日期：2026-07-21）

- **设置 UI：**「AI 服务商与模型」下模型改为紧凑 chip（仅名称、一行多个），去掉每模型思考档位徽章；仍支持拖拽排序与移除。
- **思考档位：** 继续仅在工具栏动作编辑中选择；同步得到的 `thinkingLevels` 数据保留，供动作配置使用。
- 产品版本升至 0.3.41。

### 0.3.42：复制后一次点空白收起工具栏 + 流式再加速

（整理日期：2026-07-21）

- **工具栏：** 复制成功反馈保留；点击工具栏外空白一次即隐藏（修复此前需点两次）；Dismiss 强制 hide 并同步前端状态。
- **流式：** 进一步收紧 SSE 首包/合并与结果窗 store/播放预算，改善翻译/解释/总结/润色首字与流畅度。
- 产品版本升至 0.3.42。

### 0.3.43：复制后一次外点真正收起工具栏

（整理日期：2026-07-21）

- **工具栏：** 复制成功后，若外点触发的 mouse-up 再次捕获到同一段仍选中文本，会短暂抑制自动弹出，避免工具栏跳到点击处并需点第二次才消失。
- 产品版本升至 0.3.43。

### 0.3.44：复制后恢复来源应用焦点

（整理日期：2026-07-21）

- **工具栏：** 复制成功后立即把激活状态还给划词来源应用，避免下一次外点只用于「切回窗口」而仍需再点一次才取消选区/收起工具栏。
- 产品版本升至 0.3.44。

### 0.3.45：无感焦点交还 + 结果框底部悬停显示

（整理日期：2026-07-21）

- **工具栏：** 复制成功后改为 deactivate + 协作 yield/软激活来源应用，去掉 ActivateAllWindows 导致的窗口闪动。
- **结果窗：** 「继续提问 / 重试 / 复制」仅在指针位于结果框下沿热区时显示，平时隐藏。
- 产品版本升至 0.3.45。

### 0.3.46：结果窗关闭后不再重弹工具栏

（整理日期：2026-07-21）

- **结果窗：** 翻译/解释/总结/润色等结果展示期间及关闭时，对原文选区做同文短时抑制；关闭时清掉同源 live selection 并 force-hide 工具栏，避免外点后工具栏跟鼠标弹出。
- 产品版本升至 0.3.46。

### 0.3.47：结果关闭取消宿主划词 + 流式首字近零延迟

（整理日期：2026-07-21）

- **结果窗：** 外点/关闭结果会话时，在已有选区身份判定与 force-hide 工具栏之外，best-effort 通过 AX/UIA 折叠仍匹配原文的宿主选区高亮（不可写场景静默跳过）。
- **TTFT：** 结果 document 在 React mount 前即 `set_ready`，避免首 token 堵在 Rust pending；hydrate 后保留 immediate 字符预算；streaming 显示直通，去掉本地 typewriter 排队。
- **流式：** SSE 首包即时窗口略增；前端 store 即时预算提高。
- 产品版本升至 0.3.47。


### 0.3.48：翻译流式去卡顿

（整理日期：2026-07-21）

- **流式传输：** `StreamDeltaBuffer` 对每个 SSE content delta 立即下发，去掉「首包窗口后按字节合并」导致的中文断续感。
- **结果 store：** 取消 immediate 字符预算 + rAF 合并；每个 delta 同步进入快照，长翻译不再周期性冻结后跳跃。
- **播放层：** streaming 时同步返回 target 文本（无 useLayoutEffect 一帧延迟），安全后缀跳过全文 Segmenter；仅 ZWJ/组合字符仍排队。
- 产品版本升至 0.3.48。

### 0.3.49：可迁移开发源码包

（整理日期：2026-07-22）

- **文档与导出：** 同步源码包版本与 README；`pnpm export:dev-source` 导出干净可迁移工作区（无 `node_modules` / `target` / 安装包）。
- **包内说明：** 增加 `PORTABLE_DEV_SOURCE.md`，约定新机器上的 Node / pnpm / Rust 与平台工具链要求。
- 产品版本升至 0.3.49。

### 0.3.50：问AI 多轮对话 + 流式首字再优化

（整理日期：2026-07-22）

- **动作模型：** 默认「引用」升级为「问AI」（`ask`）；设置 schema 迁移；动作编辑与图标选择同步。
- **问AI 会话：** 点击后打开结果窗，可不立即请求模型；底部输入框支持首轮 continue 与多轮 transcript；会话仅在当前结果窗内累计。
- **流式性能：** SSE 每个 content delta 立即 emit；前端 store 同步直通，进一步压低 TTFB 与卡顿。
- **文档：** README 功能与版本表同步到 0.3.50。
- 产品版本升至 0.3.50。

### 0.3.51：自适应流式显示与 thinking 透传

（整理日期：2026-07-22）

- **显示层：** 新增自适应打字机（`useSmoothStreamText` / `streamPlayback` 平滑控制器）：逻辑全文仍即时进入 store，UI 按 backlog 调节揭晓速度，突发 token 不再整段倾泻；首包小 burst 兼顾 TTFB。
- **Thinking：** OpenAI-compatible SSE 解析 thinking/reasoning 增量；会话 flusher 与结果窗事件链路端到端透传。
- **后端泵：** action flusher 按批（约 16 次 emit）再 yield，减少密集 IPC 与调度抖动。
- **版本对齐：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf、README 与开发记录统一为 0.3.51。
- 产品版本升至 0.3.51。

### 0.3.52：设置页侧边栏 + 精简提示词 + Markdown 优化

（整理日期：2026-07-22）

- **设置 UI：** 设置页改为左侧分区导航 + 右侧内容区（常规 / 工具栏动作 / AI 服务商 / 结果窗口等），分区文案与信息层级更清晰。
- **默认提示词：** 翻译、总结、解释默认提示词精简；TypeScript 与 Rust 双端默认值对齐；未改动的内置 v11 长文案在加载时迁移到新默认，自定义提示词保留。
- **结果 Markdown：** 结果窗 Markdown 样式与明暗主题兼容性增强；仅在内容疑似含公式时启用 remark-math / rehype-katex。
- **版本与源码包：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf、README 与开发记录统一为 0.3.52；`pnpm export:dev-source` 导出可迁移源码包。
- 产品版本升至 0.3.52。

### 0.3.53：结果后重划工具栏 + 选择性模型获取 + UI 打磨

（整理日期：2026-07-22）

- **划词工具栏可靠性：** 结果关闭后延迟清除宿主选区，避免与下一次手势竞态；宿主清除完成后再释放 same-text 抑制，同一文本也可重新弹出工具栏；AI 结果展示不再强制把焦点交还来源应用，避免失焦关闭结果窗失效。
- **结果内再划词 dismiss：** 结果正文再次划词弹出工具栏后，在结果框内点击工具栏以外区域应与外部点击一致地隐藏工具栏、且不关闭结果窗。新增 `hide_result_selection` IPC（仅清除来自该结果会话的选区并 force-hide 工具栏，reason `resultClick`）；结果窗 `pointerdown` / 无选区 `pointerup` 调用；macOS `selection_bridge` 对本进程 mouseDown/scroll 仍 enqueue dismiss（不捕获选区；点在工具栏上由 runtime 忽略）。
- **服务商模型：** 新增 `list_provider_models`（只读拉取，不写设置）；设置页「获取模型」→ 多选合并到草稿列表，支持纵向拖拽排序；不再用整表同步覆盖已选模型。
- **UI 打磨：** 设置页分区说明与脏保存强调更清晰；结果窗单行等待态、thinking 仅「思考」徽章 + 自动展开、停止/复制等微交互与减少动效路径；保留 TextLens 原生 token，不引入外来 AI 紫配色。
- **版本与源码包：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf、README 与开发记录统一为 0.3.53；`pnpm export:dev-source` 导出可迁移源码包。
- 产品版本升至 0.3.53。

### 0.3.54：划词工具栏兼容加固 + 结果 Markdown 段落/列表

（整理日期：2026-07-23）

- **Windows 划词兼容：** allowlist 应用在 UIA `IsPassword` 未知时允许剪贴板 Ctrl+C 回退；将 WPS / 微信 / QQ 等进程族视为相关目标以改善多进程 UIA；UIA 始终解析不到目标时走前台 Ctrl+C 路径；上述路径仅短时重试一次后 break，避免额外延迟。
- **macOS 划词兼容：** 剪贴板 allowlist 匹配改为大小写不敏感；拓宽金山 / WPS 的 bundle id 与应用名识别，覆盖渠道变体。
- **结果 Markdown：** 渲染前规范化模型输出——句末单换行拆成段落、识别中文列表标记（`1、` / `•`）、在块边界补空行；表格行与同级列表项保持紧凑、代码围栏内容不改动；normalize / math 检测 / 组件 memo 化；段落与列表 CSS 节奏收紧。
- **思考区字号：** 思考正文不再挂 `stream-plain-text`（该规则与答案同字号且写在后面，会盖掉缩小设置）；`.result-thinking__body` 使用 `max(11px, calc(var(--result-font-size) * 0.82))`，默认约 11.5px，明显小于答案正文。
- **版本与源码包：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf、README 与开发记录统一为 0.3.54；`pnpm export:dev-source` 导出可迁移源码包。
- 产品版本升至 0.3.54。

### 0.3.55：服务商启停 + 多语言翻译 + 统一模型/思考选择

（整理日期：2026-07-23）

- **服务商启停：** `ProviderConfig.enabled`（默认 true）；设置页可禁用服务商；禁用后不出现在动作模型绑定与结果窗换模列表，已绑定该服务商的动作仍可运行。
- **动作模型选择：** 编辑动作时由「服务商 + 模型」双列表改为按服务商分组的统一 optgroup 列表（与结果窗一致）；过滤未启用服务商。
- **思考档位：** 按模型名自动推断可用思考强度（`inferThinkingLevels` / `effectiveThinkingLevels`），支持「关闭思考」；无思考能力的模型不展示档位。
- **多语言翻译：** 目标语言扩展为简体中文 / English / 日本語 / 한국어 / Русский / Deutsch / Français；结果标题栏以 CN/EN/JA/KO/RU/DE/FR 短码切换；设置提供「默认语言 / 翻译成」，检测到翻译目标时反向译为默认语言，其余语种译为翻译目标。`detectTranslationLanguage` 与 `defaultTranslationTarget` 双端对齐，源语言按字符占比判定（假名 10%，汉字/谚文/西里尔 30%），避免单个外文字符劫持整段。
- **提示词：** 翻译提示词强化多语言专业翻译；解释提示词改为整体解释、仅展开重要术语；未改动的内置默认文案在加载时迁移，自定义提示词保留。
- **版本与源码包：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf、README 与开发记录统一为 0.3.55；`pnpm export:dev-source` 导出可迁移源码包。
- 产品版本升至 0.3.55。

### 0.3.56：macOS 托盘死锁修复 + 开发手册

（整理日期：2026-07-24）

- **死锁修复：** 现场 sample 显示 macOS 长跑后菜单栏图标不可点，CPU 极低（睡眠等待）而非忙循环。根因是 `show_toolbar` 在持有 `WindowState` 锁时调用 `order_front_without_focus`（`run_on_main_thread` + `recv`），与主线程在 WebKit 原生 `<select>` 弹层嵌套 runloop 中处理 `Destroyed → remove_result` 互相等待。修复为：先 `commit_toolbar_selection` 并释放锁，再算 layout / `apply_window_layout` / `order_front`；展示前复查 `toolbar_selection_id`，避免 hide 竞态后复活工具栏。
- **结果窗防护：** 关闭、pagehide、卸载时 blur 原生 `<select>`，降低 NSMenu 嵌套 runloop 与窗口销毁并发的触发概率。
- **文档：** `DEVELOPMENT_HISTORY.md` 重命名为 `DEVELOPMENT_HANDBOOK.md`（开发手册），并入本地开发（Windows / macOS）、可迁移源码导出与版本演进；README 去掉版本号简介与上述开发小节，改为指向手册；「设计灵感与第三方来源」标题不再出现第三方产品名；安装小节改为「安装 - Windows / 安装 - macOS」。
- **版本与源码包：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf 统一为 0.3.56；`pnpm export:dev-source` 导出可迁移源码包。
- 产品版本升至 0.3.56。

### 0.4.0：划词响应、Markdown 稳定渲染与快捷键注册加固

（整理日期：2026-08-10）

- **双端划词兼容：** macOS 从 allowlist 扩展为 denylist 与安全输入判断，增加 AX 调用限时、按进程兼容属性缓存和事件监听自愈；Windows 增加 UIA 硬预算、按应用历史自适应路由、前台无障碍树预热，以及 PDF/Office 跨段长划词的 settle、UIA、WM_COPY、Ctrl+C 和空结果重试链路。
- **捕获稳定性：** 将可能被第三方 UIA/OLE provider 阻塞的 Windows 捕获隔离到 helper 进程，限制超时、重建失效 helper，并处理滚轮、generation、前台窗口和应用族关联，降低长时间运行后划词失效。
- **工具栏：** 双端采用 stage/present 展示事务，改善首次展示延迟、透明等待、窗口复活和悬停高亮；工具栏动作加入忙态单飞和指针采样反馈。
- **结果 Markdown：** 流式输出提前复用稳定的 Markdown 渲染路径，统一段落、列表、代码块与公式间距，降低纯文本到 Markdown 切换时的视觉跳变。
- **快捷键：** 保存设置时按解析后的快捷键身份切换注册，支持失败回滚与残留注册恢复；启用状态、事件 ID、当前注册项和持久化设置保持一致，录制组合键保留全部修饰键。
- **版本与源码包：** `package.json`、`Cargo.toml`、`tauri.conf.json`、Windows conf、`Cargo.lock` 统一为 0.4.0；`pnpm export:dev-source` 导出可迁移源码包。
- 产品版本升至 0.4.0。

### 0.4.1：划词捕获加固、取词性能与覆盖面、设置审计与 UI 打磨

**背景：** 0.4.0 重写 `apps/macos/native/selection_bridge.mm` 时未迁移 0.3.x 的「临时复制并恢复剪贴板」兼容路径，而 WPS（Qt 自绘画布）与浏览器内嵌 PDF 的 AX 树不暴露选中文本，划词后不弹工具栏；画布类应用还会在空白处保留旧模型选区、或对无选区的 Cmd+C 做隐式复制，导致空白处双击误弹。

**macOS 剪贴板兼容捕获（临时复制事务）：**

- 快照 `NSPasteboard`（含「原本为空」这一状态）→ 按 pid 投递带 `kCGEventSourceUserData` 标记的 Cmd+C，200ms 无反应再按会话级重发一次（覆盖忽略 pid 定向事件的应用）→ 轮询 `changeCount` 并容忍「先清后写」两段式复制 → 读取字符串 → 只以本次合成复制产生的 `copiedChangeCount` 为恢复边界。预算 480ms、轮询 2→12ms 指数退避、停止轮询后 120ms 的「迟到认领」仅在无新输入时启用，避免把用户自己的 Cmd+C 当成我们的。
- 快照 fail-closed：惰性/承诺表示、超过 16MB 单表示或 32MB 总额时**整体放弃本次回退**（部分快照写回会永久丢失没被捕获的内容）；`dyn.*` 是静态表示的派生形式，跳过读取以防跨进程懒解析阻塞。
- 恢复前复查 `changeCount`，等待窗口内用户的真实复制永远优先；非文本响应用白名单判定——携带已知图片/文件类型且不含任何文本类型才算非文本，等约 60ms 写入稳定窗口后按最终 `changeCount` 恢复并放弃本次捕获，未知类型组合等满预算。
- 门禁三级：密码管理器与 Finder 一律拒绝；GPU 终端（kitty/Alacritty/Ghostty/WezTerm 等）Cmd+C 无选区时是 no-op，全触发方式放行；Terminal.app/iTerm2 及名称含 terminal/iterm 的「智能 Cmd+C」终端无选区会发 SIGINT 杀掉前台，仅放行键盘触发（Cmd+A、Shift+方向键必然留下真实选区）；另有 `IsSecureEventInputEnabled()` 系统级判定、聚焦元素 AXSecureTextField/AXProtectedContent、前台应用必须 active 且非自身、事件 tap 用合成标记忽略自身按键（否则会自毁 generation 并触发 dismiss）、每次 AX 往返 80ms 消息超时。
- 图片查看器：回退前查聚焦元素 role，`AXImage` 直接跳过合成复制；role 不可读时按约 30ms 间隔重试两次——兼容属性常在首次捕获期间才建立，否则就是「首拖必复制、后续不再」。拖动图片本身会被判为 Drag 划词，因此再加指针命中测试（`AXUIElementCopyElementAtPosition`）：角色链含 AXImage 即拒绝；微信图片查看器在光标下只暴露裸窗口，故另加「微信 + 鼠标手势 + 裸窗口表面」规则（`skip: wechat-bare-window-drag`）。键盘触发不参与该判定（光标位置无意义）。
- 误弹收窄：空选区预检只在「聚焦元素为文本角色（AXTextArea/AXTextField/AXTextView/AXStaticText）且触发为双击/Shift 点击」时信任 `kAXSelectedTextRange == {0,0}`——微信聊天窗口的容器同样报 {0,0}，0.4.1 开发期间曾因此让微信划词完全失效，拖动与键盘触发一律不信任该信号；剪贴板结果与上次已接受捕获文本相同且位置相距超过 48px 判为残留（`stale-same-text-far-location`）；鼠标触发的捕获去空白后 ≤3 字符且不含字母类字符（ASCII/西里尔/假名/CJK/谚文）丢弃（`1. `、`• `、`—` 这类渲染标记碎片）；双击容差从 56px 收紧到 20px（双击选中的词必然在光标下，Chrome 里双击公式行右侧空白会整段吞下无空格公式串），拖动与 Shift 点击保持 56px。
- trace：`TEXTLENS_SELECTION_TRACE=1` 输出 `[selection-fallback]` 各出口原因（app-policy-denied / secure-event-input / text-element-empty-range-on-click / focused-element-is-image / clipboard-unrestorable / chord-ignored-by-app / non-text-copy / restore-boundary-missed / stale-same-text-far-location / text-matches-app-identity / capture: ok），并镜像到 `~/Library/Logs/TextLens/selection-trace.log`（Finder 启动的 GUI 看不到 stderr）；打包应用用 `launchctl setenv` 开启。选区文本默认只记长度与 FNV-1a 哈希，`TEXTLENS_TRACE_CONTENT=1` 才加 24 字符脱敏前缀。

**取词性能与覆盖面：** `AXSelectedTextMarkerRange` 文本标记回退让 Chromium 网页选区两次 AX 调用直达；移除捕获前 12ms 固定延时，改为首读失败后自适应单次重试（28/24/12ms）并按 pid 复用 AXUIElement 缓存；新增浏览器 AppleScript 取词层（位于 AX 失败与剪贴板回退之间，osascript 子进程 200ms 硬超时防 TCC 授权弹窗卡死流水线，`method:"applescript"` 上报）；剪贴板回退移入专用线程（latest-wins 队列 + generation 逐阶段取消，手动快捷键仍内联同步），480ms 合成复制不再阻塞下一次 AX 读取；macOS 展示路径四次跨线程往返合并为单次主线程 hop。

**工具栏与自愈：** 把 Windows 的工具栏 WebView 重建机制（`rebuild_toolbar`、恢复预留/取消状态机、Destroyed 后重建单例、`toolbar_ready` 重放内存选区、每选区有界一次尝试）扩展到 macOS，此前这些路径会直接丢弃本次划词。权限轮询改自适应：未授权 600ms、已授权 2s；工具栏启动期设置读取失败后，每次新划词自动重拉重试链。

**设置与界面：** 对 7 个分区的全部叶子设置做端到端核对（UI → 落盘 → 消费方），修复两处——「划词获取」分区仅 Windows 消费，macOS 按平台隐藏；`providers[].enabled` 此前只影响模型选择器可见性，执行链 `prepared_provider` 现对已停用服务商拒绝执行（连接测试/获取模型不拦）。推理模型答案首行空白由显示层 `stripLeadingBlankLine` 剥除（原始内容与复制不受影响），思考折叠改紧凑胶囊。合并实现相同的 `hide_result`/`close_result` 为 `close_result_session`，删除无调用者的命令、死字段与过时 `#[allow(dead_code)]`。

**回归清单（实机验证用）：** WPS 文字/表格划词；Chrome/Safari/Edge 打开 PDF 划词；TextEdit 与普通网页文本不进剪贴板路径。kitty/Alacritty/Ghostty 划词可弹且空选拖动不误发中断；Terminal.app/iTerm2 拖动走 AX、Cmd+A 可用；1Password 不触发合成复制；此前复制过文件时在 WPS 划词。空白处双击不弹、同位置反复双击同一单词仍弹、远处重划完全相同文本被 stale 护栏拒绝（已知取舍）。合成 Cmd+C 等待期间按键立即取消且剪贴板无残留；捕获完成后剪贴板恢复划词前内容；连续划词互不阻塞。授权后约 1 秒内划词可用；工具栏渲染器异常时下一次划词自动重建。Chromium 命中 marker-range、Safari 见 `applescript-capture: ok` 且剪贴板不变；设置页 macOS 不显示「划词获取」分区；停用服务商后其动作报「已停用」；思考折叠后正文无空洞。

### 0.4.2：稳定性重构、与宿主应用共存、划词判定

Windows 侧改动本机无法编译验证（`aws-lc-sys` 在交叉编译阶段失败），一律以 CI 的 `verify:windows` 或真机为准。

- **捕获结构：** macOS 应用差异集中到纯 C++ 表 `apps/macos/native/app_profiles.h`（bundle 精确/前缀/名称匹配，clipboard/appleScript/surfaceGuard 能力），微信画布与不透明窗口门闩保留并泛化；受信任文本窗口缓存改为 bundle + pid + window + 15 分钟 TTL，避免跨应用误用。
- **事件排序：** 原生 selection/dismiss 事件附带进程内单调 `sequence`，Rust 侧优先按序号仲裁，缺少序号的生产者（Windows）回退到毫秒时间戳；不再依赖系统时钟。
- **诊断：** trace 默认只记选区长度与哈希，`TEXTLENS_TRACE_CONTENT=1` 才输出 24 字符前缀。
- **设置热路径：** 设置读路径使用 `Arc` 快照，写盘与 fsync 不再阻塞选区线程；写入串行化并按 generation 避免启动默认值覆盖用户刚落盘的设置；写入前保留 `.bak`，未知字段宽松忽略。
- **设置界面：** 7 个分区（常规/权限与诊断/外观/AI 服务商/工具栏动作/语言与翻译/捕获与过滤），字段级错误定位、过滤标签编辑器、测试连接/获取模型前显式保存提示、显式翻译方向交换。
- **主题与弹出窗：** 五套主题补齐明暗对称 token；间距/字号/字重/圆角全部 token 化并在测试中禁用数字字重与 px 圆角。工具栏恢复键盘 focus ring；模型下拉补齐 listbox 键盘导航；主题/触发方式 radiogroup 使用 roving tabindex；问AI 模式复制序列化完整多轮 transcript。

**与宿主应用共存**：冲突时以不影响宿主为准——剪贴板不能完整还原就不做临时复制，选区没有新增证据就不弹工具栏，动作结束后把选中状态还给宿主。

- **剪贴板快照不完整即放弃临时复制**：无法忠实重建时（惰性/承诺表示、超过 16MB 单表示或 32MB 总额）不再发合成 Cmd+C，截图工具留在剪贴板的位图不会被擦掉；Windows 按位图格式同规则。机制细节见 0.4.1 的「macOS 剪贴板兼容捕获（临时复制事务）」。
- 动作完成后宿主应用仍报告同一段选区，此后的「随手一点」常带 5–8px 位移，原生层按 `distance >= 4px` 判为拖动并重新捕获同一段文本，工具栏就会跳到光标处重新弹出。原先由 Rust 侧 `SAME_TEXT_SELECTION_SUPPRESS_MS = 2000` 兜底，而结果窗关闭后 200ms 的宿主高亮折叠会把抑制提前解除，2 秒之后完全失效。
- 现在 Rust 侧记录**上一次已呈现的选区身份**（宿主 bundle id + 文本 + 选区 bounds），并只看本次捕获的手势端点间距来判定，见 `runtime.rs: judge_selection_presentation`：文本或宿主不同→呈现；bounds 存在且差超过 2px→是同一段文字的另一次选中，呈现；bounds 相同或缺失时，必须**同时**满足两条才算用户重新划选：拖动端点间距 ≥ `RESELECT_MIN_REACH_PX = 12.0`，且起点或终点落在上次那个选区上（有 bounds 就按矩形外扩 `RESELECT_ANCHOR_RADIUS_PX = 16.0` 判定，无 bounds 就与上次记录的拖拽端点比距离）——只看位移是不够的，翻译结果框被外点关闭后那一下点击常带 15–20px 漂移，只按位移判定会把框外某处的残留选区再弹一次。双击 / Cmd+A / Shift 点击在身份未变化时一律丢弃（残留选区没有任何新手势证据），快捷键 `Manual` 触发与 `force_capture` 始终呈现。
- 身份在选区被接受并呈现时记录一次，因此 dismiss、复制、搜索、结果窗关闭都不再需要「武装计时器」；`dismiss_precedes_selection` 命中时提前 return 也不会丢失抑制（旧实现里那条路径会在武装抑制之前返回，导致同 tick 的 dismiss 永远不产生抑制）。宿主高亮折叠仍按 `HOST_SELECTION_CLEAR_DELAY_MS = 200` 延后执行，但不再解除任何抑制。
- **动作接管成功后折叠宿主高亮**：复制、搜索沿用各自的成功承诺点；AI 动作先完成本地配置与 Key 预检，结果窗完成显示后才标记 `action_taken_over` 并安排清除，沿用捕获时的 `host_clear_generation`。延迟 200ms 执行前重新检查接管状态、原选区 token 与已提交路线。创建失败、显示超时或配置拒绝均保持原文选中，便于重试；关闭旧结果不能创建一个覆盖新选区的 token。原生折叠按宿主 bundle id 在 `NSWorkspace.runningApplications` 中定位目标应用，不依赖 TextLens 显示后谁处于前台。结果窗内的自身选区不参与宿主折叠。Windows 当前仍禁用宿主范围清除，等待可隔离、可取消的原生实现。
- **折叠结果全部可观测**：新增 `[selection-host-clear]` 通道，逐条输出 `cleared` / `skip: not-trusted` / `skip: app-not-found` / `skip: no-focused-element` / `skip: selected-text-unreadable` / `skip: text-mismatch (host=N captured=M)` / `skip: no-selected-range` / `skip: write-rejected`，与回退日志同一开关同一文件。此前折叠失败完全静默，「点了动作没反应」无从判断。`text-mismatch` 表示宿主当前选区与捕获文本不逐字节相同（AppleScript/剪贴板取词路径常见），此时不折叠是有意取舍。
- 已知取舍：抑制不再随时间失效——在同一段文字上做一次小于 12px 的短拖动不会再弹工具栏，需要重新划出更长的一次拖动、或用快捷键触发。反向取舍：在选区 16px 范围内做一次 ≥12px 的短拖动仍会重新弹出（用户确实在动那段文字）。

**回归清单（实机验证用）：** 全屏截图把位图留在剪贴板后去 WPS/浏览器 PDF 划词——出现 `skip: clipboard-unrestorable`、工具栏不弹，但截图仍可粘贴；剪贴板是纯文本时回退正常且事后原样恢复。划词→点翻译→外点关框→再点框外别处（带 15–20px 漂移）不弹，在原文字上明显长距离重划仍弹，快捷键对同段文字始终弹。macOS 动作成功接管后延迟折叠原文高亮；配置拒绝、结果窗创建失败与显示超时保持选中。快速双击空白与 Cmd+A 全选同一文本不重复弹。Windows 需在真机或 CI 复验捕获、取消、工具栏和结果窗生命周期。

### 2026-11-06：源码审查修复与弹出响应（0.4.2 工作分支）

- **请求准备与 Ask 会话：** 仍存活且 reservation 匹配的会话在准备成功时发 `Started`，准备失败或准备中取消时发 `PreparationFailed`；关闭会话后旧 ticket 不再提交或复活 session。准备失败保留已提交的请求、答案与路线，恢复未提交的问题。Ask 重试重放失败轮消息并保留历史，快到同一批次内结束的回答仍能显示、复制。`Started` 与恢复 snapshot 附带稳定 turn ID 的完整 transcript；尚未提问的 Ask 切换模型只提交路线。
- **内容与协议：** 空白答案和长度截断返回明确错误，截断保留已有正文；JSON 回退使用首字节与读取空闲预算。Markdown 不再猜测反转义，保留 Setext 标题、标准代码围栏与嵌套列表。德语、法语、英语识别共享 TS/Rust fixture。
- **窗口响应：** 工具栏原生操作集中在主线程串行队列，队列锁只保护入队与出队；尺寸测量按选区去重，DOM 与原生窗口使用同一工作区宽度预算，长名省略并保留 tooltip，极窄窗口允许水平滚动。结果窗两端统一 `Hidden → Prepared → Committed`，原生准备与首个 React 帧并行，原生 commit 确认后单次淡入；显示提交与尺寸记忆、鼠标移出关闭和宿主接管分别门控。自动 CloseRequested 最终重新核对物理指针或原生焦点，并在原生读取后重验 generation/pin/mode，避免旧关闭请求越过重新获焦。工具栏与结果窗仅用一次 90ms 透明度过渡，正文、思考与等待容器不叠加位移入场动效；等待响应的三个点独立错峰轻跳并渐明渐暗。系统减少动态效果时停用动画与 JS 打字播放，输出期间切换该偏好立即显示完整积压文本。
- **设置与迟到任务：** 保存按 draft/Key 版本确认，保留等待期间的新输入；规则行使用稳定身份，reveal 在清除、保存或删除后失效。尺寸 reset epoch 与 resize revision 在 repository 写锁内复查，旧尺寸与旧广播不能覆盖最新状态。共享 Dialog 约束并恢复焦点，校验能切换到出错分区，主题预览独立绑定 palette，早期 WebView 使用基础背景与焦点颜色。
