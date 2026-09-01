# Clew Development Checkpoint

状态：**正式开发前基线**
架构基线：**Architecture v1.5**
更新时间：**2026-09-01**

本文是 Clew 开始实现后的**唯一开发进度台账**。`00`–`06` 文档定义产品、架构和已经裁定的边界；本文负责回答四个问题：**现在做到哪一块、这一块怎样算完成、验证过什么、下一块是什么。**

每完成一个 coherent development block，都必须在进入下一块前同步更新本文。不要另外维护一份会漂移的计划。

## 1. 更新规则

每个开发块完成时至少更新：

1. `Status`：`TODO` / `IN PROGRESS` / `BLOCKED` / `DONE`；
2. 完成日期；
3. 实际落地的主要代码/文档路径；
4. 验证证据：测试、smoke、真实机器验收；
5. 未解决风险或明确 deferred 项；
6. 下一开发块；
7. 如本块形成独立 commit，记录 commit short hash/subject。

**禁止**只因为“代码写完了”就标 `DONE`。至少要完成本块列出的 acceptance gate；平台能力暂时无法验证时必须写成 `BLOCKED` 或明确记录缺失证据。

建议一个 coherent block 对应一个可独立 review 的 commit；如果一个 block 跨多个 commit，checkpoint 记录最后一个收口 commit。

## 2. 不允许在实现中重新打开的架构边界

以下已经由 Architecture v1.5 裁定，除非显式开启新的 architecture revision，否则实现必须遵守：

- Controller 是长期状态唯一 owner；GUI/CLI/MCP 都是 Local API client/adapter；
- Site 是基础对象，单机只是一个 member 的 Site；能力使用 `EXECUTE` / `CONNECTOR`，不重新引入独立 GW 角色；
- **Target↔Controller `InnerSession` 是业务安全边界**；Connector 只搬 opaque ciphertext，不能读取 Read/Shell/File 业务明文；
- Helper-only `EXECUTE=false`，绝不能被 MCP/Read/Shell/File 当成执行目标；
- 默认设备名来自 hostname；冲突使用稳定 5 字符 `DeviceTag`（如 `GPU-01-K7M4Q`），不使用 `(2)`/`(3)` 顺序号；
- DeviceKey 存 OS-user state，不写在 Site Kit 旁；相同 machine/user/site 重开复用 DeviceId；
- `site.clew` 固定恢复顺序：显式选择/拖入 → 程序同目录 → 本机 membership → 人话缺失页；
- `device.revoke`、`invite.close`、`invite.revoke`、`site.revoke` 语义分离；
- Controller identity 必须可加密备份；无备份丢 key 时重新邀请，不做无声 key migration；
- Windows/macOS v1 是可见 GUI + tray/menu bar；Linux v1 是可观察 foreground，不假装有可靠 tray；
- Advanced Service Runtime 后置到 V7；不得因为 Connector promotion 静默安装 Windows Service/systemd service；
- 不提前扩张第二 transport、Directory、企业 IdP、远程桌面或任意 HTML skin。

详细约束见 [01-design.md](01-design.md) 与 [06-gaps.md](06-gaps.md)。

## 3. 总体路线图

| Block | 目标 | Status | 核心验收 |
|---|---|---|---|
| P0 | 开工前仓库/文档基线 | DONE | checkpoint、repo hygiene、Rust baseline validation、首个基线 commit |
| V0.1 | Stable IDs / state / proto skeleton | DONE | 类型与 schema 可编译、序列化边界有测试 |
| V0.2 | Controller single-instance + Local API | DONE | 第二进程能查询状态，不能产生第二份 ownership |
| V0.3 | Controller GUI shell | TODO | GUI 空列表/ready 状态来自 Local API，不直接持有网络状态 |
| V1.1 | ControllerKey / DeviceKey / enrollment | TODO | signed bootstrap、持久 DeviceKey、claim/半提交边界有测试 |
| V1.2 | Site Kit / Host lifecycle / naming | TODO | sidecar recovery、identity reuse、DeviceTag、Host 单实例与基础 UI |
| V1.3 | Direct iroh + InnerSession E2E | TODO | Controller/Target 双向认证，业务 payload 全部在 inner ciphertext |
| V1.4 | Bounded Read + v1 control plane | TODO | 两台真实机器 Read；Activity/revoke/backup 最小闭环 |
| V1.25 | Distribution Studio foundation | TODO | preset → preview → branded Site Kit，不增加朋友步骤 |
| V1.5 | Zero-config Site Connector | TODO | 无公网 Target 经 helper 首次 enrollment + Read，helper 看不到业务明文 |
| V2 | Agent minimum | TODO | Glob/Grep/Read/Edit/Write/Shell + MCP stdio/HTTP + bounds/cancel |
| V3 | Reliability | TODO | reconnect/replay/reattach/resume/version negotiation |
| V4 | Dynamic networking | TODO | TCP forward / SOCKS5 TCP / HTTP CONNECT，listener 归 Controller |
| V5 | File plane | TODO | chunk/hash/resume/directory/bounds/progress/cancel |
| V6 | Release packaging | TODO | Windows signing、macOS signing/notarization、Linux artifact、ClientFlavor pipeline |
| V7 | Advanced Service Runtime | TODO | systemd --user → Windows Service/Linux system service，显式 opt-in |
| V8+ | Deferred expansion | TODO | 仅按真实需求评估 Directory、dedicated relay、第二 transport 等 |

