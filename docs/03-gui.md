# GUI 优先交互方案

人不再打开 cmd 输入 `clew mint` / `clew forward`。人类操作全部在窗口和托盘里完成：点击、填写少量字段、或由程序自动生成。CLI / MCP 只服务 agent 和脚本。

对应：[00-pain-points.md](00-pain-points.md)、[01-design.md](01-design.md)、[02-ux.md](02-ux.md)。

## 1. 原则

1. **两条入口，两种用户**
   - 人 → GUI（windui）
   - agent → 本机 MCP / Local API
   两者都打进同一个 controller，不各连一套远程。

2. **能自动生成的不要让人敲**
   ControllerId、邀请文件名、本地监听端口、聊天稿、MCP 配置片段，默认程序生成；人只改「叫什么、给谁、权限松还是紧」。

3. **状态在窗口里，不在终端滚动日志里**
   设备在线、路径、活动、转发、传输进度都是列表和徽章。

4. **第一次打开就能用**
   安装或解压后双击 `Clew`：没有 controller 就创建身份并启动；有就显示主窗。不先跑 `clew controller`。

5. **Controller 与 Host 共用交互语言，但不强求同一品牌外观**
   Controller 保持 Clew 自身管理界面；朋友端 Host/Connector 由 `OutfitProfile` 渲染，可使用定制名称、图标、Logo、颜色和字符串。两边共享连接状态语义与组件规则，不要求视觉资产完全相同。

6. **朋友端是 tray-first，不是 window-first 常驻**
   首次打开用主窗口确认连接；确认成功后窗口可以完全退居托盘。窗口只是状态视图，托盘图标才是“Clew 仍在运行”的长期存在感。

## 2. 谁看见什么

```text
你双击 Clew.exe / Clew.app
        → Controller 主窗 + 托盘
        → 需要时自动写 MCP 配置（可选勾选）

朋友打开对应平台的 Alice-Clew-Windows.zip / Alice-Clew-macOS.zip 中「① 使用这台电脑」
        → Host 小窗 + 托盘
        → 无设置页也能连上
若目标机无法联网，同一份包在附近联网电脑打开「② 只帮助其它电脑连接」
```

命令行仍存在，但帮助页第一句写：日常请打开图形界面。`clew invite` 等命令是 GUI 按钮背后的同一 Local API。

## 3. Controller 主窗（你这边）

windui，一窗多页，默认一页就够用。

### 3.1 首页「设备」

上栏：

- 标题 Clew
- 徽章：本机已就绪 / 启动中
- 按钮：**邀请合作者**、**打开 MCP 说明**
- 按钮：**分发穿搭**（进入 Distribution Studio；也可从邀请对话框进入）

首页首先按 **Site / 合作者地点** 分组，再显示设备。单机邀请看起来仍像一行设备；多机时才展开站点。

例如：

```text
Alice 实验室                 已连接
  CryoEM-PC                   已连接
  GPU-01                      已连接
  Lab-PC                      连接助手 · 已就绪

Bob                           离线
  Bob-Laptop                  离线
```

列表每行一台设备：

| 列 | 内容 |
|---|---|
| 名 | 默认 hostname（如 GPU-01），可改显示名；邀请名不复制到每台设备 |
| 状态 | 等待上线 / 已连接 / 重连中 / 睡眠（仅收到明确 suspend 时）/ 离线 |
| 路径 | 对人显示「已连接」；鼠标悬停才是中继或直连 |
| 活动 | 空闲 / 读文件 / 命令 / 传输 34% |
| 操作 | 文件、转发、代理、改名、停止这台、详情 |

空状态不是空白：

```text
还没有合作者
[ 邀请合作者 ]
把生成的文件发给对方，对方双击即可。
```

设备上线：托盘气泡「Alice 实验室 / GPU-01 已连接」，对应设备行变绿。
Site card 的更多菜单区分：**停止继续加入**、**作废这份分发包**、**停止整个 Site**。其中“停止继续加入”只关闭 bootstrap，不影响已经加入的设备；“作废分发包”才同时撤销通过该 invite 加入的设备。

### 3.2 对话框「邀请合作者」

人只填（均可有默认）：

