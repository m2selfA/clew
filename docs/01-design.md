# 设计方案

对应需求见 [00-pain-points.md](00-pain-points.md)。本文是 Clew Architecture v1.5 的实现总图。

本版吸收的成熟模式不是照抄某一个产品：

- **Tailscale**：长期 daemon/controller 拥有网络与状态，CLI/GUI 走本地 API；路径层对上层隐藏；能力版本独立于软件版本；
- **iroh**：v1 的 Direct/Relay/QUIC path 管理；
- **Syncthing**：设备密钥身份、relay 与端到端业务安全分层、可演进协议思维；
- **RustDesk**：把复杂连接配置前置到可直接交付的 collaborator artifact；
- **MCP 2026**：MCP 是 agent adapter，不拿 transport session 当长期业务状态。

## 1. 名字与角色

**Clew**：线团，阿里阿德涅穿迷宫的线。

Worm 不再表示“永久藏在二进制里的控制身份”，而是预先绕进线团的 **enrollment capability**：它让一台新 host 有权第一次找到并加入指定 controller。

v1 只有两个长期运行角色：

- **controller**：用户侧唯一状态 owner；
- **host**：合作者侧 capability runtime。

另有短生命周期 adapter/client：

- `clew` CLI；
- `clew mcp --stdio`；
- controller 内置的 HTTP MCP adapter。

Directory 和其它 transport 后置；**Site/连接助手模型不后置**。内部不再把 `gw` 当永久独立角色，而把连接能力建模成 Site member 的 `CONNECTOR` capability。完整多机体验见 [04-site-connector-ux.md](04-site-connector-ux.md)。

## 2. 核心原则

1. **一个 collaborator distribution package**：对朋友永远是一份按目标系统生成的 Site Kit；包内可以是签名 `.exe` / `.app` + `site.clew`。不要再把“一个裸 exe 里焊死邀请”当产品约束。
2. **controller 唯一拥有状态**：Endpoint、设备、session、task、forward、proxy、transfer 都归 controller；CLI/MCP 不自己维持平行连接。
3. **worm 只负责 enrollment**：长期身份来自 host 自己生成并持久化的 DeviceKey。
4. **transport 管路径，Clew 管业务恢复**：iroh 处理 Relay↔Direct；Clew 只处理整个 connection 丢失后的 reconnect/replay/resume。
5. **协议从 v1 就可演进**：wire major、capability version、software version 分开。
6. **不逐操作弹框，但强制 capability 边界**：预配置 policy 可很宽松，host 本地策略只能继续收窄。
7. **MCP 是 adapter，不是 session layer**。
8. **人用 GUI，agent 用 MCP**。GUI 负责点击、表单、自动生成和状态展示，不承担协议逻辑；详见 [03-gui.md](03-gui.md)。
9. **按 vertical slice 推进**，不为了未来想象先拆十几个 crate/trait。人能点的界面必须和该 slice 的协议能力同一天出现。
10. **Site 是基础对象**：单机是一个 member 的 Site；多机时任意有上行的 member 可按 policy 自动兼任连接助手，朋友不配置 gateway。
11. **业务端到端加密不因 Connector 降级**：Target↔Controller 的 InnerSession 是唯一业务安全边界；Connector 只搬不可读密文，不能终止 Shell/File/Tool 的安全会话。

## 3. 运行时拓扑与状态所有权

```text
                              controller machine

        Agent
          │
   ┌──────┼───────────────┐
   │      │               │
MCP stdio MCP HTTP        CLI
   │      │               │
   └──────┴──────┬────────┘
                 │
        Local Controller API
      Named Pipe / Unix socket
                 │
                 ▼
        ┌───────────────────┐
        │ clew controller   │
        │ DeviceRegistry    │
        │ SessionRegistry   │
        │ ActivityStore     │
        │ Task/Forward/...  │
        │ iroh Endpoint     │
        └─────────┬─────────┘
                  ║
                  ║  Clew InnerSession
                  ║  Target↔Controller E2E ciphertext
                  ║
          ┌───────╨──────────────────────────┐
          │                                  │
     direct outer link                 Connector path
          │                                  │
          │                         ┌──────────────────┐
          │                         │ SiteMember       │
          │                         │ CONNECTOR        │
          │                         │ opaque forwarding│
          │                         └────────┬─────────┘
          │                                  │
          └──────────────────┬───────────────┘
                             ▼
                    ┌───────────────────┐
                    │ collaborator host │
                    │ DeviceKey         │
                    │ policy/tools/tasks│
                    └───────────────────┘
```

Connector 可以终止自己的 **outer transport link**，但绝不能终止 `InnerSession`。因此直连和经 Connector 只改变 transport topology，不改变业务加密/认证模型。

### 3.1 Controller 的唯一 ownership

Controller 唯一拥有：

- controller identity / iroh Endpoint；
- enrolled devices；
- 每台设备的 active/reconnecting session；
- Shell 等远程 task metadata；
- 本地 TCP/SOCKS/HTTP listener；
- file transfer state；
- retry / reconnect / idempotency bookkeeping。

本地 IPC 需要单实例语义。第二个 `clew controller` 不允许静默创建平行状态；它应检测已有实例并退出或转成 client。

这解决一个关键生命周期问题：

```text
clew forward add ...
        ↓
CLI 请求结束并退出
        ↓
listener 仍由 controller 持有
```

同理，MCP stdio 子进程退出不能杀掉远端 Shell task 或 file transfer。

### 3.2 Local Controller API

默认：

- Windows：Named Pipe；
- macOS/Linux：Unix Domain Socket；
- 不默认监听 LAN TCP。

Local API 至少包含：

```text
controller.status
controller.backup_export
controller.backup_restore

device.list
device.get
device.rename
device.revoke

invite.close
invite.revoke
site.revoke

session.path_info

activity.list
activity.clear

task.list
task.get
task.cancel

forward.add
forward.remove
forward.list

proxy.add
proxy.remove
proxy.list

transfer.put
transfer.get
transfer.list
transfer.cancel
```

Site API 从基础阶段就存在：

```text
site.list
site.get
site.finish_bootstrap
site.add_member_pass
```

Device 记录必须带 `site_id` 与 negotiated member capabilities。

MCP 和 CLI 共享这一层，不各自复制业务逻辑。

## 4. 初始 crate / module 划分

不要从 3 行程序直接拆十多个 crate。初始 workspace 只建立已经有明确稳定边界的部分：

```text
clew                 # 最终入口、CLI、platform packaging glue
clew-core            # identity、policy、config、error、stable ids
clew-proto           # protobuf schema、framing、wire negotiation
clew-runtime         # controller + host + iroh transport + tasks
clew-mcp             # MCP stdio/HTTP adapter -> Local Controller API
```

