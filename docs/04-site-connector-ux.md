# Site / 连接助手：零配置多机部署

本文补足 Clew 在 **“目标电脑不能直接联网，需要另一台电脑帮忙连接”** 时的体验与架构。

目标不是让合作者学会部署 Node + GW，而是让他只做：

```text
把同一份协作包放到需要参与的电脑上
        ↓
双击 Clew
        ↓
结束
```

所有 gateway、route、IP、端口、CIDR、节点 ID、配对码都属于实现细节，不进入合作者主流程。
Site Kit 的目标机与 helper-only 入口必须使用同一个 `OutfitProfile`：同样的 app name/icon/logo/color，只切换角色相关字符串。不要因为多机模式突然退回 Clew 默认皮肤或生成一套看起来不同的“GW 程序”。

更进一步：**所有普通邀请默认就是 Site Kit**。控制者不需要提前知道对方是一台电脑还是“目标机 + 联网帮手机”。单机只运行一次；如果目标机连不上，把同一份包拿到附近能联网的电脑再运行一个“帮助连接”入口即可。

## 1. 体验预算

普通单机：

```text
朋友：下载/收到 1 个 artifact -> 双击 1 次
```

一台目标机 + 一台联网帮手机：

```text
朋友：在两台电脑各双击 1 次
确认“已连接/已就绪”后，两台都可以把主窗口收进托盘；这不计作额外部署步骤。
```

多目标机：

```text
朋友：每增加一台需要参与的电脑，只多一次双击
```

正常路径不允许新增：

- 手填 Gateway IP；
- 手填端口；
- 手填 Controller 地址；
- 输入 pairing code；
- 开 IP forwarding；
- 配 route/CIDR；
- 改防火墙；
- 管理员终端命令。

如果 LAN 自动发现失败，可以有 fallback，但 fallback 也优先是 **复制一个小文件 / 点一个按钮**，不是让朋友学网络。

## 2. 借鉴的成熟产品模式

### 2.1 Twingate：Remote Network 比 Connector 更稳定

Twingate 把一个物理网络建模成 Remote Network，Connector 只是其中可替换的连接入口；同一个 Remote Network 可以有多个 Connector，客户端不需要知道本次实际走了哪一个。

Clew 对应：

```text
Site = Alice 实验室 / Bob 家里 / 某个课题组局域网

Site
├── 目标电脑
├── 目标电脑
└── 一个或多个连接助手
```

因此不要把某个 `gateway_device_id` 永久写进目标节点。

### 2.2 Tailscale reusable auth key：部署一组机器不必每台重新发券

One-time invite 适合一台电脑；Site 部署往往一次要加入 2–8 台机器。如果每加入一台都要求控制者重新生成邀请、朋友重新收文件，安全模型反过来破坏体验。

Clew 引入 **SiteBootstrapPass**：

- 同一个 Site Kit 可在有限部署窗口内登记多台设备；
- 每台设备登记后仍生成自己的 DeviceKey；
- pass 只负责 bootstrap，不成为长期身份；
- controller 可以自动在达到预计设备数、超时或用户点“完成部署”后关闭它。

朋友完全看不到 pass/token。

### 2.3 RustDesk Custom Client：复杂配置应该在发出去前完成

Controller 生成的是可直接转发给朋友的 **Site Kit**，不是一组配置命令。

### 2.4 remote.it claim / device package：首次登记应该自动落成持久设备

第一次启动是 claim/enrollment；成功后本机持久化身份，以后继续启动不重复登记。

## 3. 核心模型：Gateway 不是角色，而是 capability

不要把程序永久分成：

```text
NODE binary
GW binary
```

设备统一建模：

```text
SiteMember {
    DeviceId
    SiteId
    display_name          # 默认来自 hostname，可由 Controller 改名
    hostname_observed
    enrolled_via_invite_id
    capabilities {
        EXECUTE?          # agent 能否 Read/Shell/File 使用这台电脑
        CONNECTOR?        # 是否可帮助同 Site 的其它设备连接 controller
    }
}
```

因此一台机器可以：

