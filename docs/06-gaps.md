# Architecture v1.5 裁定与实施检查表

本文由 v1.4 的“文档审计与待完善”收口而来。这里不再提出新的产品愿景，而是记录 **已经裁定的边界、实现门禁和第一批 vertical slices**。除非显式开启新的 Architecture revision，下列决定不再在实现过程中重新 A/B。

## 1. 已裁定的架构边界

| 问题 | v1.5 裁定 | 实现门禁 |
|---|---|---|
| Connector 看不看业务明文 | **A 固定**：Target↔Controller `InnerSession` 端到端；helper 只搬 outer ciphertext | helper 日志/协议 payload 不能出现 Read/Shell/File 测试明文 |
| Site Kit 交付物 | 对朋友是一份**按平台生成**的分发包；signed ClientFlavor + `site.clew` | 不做 universal fat zip；不再承诺邀请焊进一个 exe |
| 多机设备名 | 邀请名是 Site 名；设备默认 hostname；碰撞组统一加固定 5 字符 DeviceTag，如 `GPU-01-K7M4Q`，可 rename | 不用 `(2)` 序号；tag 稳定、非秘密、不是认证凭据 |
| MCP 选机 | `Devices` 返回 Site/name/hostname/executable/connector；helper-only 不可执行 | 重名/多候选必须报候选，不静默挑第一台 |
| `site.clew` 丢失 | 显式打开/拖入 → 程序同目录 → 本机 membership → 缺失页 | 不扫全盘、不猜 cwd；有固定人话和“选择邀请文件” |
| 本机身份 | DeviceKey 在 OS-user state store；同 machine/user/site 复用 | 第二次双击只唤起已有 runtime，不新增 DeviceId |
| 撤销 | `device.revoke`、`invite.close`、`invite.revoke`、`site.revoke` 分开 | 关闭 bootstrap 不等于撤销已 enrollment 设备 |
| ControllerKey 丢失 | 提供加密 backup/restore；无备份就重新邀请 | 旧 host 不自动信任新的 ControllerId |
| 本机活动 | Controller 有 bounded local ActivityStore | 不上传；默认不保存文件正文/stdout 全文/env |
| 睡眠/合盖 | 不阻止系统睡眠；resume 自动重连/重新广播 | 只有收到 suspend 事件才显示“睡眠”，否则只说离线 |
| Linux 无托盘 | v1 foreground/headless，可观察状态 + Ctrl-C 退出 | 不隐藏成用户找不到的后台进程 |

## 2. Connector 威胁模型：A 已经锁定

v1.5 不接受：

```text
Target -- plaintext business session --> Helper -- plaintext business session --> Controller
```

只接受：

```text
Target ================================================ Controller
       authenticated + encrypted Clew InnerSession
          \                                          /
           outer link -> Site Connector -> outer link
                         opaque forwarding
```

第一条 direct `Read` 就必须跑在 InnerSession 上。V1.5 只新增 outer tunnel，不新增一套“helper 终止业务协议”的兼容模式。

Connector 最多看到 tunnel/routing id、包长、方向、时序、连接健康等外层元数据；不能得到业务解密 key，也不能解析 `StreamOpen.kind`、tool name、path、command、file bytes 或 stdout/stderr。首次 enrollment 经过 Connector 时使用 sealed-to-Controller envelope。

**如果 InnerSession 没完成，V1.5 Connector data plane 就没完成。**

## 3. 身份、命名与第二次打开

每台新 SiteMember 保存：

```text
DeviceId
SiteId
display_name
hostname_observed
enrolled_via_invite_id
capabilities { EXECUTE, CONNECTOR }
```

`Alice 实验室` 是 Site 名；`GPU-01`、`CryoEM-PC` 才是设备名。Controller 可 rename。重名时整个碰撞组使用 `hostname-XXXXX`：`XXXXX` 是由 DeviceId 派生的固定 5 字符 Crockford Base32 DeviceTag，例如 `GPU-01-K7M4Q`。不使用 `(2)`/`(3)`；tag 一旦分配就持久化，避免 selector 漂移。

DeviceKey 不跟 Site Kit 目录走。状态以 `(ControllerId, SiteId, OS-user scope)` 查找。相同 machine/user/site 再次打开同一包或升级包时复用 DeviceId；如果 runtime 已在运行，第二进程只恢复窗口。

U 盘“身份跟着盘走”的 portable identity **不在 v1**。

## 4. `site.clew` 和聊天分发事故

启动查找顺序已经固定：

1. 显式打开、拖入或 `--site` 指定的 `.clew`；
2. executable / `.app` 同级 `site.clew`；
3. 本机已 enrollment membership；
4. 缺失页。

首次缺失时固定说：

```text
还缺一个邀请文件。
请把 site.clew 和这个程序放在同一个文件夹，
或把 site.clew 拖到这里。
[ 选择邀请文件 ]
```

