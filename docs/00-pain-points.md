# 需求与痛点

状态：Architecture v1.5 需求基线。

Clew 第一版仍然坚持 **点开即用、便利优先**，但便利不再通过省略身份边界、协议边界或状态所有权来换取。安全机制应尽量前置到打包和 enrollment 阶段，而不是在合作者每次操作时弹确认框。

## 1. 问题从哪来

我们需要让自己的 **agent CLI** 在获得合作者明确授权后，像操作本机能力一样读写对方机器上的文件、运行命令、访问端口、走代理和传文件。合作者不是运维，不能要求他们：

- 安装和理解 Tailscale / VPN / DERP / 节点 ID；
- 开端口映射、申请公网 IP 或维护 SSH 跳板；
- 注册账号、抄配对码、配置复杂 ACL；
- 在公司内网额外部署一套网络基础设施；
- 为了临时协作长期安装一个重量级远控管理平台。

现成工具通常只覆盖其中一段：

| 工具 | Clew 仍需补上的部分 |
|---|---|
| RustDesk / Deskhub | 远程桌面和人工交互强，但 agent 可编程能力面不是核心；复杂配置也不天然变成一次性交付物 |
| MeshCentral | 管理与 shell 能力完整，但服务端、账号和运维模型偏重 |
| Tailscale | 状态所有权、身份和 NAT 穿透成熟，但要求用户进入一个网络产品模型，不直接提供 agent 工具面 |
| Syncthing | 设备身份、发现、relay 和端到端数据模型成熟，但目标是同步而不是远程 capability runtime |
| wush / iroh-ssh | 跨网 shell 很轻，但没有预配置 collaborator artifact、持久 controller 和 MCP/工具面 |
| 自建 frp / SSH 跳板 | 可控，但需要公网机、端口、密钥和人工配置 |

痛点收成一句：

> 缺少一个「合作者拿到一个预配置 artifact 后直接启动，agent 就能通过本机接口使用远端能力；网络尽量直连、失败自动中继，并能在同一长期会话上动态增加工具、转发、代理和文件任务」的轻量协作工具。

Clew 不是另一个远程桌面，而是 **agent-facing remote capability bridge**。

## 2. 目标用户与场景

- **Controller 用户**：在自己的机器上运行 coding/research agent CLI，需要把一台或多台合作者机器暴露成本地可调用能力。
- **合作者**：收到一个为自己预配置的可启动 artifact，双击后看见窗口/托盘和连接状态，可以随时断开并退出。
- **Host**：合作者机器上的 capability runtime，执行文件、Shell、转发、代理和文件任务。
- **多机 / 内网站点**：目标 host 无法直接访问公网时，同一 Site 中能联网的设备自动提供连接助手能力；朋友不配置 gateway、IP、route。完整体验见 [04-site-connector-ux.md](04-site-connector-ux.md)。

非目标：隐蔽远控、无窗口静默常驻、对抗杀软、绕过操作系统安全机制、未授权接入。

托盘/窗口、连接状态和一键退出是 collaborator 侧的产品边界，不是可选装饰。

## 3. 产品原则

### 3.1 一个可启动 artifact，而不是强求所有平台一个裸二进制

合作者体验必须是：**下载一个 artifact，直接启动，无安装向导、无配对输入。**

平台形式允许不同，但朋友收到的始终是**一份按目标平台生成的 Site Kit**：

- Windows：`*-Windows.zip`，内含签名 portable `.exe` + `site.clew` + 开始说明；
- macOS：签名/公证后的 `.app` 与 `site.clew` 组成 zip/DMG；
- Linux：`tar.gz`/目录包，内含 executable + `site.clew`，v1 以前台模式运行。

“单 artifact”是**一份分发包**的用户体验约束，不再承诺“邀请必须焊进一个裸 exe”；这也避免为每个 collaborator 重新签名/公证程序。

### 3.2 预配置，但不把静态配置当身份

