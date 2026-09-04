# Advanced Service Runtime

Clew V7 的 Service Runtime 是**显式 opt-in** 的长期在线能力，不改变默认 portable / tray / foreground 使用方式。V7a Linux `systemd --user` 已完成；V7b 分成 Windows machine runtime 与 user-session control plane 两层，其中 V7b-1 machine Connector service 已完成，V7b-2 的 protected IPC / standard-user lifecycle backend / GUI+tray client 已实现并通过安全面验收，仍等待独立交互 Windows session 的可见窗口/tray acceptance；Linux system service 属于后续 V7c。

## V7a：Linux `systemd --user`

V7a 服务化的是 **Controller lifecycle owner**。Controller 仍使用当前 OS user 的同一份 state root、Controller identity、Local API、transport/session/task 状态；CLI/MCP 继续作为 Local API client。服务化只把进程保活责任交给 systemd user manager，不扩大 Site/DeviceKey/EXECUTE 权限。

入口保持浅层：

```text
clew service status --scope user
clew service install --scope user [--state-dir DIR]
clew service enable --scope user
clew service start --scope user
clew service stop --scope user
clew service disable --scope user
clew service uninstall --scope user
clew service enable-linger --scope user
clew service disable-linger --scope user
```

Linux user runtime 下 `--scope user` 可省略。Windows machine runtime 必须显式写 `--scope machine`，避免把 machine-level persistence 变成默认动作。

### 生命周期语义

- `install`：只写入 Clew-managed user unit 并执行 `daemon-reload`；**不会 enable、不会 start、不会改变 linger**。
- `enable`：只设置 `default.target` 登录后启动；不会立即 start。
- `start` / `stop`：只改变当前 user service 运行状态。
- `disable`：只取消后续登录自动启动；不会替用户开启/关闭 linger。
- `uninstall`：若正在运行会先 stop，若已 enable 会先 disable，然后删除 Clew-managed unit。
- `enable-linger` / `disable-linger`：是独立、显式的更高级动作。Clew 永远不会在 install/enable/start 中隐式调用它们。

默认 user-service 语义因此仍是“用户登录后由 user manager 常驻”。只有用户明确执行 `enable-linger` 后，才请求 systemd 允许该 user manager 在未登录时继续存在；是否获准仍由系统 policy / polkit 决定。

### Controller ownership

Clew-managed unit通过固定环境标记：

```text
CLEW_CONTROLLER_LIFECYCLE=systemd-user
```

启动普通 `clew controller --state-dir ...`。这个标记不是凭据，也不进入 Local API auth；未知值 fail closed。

service-owned Controller 的 `ControllerStatus.lifecycle_owner` 为 `systemd_user`。普通 `clew shutdown` 会返回 `Denied`，因为 Local API client 不应绕过 systemd 停掉长期在线 owner；请使用：

```text
clew service stop
```

systemd stop 使用 SIGTERM，Clew Controller 会走正常 graceful shutdown，释放 Local API endpoint、iroh transport 与 ownership lock。

### unit 安全边界

V7a unit：

- `Type=simple`；
- `Restart=on-failure`，正常 stop 不自动拉起；
- `UMask=0077`；
- `NoNewPrivileges=true`；
- `WantedBy=default.target`，不是 machine-level `multi-user.target`；
- `ExecStart` 和 state root 都固定为安装时解析出的绝对路径。

Clew **拒绝覆盖或卸载同路径的非 Clew-managed unit**。更新已有 Clew-managed unit 时，如果 `daemon-reload` 失败，会恢复旧 unit；新安装失败则删除刚写入的 unit并再次 reload，避免留下半安装状态。

`install` 会把当前 Clew executable 的 canonical absolute path 写进 unit。若之后移动了程序，或者切换到新的安装路径，应重新执行 `clew service install` 更新 unit；它不会偷偷搜索或改写 executable path。

### 状态查询

`clew service status` 返回 JSON，至少包含：

- `installed` / `managed`；
- `manager_available`；
- `enable_state`；
- `active_state`；
- `linger_enabled`；
- `unit_path` / `unit_name`。

查询本身不改变任何 lifecycle state。

## V7b-1：Windows machine Connector service

V7b-1 服务化的是**朋友侧长期在线 Connector Host runtime**，不是把现有 user Controller 塞进 Windows Service。入口为：

```text
clew service status --scope machine
clew service install --scope machine --site PATH\TO\site.clew
clew service enable --scope machine
clew service start --scope machine
clew service stop --scope machine
clew service disable --scope machine
clew service uninstall --scope machine
```

安装/卸载需要显式管理员权限。`install` 只创建 SCM service 与 machine state，不 enable、不 start；`enable` 只把 start type 改成 AutoStart，仍不立即启动；`start` / `stop` 只改变当前 service 运行状态；`disable` 只阻止后续自动启动；`uninstall` 删除 Clew-owned service 和 machine state。

### machine identity 与权限

Windows machine service：