**第一条对外可用版本仍是 V1.4 完成后的 V1。** Studio 和 Connector 都不能阻塞 direct Read。

## 4. P0 — 正式开发前基线维护

**Status：DONE**
**Date：2026-09-01**

### 范围

- 固化 Architecture v1.5 设计文档；
- 建立本 checkpoint；
- 补跨平台 repository hygiene：generated artifacts/local state/secret-like env 文件忽略；
- 统一文本换行与 editor 基线；
- 根 README 与 docs 索引指向开发计划；
- 保留应用型 Rust binary 的 `Cargo.lock`；
- 对当前 Rust 占位包执行 `cargo fmt -- --check`、`cargo check --all-targets`、`cargo test --all-targets`；
- staged `git diff --check` + workspace hygiene；
- 建立首个 repository baseline commit。

### Acceptance

- [x] `docs/DEVELOPMENT_CHECKPOINT.md` 存在且可持续更新；
- [x] Architecture v1.5 的硬边界在计划中显式列出；
- [x] `.gitignore` / `.gitattributes` / `.editorconfig` 有跨平台基线；
- [x] Rust baseline validation 通过；
- [x] staged diff/hygiene 通过；
- [x] baseline commit：`chore: establish clew development baseline`（P0 root commit）。

### Validation evidence

- `cargo fmt -- --check`：PASS；
- `cargo check --all-targets`：PASS；
- `cargo test --all-targets`：PASS（当前占位包 0 tests，验证的是 build/test harness 基线）；
- Markdown local-link scan：PASS，`markdown_link_errors=0`；
- `git diff --cached --check`：PASS；
- pre-commit workspace hygiene：无临时文件、缓存或 secret-like artifact；当时仅存在待提交的 staged worktree，属预期。

P0 至此允许进入 V0.1。

## 5. V0 — 本机骨架

### V0.1 — Stable IDs / State / Proto Skeleton

**Status：DONE**

**Date：2026-09-01**

目标是先建立后面所有 vertical slice 共用、但不掺入网络实现的最小数据骨架。

实际落地：

- 根包升级为 compact Cargo workspace；本块只新增真实需要的 `crates/clew-core` 与 `crates/clew-proto`，没有提前创建 `runtime` / `mcp` / transport / directory / gw 空 crate；
- `clew-core` 建立 `ControllerId` / `SiteId` / `DeviceId` / `InviteId` 强类型 UUID 边界，拒绝 nil / 错误长度输入；
- 建立 `SiteMember`、`MemberCapabilities { execute, connector }`、`DeviceRecord` / `DeviceSummary`，并保留 `enrolled_via_invite_id` provenance；
- 建立 domain-separated 5 字符 Crockford Base32 `DeviceTag`：仅由 `DeviceId + tag_generation` 派生，不读取 hostname/MAC/序列号等信息；碰撞时确定性递增 generation，显示长度不变；
- 建立 versioned state envelope 与 `v1/` state layout skeleton，membership 路径显式按 Controller/Site/Device scope；同 schema version 容忍未知字段，未知 schema version fail closed；
- state JSON encode/decode 在 serde 前后都有 16 MiB document hard limit；
- `clew-proto` 使用 `proto3 + prost` 建立 `clew/1` 的 Hello、PeerRole、Feature、Request/Response Envelope、Error/ErrorCode skeleton；
- wire validator 固化 wire-major、ID、role、peer-advertised frame/concurrency bounds；未知正数 feature 保留以支持同一 wire major 内前向兼容；
- protobuf decode/encode 入口在 prost 前执行 16 MiB hard frame limit；build 使用 vendored `protoc`，不依赖开发机全局安装版本。