Worm/邀请材料不再定义为“藏在 exe 尾部的长期身份”，而是 **Bootstrap Capability + 配置胶囊**：

- 预埋 controller 身份和连接 hint；
- 预埋 collaborator/Site 名称和 capability policy；
- 单机邀请可以是 one-time enrollment；默认 Site Kit 使用有限部署窗口、有限 claim 的 `SiteBootstrapPass`，让同一份包能自然加入目标机与连接帮手机；
- 必须可验证来源完整性（v1 就要求签名）；
- 每台设备第一次成功 enrollment 后都生成并持久化自己的 DeviceKey，长期身份改由设备密钥承担；
- bootstrap pass/token 对朋友完全不可见，部署窗口/claim 状态由 Controller 自动管理；
- 对邀请内容做混淆/加密可以降低随手 `strings` 暴露，但这不是信任根，也不宣称抗逆向。

合作者仍然不需要输入任何码。

### 3.3 Controller 是唯一长期状态 owner

本机必须有一个长期存活的 **Clew Controller**，唯一拥有：

- iroh Endpoint；
- 已 enrollment 的设备注册表；
- 活跃连接 / session；
- Shell task；
- forward / proxy listener；
- file transfer task；
- reconnect / retry / replay 状态。

MCP、CLI 和未来 GUI 都只是本地 adapter/client，通过 Named Pipe / Unix Domain Socket 等 Local Controller API 操作 controller。

不能让 `clew mcp`、`clew forward`、`clew file` 各自隐式拥有一套远程连接，否则动态能力的生命周期无法定义。

### 3.4 网络路径由 transport 负责，业务恢复由 Clew 负责

v1 使用 iroh：

- 能直连就 Direct；
- 直连不可用时 Relay 保底；
- relay 已经承载业务时仍继续尝试 direct；
- direct/relay 的路径选择和切换交给 iroh 本身；
- Clew 订阅 path 事件用于状态/UI/诊断，不重新实现第二套路径调度器。

必须区分：

- **path change**：同一个 iroh connection 内 relay ↔ direct，业务不感知；
- **connection loss**：整个连接失效，由 Clew reconnect，并按不同任务语义 retry / reattach / resume。

已经建立的任意 TCP 转发连接在底层 connection 真正死亡后，v1 不承诺无损透明迁移。

### 3.5 不弹每次确认框，但权限必须可静态收窄

第一版仍然不弹“是否允许这条命令”的逐操作确认框。

权限改成分层 capability policy：

```text
built-in hard limits
        ↓
signed enrollment policy       # 最大授权边界
        ↓
host local policy              # 只能进一步收窄
        ↓
session capability
        ↓
individual request
```

典型边界包括：

- filesystem read/write roots；
- Shell 是否启用；
- forward 的 listen/destination 范围；
- proxy 类型和监听范围；
- file read/write roots。

可以提供 `research-full` 之类宽松 profile 保持易用，但不能把“易用”实现成“没有权限模型”。

## 4. 必须打通的需求

### 4.1 Controller 与本地接口

Controller 在用户机器上长期运行，CLI/MCP 都通过本地 IPC 使用它。

最低能力：

- `Devices`：设备列表、在线/离线、版本、capabilities；
- `PathInfo`：direct/relay 和诊断信息；
- task / transfer / forward / proxy 的列表与生命周期管理；
- controller 重启后可恢复持久设备身份和必要配置。

Local API 默认不暴露到 LAN；必须有单实例 ownership，避免多个进程争抢同一 endpoint / IPC / listener。
Controller 还必须提供最小管理面：设备改名/撤销、关闭继续加入、作废分发包/整 Site、活动记录，以及 Controller 身份备份/恢复。控制者不能只能等朋友自己点退出。

### 4.2 给 agent 用

MCP 是 adapter，不是 session owner。

v1 提供：

- stdio；
- 当前 MCP Streamable HTTP 的 `POST /mcp`；
- 旧 HTTP+SSE 只作为显式 legacy compatibility，可后置，不与新传输同等设计。

