# Clew

一根穿过 NAT 的线：合作者打开一份预配置的 Site Kit 并双击，你的 agent 就可以在对方机器上读文件、改文件、跑命令、转发端口、走代理、传文件。人在 GUI 里点；agent 走本机 MCP。

Architecture v1.5 规划文档见 [docs/README.md](docs/README.md)。V0 与 V1.1 已完成 stable IDs/state/proto、Controller 单实例 + Local API/GUI、ControllerKey/DeviceKey、signed enrollment 与 encrypted backup skeleton；当前进入 V1.2 Site Kit + Host lifecycle/naming，随后继续 Target↔Controller InnerSession → bounded `Read`。已裁定边界与实施 checklist 见 [docs/06-gaps.md](docs/06-gaps.md)。

正式开发进度与下一开发块统一记录在 [docs/DEVELOPMENT_CHECKPOINT.md](docs/DEVELOPMENT_CHECKPOINT.md)；每完成一个 coherent block，都同步更新该文档后再进入下一块。
