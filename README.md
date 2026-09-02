# Clew

一根穿过 NAT 的线：合作者打开一份预配置的 Site Kit 并双击，你的 agent 就可以在对方机器上读文件、改文件、跑命令、转发端口、走代理、传文件。人在 GUI 里点；agent 走本机 MCP。

Architecture v1.5 规划文档见 [docs/README.md](docs/README.md)。V0–V1.3 已完成 stable IDs/state/proto、Controller 单实例 + Local API/GUI、ControllerKey/DeviceKey、signed Site Kit + Host lifecycle/naming，以及 iroh direct/public-relay + Target↔Controller Noise InnerSession E2E；V1.4 的 Controller-owned control state、network enrollment、bounded `Read`、Activity、backup/Recovery Review 与 GUI/CLI surface 已完成，并已有 Windows 双机真实 Read、Linux foreground/live revoke，以及 macOS Aqua Controller/Host tray + Read 真运行证据。**V1 release gate 已关闭，当前可对外试用；V1.25 Distribution Studio 已封板：Outfit schema/library/CLI、`clew invite --outfit`、bounded PNG/SVG distribution、Controller Studio editor/live preview，以及 Host imported app/tray/logo/key-visual runtime projection均已通过 Windows + macOS desktop acceptance。V1.5 已完成 discovery/opaque-transport spike：same-Site mDNS candidate、Controller-signed helper lease 与 Target↔Controller InnerSession opaque forwarding 已用真实 iroh endpoint 验证；当前继续 sealed-to-Controller first enrollment 与 Controller/Host runtime integration。** 已裁定边界与实施 checklist 见 [docs/06-gaps.md](docs/06-gaps.md)。

正式开发进度与下一开发块统一记录在 [docs/DEVELOPMENT_CHECKPOINT.md](docs/DEVELOPMENT_CHECKPOINT.md)；每完成一个 coherent block，都同步更新该文档后再进入下一块。