HTTP MCP 默认只监听 `127.0.0.1`，并按当前规范处理 Origin/本地访问边界。

核心工具：

| 工具 | 用途 |
|---|---|
| Glob | 按模式列文件，结果有上限/分页 |
| Grep | 目录内搜索，支持结果上限/超时 |
| Read | 读文件，可按 byte/line range，返回有上限 |
| Edit | 带内容 hash/precondition 的替换，避免陈旧覆盖 |
| Write | 原子写入/替换，受 root policy 限制 |
| Shell | 远程 task，而不是一次 HTTP 生命周期；支持 stdout/stderr、取消、超时、重新 attach |

长状态通过 Clew 自己的 `task_id` / `transfer_id` / `forward_id` 表达，不依赖 MCP transport session。
`Devices` 不能只返回一个模糊名字。它至少返回 Site、设备显示名、观察到的 hostname、在线状态以及 `EXECUTE/CONNECTOR` 能力。执行类工具只允许选择 `EXECUTE` 设备；helper-only 永不成为默认候选。短名只有在在线 executable devices 中唯一时才可直接使用，重名必须返回候选，不能静默挑第一台。

### 4.3 跨网连接

- host 只需主动出站，不要求开放入站端口；
- v1 只实现 iroh transport；
- 公益 relay 适合开发/试用，生产路径必须预留 dedicated/self-hosted relay 配置；
- host enrollment 后以持久 DeviceKey 建立身份；IP 地址不是身份；
- Tailscale/DERP 不在 v1 做“占位式双栈”，等真实需求出现后作为完整 transport/backend 单独设计。
无论 Direct、公共 iroh relay 还是同 Site Connector，业务安全边界都固定为 **Target↔Controller InnerSession**。Connector 只转发已经加密的 outer payload；它可以看到包长/时序/连接关系，但不能看到 Read 路径、Shell 命令、文件内容或 stdout/stderr。若这层 E2E 尚未实现，Connector data plane 不得上线。

### 4.4 会话期动态能力

连上之后，不重启 host 即可：

- 动态增加 / 删除 TCP 转发；
- 动态增加 / 删除 SOCKS5 / HTTP CONNECT 代理；
- 独立的大文件/目录传输任务，支持进度、取消和断点；
- Shell 作为可重新查询/attach 的远端任务运行。

所有本地 listener 由 controller 持有，因此发起它们的 CLI/MCP 调用结束后能力仍然存在，直到显式删除、策略撤销或 controller 退出。

### 4.5 协议演进

Host 很可能比 controller 老很多，v1 就必须有 wire compatibility 设计。

至少分开三个版本概念：

- software semver：只用于诊断和 UI；
- wire major / ALPN：不兼容的大版本边界；
- monotonic capability version + feature set：同一 wire major 内的能力协商。

所有 request/response 必须带稳定 request id、明确错误类型和大小/超时边界；协议编码优先使用可演进 schema（如 protobuf/prost），不把 Rust 内部 enum 的二进制布局直接当长期 wire contract。

### 4.6 文件传输

不能用 `Write + base64` 搬大文件。

最低要求：

- chunk hash / 完整性；
- resume；
- 进度；
- cancel；
- 有界并发；
- 目录树传输；
- 明确覆盖/冲突策略；
- 受 filesystem policy 限制。

### 4.7 Host UX：主窗口确认，托盘常驻

Windows/macOS 的朋友端是 **GUI-first + tray-first runtime**：