```text
目标机：        EXECUTE
连接帮手机：    CONNECTOR
既是目标又帮忙：EXECUTE + CONNECTOR
```

对朋友不说 `EXECUTE` / `CONNECTOR`。

人话：

- **使用这台电脑**；
- **帮助附近电脑连接**。

更重要的是：如果某个普通目标节点本身能访问 controller，并且 Site 允许自动帮助，它可以在后台自动兼任 CONNECTOR，不需要朋友重新配置。
`Alice 实验室` 这类邀请名只用于 Site card。每台新 member 首次加入时用本机 hostname 生成设备显示名。Site 内出现同 hostname 时，不使用 `GPU-01 (2)` 这样的序号；碰撞组内每台设备都显示固定 5 字符 DeviceTag，例如 `GPU-01-K7M4Q`、`GPU-01-P2D8N`。DeviceTag 由 DeviceId 稳定派生、使用人眼友好的 Crockford Base32，只负责区分，不携带认证意义；一旦启用即持久化，避免名字随设备上下线变化。控制者可随时改名。helper-only 的 `EXECUTE=false`，因此即使它也有 hostname，也永远不是 MCP Read/Shell/File 的执行候选。

## 4. Site Kit：一个包覆盖整个地方

Controller GUI 的默认邀请产物是**按目标平台分别生成**的 Site Kit，例如：

```text
Alice-实验室-Clew-Windows.zip
Alice-实验室-Clew-macOS.zip
Alice-实验室-Clew-Linux.tar.gz
```

Windows 示例：

```text
Alice-实验室-Clew/
├── Clew.exe
├── site.clew
└── 开始这里.html
```

macOS/Linux 使用各自平台 runtime，但 `site.clew` 语义相同。`Clew` 是可复用的签名 ClientFlavor；个性化 Site/bootstrap 信息主要放在 controller 签名的 `site.clew`。不要为了“一份包支持所有 OS”默认塞入三套 runtime。

因此不存在“先发 Node 包，发现不通，再找控制者生成一个 GW 包”的第二轮。朋友始终复用最初收到的 Site Kit。

### 4.1 为什么尽量不为每个设备重签 binary

这样可以：

- 同一份包复制到多台机器；
- 不需要为每个 Site member 重新 codesign/notarize；
- 更换连接帮手机时不需要重新构建整个应用；
- 以后升级 Clew 时可以保留 Site identity / DeviceKey，不重新走邀请。

### 4.2 常见“1 个目标 + 1 个 helper”可以更直观

为了让朋友不用做角色判断，Generator 可以额外生成两个带 role hint 的文件夹：

```text
① 在目标电脑上打开/
   Clew
   site.clew
   role-hint.clew   # prefer EXECUTE

② 在能上网的电脑上打开/
   Clew
   site.clew
   role-hint.clew   # prefer CONNECTOR-only
```

这不是两套程序，binary 字节相同；role hint 只是第一次启动的默认行为。

朋友只需要看文件夹名字。

如果放错也不应该失败：设备仍能登记到 Site，Controller 可远程调整 capability。

## 5. SiteBootstrapPass：为体验服务的有限多次 bootstrap

建议：

```text
SiteBootstrapPass {
    site_id
    controller_id
    bootstrap_secret
    allowed_profiles
    max_claims?
    expires_at
    signature
}
```

默认由 Controller 自动选择合理限制，例如：

```text
部署窗口：从第一次 claim 起 48 小时
预计设备：自动 / 4 台
```

部署窗口尽量从 **第一次真正使用** 开始计时，而不是从生成 zip 的那一刻开始，避免文件在聊天里放两天就“过期”。Controller 侧可以延长/重新打开同一个 bootstrap id，不要求朋友重新下载整个 Site Kit。

这些是后台细节，不要求朋友理解。

每台新机器：

```text
SiteBootstrapPass
      +
fresh DevicePublicKey
      ↓
Controller
      ↓
独立 DeviceId / DeviceKey identity
```

Controller 首页提供：

```text
Alice 实验室 · 正在部署
3/4 台已加入
[ 完成部署 ]
```

达到预计数量可以自动关闭 pass；用户也可提前完成。

