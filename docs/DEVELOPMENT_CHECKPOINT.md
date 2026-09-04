# Clew Development Checkpoint

状态：**正式开发 / V2 Agent Minimum IN PROGRESS**
架构基线：**Architecture v1.5**
更新时间：**2026-09-03**

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
| V0.3 | Controller GUI shell | DONE | GUI 空列表/ready 状态来自 Local API，不直接持有网络状态 |
| V1.1 | ControllerKey / DeviceKey / enrollment | DONE | signed bootstrap、持久 DeviceKey、claim/半提交边界有测试 |
| V1.2 | Site Kit / Host lifecycle / naming | DONE | sidecar recovery、identity reuse、DeviceTag、Host 单实例与基础 UI |
| V1.3 | Direct iroh + InnerSession E2E | DONE | Controller/Target 双向认证，业务 payload 全部在 inner ciphertext |
| V1.4 | Bounded Read + v1 control plane | DONE | Windows/Linux/macOS release gate、live revoke、Read/Activity/backup 全闭合 |
| V1.25 | Distribution Studio foundation | DONE | preset → preview → branded Site Kit，不增加朋友步骤 |
| V1.5 | Zero-config Site Connector | DONE | 三物理机 no-public enrollment + stable Read，helper 看不到业务明文 |
| V2 | Agent minimum | DONE | selector + bounded filesystem + live-session Shell + MCP stdio/Streamable HTTP 全闭合 |
| V3 | Reliability | DONE | reconnect/replay/Shell reattach/sleep-resume/compatibility 已闭合 |
| V4 | Dynamic networking | DONE | Controller-owned TCP forward / SOCKS5 CONNECT / HTTP CONNECT + non-migration |
| V5 | File plane | IN PROGRESS | single-file put + cross-generation resume DONE；继续 get / durable restart / directory |
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

**Status：DONE**

**Date：2026-09-01**

实际落地：

- Windows/macOS desktop 构建使用 `eframe` + `tray-icon` 建立 Controller GUI shell；GUI 本身不持有 iroh/session/network state，只通过 `LocalApiClient` 读取 `controller.status` / `device.list`；
- `clew gui` 在 Controller 缺失时通过独立 `clew controller` 进程自动拉起唯一 owner，再连接同一 Local API；已有 Controller 时只连接，不创建平行 runtime；
- 主窗显示 ready/error、设备列表与完整“还没有合作者”空状态；邀请按钮明确保留为 V1，不在 V0.3 偷跑业务；
- 建立 tray 生命周期骨架：显示主窗/恢复焦点、隐藏到托盘、显式“退出 Clew”；窗口 `X` 通过 `CancelClose + Visible(false)` 只隐藏；
- Local API 新增 authenticated `controller.shutdown`：服务端先返回 ACK，再触发 runtime shutdown；CLI `clew shutdown` 与 GUI Exit 共用同一边界；
- shutdown watch 同时进入连接饱和/正常 accept 两条 server loop，避免 GUI Exit 在 16-connection hard cap 下失效；
- GUI Local API worker 独立线程运行 current-thread Tokio runtime，以 message passing 投影状态到 UI，不让 UI 线程成为 Controller owner。

Acceptance：

- [x] 没有设备时 GUI 是完整空状态而不是 terminal fallback；
- [x] 关闭主窗逻辑只隐藏；显式 Exit 通过 authenticated Local API 停止 Controller；
- [x] GUI 与 CLI 读取同一个 Local API `controller.status` / `device.list`；
- [x] Windows desktop 实际 smoke 能启动 GUI/tray、自动拉起 Controller 并查询 `ready=true`。

Validation evidence：

- `cargo check --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**30 tests passed**（root integration 2 + core 14 + proto 6 + runtime 8）；
- 新增回归：authenticated Local API shutdown、CLI shutdown → process exit → same state-dir restart；
- Windows GUI runtime smoke：`clew gui --state-dir <temp>` 成功启动独立 GUI process 与 Controller process，`clew status` 返回 `ready=true`；
- GUI 依赖在 cap00 Windows Rust 1.96 toolchain 实际 build/link 通过。

Known / deferred：

- 本块真实桌面 runtime smoke 是 Windows；当前 runner 没有 macOS target/desktop，因此 macOS `eframe + tray-icon` 仍需要后续真机补 smoke；
- 当前 tray icon 是代码生成的 Clew placeholder；正式 Clew Original/Outfit 资源属于 V1.2/V1.25；
- V0.3 不实现 invite/Site card/Activity/Studio 业务页，也不把 ControllerKey/remote session 塞进 GUI；这些继续按 checkpoint 推进；
- commit subject：`feat: add v0.3 controller gui shell`。

V0.3 至此允许进入 V1.1。

## 6. V1 — 第一条真实远程 Read

### V1.1 — Identity + Enrollment

**Status：DONE**

**Date：2026-09-01**

实际落地：

- 新增 `clew-identity` crate，长期 Controller/Device identity 使用 Ed25519；`ControllerId` 由 Controller public key 经 domain-separated SHA-256 fingerprint 派生并以 UUIDv8-style 形式表示，不再把随机 UUID 当密码学身份；
- Controller 在取得 V0.2 single-owner lock 后才 `load_or_create ControllerKey`，`controller.json` 同时持久化 private key 与 public pin；Local API `controller.status` 现投影稳定 `controller_id`；同 state dir graceful/crash restart 均保持 ControllerId 不变；
- Host DeviceKey 使用 OS-user `StateLayout` 下 `(ControllerId, SiteId)` scope 的 pending/active 两阶段 state；private key 与 public pin 同时持久化，加载时重算比对，损坏不会静默变成新身份；
- signed `SiteBootstrapPass` 将 bearer bootstrap secret 与长期身份彻底分离：signed payload 只保存 domain-separated secret hash、Controller public pin、Site/Invite、grant、有效期、first-claim deployment window 与 claim capacity；Debug 对 bearer secret/persist ACK token 全部 redact；
- `PermissionGrant` 统一做 requested grant ∩ Controller policy ceiling；helper-only ceiling 会强制清掉 Read/Write/Shell；
- `EnrollmentRegistry` 收口 not-before/expiry、first-claim deployment window、claim capacity、close/revoke、finalized replay 与 registered-pass conflict；one-time capacity 并发回归保证只允许一个 winner；
- enrollment 使用 `PendingHostPersist → Active` 两阶段：Controller 已登记但 Host 未完成落盘时，同一 DeviceKey + 同一 pass 可幂等取回同一 receipt，即使 bootstrap 后来过期；Active 后 replay fail closed；
- encrypted Controller backup skeleton 使用固定 Argon2id KDF 参数 + XChaCha20-Poly1305 AEAD；JSON 只包含 salt/nonce/ciphertext，不出现明文 ControllerKey；restore 要求 empty local state，并自动进入 `RecoveryReview { remote_access_paused=true }`、关闭历史 bootstrap；
- backup/state parse 前继续保留 16 MiB hard bounds；本地长期 secret 只落在 per-user state scope，不写 app/site-kit 目录。

Acceptance：

- [x] bootstrap bearer secret 永不成为 Controller/Device 长期身份；
- [x] 每个 enrollment 使用独立 DeviceKey，并在半提交恢复时复用原 key；
- [x] replay/expired/closed/revoked/conflicting claim fail closed；
- [x] 无备份重新生成 ControllerKey 得到不同 ControllerId，Host pin comparison 不会自动信任；
- [x] 本地 key state 损坏导致 private/public pin 不一致时 fail closed，而不是静默身份漂移；
- [x] backup restore 强制 empty state + Recovery Review，并关闭历史 invite claim surface。

Validation evidence：

- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**45 tests passed**（root integration 2 + core 14 + identity 15 + proto 6 + runtime 8）；
- identity 覆盖：Controller fingerprint/pin、fresh Controller mismatch、signed bootstrap tamper、grant intersection、expiry/close/revoke/replay、first-claim window、one-time concurrency；
- half-commit 回归：pending DeviceKey 持久化 → Controller claim → 模拟 Host 落盘前重启 → bootstrap 已过期仍取回同一 DeviceId/receipt → promote active/finalize；
- backup 覆盖：Argon2id + XChaCha20-Poly1305 roundtrip、wrong passphrase/tampered ciphertext auth fail、non-empty restore 拒绝、restore 后 old invite closed；
- Windows CLI integration 已验证 graceful/crash restart 的 `instance_id` 会变化而 `controller_id` 保持不变；
- `cargo check --workspace --all-targets --target x86_64-linux-android`：PASS，Unix cfg/state/crypto 路径 0 warnings。

Known / deferred：

- V1.1 只建立 identity/enrollment/backup **核心与 runtime identity persistence**；正式 invite GUI/CLI、Controller Registry 持久事务与 backup GUI/CLI 入口按 V1.2/V1.4 继续；
- Host 的 `site.clew` 查找、Host single-instance/window/tray、hostname collision rename 属于 V1.2；
- transport identity secret 已纳入 Controller state/backup skeleton，但不在 V1.1 启动网络；V1.3 才进入 iroh + InnerSession；
- commit subject：`feat: add v1.1 identity enrollment foundation`。

V1.1 至此允许进入 V1.2。

### V1.2 — Site Kit + Host Lifecycle + Naming

**Status：DONE**

**Date：2026-09-01**

实际落地：

- 新增 `clew-host` crate，建立 per-platform Site Kit contract：Windows `Clew.exe + site.clew`、macOS `Clew.app + site.clew`、Linux `Clew + site.clew`，并以 `ClientFlavor(runtime/platform/arch/outfit/revision)` fingerprint 防止 sidecar 被错误 runtime/outfit 消费；
- `site.clew` 使用 ControllerKey domain-separated Ed25519 signature，并嵌套 V1.1 signed bootstrap；读取前有 1 MiB hard bound、版本 header 预检、ClientFlavor fingerprint 与 Controller signature 双重校验；
- Host 固定查找顺序落地为：显式 `--site`/GUI 拖入或选择 → executable/app sibling `site.clew` → 当前 OS-user state 中唯一可恢复 membership → 固定 MissingInvite recovery UI；不扫描全盘、不猜 cwd；
- active membership 保存 Controller pin、Site/Invite、DeviceId、ClientFlavor 与 DeviceRecord，并与 V1.1 active DeviceKey/DeviceRecord 交叉校验；sidecar 丢失时可由唯一 membership 恢复，同 machine/user/site 重开复用原 DeviceId/DeviceKey；
- Host single-instance 使用 `(ControllerId, SiteId)` domain-separated instance key + 内核 file lock + local-only named pipe/UDS wake channel；第二次启动只向现有实例发 authenticated `wake`，不形成第二个长期 owner；
- Windows named pipe 明确 `reject_remote_clients(true)`，Unix runtime dir/socket/secret 使用 0700/0600；wake frame 4 KiB、I/O 2 秒、retry window 5 秒，避免本机 IPC 无界阻塞；
- 默认设备名取 hostname；generic hostname 使用平台人话 fallback。自动 hostname 发生碰撞时整个 collision group 切换到稳定 5-char Crockford DeviceTag；已 tagged 名称即使 peer 消失也保持 tagged，不退回裸 hostname，也不使用 `(2)`；显式 rename 不被自动 collision policy 覆盖；
- executable selector 支持 DeviceId、`SiteName/DeviceName`、唯一 short name；只在 online + executable 集合中匹配，helper-only 明确返回 `NotExecutable`，歧义返回候选而不是取第一个；
- 根 CLI 新增 `clew host --site ... [--foreground]`；Windows/macOS 使用可见 Host GUI + tray，关闭窗口只隐藏，第二次启动唤醒/聚焦已有窗口，显式“退出并断开”才结束；Linux 默认/`--foreground` 走可观察终端路径；
- 建立 `UiResources` / `OutfitRuntimeView::clew_original()`，MissingInvite/解压提示/Host 基础文案从资源层读取，为 V1.25 Studio 留接口但不提前实现 Studio。

Acceptance：

- [x] 程序与 `site.clew` 被拆开时进入固定 MissingInvite 人话恢复页；archive-temp 检测时额外提示“先全部解压”；
- [x] 同 machine/user/site 重开 pending/active identity 均复用，不产生第二个 DeviceId/DeviceKey；
- [x] 同 hostname collision 显示稳定 `GPU-01-XXXXX` 形式 5-char DeviceTag，不使用序号后缀；peer 消失后仍保持 tagged；
- [x] helper-only capability 不进入 executable selector；DeviceId 直选 helper 也 fail closed；
- [x] 第二 Host 在 primary ready 后只发送 authenticated wake，primary 退出后才可重新取得 owner；
- [x] Windows Host GUI/tray 实机 smoke 通过，关闭/第二启动生命周期与设计一致。

Validation evidence：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**60 tests passed**，另 1 个 Windows interactive smoke 默认 ignored；其中 `clew-host` 14 tests，root Host/Controller integration 3 tests；
- Host 覆盖：signed sidecar roundtrip/tamper、per-platform Site Kit、fixed lookup order、missing-invite copy、unique membership recovery、membership/DeviceId reuse、hostname collision/tag persistence/rename、helper-only selector、single-instance wake/reacquire；
- Windows/cap00 真实 GUI smoke：`windows_host_gui_and_tray_smoke` 使用 `--ignored --exact` 单独执行，**1/1 PASS**；Host window/tray 初始化后第二 `clew host` 返回 already running 并唤醒已有实例；
- `cargo check --workspace --all-targets --target x86_64-linux-android`：PASS，Unix Host foreground/UDS/state permission cfg 路径 **0 warnings**；
- integration 初版曾把 pending DeviceKey 当作 primary-ready 同步点，暴露并发 launch test race；已改为等待 Host runtime secret/lock ready，再验证 second-instance wake，focused regression PASS。

Known / deferred：

- V1.2 只完成 Site Kit/Host lifecycle 与本机 identity reuse；Host 还不会主动连接远端 Controller 或完成真实 network enrollment，网络与 InnerSession 属于 V1.3；
- macOS GUI/menu-bar 代码已按相同 desktop cfg 编译结构实现，但当前 cap00 没有 macOS 真机 runner，仍需后续平台 smoke；
- Windows/macOS 当前 Clew 图标仍是程序生成 placeholder；正式 branding/outfit asset pipeline 属于 V1.25/V6；
- Host GUI 的 invitation enrollment 状态目前停留在“signed invitation verified / waiting Controller”，直到 V1.3 data plane 接入；
- commit subject：`feat: add v1.2 host site kit lifecycle`。

V1.2 至此允许进入 V1.3。

### V1.3 — Direct iroh + InnerSession E2E

**Status：DONE**

**Date：2026-09-01**

实际落地：

- 新增 `clew-transport` crate；outer transport 使用 **iroh 1.1.0** 当前 API，默认 `presets::N0` 提供 direct/relay，另有 `N0DisableRelay` 作为 direct-only 测试面；ALPN 继续固定为 `clew/1`；
- `IrohOuter` 只负责 `EndpointAddr`、QUIC connection 与 bi-stream；Clew 的 tool kind/path/payload 不进入 outer 路由元数据，`IrohStream` 显式持有 `Connection`，避免 stream 仍在读写时 connection 被提前 drop；
- `InnerSession` 使用 **snow 0.10.0** 的 `Noise_XX_25519_ChaChaPoly_BLAKE2s`，不自定义 handshake/AEAD construction；Controller Noise static 使用 V1.1 已持久化且进入 encrypted backup 的独立随机 transport secret；Device Noise static 使用标准 **HKDF-SHA256 (hkdf 0.13)** 从随机 DeviceKey seed 做 domain-separated key separation，不直接复用 Ed25519 signing key；
- Noise XX 完成后，双方在 **Noise transport ciphertext 内**交换 Ed25519 identity proof；proof 签名绑定最终 Noise transcript hash + `WIRE_MAJOR` + role + `ControllerId/SiteId/DeviceId` + 完整 Controller/Device public identity，因此 Target pin Controller、Controller 验证 DeviceKey，且旧 session proof 不能搬到新 handshake；
- 新增 `ControllerSessionIdentity::from_stored` / `DeviceSessionIdentity::from_active`，直接消费 V1.1/V1.2 已持久化真实 identity state，不另造第二套凭据；
- business frame 仅在 InnerSession 内包含 `wire_major + sequence + kind + payload`；inner plaintext hard cap 60 KiB、Noise packet hard cap 65,535 bytes、identity proof cap 8 KiB，length 在 payload allocation 前检查；handshake framing 10 秒 timeout；
- replay/corruption/wrong-order/wrong-wire 均 fail closed；任何认证/协议失败后 session poison；进一步 hardening 后，post-handshake frame read/write I/O failure 也会 poison session，避免 Noise nonce 已推进后复用不同步 state；
- direct iroh integration 增加 `TapStream` 旁路捕获 outer stream 写出字节，验证 `\"kind\":\"read\"` 与 `C:/private/data.mrc` 都不会出现在 outer plaintext；代码本身不记录业务 payload。

Acceptance：

- [x] direct 与公共 relay 路径使用完全相同的 `InnerSession` / identity proof / business framing；
- [x] outer stream capture 不出现测试 Read tool kind/path 明文；
- [x] wrong Controller / wrong Device / replay / corrupted frame / post-handshake I/O failure 均 fail closed；
- [x] 公共 relay 地址建立连接后由 iroh 自己管理 Relay↔Direct path，Clew InnerSession 不迁移/重建业务 stream。

Validation evidence：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**68 tests passed**，另 2 个平台/网络 smoke 默认 ignored（Windows interactive Host GUI、public n0 relay）；其中 `clew-transport` 常规 **7 tests passed**，`clew-identity` 增至 16 tests；
- direct iroh QUIC + Noise + Ed25519 + encrypted `read/read_result` integration：PASS；同一 test 的 outer byte tap 明确断言 tool kind/path marker 不存在；
- public n0 relay smoke 在 cap00/Windows 当前网络下单独执行：`public_relay_dial_carries_inner_session_without_rekeying` **1/1 PASS（5.27s）**；只把 server relay address 交给 client，InnerSession 正常建立并往返业务帧；
- security regressions：wrong Controller pin、wrong DeviceKey、replay、ciphertext corruption、post-handshake write failure、EOF/read failure 均拒绝并在需要时 poison session；
- Android/Unix cross-target 尝试在进入 Clew 代码前被环境挡住：iroh 的 `ring`/`blake3` build script 需要 Android NDK `x86_64-linux-android-clang`，cap00 当前只有 Rust target 没有 NDK C toolchain；这不是编译诊断中的 Clew source error，不能作为 Unix runtime PASS 证据。

Known / deferred：

- V1.3 收口的是 transport/InnerSession 安全原语与真实 direct/public-relay network smoke；Controller/Host 的长期 remote task、enrollment over network 与真实 `Read` request dispatch 属于紧接的 V1.4 vertical slice；
- 当前实际网络 smoke 平台是 Windows/cap00；macOS/Linux 真机 transport runtime 证据仍缺失，Android cross-check 也受缺失 NDK C toolchain 阻塞；后续有对应 runner 时必须补，不把 Windows 结果冒充跨平台；
- iroh 的 relay/direct 选择和迁移属于 iroh outer transport；Clew 不把 path type 纳入 identity 权限，也不因 path 变化重建 InnerSession；
- commit subject：`feat: add v1.3 encrypted iroh transport`。

V1.3 至此允许进入 V1.4。

### V1.4 — Bounded Read + V1 Control Plane

**Status：DONE（V1 release gate closed；V1 可对外试用）**

**Date：2026-09-01；release-gate evidence update：2026-09-02**

实际落地：

- 新增 Controller-owned `ControllerControlStore`：双槽 generation 持久化 `EnrollmentRegistry + ControllerCatalog + bounded Activity + RecoveryReview`；任一已有 control slot 不可读时不会静默重置，较新槽损坏可回退到上一有效 generation；
- `ControllerCatalog` 建立 Site/Device projection，持久化 Site 名、`ReadPolicy`、Device capabilities、rename/revoke 投影；Local API/CLI 增加 `mint`、`invite-close`、`read`、`rename`、`revoke`、`activity` / `activity-clear`；GUI/CLI 继续只通过 Local API，不形成第二个 Controller state owner；
- `site.clew` 的 network variant 将**当前 online 后**的 iroh `EndpointAddr` 与 bounded `ReadPolicy` 一并签名；不缓存 Controller 启动瞬间的地址快照，避免 relay/address hint 因启动时序缺失；
- Host 将 V1.1 enrollment 正式接到 bootstrap ALPN：`Claim → Claimed → Host persist → Persisted → Activated → ActivatedAck`；Controller claim/ACK 事务与 Host DeviceKey/membership 持久化保持分离，半提交窗口使用 0600 `pending-controller-activation` ticket 恢复；matching token 的重复 `Persisted` 幂等完成 Pending→Active/Active；
- Host active membership 持久化 Controller endpoint + ReadPolicy；同 Controller/Site 重启继续复用 DeviceId/DeviceKey。V1.4 网络 Host 使用一个长期 iroh endpoint 做 InnerSession 重连，不再每次 reconnect 重新 bind endpoint；旧 V1.2 sidecar 若 endpoint/read policy **同时缺失**仍保留 AwaitingEnrollment 兼容状态，只有半配置才 fail closed；
- bounded `Read` 同时在 Controller 与 Host 两侧执行：Controller 先检查 device/site/revoke/capability/max-result，Host 再执行 canonical root policy、offset/limit、文件类型、结果硬上限与 timeout；root 外目标在 Host 侧拒绝，超过 Site 结果上限在 Controller 侧不下发；业务 request/result 始终走 V1.3 InnerSession；
- Controller Activity 只保存 bounded metadata 摘要（设备/Site、operation、path summary、result、duration、bytes），不保存文件内容/stdout/env；支持 retention/count hard bound 与显式 clear；
- bootstrap policy rejection 改为结构化且固定的人话错误；邀请关闭/过期/撤销/额度、Recovery Review 等不再靠 QUIC reset 表达，也不泄漏 bootstrap secret/token/内部 state。发送错误后 graceful shutdown send half，并给对端短窗口读取，真实 Gamma smoke 从 `connection lost` 收敛为 `Denied: This invitation is closed to new devices.`；Controller 日志只记录静态错误类别；
- Controller backup 从 V1.1 crypto primitive 升级为 v2 payload，纳入 Registry + Catalog；CLI `backup-export` 使用环境变量取口令，`backup-restore` 只允许 stopped + empty state，恢复失败回滚部分 state；恢复后历史 bootstrap 全关闭且 `remote_access_paused=true`，显式 Recovery Review 后才重新接受旧 DeviceKey；
- Controller GUI V1.4 adapter 增加 Activity、Recovery Review 与加密 backup export；口令使用 password field、两次确认且不进入 Debug/argv/log。restore 仍保持离线 CLI，因为运行中的 Controller GUI 不应绕过“empty stopped state”安全门；
- Host/Controller human state 与错误面已覆盖 network enrollment、closed invite、Recovery Review、Local API error；root CLI 仍保留 foreground 可观察路径。

Acceptance evidence：