- 第一次启动时显示主窗口，让朋友明确看到“正在连接 → 已连接 / 连接已就绪”；
- 主窗口确认成功后可以随时最小化或点右上角 `X`，Clew **隐藏到系统托盘/菜单栏但继续运行**；
- `X` 与最小化的语义保持一致，不因为当前有无任务而突然变成“断开”；真正停止必须走明确的“退出并断开”；
- Windows 托盘图标悬停显示短状态；macOS 菜单栏以图标 + 点击菜单承载同样信息，不能把关键状态只放在 hover；
- 托盘右键/菜单只保留少量快捷动作：显示窗口、暂时断开/重新连接、复制状态、退出并断开；高级诊断不占普通菜单；
- 托盘图标始终可见地表达运行状态，普通 friend UI 不显示 Relay/Direct 等网络术语；
- 朋友把主窗口收起后，agent 的 Read/Shell/File、Site Connector 等能力继续运行；
- helper-only 机器同样可以收进托盘，不需要长期占据桌面；
- 可选“登录后自动保持可用”；一旦启用，后续登录应直接进入托盘，不在每次开机把主窗口强行顶到前台。

### 4.8 分发穿搭：控制者方便定制，朋友零额外步骤

Controller 侧必须提供 Distribution Studio / 分发穿搭能力，让控制者不改源码即可为 Site Kit 设置：

- 程序显示名、窗口标题、Site Kit 文件名；
- app icon、Logo、可选 key visual、tray base icon；
- 少量品牌色 token；
- friend-side 状态/按钮/托盘/说明字符串资源及多语言；
- `开始这里` 和聊天稿文案。

必须提供可复用预设和 live preview；邀请时默认选择已有 Outfit，普通邀请不增加新的必填项。

穿搭只影响外观与文案，不承载权限或秘密。正式模型为 `OutfitProfile -> platform ClientFlavor -> Site Kit`；修改平台 app icon/name 等资源时在签名前生成新的可复用 ClientFlavor，而 Alice/Bob 的 `site.clew` 变化不应导致每份邀请重新签整个 app。完整设计见 [05-distribution-studio.md](05-distribution-studio.md)。
### 4.9 丢包、重开、撤销与灾难恢复也必须是产品路径

- `site.clew` 查找顺序固定为：显式打开/拖入 → 程序同目录 → 已有本机 membership 恢复 → 人话缺失页面；不扫全盘、不猜 cwd；
- 缺少 sidecar 时显示“还缺一个邀请文件”，提供“选择邀请文件”，并提示把 `site.clew` 放回程序旁边；从压缩包临时目录启动则提示“请先全部解压”；
- DeviceKey 存在当前 OS user 的平台 state 目录，不放在 exe 旁；同一机器/user/site 第二次打开复用 DeviceId，有 runtime 时第二进程只唤起窗口；
- 同一 Site Kit 在多机首次加入时，设备默认名来自 hostname；发生碰撞时不用 `(2)`/`(3)`，而给碰撞组统一加由 DeviceId 派生的固定 5 字符 DeviceTag，例如 `GPU-01-K7M4Q`；tag 持久、非秘密、可由控制者改名覆盖；邀请名只是 Site/合作者名；
- Controller 必须支持“停止这台”“停止继续加入”“作废这份分发包”“停止整个 Site”；
- ControllerKey 必须有加密备份入口。无备份丢失时明确告诉控制者需要重新邀请；旧 host 不自动信任新 Controller；
- Controller 本机保留有限 ActivityStore，让控制者能查看最近在哪台设备读了什么路径/跑了什么命令；不上传、不默认保存文件内容或 stdout 全文；
- suspend/合盖遵循 OS，不强行阻止睡眠；resume 后自动重建 outer transport/InnerSession，Connector 自动重新广播。

推荐状态语义：

- 绿色：已连接 / 已就绪；
- 琥珀：连接中 / 重连中；
- 灰色：用户主动暂时断开；
- 红色只用于持续失败且确实需要人处理的情况，不把普通中继路径标红。

Linux v1 不假装有 tray：基线是 `clew host --foreground`，终端持续显示一行人话状态，`Ctrl-C` 明确“退出并断开”。在可靠 tray 支持落地前，不把进程隐藏成用户找不到的后台任务。

## 5. 后置需求

### 5.1 Directory