Host UI 先作为 `clew-runtime`/主程序的 platform module；等 UI 构建和发布边界稳定后再独立 crate。

暂时不创建：

```text
clew-transport-ts
clew-directory
clew-gw
```

也不为了抽象完整度提前建立一个“所有 transport 都必须等价于 stream transport”的 trait。先把 iroh vertical slice 跑通，再从真实第二实现抽象接口。

## 5. Identity：Enrollment 与长期 DeviceKey 分离

### 5.1 Controller identity

Controller 首次初始化生成持久密钥：

```text
ControllerKeyPair
ControllerId = fingerprint/public identity
```

controller identity 存在用户本机 state store 中。collaborator artifact 预埋目标 `ControllerId`，host 不能只因 DNS/URL 指向某服务器就信任它。
#### 5.1.1 Controller 备份与丢失

ControllerKey 不是可从朋友端重新推导的“账号密码”。它一旦丢失，新机器生成的是新的 `ControllerId`，旧 Site Kit / DeviceKey **不会也不应该自动信任新 Controller**。

Controller GUI 在第一次生成邀请后给一个非阻塞入口 **“备份控制者身份”**；CLI 等价为 `clew controller backup export`。备份是版本化、加密的 controller-state backup，至少包含 ControllerKey、transport identity、Site/Device 公钥注册表和 revoke 状态；Outfit 导出与它严格分开。

v1 恢复只支持“把备份恢复到一个空 Controller state”，不做两个 Controller 的自动 merge。portable backup 必须加密；GUI 导出时让控制者设置备份口令并明确提示保存到密码管理器/安全位置，口令不写进备份旁边。

恢复后先进入 **Recovery Review**：远程设备默认暂停接入，Controller 列出备份中的 Site/Device/revoke 状态，由控制者确认“恢复这些设备”后才重新接受连接；历史 bootstrap pass 默认保持关闭。这样即使恢复的是较旧快照，也不会在无人注意时自动重新放行曾经撤销过的设备。

没有可用备份时，人话固定为：**“这是新的控制者身份。以前发出的连接不会自动迁移，需要重新邀请。”** 朋友端长时间无法验证原 Controller 时只提示“无法连接到原来的控制者；如果对方更换了电脑，请让他重新发一份邀请”，不能偷偷接受新 key。

### 5.2 Enrollment Capability（Worm）

建议结构：

```text
EnrollmentCapability {
    format_version
    enrollment_id
    one_time_secret

    controller_id
    controller_endpoint_hints[]

    site_display_name
    policy

    issued_at
    expires_at?
    nonce

    signature
}
```

核心语义：

- `signature` v1 必须校验；
- `one_time_secret` 只用于首次注册；
- 可以不强制很短 TTL，但支持 expiry；
- 成功 enrollment 后 controller 原子标记 capability 已消费；
- 重放一个 one-time capability 必须失败或进入明确恢复流程，而不是自动生成第二个同名设备。

单设备 invite 可以继续使用 one-time capability；**默认 GUI 邀请产物则按 Site Kit 设计**，使用 [04-site-connector-ux.md](04-site-connector-ux.md) 的 `SiteBootstrapPass` 在有限部署窗口内 claim 多台 Site member。无论 bootstrap 是 one-time 还是 multi-claim，每台最终设备都必须落成独立 DeviceKey，bootstrap credential 永不成为长期身份。
邀请里的名字是 **Site/合作者标签**，不是设备名。四台电脑使用同一个 `Alice 实验室` Site Kit 时不能都注册成 `Alice`；设备显示名在 enrollment 时根据该机器 hostname 单独生成。

### 5.3 Host DeviceKey

首次启动：

```text
artifact starts
    ↓
load + verify EnrollmentCapability
    ↓
generate DeviceKeyPair
    ↓
connect pinned ControllerId
    ↓
Enroll {
    enrollment_id,
    proof(one_time_secret),
    device_public_key,
    device_info
}
    ↓
controller consumes enrollment
    ↓
host persists DeviceKeyPair + DeviceId
```

之后连接不再依赖 worm secret：

```text
DeviceKey <-> Controller
```

因此：

```text
worm != device identity
worm == permission to create one device identity
```
### 5.4 本机身份复用、设备名与第二次启动

DeviceKey 不写在 exe/app 旁边。状态按当前 OS user 持久化到平台用户数据目录，例如 Windows `%LOCALAPPDATA%\\Clew`、macOS `~/Library/Application Support/Clew`、Linux XDG state/data directory；核心 lookup key 是 `(ControllerId, SiteId, OS-user scope)`。

同一台机器、同一 OS user、同一 Site 再次打开同一个或更新后的 Site Kit：

1. 先找到已有 local membership；
2. 复用 DeviceKey / DeviceId；
3. 跳过 bootstrap claim；
4. 如果已有 runtime 正在运行，第二进程只唤起现有窗口，不创建第二台设备。

换电脑或换 OS user profile 才默认成为新 member。若确实要在同一 user/site 上重建身份，必须走显式“重新加入这台电脑”，不能因为双击两次自动克隆。

设备默认显示名来自 OS hostname；为空/过于泛化时回退为平台人话名。**不要用 `(2)` / `(3)` 这类顺序号处理碰撞。** 每个 DeviceId 可派生一个固定 5 字符 `DeviceTag`：使用带 domain separation 的稳定 hash 取 25 bit，再编码为 Crockford Base32（字符集避开 `I/L/O/U` 等易混字符），例如 `K7M4Q`。它只是人类可读的 disambiguator，不是身份或安全凭据，也不从 MAC、硬盘序列号、用户名等隐私/易变信息生成。

同一 Site 中某个自动 hostname **首次发生碰撞时，碰撞组内所有自动命名设备都切换为 `hostname-DeviceTag`**，例如 `GPU-01-K7M4Q`、`GPU-01-P2D8N`，并持久化该显示名；即使以后只剩一台，也不自动去掉 tag，避免 GUI/MCP selector 抖动。极低概率的 5 字符 tag 冲突通过持久化 `tag_generation` 重新 hash 解决，显示长度仍保持 5 字符。`device.rename` 可以覆盖自动名，MCP/CLI 立即使用新名字。

### 5.5 Worm confidentiality

为了降低随手扫描暴露，可以对 artifact 内 enrollment blob 做混淆/加密：

```text
container header | salt | nonce | AEAD(ciphertext) | checksum
```

但静态解密材料最终仍在应用中，因此：

- 它只提供 at-rest obscurity；
- 不作为 authenticity 根；
- 不宣称抵抗逆向、dump 或调试器。

真正的信任根是 controller signature + DeviceKey。
### 5.6 撤销：Controller 必须能主动收回访问

