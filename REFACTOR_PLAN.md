# TextLens 重构与优化计划

- 整理日期：2026-10-31
- 审计基线：`main` @ `2d535cb`（保持未改动；本轮改动全部在 `fix/review-stability`，基于同一提交）
- 目标：稳定性、划词弹出响应效率、更多应用/场景兼容、隐藏 bug 修复；设置页信息架构重构与美化；弹出框适度美化
- 说明：以下 file:line 均按 `2d535cb` 实码核对，后续改动会使行号漂移

## 执行进度（2026-10-31）

- 阶段 0：完成。契约黄金样本 + `Point` 统一为 f64；explain 默认提示词对齐并加 TS↔Rust 等值测试；迁移版本守卫；热键语法语料；macOS 原生纯谓词测试（`-Werror`）；捕获规则/语言列表等值守卫；`deny.toml` + CI 供应链检查；密钥扫描与 macOS 打包 CI。
- 阶段 1（P0 #1–#17）：完成，每项含回归测试；Windows 侧仅本机静态修改，待 CI/真机复核。
- 阶段 2（响应效率）：完成。macOS 两段式互斥、generation 取消、轮询退避、窗口标题延迟读取；Rust 设置 Arc 快照、单一定时关闭、RAII 消费集合；前端 markdown latch 与打字机追加偏移；Windows 快照降级与 hook 刷新窗口去重。
- 阶段 3：macOS AppProfile 与泛化 surface guard 完成；协议差异已集中于 `protocols.rs`（传输层协议中立），thinking 中的并行 match 属于各协议能力推断、由测试钉住；Windows 完成 `parent_capture_accepts` 生产/测试双胞胎合并与丢失 mouse-down 的拖动回退。**`selection.rs` 六模块拆分、Windows AppProfile、`runtime.rs` 三模块拆分未执行**：本机无法编译 Windows 目标，且属纯结构移动、不改变行为；留待 CI/真机窗口期按模块逐步搬运。
- 阶段 4：完成。7 分区 IA、字段级错误、过滤标签编辑器、测试/获取模型前显式保存提示、显式翻译交换；设计 token 化（间距/字号/字重/圆角）并在测试中禁止数字字重与 px 圆角；主题明暗对称补全。
- 阶段 5：完成。工具栏键盘 focus ring、忙态统一；结果窗 CSS 去重与 slate 回退清理、翻译方向排版收敛；模型 listbox 键盘导航与 roving tabindex；radiogroup 方向键；ask 复制序列化多轮 transcript。
- 阶段 6：完成。trace 内容前缀改为独立 `TEXTLENS_TRACE_CONTENT` 开关（默认只记长度/哈希）；selection/dismiss 增加单调 `sequence` 并优先按序号仲裁；设置写入保留 `.bak`，未知字段宽松策略写入代码注释与手册；手册版本/密码策略/trace 说明对齐。
- 验证：前端 379 项、Rust 276 项、原生策略测试、typecheck、rustfmt 通过；完整 `pnpm verify` 与 `verify:windows` 见最终回归记录（Windows 需 CI/真机）。

---

## 一、审计结论

### P0 稳定性（长期劣化或偶发卡死）