v1 的 host 已经主动连接预绑定 controller，所以 controller 天然知道哪些设备在线，**不需要先做独立 directory**。

只有出现多 controller、跨 controller 发现、federation 或 roaming 需求时再实现。Directory 始终只是发现面，不经手业务数据。

### 5.2 Site 与连接助手

多机/内网站点不再建模成“朋友手工部署一个 gw”。内部引入 `SiteId`；设备能力拆成 `EXECUTE` 与 `CONNECTOR`。能访问 controller 的 Site member 在 policy 允许时可以自动兼任连接助手，无法直连的目标机通过 LAN 自动发现任意健康 Connector。

Site 数据模型从 v1 基础阶段就存在；单机只是只有一个 member 的 Site。连接助手的数据面可以在单机 vertical slice 后落地，但不能等到所有 Device/API/GUI 都固化后才补。

目标仍然保持 host↔controller 的 E2E secure-session 不变量，使连接助手只转发不可读 payload。详见 [04-site-connector-ux.md](04-site-connector-ux.md)。

### 5.3 其它 transport

Tailscale/DERP、其它 relay/backend 只有在真实 vertical slice 需要时再引入；不提前为了 trait 完整度建立空 crate 或半兼容实现。

### 5.4 高级长期在线 / Service Runtime

v1 仍以 portable session、Windows/macOS tray 和 Linux foreground 为默认，不要求管理员安装。后续高级功能明确计划接入：

- Linux `systemd --user`：用户级长期在线，优先实现，不需要 root；默认登录后常驻，boot-before-login 的 linger 另做显式高级选项；
- Windows Service：机器级长期在线，runtime 与用户 tray/GUI 分离，通过受保护 Local IPC 控制；
- Linux system `systemd` service：机器级长期在线，使用独立低权限 service identity/state。

这些模式必须由用户显式启用，不能因为 Connector 自动 promotion 就偷偷安装。machine service 默认优先服务 `CONNECTOR-only`；要开放 Read/Shell/File 时必须另外选择 execution identity 和 policy，不能继承 LocalSystem/root 的整机权限。portable/user identity 与 machine-service identity 不静默共用 DeviceKey，迁移需要 Controller 明确授权或重新 enrollment。

## 6. 痛点清单（实现时逐项对照）

1. 合作者不会配网络 → Site Kit + 自动 bootstrap + host 主动出站；每台设备最终都有独立 DeviceKey。
2. agent 不应该管理连接生命周期 → persistent controller + Local API。
3. 两边都在 NAT 后 → iroh Direct/Relay，路径迁移由 iroh 负责。
4. 中继慢 → relay 保底后仍由 iroh 尝试 direct；UI 可观察路径。
5. connection 真掉线 → Clew 做 reconnect；RPC retry、Shell reattach、File resume 分别定义恢复语义。
6. 设备不能靠 IP/URL 当身份 → 持久 DeviceKey；worm 只负责 enrollment。
7. 合作者不想每次确认 → signed capability policy 前置授权，host 可继续收窄。
8. CLI/MCP 调用结束后 forward 仍应存在 → listener/task 生命周期归 controller。
9. 临时访问对方 8080 / 内网 → 动态 forward、SOCKS5、HTTP CONNECT。
10. 搬安装包、日志、工程目录 → 专用 transfer plane。
11. agent 输出容易失控 → 所有 Glob/Grep/Read/Shell 都有 bounded output、分页/超时/取消。
12. host/controller 更新不同步 → wire major + capability negotiation，从 v1 开始保证协议演进。
13. Windows/macOS 分发规则不同 → 一个 artifact 的 UX，不强求一个裸 binary 的实现。
14. `strings` 扫到绑定材料 → 可做混淆/加密，但信任根是签名 enrollment + DeviceKey。
15. 分发物过于“通用工具感” → Distribution Studio + reusable Outfit preset，让朋友看到熟悉的名字/图标/文案，但不增加任何连接步骤。
16. 同一 Site Kit 打开多台机器会重名 → hostname 默认设备名 + 固定 5 字符 DeviceTag（如 `GPU-01-K7M4Q`）+ Controller rename；不使用 `(2)` 序号，邀请名不等于设备名。
17. `site.clew` 被聊天软件/用户拆丢 → 固定查找顺序 + drag/drop/file-picker + 本机 membership 恢复 + 人话提示。
18. 包被转发第三人后需要收口 → device/invite/site revoke 分层，关闭 bootstrap 不等于撤销已加入设备。
19. ControllerKey 丢失 → 显式加密备份；无备份就重新邀请，不做无声 key migration。
20. agent 不知道“哪台 GPU” → Site-qualified device selector；helper-only 不参与执行候选。
21. 控制者不知道 agent 刚做了什么 → 本机 bounded ActivityStore。
22. 办公 helper 合盖睡眠 → best-effort suspend 状态 + resume 自动重连/重新广播。
23. Linux 没可靠 tray → foreground 可观察进程，不藏后台。