- 称呼：默认 `合作者`，可改 `Alice`
- 权限：两个大选项 **宽松（研究用）** / **只读**，高级里才展开根目录、是否允许 Shell
- 系统：Windows / macOS / Linux，可多选；同一个 Site 复用相同逻辑 `site.clew`，但 **每个平台分别生成一个 Site Kit**，不打 universal fat zip
- 邀请默认就是 **site-capable Site Kit**，不再让控制者提前判断对方是否需要 gateway。单机只用其中一个目标入口；需要连接助手时直接复用同一份包。
- 外观：默认自动选用户设为 default/最近使用的 Outfit，例如 `Huang Lab Light`；通常不用改，旁边只有 **预览 / 更换 / 编辑穿搭**。

点 **生成**：

1. 程序创建 enrollment、打包 artifact、签名流水线若已配置则带上
2. 打开系统「另存为」；单平台默认如 `Alice-Clew-Windows.zip`，多平台则一次生成 Windows/macOS/Linux 各自的分发包
3. 同时生成聊天稿，窗内只读文本 + **复制说明**
4. Site 列表立刻出现卡片「Alice · 等待电脑加入」；具体设备名要等真实电脑 enrollment 后按 hostname 出现

生成物统一为带平台后缀的 Site Kit，例如 `Alice-Clew-Windows.zip` / `Alice-实验室-Clew-macOS.zip`。同一平台的多台电脑可复用同一份 Site Kit。Generator 可附带两个显眼的人话启动入口“① 使用这台电脑 / ② 只帮助其它电脑连接”，但同一平台 runtime/binary 仍是同一份；朋友不填 IP/端口/code。普通单机只打开①即可。
每个平台包内的说明固定写“请先完整解压，程序和 `site.clew` 放在一起”。聊天稿同样带这句，降低微信/邮箱/网盘拆包后只剩程序的概率。

人不需要知道 mint/pack/worm。失败时窗内三行人话 + 「复制诊断」（才含技术日志）。

### 3.2.1 Distribution Studio / 分发穿搭

这是控制者侧独立页面/窗口，不给朋友看。主入口可以是 Controller 首页“分发穿搭”，也可以从邀请对话框点“编辑穿搭”。完整规格见 [05-distribution-studio.md](05-distribution-studio.md)。

首页是穿搭卡片，而不是空配置表：

```text
Clew Original          [默认]
Huang Lab Light        [最近使用]
Friendly Minimal
Institute Clean

[ 新建穿搭 ]
```

支持 **使用、编辑、复制、设为默认、导入/导出、删除**；内置 preset 只能复制/重置，不能删。

编辑器采用四步：

```text
预设 -> 名称与图标 -> 颜色与文案 -> 预览并生成
```

右侧 live preview 始终能切换：

```text
主窗口 | 连接助手 | 托盘 | Site Kit
```

常用字段只显示人话：程序名、窗口标题、Logo、App Icon、主色、欢迎/连接文案。完整 string resource 和各 locale 放“高级/翻译”。

用户拖一个 PNG/SVG 即可；程序自动生成 Windows/macOS/Linux icon 资源和 tray 状态变体。若 16–24px tray preview 不可辨识，立即 warning，并允许单独提供 tray base icon。

Outfit 与权限 profile 分开：换外观不改变 Shell/filesystem 权限。

### 3.3 对话框「转发」

从设备行点「转发」：

- 对方端口：人填 `3000` 或下拉最近用过的
- 本机端口：**程序自动占一个空闲端口**，显示为只读，可点「改」才编辑
- 生成后主窗出现一条转发，并显示 `http://127.0.0.1:13107`
- 按钮：**在浏览器打开**、**复制地址**、**停止**

不提供「远端监听」入口（v1 后置）。

### 3.4 对话框「代理」

- 类型：SOCKS5 / HTTP，默认 SOCKS5
- 本机端口：同样自动选 1080，占用则 1081…
- 显示 `socks5://127.0.0.1:1080`，复制给浏览器/其他程序
- 列表里可停

### 3.5 对话框「文件」

- 拖入文件/文件夹 → 发到对方（默认对方家目录或政策 root）
- 或「从对方取」：填相对路径或从简单目录列表点选（列表走 Glob，有上限）
- 进度条在主窗设备行和底部任务条两处都能看
- 可取消

### 3.6 页「本机 / Agent」

给自己看的，不给朋友：

- MCP 地址 `127.0.0.1:4877`
- 按钮：**复制 Cursor 配置**、**复制 Claude 配置**、**检查本机**（doctor）
- 按钮：**备份控制者身份** / **从备份恢复**；第一次成功生成邀请后非阻塞提醒一次“建议备份”
- 页签/入口：**活动**，查看本机最近的 Read/Shell/File/Forward 摘要，支持按 Site/设备筛选和清空
- 勾选「登录后自动打开 Clew」（可选，默认关，避免吓到；控制者可以开）

