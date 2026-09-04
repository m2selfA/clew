# Advanced Service Runtime

Clew V7 的 Service Runtime 是**显式 opt-in** 的长期在线能力，不改变默认 portable / tray / foreground 使用方式。V7a 只实现 Linux `systemd --user`；Windows Service 与 Linux system service 属于后续 V7b/V7c。

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

`--scope user` 当前是唯一实现的 scope，因此可省略。

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

## 后续 V7

- **V7b Windows Service**：machine-level runtime + 用户 session 中的 GUI/tray Local API client；不把 UI 放进 Session 0。
- **V7c Linux system service**：专用低权限 service account 与 machine state scope。

machine-level service 不得静默复用 portable/user DeviceKey，也不得因为 service account 权限较高就自动扩大 Shell/File/EXECUTE policy。相关总原则见 `01-design.md`、`03-gui.md` 与 `06-gaps.md`。