- [x] **真实双机 Windows**：cap00 Controller ↔ Gamma Target 完成 `mint/site.clew → enrollment → InnerSession → bounded Read`；Gamma `D:\\tmp\\clew-v14-gamma\\share\\proof.txt` 返回精确 proof 内容；同一 Site root 外文件返回 `Denied`；50,000-byte 请求超过 49,152-byte Site cap 时 Controller 在下发前拒绝；
- [x] 第二次启动 Gamma Host 只唤醒现有 instance；进程重启后仍是同一 DeviceId/DeviceKey，显式 rename `Gamma-Renamed` 跨重启/backup restore 保留；`invite.close` 后已有设备继续 Read，新空 state enrollment 被拒绝且收到固定人话；
- [x] Activity 真实记录成功 Read 与 root-policy denial，能回答设备/路径/result/bytes；`activity-clear` 后真实返回空列表；
- [x] backup/restore + Recovery Review 真实 smoke：从含 Site/Device/Activity 的 Controller 导出加密 backup，恢复到空 state 后保持原 ControllerId/transport identity；确认前 Gamma 旧 DeviceKey `online=false`，`recovery-confirm` 后无需重启 Host 即恢复 `online=true`，随后再次真实 Read 成功；
- [x] Windows Controller GUI 最新 V1.4 build 在 cap00 实机启动、自动拉起唯一 Controller，连续多轮 Device/Activity/Recovery Local API refresh 无崩溃；V1.2 Windows Host GUI/tray 实机 smoke 仍有效；
- [x] **live device revoke 真运行闭环**：在 gyz Linux 临时 Controller/Host 分离 state/process 的真实 InnerSession 上执行 `clew revoke 97386e1a-…` 成功；设备立即投影为 `online=false / executable=false`，后续 Read 在 Controller 权限面返回 `Denied: read is not permitted`。原 Host 持续重连 5 秒仍离线；停止后使用完全相同的 host-state/DeviceKey 重启，再等待 5 秒仍只有原 DeviceId 且保持离线/不可执行，没有重新 enrollment 或产生第二台设备；
- [x] **Linux foreground 真机 smoke**：gyz.sustech（Linux 6.4 / x86_64）从当前 `4a66937` 的精确 `git archive`（217,603 bytes，SHA-256 `38fda1bd0837310f8804fbbb5f3059e54c9878e49e8a79f21985f4dc620e60d7`）native `cargo check --all-targets` / `cargo build --bin clew` 通过；Linux Controller + `host --foreground` 真实完成 mint/enrollment/InnerSession/bounded Read，第二 Host 只返回 `already running`，root 外 Read 固定 `Denied`；
- [x] **macOS tray 真机 smoke**：`macos-3dv0`（macOS 15.7.9 / x86_64，console user `inter`，Aqua `gui/501` domain 可用）在其 WebCodex `scratchpad` 项目内使用独立测试目录运行当前代码。首轮原生 workspace test 真实暴露 Host UDS `path must be shorter than SUN_LEN`；已修为 macOS Controller/Host Local API/wake socket 使用用户专属 `TMPDIR` + domain-separated state hash 的短稳定路径，lock/secret/state 仍留原 state dir。修复后 macOS `cargo test --workspace --all-targets` **93 passed / 0 failed / 1 ignored**；Controller GUI 与 Host GUI 均在 Aqua session 中长期运行且 tray/AppKit 初始化无错误，Host enrollment 后 `online=true / executable=true`，第二次 Host 启动只 wake 已有实例，真实 bounded Read 返回 `CLEW-V1.4-MACOS-TRAY-PROOF`。`System Events` 额外窗口查询因 macOS Automation/Accessibility 未授权而超时，未修改系统隐私权限，也不作为 PASS 必要条件。

Validation evidence：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**92 tests passed / 0 failed / 2 ignored**；ignored 为 interactive Windows Host GUI smoke 与 public n0 relay smoke，两者此前均有单独真实 PASS 证据；
- V1.4 真实 Windows Controller GUI runtime smoke：PASS；最新 GUI process 保持运行超过多个 refresh 周期，Local API `ready=true`，随后 Controller graceful shutdown、GUI job stop 与临时 state cleanup 完成；
- V1.4 Linux/gyz native runtime：PASS；Host DeviceId `97386e1a-…` online 后真实读取 `/home/shark/tmp/rabisu/scratch/clew-v14-linux-e2e/share/proof.txt` 返回 33 bytes；同 root 外 `outside.txt` 被 Host policy 拒绝，Activity 分别记录 `succeeded/33 bytes` 与 `denied/0 bytes`；第二 foreground Host 只 wake 现有实例；
- V1.4 Linux live revoke：PASS；revoke 后当前 session 被移除、catalog 立即不可执行，持续 reconnect 与同 host-state restart 均不能恢复旧 DeviceKey 权限。此前 Windows smoke 的安全拦截因此不再是产品 release blocker；
- V1.4 macOS native runtime：PASS；`macos-3dv0:scratchpad` 上原生 `cargo check --workspace --all-targets` PASS；最终修复版本 `cargo test --workspace --all-targets` **93 passed / 0 failed / 1 ignored**（public relay 默认 ignored）。Controller GUI 使用深层 scratchpad state 成功 `ready=true`，证明 Controller 短 UDS；Host GUI/tray 完成 enrollment、online、second-launch wake 与真实 Read，证明 Host 短 UDS + tray/runtime 链；
- macOS 首轮 test 暴露的 `SUN_LEN` 回归已转绿：focused `host_cli` **1/1 PASS**、`clew-host` macOS short-socket **1/1 PASS**、`clew-runtime` macOS short-socket **1/1 PASS**。
- 初次 final workspace test 曾暴露 legacy non-networked Site Kit 会在拿到 Host instance 后立刻 `MissingNetworkConfig` 退出；已改为“endpoint/read policy 同时缺失 → 保持 AwaitingEnrollment；只缺一项 → fail closed”，`host_cli` focused regression 与同一 final workspace test 均转绿；
- cap00/Gamma smoke 临时目录均确认删除。早期 mzd smoke 在发现并修复半提交/endpoint lifecycle 问题后，mzd WebCodex agent 掉线；后续两次 SSH cleanup 都在环境层 timeout，因此 `D:\\tmp\\clew-v14-smoke` 的最终清理状态**未能验证**，不计为 Clew correctness PASS。

Known / release blockers：

- V1.4 代码实现面、Windows 双机/GUI、Linux foreground/live revoke、macOS Aqua/tray acceptance 均已闭合；**V1 release gate closed，允许对外试用**；后续新平台/打包问题进入 V6 packaging gate，不回滚 V1.4 状态；
- V2 才引入通用 cancel token/agent tool plane；V1.4 Read 已有 bounded timeout 与任务取消传播，但不提前设计 V2 的跨工具 cancel protocol；
- commit subject：`feat: implement v1.4 bounded read control plane`。

**V1 release gate 已全部关闭；V1 现在可对外试用。下一块正式进入 V1.25。**

## 7. V1.25 — Distribution Studio Foundation

**Status：DONE（V1.25a / V1.25b-1 / V1.25b-2 全部封板；Windows + macOS desktop acceptance 已闭合）**

**Date：2026-09-02**

V1.25a 已落地：

- 新增 bounded/versioned `OutfitProfile`（schema v1、revision、256 KiB encoded hard bound）及 Clew Original / Research Lab / Friendly Minimal / Institution Clean 四套 built-in preset；identity/visual/string/distribution-copy 都有字段/长度/颜色/locale/resource-key validation，权限与执行策略不进入 Outfit schema；
- V1 desktop 固定运行文案默认切换为 English/ASCII，避免 macOS `eframe/default_fonts` 缺 CJK glyph 时显示方框；显式 `zh-CN` resource map 保留为 Outfit locale 资产，但非本地化 identity metadata（app/window/profile display name）默认 ASCII；
- Controller-owned `OutfitLibrary` 使用双槽 generation 持久化 custom profile/default/recent，built-in read-only；支持 create-from-preset、clone、set field、set default，损坏 newest slot 可回退，已有但不可读 state fail closed；
- Local API + `clew outfit list/show/new/clone/set/set-default` 已接通，GUI/CLI 仍不直接拥有第二份 Outfit state；
- `site.clew` 可选携带**受 Controller 签名保护的 OutfitProfile**；profile id/revision 必须与 `ClientFlavor` 匹配。Host 只允许签名 Outfit 替换 flavor 的 outfit id/revision，runtime version/platform/arch pin 仍必须与当前 binary 完全一致；
- membership marker 持久化完整 `ClientFlavor + OutfitProfile`，因此首次 enrollment 后即使移走 `site.clew`，generic Clew runtime 仍可恢复同一个 branded profile/DeviceKey；旧 V1 marker 通过 `serde(default)` 保持兼容；
- Host desktop window title/app name/status/button/tray labels 改为从 signed/persisted `OutfitRuntimeView` 解析，不再在 Host GUI 内固定 Clew Original；
- `clew mint` 保持兼容并增加 `clew invite` alias；`--outfit <id>` 选择显式 Outfit，不指定时使用 Controller OutfitLibrary default，实际签发会标记 recent。

V1.25a acceptance evidence：

- [x] Local API/CLI：隔离 Controller state 中四 built-in 正常列出；从 Research Lab 创建 `huang-lab`，修改 primary color 后 revision 1→2，设为 default/recent；Controller 重启后 revision/default/recent 全部保留；
- [x] 真实 custom Site Kit：`clew invite "Custom Outfit Smoke" --outfit huang-lab ...` 生成 signed `site.clew`，安全 projection 显示 flavor/profile 均为 `huang-lab` revision 1、window title `Research Connect`、locale `en-US`；
- [x] generic Windows Host 使用该 custom Site Kit 完成 enrollment/InnerSession，Controller 显示同一设备 `online=true / executable=true`，bounded Read 精确返回 `CLEW-V125-CUSTOM-OUTFIT-PROOF`；
- [x] 停止 Host 后不再提供 sidecar，只用同一 host-state 重启 generic binary：仍恢复同一 DeviceId、online/executable，并再次 Read 成功；朋友侧仍是打开同一 Site Kit/Host，不增加 route/code/IP/额外 enrollment 动作；
- [x] security regressions：signed Outfit 不能改变 runtime/platform/arch；tampered profile 验签失败；custom profile membership 无 sidecar 恢复与 wrong-runtime rejection 均有测试。

V1.25a validation：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**102 tests passed / 0 failed**；另有 Windows interactive Host GUI 与 public n0 relay 两项默认 ignored，均有既往独立 smoke 证据；
- custom Site Kit smoke 的 Controller/Host 进程与 `%TEMP%\\clew-v125-custom-site-smoke` 临时 state 已确认清理。

V1.25b-1 bounded asset distribution 已落地：

- Controller-owned content-addressed asset store：PNG/SVG import，单 asset 512 KiB、最多 128 个、总计 16 MiB；PNG 在 full decode 前先检查 dimensions（≤2048），SVG 使用 usvg/resvg parser、尺寸 ≤4096，并拒绝 DOCTYPE/ENTITY/script/foreignObject 与外部 href；asset id 固定为 canonical `sha256-<64 lowercase hex>`；
- Local API/CLI 新增 `outfit assets`、`outfit import-asset`、`outfit set-asset`；资产 bytes 不写 Outfit JSON，profile 仅引用 content hash；built-in Outfit 仍 read-only，custom set-asset 仅实际变化时 revision +1；
- `clew invite --outfit` 会先通过 Local API 拉取该 profile 实际引用的 imported assets，base64 解码后再次验证 content hash，并原子写入 sibling `outfit-assets/<asset_id>.<png|svg>`；assets 全成功后才写 `site.clew`，避免生成缺资产的已签名 sidecar；
- Host 在处理 signed Site Kit 时先验证 sibling asset 的 signed content hash/size，再使用 create-new temp + atomic rename 写入自身 state cache；并发双击争用时只接受已出现且 hash 正确的目标；tamper/missing/双格式 ambiguity 均在 enrollment 前 fail closed；
- membership 已持久 OutfitProfile，因此分发包移走后 custom brand 仍可定位 Host state cache；`OutfitProfile::build_cache_key()` 使用 domain-separated SHA-256 哈希 validated canonical profile，imported content id 已进入该 key，不依赖原始导入路径。

V1.25b-1 acceptance evidence：

- [x] 真实 CLI asset：168-byte SVG import 为 `sha256-26e592d...cf2c52`，`set-asset asset-lab logo` 使 revision 1→2，Controller restart 后 profile/reference 保留；
- [x] 真实 asset Site Kit：`invite --outfit asset-lab` 生成一个 sibling SVG；文件名 content id 与实际 SHA-256 完全相同，`site.clew` 最后落盘；
- [x] generic Host 使用该 kit 完成 enrollment/InnerSession，DeviceId `8425cded-...` online/executable，state 中缓存唯一 `v1/outfit-assets/<same sha>.svg`，bounded Read 返回 `CLEW-V125-ASSET-DISTRIBUTION-PROOF`；
- [x] 停止 Host 后删除整个 kit（site.clew + sibling assets），只保留 Host state 重启：仍恢复同一 DeviceId、online/executable，第二次 Read 成功；
- [x] focused security：正确 asset cache PASS；tampered bytes → `AssetHashMismatch`；missing asset → `MissingOutfitAsset`，均在 claim 前拒绝。

V1.25b-1 validation：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**109 tests passed / 0 failed**；另 2 项默认 ignored（interactive Windows Host GUI / public relay）；
- Runtime focused **24/24 PASS**，Host focused **26/26 PASS**；asset smoke Controller/Host 进程与 `%TEMP%\\clew-v125-asset-smoke` 已确认清理。

V1.25b-2 Studio GUI/live preview implementation 已落地：

- Controller GUI 新增默认展开的 Outfit Studio；library/recent/default、create-from-preset、clone、batch edit、set-default、asset import/slot assignment 全部只通过 Local API，GUI 仅持可丢弃 draft/texture cache，不直接读写 OutfitLibrary/asset state；
- `OutfitEditPatch` 把 identity/color/default-locale core copy/Site Kit copy 合成单个 Controller transaction：一次 Apply 最多 revision +1，内容不变不增 revision；built-in 仍 read-only；
- asset preview 继续由 Controller runtime 负责解析/render：PNG/SVG 复用同一安全 parser，max edge 256，输出 bounded RGBA，再经 Local API base64 返回；最大 256×256 RGBA response 仍在既有 1 MiB Local API frame 内；
- Studio live preview 已覆盖 Main window / Helper / Tray / Site Kit，unsaved draft 只投影到 preview clone；imported app/tray/logo/key-visual 使用 bounded thumbnail texture；
- Host desktop 不再只消费 Outfit 文案：从**已验证的 Host state asset cache**加载 imported app icon / tray icon / logo / key visual；app icon 进入 window icon/header，tray icon 真正进入系统 tray，primary color 进入窗口 accent；signed imported asset cache 缺失/损坏时 fail closed，不静默回退为 Clew Original。

V1.25b-2 Windows acceptance evidence：

- [x] 隔离 Controller state 创建 `studio-smoke`，同一 170-byte SVG content id `sha256-3937a6d6...0ee5eb` 分配到 app/tray/logo/key-visual，primary color `#C25435`，revision 1→6 并设 default；
- [x] Controller Studio GUI 在 custom default/recent profile 下默认展开并持续运行，stderr 为空；WebCodex runner 中途发生 `runner_instance_replaced` 后，旧 smoke 进程消失但 Outfit 双槽/asset/site state 保留，新 runner 重启 Controller/Studio 后 GUI 再次稳定运行，证明 Studio catalog/profile/asset preview 不依赖旧进程内存；
- [x] 同一 profile 生成真实 network Site Kit；Windows desktop Host 使用 imported window/tray visual 完成 enrollment，GUI 持续运行，second launch wake PASS；Controller 显示 DeviceId `91d7f530-...` `online=true / executable=true`，bounded Read 返回 `CLEW-V125-STUDIO-PROOF`；
- [x] Host state cache 唯一 SVG 的实际 SHA-256 与 signed content id 完全一致；停止 Host 后删除整个 Site Kit，再仅以 Host state 启动 desktop Host：仍恢复同一 DeviceId、branded visual event loop 持续运行，第二次 Read 同样成功；朋友侧没有增加 route/code/IP/额外 enrollment 动作。

V1.25b-2 implementation validation：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**114 tests passed / 0 failed**；另 2 项默认 ignored（interactive Windows Host GUI / public relay）；
- Runtime focused 28/28、root/Studio/CLI focused 5 passed + 1 Windows interactive default ignored；Windows branded Studio/Host smoke 进程与 `%TEMP%\\clew-v125-studio-smoke` 已确认清理；
V1.25b-2 macOS acceptance evidence：

- [x] 将 exact implementation commit `073acc0` 的 tracked source archive（SHA-256 `cad676b9ca6d62309d25766b70b0815cb2822fa60f89abb5601c8599a30cc514`）放入 `macos-3dv0:/Users/inter/Documents/Scratch/scratchpad/clew-v125-073acc0`；native `cargo fmt -- --check`、workspace check 均 PASS，workspace tests **115 passed / 0 failed / 1 ignored**（仅 public n0 relay）；
- [x] macOS Aqua Controller Studio 使用 persisted `studio-mac` custom default/recent profile（同一 imported SVG 绑定 app/tray/logo/key-visual、primary `#C25435`、revision 6）持续运行 12s，GUI stderr 0；
- [x] macOS desktop Host 使用该真实 Site Kit 完成 enrollment，imported window/tray visual 初始化后 event loop 持续运行 12s，stderr 0；second launch wake PASS；Controller 显示 DeviceId `6632fa52-82e8-4c8f-80cb-566d6f3303e5` online/executable，bounded Read 返回 `CLEW-V125-MACOS-STUDIO-PROOF`；Host cache 唯一 SVG 的 SHA-256 与 signed content id `sha256-ce5ed88b...18a49a` 完全一致；
- [x] 停止 Host、删除整个 Site Kit 后，只保留 membership + Host state asset cache 无 sidecar 重启：同一 DeviceId、branded Aqua event loop、online/executable 与第二次 Read 全 PASS；macOS smoke 进程/运行态已清理，scratchpad 仅保留 exact source 测试目录。

最终 V1.25 Acceptance：**PASS**。从 preset 创建一套自定义 Outfit（含可选 imported visual assets）用于真实 Site Kit；朋友侧连接动作数与 Clew Original 完全相同。

## 8. V1.5 — Zero-config Site Connector

**Status：DONE（2026-09-03；V1.5a–e + formal three-physical-machine no-public gate PASS）**

V1.5a implementation spike 已完成并冻结当前依赖/API事实：项目使用 `iroh 1.1.0`；mDNS AddressLookup 已从核心 crate 拆到 `iroh-mdns-address-lookup 0.5.0`。Clew 使用独立 Clew-only mDNS service，而**不**把 Site metadata 挂进 endpoint-global `AddressLookupServices` / `UserData`，避免 N0 preset 的 DNS/Pkarr publisher 把 LAN Site hint 带到公网。Clew 不按旧版 iroh discovery 示例编码。

V1.5a 已落地：

- 新增 `clew/connector/1` 独立 ALPN；现有 Controller runtime 在 V1.5b 接线前对该 ALPN **显式 fail closed**，不会把 Connector 误当普通 member 或开放半成品入口；
- LAN discovery 使用 `iroh-mdns-address-lookup 0.5.0`、自定义 service `clewv1` 和 `AddrFilter::ip_only()`，独立 publish 当前 iroh Endpoint 的 direct IP candidates；mDNS TXT `UserData` 仅携带 bounded/versioned `clew1;c;<site-tag>`，严格低于 iroh 245-byte hard bound，且不会进入 N0 DNS/Pkarr AddressLookup publisher；
- `SiteDiscoveryTag` 是 domain-separated SHA-256(`ControllerId`,`SiteId`) 的 16-byte/32-lowercase-hex 非秘密 equality tag；它**只用于候选过滤，不是授权证明**。missing/malformed/wrong-Site/self/未来未知 mDNS event 都忽略；`Expired` 只对先前 same-Site matched EndpointId 生效；
- 新增 Controller-signed `SignedConnectorLease`，独立签名 domain `clew/connector-lease/v1`，绑定 ControllerId / SiteId / helper DeviceId / helper iroh EndpointId / Connector role / issued+expires；最长 10 分钟、encoded ≤8 KiB；wrong Controller/Site/endpoint、tamper、过期/未生效/过长 lifetime 全 fail closed；
- Target→Helper 先交换 bounded outer control preface（version/site-tag/purpose，max 16 KiB）；Helper 返回 signed lease，Target 验证 Controller pin + Site + **实际发现的 helper EndpointId** + validity 后才允许开始 InnerSession；oversized control frame 在 payload allocation 前拒绝；
- Helper 数据面只有 `copy_bidirectional` 级 opaque pump 和双向 aggregate byte counts。真实三端 iroh 测试中，Target↔Controller 直接在该转发 stream 两端跑既有 Noise XX InnerSession/Read；Helper 捕获的全部转发 bytes 中不存在 `read`、秘密 Read path 或 result marker；
- helper 的 control preface/lease 只暴露非秘密 Site routing hint、purpose、签名 lease、sizes/timing；业务 kind/path/file bytes 仍只存在于 InnerSession ciphertext。

V1.5a acceptance evidence：

- [x] real mDNS：同机 listener + wrong-Site advertiser + same-Site helper；wrong-Site 被过滤，same-Site helper 被发现，并使用 mDNS 返回的 `EndpointAddr` 真正完成 Connector ALPN ping/pong；
- [x] real opaque tunnel：三套真实 iroh Endpoint（Target/Helper/Controller），Target 先验证 Controller-signed helper lease，再经 Helper opaque pump 建立原有 InnerSession；Controller→Target Read、Target→Controller result、encrypted ack 全闭环；Helper plaintext tap 对 tool kind / secret path / result marker 全阴性；
- [x] bounds/security：noncanonical Site tag、wrong control version、>16 KiB preface、wrong/tampered/expired lease 均 fail closed；mDNS 只发布 direct IP candidate；
- [x] transport focused：**21 passed / 0 failed / 1 ignored**（public n0 relay）；workspace `cargo check --workspace --all-targets` PASS，0 warnings；workspace tests **121 passed / 0 failed / 2 ignored**（interactive Windows Host GUI / public n0 relay）。

### V1.5b — Sealed enrollment + authenticated Helper runtime

**Status：DONE（2026-09-02）**

实际落地：

- 首次 enrollment 新增独立 `Noise_NK_25519_ChaChaPoly_BLAKE2s` sealed bootstrap；prologue 绑定 wire major / ControllerId / SiteId，bootstrap bearer、fresh DevicePublicKey、hostname 与后续 Persisted/Activated 全程只在 Controller-pinned ciphertext 内。Controller bootstrap static key 从 transport seed 以独立 HKDF info 派生，与 V1.3 InnerSession Noise key、iroh Endpoint key 三者互相 key-separated；签名 `site.clew` 携带对应 public pin，active membership 也持久化该非秘密 pin，sidecar 丢失后的 pending activation recovery 仍可经 Connector 完成；
- Controller `clew/connector/1` runtime 已启用：只解析 bounded outer `ConnectorOpen`，按 SiteDiscoveryTag 找到唯一未 revoke Site；`Bootstrap` 进入 sealed channel，`InnerSession` 仍交给原 V1.3 mutual-auth handler，不存在 helper 终止业务会话的兼容分支；
- `CONNECTOR` member 先用自己的 DeviceKey 完成普通 InnerSession。Controller 仅在 catalog/registry 实时确认 Site/device 未 revoke、DeviceId/Site 匹配且 `connector=true` 后，签发 5 分钟 `SignedConnectorLease`；lease 的 helper EndpointId 直接取该**已认证 QUIC connection 的 `remote_id()`**，不是信任 mDNS 声明；
- Host 只有收到并验证 Controller-signed lease（Controller/Site/自己的 DeviceId/自己的当前 iroh EndpointId/expiry）后才开始 mDNS advertise。lease 到期前 30 秒撤广播、结束 tunnel 并重连 Controller 换新 lease；单 helper 同时 tunnel hard cap 为 64；每条 tunnel 只读 outer preface/lease，之后纯 opaque `copy_bidirectional`，正常 `NotConnected/BrokenPipe/ConnectionReset/ConnectionAborted/UnexpectedEof` 只结束该 tunnel，不误杀 Helper runtime；
- 同一 Site Kit 的 signed pass 默认可授予 `EXECUTE + READ + CONNECTOR`，支持普通 Target 自动兼任 Connector；`BootstrapMemberMode` 只是**继续收窄**的 per-claim ceiling。`ConnectorOnly` 必然与 signed pass + Controller ceiling 再做 intersection，最终 `EXECUTE=false/read=false/write=false/shell=false/connector=true`，客户端不能靠 role hint 升权；
- Target 首次 activation 使用同一个 iroh endpoint 并行竞速 signed Controller endpoint 与 same-Site mDNS candidate。假/错误 helper 在 lease 验证前拿不到任何 bootstrap plaintext，candidate 失败不会消费 credential，也不会终止仍在进行的 direct attempt；verified helper 收到 `ConnectorReady` 后，Target 才启动 sealed Noise NK；
- `ActivatedAck` 的 sealed path 增加 `ActivationConfirmed`：Target 只有真正收到 Controller 确认后才清本机 pending-controller-activation。Target 随后发送无授权语义的 drain ACK，Controller 最多保活 2 秒 best-effort drain，避免 QUIC application-close 抢在确认帧交付前；direct V1.4 wire 行为不要求该终态，保持滚动兼容；
- mDNS privacy hardening：Clew Site tag 现在由 standalone `MdnsAddressLookup` 直接发布/订阅，不再调用 endpoint-global `set_user_data_for_address_lookup`，从实现上保证 Site equality hint 只进入 LAN mDNS TXT，而不是 N0 DNS/Pkarr。