“朋友点退出”不是唯一收口方式。Controller 持有 authoritative revoke registry：

- **停止这台** → `device.revoke(DeviceId)`：立即拒绝该 DeviceKey 的新 InnerSession / 新请求，并在连接仍在时通知 host 结束会话；
- **停止继续加入** → 关闭对应 `SiteBootstrapPass`，已有设备保留；
- **作废这份分发包** → `invite.revoke(InviteId)`：关闭后续 claim，并 revoke 所有 `enrolled_via_invite_id == InviteId` 的设备；
- **停止整个 Site** → `site.revoke(SiteId)`：撤销该 Site 全部 member。

每个 DeviceRecord 必须保留 `enrolled_via_invite_id` provenance，避免“作废整包”误伤后来通过另一份邀请加入的设备。revoke 是 Controller 认证时的硬拒绝条件，不依赖朋友配合卸载程序。对已经离线且仍在本机运行的远程进程，Controller 不能承诺瞬时杀死；连接可达时应 best-effort cancel，重新连接时必须先应用 revoke。

## 6. Capability Policy

建议 policy 从 v1 就按资源边界描述，而不是只有 `{ tools, forward, proxy, file }` 四个布尔值。

```text
Policy {
  filesystem: [
    {
      root,
      read,
      write
    }
  ],

  shell: {
    enabled,
    max_runtime?,
    max_output?
  },

  forward: {
    enabled,
    allowed_listen,
    allowed_destinations?
  },

  proxy: {
    socks5,
    http_connect,
    allowed_listen,
    allowed_egress?
  },

  transfer: {
    read_roots,
    write_roots,
    max_file_size?
  }
}
```

权限合并规则：

```text
BuiltInHardLimit
    ∩ EnrollmentPolicy
    ∩ HostLocalPolicy
    ∩ NegotiatedSessionCapability
    = EffectivePolicy
```

后续层只能减少权限，不能扩大 enrollment 授予的最大边界。

提供预设 profile，例如：

```text
clew mint alice --profile research-full
clew mint bob   --profile read-only
```

从而保持“点开即用”，而不取消策略模型。

## 7. Transport：iroh 负责路径，Clew 不重写路径调度器

### 7.1 v1 transport

v1 只使用 iroh：

```text
host/controller
      │
   iroh QUIC
      │
  ┌───┴────┐
Relay    Direct
```

公益 relay 可作为开发/初始试用默认，但配置结构必须允许 dedicated/self-hosted relay；不能把公益服务当生产 SLA。

### 7.2 InnerSession：业务安全边界固定在 Target↔Controller

v1 选择 **A：Connector 永远看不到业务明文**。第一条 Direct `Read` 就建立 Clew `InnerSession`；V1.5 只是把同一 InnerSession 密文改为经 Connector outer tunnel 搬运，不新增一套“helper 终止业务会话”的模式。

InnerSession 必须满足：

- Target 验证 pinned `ControllerId`，Controller 验证持久 `DeviceKey`；
- 使用成熟标准的 authenticated key exchange / secure-channel construction（首选评估 Noise IK 一类模式），由已绑定的 Controller/Device identity 认证会话；禁止自行拼接“ECDH + 自定义签名/nonce”协议；必须具备前向安全、transcript binding、AEAD 和严格序列/重放防护；
- wire major / DeviceId / ControllerId 绑定进握手上下文；
- `StreamOpen`、tool kind、文件路径、Shell 命令、文件内容、stdout/stderr 都在 inner ciphertext 内；
- Connector 最多看到 tunnel/routing id、包长、方向、时序和连接健康，不获得业务解密 key。

首次 enrollment 经过 Connector 时同样使用 **sealed-to-Controller bootstrap envelope**；Connector 不读取 `SiteBootstrapPass` secret，也不替 Controller 消费 claim。

如果 InnerSession 尚未实现，**V1.5 Connector data plane 不得上线**；不提供“先让 helper 看明文，以后再补 E2E”的兼容模式。

### 7.3 Path change 与 Connection loss 是两类事件

iroh 自己可以让同一个 logical connection 在 relay/direct path 间变化。Clew 订阅 path event：

```text
PathState {
    Direct
    Relay
    Mixed/Unknown
}
```

用于：

- Host UI；
- `PathInfo`；
- 日志和诊断；
- 性能指标。

**Clew session 不做：**

```text
old_conn streams -> manually migrate -> new_conn
```

也不自己按 2s/5s/15s 再实现一层 P2P probing。

### 7.4 真正的 Clew reconnect state

Clew 只对整个 connection loss 建状态机：

```text
Disconnected
     ↓
Connecting
     ↓
Connected
     ↓
Reconnecting ──┐
     │          │ backoff
     └──────────┘
```

重连成功是 **新的 connection**，业务恢复由上层按语义处理。

| 业务 | connection 重建后的 v1 语义 |
|---|---|
| Glob/Grep/Read | 若请求没有副作用，可自动 retry |
| Edit/Write | 依赖 request id + precondition/idempotency 决定是否可 retry |
| Shell | host task 不与一条 stream 同寿命；重连后按 `task_id` reattach |
| File | 按 transfer manifest/chunk resume |
| Forward listener | controller 自动保留 listener，新的入站连接可在 session 恢复后继续 |
| 已建立 TCP stream | connection 真死亡后关闭；v1 不承诺无损迁移 |

这比把所有 stream 宣称为“透明迁移”更真实，也更容易测试。

## 8. Clew Wire Protocol

### 8.1 三种版本必须分离

```text
software_version = "0.3.1"   # UI/diagnostic
wire_major       = 1          # incompatible framing/semantics
cap_version      = 7          # monotonic feature capability
```

连接使用固定 ALPN，例如：

```text
clew/1
```

兼容规则：

```text
wire_major 不兼容
    -> fail closed with clear error

wire_major 相同、cap_version 不同
    -> negotiate feature intersection

software_version 不同
    -> 不作为拒绝连接依据
```

### 8.2 Hello

```text
Hello {
    wire_major
    cap_version
    software_version
    role: HOST | CONTROLLER

    device_id?
    features[]

    max_frame_size
    max_concurrent_requests
}
```

feature 示例：

```text
TOOL_RPC
SHELL_TASK
FORWARD
SOCKS5
HTTP_CONNECT
FILE_RESUME
```

### 8.3 编码与 framing

建议 `proto3 + prost`，避免把 Rust 内部 enum/bincode 直接变成长期 wire contract。

控制面 envelope：

```text
RequestEnvelope {
    request_id
    trace_id?
    deadline_ms?
    oneof body { ... }
}

ResponseEnvelope {
    request_id
    oneof result {
        success
        error
    }
}
```

