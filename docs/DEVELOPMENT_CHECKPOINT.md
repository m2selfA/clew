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
| V0.3 | Controller GUI shell | DONE | GUI 空列表/ready 状态来自 Local API，不直接持有网络状态 |
| V1.1 | ControllerKey / DeviceKey / enrollment | DONE | signed bootstrap、持久 DeviceKey、claim/半提交边界有测试 |
| V1.2 | Site Kit / Host lifecycle / naming | DONE | sidecar recovery、identity reuse、DeviceTag、Host 单实例与基础 UI |
| V1.3 | Direct iroh + InnerSession E2E | DONE | Controller/Target 双向认证，业务 payload 全部在 inner ciphertext |
| V1.4 | Bounded Read + v1 control plane | BLOCKED | 实现完成；Windows 双机 Read/Activity/backup 已闭环，仍缺 live revoke + macOS/Linux 真机 release 证据 |
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

**Status：BLOCKED（implementation complete；V1 release gate 仅缺 macOS tray 真机证据）**

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
- [ ] **macOS tray 真机证据缺失**：当前 WebCodex runner inventory 中没有 macOS path/runtime；Windows tray 与 Linux foreground 均已有真实 smoke，但不能替代 macOS。V1.3 的 Android cross-target 仍受缺失 NDK C toolchain 阻塞。

Validation evidence：

- `cargo fmt -- --check`：PASS；
- `cargo check --workspace --all-targets`：PASS，0 warnings；
- `cargo test --workspace --all-targets`：PASS，**92 tests passed / 0 failed / 2 ignored**；ignored 为 interactive Windows Host GUI smoke 与 public n0 relay smoke，两者此前均有单独真实 PASS 证据；
- V1.4 真实 Windows Controller GUI runtime smoke：PASS；最新 GUI process 保持运行超过多个 refresh 周期，Local API `ready=true`，随后 Controller graceful shutdown、GUI job stop 与临时 state cleanup 完成；
- V1.4 Linux/gyz native runtime：PASS；Host DeviceId `97386e1a-…` online 后真实读取 `/home/shark/tmp/rabisu/scratch/clew-v14-linux-e2e/share/proof.txt` 返回 33 bytes；同 root 外 `outside.txt` 被 Host policy 拒绝，Activity 分别记录 `succeeded/33 bytes` 与 `denied/0 bytes`；第二 foreground Host 只 wake 现有实例；
- V1.4 Linux live revoke：PASS；revoke 后当前 session 被移除、catalog 立即不可执行，持续 reconnect 与同 host-state restart 均不能恢复旧 DeviceKey 权限。此前 Windows smoke 的安全拦截因此不再是产品 release blocker；
- 初次 final workspace test 曾暴露 legacy non-networked Site Kit 会在拿到 Host instance 后立刻 `MissingNetworkConfig` 退出；已改为“endpoint/read policy 同时缺失 → 保持 AwaitingEnrollment；只缺一项 → fail closed”，`host_cli` focused regression 与同一 final workspace test 均转绿；
- cap00/Gamma smoke 临时目录均确认删除。早期 mzd smoke 在发现并修复半提交/endpoint lifecycle 问题后，mzd WebCodex agent 掉线；后续两次 SSH cleanup 都在环境层 timeout，因此 `D:\\tmp\\clew-v14-smoke` 的最终清理状态**未能验证**，不计为 Clew correctness PASS。

Known / release blockers：

- V1.4 代码实现面、真实 live revoke 与 Linux foreground acceptance 已闭合；当前仅因 **macOS tray 真机 smoke** 仍缺，**V1 暂不标记为对外可用**；不得提前进入 V1.25 并把此 gate 遗留；
- V2 才引入通用 cancel token/agent tool plane；V1.4 Read 已有 bounded timeout 与任务取消传播，但不提前设计 V2 的跨工具 cancel protocol；
- commit subject：`feat: implement v1.4 bounded read control plane`。

完成上述 release blockers 后，才把 **V1** 标记为可对外试用。

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

**Current block：V1.4 — Bounded Read + V1 Control Plane（BLOCKED：implementation complete）**

**Next block：V1.4 release-gate evidence closure（macOS tray smoke only）**

V1.4 已完成 Controller-owned control state、真实 network enrollment、长期 InnerSession、bounded Read、Activity、rename/invite-close/revoke、backup/restore/Recovery Review、GUI/CLI Local API surface，以及 Windows 双机/GUI、Linux foreground 与 live revoke 真运行闭环。当前不进入 V1.25：只剩 macOS tray 真机 smoke；拿到对应 runner 后补证据，之后才允许把 V1 标为对外可用。

### Change log

- **2026-09-02** — V1.4 release-gate closure 前进：gyz Linux x86_64 对 `4a66937` 精确 source archive 完成 native build，并真实跑通 Linux Controller + foreground Host enrollment/InnerSession/bounded Read、root-policy denial、single-instance wake；随后 live revoke 使当前 session offline，持续 reconnect 与同 DeviceKey restart 均无法恢复权限。release blocker 收窄为仅 macOS tray 真机 smoke。

- **2026-09-01** — V1.4 implementation complete / release gate BLOCKED：ControllerControlStore、network enrollment + half-commit recovery、long-lived Host iroh endpoint、bounded Read、Activity、control-plane Local API/CLI、human bootstrap errors、backup v2/Recovery Review 与 Controller GUI V1.4 surface 已落地；cap00↔Gamma 双机 Read/identity-reuse/backup recovery 真实 PASS；仍缺 live revoke 与 macOS/Linux 真机证据，因此不发布 V1、不进入 V1.25。
- **2026-09-01** — V1.3 DONE：新增 `clew-transport`、iroh 1.1 direct/public-relay outer、Noise XX InnerSession、HKDF-separated Device transport key、Ed25519 transcript/context binding、bounded encrypted framing、replay/corruption/I/O poison 与 outer plaintext tap；下一块冻结为 V1.4。
- **2026-09-01** — V1.2 DONE：新增 `clew-host`、signed `site.clew`/ClientFlavor、fixed sidecar recovery、OS-user membership reuse、Host single-instance wake、stable DeviceTag naming、helper-safe selector、Windows Host GUI/tray 与 Linux foreground；下一块冻结为 V1.3。
- **2026-09-01** — V1.1 DONE：新增 `clew-identity`、Ed25519 Controller/Device identity、signed bootstrap/enrollment、half-commit recovery、Argon2id + XChaCha20-Poly1305 backup skeleton，并把稳定 ControllerId 接入 runtime/Local API；下一块冻结为 V1.2。
- **2026-09-01** — V0.3 DONE：新增 Controller GUI/tray shell、GUI auto-start + Local API adapter、authenticated `controller.shutdown` 与 Windows desktop runtime smoke；下一块冻结为 V1.1。
- **2026-09-01** — V0.2 DONE：新增 `clew-runtime`、跨平台 single-owner file lock、Windows named pipe/Unix UDS Local API、local auth + bounds、CLI client 化与真实 Windows 跨进程 crash-recovery smoke；下一块冻结为 V0.3。
- **2026-09-01** — V0.1 DONE：建立 `clew-core` / `clew-proto`、稳定 ID/DeviceTag/state layout/proto skeleton 与 20 个边界测试；下一块冻结为 V0.2。
- **2026-09-01** — 建立 Architecture v1.5 正式开发计划；登记 P0 基线维护；下一块冻结为 V0.1。