- SCM account 固定为 `NT AUTHORITY\LocalService`，不是 LocalSystem；
- runtime 固定使用 `%ProgramData%\Clew\Service`，不读取或迁移 `%LOCALAPPDATA%\Clew` 的 user DeviceKey；
- install 把当前 Clew executable、已验证的 `site.clew` 与其引用的 Outfit/nearby 资源复制进 machine root，并写 `service.json` 绑定 binary/Site Kit SHA-256；
- enrollment 强制使用 `HostLaunchMode::ConnectorOnly`。即使原 Site Kit 允许 EXECUTE，也只申请 `connector=true / execute=false`，不开放 Read/Shell/File；
- uninstall 删除 machine identity/state；以后重新 install 会作为新的 machine member enrollment，而不是偷偷复活旧 user identity。

machine root 在任何 DeviceKey/Site Kit state 写入前就被收紧为 protected DACL。ACL 直接使用 Win32 security API写入并反验，不依赖本地化 `icacls` 文本或 service-account 名称解析；仅允许：

```text
SYSTEM
BUILTIN\Administrators
NT SERVICE\ClewConnector 的 service SID
```

三者均为 inheritable FullControl，DACL禁止从父目录继续继承。SCM同时把 `ClewConnector` 设为 `ServiceSidType::Unrestricted`，使运行进程token包含该独立 service SID；其它同为 LocalService 的服务不会因此得到 Clew machine state权限。

### lifecycle 与恢复

`ClewConnector` 是 `WIN32_OWN_PROCESS`，正常 stop 走 SCM Stop → graceful Host shutdown。异常退出配置有限 SCM recovery：2 秒、10 秒、60 秒三次 restart，1 小时后重置失败计数；之后不无限 crash-loop。

`start` / `enable` 会重新验证 SCM identity、protected DACL、binary/Site Kit hash。若 payload 被篡改，启动 fail closed。`stop` / `disable` / `uninstall` 只要求能证明该 SCM entry 与 metadata 属于 Clew，因此即使 binary/Site Kit 已损坏，管理员仍能停止和卸载，不会被坏安装锁死。

V7b-1 不创建任何交互窗口，也没有跨 Session 0 的普通用户控制 pipe；当前 machine lifecycle 由提升权限的 CLI/SCM管理。

## V7b-2：user-session control plane

V7b-2 不把 ProgramData machine state 或 DeviceKey 暴露给普通用户，而是把“安装时的本机用户”固化成唯一日常控制主体：

- install 从当前 process token 读取 installer user SID，并把它写入 machine metadata；SYSTEM / Administrators / service SID 都不能冒充这个 user SID；
- SCM service object 使用显式 DACL：SYSTEM 与 Administrators 保留 full control；authorized user只得到 `QUERY_CONFIG / QUERY_STATUS / START / STOP / INTERROGATE / READ_CONTROL`，没有 `CHANGE_CONFIG / DELETE / WRITE_DAC`；因此安装、重配、卸载仍要求管理员，而日常 start/stop/status 不需要再次提权；
- `%ProgramData%\\Clew\\Service` DACL**没有**加入 authorized user，所以 GUI/CLI不能读取 `service.json`、Site Kit、DeviceKey 或其它 machine state；
- Session 0 service另开 `\\.\\pipe\\clew-machine-control-v1`，不复用 Host wake pipe。pipe使用显式 `SECURITY_ATTRIBUTES`，只允许 SYSTEM / Administrators / service SID full access与 authorized user read/write，并继续 `reject_remote_clients(true)`；
- control API v1当前只暴露 bounded `Status`，16 KiB frame、2s I/O、最多7个active handlers+1个listener；生命周期 start/stop仍走 Windows SCM，不在自定义 RPC里重新实现 privileged service manager；
- user client在连接固定 pipe名后调用 `GetNamedPipeServerProcessId`，必须与 SCM当前 service PID一致，避免同名 pipe被普通进程抢占后冒充 service；
- runtime telemetry只投影 `starting / awaiting_enrollment / serving_connector / stopping`、Site name / DeviceId以及固定 `executable=false / connector=true`，不返回任何 machine secret；
- `clew service status --scope machine` 会把 SCM状态与 protected IPC telemetry合并；`clew service gui --scope machine` 是 user-session Eframe/tray client，后台线程只调用同一 service manager/status backend；
- GUI关闭按钮默认 hide-to-tray，明确显示“关闭/退出窗口不会停止后台服务”；安装永远仍是显式 administrator CLI动作，GUI不会静默安装或扩大权限。

安全验收使用 restricted token真实移除 Administrators membership后完成：同一 authorized user仍能查询 protected IPC并用生产 backend stop/start service，但 `CHANGE_CONFIG` / `DELETE` 均被SCM拒绝，读取ProgramData `service.json`也返回 Access Denied；重启后仍恢复 `serving_connector` 且不获得 EXECUTE。cap00 WebCodex agent本身位于 Windows Session 0，因此它只能验证GUI client进程与backend稳定共存，**不能**被计作可见桌面窗口/tray证据；V7b-2最终封板仍要求在独立 interactive Windows session完成该视觉/生命周期 acceptance。

## 后续 V7

- **V7c Linux system service**：专用低权限 service account 与 machine state scope。

machine-level service 不得静默复用 portable/user DeviceKey，也不得因为 service account 权限较高就自动扩大 Shell/File/EXECUTE policy。相关总原则见 `01-design.md`、`03-gui.md` 与 `06-gaps.md`。