| # | 问题 | 证据 |
|---|---|---|
| 1 | 指针命中测试每次泄漏一个 `AXUIElementRef`：`CFRetain(current)` 时 `current == hit`，循环内 `previous != hit` 跳过释放，结尾只 `CFRelease(hit)` 一次 → 净 +1。每次鼠标触发的剪贴板回退都会泄漏 | [selection_bridge.mm:1732](apps/macos/native/selection_bridge.mm#L1732)、[:1794](apps/macos/native/selection_bridge.mm#L1794)、[:1801](apps/macos/native/selection_bridge.mm#L1801) |
| 2 | AX 应用元素缓存 FIFO 淘汰时 `erase` 不 `CFRelease` → 超过 16 个 pid 后每次淘汰泄漏一个 | [selection_bridge.mm:1107](apps/macos/native/selection_bridge.mm#L1107) |
| 3 | macOS 六处主线程 hop 用裸 `receiver.recv()` 无超时；Windows 路径是 `recv_timeout(2s)`。AppKit 主线程被嵌套 runloop 卡住时，唯一的选区消费线程 `textlens-events` 会无限期冻结（所有划词 + dismiss 全停） | [windows.rs:2552](src-tauri/src/windows.rs#L2552)、[2586](src-tauri/src/windows.rs#L2586)、[2641](src-tauri/src/windows.rs#L2641)、[2774](src-tauri/src/windows.rs#L2774)、[2794](src-tauri/src/windows.rs#L2794)、[2845](src-tauri/src/windows.rs#L2845) |
| 4 | 微信“信任窗口”缓存 `gLastWeChatTextWindow` 只在 `CaptureSelection` 写入，而它只被手动/快捷键路径 `captureCurrent` 调用；自动划词走 `CaptureViaAccessibilityPipeline` + `CaptureViaFallbackChain`，永远不播种 → 逃生阀在最常用模式下是死代码，opaque 判定只剩窗口子树扫描（最坏约 1.4s 且不可取消） | [selection_bridge.mm:1305](apps/macos/native/selection_bridge.mm#L1305)、[:2683](apps/macos/native/selection_bridge.mm#L2683)、[:3032](apps/macos/native/selection_bridge.mm#L3032)、[:3304](apps/macos/native/selection_bridge.mm#L3304) |
| 5 | 会话表关闭后留 `Closed` 墓碑，全仓无 `remove/retain/clear` → 长期运行每次动作永久增加一条 | [session.rs:1124](src-tauri/src/actions/session.rs#L1124) |
| 6 | Windows 密码状态**未知**（COM 出错/取焦点失败）被映射成 `false`，不阻止自动 `Ctrl+C` 回退，与手册“密码状态无法确认时不使用自动剪贴板复制”相反（隐私） | [selection.rs:6184](apps/windows/src/selection.rs#L6184)、[DEVELOPMENT_HANDBOOK.md:535](DEVELOPMENT_HANDBOOK.md#L535) |
| 7 | helper 进程被“协作式取消”也杀掉：任何 `Err`（含 `Cancelled`）都 `terminate()`，而 helper 正常应答取消同样返回 `Cancelled` → 用户中途打字/重新划词就 churn 一次进程 | [selection.rs:2161](apps/windows/src/selection.rs#L2161)、[:2447](apps/windows/src/selection.rs#L2447) |
| 8 | 启动时在主线程同步建仓库：读盘 + 解密 + 整条迁移阶梯 + 可能的 fsync 写在首帧前 | [lib.rs:69](src-tauri/src/lib.rs#L69) |

### P0 正确性 / TS↔Rust 一致性

| # | 问题 | 证据 |
|---|---|---|
| 9 | 手动添加的思考模型：UI 用 `effectiveThinkingLevels`（空则推断）显示档位，Rust 保存时只按已存档位夹取 → 档位静默变 Off | [models.rs:1329](src-tauri/src/models.rs#L1329)、[thinking.ts:239](src/shared/thinking.ts#L239) |
| 10 | DeepSeek `r1` / `*-reasoner` 被推断成 `ReasoningEffort` 方言，但 DeepSeek 无该参数；无字段回退只在 `mode==Off && attempt==1` 触发 → 选低/中/高档直接失败且不重试 | [thinking.rs:418](src-tauri/src/thinking.rs#L418)、[:42](src-tauri/src/thinking.rs#L42) |
| 11 | explain 默认提示词两端交叉错位：Rust 当前默认 == TS 的 `LEGACY_CURRENT_EXPLAIN_PROMPT`，TS 当前默认 == Rust 的 `LEGACY_V11_EXPLAIN_PROMPT`。点“恢复默认提示词”会写入旧文案，下次启动 Rust 又改回来并重写磁盘 | [models.rs:42](src-tauri/src/models.rs#L42)、[:96](src-tauri/src/models.rs#L96)、[defaults.ts:52](src/shared/defaults.ts#L52)、[:69](src/shared/defaults.ts#L69) |
| 12 | 模型 id 长度校验用字节（`value.len() <= 256`），normalize 与 TS 用字符 → 86–256 个汉字的 id 能存但每次执行都校验失败 | [actions.rs:2364](src-tauri/src/actions.rs#L2364) |
| 13 | 结果窗临时切换的翻译目标语言会被任意一次设置变更重置（effect 依赖 `settings?.translate` 的对象身份） | [ResultApp.tsx:885](src/renderer/result/ResultApp.tsx#L885) |
| 14 | 应用过滤 textarea 边输入边 `split/trim/filter` → 打不出空行/尾换行，光标跳动 | [SettingsApp.tsx:1798](src/renderer/settings/SettingsApp.tsx#L1798) |
| 15 | 三个窗口都没有根 ErrorBoundary，渲染抛错即白屏（结果窗还会永远不 reveal） | [result/main.tsx:48](src/renderer/result/main.tsx#L48) 等 |
| 16 | TS `migrateAppSettings` 对未知/更高版本落入 legacy 分支并用 `input.ai` 重建 providers（丢配置）。目前只有测试调用，属埋雷 | [defaults.ts:637](src/shared/defaults.ts#L637) |
| 17 | PDF 类剪贴板 profile 不校验 owner（`requires_target_clipboard_owner` 只对 General 为真）+ `wait_for_fresh_clipboard_text` 采纳任意更新序列 → 剪贴板管理器/延迟渲染器的外部写入可能被当成用户选区送给模型 | [selection.rs:6415](apps/windows/src/selection.rs#L6415)、[:6974](apps/windows/src/selection.rs#L6974) |

### P1 响应效率

- 手动/快捷键路径持 `captureMutex` 跑完整个约 1s 剪贴板回退，且 `captureGeneration = nullptr` 不可取消 → 阻塞下一次自动划词的 AX 读取（[selection_bridge.mm:3032](apps/macos/native/selection_bridge.mm#L3032)，自动路径的正确做法在 [:3313](apps/macos/native/selection_bridge.mm#L3313)）。
- 每次回退线性扫描 `runningApplications`（[:3384](apps/macos/native/selection_bridge.mm#L3384)）；剪贴板轮询固定 12ms、探针重试固定 30ms×2（[:2219](apps/macos/native/selection_bridge.mm#L2219)、[:1495](apps/macos/native/selection_bridge.mm#L1495)）；`stop()` 各循环只看 generation 不看 `running`，最坏 >1s 才 join（[:3000](apps/macos/native/selection_bridge.mm#L3000)）。
- 每次 mouse-up 克隆整份 `AppSettings`，且设置写锁跨 fsync 会拖住选区线程读者（[runtime.rs:804](src-tauri/src/runtime.rs#L804)、[settings.rs:491](src-tauri/src/settings.rs#L491)）；`schedule_close_unpinned_results` 每次新建线程 + sleep（[runtime.rs:926](src-tauri/src/runtime.rs#L926)）。
- 结果窗每帧无条件跑两次全文正则判定 rich markdown（latch 后仍跑），打字机每帧 `startsWith/slice` 是 O(n)（[ResultOutput.tsx:286](src/renderer/result/ResultOutput.tsx#L286)、[streamPlayback.ts:236](src/renderer/result/streamPlayback.ts#L236)）。
- Windows：`toolbar_operation` 锁跨 ≤2s UI 等待（[windows.rs:980](src-tauri/src/windows.rs#L980)）；每 10s 盲目重装 hook + 单槽去重可丢同毫秒滚轮事件（[selection.rs:113](apps/windows/src/selection.rs#L113)、[:8174](apps/windows/src/selection.rs#L8174)、[:8269](apps/windows/src/selection.rs#L8269)）；64MB 快照超限直接放弃整次捕获（[clipboard.rs:11](src-tauri/src/clipboard.rs#L11)、[:166](src-tauri/src/clipboard.rs#L166)）。

### P1/P2 兼容性架构债

- macOS 的应用知识散落成字符串启发式：WPS（[selection_bridge.mm:1213](apps/macos/native/selection_bridge.mm#L1213)）、文档/Office/PDF 预算类（[:1231](apps/macos/native/selection_bridge.mm#L1231)）、剪贴板 denylist 与“智能 Cmd+C 终端”（[:1864](apps/macos/native/selection_bridge.mm#L1864)）、AppleScript 浏览器（[:2370](apps/macos/native/selection_bridge.mm#L2370)）、微信字面量（[:2025](apps/macos/native/selection_bridge.mm#L2025)、[:2673](apps/macos/native/selection_bridge.mm#L2673)）。
- Windows 侧同构：15 个族/profile 谓词分散在 [selection.rs:6247-6800](apps/windows/src/selection.rs#L6247)；9673 行单文件是维护风险。

### 设置界面与弹出框

- IA 不自洽：主题选择在“结果窗口”里（影响全部窗口，[SettingsApp.tsx:1645](src/renderer/settings/SettingsApp.tsx#L1645)），工具栏显示方式在“通用”（[:1250](src/renderer/settings/SettingsApp.tsx#L1250)），权限卡片塞在“通用”（[:1285](src/renderer/settings/SettingsApp.tsx#L1285)），“划词获取”与“应用过滤”是两套按应用配规则的分区（[:1684](src/renderer/settings/SettingsApp.tsx#L1684)、[:1781](src/renderer/settings/SettingsApp.tsx#L1781)），4 个分区导航文案 ≠ 标题；语言区把“AI 回复语言”和“翻译语言对”混在一起，且 `defaultLanguage` 实为反向译文语言，标签反直觉。
- 设计系统只有颜色 token：77 处硬编码 `border-radius`、10 档魔法字重、`--radius-*` 仅用 4 次，残留硬编码色与 slate 回退色，暗色主题块覆盖不对称，死 CSS 若干（[styles.css:9](src/renderer/styles.css#L9)、[settings.css:11](src/renderer/settings/settings.css#L11)、[result.css:698](src/renderer/result/result.css#L698)、[themes.css:125](src/renderer/themes.css#L125)）。
- 无障碍：工具栏 `outline: none` 且测试把这个缺陷钉住了（[toolbar.css:116](src/renderer/toolbar/toolbar.css#L116)、[ToolbarApp.test.tsx:880](src/renderer/toolbar/ToolbarApp.test.tsx#L880)）；radiogroup/listbox 无键盘导航（[SettingsApp.tsx:1189](src/renderer/settings/SettingsApp.tsx#L1189)、[ResultApp.tsx:1131](src/renderer/result/ResultApp.tsx#L1131)）。
- 隐式行为：点“测试连接/获取模型”会先把整份脏草稿（含密钥）静默保存（[SettingsApp.tsx:856](src/renderer/settings/SettingsApp.tsx#L856)）；ask 模式复制丢掉多轮 transcript（[ResultApp.tsx:619](src/renderer/result/ResultApp.tsx#L619)）。

### 工程安全网

- CI 已有且 `pnpm verify` 内含 fmt + clippy（[verify.yml:33](.github/workflows/verify.yml#L33)、[package.json:38](package.json#L38)）。
- 缺：打包/产物校验作业、依赖审计与密钥扫描、`.mm` 的 `-Werror`（[build.rs:13](src-tauri/build.rs#L13)）、契约黄金样本测试（Rust `ActionSnapshot.errorCode` 在 TS 无对应字段、状态枚举两端不同：[models.rs:957](src-tauri/src/models.rs#L957) vs [schemas.ts:682](src/shared/schemas.ts#L682)）、字符计数语义统一（Zod UTF-16 单元 vs Rust 标量 vs 字节）。
- `cargo test --test-threads=1` 的真实约束只有 macOS 真 `NSPasteboard` 测试，可尝试放宽以加速。

---

## 二、修改计划

### 阶段 0 · 安全网先行（0.5–1 天，低风险）

1. 契约黄金样本：Rust 侧序列化真实 `ResultSessionSnapshot`/流事件为 JSON fixture，vitest 用现有 schema 校验；同时统一 `Point` f64/i32（[schemas.ts:44](src/shared/schemas.ts#L44) vs [models.rs:829](src-tauri/src/models.rs#L829)）。
2. 单一真源：默认提示词、默认捕获规则（现三处：[schemas.ts:543](src/shared/schemas.ts#L543)、[constants.ts:147](src/shared/constants.ts#L147)、[models.rs:388](src-tauri/src/models.rs#L388)）、语言列表由脚本从一侧生成另一侧 + CI 等值断言，顺手修掉第 11 条。
3. 把 Rust 的快捷键解析器真值语料搬到 vitest；`migrateAppSettings` 显式拒绝 `version > SETTINGS_VERSION`。
4. 抽取原生纯谓词（退化片段过滤、身份匹配、边界容差、手势分类、opaque 判定、非文本粘贴板判定）为可宿主编译单元并加测试；CI 加打包 + 产物校验作业、`cargo deny`、密钥扫描。

### 阶段 1 · P0 稳定性与正确性（2–3 天，低-中风险）

按上表 1–17 逐项修，顺序建议：

1. 两端原生泄漏与无界等待（1/2/3）。
2. 微信信任缓存播种（4），同时加 pid 变更清空与子树扫描 deadline。
3. Windows 密码三态与 helper 保活（6/7）、会话墓碑有界化（5）、启动读盘移出主线程（8）。
4. TS/Rust 一致性（9/10/11/12/16）与前端三处（13/14/15）可并行。

每项配一个能复现的测试；原生项走阶段 0 抽出的纯函数测试。

### 阶段 2 · 响应效率（2–3 天，中风险）

- macOS：手动路径两段式释放 `captureMutex` 并接入 generation；`runningApplicationWithProcessIdentifier:` 取代全表扫描；剪贴板轮询 2–4ms 起步指数退避、探针重试 10ms 且受 generation 约束；`windowTitle` 延迟读取；`running` 纳入所有等待循环谓词。
- Rust：设置读路径改 `Arc` 快照（避免写锁跨 fsync 拖住选区线程）；`schedule_close_unpinned_results` 改单一定时任务；`consuming_selection_ids` 改 RAII guard。
- 前端：rich-markdown 判定 latch 后跳过正则；打字机用追加偏移替代 `startsWith/slice`。
- Windows（只能靠 CI + 真机，放最后、每步最小）：`toolbar_operation` 锁收窄；hook 改事件驱动重装 + 双槽去重；快照超限降级而非放弃整次捕获；helper 管道改阻塞读 + `CancelSynchronousIo`（先加 trace 计数观察）。

### 阶段 3 · 兼容性泛化与结构（4–6 天，中风险，依赖阶段 0 的测试）

- macOS：声明式 AppProfile 表（三级匹配：bundle 精确 → bundle 前缀 → 名称后缀），字段含 `axBudgetMs / retryProfile / clipboardPolicy / applescriptDialect / surfaceGuard`。先做**语义等价搬迁**（纯移动 + golden 测试），再把微信的 opaque-window 守卫泛化成 profile 能力，供 QQ/钉钉/Telegram 等画布聊天复用。
- Windows：`selection.rs` 拆 6 个模块（helper_proto / app_profiles / clipboard_route / uia_capture / hooks / worker），每步纯 `mod` 移动由 CI 编译验证；把纯策略谓词下沉到 `src-tauri`，让 macOS 宿主 CI 也能跑那约 1200 行测试；随后引入同构 `AppProfile`（保留 first-match-wins 顺序）；PDF owner 校验改为“拒绝可识别的外部 owner”；合并 `parent_capture_accepts` 双胞胎；补 `drag_duration_valid` 丢失 mouse-down 的回退。
- 协议层：把 [protocols.rs](src-tauri/src/protocols.rs)、[openai_protocol.rs](src-tauri/src/openai_protocol.rs)、thinking 里对同一枚举的约 8 处平行 match 收成每协议一个模块或 `ProtocolAdapter` trait（行为已被测试钉住，低风险）。
- `runtime.rs`（4400+ 行）拆 `runtime/{selection,settings_ipc,sessions}.rs`；provider CRUD 与迁移阶梯表驱动化；请求任务里的不变量 `expect` 改错误返回。

### 阶段 4 · 设置界面重构与美化（4–6 天，中风险）

- 新 IA（7 区重新归位）：
  1. 常规（启用/触发/快捷键/关闭行为）
  2. 权限与诊断（独立）
  3. 外观（主题、工具栏显示方式、结果字号、透明度）
  4. AI 服务商
  5. 工具栏动作
  6. 语言与翻译（改名“回复语言”/“翻译方向”，加显式交换按钮替代静默互换）
  7. 捕获与过滤（Windows 合并两区，macOS 仅过滤）

  导航文案与 h2 由同一 meta 生成，杜绝再次漂移。
- 字段级校验：`buildSettingsUpdate` 返回字段键错误映射（[settingsForm.ts:83](src/renderer/settings/settingsForm.ts#L83)），错误定位到控件而非只在顶部横幅；应用列表改标签编辑器（修第 14 条）；“测试连接/获取模型”改为显式提示后再保存。
- 组件拆分：`settings/sections/*`、`settings/dialogs/*`、`ProviderCard`，`SettingsApp.tsx` 2432 行降到每区 <300 行；sessionStorage 活跃分区做旧→新 id 映射，guidance focus id 保持 `providers`/`actions` 稳定；预算约 10–14 个测试更新（[SettingsApp.test.tsx:134](src/renderer/settings/SettingsApp.test.tsx#L134) 起）。
- 视觉：补 `--space-1..8`/`--text-xs..lg`/`--weight-*`/`--radius-xs..pill`/`--on-accent`/`--scrim`/`--border-subtle`，清掉硬编码 radius/字重/颜色与死规则，补齐暗色主题块对称性；扩展 [themes.test.ts:60](src/renderer/themes.test.ts#L60) 成 token 守护测试（token 块外禁止 hex/rgba/数字 radius/字重）。

### 阶段 5 · 弹出窗口美化与无障碍（2–3 天，低风险）

- 工具栏：恢复可见 focus ring 并重写那条钉住缺陷的 CSS 字符串断言；间距/圆角/阴影走 token；悬停与忙态反馈统一。
- 结果窗：标题栏层级与翻译方向排版收敛、合并重复定义（[result.css:484](src/renderer/result/result.css#L484)、[:500](src/renderer/result/result.css#L500)）与顺序敏感的 `.markdown-body`（[:767](src/renderer/result/result.css#L767)、[:810](src/renderer/result/result.css#L810)）、去掉 slate 回退色。
- 模型下拉改正确 listbox 语义 + 键盘导航；radiogroup 方向键 + roving tabindex；ask 模式复制序列化 turns。

### 阶段 6 · 收尾（1 天）

- 文档与代码对齐：Windows 密码未知策略、“无盲目重装 hook”、微信门闩现状与新增 trace 行、版本表演进到 0.4.x。
- trace 的 24 字符选区前缀改由独立 `TRACE_CONTENT` 开关控制，默认只记长度/哈希。
- 选区 vs dismiss 排序改单调 emit 序号（不再依赖 `system_clock`，[selection_bridge.mm:282](apps/macos/native/selection_bridge.mm#L282)、[runtime.rs:3385](src-tauri/src/runtime.rs#L3385)）。
- 设置写入加 `.bak`；Rust 设置结构补 `deny_unknown_fields` 或明确记录宽松策略。

---

## 三、验证与回归

- 每阶段结束跑 `pnpm verify`（typecheck + vitest + build + fmt + clippy + rust test + check）；Windows 侧跑 `verify:windows`（本机无法编译 Windows，只能 CI/真机）。
- 原生与捕获改动额外走手册九项序列 + 专项：
  - 微信图片查看器（拖动/双击/边缘点击）、微信聊天文字拖动与双击
  - WPS、浏览器内嵌 PDF
  - Terminal/iTerm2 与 GPU 终端、Safari/Chrome AppleScript 路径
  - Electron（VS Code/Cherry Studio/ChatGPT）
  - 连续快速划词、闲置 10 分钟后首次划词
  - 100/125/150/200% 缩放与负坐标副屏

## 四、明确不做

- 不引入新依赖或 UI 框架。
- 不改成固定延时/精确 HWND 匹配来“解决”竞态（手册已记录该路线失败）。
- 不动已验证的流式 flusher 租约机制。
- Windows 高风险项（阻塞管道读、hook 重装策略）排在最后并先加 trace 观察。
- 设置页搜索框等低价值功能不做。

## 五、工作量与顺序

总量约 15–20 个工作日。建议顺序 0 → 1 → 2 → 3；阶段 4/5 与阶段 3 代码域不重叠，可并行。