要求：

- request id 全局稳定；
- 明确 maximum frame；
- 对未知字段前向兼容；
- error 有稳定 code，不依赖人类字符串解析；
- deadline/cancel 是协议能力，不只靠 socket close；
- 写操作必须定义 replay/idempotency 行为。

## 9. Session 与 stream 分类

这里的 stream 分类是 **InnerSession 里的逻辑业务 stream**。Direct 模式可以直接映射到 iroh QUIC streams；Connector 模式可以把这些 inner frames 封装进 opaque tunnel。业务层不能假设 Connector 会解析 `StreamOpen`，更不能让 helper 依据工具类型转发。

建议每条逻辑 stream 首帧声明类型：

```text
StreamOpen {
    kind: CONTROL | TOOL | SHELL_IO | TCP_FORWARD | PROXY | FILE
    request_or_task_id
}
```

典型用途：

- Control：Hello、capability、task lifecycle、health；
- Tool：短 RPC；
- Shell I/O：attach 到持久 host task；
- TCP_FORWARD：每条 TCP connection 一条 stream；
- PROXY：每个 CONNECT/SOCKS target 一条 stream；
- FILE：每个 transfer 可有控制 stream + 若干有界 data stream。

MCP 消息从不直接穿过远端连接；MCP tool call 先进入 controller，再转换成 Clew protocol。

## 10. Tool Runtime

### 10.1 Glob / Grep / Read 必须 bounded

所有读取类工具都必须带/继承：

- result limit；
- byte limit；
- timeout/deadline；
- pagination/cursor（适用时）；
- truncation metadata。

不能让一个 `Grep /` 或几十 GB 日志直接塞满 controller 内存/MCP context。

### 10.2 Edit

建议：

```text
Edit {
    path
    expected_sha256?
    replacement
}
```

如果提供 precondition，内容已变化必须失败为 conflict，而不是覆盖 collaborator 新修改。

### 10.3 Write

Write 默认：

```text
write temp
fsync where appropriate
atomic replace
```

并受 policy root 约束。

### 10.4 Shell 是 Task

不要把 Shell 生命周期绑定到一次 MCP request 或 QUIC stream。

```text
shell.start -> task_id
shell.attach { task_id, offset? }
shell.status { task_id }
shell.cancel { task_id }
```

host 持有 process/task；controller 持有 task projection；MCP 可以把短命令包装成同步体验，但底层模型仍是 task。

stdout/stderr 必须有：

- bounded retained buffer；
- streaming cursor/offset；
- exit code；
- timeout/cancel；
- connection 重建后 reattach。

## 11. 动态 TCP Forward

Local listener 永远由 controller 持有：

```text
forward.add {
    id,
    side: local | remote,
    listen,
    dest
}

forward.remove { id }
forward.list
```

v1 优先：

```text
side = local
127.0.0.1:L -> host dest
```

`side=remote` 可以后置，因为它扩大 host 对外暴露面。

默认只允许 controller 本机 listener 绑 loopback；非 loopback 必须显式配置并通过 policy。

当前 V4a 实现冻结为：Controller 只绑定 loopback listener，listener 本身由 Controller runtime 持有，CLI `forward add` 返回后继续存在；Target outbound TCP 只有 signed `effective_grant.tcp_egress=true` 才允许，legacy/default/helper-only 全部 fail closed。

每条 accepted local TCP connection 分配独立 `ForwardConnectionId`，并固定到当时的 Target **session generation**。数据面不绕过业务 E2E 边界：Controller 与 Target 在原 Target↔Controller `InnerSession` 内交换 bounded `tcp_forward { Open / Exchange / Close }` RPC；单次读写 chunk ≤12 KiB，Host active session 最多 64 条 outbound TCP connection，Controller 最多 64 个 listener/每 listener 64 条 accepted connection。Helper 若在路径中仍只看到 InnerSession ciphertext。

已建立 TCP connection **不做 generation migration**：Target session 丢失/切 generation 后旧 local connection 关闭，不把 Exchange/Close 重放到新 generation；Controller listener 继续存在，新 local connection 可在新 generation 重新 `Open`。这与 §7.4 已冻结的“已建立 TCP stream 断线关闭、新连接恢复”语义一致，不能宣传成透明迁移。

## 12. SOCKS5 / HTTP Proxy

v1 先做 egress：

```text
controller loopback proxy
       ↓
Clew session
       ↓
host outbound connection
```

```text
proxy.add {
    id,
    kind: socks5 | http_connect,
    listen: "127.0.0.1:1080"
}
```

范围：

- SOCKS5 TCP CONNECT；
- HTTP CONNECT；
- 普通 forward proxy GET/POST 可后置；
- SOCKS5 UDP ASSOCIATE 不在 v1。

默认 loopback listener 可以无额外代理口令；如果允许非 loopback 监听，则必须显式认证/策略，不能沿用“空密码图方便”。

## 13. File Transfer Plane

不要用 Write 搬大文件。

控制面：

```text
file.put
file.get
file.status
file.cancel
```

Transfer manifest 至少记录：

```text
transfer_id
source identity/path
destination path
size
chunk_size
file hash
completed ranges/chunks
conflict policy
```

要求：

- chunk hash + final hash；
- resume；
- 有界并发；
- progress；
- cancel；
- directory tree；
- overwrite/rename/fail conflict policy；
- atomic finalization；
- policy root 检查。

连接重建后 controller/host 通过 `transfer_id + manifest` 对齐状态，再继续缺失 chunk。

## 14. MCP Adapter

### 14.1 生命周期

MCP 不拥有远端 session：

```text
MCP request
   ↓
Local Controller API
   ↓
controller state/session
```

`clew mcp --stdio` 是薄 adapter，可随 agent 启停。

Controller 可选内置 HTTP MCP：

```text
POST /mcp
bind 127.0.0.1:4877 by default
```

按当前 MCP 规范处理 protocol version、Origin/localhost 等边界。

旧：

```text
GET /sse
POST /messages
```

只作为显式 `--legacy-sse` compatibility；不让新架构依赖 legacy session semantics。

### 14.2 MCP tools

基础：

```text
Devices
PathInfo
Glob
Grep
Read
Edit
Write
Shell
```

动态能力：

```text
ForwardAdd / ForwardRemove / ForwardList
ProxyAdd   / ProxyRemove   / ProxyList
FileGet / FilePut / FileStatus / FileCancel
TaskGet / TaskCancel
```

`Devices` 必须返回可供 agent 做正确选择的信息：

```text
DeviceSummary {
  device_id
  site_name
  display_name
  hostname_observed
  online
  executable      # has EXECUTE capability
  connector       # has CONNECTOR capability
  last_seen
}
```