V1.5b acceptance / validation evidence：

- [x] **forced Connector first enrollment**：真实三角色 iroh integration 中，signed `site.clew` 的 Controller EndpointId 被故意替换成不可达值；同 Site Helper 通过真实 mDNS 被发现，Target 经 production `serve_one_connector_tunnel` 完成 sealed `Claim → Claimed → Persisted → Activated → ActivatedAck/ActivationConfirmed`，最终 Active DeviceId 与 Controller record 一致；该 test **1/1 PASS**；
- [x] helper-only narrowing focused test **1/1 PASS**；同一 signed pass 在 `CONNECTOR_ONLY` ceiling 下不能获得 EXECUTE/Read/Write/Shell；
- [x] `clew-transport --all-targets`：**25 passed / 0 failed / 1 ignored**（public n0 relay）；其中 sealed wire tap 对 bearer/hostname/Site marker 全阴性、wrong pin/context/oversize fail closed、real mDNS 与 real opaque InnerSession tunnel 均 PASS；
- [x] `clew-host --all-targets`：**28 passed / 0 failed**；`clew-runtime --all-targets`：**28 passed / 0 failed**；
- [x] unified DoD：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **128 passed / 0 failed / 2 ignored**（interactive Windows Host GUI / public n0 relay）。

V1.5b known / next：

- desktop 首次 activation 目前仍发生在 GUI 展示前，单次 direct/helper discovery window 为 20 秒。`Target 先开、Helper 晚开` 的真正顺序无关体验不能靠无限阻塞 GUI；V1.5c-2 要把 AwaitingEnrollment 重试搬到 GUI/runtime 存续期间的 background state machine；
- multiple-helper health/failover、mDNS 失败时 `附近连接.clew` fallback、suspend/resume re-advertise 仍未闭合；helper-only protocol/role ceiling 已有，但最终分发包里的 friend-facing helper-only role hint 入口仍需后续 UX 接线。

### V1.5c-1 — Active no-public InnerSession + Read through Connector

**Status：DONE（2026-09-02）**

实际落地：

- 已 enrollment member 不再先等待自身 iroh relay-online。`serve_networked_membership_until/once` bind endpoint 后直接进入 path race：signed Controller endpoint direct dial 与 same-Site mDNS candidate 并行；20 秒窗口内 verified Helper 可在 direct 不可达时获胜，长期 `until` loop 会按原 1 秒节奏继续重试；
- active Helper candidate 与 bootstrap 使用同一信任顺序：Target 连接 `clew/connector/1` → 发送 bounded `ConnectorOpen{purpose=InnerSession}` → 读取 `ConnectorReady` → 验证 Controller signature、Site、**实际 candidate EndpointId** 与 expiry；只有通过后才在同一 stream 上运行原 V1.3 `DeviceSessionIdentity` Noise XX。Helper 继续只搬 InnerSession ciphertext；
- Controller 对 direct member 和 Connector-carried member 显式区分：只有 direct outer connection 才允许以 `stream.connection().remote_id()` 签发“该设备自身”的 Connector lease。经 Helper 的 Target 不再把 Helper EndpointId 错当自己的 endpoint，也不会在没有独立 Controller uplink 时误广播自身为 Connector；
- 普通新 Target 默认已有 `EXECUTE+CONNECTOR` capability：direct 时能拿自己的 lease 并自动 promotion；经 Helper 时仍可 Read/执行，但本次 connection 不获得自身 lease。helper-only 同理只有在自己 direct 连 Controller 时才广播；
- forced Connector integration 扩展为两阶段：signed Controller EndpointId 始终故意不可达。第一阶段仍经 mDNS Helper 完成 sealed enrollment；第二阶段同一 active membership/DeviceKey 再次经 verified Helper 建 InnerSession，Controller 发送真实 bounded Read，Target `HostReadService` 返回精确 `CLEW-V15C-NO-PUBLIC-CONNECTOR-READ`。

V1.5c-1 acceptance / validation evidence：

- [x] forced-unreachable-direct enrollment + active Read real iroh/mDNS integration **1/1 PASS**；没有可用 direct endpoint，首次 bootstrap 与后续 DeviceKey InnerSession/Read 均只能走 Helper；
- [x] `clew-host --all-targets` **28 passed / 0 failed**；`clew-runtime --all-targets` **28 passed / 0 failed**；
- [x] `cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **128 passed / 0 failed / 2 ignored**（interactive Windows Host GUI / public n0 relay）。

V1.5c-1 known / next：

- active path 已具备重新进入 discovery race 的基础，但 multiple-helper explicit health ranking、Helper-A 断线→Helper-B 自动切换的真实 acceptance 仍需 V1.5d；
- suspend/resume re-advertise 仍后续。

### V1.5c-2 — Order-independent visible Host runtime

**Status：DONE（2026-09-02）**

实际落地：

- desktop 不再在创建 GUI 前同步等待一次 20 秒 enrollment。Host 窗口立即显示 `AwaitingEnrollment`，同一 instance 的后台 network lifecycle 持续竞速 direct + verified Connector path；Helper 晚出现时无需退出/重启 Target；
- background activation 使用可取消 `watch` state machine。GUI 退出、切换 Site、选择 membership 都会先停止旧 network task；即使当前 discovery window 为 30 秒，shutdown focused test 约 50 ms 后发出取消并在 1 秒 gate 内结束，不留下网络 timeout 尾巴；
- activation 成功后通过本地 mpsc state channel 原地把 GUI 从 Awaiting 更新为 Active，并在同一后台 lifecycle 继续长期 member/Connector runtime；second-launch wake/tray 语义保持不变；Linux/`--foreground` 同样持续 retry，`Ctrl-C` 明确取消；
- Controller 自动 lease issuance 已由真实 authenticated Connector member test 覆盖：Helper 先完成 DeviceKey InnerSession，Controller 才按 catalog/registry 实时状态与 QUIC `remote_id()` 签 endpoint-bound lease；
- 顺序 acceptance 真实把 Helper 延迟 8.5 秒、单次 discovery window 固定 8 秒，因此第一轮必失败；Target 第二轮自动恢复并完成 sealed enrollment + active Read，证明不是“Helper 恰好已在线”的测试。

Acceptance / validation evidence：

- [x] delayed Helper order gate（cap00/Windows multicast）**1/1 PASS**；
- [x] activation cancellation focused **1/1 PASS**；authenticated Connector lease focused **1/1 PASS**；
- [x] Windows interactive Host GUI/tray smoke **1/1 PASS**；
- [x] 当前 workspace `cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；workspace tests **134 passed / 0 failed / 4 ignored**（interactive Windows GUI、2 个显式 multicast integration、public n0 relay）；两条 multicast integration 在 cap00 又分别显式运行并 **1/1 PASS**。

### V1.5c-3 — Nearby Connector file fallback

**Status：DONE（2026-09-03）**

实际落地：

- canonical friend-facing 文件名为英文 `nearby-connection.clew`，避免 macOS 上缺字字体造成方框；读取层继续兼容设计文档旧名 `附近连接.clew`。文件只在明确 Site Kit sibling、显式 drag/drop 或 per-Site imported state 中读取，**不**扩大 cwd/全盘扫描；
- v1 文件格式 hard bound 32 KiB、最多 16 个 direct-IP hints，绑定 SiteDiscoveryTag、Helper iroh EndpointId 与 Controller-signed Connector lease；relay/custom transport hint、wrong Site/endpoint、tamper、oversize 全 fail closed；文件读取使用 `File::open + take(MAX+1)`，在 JSON decode/可增长分配前执行 hard bound，避免 metadata→read 的 TOCTOU；
- 文件里的短期 lease 只作为 Controller-signed **historical route binding**。因此文件可跨 lease expiry/重启长期保留；真正每次拨 Helper 仍必须现场读取 `ConnectorReady` 并按当前时间验证 fresh Controller-signed lease。旧/revoked Helper 最多形成失败 candidate，无法获得 sealed bootstrap 或 InnerSession 权限；
- per-Site state 将 `import` 与本机 Helper `export` 分槽，防止 Target 把别人给的 candidate 当作自己可分发的 Helper hint；Helper 只有 direct authenticated uplink 获得自身 fresh lease 后才刷新 export；
- bootstrap 与 active member 两条 path race 现在都是 direct + mDNS + verified imported nearby candidate。坏/过期 route 文件不会阻塞 direct/mDNS；candidate 真正使用前仍执行现有 fresh lease gate；
- GUI Awaiting 页面提示可拖入 `nearby-connection.clew`，导入成功后后台 retry 自动接管；Active connector-capable Host 提供 `Save Nearby Connection File...`；固定 sibling 文件也会在 `site.clew` resolve 时自动验证并导入；
- no-mDNS real integration 完全不启动 mDNS advertiser、并故意使用不可达 signed Controller endpoint；Target 只靠 imported nearby candidate 完成 sealed enrollment，随后刷新 route file 后再次经 same Helper 完成 active DeviceKey InnerSession/Read，**1/1 PASS**。
- cap00 validation：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **134 passed / 0 failed / 4 ignored**（interactive Windows GUI、2 个显式 multicast integration、public n0 relay）；两条 multicast integration 又分别显式 **1/1 PASS**，Windows interactive Host GUI/tray **1/1 PASS**。

平台事实 / gate：

- `macos-3dv0`（QEMU）上 exact `eaacd5e` 的 full no-public mDNS test 失败；继续缩小到 `clew-transport::real_mdns_discovers_only_same_site_and_connects` 后同样在 10 秒内收不到同机 advertiser。由此冻结为该 VM/network 的 multicast/mDNS 不可用事实，不再靠调大 timeout 掩盖；
- 这正是 nearby fallback 的目标环境。exact implementation commit `c4fbae6` 的 tracked source archive（SHA-256 `6356f533c82427fdef8454d4d09b691e0186ef750af4303534777596e9a85dc4`）已放入 `macos-3dv0:/Users/inter/Documents/Scratch/scratchpad/clew-v15c3-c4fbae6`；在该已知 mDNS 不可用环境中，no-mDNS sealed enrollment + active DeviceKey InnerSession/Read **1/1 PASS（3.33s）**；native `cargo fmt -- --check`、workspace check 均 PASS，workspace tests **135 passed / 0 failed / 3 ignored**（Host mDNS、transport mDNS、public n0 relay）；
- [x] **macOS Aqua Host/tray smoke**：`gui/501` domain 可用；exact `c4fbae6` standalone binary 在隔离 `aqua-smoke-state` 中持续运行超过 15 秒且 stderr 为空，second launch 179 ms 返回 `already running; requested the existing window to show`，证明本轮新增 Nearby store/drag-drop/export UI 未破坏 AppKit/tray/single-instance wake；随后 smoke job/state 已清理；
- V1.5d 继续 multiple-helper health/failover、Helper-A→Helper-B 自动切换与 suspend/resume re-advertise。

### V1.5d-1 — Bounded concurrent Helper selection

**Status：DONE（2026-09-03）**

实际落地：

- Target 不再在 mDNS `Candidate` 分支里串行 `await` 单个 Helper。bootstrap 与 active member 两条 path race 统一使用 bounded candidate queue + `JoinSet`：最多 **4 个并发 verified dials**、单个 candidate 最长 **12 秒**、单 path window 最多接受 **32 个不同 EndpointId**；fallback 与 mDNS candidate 共用 EndpointId 去重；
- direct Controller dial 继续作为独立 future 并行存在，坏/慢 Helper 不再阻塞 direct；多个 Helper 中**第一个完成 outer connect + fresh Controller-signed lease gate（bootstrap 还包括 sealed Noise 建链）**的候选获胜，实际等价于按可达性/验证完成速度选择健康路径；
- candidate task 超时/普通连接错误只淘汰该候选并继续队列；task panic/join failure 才上升为 runtime error。path window 到期时 JoinSet 随 future drop 自动取消剩余 speculative dials，不留下后台连接；
- 默认无 mDNS real-iroh regression 明确把 stalled Helper 放队首、healthy Helper 放第二，并要求 healthy 在发送 lease 前确认 stalled 已经收到 `ConnectorOpen`；两条 dial 真并发时 **1/1 PASS（1.46s）**，串行实现会卡死在第一条；candidate queue 32 上限 + EndpointId 去重 focused **1/1 PASS**；
- cap00 explicit real-mDNS regression 让 bad Helper 先广播，只有它收到 `ConnectorOpen` 后 healthy Helper 才开始广播；Target 在 **5.00s** 内切到 healthy EndpointId，**1/1 PASS**，证明主 discovery loop 在 stalled dial 期间仍处理后来出现的 Helper。

Validation：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **136 passed / 0 failed / 5 ignored**（interactive Windows GUI、3 个显式 multicast integration、public n0 relay）；real-mDNS stalled-first acceptance 另显式 1/1 PASS。

下一块 V1.5d-2：在已经建立并实际承载 InnerSession/Read 的 Helper-A 断开后，让长期 Host runtime 自动回到 path race，选择 Helper-B 并恢复同一 DeviceKey session；Target 仍只绑定 Site，不持久绑定 HelperId。

### V1.5d-2 — Active Helper-A → Helper-B session failover

**Status：DONE（2026-09-03）**

实际验收：

- 没有新增第二套“session migration”协议。长期 `serve_networked_membership_until_with_layout` 在当前 outer/InnerSession 断开后按既有可取消 retry 语义重新进入 Site path race；V1.5d-1 的并发候选选择让这个重建天然支持多个 Helper；
- 同一个 active membership/DeviceKey 先经 Helper-A 建立原 V1.3 InnerSession，Controller 从 A 的 outer EndpointId 收到连接并发送真实 bounded Read-A；A 随后真正关闭 endpoint/撤 discovery，Target session 断开后自动重试；Helper-B 再上线后，Controller 第二次从 B 的 outer EndpointId 收到连接并发送 Read-B；
- Controller 两次均只接受**相同 DeviceId + 相同 DevicePublicIdentity**，Target 没有重新 enrollment、没有新 DeviceKey、没有持久绑定 HelperId；两次业务协议都还是 Target↔Controller InnerSession，Helper 只转发 opaque ciphertext；
- 默认 no-multicast acceptance 用 per-Site `nearby-connection.clew` route 从 A 原子替换为 B，完整 failover **1/1 PASS（2.41s）**；cap00 real-mDNS acceptance 撤掉 A 广播并关闭 A，再发布 B，完整 failover **1/1 PASS（3.48s）**。

Validation：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **137 passed / 0 failed / 6 ignored**（interactive Windows GUI、4 个显式 multicast integration、public n0 relay）。

下一块 V1.5d-3：刷新独立 LAN presence，使 helper 在系统 resume / 接口变化后用当前 `outer.addr()` 重建 mDNS advertisement 和 export hint；不伪造“睡眠”状态，没有 OS suspend event 时 UI 仍只显示 offline/reconnecting。

### V1.5d-3 — Resume / interface-change LAN presence refresh

**Status：DONE（2026-09-03）**

实际落地：

- Connector direct session 拿到 Controller-signed lease 后，除了原有 lease-renewal deadline，再维护 **30 秒 LAN presence refresh**；refresh 不关闭当前 Controller InnerSession、不终止正在转发的 tunnels，也不制造新的业务协议；
- Clew 自己的 `MdnsConnectorDiscovery` 新增 current-address republish：每次从当前 `outer.addr()` 重新提取 direct-IP hints 并保留 same-Site tag。publisher 与创建时的 iroh EndpointId 强绑定；若拿另一个 EndpointId 刷旧 publisher，直接 `EndpointIdentityChanged` fail closed，避免把旧 lease/presence 错绑到新 transport identity；
- 同一个 tick 同步刷新 per-Site nearby export，因此睡眠恢复、Wi-Fi/接口切换后用户新保存的 fallback 会拿当前 direct hints。export 失败仍是 best-effort，不反向杀死已认证 Controller session；真正 Helper 使用前仍必须验证 fresh lease；
- 不伪造 OS 电源语义：Clew 没收到真实 suspend event 时 GUI 仍只说 offline/reconnecting。若睡眠导致 Controller connection 断开，V1.5c 的长期 retry 重建 session；若连接仍存活但 LAN 地址变化，presence timer republish 当前 hints；通用 task/session resume 仍保留给 V3 reliability；
- 自动 timer integration 使用真实 direct Controller↔Helper InnerSession，只签发一次 lease并保持 session 不动：初始 export 生成后测试手工删除它，100 ms test tick 自动重建并通过 Controller/Site/EndpointId/device 验证，**1/1 PASS（0.53s）**；transport EndpointId binding focused **1/1 PASS（0.16s）**；cap00 real-mDNS integration 在显式 republish 后仍完成 same-Site discovery + Connector ping/pong，**1/1 PASS（1.13s）**。

Validation：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **139 passed / 0 failed / 6 ignored**（interactive Windows GUI、4 个显式 multicast integration、public n0 relay）。

下一步：V1.5 final gap audit，确认 helper-only friend-facing 分发入口、最终跨平台/两机证据与文档一致后再决定整体封板；不把 V2/V3 的 agent tools/task-resume 工作提前塞回 V1.5。

### V1.5e — Real Controller GUI invite + same-Site-Kit helper-only entry

**Status：DONE（2026-09-03）**

实际落地：

- final gap audit 发现两个真实产品缺口并一并关闭：Controller 主窗的 `Invite collaborator (V1)` 仍是 disabled 占位；helper-only 虽已有 protocol/permission ceiling，但用户没有从同一 Site Kit 进入该模式的产品入口；
- Controller GUI 现在通过既有 authenticated Local API 真正执行 `invite_issue`，不在 GUI 内复制签名逻辑。邀请表单不问 Gateway/Connector，只收 Site 名和“**合作者电脑上的 absolute read root**”；后者明确不是 Controller 本机 folder picker，避免把本机路径误签成远端 policy；默认 Outfit 与 bounded invite/read 参数沿既有安全面复用；
- CLI `mint` 与 GUI 共用新 `invite_io`：先从 Controller 取回 Outfit assets，核对 asset id/长度/content hash，原子写到 kit sibling，再落 signed `site.clew`。显式隔离 Controller smoke 真正走 Local API 签出 **7990-byte** `site.clew`，serde role 为 `execute_preferred`，证明 GUI 默认邀请仍是 site-capable artifact而不是预判 Gateway；随后 authenticated shutdown 与 smoke state/output 清理均完成；
- 新 `HostLaunchMode::{Default, ConnectorOnly}` 只存在于**尚未 enrollment** 的启动 intent；已有 membership 以持久化 capability 为准，第二次从另一个入口打开不能改写既有授权；组合规则是 signed `HostRoleHint::ConnectorOnly` **或**本次 `--connector-only` 任一成立就只发送 `BootstrapMemberMode::ConnectorOnly`，因此入口只能降权、不能升级；
- 同一 signed `ExecutePreferred` Site Kit 的真实 no-public sealed enrollment 新增 helper-only acceptance：Controller 的 production `claim_with_ceiling` 最终得到 `execute=false/read=false/write=false/shell=false/connector=true`，Active membership 也 `executable=false`，**1/1 PASS（2.84s）**；monotonic downgrade 与 pending-only state 另有 focused regression；
- `SiteKitContract` 冻结同 runtime / 同 `site.clew` 的双入口 argv：普通入口 `host`，helper 入口 `host --connector-only`，Windows/macOS/Linux 一致。**这里冻结的是 V1.5 产品语义 contract；真正 Windows shortcut/macOS launcher/Linux packaging、codesign/notarization 仍属于 V6，不提前宣称已生成正式双 launcher artifact。**
- Host GUI/foreground 对 helper-only 使用 Outfit 的 helper-ready string，并固定明确“这台电脑只帮助附近电脑连接；不会开放文件或命令；Target↔Controller 内容仍端到端保护”；普通 EXECUTE+CONNECTOR 自动 promotion 不要求朋友切换模式；
- Controller GUI disabled invite 占位已移除，真实表单/后台 command 已接通；Windows interactive GUI job 持续 >15 秒 stderr 为空，且 GUI 自动拉起的隔离 Controller `status` 返回 `ready=true` + remote EndpointId。Host interactive GUI/tray regression **1/1 PASS**。

Windows/cap00 validation：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **143 passed / 0 failed / 6 ignored**；`host --help` helper entry **1/1 PASS**；same-Site-Kit helper-only real enrollment **1/1 PASS**；真实 invite output smoke PASS；Windows Host GUI/tray **1/1 PASS**；Controller GUI/auto-Controller smoke PASS。

macOS exact-commit acceptance：implementation commit `84b99c7` 的 tracked archive SHA-256 为 `9aa11f25cac6f509778ce3bbb1691c1ec4dad4acc80b87e7e36ed2cbb1f8bd09`，落在 `macos-3dv0:/Users/inter/Documents/Scratch/scratchpad/clew-v15e-84b99c7`；native `cargo fmt -- --check` 与 workspace check PASS，workspace tests **144 passed / 0 failed / 5 ignored**；same-Site-Kit helper-only sealed enrollment **1/1 PASS（4.69s）**，CLI entry **1/1 PASS（1.85s）**。

Aqua product gate：exact binary 的 Controller GUI 持续 >15 秒 stderr 为空并自动拉起 `ready=true` Controller；该 Controller 又在 **442 ms** 内真实签出 macOS 当前 ClientFlavor 的 `site.clew`。同一普通 `execute_preferred` sidecar 以 `host --connector-only` 启动后，Host Aqua GUI 持续 >15 秒 stderr 为空，Controller `devices` 明确返回 `online=true / executable=false / connector=true`；second launch **112 ms** 返回 `already running; requested the existing window to show`。Host/Controller job 与 smoke state/kit 均已清理。

V1.5 final multi-machine acceptance 已推进到真实三机产品路径；2026-09-03 的连续物理诊断又把 blocker 从“LAN 拓扑”与“Windows GPO/Defender”两种过早归因中进一步收窄：**当前测试网络对普通终端之间的 peer UDP/QUIC direct ingress 存在上游 ACL / isolation，无法作为稳定的 physical Connector acceptance 拓扑**。最终 physical Site Kit 双 launcher/signing 继续留在 V6。

2026-09-03 三机 acceptance 与后续物理诊断：

- 原测试 HEAD `7d5fed9`、Windows binary SHA-256 `d3a373ed2a186bfd3ca0dafcd1db0c5610d697e4c022056cee74b1a224cbe971`：cap00 = isolated Controller，imini = Helper，mzd = Target；三机使用同一精确 binary 与同一真实 Controller-signed `site.clew`；Helper 返回 `online=true / executable=false / connector=true`，Target 返回 `online=true / executable=true / connector=true`，Controller 真实 bounded Read 精确读回 `CLEW-V15-FINAL-MULTIMACHINE`。该轮 mzd 自己建立过 direct Controller uplink，因此只证明真实机器/产品入口/权限/Read，不冒充 no-public Connector gate；
- imini 的 `10.100.9.235/22` / `10.100.10.199/22` 与 mzd `10.100.11.93/22` 实际具备 L2/L3 与 TCP management reachability：mzd 能解析真实 MAC，两个地址 TCP/22 都直接 CONNECTED。故“二者不在 nearby LAN”初判已撤回；
- 随后又撤回“主要是 Defender/GPO”这一过强归因：即使在 imini 临时关闭实际 Public firewall，任意 peer TCP 端口仍不能进入；在 gamma 上更做了 `try/finally` 的 Private firewall 完全关闭 A/B，mzd→gamma UDP 仍 timeout，且 firewall 已恢复。因此阻断发生在 host firewall 之外；
- 为寻找稳定物理 Helper，先后筛了 gamma `10.100.9.77/22`、witand0 `10.100.10.10/22`、cap00 `10.100.9.59/22`。gamma 曾有一次真实 UDP probe 收到 mzd `10.100.11.93` 的 `CLEW-GAMMA-UDP`，但后续复验包括“gamma Private firewall 完全关闭”均 timeout；witand0 与 cap00 的同子网 UDP probe 也 timeout。当前网络环境的 peer UDP 可达性因此不稳定/被上游 ACL 隔离，不能拿来封 formal physical gate；
- 先前用 `RemoteAddress=Internet` 对 Target 测试 binary 做 outbound block 的构造也不再被当作“必然保留 LAN”的证明。后续 formal no-public gate 必须先独立证明 Target→Helper LAN UDP 真可达，再使用一个**经实测不会切断该 Helper path**的公网隔离方法；不能仅凭 Windows `Internet` keyword 假设地址分类。