从压缩包临时目录运行时优先提示“请先全部解压这个压缩包，再打开程序”。邀请按目标 OS 分别输出 Windows/macOS/Linux 包；说明和聊天稿都写“完整解压，程序和 `site.clew` 放在一起”。

## 5. Controller 收口与灾难恢复

四种动作必须在 GUI/CLI/API 都可区分：

- **停止这台**：revoke 一个 DeviceKey；
- **停止继续加入**：只关闭 SiteBootstrapPass；
- **作废这份分发包**：关闭该 invite 的新 claim，并 revoke 通过该 invite 加入的设备；
- **停止整个 Site**：revoke Site 全部 member。

Controller backup 与 Outfit export 完全分开。backup 是敏感、加密、版本化的 Controller state；导出时设置独立备份口令。v1 restore 只恢复到空 state，不做 merge；restore 默认不重新打开历史 bootstrap pass。

恢复后进入 **Recovery Review**：远程接入先暂停，控制者 review 备份中的设备/Site/revoke 状态并显式恢复选中的设备。这样旧快照不会自动重新授权曾被后来撤销的设备。

没有 backup 丢失 ControllerKey 时，旧设备不会“自己找到新电脑”，只能重新邀请。

## 6. Agent 选机与本机 Activity

执行工具接受：

```text
DeviceId
Alice 实验室/GPU-01
GPU-01   # 仅在线 executable devices 中唯一时
```

helper-only 明确 `executable=false`，不能被 Read/Shell/File 选中。省略 device 只有在恰好一个在线 executable device 时才能自动选择。

Controller ActivityStore 本机记录：时间、Site/device、操作、路径/命令摘要、结果、耗时、传输字节。默认不保存正文、stdout/stderr 全文和环境变量；按时间/数量轮转，可过滤、可清空。

## 7. 睡眠、唤醒、锁屏和 Linux

Clew 不为了远程可用强制 keep-awake。锁屏不等于退出；合盖是否睡眠由 OS 决定。

收到 suspend event 时 Controller 可显示“睡眠”；没收到时只显示离线/等待连接。resume 后 host 重建 outer transport + InnerSession；Connector 刷新 endpoint/address 并重新广播。目标优先 failover 到其它 helper。

Linux v1：

```text
clew host --foreground
状态：已连接到 CJ · 当前空闲
Ctrl-C 退出并断开
```

可靠 tray 后续再加，不阻塞 v1。

## 8. Vertical slice 收口

```text
V0     Controller 单实例 + GUI 空列表 + Local API + stable IDs/schema skeleton
V1     per-platform Site Kit -> 双击 -> enrollment -> InnerSession -> bounded Read
       同时验收：hostname naming、sidecar recovery、identity reuse、device revoke、backup、Activity、Linux foreground
V1.25 Distribution Studio / Outfit preview（此前 Clew Original 即可）
V1.5  LAN discovery + sealed enrollment through Connector + opaque tunnel + failover
V2     Shell/Grep/Edit/Write + 完整 MCP adapter
V3     reconnect / task reattach / resume
V4     forward / proxy
V5     file plane
V6     release signing/notarization + immutable ClientFlavor cache + dual-launcher Site Kit assembly + Controller GUI distribution
V7     Advanced Service Runtime: Linux systemd --user -> Windows Service / Linux system service
```

**第一条对外可用版本仍是 V1。** Site schema 可以在 V0 占位，但 Connector 不阻塞 direct Read；Studio 也不阻塞 Read。

## 9. 实现时必须保留的 API/字段

最低检查项：

```text
controller.backup_export / backup_restore
device.rename / device.revoke
invite.close / invite.revoke
site.revoke
activity.list / activity.clear

DeviceSummary {
  device_id, site_name, display_name, hostname_observed,
  online, executable, connector, last_seen
}
```

DeviceRecord 保留 `enrolled_via_invite_id`，否则“作废这份分发包”无法精确收口。

## 10. 已登记但后置：Advanced Service Runtime

服务化不是取消，而是明确后置到 V7：

- Linux `systemd --user`：用户级、无需 root，优先实现；默认登录后常驻，boot-before-login 的 linger 必须显式开启；
- Windows Service：机器级 runtime + 用户态 tray/GUI Local API client；
- Linux system service：机器级、专用低权限 service account；
- machine service 默认更适合 Connector-only；EXECUTE 必须显式定义 execution identity 与 policy；
- portable/user DeviceKey 不静默迁移到 system scope；需要 Controller 批准迁移或新 enrollment；
- 安装、启用、停止、卸载都必须显式可见，不做隐蔽 persistence。

## 11. 暂时不要再展开

- 第二 transport / Directory / 企业 IdP；
- 远程桌面；
- 在 V1/V1.5 之前实现系统服务；Service Runtime 已登记为 V7 高级功能，但不得抢占 direct Read / Connector 主线；
- 任意 HTML 皮肤；
- portable identity on USB；
- 在 V1 Read 之前继续扩张愿景文档。

后续开发优先把 V0/V1 做成真实代码和两机验收；本文件作为实现 checklist 更新，不再重新讨论 A/B。