### 3.7 Controller 托盘

- 左键：显示主窗；
- hover：`Clew · 3 台已连接 · 1 个任务进行中` 这类简短摘要；
- 菜单：显示主窗、邀请合作者、设备状态摘要、退出 Clew；
- 上线/传完/对方退出：系统通知一条。

关闭主窗 = 藏到托盘，**不退出 controller**。退出只在托盘“退出 Clew”。Host 也遵循同一条“X 只隐藏、Exit 才退出”的一致性原则。

### 3.8 活动与灾难恢复

“活动”只服务控制者自己，不做企业审计。每行显示时间、Site/设备、动作、人话目标、结果，例如：

```text
16:02  Alice 实验室 / GPU-01  Read   D:\\proj\\README.md   成功
16:05  Alice 实验室 / GPU-01  Shell  python train.py        运行中
```

默认不显示/保存文件正文、stdout/stderr 全文、环境变量。日志有界并可一键清空。

ControllerKey 恢复入口必须明确：从备份恢复只用于空 Controller state。恢复完成先进入“检查恢复的设备”页面，远程访问保持暂停，用户确认设备/Site 后再恢复；旧 bootstrap 不自动重开。没有备份时 UI 写“以前的连接不会自动迁移，需要重新邀请”，不提供“强制接管旧设备”按钮。

## 4. Host / 连接助手窗（朋友这边）

仍然极简，但按 GUI 优先补全：

打开即全屏语义只有一件事：**连上或正在连**。

```text
Clew
已连接到「你的名字」
当前空闲             ← 有活动时显示“正在读取文件 / 命令运行中 / 传输 34%”

可以关闭这个窗口，Clew 会继续在托盘运行。

[ 隐藏到托盘 ]    [ 退出并断开 ]
```

托盘/菜单栏是朋友端的长期入口：

- 图标：绿=已连接/已就绪，琥珀=连接/重连中，灰=主动暂停，红=持续失败且需要处理；
- Windows hover：`Clew · 已连接 · 当前空闲`；helper-only：`Clew · 连接已就绪 · 正在帮助 2 台电脑`；
- 左键/双击：恢复主窗口；
- 右键菜单：状态摘要（不可点）→ **显示 Clew** → **暂时断开/重新连接** → **复制状态** → 分隔线 → **退出并断开**；
- helper-only 把快捷动作写成“暂停帮助连接/恢复帮助连接”；
- 主窗口的最小化按钮和 `X` 都只隐藏到托盘，不终止 runtime；
- 第一次隐藏时只弹一次“Clew 会继续在托盘运行”的说明通知，无需朋友确认。

没有邀请、没有转发设置、没有命令框。朋友不配置网络。高级诊断不堆进右键菜单；从主窗口“详情/复制诊断”进入。
如果启动时缺 `site.clew` 且本机没有可唯一恢复的 membership，显示独立恢复页：

```text
还缺一个邀请文件
请把 site.clew 和这个程序放在同一个文件夹，
或把 site.clew 拖到这里。
[ 选择邀请文件 ]
```

若从 zip 临时目录运行则改为“请先全部解压这个压缩包，再打开程序”。被 Controller revoke 时显示“这台电脑的连接已被对方停止”，不无限重连。原 ControllerKey 不再可达时不自动接受任何新 identity。

如果这台电脑是 helper-only，窗口仍然极简：

```text
Clew
连接已就绪
正在帮助 2 台附近电脑连接

这台电脑不会开放文件和命令

[ 退出并断开 ]
```

如果普通目标节点本身有上行且 Site policy 允许，它可以后台自动兼任连接助手，不要求朋友切换模式或重开程序。谁先启动都可；目标节点会自动寻找同 Site 的可用 helper。完整规则见 [04-site-connector-ux.md](04-site-connector-ux.md)。
同一机器第二次打开 Site Kit：若本地已有 membership 就复用 DeviceId；若 runtime 正在运行，第二进程只恢复现有窗口。不会因为双击两次在 Controller 里多出一台设备。

朋友把 helper-only 主窗口收进托盘后，连接助手继续工作；“窗口看不见”不能被解释成 helper 离线。