这轮物理测试同时暴露并修复了一个真实的 V1.5 Connector bug：**fresh Controller-signed Connector lease 对 Helper 本机时钟零容忍**。

- cap00 Controller 与 gamma Helper 的 UTC 实测相差约 **6 秒**；gamma 已成功 enrollment、Controller 显示 `connector=true`，但 active direct session 每次收到 fresh lease 后都重复失败 `connector lease is not yet valid`，所以永远无法进入 Connector serving / 写出 Nearby export；
- 根因是 `SignedConnectorLease::verify_for_candidate` 原先要求 `now_unix_ms >= issued_unix_ms`，而 Controller 每次重试都按自己的当前时间重新签一张 lease；落后约 6 秒的 Helper 因而形成稳定死循环；
- 修复增加 `MAX_CONNECTOR_LEASE_NOT_BEFORE_SKEW_MS = 30s`，**只**给 `issued_unix_ms` / not-before 检查一个 bounded skew：`now + 30s < issued` 才判 `NotYetValid`。expiration 仍按 `now >= expires` 严格拒绝；Controller signature、Site、EndpointId、DeviceId 与 10 分钟最大 lifetime 边界完全不放宽；
- focused regression 冻结边界：恰好落后 30s PASS，落后 30s+1ms 仍 `NotYetValid`，到达 `expires` 仍 `Expired`；
- 命名收紧前但语义完全相同的真实 candidate binary SHA-256 `0ae4d3a54964dbd44b87d8f598bf3adc5d609aa448ce93b0b0f88d012055b6c4` 在 gamma 上复验：同一 Controller / 同一 membership 下约 **4 秒**生成 fresh production `nearby-connector.export.json`；最终一次 export 为 1984 bytes，EndpointId `11f4ca730dad6e5801c0384ace1785348795be6f049a968b7eb8763176453549`，包含真实 LAN hint `10.100.9.77:63514`。这证明 clock-skew 修复直接消除了 Helper promotion/export 死循环；
- mzd 随后正确导入该 canonical `nearby-connection.clew`，但因当前 mzd→gamma peer UDP 已被上游网络阻断而仍停在 pending；这条失败不再归因于 Clew lease/fallback/Helper promotion。
- macOS 已知 mDNS-blocked 环境中的 no-mDNS enrollment+active Read 证据仍由 V1.5c-3 exact `c4fbae6` 保持 PASS。

本轮代码 gate：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **143 passed / 0 failed / 6 ignored**。所有 mzd/gamma/witand0/cap00 临时 `Clew-V15-*` firewall rule、测试目录与 cap00 isolated state/job 均已清理；gamma Private firewall 已明确恢复 `Enabled=True`。

2026-09-03 formal three-physical-machine no-public acceptance（exact HEAD `7f01cb8`）最终闭合：

- exact tracked source archive SHA-256 `6f4d5af752f2eeef60f763505b3dbfe760e16457ed7a588be00aec41b012cbdb`（1,372,160 bytes）；在 `3dv0` 原生构建一次 Linux x86_64 binary，随后只分发同一 ELF 到三机，`delta` / `dnk` / `3dv0` 三边 SHA-256 均为 `eb464aedc2946abe9fafac8d4bb326c9b29e048ca959f85cb2ace5b00ecbb4b6`；
- 三个真实物理角色固定为 `delta = Controller`、`dnk = Helper`、`3dv0 = Target`。delta 使用该 exact Linux binary 启动 Controller（ControllerId `06e6d4b0-4883-8d87-b097-1bcfe37f6058`），并签出 Linux `V15-LINUX-PHYSICAL` Site Kit；三机使用同一 `site.clew`，SHA-256 `58fb5df9062566ca0c50fb120b32ed003f083577f57e64b67674c4ca82061e9b`（7904 bytes）；
- dnk 用同一普通 Site Kit 的 production `host --connector-only` enrollment，Controller 明确返回 `online=true / executable=false / connector=true`；production Nearby export 真正落盘并包含 dnk 私网 direct hints `10.100.8.141` / `172.26.54.44`。同时显式等到 30 秒 renewal margin，lease 从旧的 5-minute interval 自动刷新到新的 authenticated 5-minute interval，证明 long-lived Helper renewal 也在真实机器上工作；
- Target 不修改整机 firewall。3dv0 使用 rootless `pasta` 创建**仅该 Clew Target 进程**可见的独立 user+network namespace，并在 namespace 内用 nft output policy 只允许 loopback，以及 UDP→dnk 两个 production Helper private IP；其余 IPv4/IPv6/TCP/UDP 全部 `reject`。`1.1.1.1:443` public TCP probe 在 Host 启动前明确失败；宿主 WebCodex/SSH 不受影响。该规则也不允许 dnk 的 public hint、Controller delta 或任何 relay/public endpoint，因此不是“假装 no-public”；
- **negative control**：同一 namespace policy 下移走 `nearby-connection.clew`，fresh Target state 运行 26 秒只生成 pending DeviceKey，不产生 membership/device record，Controller catalog 仍只有 dnk Helper；nft 记录 **433 packets / 71,860 bytes rejected**。这证明 Target 自己不存在可偷偷使用的 Controller/public path；
- **positive control**：恢复 Helper 的 fresh production canonical `nearby-connection.clew`（Target-side SHA-256 `dcf8db01a3bb43b72fcd5cc14c30d840bcfd0279f63f87d5f92eec0de38d31b8`），其余 namespace policy 完全不变，fresh Target 在约 8 秒内完成 sealed first enrollment，DeviceId `b28cdf65-0c09-4a6c-9d9f-721f4dc458fd`，Controller 返回 `online=true / executable=true / connector=true`；随后真实 bounded Read 精确返回 `CLEW-V15-LINUX-PHYSICAL-NOPUBLIC`；
- active stability 不是瞬时成功：约 51 秒内每 10 秒采样一次，共 **6/6 `online=true` + 6/6 bounded Read PASS**；positive run 约 125 秒后主动终止，最终 nft 记录仍有 **951 packets / 88,334 bytes rejected**，Host stdout 只经历 `waiting for Controller enrollment → host ready`。因此首次 bootstrap 与长期 DeviceKey InnerSession/Read 均是在 public path 被实际阻断时经 Helper 闭合；
- 第一轮 positive smoke 曾出现“刚 online 随后 offline / Read unavailable”，最终确认不是产品 bug：当时 dnk Helper 的 WebCodex test job 恰好到达 300 秒硬 timeout 被测试框架终止。把 Controller/Helper 都改为 1800 秒 job 后，同一正式 binary/协议立即稳定通过上述 6/6 Read；没有为该假故障修改源码；
- 另做纯 mDNS 物理诊断：移走 Nearby file，并在同一 no-public namespace 额外只放行 `224.0.0.251:5353` / `ff02::fb:5353`，65 秒仍 pending。该结果只说明当前 campus/`pasta` physical path 不提供可用 multicast，不被伪称为 mDNS 产品回归；mDNS 零输入、delayed-order 与 Helper failover 继续由既有 real-mDNS integrations 覆盖，mDNS-blocked physical 环境则由 production Nearby fallback 覆盖。

Final closeout validation：`cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **143 passed / 0 failed / 6 ignored**。3dv0/dnk/delta 的 `.clew-v15-7f01cb8` acceptance 目录全部删除，dnk Helper job 已停止，delta Controller 通过 authenticated `shutdown` 正常退出，cap00 的 source archive/Linux binary/Site/Nearby 临时 artifact 也已删除。

因此 V1.5 不再有协议或 formal no-public blocker。Windows installer 后续仍应明确处理/说明 Helper UDP inbound permission，但属于 V6 packaging ergonomics，不再阻塞 V1.5。

计划：

- SiteBootstrapPass bounded multi-claim；
- LAN/mDNS same-Site discovery；
- order-independent target/helper startup；
- sealed-to-Controller enrollment 经 Connector；
- **只承载已有 InnerSession ciphertext 的 opaque outer tunnel**；
- auto Connector promotion；
- multiple Connector health/failover；
- helper-only `EXECUTE=false`；
- mDNS 失败时 `nearby-connection.clew` 文件 fallback（legacy `附近连接.clew` 仍可读）；
- suspend/resume 后 helper 重新广播。

硬门禁：

- helper 不获得 InnerSession key；
- helper 日志/协议 payload 中不得出现 Read path、Shell command、file bytes；
- 如果只能靠 helper 终止两条业务会话实现，本块视为失败，不提供明文降级模式。

Acceptance：**PASS（组合证据，路径不混淆）**。mDNS 可用网络中，Target/Helper 使用同一 Site Kit、顺序任意且无 IP/端口/code/route 输入即可首次 enrollment + active Read；该零输入路径、delayed-order 与 Helper-A→B failover 已由 real-mDNS integrations 覆盖。mDNS 不可用/受限网络中，使用 bounded Controller-signed `nearby-connection.clew` fallback，真实三物理机 no-public gate 已完成首次 enrollment + 稳定 active Read；两种路径最终都必须现场验证 fresh Controller-signed Helper lease，Helper 都只转发既有 InnerSession ciphertext。

## 9. V2 — Agent Minimum

**Status：DONE（2026-09-03）**

### V2a — Shared device selection + selector-aware bounded Read

**Status：DONE（2026-09-03）**

- 把原本位于 `clew-host`、但只依赖 `DeviceSummary/DeviceId` 的 target selector 提升到 `clew-core`，作为 CLI 与后续 MCP 共享的 authoritative adapter 语义；`clew-host` 保留兼容 re-export，不复制规则；
- 选择规则与设计冻结一致：稳定 DeviceId、`Site/Device` qualified name、在线 executable 集合内唯一短名；省略 selector 时仅在恰好一个在线 executable device 时自动选择；重名返回完整候选，不静默取第一台；helper-only 显式选择返回 `NotExecutable`，永远不成为默认执行候选；
- `Devices` 继续直接来自 Controller Local API 的 `DeviceSummary`，已包含 `site_name/display_name/hostname_observed/online/executable/connector/last_seen`，不新增第二份 agent catalog；
- `clew read` 保持旧的双 positional 兼容：`read <DeviceId> <path>` 仍有效，同时第一项现在可以是 `Site/Device` 或唯一短名；只给一个 operand 时把它解释为 path，并通过同一 selector 规则自动选择唯一在线 executable device；
- selector 只在 CLI/MCP adapter 层把人类/agent selector 解析成 DeviceId；Local Controller API 与远端 `ReadRequest` 仍只接受稳定 DeviceId，因此没有把名称歧义或 GUI 状态带进 wire protocol；真正 Read 继续复用 V1.4 已有 policy/root/byte-limit/timeout/InnerSession 路径；
- focused regression：core selector **3/3 PASS**（helper-only、qualified/unique/ambiguous、omitted selector）；CLI adapter **1/1 PASS**，证明单 operand 会进入唯一设备自动选择语义、双 operand 会进入 named selector 语义；`cargo fmt -- --check` 与 `cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **145 passed / 0 failed / 6 ignored**。

### V2b-1 — Bounded PathInfo + Glob

**Status：DONE（2026-09-03）**

- 新增共享 `FsQueryRequest/FsQueryReply` InnerSession RPC，当前只承载 `PathInfo` 与 `Glob`；仍走原 V1.3 Target↔Controller authenticated InnerSession，Connector 只转发 ciphertext，没有新增 helper-visible 文件协议或旁路 transport；
- `PathInfo` 复用 V1.4 `ReadPolicy` 的 canonical allowed-root 判定，返回 bounded UTF-8 canonical path、`File/Directory/Symlink/Other`、size 与可选 modified time；越界 root、缺失目标、I/O 与 timeout 都是结构化 fail-closed error；
- `Glob` 要求一个已授权 absolute root + bounded relative pattern；支持 path segment、`**`、segment 内 `*` / `?`。Host 使用 deterministic breadth-first scan、每目录稳定排序、最多 **100,000 scanned entries**、最多 **1,024 result items**、pattern ≤ **1,024 bytes**，并同时受 Site `max_result_bytes` 与请求 `max_bytes` 约束；cursor 是“已消费匹配数”，truncated page 返回 next cursor；
- 递归目录不能靠 junction/reparse/symlink 逃出 requested root：每个候选子目录再次 canonicalize，只有仍 `starts_with(requested_root)` 且 canonical path 未访问过才入队；symlink directory 不递归，canonical directory visited set 阻止环路；
- Host 生成 reply 前按实际 JSON payload 逐项检查 `max_bytes`；transport 层还独立拒绝超过 `HARD_MAX_READ_RESULT_BYTES = 48 KiB` 的 `fs_query_result`；Controller Local API 再次检查实际 Glob reply 不超过本次请求的 `max_bytes`，因此 compromised/buggy Host 也不能绕过 agent output budget；
- Controller Local API 新增 DeviceId-only `RemotePathInfoRequest` / `RemoteGlobRequest`，继续在 Controller catalog 复核 Site/device revoke、execute capability、ReadPolicy 与 timeout，并记录 `path_info` / `glob` Activity；CLI `path-info` / `glob` 只在 adapter 层使用 V2a shared selector，wire 里仍没有人类名称；
- focused validation：protocol roundtrip/hard-bound **1/1 PASS**；Host root/pagination/byte-bound **1/1 PASS**；真实 no-public Connector regression 在同一 InnerSession 中连续 `Read → PathInfo → Glob` **1/1 PASS（3.48s）**；CLI shared-selector surface **1/1 PASS**；`cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **148 passed / 0 failed / 6 ignored**。

### V2b-2 — Bounded streaming Grep

**Status：DONE（2026-09-03）**

- `FsQueryRequest/FsQueryReply` 在 V2b-1 同一 RPC 上增加 `Grep`，没有新增第二套文件协议；Local API 仍只吃 DeviceId，CLI `grep` 继续复用 V2a shared selector，Connector 仍只转发 InnerSession ciphertext；
- regex 使用 `regex::bytes` 线性时间引擎，pattern 继续受 **1,024-byte** hard bound，并显式把 regex compiler NFA/DFA size limit 固定为 **1 MiB / 2 MiB**，不依赖 crate 默认内存预算；invalid regex 结构化 `InvalidRequest`；
- 内容扫描完全流式：`BufReader::fill_buf/consume` 自实现 bounded line reader，不使用 `read_to_string/read_until` 等可能对单行/整文件无界扩容的 API；单行 hard cap **16 KiB**，单请求 content scan hard cap **32 MiB**，调用方还可进一步降低 `max_scan_bytes`；超 line/scan budget 分别 fail closed 为 `ContentLimit` / `ScanLimit`，不把部分结果冒充完整结果；
- 目录模式复用 Glob 的 deterministic BFS、每目录稳定排序、最多 **100,000 entries**、canonical-root containment + canonical visited set；symlink file 不扫描、symlink/junction/reparse directory 不递归逃出 root；也支持直接 grep 一个 authorized regular file；可选 `--include` 复用 bounded relative glob；
- cursor 与 Glob 一致，是“已消费匹配数”；page item 上限 **1,024**。返回前逐项按真实 JSON payload 检查请求 `max_bytes`，transport 再封 **48 KiB** hard reply cap，Controller Local API 再复核实际 Grep reply 不超过请求 budget；匹配到无法安全返回的非 UTF-8 line 时 fail closed，不做 lossy 文本伪造；
- Controller 继续复核 Site/device revoke、execute capability、ReadPolicy、timeout，并记录 `operation=grep` Activity；真实 Windows product smoke 用 isolated Controller + 普通 Host 运行 `clew grep <root> 'TODO|FIXME' --include '**/*.rs'`，自动选择唯一在线 executable device，精确返回 2 个 `.rs` 匹配、排除 `.txt`；Activity 记录 `grep / succeeded / transferred_bytes=320 / duration_ms=6`；smoke Controller authenticated shutdown、Host job 与 state 均已清理；
- focused validation：protocol Grep request/reply/bounds **1/1 PASS**；Host streaming pagination + regex/scan/line hard caps **1/1 PASS**；真实 no-public Connector regression 在同一 InnerSession 中连续 `Read → PathInfo → Glob → Grep` **1/1 PASS（3.33s）**；CLI selector surface **1/1 PASS**；`cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **150 passed / 0 failed / 6 ignored**。

V2 的 read-only filesystem surface（Devices/selector + Read/PathInfo/Glob/Grep）至此封板。下一块 V2c：Edit/Write，要求 explicit preconditions + atomic replace + hard write/result bounds；不提前引入 V3 generic task/replay/resume。

### V2c-0 — Signed write authority projection

**Status：DONE（2026-09-03）**

- 先修长期权限投影再写 mutation RPC：`PermissionGrant.write` 原本已存在于签名 bootstrap / enrollment receipt / Controller enrollment registry，但 Host membership marker 只持久化 member + ReadPolicy；直接实现 Write 会把长期 Host runtime 错误退化成“只要 EXECUTE 就能写”；
- 新增 `PermissionGrant::EXECUTE_READ_WRITE_CONNECTOR` 作为 execute-preferred **ceiling**：允许 read/write/connector、仍明确 `shell=false`。Controller bootstrap 继续做 `signed_grant ∩ ceiling`，所以旧/默认 Site Kit 的 `write=false` 不会被 ceiling 提权，helper-only 仍与 `CONNECTOR_ONLY` 相交；
- owner CLI `mint/invite` 新增显式 `--allow-write`；`InviteIssueRequest` 增加 serde-default `allow_write=false`。默认 CLI/Controller GUI 仍签 `EXECUTE_READ_CONNECTOR`，GUI 明确传 `false`，因此现有产品流程不会静默获得写权限；只有 owner 明确 opt-in 才签 `EXECUTE_READ_WRITE_CONNECTOR`；
- `HostMembershipMarker` 新增可选 `effective_grant`，新 enrollment 直接持久化 Controller receipt 的完整 effective grant；sidecar 丢失后从 signed Site Kit 恢复 active membership 时按 persisted member mode 重算同一 ceiling intersection；旧 marker 缺字段时 serde 为 `None`，后续 Write/Shell 必须把 `None` 当 deny，因此 legacy device 不会因升级获得新工具权限；
- Controller 长期运行态不新增第二份 site write flag：per-device authoritative tooling grant 继续由 enrollment registry 的 `effective_grant` 持久化，后续 mutation Local API 必须同时检查 registry grant、device/site revoke、execute 与 signed roots；
- focused regression：grant ceiling/intersection **2/2 PASS**，证明 write ceiling 不会给 unsigned read-only grant 增加 write、且始终剥离 shell；Host membership **1/1 PASS**，证明新 marker 保存 receipt grant、legacy JSON 删除该字段后解析为 `None`；`cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **151 passed / 0 failed / 6 ignored**。

下一块 V2c-1：实现 bounded text Write/Edit mutation RPC。Write 只允许 `create-only` 或 `expected SHA-256 replace`，Edit 必须 `expected SHA-256` 且 old text 恰好一次；同目录 secure temp + sync + atomic persist，Host/Controller 双端都验证 write authority/root/bounds，不引入 V5 大文件或 V3 replay/task。

### V2c-1 — Bounded atomic text Write / Edit

**Status：DONE（2026-09-03）**

- 新增独立但同一 InnerSession 内的 `fs_mutation` business RPC；Helper/Connector 仍只搬运 InnerSession ciphertext，没有新增旁路文件通道。`Write` / `Edit` Local API 继续只接 DeviceId，CLI adapter 复用 V2a selector；
- mutation 只定位为小文本 agent editing，不冒充 V5 transfer plane：Write UTF-8 content hard cap **32 KiB**；Edit `old/new` 各 **16 KiB**，编辑后文件仍必须 ≤ **32 KiB**；路径继续受既有 signed root/path hard bounds；SHA-256 precondition 必须是 **64-hex**；
- `Write` 强制二选一 precondition：`CreateOnly` 使用 same-directory secure temp + sync + `persist_noclobber`，已有目标绝不覆盖；`MatchSha256` 仅在当前 bounded file SHA-256 匹配时 replace。`Edit` 必须 expected SHA-256，且 `old` 在 UTF-8 文件中**恰好出现一次**；0 次或多次都返回 Conflict；
- Host 不用 `fs::read` 读取 mutation target：先 `symlink_metadata` / regular-file check，再 canonical-root containment，随后 `File::open + take(32KiB+1)` bounded read；create-only 先 canonicalize parent 并要求仍在 signed roots，最终组件必须是 normal filename；symlink target 直接拒绝；
- 落盘使用同目录 `NamedTempFile`，写入后 flush + `sync_all`，replace/edit 保留原权限，再 `persist` 到目标；Unix 额外 sync parent directory。这里的 **atomic** 指落地 replace 不暴露半写文件；expected SHA-256 是 mutation 前的 optimistic stale-check，不宣称跨进程 OS-level CAS；
- write authority 三层 fail closed：Controller Local API 查 enrollment registry `effective_grant.write` + site/device revoke + execute + signed roots；Host dispatcher 查 persisted membership grant；Host service 还要求显式 `allow_write=true`。legacy marker 的 grant=`None` 永远不能 mutation；同时 Read/FsQuery 的 Controller/Host 路径也补齐 `effective_grant.read` 双端检查；
- 默认 invitation 继续 read-only；真实 Windows product smoke 中 `mint --allow-write` Host 完成 Write(create-only) → Edit(expected returned SHA) → Read，最终精确得到 `alpha NEW omega`，Activity 仅记录 `write/edit`、path/result/15 bytes，不记录正文；另一个未带 `--allow-write` 的 `V2C-READONLY` Site 对同一 Write 明确返回 `Denied: filesystem mutation is not permitted`，目标文件不存在；所有 smoke jobs/state 已清理；
- focused validation：transport mutation roundtrip/bounds **1/1 PASS**；Host authority/root/create/conflict/edit **1/1 PASS**；真实 no-public NearbyFile Connector 在同一 InnerSession 中连续 `Read → PathInfo → Glob → Grep → Write → Edit → Read verify` **1/1 PASS（3.45s）**；同一 write-capable Site Kit 的 `--connector-only` ceiling 降权 **1/1 PASS（2.99s）**；CLI selector/precondition surface **1/1 PASS**；`cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **154 passed / 0 failed / 6 ignored**。

V2 filesystem agent surface（Devices/selector + Read/PathInfo/Glob/Grep/Write/Edit）至此封板。下一块 V2d：live-session bounded Shell task；只做明确 command/cwd/env policy、bounded stdout/stderr、timeout/cancel 与同一会话内 task lifecycle，V3 才增加 reconnect 后 reattach/replay。

### V2d-0 — Signed Shell authority + Mint parser hardening

**Status：DONE（2026-09-03）**