主要路径：

- `crates/clew-core/src/{id,device,naming,state}.rs`
- `crates/clew-proto/proto/clew/v1/clew.proto`
- `crates/clew-proto/src/validate.rs`
- `crates/clew-proto/build.rs`
- `Cargo.toml` / `Cargo.lock`

Acceptance：

- [x] IDs 不以 hostname/IP 充当身份；
- [x] DeviceTag 定长、稳定、非安全凭据，冲突路径有确定性测试；
- [x] wire/state schema 有明确版本边界与前向兼容测试；
- [x] malformed/truncated/oversized serialization input 有 fail-closed 测试；
- [x] workspace `fmt/check/test` 全通过。

Validation evidence：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，`clew-core` 14 + `clew-proto` 6 = **20 tests passed**；根 binary 仍为 0 tests；
- 覆盖：typed ID roundtrip/nil/length、helper-only capability projection、DeviceTag 稳定性/碰撞重派/字符集、state version/unknown field/malformed/oversize、Hello unknown feature roundtrip、wire major/ID/bounds、truncated/oversize protobuf；
- 独立 review 后补上 state/proto **decode-before-parse size guard**，避免只验证字段却允许无界 document/frame 先进入 parser。

Known / deferred：

- 本块刻意没有实现 Controller runtime、Local API、持久锁、GUI、iroh、enrollment 或 Connector；这些分别属于 V0.2+ / V1+；
- `src/main.rs` 仍是占位入口，V0.2 才开始把根 `clew` 接到 Controller/Local API vertical slice；
- commit subject：`feat: establish v0.1 data and protocol skeleton`。

V0.1 至此允许进入 V0.2。

### V0.2 — Controller Single Instance + Local API

**Status：DONE**

**Date：2026-09-01**

实际落地：

- 新增真正进入 vertical slice 的 `clew-runtime` crate；Controller runtime 成为 Local API listener 与本机 ownership 的唯一 owner，没有提前引入 iroh/session/GUI；
- 使用 Rust 标准库跨平台 file lock 持有 `v1/controller.lock`；lock 文件可以残留，但进程退出/崩溃后内核锁自动释放，下一实例可安全重新取得 ownership；
- shutdown 析构顺序显式保证 Local API listener/state 先销毁，`ControllerOwnership` 最后释放，避免新实例在旧 endpoint 尚未完全关闭时抢到 owner；
- Windows Local API 使用 `\\.\\pipe\\clew-controller-<state-root-hash>` named pipe，并显式 `reject_remote_clients(true)` / first-pipe-instance；
- Unix Local API 使用 `v1/controller.sock` UDS，state version dir 设为 `0700`、socket/credential 设为 `0600`；
- Local API 再叠加每次 Controller 启动随机轮换的 256-bit credential；secret `Debug` 固定脱敏，默认放在 OS-user state scope；
- Local API v1 使用 4-byte length-prefixed bounded JSON frame，hard frame limit 1 MiB、最多 16 个并发 connection、单次 I/O 2 秒 timeout；未认证/半帧连接不能无界占用 handler；
- 第一批只读方法落地为 `controller.status` / `device.list`；当前 DeviceRegistry 尚未进入 V1，因此 `device.list` 正确返回空列表；
- 根 `clew` CLI 落地 `controller` / `status` / `devices`：第二个 `clew controller` 遇到已有 owner 时转为 Local API client，查询同一个 instance 后退出，不创建第二份 runtime；
- graceful shutdown 会停止 listener/handler 并释放 ownership；异常 kill 后同一 state dir 可以直接重启。

主要路径：

- `crates/clew-runtime/src/{config,lock,transport,local_api,controller}.rs`
- `crates/clew-core/src/state.rs`
- `src/main.rs`
- `tests/controller_cli.rs`
- `Cargo.toml` / `Cargo.lock`