所有 Read/Grep/Glob/Edit/Write/Shell/File 类请求的 `device` selector 规则固定为：

1. 接受稳定 `DeviceId`；
2. 接受 `SiteName/DeviceName` qualified name，例如 `Alice 实验室/GPU-01`；
3. 仅当 display name 在 **在线 executable devices** 中唯一时，才接受短名 `GPU-01`；
4. helper-only (`executable=false`) 永远不是默认候选，也不能被执行类工具选中；显式选中时返回 `device_not_executable`；
5. 省略 `device` 时，只在恰好一个在线 executable device 时自动选择；否则返回候选列表并要求 agent/user 明确选择；
6. 重名永远不静默取第一台。

这样 agent 才能可靠表达“实验室那台 GPU”，而不是靠列表顺序猜。

## 15. GUI

人类日常不打开命令行。Controller 与 Windows/macOS Host 使用 windui 窗口；Linux Host v1 使用可观察 foreground 模式。GUI 按钮背后调用 Local Controller API。完整线框、自动生成清单和验收见 [03-gui.md](03-gui.md)。

### 15.1 Host UI：窗口只是前台视图，托盘才是长期驻留入口

Windows/macOS 提供极简主窗口 + 托盘/菜单栏。第一次启动默认显示主窗口，直到朋友能够确认：

- `正在连接`；
- `已连接`；
- `正在重连`；
- `暂时断开`；
- helper-only 时显示 `连接已就绪` / `正在帮助 N 台附近电脑连接`。

普通 friend UI 不直接写 `Relay` / `Direct`，技术路径放详情/诊断。

连接成功后主窗口给一句明确提示：

```text
已连接。
可以关闭这个窗口，Clew 会继续在托盘运行。
```

窗口生命周期必须与网络/runtime 生命周期分离：

```text
MainWindowVisible
      ↓ minimize / X
MainWindowHiddenToTray
      ↓ tray: 显示窗口
MainWindowVisible

只有：退出并断开
      ↓
RuntimeStopping
```

因此 `X` 不等于退出，也不能因为“当前恰好有会话/任务”而改变语义。

### 15.2 Tray runtime contract

托盘/菜单栏不是装饰，而是 friend-side 长期运行的主入口：

- tray 存在 = Clew runtime 仍在；
- main window 隐藏不影响 session、task、transfer、Connector；
- 显式 `退出并断开` 才停止 runtime；
- 显式 `暂时断开` 只暂停远程连接，程序仍留在托盘，可一键重新连接。

状态图标保持很少的视觉语义：

```text
GREEN   available / connected / helper ready
AMBER   connecting / reconnecting
GRAY    paused by user
RED     action required after sustained failure
```

不要用红色表达普通 Relay，也不要为 Direct/Relay 分裂两套图标。

Windows hover tooltip 保持短且不泄露具体路径/命令，例如：

```text
Clew · 已连接
连接到 CJ · 当前空闲
```

helper-only：

```text
Clew · 连接已就绪
正在帮助 2 台附近电脑连接
```

重连：

```text
Clew · 正在重连
无需操作
```

托盘菜单建议固定为：

```text
已连接到 CJ                    # disabled status row
显示 Clew
暂时断开                       # paused 时变“重新连接”
复制状态
----------------
退出并断开
```

helper-only 把“暂时断开”改成人话“暂停帮助连接”。高级 bug report / raw path / endpoint id 不放普通菜单；需要时可在主窗口“详情/复制诊断”或隐藏 debug 入口提供。

首次隐藏到托盘时可发送一次无按钮说明通知：

```text
Clew 会继续在托盘运行。
需要停止时，右键图标选择“退出并断开”。
```

之后不重复提示，也不为每次 agent Read/Shell 发系统通知。持续断线超过阈值、需要朋友动作时才发通知。

macOS 使用 menu bar 原生模式：点击图标打开同样的精简菜单；hover 不是核心交互，因此关键状态必须同时体现在菜单首行和图标状态中。

如果朋友启用“登录后自动保持可用”，后续登录默认 **tray-only/menu-bar-only 启动**，不要每次把主窗口抢到前台。

GUI 只订阅 runtime state，不直接操作 iroh stream 或实现 RPC。

### 15.3 睡眠、合盖与恢复

Clew 不阻止系统睡眠，也不因为 helper 角色去申请永久 keep-awake。Windows/macOS 能收到 suspend 事件时，host best-effort 发 `MemberSuspending`；Controller 可显示“睡眠”。收不到通知时只能显示“离线/等待连接”，不能假装知道远端仍醒着。

resume 后 runtime 必须自动：重建 outer transport、重新建立 InnerSession、刷新 endpoint/address、重新广播 Connector presence/lease。锁屏本身不等于断线；合盖是否睡眠遵循 OS 设置。

恢复语义沿用业务层规则：Read 可 retry；仍存活的 Shell task 可按 `task_id` reattach；File resume；已经建立的 TCP stream 关闭重建。helper 睡眠时 target 优先 failover 到其它 Connector，没有其它路径才显示“正在重连”。

### 15.4 Controller 本机活动记录

Controller 保存一个 **本机、有限、可清空** 的 ActivityStore，目标是回答“刚才 agent 对哪台电脑做了什么”，不是企业审计平台。

建议 `ActivityEvent` 记录：时间、Site/Device、操作类型、路径或命令摘要、结果、耗时、传输字节；默认不保存文件内容、stdout/stderr 全文或环境变量。日志只落 Controller 本机，不上传；使用条数/时间双重轮转（例如 7 天或 10k 条），GUI 可按 Site/Device 过滤并“一键清空”。Local API 为 `activity.list/activity.clear`。

### 15.5 Linux v1：没有托盘就不要假装有

Linux v1 不把可用性绑定到 system tray。基线是 foreground/headless：

```text
clew host --foreground
状态：已连接到 CJ · 当前空闲
Ctrl-C 退出并断开
```

如果以后某桌面环境实现可靠 StatusNotifier/AppIndicator，可增量提供 tray；在此之前不能把窗口隐藏成用户找不到的后台进程，也不默认提供“登录后静默保持可用”。

## 16. Packaging、Distribution Studio 与 ClientFlavor

完整穿搭/branding 设计见 [05-distribution-studio.md](05-distribution-studio.md)。这里固定构建边界。

### 16.1 三层模型

```text
PermissionProfile          # 能做什么
OutfitProfile              # 长什么样、怎么说话
SiteInvite                 # 给谁/哪个 Site
        ↓
ClientFlavor + site.clew
        ↓
Site Kit
```

`OutfitProfile` 与权限、identity secret 严格分离。

### 16.2 Distribution Studio

Controller GUI 提供“分发穿搭”入口，支持：