- `PermissionGrant.shell` 原本已进入 signed bootstrap / enrollment registry / Host membership，但 execute-preferred claim ceiling 仍固定 `shell=false`；V2d-0 新增 `EXECUTE_READ_WRITE_SHELL_CONNECTOR` 作为**ceiling only**，继续做 `signed grant ∩ ceiling`，所以 ceiling 自身不能给默认/旧 Site Kit 增加 Shell；`CONNECTOR_ONLY` 继续把 read/write/shell 全部剥离；
- `InviteIssueRequest` 新增 serde-default `allow_shell=false`；owner CLI `mint/invite` 新增显式 `--allow-shell`。Controller GUI 仍明确传 `allow_write=false / allow_shell=false`，朋友侧 invite 流程不新增问题、不默认开放命令执行；write 与 shell 可以独立 opt-in；
- 真实 Windows mint smoke 用同一 isolated Controller 生成两份 kit：默认 kit grant=`execute+connector/read`、`write=false/shell=false`；仅加 `--allow-shell` 后 grant=`execute+connector/read/shell`、`write=false`。因此 Shell authority 是签名 owner opt-in，不从 capability ceiling 或 GUI 默认获得；smoke Controller authenticated shutdown，临时 state/kit/share 全部清理；
- 产品 smoke 同时发现并修复一个真实 Windows CLI blocker：在原有字段很多的 `Command::Mint { ... }` 再增加参数后，Clap derive 的单个巨大 enum variant 会让 Windows main-thread parser stack 越界，甚至 `clew mint --help` 直接 stack overflow。将参数等价提取为独立 `#[derive(Args)] MintArgs`，保持 CLI surface 完全兼容，同时缩短 derive/builder stack depth；修复后 `mint --help` 与真实两次 mint 均 PASS；没有通过增大 linker/thread stack 掩盖问题；
- focused validation：grant intersection **2/2 PASS**，证明 full execute ceiling 不增加 unsigned write/shell，helper-only 仍剥离全部执行工具；Windows `mint --help` PASS；默认/shell-opt-in 真实 signed kit grant 对照 PASS；`cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **154 passed / 0 failed / 6 ignored**。

下一块 V2d-1：实现 live-session Shell task core。Host 持有 process/task，Controller 持有 projection；`shell.start/status/attach/cancel` 都是短 InnerSession RPC，stdout/stderr 有 bounded retained buffer + cursor，timeout/cancel 必须终止 child。V2 不承诺 connection loss 后 reattach；该恢复语义仍在 V3。

### V2d-1a — TaskId + bounded Shell wire contract

**Status：DONE（2026-09-03）**

- `TaskId` 提升为 `clew-core` stable UUID strong type，与 DeviceId/SiteId 同样拒绝 nil/错误长度；不使用进程 PID 或 Local API connection identity 充当长期 task identity；
- 新增 `shell_task` InnerSession business RPC，冻结 `Start / Status / Attach / Cancel` 四个短请求与 `Started / Status / Output / CancelAccepted / Error` 回复；Host process lifetime 不绑定某一次 CLI/MCP Local API 请求；
- Start hard bounds：command **8 KiB**，cwd 复用 signed root path **2 KiB** hard bound，env 最多 **32** 项、key **128 B**、value **2 KiB**、总 **16 KiB**，timeout **1..=30 min**；env key 只接受 `[A-Za-z_][A-Za-z0-9_]*`，NUL 全拒绝；
- output contract：stdout/stderr 各自的 retained ring hard cap预留为 **32 KiB**；Attach 每路最多 **12 KiB raw bytes**，Base64 后两路同时仍明显低于 **60 KiB** InnerSession plaintext；cursor 使用绝对 byte offset，response 同时携带 requested/start/next/retained base+next，若请求 cursor 已被 ring 淘汰必须显式 `lost_prefix=true`，不能静默伪装连续输出；
- phase 冻结为 `Running / Exited / TimedOut / Cancelled / Failed`，V2 只有 live-session task semantics；wire 里没有 reconnect generation/replay token/reattach proof，避免提前实现 V3；
- focused validation：Shell protocol roundtrip/bounds/Base64/cursor **1/1 PASS**；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **155 passed / 0 failed / 6 ignored**。

下一块 V2d-1b：Host-owned task/process store。必须 `env_clear` 后只投影最小 host baseline + bounded explicit env；初始 cwd 必须 canonicalize 到 signed roots；stdout/stderr reader 必须持续 drain 且 ring bounded；timeout/cancel 必须终止 child；live InnerSession drop 必须触发全部 task cancel，V3 才允许跨 session reattach。

### V2d-1b — Host-owned live-session Shell task/process store

**Status：DONE（2026-09-03）**

- 新增 `HostShellService`，生命周期严格绑定一次 active Target↔Controller `InnerSession`：Host 持有 child/task store，`Start/Status/Attach/Cancel` 全通过 V2d-1a 的 `shell_task` 短 RPC；service/InnerSession drop 会对该 session 的全部 live task 发 cancel，V2 不把 task 跨 reconnect 持久化，V3 才增加 reattach/replay；
- Shell authority 继续三层 fail closed：Host dispatcher 必须看到 persisted `effective_grant.shell=true`，service 还要求显式 `allow_shell=true`；legacy membership `effective_grant=None` 与 helper-only 都不能启动 Shell。shell-capable signed Site Kit 改用 `--connector-only` 时仍由 `CONNECTOR_ONLY` ceiling 剥离 read/write/shell；
- `Start` 只允许 absolute cwd，并把**初始 cwd** canonicalize 后要求位于 signed roots 内且为目录；这不是 filesystem sandbox：owner 一旦显式签 `shell=true`，command 仍按 Host OS user 的正常权限执行，命令自身可访问该用户原本能访问的资源。V2 不偷偷引入容器/沙箱，也不把 cwd containment 误称为完整 Shell filesystem isolation；
- process env 先 `env_clear()`，再只投影最小 host baseline（Windows：PATH/PATHEXT/SystemRoot/WINDIR/ComSpec/TEMP/TMP/USERPROFILE；Unix：PATH/HOME/TMPDIR/LANG/LC_ALL/LC_CTYPE）和 V2d-1a 已 hard-bound 的显式 env；stdin 固定 null，stdout/stderr pipe 持续 drain；
- stdout/stderr 各自进入 **32 KiB** absolute-offset ring；Attach 最多 **12 KiB/stream**，旧 cursor 被淘汰时明确 `lost_prefix=true`。正常退出只有在 stdout/stderr drain 都成功后才标 `Exited`；否则 fail closed 为 `Failed`；
- independent review 发现最初实现是“先 spawn child、后检查 64-task store capacity”，并发 Start 可能产生短暂超额 process。现已改成 **spawn 前 semaphore reservation**，64 个 permit 是真实 process admission hard bound，permit 与 task entry 生命周期绑定；terminal task prune 后才释放 capacity；
- timeout/cancel 会终止当前 shell child；service drop 也触发 cancel。V2 当前不额外宣称 descendant process-tree/job-object 级 kill，也不承诺 connection-loss 后 reattach；这些语义不能从 `kill child` 夸大；
- focused Host 回归 **3/3 PASS**：ring cursor/lost-prefix；grant/root/stdout+stderr/exit/timeout/cancel；spawn-before-capacity reservation + explicit env + service-drop cancel；`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **158 passed / 0 failed / 6 ignored**；
- 真实 no-public NearbyFile Connector 回归在同一 Helper-opaque InnerSession 中连续 `Read → PathInfo → Glob → Grep → Write → Edit → Read verify → Shell Start → Status → Attach` **1/1 PASS（3.51s）**；shell-capable 同一 signed Site Kit 的 `--connector-only` 降权 **1/1 PASS（2.97s）**，证明 Helper 没有得到 Shell 明文或执行能力。

下一块 V2d-1c：Controller-owned live Shell task projection + DeviceId-only Local API/CLI。Controller 必须把 `TaskId` 绑定启动它的 DeviceId/current live session，Status/Attach/Cancel 不能拿任意 TaskId 去另一设备；Activity 只记录 command 摘要/结果/字节数，不保存 stdout/stderr 全文或 env。V3 继续独占 reconnect 后 reattach/replay。

### V2d-1c — Controller live Shell projection + Local API/CLI

**Status：DONE（2026-09-03）**

- `RemoteHub` 新增纯内存 live projection：`TaskId → {DeviceId, current session token}`；Start 仍由 DeviceId 定位 live device，Status/Attach/Cancel 只接受 TaskId 并由 Controller projection解析，caller 不能自带另一个 DeviceId 把 task 路由到别的设备；同 Device 新 session register、unregister、disconnect 都清理旧 token projection，所以 V2 明确不支持 reconnect 后 reattach；
- Controller live projection hard cap固定为 `128 remote connections × 64 Host tasks = 8192`，Start 在发命令前 reservation；independent review 又发现 Local API timeout/drop Start future 可能让 Host 已 spawn 但 Controller未登记 TaskId，现将 Host reply/projection completion 移进 `RemoteHub` 内部 completion task：caller消失后仍先拿到 Started 并建立关联，然后自动发送 Cancel并清 projection，避免 orphan command；reservation 使用 RAII，所有错误/cancel路径都会释放；
- reply correlation fail closed：Status 必须返回同 TaskId status，Attach 必须返回同 TaskId output，CancelAccepted 必须返回同 TaskId；Host wrong-kind/wrong-id直接拒绝。focused projection回归 **2/2 PASS**：A/B 两设备不串路、wrong TaskId reply拒绝、session replacement使旧 TaskId失效；caller abort后 Host晚到 Started会被自动 Cancel且 projection/reservation 清零；
- Local API 新增 `ShellStart / ShellStatus / ShellAttach / ShellCancel`。Start Controller-side再次检查 catalog/site revoke、`EXECUTE`、enrollment active 与 `effective_grant.shell=true`；follow-up先由 TaskId projection解析 DeviceId，再做同一授权检查；Host V2d-1b仍独立检查 persisted shell grant，形成 Controller+Host 双端 fail closed；
- CLI 新增 `clew shell start/status/attach/cancel`：只有 Start 支持 shared selector / unique single-device auto-selection；follow-up只接受 TaskId，不重新要求或接受 Device selector。`--env KEY=VALUE` 重复 key 本地拒绝，再由 shell wire contract做 key/value/count/total hard-bound验证；CLI focused **1/1 PASS**；
- Activity 隐私边界：`shell_start` 只记录 command**首 token（最多64 chars）+ command总字节数**，不记录 command args、explicit env、stdout/stderr；Status/Attach/Cancel只记录 `task:<TaskId>`，Attach仅记录实际返回字节数。真实 Windows product smoke使用 command `echo ...` + private explicit env，Start→Status→Attach PASS，42-byte stdout proof正确，Activity summary=`echo (50 bytes)`且完整 Activity JSON不含 stdout proof或 env value；
- 默认 Site Kit（无 `--allow-shell`）真实 enrollment 后 `clew shell start` 在 Controller Local API 精确 `Denied: Shell is not permitted for this device`；shell-capable正向 product smoke在 orphan-cancel hardening后再次 PASS；所有 smoke Controller/Host/state均清理；
- final validation：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **161 passed / 0 failed / 6 ignored**。

V2d Shell minimum 至此 DONE：这是**live-session persistent task**，不是 reconnect-persistent task。connection loss 后 task reattach/replay 仍严格属于 V3。下一块 V2e：MCP stdio + Streamable HTTP，只做 Controller Local API adapter，不在 MCP 内复制选机/权限/文件/Shell状态。

### V2e-1 — MCP stdio Local API adapter

**Status：DONE（2026-09-03）**

- 采用官方 Rust MCP SDK `rmcp 3.2.0`，只启用 server/macros/stdio/schema 所需 features；不手写 JSON-RPC/MCP framing，也不把 MCP 变成第二个 Controller runtime；
- 新增 `clew mcp stdio --state-dir ...`。MCP server 只持有 `LocalApiClient`，没有 Controller state store、Host/iroh/InnerSession import；设备选择直接调用 `clew-core::select_executable_device`，所以 DeviceId / `Site/Device` / unique short name / single-device auto-select 与 helper-only 排除语义完全复用 CLI/core，而不是复制一套 selector；
- stdio暴露 11 个 typed tools：`devices/read/path_info/glob/grep/write/edit/shell_start/shell_status/shell_attach/shell_cancel`。typed `JsonSchema` 输入继续把所有实际 bounds交给既有 Local API/transport验证；Read以 Base64 structuredContent保真返回 binary bytes；其他结果直接复用既有 bounded Local API结构；tool error text额外截到 2KiB且 UTF-8 safe；
- Write MCP contract要求显式 `mode=create_only|match_sha256`；match模式必须带 expected SHA-256，create-only禁止混带 hash。Shell Start才接受 shared device selector；Shell Status/Attach/Cancel只接受 TaskId，继续复用 Controller live projection，不给 MCP重新指定 Device的机会；
- MCP启动前先通过 Local API探测 Controller；若不存在，只 spawn同一个 `clew controller --state-dir ...` single-owner进程并等待最多8秒 ready。MCP stdio退出不关闭 Controller；真实 auto-start smoke确认空 state initialize成功、devices可调用、MCP退出后 Controller仍存活并可单独 authenticated shutdown；
- raw MCP wire smoke（protocol `2025-06-18`）完成 initialize → initialized → tools/list → tools/call；11个工具均有 object inputSchema，`devices`返回 structuredContent，no-device Read通过 shared selector返回 expected tool error；
- 真实 Windows Host product smoke：默认 read Site Kit下 MCP `read`真实返回19-byte `CLEW-MCP-STDIO-READ`；另一个 `--allow-write --allow-shell` Site Kit通过 MCP wire连续 `PathInfo → Glob → Grep → Write(create-only) → Edit(expected SHA) → Read verify → Shell Start → Status → Attach` 全 PASS；所有 smoke state/process均清理；
- focused MCP unit **2/2 PASS**（exact V2 tool surface + UTF-8-safe bounded error）；final `cargo fmt -- --check` PASS，`cargo check --workspace --all-targets` PASS / 0 warnings，`cargo test --workspace --all-targets` **163 passed / 0 failed / 6 ignored**。

### V2e-2 — MCP Streamable HTTP Local API adapter

**Status：DONE（2026-09-03）**

- `clew mcp http` 继续复用 V2e-1 的同一个 `ClewMcpServer` / `LocalApiClient` / shared selector / 11-tool router，只新增官方 `rmcp 3.2.0` Streamable HTTP server transport；MCP HTTP 不拥有 Controller state、Host、iroh 或 InnerSession；
- 默认 listener 为 `127.0.0.1:4877`，`--listen` 只接受 IPv4/IPv6 loopback，非 loopback 地址在启动 Controller/bind listener 前直接拒绝；endpoint 仅挂载 `/mcp`；HTTP request body hard cap **128 KiB**；
- independent security review 补齐本机浏览器/DNS-rebinding 边界：保留 rmcp 的 loopback `Host` validation，并在 listener **实际 bind 后**按真实 port 显式配置 Origin allowlist，仅允许 `http://<bound-loopback>:<port>` 与 `http://localhost:<port>`；无 `Origin` 的原生 MCP client 仍可用，恶意 Host/Origin fail closed；
- 真实 Windows HTTP product smoke 使用独立 Controller + 独立 HTTP adapter：MCP `2025-06-18` initialize **200**；`tools/list` **200** 且仍精确 11 tools；同端口 loopback Origin **200**；`Host: evil.example` **403**；`Origin: https://evil.example` **403**；约 140 KiB request **413**；
- HTTP adapter 停止后独立 Controller 仍 `ready=true`，随后通过 authenticated Local API `shutdown` 正常退出；临时 state 全部清理，证明 HTTP adapter 生命周期不成为 Controller owner；
- focused MCP unit **4/4 PASS**（loopback listener、exact Origin allowlist、tool surface、UTF-8-safe bounded error）；final `cargo fmt -- --check` PASS；`cargo check --all-targets` PASS，0 warnings；显式 `cargo test --workspace --all-targets` **165 passed / 0 failed / 6 ignored**；
- V2 不使用 MCP HTTP session 实现 Shell reconnect/reattach/replay；这些恢复语义仍严格属于 V3。

V2 Agent Minimum 至此 DONE：coding agent 已可通过 CLI、MCP stdio 或 loopback Streamable HTTP，以同一 authoritative selector/Local API 完成 Devices + bounded Read/PathInfo/Glob/Grep + atomic Write/Edit + live-session Shell task。下一阶段进入 V3 Reliability。

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

**Status：DONE（2026-09-03；V3a–V3f 全部封板）**

### V3a — Session generation + path telemetry + idle liveness

**Status：DONE（2026-09-03）**

- 将 V2 Shell projection 内部使用的 RemoteHub `token` 正式提升为 **session generation**：每次 Controller 接受新的 Target InnerSession 都分配新的 process-local monotonic generation；同一 iroh connection 内 Relay↔Direct path 变化只更新 path telemetry，**绝不增加 generation**；generation 不落盘、Controller 重启后重新开始，明确不是持久 request/replay id；
- 新增 `RemoteSessionInfo`：`connected|disconnected`、`direct|connector` topology、`direct|relay|mixed_or_unknown` path、`connected_unix_ms` / transition / path-change 时间。Direct session 订阅 iroh `path_events()`；Connector path 明确标 `connector + mixed_or_unknown`，不把 Helper↔Controller 外链冒充 Target direct/relay；stale generation 的 path event / unregister 不得覆盖新 session；
- Local API 新增 `session.path_info` 语义与 `RemoteSessionPathInfo`，CLI 新增 `clew session-path-info <DeviceId>`；MCP 在保留全部 V2 11 tools 的基础上增加 `session_path_info`（V3 当前 12 tools）。该诊断面故意要求 stable DeviceId，因为它也覆盖 helper-only / offline device，不复用只面向 online executable 的 V2 selector；未知 DeviceId fail closed；
- `Devices.last_seen_unix_ms` 现在在真实 disconnected transition 后投影；在线状态统一由 session telemetry 驱动，不再只看一个裸 `sessions.contains_key`；
- 真实 Windows product smoke 暴露并修复一个重要 idle-loss bug：旧 member loop 空闲时只等 Controller command，Host hard-kill 后 15s 仍误报 connected。现在 Controller 同时监听 iroh connection close，并增加 **5s interval / 5s timeout 的 InnerSession 加密 heartbeat**；Host 只在 E2E InnerSession 内 echo generation payload 为 `session_pong`，Connector 仍只能搬密文；heartbeat 不改变 generation，也不成为业务 retry；
- 修复后真实 hard-kill **686ms** 进入 disconnected，`last_seen_unix_ms == last_transition_unix_ms`；同一 Host state 重启复用同一 DeviceId，session **generation 1 → 2**；heartbeat 跨过空闲周期后真实 bounded Read 仍返回 `CLEW-V3A-HEARTBEAT`；MCP `session_path_info` structuredContent PASS；
- focused generation/stale-event **1/1 PASS**，MCP V3 surface **1/1 PASS**；`cargo check --workspace --all-targets` PASS / 0 warnings；`cargo test --workspace --all-targets` **166 passed / 0 failed / 6 ignored**。

下一块 V3b：request idempotency/replay matrix。先冻结每类现有短 RPC 在“发送前失败 / 已发送未知结果 / 收到结果后 caller 丢失”三种边界下是否可自动 retry；Read/PathInfo/Glob/Grep 可安全 replay，Write/Edit 只能凭现有强 precondition + stable request identity 收口，Shell Start 不能盲 replay。V3b 不把 File resume 或 Shell reattach 偷塞进一次通用 retry wrapper。

### V3b-1 — Stable RequestId + RPC correlation envelope

**Status：DONE（2026-09-03）**