Acceptance：

- [x] 启动第二个 Controller 不能形成平行 state owner；
- [x] 第二进程可以通过 Local API 查询 ready/status，并可查询 `device.list`；
- [x] Local API 默认只使用本机 IPC；Windows 拒绝 remote named-pipe client，Unix 使用 private UDS；两侧均叠加 state-scope credential；
- [x] graceful shutdown 后可重新取得 owner；stale lock/异常 kill 后可恢复；
- [x] Local API frame/concurrency/I-O 全部有硬边界。

Validation evidence：

- Windows/cap00 真实跨进程 integration：PASS；第一个 `clew controller` 启动后 `clew status` / `clew devices` 通过，第二个 `clew controller` 返回 `already running` 且 instance id 与第一实例一致；
- 同一 integration 强制 kill 第一 Controller 后，复用同一 state dir 再启动：PASS，新 instance id 正常生成，证明 stale lock file 不保留 ownership；
- runtime 单测覆盖 graceful shutdown/reacquire、parallel ownership rejection、stale lock recovery、Local API auth、oversized frame、Windows local pipe name；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS：root integration 1 + `clew-core` 14 + `clew-proto` 6 + `clew-runtime` 7 = **28 tests passed**；
- `cargo check --workspace --all-targets --target x86_64-linux-android`：PASS，用现有 Unix target 静态覆盖 `cfg(unix)` UDS/permission 路径；
- 独立 review 后补上两处 hardening：16-connection/2s-I/O bound，以及 listener-before-ownership 的 shutdown drop ordering。

Known / deferred：

- 本轮实际运行 smoke 是 **Windows named pipe**；cap00 没有 Linux/macOS desktop runner。Unix 路径已有 cross-target compile gate，但 Linux/macOS 的真实 UDS runtime smoke 仍缺失，后续拿到对应 runner 时补证据；
- Windows credential 的默认权限边界依赖 `%LOCALAPPDATA%` 当前 OS user 的 ACL，再配合 local-only named pipe 与随机 credential；显式 `--state-dir` 是开发/测试 override，调用者应放在受当前用户保护的目录；
- 本块没有实现自动拉起 Controller、GUI/tray、ControllerKey、DeviceRegistry 持久化或远端网络；GUI 属于 V0.3，身份/设备/网络属于 V1；
- commit subject：`feat: add v0.2 controller local api`。

V0.2 至此允许进入 V0.3。

### V0.3 — Controller GUI Shell

**Status：TODO**

计划：

- Controller 主窗空列表、ready/error 状态；
- GUI 只消费 Local API，不直接持有 iroh/session 状态；
- tray 生命周期骨架；
- 为后续 Site card / Activity / Distribution Studio 预留 adapter surface，但不提前实现业务页。

Acceptance：

- 没有设备时 GUI 是完整空状态而不是 terminal fallback；
- 关闭主窗不杀 Controller；显式 Exit 才停止；
- GUI 与 CLI 对同一个 Controller state 观察一致。

## 6. V1 — 第一条真实远程 Read

### V1.1 — Identity + Enrollment

**Status：TODO**

计划：

- ControllerKey / ControllerId；
- Host 生成并持久化 DeviceKey；
- signed `SiteBootstrapPass` / one-time capability；
- grant/policy intersection；
- enrollment claim concurrency、expiry、first-claim deployment window；
- “Controller 已登记、Host 落盘失败”的半提交恢复；
- Controller encrypted backup/restore skeleton 与 Recovery Review 数据模型。

Acceptance：

- bootstrap secret 永不成为长期身份；
- 每台设备最终有独立 DeviceKey；
- replay/expired/revoked claim fail closed；
- 无备份的新 ControllerId 不会被旧 Host 自动信任。

### V1.2 — Site Kit + Host Lifecycle + Naming

**Status：TODO**

计划：

- per-platform Site Kit contract；
- stable runtime + signed `site.clew` sidecar；
- sidecar 固定查找/恢复顺序；
- OS-user state store 与 membership reuse；
- Host 单实例；第二次启动只唤起已有窗口；
- hostname default name + 5-char DeviceTag collision handling；
- Windows/macOS Host window + tray/menu bar 基础状态；
- Linux `--foreground` 基础路径；
- Clew Original `UiResources/OutfitRuntimeView` 抽象，不做 Studio。