- 内置 preset；
- create / duplicate / reset / set-default / import / export；
- app name / window title；
- app icon / Logo / tray base icon / key visual；
- 少量 color tokens；
- friend-side strings + locale；
- 主窗口、helper、tray/menu、Site Kit live preview。

CLI 等价入口：

```text
clew studio
clew outfit list
clew outfit new "Huang Lab" --preset research-lab
clew outfit build huang-lab --target windows-x86_64
clew invite alice --outfit huang-lab --profile research-full
```

### 16.3 Build-time branding 与 invite-time data 分离

OS 可见 app icon/name 等资源进入可复用 `ClientFlavor`：

```text
BaseRuntime
  + OutfitProfile(revision)
  + platform resources
        ↓
pack
        ↓
sign / notarize
        ↓
ClientFlavor
```

邀请数据不修改已签名程序：

```text
Signed ClientFlavor
        +
site.clew
        +
开始这里 / role hints / chat copy
        ↓
Alice-Clew-Windows.zip
```

因此换 Alice/Bob 不重新签 app；只有 Outfit 或 runtime/platform 资源变化才生成新的 ClientFlavor。

### 16.4 Windows

Windows brand build 在 Authenticode 签名前生成：

- app icon（自动生成多尺寸 ICO/resource）；
- `ProductName` / `FileDescription` 等 VERSIONINFO；
- package/manifest visual resources（适用时）；
- runtime UI resource bundle。

用户只上传 PNG/SVG 和填写人话字段，不直接操作 `.rc` / manifest。

最终目标：portable、signed、双击启动，不规避 SmartScreen。

### 16.5 macOS

Outfit 可影响：

- `CFBundleDisplayName` / `CFBundleName`；
- app icon；
- `InfoPlist.strings` 本地化显示资源；
- runtime UI resource bundle。

这些必须在签名前定型：

```text
assemble branded .app
  -> codesign
  -> notarize
  -> cache ClientFlavor
```

邀请特有 `site.clew` 保持在 signed `.app` 外的 Site Kit 中，避免每个 collaborator 都重新 notarize。

### 16.6 ClientFlavor cache

```text
FlavorKey = hash(runtime_version, outfit_revision, target_os, target_arch, signing_profile)
```

Distribution Studio 显示每个平台 flavor 是否“已就绪 / 需要构建 / 构建失败”。邀请时优先直接复用缓存 flavor。

### 16.7 Site Kit 按平台生成；`site.clew` 查找与丢失恢复写死

邀请默认 **按目标 OS 分开生成**，而不是一个同时塞 Windows/macOS/Linux runtime 的 universal fat zip。多选平台时输出多个明确文件，例如 `Alice-Clew-Windows.zip`、`Alice-Clew-macOS.zip`、`Alice-Clew-Linux.tar.gz`。

`site.clew` 只负责 bootstrap/config，不是 DeviceKey。启动查找顺序固定：

1. 显式传入/打开/拖入的 `.clew` 文件（GUI file picker、drag-drop、`--site`）；
2. executable / `.app` **同级目录**的 `site.clew`；
3. 若 sidecar 缺失，查询本机持久 state：同一 ClientFlavor 只有一个已 enrollment membership 时直接恢复；多个时给人话选择器；
4. 都没有时进入“缺少邀请文件”页面，不扫描全盘、不猜当前工作目录。

找到 `site.clew` 后，用 `(ControllerId, SiteId, OS-user scope)` 查已有 DeviceKey；有则复用并跳过 claim。首次 enrollment 成功后只持久化长期 identity/controller hints，不把 bootstrap secret 当长期凭据保存。

缺 sidecar 的固定人话：**“还缺一个邀请文件。请把 `site.clew` 和这个程序放在同一个文件夹，或把 `site.clew` 拖到这里。”** 提供“选择邀请文件”按钮。若检测到从压缩包临时目录直接运行，则优先提示 **“请先全部解压这个压缩包，再打开程序。”**

Site Kit 的聊天稿也明确一句“请先完整解压，程序和 `site.clew` 要放在一起”。不绕过 Mark-of-the-Web / SmartScreen；通过正常代码签名和人话说明处理。

## 17. Directory：为什么 v1 不做

v1 是 host 主动连接预绑定 controller：

```text
host -> controller
```

controller 自然已经知道：

```text
Alice online
Bob offline
Carol online
```

所以不需要先做 RustDesk hbbs / Syncthing global discovery 那类独立发现服务。

未来只有出现以下需求再引入：

- 多 controller；
- controller federation；
- host roaming；
- 非预绑定 controller 发现。

届时仍坚持：Directory 只返回身份/地址 hint，不经手业务数据。

## 18. Site / Connector：多机不是“部署 Gateway”

完整 UX 与数据模型见 [04-site-connector-ux.md](04-site-connector-ux.md)。这里固定架构不变量。

### 18.1 Site 是稳定对象

```text
Controller
   ↓
Site
   ├── member: EXECUTE
   ├── member: EXECUTE + CONNECTOR
   └── member: CONNECTOR-only
```

目标节点只绑定 `SiteId`，不绑定某个具体 GatewayId。多个 Connector 在同一 Site 中可以自动替换和 failover。

### 18.2 Connector 是 capability，不是独立 binary

统一 Clew runtime 根据 enrollment profile / controller policy 获得：

```text
EXECUTE
CONNECTOR
```

同一设备可同时拥有两者。能直连 controller 的 EXECUTE member 在允许时可以自动 promotion 为 CONNECTOR，帮助附近无法直连的同 Site 节点。

### 18.3 Connector 是 userspace outer tunnel，不是 L3 router，也不是业务 MITM

朋友不配置 IP forwarding、CIDR、route、NAT。Connector 可以终止 Target↔Connector / Connector↔Controller 的 **outer transport links**，但它处理的 payload 必须已经是 Target↔Controller `InnerSession` ciphertext。

明确禁止：

```text
Target -- plaintext Clew RPC --> Connector -- plaintext Clew RPC --> Controller
```

允许的是：

```text
Target ============================================= Controller
       authenticated + encrypted Clew InnerSession
          \                                       /
           outer link -> Connector -> outer link
                        opaque only
```

Connector outer header 只保留完成路由/背压所需的 tunnel id、sequence/length、direction 等最小元数据；`StreamOpen.kind`、tool name、path、command、file bytes 全部在密文内。helper 日志也不得打印 inner payload。

### 18.4 Enrollment 必须可以穿过 Connector，但仍 sealed to Controller

完全无公网的 Target 第一次启动时，`SiteBootstrapPass + fresh DevicePublicKey` 形成 **sealed-to-Controller enrollment envelope**，经 Connector 原样搬运。Connector 不读取 bootstrap secret、不验证业务 grant、不替 Controller 消费 claim。