- 新增 `RequestId` stable UUID strong type；它与 `TaskId` 分离，表示一次业务 RPC 的逻辑身份，不使用 InnerSession frame sequence、Local API connection 或 session generation 代替；
- 所有现有业务短 RPC（Read / FsQuery / FsMutation / ShellTask）统一包入 bounded `rpc_request` / `rpc_reply` envelope：16-byte RequestId + bounded nested kind + exact payload length；Host 解包后执行业务，再以同一 RequestId 封装结果；Controller 对 reply RequestId mismatch fail closed；
- `session_ping/session_pong` heartbeat 故意保持 envelope 外的独立 liveness frame，避免把 heartbeat 误变成可 replay 业务请求；Connector 仍只搬已有 InnerSession ciphertext；
- Controller 当前每个新业务调用只生成一次 RequestId，并在当前 live session 内用于 send/reply correlation；**本块不声明自动 retry、Host dedupe 或 exactly-once**。连接在 request 已发送但 reply 未确认时断开，仍返回失败；下一块才基于同一 RequestId 实现按操作分类的 replay/dedupe；
- focused validation：RPC typed/binary roundtrip + wrong-id/nil/truncation/wrong-kind **2/2 PASS**；真实 no-public NearbyFile Connector 的全业务链在 correlation envelope 下 **1/1 PASS（3.77s）**；`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS，0 warnings；`cargo test --workspace --all-targets` **168 passed / 0 failed / 6 ignored**。

下一块 V3b-2：冻结并实现 replay matrix。Read/PathInfo/Glob/Grep 可在新 generation 上以**同一 RequestId**安全 replay；Write/Edit 需要 Host bounded RequestId result cache，重复 RequestId只能返回第一次已完成结果，绝不能再次落盘；Shell Start 不自动 replay，Status/Attach/Cancel 仍依赖后续 V3 Shell reattach 语义，不能由通用 RPC retry 擅自恢复。

### V3b-2a — Host-process mutation RequestId dedupe

**Status：DONE（2026-09-03）**

- `HostReadService` 增加跨 active InnerSession reconnect 保持的 mutation replay state；它与 Host membership runtime 同寿命，不放在单次 dispatcher/session 内。cache hard cap **128 entries**，只保存 `RequestId + SHA-256 request fingerprint + bounded FsMutationReply`，不保存 Write/Edit 正文；completed entries按最老 sequence有界淘汰，in-flight entry不被驱逐；
- Host `fs_mutation` dispatcher 把 V3b-1 envelope 的 RequestId传给 service。相同 RequestId + 相同 request fingerprint：已完成时直接返回第一次 reply；仍 in-flight 时等待同一个 worker；相同 RequestId但正文/precondition不同则 `InvalidRequest`，绝不执行磁盘操作；cache 全为 in-flight 时结构化 `Capacity` fail closed；
- 修复一个 V2 mutation timeout 的真实未知结果边界：原实现 `timeout(spawn_blocking)` 超时后 worker仍可能继续落盘，但超时 reply之后没有任何状态可供重试识别。现在首次 request先登记 in-flight，再由独立 completion task等待 blocking worker；调用等待超过 signed policy timeout只返回“still in progress”，worker最终真实 reply仍写入 cache并唤醒后续同 RequestId replay；
- 这不是无限 exactly-once：cache只在当前 Host process内且有界。Host进程重启/entry淘汰后，同一逻辑 mutation仍依靠既有 create-only / expected SHA-256 precondition fail closed；Controller重启或客户端发起一个**新**逻辑请求会产生新 RequestId，不把 bounded cache宣传成持久事务日志；
- focused dedupe **1/1 PASS**：首次 create-only成功后测试人为修改目标文件，再用同 RequestId重放，精确返回第一次 result且目标保持人为修改内容，证明没有再次执行；同 RequestId换正文明确 `InvalidRequest`。既有 mutation authority/precondition **1/1 PASS**；no-public Connector全业务链 **1/1 PASS（3.62s）**；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **169 passed / 0 failed / 6 ignored**。

下一块 V3b-2b：Controller在同一 Local API总 timeout预算内等待新 session generation，并对 Read/FsQuery/FsMutation用**同一 RequestId**重发。Read/PathInfo/Glob/Grep天然安全；FsMutation依赖本块 Host dedupe + 原强 precondition；Shell Start/Status/Attach/Cancel仍不进入通用 replay。

### V3b-2b — Cross-generation replay + active-RPC liveness

**Status：DONE（2026-09-03）**

- `RemoteHub` 增加 process-local session-change revision `watch`：register/unregister/disconnect 都在释放 state lock 后推进 revision；replay caller先检查当前 session，再等待 revision变化，因此没有 polling，也没有“先检查、后 subscribe”式 lost wakeup。每次重放都要求 generation 与刚失败的 generation不同，不能在已坏旧 channel 上自旋；
- Read/FsQuery/FsMutation 每个 Local API逻辑调用只生成一个 RequestId。send失败、reply channel断开或当前 session RPC失败后，Read/Query/Mutation等待新 generation并用**同一 RequestId**重发；Read/PathInfo/Glob/Grep天然可 replay，mutation依赖 V3b-2a Host cache + create-only/expected-SHA precondition。Host返回 `FsMutation Timeout` 表示 worker仍 in-flight 时，Controller先在**同一 generation / 同 RequestId**再次查询 cache；若随后 session断开，再在新 generation继续同 ID；
- Shell Start与 Shell Status/Attach/Cancel故意不进入上述通用 replay loop。Start在 reply未知时不能 blind replay；V2 task projection仍绑定当前 generation，generation替换会使旧 projection stale。Shell跨 reconnect恢复只由下一块 V3c `TaskId` reattach定义，不能借通用 RequestId retry偷跑；
- 真实 Windows 50k-entry Glob hard-kill gate暴露并修复 V3a 的 active-RPC liveness遗漏：旧 5s/5s heartbeat只在 member loop idle时运行，进入长 `inner.recv(reply)` 后会被业务 RPC串行等待遮住；iroh `connection.closed()`本身又不会在远端进程 hard-kill 后立即完成，因此 Host死后10s仍可能误报 online；
- 新增 E2E `rpc_progress`：Host长业务操作未完成时每 **2s** 只发送 `rpc_progress + 16-byte RequestId`，不包含业务正文、path、stdout/stderr或env；Controller等待最终 reply时要求每 **5s** 至少收到一次同 RequestId progress或最终 reply。progress ID错配/截断 fail closed；合法 15s Glob/Grep timeout不会被5s liveness窗口截短，Host hard-kill后 progress停止，当前 generation约5s内退出并触发 replay。idle `session_ping/pong`仍独立存在；
- Controller仍不拥有无界后台 retry deadline：整个 RemoteHub replay future受既有 Local API `signed policy timeout + 2s` 外层 timeout取消。caller超时/取消后不会继续偷偷重放。mutation若在 caller超时后最终完成，Host process-lifetime cache仍会保存该 RequestId结果；客户端之后发起**新**逻辑调用会获得新 RequestId，并继续依靠强 precondition fail closed；这仍不是持久 exactly-once事务；
- focused validation：`rpc_progress` bounded/correlation **3/3 PASS**；RemoteHub synthetic replay **1/1 PASS**，覆盖 initial-offline wait、Read/FsQuery跨 generation同 ID、FsMutation same-generation in-flight retry→cross-generation同 ID，以及 Shell Start零 replay；V3a generation stale-update **1/1 PASS**；no-public Connector full business chain **1/1 PASS（3.33s）**；
- 真实 Windows product gate：50,000 empty files，启动 `clew glob <root> '**/*.zzz' --limit 1` 后250ms确认原始 Glob仍 in-flight，随后 hard-kill Host；Controller **6073ms**进入 offline，原 DeviceId重启后 session **generation 1→2**；没有重启/重发 CLI，**原始同一个 Glob进程**在新 generation完成，exit `0`，返回完整 `entries=[] / truncated=false` page。smoke finally authenticated shutdown Controller并删除全部 fixture/state；
- final gate：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **171 passed / 0 failed / 6 ignored**。

V3b request replay matrix至此封板。下一块 V3c：Shell task reattach。目标是 Host task在 active InnerSession断开时不再按 V2语义立刻取消，而是进入有界 reconnect grace；Controller用 stable TaskId在新 generation重新证明 device/session后恢复 projection。必须保持 explicit timeout/cancel，不能把断线 grace升级成永久 orphan process。

### V3c-1 — Host membership-owned Shell reconnect grace

**Status：DONE（2026-09-03）**

- 将 `HostShellService` 生命周期从单次 active `InnerSession` 提升到 `serve_networked_membership_until_inner` 的 Host membership reconnect runtime，与已跨 reconnect 持有的 `HostReadService` 同层；只有 signed `effective_grant.shell=true` 的 executable membership才创建 Shell service，helper-only/legacy grant=None仍无 Shell owner；
- 新增共享 `SHELL_RECONNECT_GRACE_MS = 30_000`。每条成功认证的 Target↔Controller InnerSession只取得一个 session guard：detach时不再立即 cancel live task，而是进入 **30s** reconnect grace；同一 Host membership在 grace内重新建立 authenticated InnerSession会推进 epoch并取消旧 timer的取消资格，因此 TaskId/process/output ring继续存活；
- grace timer只持有 `Weak<HostShellServiceInner>`，不会为了“等待重连”反向延长 Host runtime寿命。真正 Host membership/service drop仍走既有 Drop并立即 cancel所有 live child；如果 guard析构时 Tokio runtime已不可用，也直接 fail-safe cancel，不因启动 grace timer而 panic；
- grace只改变断线期间的 task owner生命周期，不改变 Shell权限、command/cwd/env、stdout/stderr ring、task timeout或 explicit Cancel。task自身 timeout仍优先生效；30s内无人重连且 task仍 live时统一 cancel。Host进程重启仍丢失 task，V3c不把 task写入磁盘或升级成系统后台服务；
- focused grace **1/1 PASS**：测试用80ms grace，第一次detach后20ms重连并跨越旧grace仍保持 `Running`，第二次detach不重连后进入 `Cancelled`；既有 service-drop立即取消 **1/1 PASS**；no-public NearbyFile Connector full business chain **1/1 PASS（3.57s）**；
- final gate：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **172 passed / 0 failed / 6 ignored**。

下一块 V3c-2：Controller保留 `TaskId → DeviceId` projection跨 generation，但把旧 generation标为 detached并设同一30s deadline。新 generation不能自动信任旧 projection；第一次 Status/Attach/Cancel必须先在**同一 authenticated DeviceId**上用 `Status {task_id}`证明 Host仍持有 task，再更新 projection generation。过 grace或 Host返回 NotFound时清 projection；Shell Start仍不 blind replay。

### V3c-2 — Controller TaskId reattach + per-epoch receipt proof

**Status：DONE（2026-09-03）**

- Controller Shell projection扩为 `TaskId → {DeviceId, generation, reattach_deadline}`。session disconnect/unregister/replacement只把匹配旧 generation的 projection标 detached并设共享 **30s** deadline；`shell_task_device`在 grace内仍只解析原 DeviceId供 Local API重新授权，caller始终不能传另一个 DeviceId。新 generation上的第一次 Status/Attach/Cancel必须先向**同一 authenticated DeviceId**发送 `Status {task_id}` proof；TaskId匹配后才重绑 generation，NotFound、wrong TaskId、超 grace均 fail closed并清 projection；Shell Start继续零 blind replay；
- independent review收紧了 V3c-1 的 Start 半提交窗口：Host新 task默认**没有 reconnect资格**。Controller收到 `Started {task_id}` 后先自动发一次 Status receipt，只有 Host确认该 TaskId且 Controller拿到匹配 reply后才把 Started交给 Local API。receipt前断线时Host立即cancel未确认task；caller abort或receipt reply丢失但Controller已知TaskId时，Controller内部在可用session上重新Status proof后自动Cancel并清projection，避免“Host已经spawn、caller从未拿到TaskId”的长期orphan；
- 第二轮 independent review由真实跨机时序触发：**网络 session 重连本身不等于 task reattach**。Host task entry现在记录创建 `session_epoch` 与 `confirmed_epoch`；每个新 authenticated epoch都必须由 Status/Attach重新证明同一TaskId。旧epoch grace timer不会因为“网络已经连上”就自动放过任务；如果30s内没有 newer-epoch TaskId proof，Host仍cancel该task。这样 Host task owner与Controller projection共享同一有界恢复条件，避免Controller projection过期而Host task继续运行；
- focused Host reconnect **4/4 PASS**：未确认Start快速重连仍cancel；已确认task重连但不做new-epoch proof仍在grace后cancel；new-epoch Status proof后跨旧grace保持Running；再次detach不reproof后cancel。Controller focused覆盖 same-device generation reattach、wrong TaskId reply、caller-abort orphan cleanup、receipt-reply-loss后reconnect proof+auto-Cancel、deadline精确边界；`clew-runtime --all-targets` **35/35 PASS**，V3b Shell Start zero-replay回归继续PASS；
- 真实 no-public NearbyFile Connector full business chain在per-epoch proof语义下 **1/1 PASS（3.49s）**，Helper仍只搬InnerSession密文；Local API/MCP文案同步为V3 bounded reconnect grace，并明确Shell Start never blind-replayed；
- **真实 Windows cross-machine product gate PASS**：cap00运行Controller/CLI，mzd运行同一构建的独立Host副本与真实 `--allow-shell` signed Site Kit；Windows Firewall只对mzd的Host副本做20s outbound block，Host进程全程不重启。连续监控明确看到原session **generation 3 disconnected → generation 4 connected（13.750s）**；在检测到generation 4的同一循环立刻对原TaskId `236fa14d-4b60-4ff1-a3a5-e2013d48dc64`执行Status，仍为`running`。随后原TaskId Attach返回断线前保留的26-byte stdout，Base64解码为 `CLEW-V3C2-ROUND4-BEFORE \r\n`、`lost_prefix=false`，再用同TaskId Cancel成功，证明是**同一Host task/output ring跨InnerSession generation恢复**而不是重启新task；
- smoke cleanup完成：临时firewall由远端 `try/finally`自动删除，显式复查时已无匹配规则；mzd Host测试进程终止，cap00隔离Controller authenticated shutdown，远端/本地fixture与state目录全部删除。final gate：`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **176 passed / 0 failed / 6 ignored**。

V3c Shell reattach至此封板。V3c-1中“网络重连即可免除旧grace取消”的初始规则由本块正式收紧为“**newer-epoch TaskId proof才可免除**”。下一块 V3d：File resume 接口预留；只冻结stable transfer identity、offset/hash/precondition/resume capability与reconnect ownership边界，不实现V5的chunk manifest、bulk data plane、directory transfer或并发调度。

### V3d — File resume interface reservation

**Status：DONE（2026-09-03）**

- 复用早期proto skeleton已经固定的 `FEATURE_FILE_RESUME = 6`，**没有重新分配feature号，也没有提升 `CAPABILITY_VERSION=1`**。V3f随后将这里的查询helper正式收紧命名为 `hello_advertises_feature` / `hello_advertises_file_resume`，强调它们只表示peer Hello显式广告、绝不代表本地runtime已实现或已经协商；focused test冻结Feature 6编号，并证明相同capability_version本身绝不隐含File resume支持。当前生产路径没有因为本块自动把Feature 6加入任何Hello；
- `clew-core`新增stable UUID strong type `TransferId`。它只标识Controller-owned逻辑传输，**不是authorization token**；future resume仍必须重新经过当前Controller/Site/Device授权与当前authenticated session。TransferId不复用RequestId/TaskId，也不绑定某一次InnerSession generation，因此可以跨reconnect指向同一逻辑transfer；
- `clew-transport`新增8KiB hard-bound、versioned `FileResumeDescriptor v1`：绑定 `TransferId + ControllerId + SiteId + DeviceId + direction + device_path + total_size + checkpoint_revision + confirmed_offset + confirmed_prefix_sha256 + optional final_sha256`。descriptor是peer-visible control object，**故意不携带Controller本机path**；future Controller-private local path/state只允许按TransferId本地索引，避免把本机用户名/目录结构无必要暴露给Target；
- 单descriptor fail closed：device path最多2048 UTF-8 bytes且禁NUL；revision非零；`offset <= total_size`；hash只接受canonical lowercase SHA-256；offset=0必须使用empty-prefix SHA-256；`offset == total_size`时必须已有full-file SHA-256且与prefix hash一致，避免把“complete”状态建立在没有最终完整性证明的checkpoint上；decode在JSON解析前先执行8KiB size gate；
- `validate_successor_of(previous)`冻结resume precondition：同TransferId的后继checkpoint不得换Controller/Site/Device/direction/device_path/total_size；revision必须严格递增，offset不得回退，同一offset的prefix hash不得改变；full hash可从未知升级为已知，但一旦已知不得删除或更改；complete transfer拒绝继续追加checkpoint。该比较只冻结恢复identity与checkpoint单调性，future V5仍必须对实际partial-file bytes重新验证prefix hash，不能把descriptor当成磁盘事实；
- 本块**不打开文件、不创建 `.part`、不发送chunk、不增加transfer task/store/Local API/CLI/MCP、不新增transport ALPN**，也不实现V5的manifest、目录传输、并发、progress/cancel。它只给未来V5预留能与V3 reconnect语义一致的control contract；
- focused descriptor **4/4 PASS**，proto Feature 6 **1/1 PASS**；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **181 passed / 0 failed / 6 ignored**。

V3d至此封板。下一块 V3e：sleep/resume。目标只处理OS suspend/长时间sleep造成的stale session/path/liveness与Host reconnect节奏，不做后台service persistence；恢复后必须重新authenticated session/generation，旧RPC/Shell/File resume各自继续遵守V3b/V3c/V3d既有身份边界。

### V3e — Sleep/resume continuity + wall-clock deadlines

**Status：DONE（2026-09-03）**

- Controller每个authenticated Target InnerSession新增process-local `SessionContinuity`：记录最近一次**peer activity的Unix wall-clock ms**。continuity hard gap固定为 **15s = 5s heartbeat interval + 5s timeout + 5s margin**；空闲heartbeat发出前、每个业务RPC发送前、每个 `rpc_progress`/final reply收到时都检查。墙钟前进超过15s或发生回退都视为session continuity丢失，当前generation立即fail closed并退出；不会在长suspend后的旧Noise transport上继续Read/Mutation/Shell。V3b replayable RPC随后仍以同RequestId等待新generation，Shell/File分别继续遵守V3c/V3d身份边界；
- idle heartbeat与Host active-RPC `rpc_progress` interval都显式改为 `MissedTickBehavior::Skip`，避免suspend/resume后Tokio默认burst补发一串旧timer tick。长停顿恢复只触发一次stale-session收口/重新认证，不制造heartbeat/progress storm；
- Host Shell command timeout与V3c reconnect grace从纯相对 `tokio::sleep`提升为**wall-clock deadline + 最多250ms poll**。系统/进程长停顿计入用户给出的command timeout和30s reconnect grace；墙钟回退同样fail closed。这样Controller projection的Unix deadline与Host task owner不再因平台上monotonic timer是否计入sleep而分叉；每个new session epoch仍必须重新Status/Attach proof，网络恢复本身不续task；
- focused validation：Controller continuity边界 **1/1 PASS**（15s整可接受，超过即stale，clock regression拒绝）；Host wall-deadline纯状态 **1/1 PASS**；V3c reconnect **4/4 PASS**；原 `live_shell_task_has_bounded_output_status_timeout_cancel_and_root_policy` **1/1 PASS**；V3b cross-generation replay / Shell Start zero-blind-replay **1/1 PASS**；
- **真实 Windows runtime-pause product gate PASS**：cap00隔离Controller + mzd真实Host/同一signed `--allow-shell` Site Kit。单个cap00 PowerShell测试进程先精确读取同DeviceId **generation 7**，随后仅对隔离Controller PID `86652` 调用Windows `NtSuspendProcess` 暂停 **20.039s**，`finally`中恢复；恢复后同一脚本轮询到**generation 8**，`connected_unix_ms=1788444177839`明确晚于suspend start `1788444156534`，证明长runtime pause后旧InnerSession没有被续用，而是重新authenticated session/generation。没有暂停整机、SSH或mzd；
- Host-runtime pause另外做过诊断性探针，但自动化调用间隙与自然network generation切换会让归因不稳定，因此**不计入正式PASS**、也不写成OS sleep真机证据；Host侧正式证据只采用wall-deadline focused tests +正常Shell timeout回归。V3e不声称拥有Windows/macOS/Linux power-event hook，也不引入service/background persistence；
- 真实 no-public NearbyFile Connector full business chain **1/1 PASS（3.57s）**；smoke后mzd Host job、Controller、远端/本地fixture全部清理。final `cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **183 passed / 0 failed / 6 ignored**。

V3e至此封板。下一块 V3f：wire/capability compatibility matrix。重点冻结`WIRE_MAJOR` hard incompatibility、`CAPABILITY_VERSION`与Feature位的独立语义、unknown feature forward-compat、FileResume feature 6不被版本号暗示，以及旧peer/新peer在不理解新feature时仍可安全使用V2/V3基础面；不在本块新增第二wire major或自动协议升级。

### V3f — Wire/capability compatibility matrix

**Status：DONE（2026-09-03）**

- 将“认识proto枚举值”与“当前build真正实现”拆开：新增显式 `IMPLEMENTED_FEATURES = [ToolRpc, ShellTask]`。`Forward / Socks5 / HttpConnect / FileResume`即使是当前代码生成器认识的enum，也只是future/reserved vocabulary；V4/V5 runtime未闭合前 `locally_implements_feature(...)` 必须为false，防止以后因为enum已存在就误开功能；
- V3d的feature查询helper更名为 `hello_advertises_feature` / `hello_advertises_file_resume`，只描述某个Hello是否**显式广告**位。新增 `feature_negotiated(local, peer, feature)`：先分别执行完整Hello wire validation，然后要求“当前build实现 + local显式广告 + peer显式广告”三者同时成立。`negotiated_implemented_features`只从当前实现白名单做交集，因此unknown feature与known-future feature都不会偷偷进入可用面；
- `CAPABILITY_VERSION`继续只要求非零，不作为feature推断器。compatibility matrix明确验证capability_version **1 ↔ 999**仍可在同一`WIRE_MAJOR=1`下协商共同的ToolRpc/ShellTask；高版本peer附带unknown feature `999`会被wire roundtrip保真保存，但当前build忽略其语义。双方即使都广告`FileResume=6`，因为当前build没有V5 data plane，协商结果仍为false；
- `WIRE_MAJOR`仍是硬兼容边界：任一Hello major不是当前`WIRE_MAJOR=1`，即使feature bits完全相同也在协商前返回`UnsupportedWireMajor`；本块不增加第二major、fallback major或自动协议升级；
- 新增 `NegotiatedLimits`：双方Hello都先通过各自 hard bounds，之后`max_frame_size`与`max_concurrent_requests`只取两边更严格的**min**。peer宣告超过本地hard max时直接拒绝，不允许“新版本更大额度”反向扩大旧peer/当前build安全面；
- 本块仍是proto compatibility contract，不把Hello强行接进现有InnerSession握手、不自动广告任何feature、不改变ALPN/Noise/Helper路径。它冻结的是V4/V5以后接入feature negotiation时必须遵守的语义，不伪装成已经存在第二套runtime negotiation；
- proto focused **11/11 PASS**；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **187 passed / 0 failed / 6 ignored**。

**V3 Reliability 至此 DONE。** 七项计划全部闭合：connection/session generation与path/liveness、request idempotency/replay matrix、Shell TaskId reattach、File resume接口预留、path telemetry、sleep/resume continuity、wire/capability compatibility matrix。下一阶段进入V4 Dynamic Networking；第一块只做bounded TCP forward，不提前把SOCKS5/HTTP CONNECT一起塞进同一大块。

- connection reconnect；
- request idempotency/replay matrix；
- Shell task reattach；
- File resume 接口预留；
- path telemetry；
- sleep/resume；
- wire/capability compatibility matrix。

## 11. V4 — Dynamic Networking

**Status：DONE（2026-09-04；V4a–V4c 全部封板）**

### V4a-0 — Signed TCP egress authority projection

**Status：DONE（2026-09-03）**

- 在真正打开 TCP forward data plane 前先补长期授权：`PermissionGrant` 新增 serde-default `tcp_egress=false`。旧 bootstrap/backup/state 缺字段时一律按 false 解析，避免升级后静默获得出网能力；
- 新增 execute-preferred ceiling `EXECUTE_READ_WRITE_SHELL_TCP_CONNECTOR`，但 enrollment 继续执行 `signed grant ∩ Controller ceiling ∩ claim ceiling`，因此 ceiling 只允许 owner 明确签出的 TCP egress 穿过，绝不主动提权；`CONNECTOR_ONLY` 继续把 read/write/shell/tcp_egress 全部剥离；
- owner CLI `mint/invite` 新增显式 `--allow-tcp-egress`；`InviteIssueRequest` 增加 serde-default false。Controller GUI 明确传 false，所以朋友侧默认 invitation 仍没有 TCP egress；write/shell/tcp egress 三项彼此独立 opt-in；
- Host membership sidecar recovery 使用新的 full execute ceiling 重算 persisted effective grant，Controller bootstrap execute-preferred 同样使用 full ceiling；旧 Host marker/legacy grant仍 fail closed；
- 真实 isolated Controller mint smoke：默认 `site.clew` 为 `execute=true/connector=true/read=true/write=false/shell=false/tcp_egress=false`；仅加 `--allow-tcp-egress` 后精确变为 `tcp_egress=true`，write/shell保持 false；
- focused grant **3/3 PASS**（helper-only降权、ceiling不提权、legacy缺字段false）；`cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS；`cargo test --workspace --all-targets` **188 passed / 0 failed / 6 ignored**。

### V4a-1 — Controller-owned bounded local TCP forward

**Status：DONE（2026-09-03）**

- 新增 stable `ForwardId` / `ForwardConnectionId` 与 bounded `tcp_forward { Open / Exchange / Close }` business RPC。TCP payload **不**绕过业务安全边界：Controller↔Target 继续使用既有 authenticated InnerSession；Helper/Connector 若在路径中仍只看 ciphertext；
- 当前只实现 local side：Controller `TcpForwardManager` 仅绑定 `127.0.0.1:<port>`，`listen_port=0` 可由 OS 分配。listener hard cap **64**；每 listener accepted connection hard cap **64**；Host active session outbound connection hard cap **64**；单次 Exchange raw chunk ≤ **12 KiB**、destination host ≤255 ASCII bytes、connect timeout ≤10s、单次 remote read wait ≤250ms；
- Host `Open` 先拿 **pre-connect semaphore permit** 再 `TcpStream::connect`，permit 与 connection entry 同寿命；focused 65th-open regression确认 destination listener只接受64条连接，Capacity在创建第65个 outbound socket前返回；
- Controller listener lifecycle真正归 Controller：`clew forward add` 只是 authenticated Local API client，CLI exit后 listener继续存在；`forward list/remove` 操作同一个 Controller-owned manager。Add前 Controller再次检查 device/site active + EXECUTE + enrollment `effective_grant.tcp_egress=true`；默认/legacy/helper-only fail closed；
- 每条 accepted local TCP connection在 Open时固定当前 Target **session generation**。Exchange/Close只允许同 generation，generation变化时旧 connection直接关闭，**不**把已建立 TCP流 replay/migrate 到新 session；Controller listener本身保留，新 local connection可在新 generation重新 Open。该行为与V3冻结的TCP non-migration语义一致；
- `Feature::Forward` 从V3f“known future”提升进当前 `IMPLEMENTED_FEATURES`；Socks5/HttpConnect/FileResume仍不协商，capability_version仍不能替代显式 feature bit；
- 最终 Windows exact binary SHA-256 `fec92a99e7f97778c5449d311d2445c8a0812d0a1b8326c3c54783559e78f5ca`，cap00 Controller + Gamma Target real product gate：`forward add`返回后CLI已退出，独立local client经Controller listener→InnerSession→Gamma Host→Gamma `127.0.0.1` echo精确返回`CLEW-V4A1-FINAL-PROOF`；默认未签tcp_egress的第二Site对`forward add`精确Controller-side `Denied`且没有新增listener；`forward remove`后端口不再accept；
- reconnect/non-migration exact gate：positive Target初始generation=1，保持local TCP连接后停止Host，旧连接精确EOF；Controller上的同ForwardId/listen `127.0.0.1:62167`仍存在；同DeviceId Host恢复后全局generation=3，新local connection通过同一listener精确返回`V4A1-FINAL-AFTER-RECONNECT`。首次复测中新连接失败最终定位为测试echo server被旧连接reset异常杀死，改成per-client异常隔离fixture后同产品路径立即PASS，不计产品故障；
- focused：transport wire **1/1 PASS**；Host real TcpStream/grant **1/1 PASS**；Host pre-connect capacity **1/1 PASS**；Controller manager ownership **1/1 PASS**；generation non-replay **1/1 PASS**；Forward feature matrix **1/1 PASS**。`cargo check --all-targets` 0 warnings；`cargo test --workspace --all-targets` **193 passed / 0 failed / 6 ignored**；所有 cap00/Gamma smoke jobs/state均已清理。

V4a TCP forward至此封板。下一块 V4b：SOCKS5 TCP egress，必须复用同一 Controller-owned loopback listener + signed `tcp_egress` + generation non-migration primitive；只实现 CONNECT，SOCKS5 UDP ASSOCIATE不做，不能复制一套第二TCP数据面。

### V4b — Controller-owned SOCKS5 TCP CONNECT adapter

**Status：DONE（2026-09-04）**