Acceptance：

- 程序与 `site.clew` 被拆开时出现固定人话恢复页；
- 同 machine/user/site 重开不产生第二个 DeviceId；
- 同 hostname 冲突显示稳定 `GPU-01-K7M4Q` 类名字而非序号；
- helper-only capability 不进入 executable selector。

### V1.3 — Direct iroh + InnerSession E2E

**Status：TODO**

计划：

- 使用当前 iroh stable API 建立 direct/relay outer transport；
- 明确 transport path 与 Clew business session 分层；
- 使用成熟 authenticated key exchange/Noise 类 construction 建立 `InnerSession`，不自创密码学；
- Target pin Controller identity，Controller 验证 DeviceKey；
- wire major/identity/transcript binding；
- inner framing、AEAD、replay/order/error handling；
- Read path/tool kind/payload 全部在 inner ciphertext 中。

Acceptance：

- direct 与公共 relay 路径都保持同一个业务安全模型；
- outer transport dump/log 不出现测试业务明文；
- wrong Controller / wrong Device / replay / corrupted frame 均 fail closed；
- path Relay↔Direct 变化不要求 Clew 自己迁移业务 stream。

### V1.4 — Bounded Read + V1 Control Plane

**Status：TODO**

计划：

- DeviceRegistry / Site projection；
- bounded `Read`：root policy、offset/limit、最大结果、timeout/cancel；
- Controller ActivityStore 最小实现；
- `device.rename` / `device.revoke` / `invite.close` 最小收口；
- Controller backup GUI/CLI 入口；
- Controller/Host 状态与错误人话；
- 两台真实机器 end-to-end acceptance。

Acceptance：

- `invite → 双击 → enrollment → InnerSession → bounded Read` 在两台真实机器上成功；
- 第二次双击不产生第二台设备；
- Controller 可停止该设备，revoke 后重连也不能恢复权限；
- Activity 能回答“刚才在哪台机器读了哪个路径”；
- backup/restore smoke + Recovery Review；
- Windows/macOS tray 与 Linux foreground 基础行为有对应平台 smoke。

完成这里后，才把 **V1** 标记为可对外试用。

## 7. V1.25 — Distribution Studio Foundation

**Status：TODO**

计划：

- `OutfitProfile` schema/revision；
- Clew Original / Research Lab / Friendly Minimal / Institution Clean presets；
- app name/icon/logo/color/string resources；
- PNG/SVG import + app/tray assets；
- live preview：主窗口 / helper / tray / Site Kit；
- `ClientFlavor` build/cache contract；
- `clew outfit` CLI + GUI library；
- `clew invite --outfit`。

Acceptance：从 preset 创建一套自定义 Outfit，用于真实 Site Kit；朋友侧连接动作数与 Clew Original 完全相同。

## 8. V1.5 — Zero-config Site Connector

**Status：TODO**

开始前必须完成一个明确的 iroh/LAN discovery implementation spike，确认当前版本的 local discovery/address-lookup API 和 opaque tunnel 承载方式；不得根据旧版本示例直接编码。

计划：

- SiteBootstrapPass bounded multi-claim；
- LAN/mDNS same-Site discovery；
- order-independent target/helper startup；
- sealed-to-Controller enrollment 经 Connector；
- **只承载已有 InnerSession ciphertext 的 opaque outer tunnel**；
- auto Connector promotion；
- multiple Connector health/failover；
- helper-only `EXECUTE=false`；
- mDNS 失败时 `附近连接.clew` 文件 fallback；
- suspend/resume 后 helper 重新广播。

硬门禁：

- helper 不获得 InnerSession key；
- helper 日志/协议 payload 中不得出现 Read path、Shell command、file bytes；
- 如果只能靠 helper 终止两条业务会话实现，本块视为失败，不提供明文降级模式。

Acceptance：完全无公网 Target 与一台联网 helper 各双击一次、顺序任意、无 IP/端口/code/route 输入，即可首次 enrollment 并完成 Read；关闭当前 helper 后可自动切换另一健康 helper。

## 9. V2 — Agent Minimum

**Status：TODO**

- Glob / Grep / Read / Edit / Write；
- Shell persistent task；
- MCP stdio + Streamable HTTP；
- `Devices` 带 site/name/hostname/executable/connector；
- selector 支持 DeviceId、`Site/Device`、唯一短名；
- helper-only 不可执行；
- bounded output、pagination、timeout、cancel；
- atomic write / edit preconditions。

