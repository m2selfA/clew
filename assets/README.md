# 图形资源

亮色调：米白底 `#F7F4EC`，线团金 `#E2B657` / `#C9963A`，线索蓝 `#7EB8D4`，字色 `#3D3A35`，成功绿 `#7BC47F`，中继琥珀 `#E0A45A`，离线灰 `#C4BEB4`。

## 品牌

| 文件 | 用途 |
|---|---|
| [brand/clew-mark.svg](brand/clew-mark.svg) | 主标（圆角方底 + 线团） |
| [brand/clew-mark-on-light.svg](brand/clew-mark-on-light.svg) | 透明底线团，铺在浅色上 |
| [brand/clew-wordmark.svg](brand/clew-wordmark.svg) | 横版字标 |
| [brand/palette.svg](brand/palette.svg) | 色板示意 |

## 应用图标

| 文件 | 用途 |
|---|---|
| [icons/app.svg](icons/app.svg) | 应用图标（windui WindowIcon / 打包） |
| [icons/tray.svg](icons/tray.svg) | 托盘 32px 简化 |
| [icons/status-direct.svg](icons/status-direct.svg) | 直连 |
| [icons/status-relay.svg](icons/status-relay.svg) | 中继（仍可打洞） |
| [icons/status-offline.svg](icons/status-offline.svg) | 离线 |

PNG 预览若存在，放在 `brand/preview/`（从设计稿导出，非源文件）。实现时优先 embed SVG。

## 使用

windui 可直接吃 SVG。Windows ico 由 CI 从 `icons/app.svg` 栅格化，不手绘多套位图。