首次 SmartScreen：不在我们窗里「绕过」，生成邀请时说明里已经写了点哪里。窗里若 20 秒还在连，只加一句「还在连，把窗口留着」。

## 5. 程序自动生成的清单

| 事项 | 谁生成 | 人是否需要改 |
|---|---|---|
| Controller 密钥 | 首次开主窗 | 否 |
| Enrollment / 签名 | 点邀请 | 否 |
| Site Kit 文件名 | `{称呼}-Clew-{平台}.zip` / Linux `.tar.gz` | 另存为时可改 |
| 聊天稿 | 模板填称呼和权限 | 可复制后手改 |
| 本机转发/代理端口 | 找空闲口 | 可选改 |
| MCP JSON | 按钮复制 | 否 |
| DeviceId | enrollment | 否，界面永不展示除非详情 |
| 设备显示名 | 首次 enrollment 取 hostname；碰撞组加固定 5 字符 DeviceTag，如 `GPU-01-K7M4Q` | 可在 Controller 改名 |
| SiteId / connector 选择 | Controller + 自动发现 | 否，朋友永不填写 |
| 多机 bootstrap pass | 邀请 Site 时自动生成 | 否，部署完成自动关闭 |
| Outfit / ClientFlavor | Distribution Studio | 邀请时默认复用已有穿搭；朋友无需操作 |
| 默认设备（仅一台在线） | MCP | 否 |

人真正要输入的只有：称呼、权限档、对方端口号、要传的文件。其余点击。

## 6. 状态怎么呈现

所有状态来自 Local Controller API，GUI 只订阅：

```text
controller.ready
device.list          # site, name, hostname, presence, executable, connector, path
activity.list
forward.list
proxy.list
transfer.list
task.list
```

推送用 API 通知或短轮询（v1 2s 即可）。不要在 GUI 线程打 iroh。

路径文案跟 [02-ux.md](02-ux.md) §3.2：大字不说 Relay。活动文案跟 §3.3。

转发中途 connection 丢失：该条标记「已中断」，不要假装 localhost 还通。

## 7. 与 CLI 的关系

| 人在 GUI 点 | 背后 Local API |
|---|---|
| 邀请合作者 | `invite.create` + `pack` |
| 复制说明 | 读生成的 message |
| 转发 | `forward.add` |
| 代理 | `proxy.add` |
| 拖文件 | `transfer.put` |
| 停止 | `*.remove` / `cancel` |
| 改名 | `device.rename` |
| 停止这台 | `device.revoke` |
| 停止继续加入 | `invite.close` |
| 作废分发包 | `invite.revoke` |
| 停止 Site | `site.revoke` |
| 查看/清空活动 | `activity.list` / `activity.clear` |
| 备份控制者身份 | `controller.backup_export` |
| 检查本机 | `controller.doctor` |

CLI 保留给脚本和排障，命令与按钮一一对应。文档和 `--help` 写明等价按钮在哪。

Agent 不走 GUI。

## 8. 窗口结构（windui）

Controller：

```text
App 960×640
  header（品牌 + 就绪徽章 + 邀请）
  tabs: 设备 | 活动 | 本机
  设备页：list_signal(devices) + 底栏任务
  dialog: 邀请 / 转发 / 代理 / 从对方取文件
  tray
```

Host：

```text
App 420×280
  状态大字 + 活动一行 + 退出按钮
  tray
```
Linux v1：不依赖这个 tray/window contract；使用前台终端状态行 + Ctrl-C 退出。桌面 tray 等后续有可靠实现再加。

### 8.1 后续高级 Service Runtime UI

Service mode 不进入普通邀请默认流程，只放在“高级 / 长期在线”里：

- Linux：`systemd --user` / `systemd system` 两档，明确显示“当前用户”或“整台机器”；
- Windows：**安装 Windows Service**，需要管理员权限；Service 负责 runtime，托盘/GUI 只是 Local API client；
- 状态统一显示 `未安装 / 已安装未启动 / 后台运行 / 已停止 / 需要权限`；
- 按钮分成 **启用后台长期可用**、**停止后台服务**、**卸载服务**，不能把“关闭窗口”伪装成停止 service；
- machine-level service 若启用 `EXECUTE`，GUI 必须额外显示执行身份和 filesystem/Shell policy；默认建议 Connector-only。

`assets/brand`、`assets/icons` 是 **Clew Original** Outfit 的默认资源和状态语义基线。自定义 Outfit 可以覆盖品牌图形/颜色，但不得改变 GREEN/AMBER/GRAY/RED 等状态含义或退出/隐藏等交互语义。