## 7. v1 明确不做

- 完整远程桌面画面；
- 无窗口、无托盘的静默驻留；
- 对抗杀毒、规避 SmartScreen/Gatekeeper 或其它系统安全机制；
- 未授权接入或绕过 collaborator 可见退出控制；
- 企业 IdP / 审计合规套件；
- Directory federation；
- 让朋友手工配置 gateway/IP/route/CIDR；
- Tailscale/DERP 双栈；
- SOCKS5 UDP ASSOCIATE；
- 宣称 connection 彻底断开后任意 TCP stream 都能无损迁移。

## 8. v1 产品验收口径

第一条产品闭环必须是真实 vertical slice，而不是模块各自“完成”：

```text
生成 collaborator artifact
        ↓
合作者直接启动
        ↓
bootstrap enrollment
        ↓
持久 DeviceKey
        ↓
iroh 连接 controller
        ↓
controller 显示设备在线/路径
        ↓
agent 通过本机 MCP Read / Shell 成功
```

随后才验收：

- relay → direct 的路径变化对工具调用透明；
- 整个 connection 重建后 Read 可 retry、Shell 可 reattach、File 可 resume；
- CLI 添加 SOCKS5/forward 后 CLI 退出，listener 仍由 controller 持有；
- 传输一个大文件并验证中断续传；
- collaborator 随时可通过 GUI/托盘退出并立即失去远程能力。
- collaborator 在看到“已连接”后关闭/最小化主窗口，进程继续在托盘运行；从托盘重新打开能恢复同一状态，只有显式“退出并断开”才终止远程能力。
- 对于“目标电脑无公网 + 一台联网帮手机”，朋友只需在两台电脑各双击一次且顺序无关；目标机自动发现连接助手并完成首次 enrollment。
- 控制者能从预设创建一套 Outfit，修改图标/标题/Logo/颜色/中文文案并实时预览；邀请 Alice/Bob 时直接复用该 Outfit，朋友端操作数仍不增加。
- 四台机器使用同一 Site Kit 时 Controller 列表显示各自 hostname-derived 名称，不出现四个 Alice；helper-only 不可被 MCP 执行工具选中。
- 删除/移动 `site.clew` 后重新启动会进入固定恢复页；同一机器已有 membership 时可以无需重新 enrollment 恢复。
- 同一台机器连续双击两次只得到一个 DeviceId/runtime。
- 控制者可以停止单台、关闭继续加入、作废一份分发包或停止整个 Site；被 revoke 的设备即使以后重连也不能恢复权限。
- ControllerKey 有备份/恢复入口；无备份换电脑时明确要求重新邀请。
- 经 Connector 的测试流量在 helper 侧看不到 Read/Shell/File 明文。
- Controller 能查看/清空本机活动记录；host 睡眠/唤醒后自动恢复；Linux 无 tray 时以前台模式可观察运行。