Controller 返回的 enrollment result 同样端到端封装给 Target；成功后 Target 落下 DeviceKey，随后建立正常 InnerSession。

### 18.5 v1 已裁定 A，不保留 B 兼容模式

v1 安全语义固定为：**Connector 看不到 Shell/文件/工具业务明文**。如果实现只能通过在 helper 上终止两条业务会话才能工作，则该 Connector slice 视为未完成，而不是降级发布。

验收至少包括：在 helper 侧抓应用日志/内存中的协议 payload，不应出现测试用 Read 内容、Shell command 或 file bytes；同时允许看到包长/时序等不可避免的流量元数据。

## 19. CLI 入口

CLI 是 GUI 与脚本的同一套 API 的命令面，不是给人日常用的主界面。`--help` 应指向对应按钮。

v1 目标：

```text
clew controller [--mcp-listen 127.0.0.1:4877]

clew host --foreground                 # Linux v1 / diagnostics

clew mcp --stdio [--device name]

clew invite <name> [--profile research-full] [--target windows-x86_64 ...]
    # 首选：直接生成 site-capable Site Kit + 说明

clew mint <name> [--profile research-full]          # 底层/排障
clew pack --worm x --target ... --out ...           # 底层/排障

clew ls
clew status [device]
clew device rename <device> <new-name>
clew device revoke <device>
clew invite close <invite>
clew invite revoke <invite>
clew site revoke <site>
clew activity [--device ...]
clew controller backup export --out ...
clew controller backup restore <file>

clew forward add ...
clew forward remove ...
clew forward list

clew proxy add --socks5 --listen 127.0.0.1:1080
clew proxy remove ...
clew proxy list

clew file put ...
clew file get ...
clew file status ...
clew file cancel ...
```

`clew directory` 不在 v1 命令面；不设计单独的日常 `clew gw` 命令，连接助手是 Site member capability，由 Site Kit / Controller 自动配置。

如果 packaged collaborator artifact 启动，则直接进入 host UI；普通开发/调试 binary 无 embedded enrollment 时显示 CLI usage，而不是猜测角色。

## 20. 开发分期：按 vertical slice，不按 crate 清单

### V0 — 本机骨架，不阻塞第一条 Read

只完成真正需要先存在的本机骨架：

- controller 单实例 + Local API；
- Controller GUI 空列表 / ready 状态；
- stable `ControllerId/SiteId/DeviceId` 类型和 state-store skeleton；
- `SiteMember` + `EXECUTE/CONNECTOR` 数据模型占位；
- wire/proto 工程骨架。

V0 **不要求** Distribution Studio、LAN discovery、Connector data plane 或完整文件/代理功能。

验收：controller 启动后，第二个 CLI 进程能通过 Local API 查询状态；第二个 controller 不能创建平行 ownership。

### V1 — 一台能联网电脑：Site Kit → 双击 → Read

这一刀完成：

- ControllerKey / DeviceKey + signed bootstrap；
- per-platform Site Kit + `site.clew` loader/recovery；
- local membership 复用和 host 单实例；
- hostname 默认设备名 + rename；
- direct iroh outer transport；
- **Target↔Controller InnerSession E2E**；
- 第一条 bounded `Read`；
- Controller ActivityStore 最小记录；
- `device.revoke` 和关闭 bootstrap 的最小收口；
- Controller 身份备份入口；
- Windows/macOS Host window + tray；Linux foreground fallback。

穿搭只要求所有 friend-facing strings/icon references 经过统一 `UiResources/OutfitRuntimeView`；V1 使用 Clew Original，不要求 Studio 已完成。

```text
invite alice -> Windows Site Kit
    ↓
friend 完整解压 + 双击
    ↓
bootstrap enrollment
    ↓
persistent DeviceKey + hostname display name
    ↓
direct outer connection
    ↓
InnerSession E2E
    ↓
Read
```

验收必须在两台真实机器上完成；第二次双击不能出现第二台设备，控制者能停止该设备，helper/Studio 都不阻塞这一刀。

### V1.25 — Distribution Studio foundation

增加：

- `OutfitProfile` schema + revision；
- Clew Original / Research Lab / Friendly Minimal / Institution Clean 等 preset；
- Controller GUI 穿搭库与 live preview；
- PNG/SVG import + icon/tray asset generation；
- strings/locales；
- `clew outfit` CLI；
- `clew invite --outfit`；
- `ClientFlavor` build/cache 接口（正式 signing 可在 V6 收口）。

验收：不改源码即可从 preset 创建 Huang Lab Outfit，修改 icon/name/color/中文字符串并用于一个真实 Site Kit；朋友端连接步骤与默认 Clew 完全相同。

### V1.5 — Zero-config Site Connector

在 V1 的 InnerSession 已经跑通之后增加：

- 一个 Site Kit 在有限部署窗口内 claim 多台设备；
- LAN/mDNS 自动发现同 Site member；
- sealed enrollment through Connector；
- opaque outer tunnel forwarding；
- `EXECUTE` member 自动 promotion 为 CONNECTOR（policy 允许时）；
- 多 Connector failover；
- helper suspend/resume 后自动重新广播；
- Friend UI 只说“已连接 / 正在帮助附近电脑连接”。

硬门禁：Connector 不解析 inner business frames；helper-only 也不是 MCP executable target。

验收：目标机完全无法访问公网时，朋友在目标机和联网电脑各双击一次，顺序无关，目标机完成首次 enrollment 并能被 Controller 使用；全程不输入 IP、端口、code、route；helper 侧看不到 Read/Shell/File 测试明文。

### V2 — Agent minimum

增加：

- Glob/Grep/Read/Edit/Write；
- Shell task；
- MCP stdio；
- MCP Streamable HTTP；
- bounded result / timeout / cancellation。

### V3 — Reliability

增加：

- connection reconnect；
- request idempotency/replay rules；
- Shell reattach；
- path telemetry；
- version/capability compatibility matrix。

### V4 — Dynamic networking

增加：

- local TCP forward；
- SOCKS5 TCP egress；
- HTTP CONNECT；
- controller-owned listener recovery。

### V5 — File plane

增加：

- chunk manifest/hash；
- resume；
- directory transfer；
- bounded concurrency；
- progress/cancel/conflict policy。

### V6 — Release packaging

完成：

- Windows signed portable artifact；
- macOS `.app` signing/notarization；
- Linux artifact；
- Client Generator workflow。
- Distribution Studio release pipeline；
- Windows icon/VERSIONINFO branding；
- macOS bundle display-name/icon/localization branding；
- ClientFlavor signing/notarization/cache；
- Outfit cross-platform preview/smoke。