如果未来还要加电脑，Controller 点“添加电脑”生成一个新的短期 pass/小 join 文件即可，不要求朋友重装现有设备。
### 5.1 `site.clew` 被拆丢 / 同机器重开

`site.clew` 很可能在聊天、网盘、手工拖文件时和程序分开。host 固定按：**显式选择/拖入 → 程序同目录 → 本机已有 membership → 缺失页** 查找，不扫描全盘。

如果这台机器、当前 OS user、这个 Site 已有 DeviceKey，即使 sidecar 后来丢了，也可直接恢复该 membership；如果本地同时有多个可能的 membership，弹人话选择器。首次加入而 sidecar 缺失时显示：

```text
还缺一个邀请文件。
请把 site.clew 和这个程序放在同一个文件夹，
或把 site.clew 拖到这里。
[ 选择邀请文件 ]
```

检测到从压缩包临时目录运行时先提示“请先全部解压”。同一机器第二次启动若 runtime 已存在，只显示现有窗口，不再次 claim。DeviceKey 永远写入 OS user state store，不写到 Site Kit 目录，所以复制 Site Kit 不会复制已有设备身份。

## 6. 自动拓扑：先直连，找不到再找附近连接助手

每个 Site member 启动后并行做：

```text
A. 尝试直接连接 Controller
B. 在 LAN 发现同 Site 的其它 Clew member
```

### 6.1 能直连 Controller

正常使用 iroh：

```text
member -> Relay/Direct -> Controller
```

如果设备允许 CONNECTOR，它同时广播：

```text
“我属于 Site X，并且当前有上行连接”
```

### 6.2 不能直连 Controller

目标节点不报 raw network error，而是：

```text
正在连接
  ↓
未找到外网路径
  ↓
自动寻找附近连接助手
  ↓
找到 Site X 的可用 helper
  ↓
已连接
```

朋友最终只看到“已连接”。

### 6.3 LAN discovery

优先使用：

- mDNS / local endpoint discovery；
- 同 SiteId 过滤；
- Controller 签名的 connector lease / site membership 证明。

不要求朋友输入局域网 IP。

### 6.4 多个 Connector

同一 Site 可以同时有：

```text
Helper-A
Helper-B
Helper-C
```

目标节点自动选择可达、健康、延迟较好的一个。

一个 helper 下线后：

```text
Node -> 自动尝试其它 helper -> 恢复
```

不要把 helper 选择暴露成用户配置。

## 7. 顺序无关

必须支持：

### 7.1 先开 Helper

Helper 窗口：

```text
已就绪
正在等待附近电脑
```

### 7.2 先开目标电脑

目标电脑：

```text
正在连接
正在寻找附近的连接助手
```

helper 后启动时自动接上。

### 7.3 两台同时开

自动发现并组成 Site，不要求重新启动任一边。

**谁先开都一样** 是必须验收的 UX，不是优化项。

## 8. 首次 enrollment 必须能经过 Connector

最难但最重要的场景：目标电脑从来没有公网能力，它第一次启动就必须能完成 enrollment。

流程：

```text
Target
  │
  │ sealed-to-Controller bootstrap envelope
  │ (SiteBootstrapPass proof + fresh DevicePublicKey)
  ▼
Connector
  │
  │ opaque bytes only
  ▼
Controller
```

Connector 不读取 enrollment envelope 中的 bootstrap secret，也不替 Controller 验证/消费 credential。envelope 必须只可由 pinned Controller 解开。

响应同样端到端封装沿原路返回，Target 落下 DeviceKey。

因此朋友不需要先把目标电脑临时搬到能上网的网络登记一次。

## 9. Connector 是 userspace outer tunnel：不做子网路由，也不做业务 MITM

Clew 不要求朋友：

- 开系统 IP forwarding；
- advertise route；
- 配 NAT；
- 改默认网关；
- 填内网 CIDR。

同时 v1.5 选择固定安全模型 A：

```text
Target ========================================= Controller
       Clew InnerSession（业务端到端加密）
          \                                   /
           outer link -> Connector -> outer link
                        只搬密文
```