## 9. 分期（插进原 vertical slice，不另起炉灶）

V0 只做 Controller 单实例、Local API、stable ID/schema skeleton 和 GUI 空列表；真正 ControllerKey/DeviceKey enrollment、Host UI 与第一条 Read 在 V1 一起闭环。GUI 从 **能点的那一天** 就要存在，否则人又回到 cmd。

| 切片 | GUI 必须同时有 |
|---|---|
| V0 | Controller 主窗能开、显示「未就绪/已就绪」，无设备时空状态 |
| V1 第一条 Read | Windows/macOS Host 窗 + tray；Linux foreground；Controller 设备行「已连接」；邀请对话框能产出 Site Kit；X/最小化只隐藏、Exit 才断开 |
| V1 边界同时验收 | hostname 设备名、第二次启动复用、`site.clew` 恢复页、停止这台、Controller backup、Activity 最小视图、Linux foreground |
| V1.25 穿搭 | Distribution Studio 卡片库 + preset + app/tray/Site Kit live preview；邀请可选 Outfit |
| V1.5 多机/内网 | Site 卡片 + helper-only 人话窗口；两台电脑各双击一次、顺序无关即可连通 |
| V2 Agent | 「本机」页复制 MCP 配置、doctor |
| V3 重连 | 状态徽章切换，不新增页 |
| V4 转发代理 | 设备行按钮 + 对话框 + 打开浏览器 |
| V5 文件 | 拖放 + 进度条 |
| V6 签名 | 邀请流程不改，失败提示改人话 |
| V7 高级服务 | “高级 / 长期在线”页：Linux systemd user/system、Windows Service 的安装/状态/停止/卸载；machine EXECUTE 显示执行身份与 policy |

不允许出现「协议已经 Read 通了，但生成邀请还得打命令」的中间发布。

## 10. 验收（人话）

1. 你双击 Clew，不打开 cmd，能点「邀请合作者」得到一个文件和一段说明。
2. 朋友完整解压 Site Kit 后双击其中启动入口，窗里从「正在连接」到「已连接」，能点退出。
3. 你在 `Alice` Site 下看到真实设备名（例如 GPU-01）变已连接，不必刷新终端。
4. 点转发、填 3000、系统给本机端口，点「在浏览器打开」。
5. 拖一个文件到窗口，进度在列表里走完。
6. 点「复制 Cursor 配置」后，agent 能 Read。
7. 关主窗后托盘还在，转发仍有效；托盘退出后能力消失。
8. 朋友点退出后，你的列表变成离线，localhost 转发标中断。
9. 内网场景中，朋友不输入 IP/端口，在联网电脑与目标电脑各双击一次即可看到目标电脑变“已连接”；关闭当前连接助手后若 Site 有其它可用 helper，应自动恢复。
10. 朋友看到“已连接”后点最小化或 `X`，窗口消失但托盘图标仍显示绿色，agent 继续可用；从托盘点“显示 Clew”恢复窗口，状态不丢。
11. 只有窗口按钮或托盘菜单里的“退出并断开”才真正停止 Host；“暂时断开”后托盘仍在且可一键重新连接。
12. 你能在 Distribution Studio 从 `Research Lab` preset 复制一套穿搭，换 PNG 图标、程序名、主色和中文状态文案；主窗口/连接助手/托盘/Site Kit preview 同步变化。
13. 用同一 Outfit 邀请 Alice 和 Bob 时，两人的 `site.clew` 不同但外观一致；朋友仍然只需双击，没有额外主题选择步骤。
14. 同一 `Alice 实验室` Site Kit 在 GPU-01/CryoEM-PC/Lab-PC 打开后显示三台不同名字；Lab-PC 若 helper-only 不能被 MCP Read 选中。
15. 程序和 `site.clew` 被拆开时显示固定恢复页；把文件拖回即可继续。同机器双击两次仍只有一个 DeviceId。
16. 设备/Site 菜单能分别执行“停止这台 / 停止继续加入 / 作废分发包 / 停止整个 Site”；revoke 后 host 显示已停止而不是循环重连。
17. Controller 能备份身份、查看/清空本机 Activity；无备份丢 key 时 UI 明确要求重新邀请。
18. 合盖/睡眠后醒来自动重连；Linux 无 tray 时前台运行且 Ctrl-C 退出。