- SOCKS5 不新增第二套远端 TCP 数据面：Controller `Socks5ProxyManager` 只负责 loopback SOCKS5 handshake / listener 生命周期；每个 CONNECT 最终调用 V4a 已验收的 `open_forward_connection + pump_forward_connection`，因此 TCP payload 仍在 Target↔Controller InnerSession ciphertext 内，Helper 仍只搬密文；
- 仅支持 SOCKS5 v5 + no-auth + `CONNECT`。Controller listener 固定 loopback，`listen_port=0` 可自动分配；proxy listener hard cap **64**，每 proxy accepted connection继续复用 V4a **64** connection admission。`BIND` / `UDP ASSOCIATE` 不实现，其中 UDP ASSOCIATE明确返回 `REP=command_not_supported`；
- ATYP支持 IPv4 / IPv6 / domain，domain最终仍必须通过共享 `TcpForwardDestination` 的 **255-byte ASCII/no-control/no-whitespace + nonzero port** hard bound。independent review补齐 malformed destination行为：invalid UTF-8 domain / invalid destination不再静默断开，而是明确返回 `address_not_supported`；无受支持auth method返回 `no acceptable methods`；
- `ProxyId` 是 Controller-owned proxy listener identity；`proxy socks5 add/list/remove` 全走 authenticated Local API，CLI退出不拆 listener。Add 与V4a Forward共用同一个 Controller-side `authorize_tcp_egress_device`：要求 Site/device active、EXECUTE、enrollment `effective_grant.tcp_egress=true`；默认/legacy/helper-only全部fail closed；
- SOCKS5连接继承V4a的session-generation non-migration：Open时固定当前 Target generation，Exchange/Close只走该generation；断线后旧TCP流关闭，不透明迁移。Controller proxy listener可继续存在，新客户端在新generation重新CONNECT；本块没有复制replay或连接恢复逻辑；
- `Feature::Socks5` 从V3f reserved vocabulary提升进当前 `IMPLEMENTED_FEATURES`；`Forward + Socks5` 可显式协商，`HttpConnect/FileResume`仍不能协商，capability_version仍不推断feature；
- exact Windows binary SHA-256 `4cf27090890a59c8e7318cfa91405932c1031ab8b6f60b9229291311b2d3e139`（96,634,368 bytes）。真实 cap00 Controller + Gamma Target product gate：只签 `--allow-tcp-egress` 的 Site上线后，`proxy socks5 add` 返回并退出，Controller继续持有 `127.0.0.1:56432`；独立raw client得到 `METHOD=5,0 / REP=0`，CONNECT到Gamma `127.0.0.1:45691` echo后精确返回 `CLEW-V4B-SOCKS5-PROOF`；
- 同一Controller另签默认未授权 Site，Gamma第二Host DeviceId `23e5497a-f8e8-41f4-ac4b-2535ee1a76a7`上线后，对其 `proxy socks5 add`精确Controller-side `Denied: TCP egress is not permitted for this device`，没有新增proxy；正向proxy `remove`后 `127.0.0.1:56432` 不再accept且list为空；所有Controller/Host/echo jobs及本地/远端fixture均已清理；
- focused SOCKS5 parser/manager **2/2 PASS**；workspace `cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **195 passed / 0 failed / 6 ignored**。

V4b SOCKS5 TCP至此封板。下一块 V4c：HTTP CONNECT，继续复用同一 Controller-owned loopback adapter + V4a TCP primitive；只实现 CONNECT tunnel，不实现普通forward-proxy GET/POST，不允许non-loopback listener。

### V4c — Controller-owned HTTP CONNECT adapter

**Status：DONE（2026-09-04）**

- HTTP CONNECT 同样不新增第二套远端TCP data plane：Controller `HttpConnectProxyManager` 只拥有loopback listener与HTTP handshake，成功CONNECT后继续调用V4a `open_forward_connection + pump_forward_connection`；Target仍只通过authenticated InnerSession执行outbound TCP，Helper若在路径中仍只看ciphertext；
- listener固定 `127.0.0.1:<port>`，port 0可由OS分配；proxy/listener与accepted connection上限继续为 **64**。HTTP header hard cap **16 KiB**、handshake timeout **5s**；只接受HTTP/1.0/1.1 `CONNECT authority`，支持`host:port`与`[IPv6]:port`，最终destination继续复用`TcpForwardDestination`的255-byte ASCII/nonzero-port约束；
- 普通forward-proxy GET/POST明确不实现：非CONNECT请求返回 **405 Method Not Allowed**。malformed/invalid authority返回400，header过大431，handshake timeout408；Target connect错误按Denied/Timeout/Capacity/ConnectFailed映射为403/504/503/502等明确HTTP失败响应；非loopback listener继续不开放；
- independent review专门修复CONNECT parser“读过header”的数据丢失风险：parser保留 `\r\n\r\n` 后已预读的tunnel bytes，V4a pump新增`initial_write`入口并按原 **12 KiB** chunk bound先发这批bytes；普通Forward/SOCKS5继续传空initial，不改变原路径；focused test用单write `CONNECT ...\r\n\r\nTLS-TAIL` 精确证明tail保留；
- `ProxyId`继续作为Controller-owned listener identity；Local API/CLI新增 `proxy http-connect add/list/remove`，add复用与Forward/SOCKS5相同的`authorize_tcp_egress_device`，默认/legacy/helper-only fail closed。`Feature::HttpConnect`从V3f reserved提升进当前`IMPLEMENTED_FEATURES`；当前V4 `Forward + Socks5 + HttpConnect`显式可协商，FileResume仍不能；
- exact Windows binary SHA-256 `9254535817e340deab79c7649b88a319f1543f42076bf62be5d23c4d6ac31835`（96,984,576 bytes），cap00/Gamma hash与size一致。真实 cap00 Controller + Gamma Target：positive DeviceId `ac522bc0-c501-4069-ad68-55e9c948d9b0`、Gamma echo `127.0.0.1:45692`；`proxy http-connect add`返回后CLI退出，Controller持续持有`127.0.0.1:58228`；
- 核心真实gate使用**单次TCP write**发送`CONNECT 127.0.0.1:45692 HTTP/1.1 + headers + CLEW-V4C-PIPELINED`，独立client随后精确得到`HTTP/1.1 200 Connection Established`且紧随的tunnel echo为`CLEW-V4C-PIPELINED`，证明预读tail没有被parser吞掉；同listener普通GET精确返回405；
- 同Controller默认未签tcp_egress的第二Site DeviceId `adba8872-cda9-4330-80f6-6dae52b8b050`对`proxy http-connect add`精确Controller-side `Denied: TCP egress is not permitted for this device`；正向proxy `52db2b13-aa4a-4a16-b4bb-33e120730689` remove后`127.0.0.1:58228`不再accept且list为空；两台Host/echo/Controller与本地/远端fixture均已清理；
- focused HTTP parser/manager **2/2 PASS**，feature matrix **1/1 PASS**；final `cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **197 passed / 0 failed / 6 ignored**。

**V4 Dynamic Networking 至此 DONE。** 当前完成local TCP forward、SOCKS5 TCP CONNECT、HTTP CONNECT，以及Controller listener ownership与TCP non-migration语义；SOCKS5 UDP、普通HTTP forward proxy、remote/non-loopback listener继续明确后置。下一阶段进入V5 File Plane。

- TCP forward；
- SOCKS5 TCP egress；
- HTTP CONNECT；
- listener lifecycle 归 Controller；
- disconnect/recovery 与明确的 TCP non-migration 语义。

## 12. V5 — File Plane

**Status：IN PROGRESS（V5a-0 + V5a-1a/1b + V5a-2 DONE；single-file put/get + cross-generation resume DONE）**

### V5a-0 — Single-file manifest / deterministic chunk contract

**Status：DONE（2026-09-04）**

- 复用V3d已经冻结的stable `TransferId + FileResumeDescriptor`，新增peer-visible `FileTransferManifest v1`与`FileTransferChunk`；本块不重新发明resume identity，也不打开文件/创建part文件/增加Local API/CLI/MCP/data plane；
- manifest绑定`TransferId + ControllerId + SiteId + DeviceId + direction + device_path + total_size + chunk_size + final_sha256`。chunk size只接受 **4–32 KiB power-of-two**；chunk count使用overflow-safe `u64::div_ceil`，`u64::MAX` total_size也不会少算或panic；manifest JSON decode前hard cap **16 KiB**；
- 单文件chunk采用deterministic contiguous layout：offset必须按manifest chunk_size对齐，非末chunk必须精确full-size，末chunk必须精确剩余长度；raw chunk hard cap **32 KiB**，每chunk独立SHA-256；chunk Base64 string在decode前先按raw上限计算hard cap，整个chunk JSON另有 **48 KiB** pre-parse gate，避免恶意长Base64先进入可增长decode；
- final SHA-256是manifest必备完整性承诺；empty file必须使用empty SHA。`manifest.initial_resume_descriptor()`直接生成V3d revision=1 / offset=0 / empty-prefix checkpoint并携带同一final SHA，后续runtime必须继续使用V3d successor monotonicity，不允许另一套resume语义；
- privacy边界冻结：peer manifest**永远不含Controller本机path**。`ControllerToDevice`必须携带device-side conflict policy（Fail/Replace/Rename）；`DeviceToController`的Controller本机destination/conflict policy必须保持Controller-private，peer manifest若携带则fail closed；
- V5权限映射冻结但本块不执行I/O：`DeviceToController (get)`后续必须同时要求当前device active/executable + signed/effective `read=true` + device_path在signed roots；`ControllerToDevice (put)`必须要求`write=true` +同一roots。不会新增“有EXECUTE就能传文件”的隐式权限；helper-only/legacy缺grant继续fail closed；
- `Feature::FileResume=6`在本块仍保持V3f reserved/not-implemented，双方广告也不得协商成功；只有真实single-file data plane + resume闭合后才允许加入`IMPLEMENTED_FEATURES`；
- focused manifest/chunk/privacy/hash/bounds **4/4 PASS**；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **201 passed / 0 failed / 6 ignored**。

下一块 V5a-1：单文件 data plane + Controller-owned transfer state。先只做一个transfer内的顺序chunk put/get、final hash、atomic finalization、status/cancel与V3d contiguous-prefix resume；不做directory tree或多transfer并发调度。

### V5a-1a — Host-owned put part/checkpoint/finalize state machine

**Status：DONE（2026-09-04）**

- `FileTransferRequest/Reply`把实际put生命周期冻结为`PutBegin → PutChunk* → Status → Finalize/Cancel`，仍走V3b `rpc_request/rpc_reply` InnerSession envelope；phase显式分`Receiving / ReadyToFinalize / Completed`，避免“最后一块已确认”被误当作“atomic目标已可见”；RPC payload hard cap **56 KiB**，32KiB chunk JSON仍落在既有60KiB InnerSession plaintext内；
- 新增membership-owned `HostFileTransferService`，只在persisted `effective_grant.write=true`的executable membership创建；legacy grant=None、read-only、helper-only没有transfer owner。service与Host membership reconnect runtime同寿命而不是per-InnerSession，因此part/checkpoint可跨session generation继续；Host进程重启持久化resume仍后续收口，不在本块伪称已支持；
- PutBegin再次绑定ControllerId/SiteId/DeviceId/direction与signed roots；destination必须absolute且parent canonicalize后仍在signed roots。symlink target直接拒绝；FailIfExists/Replace/Rename device-side conflict policy均在Host执行，Rename使用bounded **1024**次候选，不允许无界找名；
- part使用device destination**同目录** secure `NamedTempFile`，目标文件在Finalize前不可见。每个chunk先验证TransferId、offset对齐、deterministic expected length与chunk SHA；part现有长度必须精确等于confirmed_offset；durable `write + sync_data`成功后才推进revision/offset/prefix SHA；
- 最后一块在实际写入前先对`prefix_hasher + bytes`计算完整SHA，若不等于manifest final SHA则`HashMismatch`且**不落最后一块**，因此V3d descriptor始终保持合法。相同上一chunk的直接重放返回同Status而不再次写入；跳块/乱序返回OutOfOrder；零字节文件从Begin直接ReadyToFinalize；
- Finalize要求完整prefix SHA==final SHA；同目录atomic persist后sync最终文件，Unix额外sync parent dir。pre-commit错误保留temp可重试；若rename已commit但后续sync报错，状态会标记Completed并返回I/O error，使后续Status能区分“已可见但durability确认失败”，不伪造可重试未提交状态；Cancel只允许未完成transfer，drop temp即删除part；
- independent consistency checks：每次chunk前核对part长度/checkpoint；completed entry可被bounded prune，active/in-flight不会因capacity被静默驱逐；Host transfer store hard cap **16**。Controller-private source path仍不进入peer state；
- focused transport file-transfer RPC/manifest/chunk **5/5 PASS**；Host put/resume/finalize/cancel/scope/authority/conflict **2/2 PASS**；`cargo check --workspace --all-targets` PASS、0 warnings；`cargo test --workspace --all-targets` **204 passed / 0 failed / 6 ignored**。

### V5a-1b — Controller-owned single-file put task + cross-generation resume

**Status：DONE（2026-09-04）**

- 新增 `ControllerFileTransferManager`，生命周期归长期 Controller，不归 `clew file put` CLI。Controller-private state保留 source path / device path / manifest / progress；peer-visible manifest仍绝不包含Controller本机source。active/history hard cap **16 transfers**，terminal entry仅在新start需要容量时有界prune；
- CLI新增 `clew file put/status/cancel`。只有Put使用V2 shared device selector；Status/Cancel只接受stable `TransferId`。`--source`在短命令进程中先canonicalize为absolute UTF-8 path，manager也拒绝relative source，避免CLI cwd与长期Controller cwd不同导致路径漂移；默认conflict=`fail`，不会静默覆盖Target文件；
- Controller source预处理用**同一个打开的File handle**流式计算full SHA-256并seek回0，64KiB hash buffer有界；不把source读进内存。source在传输过程中若被本机其它进程修改，Target final SHA gate会使transfer Failed而不是错误成功；这不是跨进程snapshot/locking承诺；
- RemoteHub为file-transfer暴露“**单次attempt + generation**”API，而不是套V3通用blind replay。PutBegin/Chunk/Finalize结果未知或session loss时，manager必须等**新generation**并先发Status；Host confirmed_offset不变才可重发当前deterministic chunk，若已前进则seek source到新offset继续；任何非0/单chunk增长、scope/hash/revision异常都fail closed；Finalize未知结果同样先Status区分Ready/Completed再决定是否重发；
- Cancel语义统一收口：Preparing/hash阶段、Running阶段、WaitingForReconnect阶段都先进入Cancelling，manager task若收到Cancelled会执行bounded remote Cancel/NotFound reconcile并最终落Cancelled。independent review修复了首版“Preparing cancel后wrapper跳过set_failed，可能永久卡Cancelling”的真实bug；
- Controller Local API新增 DeviceId-only `file.put/status/cancel`；Put再次检查site/device active、EXECUTE、registry `effective_grant.write=true`和非空signed filesystem roots。Host仍独立检查persisted grant/root，形成双端fail closed；Activity只记录`file_put_start + device path`，不记录Controller source path或file bytes；
- synthetic reconnect gate **1/1 PASS**：generation 1首chunk reply故意丢失，generation 2恢复后Controller第一条请求精确为Status；Host报告confirmed_offset=4096后Controller直接发offset=4096的第二chunk而非重放offset0，随后ReadyToFinalize→Finalize→Completed；
- exact final Windows binary SHA-256 `e2f0d95599125179f8b9bf66c11e75ef859f56764a3e30b90b60d52429cab4cb`（98,104,832 bytes），cap00/Gamma完全一致。真实write-capable DeviceId `9bb5d798-1e4d-46e5-b4e7-be7cc7457372`：8MiB/4KiB transfer在generation **3** 的confirmed_offset **4,177,920**时用仅Gamma `clew.exe` 的program-scoped outbound block强制断线，Controller进入`waiting_for_reconnect`；解除后同Host进程generation **4**，原TransferId `dc372e24-...`自动续到 **8,388,608/8,388,608 Completed**，Gamma独立SHA精确为 `2daeb1f36095b44b318410b3f4e8b5d989dcc7bb023d1426c492dab0a3053e74`；
- cancellation真实gate：512MiB source在`Preparing`阶段立即cancel，**Preparing→Cancelling→Cancelled**且Target/part residual=0；另一个64MiB transfer在真实resume后confirmed_offset **18,792,448**时cancel，同样Cancelled且Target/part residual=0；说明修复覆盖pre-remote与mid-transfer两条路径；
- 默认未签write的final-binary readonly DeviceId `a70d8243-5056-4816-9d68-50dafb412669` 对file put精确Controller-side `Denied: file put is not permitted for this device`，目标不存在。Host file-transfer focused **2/2 PASS**；Controller reconnect synthetic **1/1 PASS**；final `cargo fmt -- --check` PASS、`cargo check --workspace --all-targets` PASS / 0 warnings、`cargo test --workspace --all-targets` **205 passed / 0 failed / 6 ignored**；
- 当前resume能力明确只跨**同Host进程的InnerSession generation**。Controller manager和Host transfer store仍是process-memory state，Controller/Host进程重启后不保证恢复。因此`Feature::FileResume`继续保持reserved/not-implemented，不能把本块宣传为durable restart resume。

### V5a-2 — Device→Controller single-file get + cross-generation resume

**Status：DONE（2026-09-04）**

- `FileTransferRequest/Reply` 在同一 `file_transfer` RPC 增加 `GetBegin/GetChunk` 与 `Manifest/Chunk`；Device→Controller manifest 继续只携带 device-side source path / size / chunk size / final SHA，Controller 本机 destination/conflict policy 永不进入 peer manifest；
- Host `HostFileTransferService` 将权限明确拆为 `can_get/can_put`，共享同一个 bounded 16-transfer namespace但绝不交叉授权。read-only membership 创建 service 时 `can_get=true / can_put=false`；Get 仍要求 persisted `effective_grant.read=true` + signed roots，Put 仍独立要求 write；readonly focused 证明 Get 正向而 Put 精确 Denied；
- GetBegin 对 source 做 symlink/regular-file/canonical-root检查，使用同一个打开的 File handle 流式计算 full SHA，再 seek 回 0 并保留 source handle。相同 TransferId + 相同 source/chunk size 重放返回同一 manifest；同 ID 换 source 为 Conflict。GetChunk 只允许 deterministic aligned offset，逐块 SHA；source size 漂移 fail closed，若内容原位改变但 size不变，Controller最终 full SHA仍拒绝混合文件；
- Controller manager 升级为统一 tagged `FileTransferInfo::{Put,Get}`，Put/Get 共用 stable TransferId namespace和全局 hard cap 16；Get start/status/cancel由长期 Controller 持有。Controller-private destination 先在目标同目录建立 secure temp，并维护自己的 `FileResumeDescriptor` / prefix SHA / confirmed_offset；chunk durable write+sync 成功后才推进本地 checkpoint；
- 跨 InnerSession generation 的 Get resume 不借 Host Status 伪造 Controller checkpoint：session失效后使用同一 TransferId在新 generation **重新 GetBegin 并要求完整 source manifest 与首次完全相同**，然后只从 Controller 本地 confirmed_offset继续 GetChunk。final SHA与prefix SHA均匹配后才按 Controller-private Fail/Replace/Rename policy atomic persist并sync；最后发 bounded Host Cancel释放 source handle；
- Local API/CLI 新增 DeviceId-only `file.get` 与 `clew file get [DEVICE] --source <DEVICE_PATH> --dest <LOCAL_FILE>`；Get只检查 active/executable + registry `effective_grant.read=true` + signed roots，完全不依赖 write。CLI relative `--dest`只在短命令进程cwd中绝对化；Activity只记录 `file_get_start + device source path`，不记录 Controller destination；status/cancel 对两方向统一返回 tagged FileTransferInfo；
- 实现过程中真实 Windows fresh enrollment 暴露 main-thread stack overflow：定位到 `connect_bootstrap_channel` 第一次 inline poll `IrohOuter::connect_bootstrap()`；mDNS/Nearby/address fan-out均已排除，单一 direct IP仍复现。最终没有增大线程栈，而是把 bootstrap 与 active member 的 direct Controller dial 都改为 Tokio `JoinSet` worker task，与既有 Helper candidate dial一致；未完成 direct task在 JoinSet drop时自动abort。修复后 fresh cap00 与 Gamma read-only enrollment均从 waiting 正常进入 ready；
- exact final Windows binary SHA-256 `3fc383f26733958d58e726db3b37d4c2de33280b1e4f4bf71f014cfdf2996582`（98,659,328 bytes），cap00/Gamma完全一致。真实 default read-only Gamma DeviceId `eda96692-52f1-4feb-a534-bdb6b9999d25`：102,523-byte Get自动选择唯一 executable device并 Completed，Controller独立 SHA=`a443a57526957e2f38930bd8c9004c50b151e81ff748137093ecb3da545c8683` 与 Gamma source一致；同一 Device Put精确 Controller-side Denied，Gamma目标不存在；
- 真实 8MiB/4KiB reconnect gate：transfer `35bb9c77-4548-4671-95df-f431572dcbdf` 在 generation **1** confirmed_offset **2,375,680** 时启用仅 Gamma `clew.exe` 的 program-scoped outbound block，随后观测同一 Device generation **2**；同一 TransferId confirmed_offset 继续增长到 **8,351,744**（未回0），最终 **8,388,608/8,388,608 Completed**，Controller final SHA=`a5e85ff850b4329a754123d2093d7212620299565775fbd23c49f3b9a1dcb3ad` 与 Gamma source精确一致。此次查询未捕到瞬时 WaitingForReconnect，因此不冒充该瞬时状态证据；generation变化+offset单调续传+最终SHA是正式resume证据；
- focused：transport file-transfer **6/6 PASS**；Host put/get **3/3 PASS**；Controller Get manifest re-proof/reconnect **1/1 PASS**。final `cargo fmt -- --check` PASS；`cargo check --workspace --all-targets` PASS / 0 warnings；`cargo test --workspace --all-targets` **208 passed / 0 failed / 6 ignored**；测试清单总数 214；所有 Gamma/local fixture、program-scoped firewall rule与test jobs均已清理。

V5 single-file双向 data plane至此封板。当前 resume仍只保证同进程的 InnerSession generation变化；Controller/Host进程重启后的 task/part/source state尚未durable恢复，因此 `Feature::FileResume` 继续保持reserved/not-implemented。下一块 V5a-3：process-restart durable single-file transfer state；通过后再评估是否开启 FileResume feature，最后才进入 directory tree 与 bounded multi-transfer concurrency。

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

**Current block：V5 — File Plane（IN PROGRESS；single-file put/get + generation resume DONE）**

**Next block：V5a-3 — process-restart durable single-file transfer state**

V4已在`dfa373d`完整封板。V5a现在已经闭合双向single-file data plane：put由Controller持有私有source、Host持有part/checkpoint；get由Host持有source handle/manifest、Controller持有私有destination/temp/checkpoint；两方向都已在真实Gamma上证明session generation变化后同TransferId继续并final SHA一致，default read-only Get正向且Put Denied。V5当前唯一single-file resume缺口收窄为Controller/Host **process restart durability**；在该gate闭合前FileResume feature继续reserved，不提前进入directory或多transfer调度。

### Change log

- **2026-09-04** — V5a-2 Device→Controller single-file get + cross-generation resume DONE：同一file_transfer RPC新增GetBegin/GetChunk，Host readonly can_get/can_put分权，Controller私有destination/temp/checkpoint与manifest re-proof resume；default read-only Gamma 102,523-byte Get SHA一致且同设备Put Denied。8MiB/4KiB真实gate从generation1/offset2,375,680经program-scoped断线到generation2，原TransferId offset继续到8,351,744并Completed 8,388,608，Controller final SHA=`a5e85ff8...b3ad`。真实fresh enrollment同时暴露Windows inline iroh direct dial main-stack overflow，改为JoinSet worker后cap00/Gamma均恢复，不增大线程栈。workspace **208/0/6**。下一块V5a-3 process-restart durable state；FileResume仍reserved。

- **2026-09-04** — V5a-1b Controller-owned single-file put + cross-generation resume DONE：Local API/CLI新增file put/status/cancel，Controller后台manager hard cap16并私有持有source；unknown chunk/finalize结果不blind replay，新generation先Status对齐confirmed_offset。independent review修复Preparing cancel永久Cancelling风险。exact final binary `e2f0d955...b4cb` cap00/Gamma一致；真实8MiB transfer在generation3/offset4,177,920断线后generation4续到Completed，Gamma final SHA一致；Preparing与Running cancel均Cancelled且part residual=0；default Site Denied。workspace **205/0/6**。下一块V5a-2 Device→Controller get；process-restart durable resume仍未完成，FileResume feature继续reserved。

- **2026-09-04** — V5a-1a Host put state machine DONE：新增`file_transfer` PutBegin/Chunk/Status/Finalize/Cancel与Receiving/Ready/Completed phase；membership-owned HostFileTransferService在signed write grant下持有同目录secure part、durable chunk checkpoint/prefix SHA、final SHA pre-write gate、atomic conflict-policy finalize与Cancel。wire **5/5**、Host **2/2**、workspace **204/0/6**。下一块V5a-1b Controller-owned put task/reconnect resume。