Connector 可以处理 userspace tunnel 的连接、背压、重连和最小路由 header，但 **不能解密/解析 InnerSession**。Read 路径、Shell command、文件数据、stdout/stderr、`StreamOpen.kind` 都必须位于 inner ciphertext 中。

因此“这台电脑只帮助附近电脑连接”是事实，不是 UI 美化。若实现需要 helper 终止 Target 和 Controller 的两条业务会话才能工作，则 V1.5 不发布该 data plane。

## 10. Connector-only 电脑默认不暴露本机能力

如果朋友拿自己的办公电脑只帮实验仪器联网，他最关心的是：

> “你是不是顺便也能读我这台电脑？”

因此 `CONNECTOR-only` profile 默认：

```text
EXECUTE = false
filesystem = none
shell = false
file = false
proxy-egress-for-site = true/按需
CONNECTOR = true
```

朋友窗口直接写：

```text
这台电脑只帮助附近电脑连接。
不会开放这台电脑的文件和命令。
连接内容也由目标电脑和控制者端到端保护；这台连接助手不读取远程文件或命令内容。
```

如果同一台机器也需要被 agent 使用，Controller 生成 `EXECUTE + CONNECTOR` profile；朋友不需要再开第二个进程。
`CONNECTOR-only` 的设备在 Controller/MCP 中标记 `executable=false`；执行工具即使显式选中它也返回 `device_not_executable`。它可以出现在拓扑/状态列表，但不能因为“在线且唯一”被 MCP 自动选成目标。
Outfit 与这个 capability 组合正交：`EXECUTE`、`CONNECTOR-only`、`EXECUTE+CONNECTOR` 都复用同一个 Site Outfit，helper-only 只切换 helper string set。

## 11. Friend-side UI：不要出现 Site/Gateway 技术词

### 11.1 普通目标电脑

```text
Clew
已连接到「CJ」

当前空闲

[ 退出并断开 ]
```

不需要告诉他是 direct、relay 还是经过 helper。
如果 Site Kit 使用自定义 Outfit，这里所有 `Clew` 标题、Logo、颜色和按钮文案都由 OutfitRuntimeView 渲染；连接/退出等产品语义保持不变。

### 11.2 Helper-only

```text
Clew
连接已就绪

正在帮助 2 台附近电脑连接

[ 退出并断开 ]
```

### 11.3 Helper 掉线时的目标电脑

```text
正在重连
附近连接暂时中断，Clew 会自动恢复
```

### 11.4 Friend tray：目标机和 Helper 都不占桌面

无论是普通目标机还是 helper-only，主窗口确认成功后都可以收进托盘继续运行。

普通目标机 hover / menu summary：

```text
Clew · 已连接
连接到 CJ · 当前空闲
```

helper-only：

```text
Clew · 连接已就绪
正在帮助 2 台附近电脑连接
```

右键菜单保持同一结构，只把动作名字按角色翻译：

```text
显示 Clew
暂时断开 / 重新连接
# helper-only: 暂停帮助连接 / 恢复帮助连接
复制状态
---
退出并断开
```

不要把 `SiteId`、Connector lease、LAN path 放进朋友的托盘菜单或 tooltip。

不要写：

```text
Gateway offline
route lost
mDNS timeout
```

## 12. Controller GUI：设备之上增加 Site card

首页从纯 Device list 升级为：

```text
Alice 实验室                      已连接
  CryoEM-PC                        已连接
  GPU-01                           已连接
  Lab-PC                           连接助手 · 已就绪

Bob                                离线
  Bob-Laptop                       离线
```

默认隐藏 topology 细节。

展开“详情”才显示：

```text
CryoEM-PC
  路径：经 Lab-PC
  Target↔Helper：局域网
  Helper↔Controller：中继
```