实际开发中 packaging smoke 应更早开始，V6 指发布级收口。

### V7 — Advanced Service Runtime

这是 **显式 opt-in 的高级长期在线模式**，不改变 V1 默认的 portable/tray/foreground 体验，也绝不能因为设备获得 `CONNECTOR` capability 就静默安装服务。

支持路径按风险分层：

1. **Linux `systemd --user`**：优先实现的长期在线模式；不需要 root，沿用该 OS user 的权限边界和 state scope，runtime 由 user manager 保活，GUI/CLI 仍通过 Local API 作为 client。默认语义是“用户登录后常驻”；若用户明确要求“开机未登录也运行”，再提供显式 linger 选项并说明它改变了用户会话生命周期，不能默认打开。
2. **Windows Service**：machine-level 后台 runtime。Service 拥有 transport/session/task/Connector 状态；tray/GUI 必须是用户会话里的独立 client，通过受 ACL 保护的 local IPC 连接 Service，不能把交互 UI 塞进 Session 0。安装/卸载必须显式请求管理员权限。
3. **Linux system service**：machine-level `systemd` unit，使用专用低权限 service account 与 machine state directory；安装/卸载需要显式管理员权限，不默认以 root 执行远程 Shell/File。

服务模式的安全/身份不变量：

- portable/user runtime 与 machine service **不静默共享 DeviceKey**；从 user scope 切到 system scope 要么 Controller 明确批准 identity migration，要么作为新的 SiteMember enrollment；
- Windows Service / Linux system service 默认最适合 `CONNECTOR-only`；若要 `EXECUTE`，必须显式选择 service execution identity、filesystem roots 和 Shell policy，不能因为服务是 LocalSystem/root 就自动获得整机权限；
- service crash/reboot 后自动恢复 transport/InnerSession/Connector advertising，但所有 `device.revoke/site.revoke` 仍是硬拒绝条件；
- UI 必须明确显示“后台常驻已启用”。在 service mode 下，“关闭窗口/退出 GUI”和“停止后台服务”是两个动作，不能沿用 portable 模式的 Exit 语义造成误解；
- install/enable/disable/stop/uninstall 都有 GUI/CLI 对应入口和可见状态，不做隐蔽 persistence。

验收：Windows 重启或 Linux reboot/login 后，在用户明确启用的对应 service mode 下自动恢复连接；GUI 不运行时 service 可以继续工作；停止/卸载 service 后能力消失；machine service 的 EXECUTE 权限不会因为高权限 service account 被隐式放大。

### V8+

再评估：

- Directory；
- dedicated relay management；
- 第二 transport；
- Linux tray；
- SOCKS5 UDP。

## 21. v1 验收场景

最终 v1 至少满足：

1. 为 collaborator 生成 **按目标平台** 的 Site Kit，合作者无需输入 code 即可启动；
2. `site.clew` 缺失时显示固定人话恢复页；直接从 zip 临时运行时提示先完整解压；
3. 同一 OS user/site 第二次打开复用 DeviceId；已有进程时第二次启动只唤起窗口；
4. 四台机器使用同一 Site Kit 时以 hostname 得到设备名；发生碰撞时整组切换为固定 5 字符 DeviceTag 形式（如 `GPU-01-K7M4Q`），不用 `(2)` 序号，且可 `device.rename`；
5. agent `Devices` 能看到 Site/name/executable/connector，helper-only 不可被 Read/Shell/File 选中；重名必须报候选而不是静默选择；
6. 第一条 direct `Read` 已运行在 Target↔Controller InnerSession 上；
7. 多机 Site 经 Connector 时复用同一 InnerSession，helper 看不到 Read/Shell/File 业务明文；
8. controller 记录持久 DeviceId，host 重启后不重复 enrollment；
9. controller 能“停止这台”、关闭继续加入、作废一份分发包、停止整个 Site；
10. controller 提供加密身份/状态备份入口；无备份丢失 ControllerKey 时明确要求重新邀请，旧 host 不自动信任新 key；
11. Controller GUI 有本机 Activity 记录，能回答刚才在哪台设备读了什么路径/跑了什么命令，日志有界且可清空；
12. suspend/resume 后 host 自动重连；Connector 醒来重新广播；锁屏不被当作退出；
13. Relay→Direct path 变化时业务 stream 不因 Clew 自己重连而中断；
14. 整个 connection 丢失后，Read 可安全 retry，Shell 可按 task id reattach，File 可 resume；
15. `clew forward add` 后 CLI 退出，listener 仍存在；
16. SOCKS5 能让本机应用通过 host 访问其可达网络；
17. 大文件传输中断后可 resume，并验证最终 hash；
18. policy 能拒绝 root 外文件访问或未授权 listener；
19. collaborator 点击“退出并断开”后远程能力立即消失；Windows/macOS 收窗口到 tray 不等于退出；
20. Linux v1 在无 tray 时以前台可观察进程运行，Ctrl-C 明确退出，不藏成不可见后台。

## 22. 已知风险与约束

- 公益 iroh relay 无生产 SLA，需要 dedicated/self-hosted relay 路径；
- NAT/企业网络环境不可控，必须把 relay 作为正常路径而不是异常路径；
- connection-level 断开无法让任意 TCP 连接魔法般无损迁移，文档和 UI 不应做这种承诺；
- bootstrap enrollment 需要处理“controller 已登记 claim 但 host 本地落盘失败”等半提交恢复；SiteBootstrapPass 还要处理多 claim 并发与部署窗口重开；
- DeviceKey、controller state 和 local IPC 权限是 v1 真正的安全边界，不能只关注 worm 加密；
- Windows/macOS signing 流程差异大，packaging 必须早做 smoke；
- protobuf/capability negotiation 会增加早期代码量，但能避免 host/controller 版本错位后的大规模兼容债；
- Host UI 技术栈若无法稳定覆盖 Windows/macOS，不应让 UI 框架反向绑死 runtime；GUI 始终是 adapter。
- Controller backup 是 point-in-time；恢复过旧备份可能缺少后来新增/撤销的设备状态，恢复后必须让控制者 review 设备列表；restore 默认不自动重新打开旧 bootstrap pass；
- ControllerKey 无备份丢失时没有“自动找回”路径，这是 v1 明确灾难恢复边界；
- revoke 能立即阻止 Controller 发起/接受后续控制，并在在线时 best-effort cancel 任务；对已经离线、脱离 Controller 运行的 OS 进程不能承诺瞬时远程杀死；
- sleep 通知是 best-effort，Controller 只能在收到 suspend event 时显示“睡眠”，否则显示离线/等待连接；
- Connector E2E 隐藏业务内容，不隐藏包长、时序、连接关系等流量元数据。