- **2026-09-04** — V5a-0 single-file manifest/chunk foundation DONE：复用V3d TransferId/resume descriptor，新增16KiB manifest与deterministic 4–32KiB chunks，per-chunk/final SHA，Base64 pre-decode与48KiB chunk-doc hard gates；Controller local path永不进入peer manifest，get/put权限明确映射现有read/write grant。focused **4/4**、workspace **201/0/6**；FileResume feature仍未实现，下一块V5a-1 single-file data plane。

- **2026-09-04** — V4c HTTP CONNECT DONE / **V4 Dynamic Networking DONE**：Controller-owned loopback CONNECT adapter复用V4a TCP primitive；16KiB header/5s handshake，只CONNECT，GET=405，支持`host:port`/`[IPv6]:port`。independent review增加initial-tail pump，真实single-write `CONNECT+CLEW-V4C-PIPELINED`经Gamma echo返回200+exact proof；default Site add Denied，remove后port-close。exact binary `92545358...31835`，workspace **197/0/6**。下一阶段V5 File Plane。

- **2026-09-04** — V4b SOCKS5 TCP CONNECT DONE：Controller-owned loopback SOCKS5 v5/no-auth adapter复用V4a `open+pump` primitive，不复制TCP data plane；仅CONNECT，UDP ASSOCIATE明确拒绝。exact binary `4cf27090...e139` cap00→Gamma raw `METHOD=5,0 / REP=0` + echo `CLEW-V4B-SOCKS5-PROOF` PASS，default Site add Controller-side Denied，remove后port-close；workspace **195/0/6**。下一块V4c HTTP CONNECT。

- **2026-09-03** — V4a-1 Controller-owned bounded TCP forward DONE / V4a closed：新增ForwardId/ConnectionId与InnerSession `tcp_forward` Open/Exchange/Close，12KiB chunks、Host/Controller 64上限；Host改为pre-connect semaphore admission。Controller只绑loopback且持有listener，CLI exit不拆台；每条连接固定session generation，断线旧TCP EOF、不迁移，新connection在新generation重新Open。exact Windows binary `fec92a99...8f5ca` cap00→Gamma真实echo、default Denied、remove port-close、generation 1→3 reconnect全PASS；workspace **193/0/6**。Forward feature现可协商，Socks5/HttpConnect仍false；下一块V4b SOCKS5 TCP。

- **2026-09-03** — V4a-0 signed TCP egress authority DONE：PermissionGrant新增serde-default `tcp_egress=false`与full execute ceiling，owner CLI仅显式 `--allow-tcp-egress`可签出，GUI/legacy/default均false，helper-only继续剥离。真实mint对照确认默认false、opt-in true且write/shell不被顺带打开；grant **3/3**，workspace **188/0/6**。下一块 V4a-1 bounded TCP forward。

- **2026-09-03** — V3f wire/capability compatibility matrix DONE / **V3 Reliability DONE**：feature语义拆成peer advertisement、current-build implementation、bilateral negotiation三层；`IMPLEMENTED_FEATURES`当前只含ToolRpc+ShellTask，Forward/Socks5/HttpConnect/FileResume即使双方Hello都广告也不协商。capability_version 1↔999不推断feature，unknown 999保真但忽略；wrong WIRE_MAJOR硬拒绝；frame/concurrency negotiated limit取双方valid bounds的min。proto **11/11**、workspace **187/0/6**。V3七项全部封板，下一阶段V4a bounded TCP forward。

- **2026-09-03** — V3e sleep/resume continuity DONE：Controller新增15s wall-clock `SessionContinuity`，idle heartbeat/业务send/progress/reply都拒绝长pause后的旧InnerSession，heartbeat/progress missed tick改为Skip；Host Shell timeout与30s reconnect grace改为wall-clock deadline（250ms poll），clock regression fail closed。focused continuity **1/1**、wall deadline **1/1**、V3c reconnect **4/4**、V3b replay **1/1**，no-public Connector **1/1（3.57s）**。Windows真实runtime-pause gate在同一脚本中精确观测隔离Controller **generation 7 → suspend 20.039s → generation 8**；Host-pause诊断因时序归因不稳定未计正式PASS。workspace **183/0/6**。下一块 V3f compatibility matrix。

- **2026-09-03** — V3d File resume interface reservation DONE：复用既有proto `FEATURE_FILE_RESUME=6`且保持`CAPABILITY_VERSION=1`，新增显式feature查询但当前生产不自动广告；新增stable `TransferId`与8KiB peer-visible `FileResumeDescriptor v1`，绑定Controller/Site/Device/direction/device path/size/revision/offset/prefix hash/final hash，Controller本机path故意留在future private state。successor validator冻结scope、revision/offset单调、same-offset prefix hash与known final hash；complete checkpoint必须有匹配full SHA-256。无文件I/O、无chunk/data plane/Local API/CLI/MCP。descriptor **4/4**、Feature 6 **1/1**、workspace **181/0/6**。下一块 V3e sleep/resume。

- **2026-09-03** — V3c-2 Controller TaskId reattach + per-epoch proof DONE / V3c closed：Controller projection跨generation保留原DeviceId并在30s内用Status proof重绑；independent review修复Start unknown-result orphan，新增Started→Status receipt后才向caller返回TaskId；真实跨机时序又收紧Host为每个new session epoch都必须重新Status/Attach proof，网络重连本身不再延长task。Host reconnect **4/4**、runtime **35/35**、no-public Connector **1/1（3.49s）**。cap00 Controller + mzd Host真实20s program-scoped firewall gate观测generation **3 disconnected → 4 connected（13.750s）**，原TaskId Status仍Running、Attach保留断线前26-byte stdout且`lost_prefix=false`、Cancel PASS；workspace **176/0/6**。下一块 V3d File resume接口预留。

- **2026-09-03** — V3c-1 Host Shell reconnect grace DONE：HostShellService从per-InnerSession提升到membership reconnect runtime；共享30s grace，session guard用epoch使grace内authenticated reconnect保留task，超grace取消；timer只持Weak，Host runtime/service drop仍立即cancel。focused grace **1/1**、service-drop **1/1**、no-public Connector **1/1（3.57s）**、workspace **172/0/6**。下一块 V3c-2 Controller TaskId reattach projection。

- **2026-09-03** — V3b-2b cross-generation replay + active-RPC liveness DONE / V3b closed：RemoteHub用 session-change watch等新 generation，Read/FsQuery/FsMutation跨 generation保持同 RequestId；mutation Host Timeout先同 generation查询 in-flight cache，session loss后继续同 ID，Shell Start零 blind replay。真实50k Glob hard-kill发现并修复 active RPC遮住 idle heartbeat：新增2s RequestId-only `rpc_progress`，Controller 5s progress deadline；修复后 hard-kill **6073ms** offline、generation **1→2**，原始同一 Glob进程无需重发即 exit 0并返回完整空 page。RPC progress **3/3**、synthetic replay **1/1**、no-public Connector **1/1（3.33s）**、workspace **171/0/6**。下一块 V3c Shell reattach。

- **2026-09-03** — V3b-2a Host mutation RequestId dedupe DONE：HostReadService增加128-entry process-lifetime replay cache，只存 fingerprint+bounded reply；同 ID同请求返回首次结果、同 ID异请求拒绝，in-flight worker跨 InnerSession继续完成并缓存，修复旧 `timeout(spawn_blocking)` 后副作用无人记录的歧义。dedupe **1/1 PASS**、mutation regression **1/1 PASS**、no-public Connector **1/1 PASS（3.62s）**、workspace **169/0/6**。下一块 V3b-2b Controller cross-generation replay。

- **2026-09-03** — V3b-1 stable RequestId / RPC correlation DONE：新增 RequestId 与 bounded `rpc_request/rpc_reply` envelope，Read/FsQuery/FsMutation/ShellTask 全部要求 reply id 精确匹配；heartbeat 保持独立。RPC focused **2/2 PASS**、no-public Connector full business chain **1/1 PASS（3.77s）**、workspace **168/0/6**。本块明确只做 correlation，不宣称 retry/dedupe/exactly-once；下一块 V3b-2 operation-specific replay + mutation dedupe。

- **2026-09-03** — V3a session generation + path telemetry + idle liveness DONE：RemoteHub token 升为 process-local generation；Direct iroh path event 只更新 direct/relay/mixed path、不触发 reconnect，Connector topology 不冒充 Target path。新增 Local API/CLI/MCP `session_path_info`，Devices disconnected 后投影 last_seen。真实 product hard-kill 首轮发现空闲 member 15s 仍误报 connected，修复为 connection-close watcher + E2E InnerSession 5s/5s encrypted heartbeat；修复后 hard-kill **686ms** disconnected，同一 DeviceId 重启 generation **1→2**，空闲 heartbeat 后 Read PASS。workspace **166/0/6**。下一块 V3b replay matrix。

- **2026-09-03** — V2e-2 MCP Streamable HTTP DONE / V2 Agent Minimum DONE：`clew mcp http` 复用同一 LocalApiClient/11-tool router，仅增加 loopback HTTP transport；非-loopback bind拒绝，128KiB body hard cap。independent review补显式 same-port Origin allowlist，结合 rmcp loopback Host validation抵御浏览器 cross-origin/DNS-rebinding；真实 HTTP initialize/tools/list PASS，allowed Origin 200、bad Host 403、bad Origin 403、140KiB body 413；HTTP adapter退出后 Controller仍存活并可 authenticated shutdown。workspace **165/0/6**。下一阶段 V3 Reliability。

- **2026-09-03** — V2e-1 MCP stdio DONE：接入官方 `rmcp 3.2.0`，`clew mcp stdio`只持有 LocalApiClient/shared selector，11个 typed tools覆盖 Devices + filesystem + live Shell；MCP可在 Controller缺席时拉起同一个 single-owner Controller且自身退出不拆 Controller。raw initialize/tools/list/devices smoke、real Host MCP Read、write+shell full surface sweep均 PASS；workspace **163/0/6**。下一块 V2e-2 Streamable HTTP。

- **2026-09-03** — V2d-1c Controller live Shell projection + Local API/CLI DONE：TaskId绑定 DeviceId+live session token，follow-up无 Device参数；global projection hard cap 8192。independent review修复 Start caller timeout/drop导致 orphan task的风险：RemoteHub internal completion继续接收 Started，caller已消失时自动 Cancel并清 projection，RAII reservation确保无 slot leak。projection/cancellation **2/2 PASS**，CLI **1/1 PASS**；Windows shell-capable product Start→Status→Attach + Activity plaintext-negative PASS，default Site Kit Controller-side Denied PASS；workspace **161/0/6**。V2d DONE，下一块 V2e MCP。

- **2026-09-03** — V2d-1b Host live-session Shell core DONE：`HostShellService` 按 active InnerSession 持有 process/task store；cwd canonical-root 仅约束初始目录（明确不是 filesystem sandbox），`env_clear` + minimal baseline + bounded explicit env，stdout/stderr 32KiB rings，timeout/cancel/session-drop cancellation。independent review 将 64-task capacity 从 post-spawn store check 收紧为 **pre-spawn semaphore admission**。Host focused **3/3 PASS**；no-public Connector 同一 InnerSession 扩展到 Shell Start/Status/Attach **1/1 PASS（3.51s）**；shell-capable same-kit connector-only 降权 **1/1 PASS（2.97s）**。下一块 V2d-1c Controller projection + Local API/CLI。

- **2026-09-03** — V2d-1a bounded Shell wire DONE：新增 stable `TaskId` 与 `shell_task Start/Status/Attach/Cancel` 短 RPC；command/cwd/env/timeout 全 hard-bound，stdout/stderr 设计为 32KiB ring + 12KiB/stream Base64 page，绝对 cursor + `lost_prefix` 明示丢前缀；phase 不包含任何 V3 reconnect/replay 语义。protocol **1/1 PASS**，workspace **155/0/6**。下一块 V2d-1b Host process/task store。

- **2026-09-03** — V2d-0 signed Shell authority DONE + Windows Mint parser blocker fixed：execute-preferred ceiling 扩为 read/write/shell/connector，但仍与 signed grant 相交；`allow_shell` serde-default false，CLI 仅显式 `--allow-shell` opt-in，GUI 继续 false。真实 signed kit 对照确认默认 `shell=false`、opt-in `shell=true` 且 write 独立。该 smoke 暴露 `Command::Mint` 巨大 variant 在 Windows Clap derive 下 stack overflow，拆为独立 `MintArgs` 后 `mint --help`/真实 mint 恢复 PASS。workspace **154/0/6**。下一块 V2d-1 live-session Shell task core。

- **2026-09-03** — V2c-1 bounded atomic text Write/Edit DONE / filesystem agent surface closed：新增 32KiB small-text `fs_mutation`，Write 强制 create-only 或 expected SHA-256，Edit 强制 expected SHA + unique old text；Host bounded existing-file read、canonical roots、same-directory secure temp + sync + atomic persist、permission preservation，Controller/Host/service 三层 write grant fail closed，legacy grant=None 不可写。真实 no-public Connector 同一 InnerSession `Read→PathInfo→Glob→Grep→Write→Edit→Read` PASS；write-capable same-kit connector-only 降权 PASS；Windows product positive Write/Edit/Read/Activity 与 default-read-only Denied negative 均 PASS；workspace **154/0/6**。下一块 V2d bounded live-session Shell task。

- **2026-09-03** — V2c-0 signed write authority projection DONE：新增 execute/read/write/connector ceiling（shell=false），owner CLI `--allow-write` 显式 opt-in，默认 CLI/GUI 仍 read-only；Controller 继续以 signed grant intersection 为权威，Host membership 新增可选 full effective grant，新 enrollment 持久化 receipt，legacy JSON 缺字段解析 `None` 并对 mutation fail closed。focused grant **2/2**、membership **1/1**、workspace **151/0/6**；下一块 V2c-1 atomic text Edit/Write。

- **2026-09-03** — V2b-2 bounded Grep DONE / read-only filesystem surface closed：在同一 `fs_query` RPC 增加 streaming Grep，`regex::bytes` + explicit 1MiB/2MiB compile budgets，16KiB line / 32MiB scan / 48KiB reply / 1024 item hard bounds；目录 traversal 继续 canonical-root containment + visited 去重，内容用 `fill_buf/consume`，无 unbounded full-file/line read。focused protocol/Host/CLI 与真实 Connector `Read→PathInfo→Glob→Grep` 均 PASS；Windows product smoke `clew grep` 返回正确 `.rs` matches 并写 Activity；workspace **150/0/6**。下一块 V2c atomic Edit/Write。

- **2026-09-03** — V2b-1 bounded PathInfo/Glob DONE：新增 `fs_query` InnerSession RPC 与 DeviceId-only Local API/CLI adapters；PathInfo/Glob 全复用 ReadPolicy canonical roots/timeout，Glob 加 deterministic BFS、cursor、100k scan-entry/1024 item/pattern/byte hard bounds，并对 junction/reparse 目录做 canonical-root containment + visited 去重。transport 与 Controller 双重校验 reply byte budget。focused protocol、Host、CLI、真实 Connector `Read→PathInfo→Glob` 均 PASS；下一块 V2b-2 bounded Grep。

- **2026-09-03** — V2a shared selector + bounded Read adapter DONE：将 `select_executable_device` / `DeviceSelectionError` 从 Host 提升到 `clew-core`，保留 Host compatibility re-export；`clew read` 从 DeviceId-only 扩展为 DeviceId / `Site/Device` / unique short name，并支持单 operand 时唯一在线 executable device 自动选择。Local API/wire 仍只吃 DeviceId，helper-only 与 ambiguity fail closed。focused selector **3/3**、CLI **1/1**，workspace **145/0/6**；下一块 V2b PathInfo/Glob/Grep。

- **2026-09-03** — V1.5 DONE / formal physical no-public gate closed：exact `7f01cb8` tracked source 在 3dv0 构建 Linux binary `eb464aed...bb4b6`，同一 ELF 部署到 delta/dnk/3dv0；delta Linux Controller 签同一 `site.clew`，dnk helper-only production enrollment/export/lease-renewal PASS。3dv0 Target 放入 rootless `pasta` + nft process-scoped no-public namespace，只允许 UDP→dnk private Helper IP；无 Nearby negative 26s 保持 pending、433 packets rejected；fresh Nearby positive 约 8s Active，约 51s 内 **6/6 online + 6/6 Read PASS**，最终 951 packets rejected。纯 mDNS physical diagnostic 仍受当前 multicast 环境限制，因此 zero-input mDNS 证据继续由已有 real-mDNS integration 承担，不把 fallback gate 冒充 mDNS。V1.5 正式封板，下一块 V2 Agent Minimum。

- **2026-09-03** — V1.5 final physical sweep / clock-skew hardening：真实 gamma Helper 揭示 Controller/Helper 约 6 秒系统时差会让 fresh Connector lease 永久 `NotYetValid`；增加 bounded **30s not-before-only skew**，expiry/signature/Site/Endpoint/Device/max-lifetime 均保持 fail closed。真实 candidate 在 gamma 约 4 秒生成 production Nearby export；workspace **143/0/6**。同时通过 imini/gamma/witand0/cap00 物理探针确认当前剩余 gate 是上游 peer-UDP ACL/隔离；gamma host firewall 完全关闭仍 UDP timeout。下一步只换受控 UDP-direct 拓扑，不重开协议。
- **2026-09-03** — V1.5c-3 nearby fallback DONE：commit `c4fbae6` 增加 bounded/versioned `nearby-connection.clew`、Controller-signed historical route binding、per-Site import/export、direct+mDNS+fallback race 与 Host drag/drop/export UI；cap00 workspace **134/0/4**，两条 explicit multicast 与 Windows GUI 分别 1/1 PASS。exact source 在已知 mDNS 不可用的 `macos-3dv0` 上 no-mDNS enrollment+Read **1/1 PASS**、workspace **135/0/3**、Aqua Host/tray >15s + second-launch wake PASS。下一块 V1.5d multiple-helper failover/resume。

- **2026-09-02** — V1.5a discovery/opaque-transport spike DONE：确认 iroh 1.1 的 mDNS 已拆到 `iroh-mdns-address-lookup 0.5.0`；落地 same-Site bounded candidate tag、direct-IP-only mDNS、Controller-signed 10-min ConnectorLease、16 KiB outer preface 与 `clew/connector/1` opaque pump。real mDNS wrong-Site filter + EndpointAddr connect PASS，real Target→Helper→Controller Noise/Read plaintext-negative tap PASS；workspace 121/0/2。Controller Connector runtime 继续 fail closed，下一块 V1.5b sealed bootstrap/runtime integration。
- **2026-09-02** — V1.25 DONE：exact `073acc0` source 在 `macos-3dv0:scratchpad` 完成 native fmt/check/workspace **115/0/1**，Aqua Controller Studio + imported app/tray/logo/key-visual Host 真运行、second-launch wake、Read、asset hash、删除整个 Site Kit 后同 DeviceId branded restart/再 Read 全 PASS。Windows + macOS desktop gate 至此闭合，下一块进入 V1.5 discovery spike。
- **2026-09-02** — V1.25b-2 implementation complete / macOS gate pending：Controller GUI 新增 Local-API-only Outfit Studio、单 revision batch edit、bounded PNG/SVG thumbnail/live preview；Host runtime 真正消费 imported window/tray/logo/key-visual + primary accent。Windows `studio-smoke` Studio GUI、network branded Host、second-launch wake、Read、删除整包后的同 DeviceId branded restart/再 Read 全 PASS；workspace 114/0。下一步只做 exact commit macOS native/Aqua gate。
- **2026-09-02** — V1.25b-1 asset distribution DONE：新增 Controller-owned bounded PNG/SVG content-addressed store、asset Local API/CLI、asset revision binding、deterministic build/cache key、`invite` sibling asset export 与 Host signed-hash state cache；真实 asset-lab Site Kit enrollment/Read + 删除整个 kit 后同 DeviceId 无 sidecar restart/再 Read PASS。workspace 109/0，下一块 V1.25b-2 Studio GUI/editor/live preview。

- **2026-09-02** — V1.25a foundation DONE：新增 bounded OutfitProfile/四 preset、Controller-owned 双槽 OutfitLibrary、`clew outfit` Local API/CLI、`clew invite --outfit`、signed OutfitProfile→ClientFlavor→Host runtime→membership recovery vertical slice；真实 `huang-lab` Site Kit enrollment/Read 与无 sidecar 同 DeviceId 重启/再 Read PASS。workspace 102/0，下一块 V1.25b Studio GUI/assets/preview。
- **2026-09-02** — V1 desktop copy 改为 English/ASCII：修复 macOS `eframe/default_fonts` 下 CJK 方框；`c56ee98 fix: use English desktop copy for v1` 已在 `macos-3dv0:scratchpad` exact native check/build/workspace 93/0/1 与 Controller/Host GUI/tray second-launch wake 上复验。

- **2026-09-02** — V1.4 DONE / V1 release gate closed：在 `macos-3dv0:scratchpad` 完成 macOS 15.7.9 x86_64 原生 build/test 与 Aqua Controller/Host tray smoke。首轮测试发现并修复 macOS Unix socket `SUN_LEN` 深路径缺陷；修复后 macOS workspace 93/0/1、Host enrollment/online/second-launch wake/Read 全通过。Windows、Linux、macOS 三平台 V1 acceptance 至此闭合，下一块解锁 V1.25。

- **2026-09-02** — V1.4 release-gate closure 前进：gyz Linux x86_64 对 `4a66937` 精确 source archive 完成 native build，并真实跑通 Linux Controller + foreground Host enrollment/InnerSession/bounded Read、root-policy denial、single-instance wake；随后 live revoke 使当前 session offline，持续 reconnect 与同 DeviceKey restart 均无法恢复权限。release blocker 收窄为仅 macOS tray 真机 smoke。

- **2026-09-01** — V1.4 implementation complete / release gate BLOCKED：ControllerControlStore、network enrollment + half-commit recovery、long-lived Host iroh endpoint、bounded Read、Activity、control-plane Local API/CLI、human bootstrap errors、backup v2/Recovery Review 与 Controller GUI V1.4 surface 已落地；cap00↔Gamma 双机 Read/identity-reuse/backup recovery 真实 PASS；仍缺 live revoke 与 macOS/Linux 真机证据，因此不发布 V1、不进入 V1.25。
- **2026-09-01** — V1.3 DONE：新增 `clew-transport`、iroh 1.1 direct/public-relay outer、Noise XX InnerSession、HKDF-separated Device transport key、Ed25519 transcript/context binding、bounded encrypted framing、replay/corruption/I/O poison 与 outer plaintext tap；下一块冻结为 V1.4。
- **2026-09-01** — V1.2 DONE：新增 `clew-host`、signed `site.clew`/ClientFlavor、fixed sidecar recovery、OS-user membership reuse、Host single-instance wake、stable DeviceTag naming、helper-safe selector、Windows Host GUI/tray 与 Linux foreground；下一块冻结为 V1.3。
- **2026-09-01** — V1.1 DONE：新增 `clew-identity`、Ed25519 Controller/Device identity、signed bootstrap/enrollment、half-commit recovery、Argon2id + XChaCha20-Poly1305 backup skeleton，并把稳定 ControllerId 接入 runtime/Local API；下一块冻结为 V1.2。
- **2026-09-01** — V0.3 DONE：新增 Controller GUI/tray shell、GUI auto-start + Local API adapter、authenticated `controller.shutdown` 与 Windows desktop runtime smoke；下一块冻结为 V1.1。
- **2026-09-01** — V0.2 DONE：新增 `clew-runtime`、跨平台 single-owner file lock、Windows named pipe/Unix UDS Local API、local auth + bounds、CLI client 化与真实 Windows 跨进程 crash-recovery smoke；下一块冻结为 V0.3。
- **2026-09-01** — V0.1 DONE：建立 `clew-core` / `clew-proto`、稳定 ID/DeviceTag/state layout/proto skeleton 与 20 个边界测试；下一块冻结为 V0.2。
- **2026-09-01** — 建立 Architecture v1.5 正式开发计划；登记 P0 基线维护；下一块冻结为 V0.1。