Acceptance：coding agent 能在不依赖 GUI 手工选机的前提下稳定定位目标设备并完成最小文件/命令工作流。

## 10. V3 — Reliability

**Status：TODO**

- connection reconnect；
- request idempotency/replay matrix；
- Shell task reattach；
- File resume 接口预留；
- path telemetry；
- sleep/resume；
- wire/capability compatibility matrix。

## 11. V4 — Dynamic Networking

**Status：TODO**

- TCP forward；
- SOCKS5 TCP egress；
- HTTP CONNECT；
- listener lifecycle 归 Controller；
- disconnect/recovery 与明确的 TCP non-migration 语义。

## 12. V5 — File Plane

**Status：TODO**

- block/chunk manifest；
- hash/final verification；
- resume；
- directory transfer；
- bounded concurrency；
- progress/cancel/conflict policy。

## 13. V6 — Release Packaging

**Status：TODO**

- Windows signed portable artifact；
- macOS signing/notarization；
- Linux artifact；
- Client Generator / Distribution Studio release pipeline；
- Windows icon/VERSIONINFO branding；
- macOS bundle naming/icon/localization；
- ClientFlavor signing/notarization/cache；
- cross-platform packaging smoke。

Packaging smoke 应在早期按需进行；V6 是发布级收口，不是第一次碰 signing。

## 14. V7 — Advanced Service Runtime

**Status：TODO**

服务化是高级显式 opt-in，不改变 v1 portable/tray/foreground 默认路径。

实现顺序：

1. Linux `systemd --user`；默认登录后常驻，linger 作为显式更高级选项；
2. Windows Service：machine runtime + 用户态 tray/GUI Local API client；
3. Linux system service：专用低权限 service account/state。

硬边界：

- service lifecycle owner 改变，不改变 Site/DeviceKey/InnerSession 数据面；
- portable/user runtime 与 machine service 不静默共享 DeviceKey；
- machine service 默认优先 Connector-only；
- machine EXECUTE 必须显式设置 execution identity、filesystem roots、Shell policy；
- Windows Service 不把 GUI 放进 Session 0；
- install/enable/stop/uninstall 全部可见，不做隐蔽 persistence。

## 15. V8+ — 明确后置

在真实需求出现前不实现：

- Directory/federation；
- dedicated relay management；
- 第二 transport；
- Linux desktop tray；
- SOCKS5 UDP；
- 企业 IdP / 审计套件；
- remote desktop；
- portable identity on USB。

## 16. 每块统一 Definition of Done

除块内 acceptance 外，默认要求：

```text
cargo fmt -- --check
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

V0.1 已建立 workspace，因此从本块起 check/test 统一使用 workspace/all-targets。涉及平台、网络和 GUI 的 block 必须增加实际平台 smoke，不能用 unit test 代替全部验收。

提交前还要完成：

- `git diff --check` / staged diff review；
- workspace hygiene（无临时文件、缓存、secret-like artifact）；
- 检查是否意外扩大权限、日志明文、无界输出；
- 更新本文的状态、验证结果、known issues、下一块。

## 17. 当前 checkpoint

**Current block：V0.2 — Controller Single Instance + Local API（DONE）**

**Next block：V0.3 — Controller GUI Shell**

V0.2 已把 Controller 唯一 ownership、本机 IPC、第一批 read-only Local API 和 crash/stale-lock recovery 收口。下一块只建立 Controller GUI shell，并坚持 GUI 只消费 Local API；不提前把 iroh/session state 塞进 GUI。

### Change log

- **2026-09-01** — V0.2 DONE：新增 `clew-runtime`、跨平台 single-owner file lock、Windows named pipe/Unix UDS Local API、local auth + bounds、CLI client 化与真实 Windows 跨进程 crash-recovery smoke；下一块冻结为 V0.3。
- **2026-09-01** — V0.1 DONE：建立 `clew-core` / `clew-proto`、稳定 ID/DeviceTag/state layout/proto skeleton 与 20 个边界测试；下一块冻结为 V0.2。
- **2026-09-01** — 建立 Architecture v1.5 正式开发计划；登记 P0 基线维护；下一块冻结为 V0.1。