邀请对话框**不增加“要不要 Gateway”问题**。所有邀请都生成 site-capable artifact。
实现约束已经冻结：Controller GUI 的正常邀请始终签 `ExecutePreferred` site-capable `site.clew`；friend-side helper-only 不要求控制者重签第二份邀请，而是同一 runtime 用 `host --connector-only` 启动，同一 signed pass 在 Controller 上再经过 `BootstrapMemberMode::ConnectorOnly` ceiling。该入口只能减少权限，不能把 signed/helper membership 升成 EXECUTE。V6 packaging 已按此 argv contract 生成正式分发包里的“① 使用这台电脑 / ② 只帮助附近电脑连接”双 launcher；两者复用同一已验证 ClientFlavor runtime 和同一 signed `site.clew`。

Controller 可以在高级选项里设置“预计几台电脑/包含哪些平台”，只影响包内容和部署进度展示，不改变朋友的连接步骤。
设备行菜单提供 **改名 / 停止这台**。Site/邀请菜单区分 **停止继续加入**（只关 bootstrap）、**作废这份分发包**（关 bootstrap + revoke 由它加入的设备）、**停止整个 Site**。这三个动作不能合并成一个含糊“移除”。

## 13. 自动 Connector promotion

如果 Site 中一个 `EXECUTE` 节点本身能访问 Controller，而且附近存在无法直连的同 Site 节点，且 policy 允许：

```text
EXECUTE node
   + auto_connector=true
        ↓
自动兼任 CONNECTOR
```

无需朋友重新启动、重新下载或切换模式。

因此很多场景根本不需要单独部署 helper：

```text
实验室有 4 台机器
其中 1 台能出公网
        ↓
这台自动帮助另外 3 台
```

只有当唯一能联网的电脑 **不希望被 agent 使用** 时，才需要 helper-only profile。

## 14. 更换 Helper 不应该影响目标节点

不要把目标 Node 绑定到某个 GatewayId。

绑定关系是：

```text
Target -> SiteId -> any healthy Connector in Site
```

旧 helper 坏了：

1. 在新的联网电脑上打开同 Site Kit，或 Controller 点“添加连接助手”；
2. 新设备自动加入 Site；
3. 目标节点发现它；
4. 自动恢复。

已有目标电脑不重发包、不改配置。
如果旧 helper 被 Controller revoke，它即使仍在朋友电脑上运行，也不能再建立有效的 Controller outer link/lease；Target 自动选择其它健康 Connector。

## 15. LAN discovery 失败时的 fallback 仍然不让人填 IP

企业网可能阻断 mDNS/组播。主流程不能因此退化到：

```text
请输入 Gateway IP
```

建议 fallback：

Helper 窗口（v1 UI 使用英文，避免缺字字体问题）：

```text
[ Save Nearby Connection File... ]
```

得到 canonical 文件：

```text
nearby-connection.clew
```

读取层兼容早期设计名 `附近连接.clew`。朋友把这个小文件复制到目标电脑 Clew/Site Kit 同目录并重新打开，或直接拖进 Host 窗口即可。

这个文件携带：

- helper 的 direct LAN address hints；
- same-Site equality tag；
- Controller-signed、绑定 Helper EndpointId 的短期 connector lease，作为 route-binding proof；

短期 lease 到期后，已导入文件仍只可作为“去哪里尝试连接”的 signed routing hint；**不能**直接授权 tunnel。Target 每次真正使用 candidate 时都必须从 Helper 现场取得新的 `ConnectorReady`，并再次验证当前有效的 Controller-signed lease / Site / EndpointId。被 revoke 或已经失去 Controller uplink 的旧 helper 因拿不到 fresh lease，只会成为一次失败 candidate。

这样即使 mDNS 被禁，也只增加一次“复制文件”，不增加网络知识，也不引入新的长期授权凭据。

## 16. Session mode、托盘与长期在线

Windows/macOS 默认仍是 portable session mode，但“窗口开着”不再是 availability 边界：

```text
Clew runtime / tray icon 在 = 可用
主窗口显示或隐藏          = 只影响桌面占用
退出并断开                = 真正停止
```

朋友第一次双击时默认显示主窗口，让他自然看到连接状态；主窗口在状态可确认后继续保持可见，直到朋友主动最小化或点 `X`。这两个动作都只隐藏到托盘，Node/Connector 继续运行，不让连接助手长期占据一台办公电脑的主屏幕。

v1 **不要**为了 Connector 自动变成系统服务或要求管理员权限。长期在线服务化是后续显式高级功能，绝不能和 auto Connector promotion 绑定。

如果朋友确实需要长期可用，再提供一个可选的一键动作：

```text
[ 登录后自动保持可用 ]
```

尽量使用 per-user autostart；一旦启用，后续登录应直接 tray-only/menu-bar-only 启动，不重复弹主窗口。只有平台确实要求时才解释权限。

这不是初次部署必选步骤。
Linux v1 不套用上述 tray 语义：朋友在终端以前台模式运行，状态可见，`Ctrl-C` 明确退出。没有可靠 tray 前不提供隐藏到后台的等价动作。
后续高级模式可用于实验室长期在线 helper：Linux 优先 `systemd --user`，再提供 machine-level `systemd` service；Windows 提供 Windows Service。machine-level helper 默认 `CONNECTOR-only`，使用专用低权限 identity；只有控制者和本机用户明确开启 EXECUTE policy 后才允许 Read/Shell/File。服务安装/停止/卸载都有显式 UI/CLI，Windows Service 与 tray GUI 分进程，不把 GUI 放进 service session。

睡眠/合盖不算人为退出。Clew 不阻止 OS 睡眠；helper resume 后自动刷新网络地址、重连 Controller 并重新广播连接助手能力。目标机在 helper 睡眠时优先 failover 到其它 helper；没有其它路径才显示“正在重连”。

## 17. 朋友侧“动作数”作为正式验收指标

### 单机

```text
1. 收到包
2. 双击
```

### 目标 + Helper

```text
1. 收到同一个 Site Kit
2. 联网电脑双击一次
3. 目标电脑双击一次
```

不要求顺序。

### N 台目标

每台额外目标只增加：

```text
复制包 + 双击
```

正常路径如果出现“抄一串字符、输入 IP、选网卡、配置 route、开管理员终端”，即视为 UX 回归。

## 18. 开发顺序影响

Site 不应等到最后才加，因为如果 DeviceId、invite、GUI 都先按单机固化，后面加入多机部署会重构身份和 UI。

建议：

### V0 基础阶段就加入

- `SiteId`；
- `SiteMember`；
- capability `EXECUTE/CONNECTOR`；
- `SiteBootstrapPass` 数据模型；
- Local API 的 `site.list/site.get`。

### V1 仍先跑通单机

单机只是一个只有一个 member 的 Site。

### V1.5 — Zero-config Site Connector

在 Agent minimum 之前或并行加入：

- Site Kit 多设备 claim；
- LAN discovery；
- sealed enrollment through connector；
- opaque outer tunnel forwarding（InnerSession 在 V1 已存在）；
- auto connector promotion；
- 多 connector failover；
- Friend UI 两种人话状态。
- helper suspend/resume 后自动重新广播；
- helper plaintext gate：日志/协议 payload 不得出现 Read/Shell/File 测试明文。

这样后续 Shell/File/Forward 都天然跑在统一的 Site topology 上，而不是后来再把 gw 塞进去。
### V7 — 长期在线连接助手

等 portable/Connector 主线稳定后，再把实验室常驻 helper 接入 Advanced Service Runtime：Linux 优先 `systemd --user`，机器级再用 Windows Service / Linux system service。服务化只改变 lifecycle owner，不改变 Site/DeviceKey/InnerSession/Connector 数据面；machine service 默认 Connector-only，不能借“常驻”扩大 EXECUTE 权限。

## 19. 最终目标路径

控制者：

```text
邀请合作者
  → Alice 实验室
  → 有电脑不能直接联网
  → 按平台生成 Alice-实验室-Clew-Windows.zip / Alice-实验室-Clew-macOS.zip
```

朋友：

```text
能联网的电脑：双击
目标电脑：    双击
```

Controller：

```text
Alice 实验室 · 2 台已连接
```

Agent：

```text
Read/Shell/File
```

如果朋友需要理解 `gateway`、`route`、`SiteId`、`connector lease` 才能完成部署，说明这层 UX 还没有做完。
