# Distribution Studio：给分发包“穿搭”

本文定义 Clew Architecture v1.5 的 **Distribution Studio / 分发穿搭**：在不增加朋友操作步骤的前提下，让控制者可以方便地为 Site Kit 定制程序名称、图标、窗口标题、Logo、颜色、状态文案、托盘资源和说明文字，并把这些穿搭保存成可复用预设。

核心目标不是“做一个皮肤系统”，而是：

> 你花几十秒选好一套穿搭，之后 Alice、Bob、某个实验室的邀请都可以一键复用；朋友仍然只收到一个 Site Kit，双击即可。

## 1. 为什么值得做

Clew 的朋友端是一个会直接出现在别人桌面、任务栏/菜单栏和托盘里的程序。即使连接流程已经做到零输入，如果朋友看到的仍然是一个陌生的通用工具名和通用图标，仍会增加“这是什么、是不是发错了、能不能关”的心理摩擦。

分发穿搭要解决：

- 让朋友一眼认出“这是 CJ / 某实验室发来的协作工具”；
- 不要求控制者自己改源码、替换资源、跑签名命令；
- 不要求朋友在首次启动后再选择主题或输入组织信息；
- 同一套视觉贯穿主窗口、托盘、Site Kit 说明和聊天稿；
- 一个穿搭可复用到很多邀请，不把“换个 Logo”变成每次重新构建整个项目。

## 2. 调研借鉴

### 2.1 RustDesk Custom Client Generator

RustDesk 的 Custom Client Generator 把 **名称、Logo、图标、配置、签名**放到一个生成流程里，并把它作为大规模预配置客户端的推荐路径。

Clew 借：

- branding 与预配置一起生成，而不是让最终用户启动后自己设置；
- 生成器直接面向 Windows/macOS/Linux 等目标平台；
- 签名是 generator pipeline 的一部分。

参考：<https://rustdesk.com/docs/en/self-host/client-configuration/>

### 2.2 AnyDesk Custom Client Generator

AnyDesk 当前生成器把流程拆成 **General → Security → Visual → Finish Build**。Visual 阶段包含 in-app Logo、system tray icon 等，并在最后 review build。

Clew 借：

- Wizard 只暴露少量常用项，高级项折叠；
- “视觉”和“权限/连接策略”是不同页面/数据模型；
- build 前有最终预览，不让错误图标/文案直接进入签名产物。

参考：<https://support.anydesk.com/create-custom-client>

### 2.3 TeamViewer Custom QuickSupport / Host

TeamViewer 的自定义模块允许修改模块名称、application bar title、描述文字、Logo、按钮颜色等，并生成专门的下载模块/链接。

Clew 借：

- **窗口标题、欢迎/说明文本、按钮文本**应该是正式资源，不要散落在代码常量里；
- 面向不同合作对象可以保存多套 module/profile；
- “给朋友看的语气”本身也是品牌的一部分。

参考：<https://www.teamviewer.com/en-us/global/support/knowledge-base/teamviewer-remote/modules/quicksupport-and-custom-quicksupport/>

### 2.4 ISL Online Customization Dashboard

ISL Online 2026 的 Customizations dashboard 支持 create/edit/duplicate/reset/delete、设默认项，并能改 Logo、icon、key visual、颜色、应用名称、window title、executable name；颜色修改有实时 preview。

这非常接近 Clew 应有的体验。

Clew 借：

- **实时预览**；
- profile 可以 duplicate / reset / set default；
- visual 与 configuration 分开；
- 允许多个 customer/audience 使用不同 customization；
- 自动检查文字与背景对比度，而不是允许生成看不清的按钮。

参考：<https://www.islonline.com/whats-new/>、<https://help.islonline.com/37520/3868674>

### 2.5 Zoho Assist：一个品牌可以有多个受众版本

Zoho Assist 的 Quick Support Plugin 可以为不同 customer group 创建不同 branded versions。

Clew 借：

- 不只有一个全局 Clew 皮肤；可以为“实验室”“朋友”“机构项目”等保存不同 Outfit；
- 邀请时只选择一个已有 Outfit，不重新设计。

参考：<https://www.zoho.com/assist/articles/quick-support-plugin.html>

## 3. 数据模型：Outfit 与权限严格分开

正式名称建议：

```text
OutfitProfile       # 控制者保存的一套“穿搭”
ClientFlavor        # OutfitProfile 针对某平台构建并签名后的可复用客户端
SiteKit             # ClientFlavor + site.clew + 开始这里/说明
```

不要把穿搭和权限 profile 混成一份配置。

```text
PermissionProfile     research-full / read-only
        ×
OutfitProfile         clew-default / lab-clean / huang-lab
        ×
SiteInvite            Alice / Bob / Alice-Lab
        ↓
Site Kit
```

这样换颜色不会改变文件权限，换 read-only 也不会意外把 Logo 重置。

## 4. OutfitProfile 建议结构

```text
OutfitProfile {
    outfit_id
    revision
    display_name            # 控制者侧名称，例如 Huang Lab Light
    base_preset

    identity {
        app_display_name     # 朋友看到的程序名，例如 Huang Lab Connect
        window_title         # 主窗口标题
        helper_window_title? # 可选，默认继承
        publisher_label?     # UI/About 中显示，不伪造签名证书主体
        artifact_name_template
    }

    visuals {
        app_icon
        tray_icon_base?
        logo?
        key_visual?

        primary_color
        secondary_color?
        surface_style        # light / system，v1 默认 light
    }

    strings {
        locale_default
        locale_fallback
        resources_by_locale
    }

    distribution_copy {
        start_here_title
        start_here_body
        chat_message_template
        support_contact?
    }
}
```

`OutfitProfile` 不允许包含：filesystem roots、Shell permission、Controller secret、SiteBootstrapPass、DeviceKey 等安全/身份字段。

## 5. 可以定制什么

### 5.1 程序身份

常用：

- 程序显示名；
- 主窗口标题；
- Site Kit 文件名模板；
- About/说明中的组织名；
- helper-only 窗口标题是否使用同一品牌名。

高级：

- Windows `ProductName` / `FileDescription` 等 version resource 显示字段；
- macOS `CFBundleDisplayName` / `CFBundleName`；
- 平台 bundle/package identity 由 builder 管理，不让普通 GUI 用户手填容易冲突的 identifier。

### 5.2 图形资源

允许上传：

- App icon；
- 窗口 Logo；
- 可选 key visual / header illustration；
- 可选 tray/menu-bar base icon。

Generator 自动派生：

- Windows 多尺寸 `.ico`；
- macOS app icon 资源；
- Linux desktop/AppImage 所需 icon；
- 托盘绿/琥珀/灰/红状态变体。

**状态语义不允许被品牌完全覆盖。** 控制者可以换基础图形，但 GREEN/AMBER/GRAY/RED 的可辨识状态必须保留。优先通过 badge/dot/overlay 自动生成，而不是要求用户上传四套状态图。

### 5.3 颜色

v1 暴露少量 design tokens：

```text
primary
secondary?   # 可选
surface      # 默认跟 Clew light palette
text         # 通常自动
```

不要开放几十个 CSS token。

Builder 自动做：

- 对比度检查；
- 主按钮文字自动选深/浅色；
- warning/error 不随品牌色被改成含义相反的颜色；
- dark logo 放到浅底、浅 logo 放到适合的 header surface 时给 preview warning。

### 5.4 字符串资源

把朋友端所有可见字符串都改成 resource key，而不是硬编码：

```text
app.connected_title
app.connected_body
app.reconnecting_title
app.paused_title
app.helper_ready_title
app.helper_count

tray.connected
tray.reconnecting
tray.pause
tray.resume
tray.show
tray.copy_status
tray.exit_disconnect

button.hide_to_tray
button.exit_disconnect

first_run.keep_running_hint
site.start_here_title
site.start_here_body
site.extract_first_title
site.extract_first_body
invite.missing_title
invite.missing_body
invite.choose_file
access.revoked_title
access.revoked_body
controller.original_unreachable_title
controller.original_unreachable_body
```

默认提供 `zh-CN` / `en-US`，以后可扩展。

朋友机器按系统 locale 自动选语言；缺少 key 时回退到 `locale_fallback`，再回退到 Clew built-in English，绝不能因为自定义语言缺一条资源导致 UI 空白。
这些“恢复/撤销”字符串允许品牌化语气，但**语义不可删除或改反**：缺 `site.clew` 必须明确告诉用户需要邀请文件；被控制者 revoke 必须明确说“这台电脑的连接已被对方停止”；原 Controller 身份不可达时不能把它写成“已连接到新的控制者”。

### 5.5 不建议 v1 开放的“穿搭”

- 自带任意字体文件：涉及字体许可、体积和平台渲染差异；v1 用系统字体；
- 任意 HTML/CSS 注入主 GUI；
- 自定义脚本；
- 任意更改连接状态含义；
- 把“退出并断开”改成语义模糊的词；
- 伪装成第三方知名软件的名称/图标。

## 6. Distribution Studio GUI

Controller 主窗口增加一个入口：

```text
[ 邀请合作者 ]   [ 分发穿搭 ]   [ Agent ]
```

也允许：

```text
clew studio
```

直接打开同一个 Distribution Studio 窗口。

### 6.1 首页：穿搭库

```text
分发穿搭

默认
  Clew Original                    [默认]

我的穿搭
  Huang Lab Light                  最近使用
  Friendly Minimal
  Institute Blue

[ 新建穿搭 ]
```

每张卡片支持：

- 使用；
- 编辑；
- 复制；
- 设为默认；
- 导出；
- 删除（内置 preset 不删）。

借鉴 ISL Online：**复制一个现有穿搭再改**应比从空白开始更容易。

### 6.2 新建 Wizard

四步足够：

```text
1. 选预设
2. 名称与图标
3. 颜色与文案
4. 预览并生成
```

第 1 步先让用户看到可点击的大卡片，不先扔一个空表单。

### 6.3 预设

内置至少：

**Clew Original**
- 当前品牌；
- 最稳妥默认。

**Research Lab**
- 中性、干净；
- 默认标题“研究协作”；
- 文案偏“项目文件/计算任务”。

**Friendly Minimal**
- Logo 区缩小；
- 文案更口语；
- 非技术朋友最适合。

**Institution Clean**
- Logo / 机构名更突出；
- 适合正式合作项目；
- 颜色克制。

**Connector Helper** 不是独立 Outfit，而是任何 Outfit 的自动角色变体：同样 Logo/颜色，只替换 helper-specific 文案。

预设只是起点；可以一键“恢复预设”。

## 7. Live Preview 是核心能力

右侧或第二块区域始终显示实时预览。

必须至少预览：

```text
[ 主窗口 ] [ 连接助手 ] [ 托盘 ] [ Site Kit ]
```

### 主窗口预览

显示：

- App icon；
- window title；
- Logo；
- 已连接/重连状态；
- 主按钮；
- 字符串截断。

### 托盘预览

显示：

- normal/green/amber/gray/red icon；
- hover tooltip；
- 右键菜单；
- helper-only 变体。

这很重要，因为一个漂亮的 256px Logo 可能在 16–24px tray icon 上完全不可读。

### Site Kit 预览

显示：

- ZIP 名；
- `开始这里` 标题；
- “① 使用这台电脑 / ② 只帮助其它电脑连接”的品牌化入口；
- 聊天稿。

## 8. 资源导入必须“帮用户做对”

不要要求用户先自己制作所有平台格式。

### 8.1 Icon import

用户只拖一个高质量 PNG/SVG：

```text
拖入 logo/icon.png
        ↓
自动裁切预览
        ↓
检查透明背景 / 尺寸
        ↓
生成 Windows/macOS/Linux 所需资源
```

如果用户提供 SVG，builder 要先 rasterize 到平台尺寸并保留原始文件用于以后重新生成。

### 8.2 Logo import

自动给出：

- 推荐安全区域；
- 浅色/深色背景 preview；
- 太宽、太小、透明边距过多警告。

### 8.3 文案

每个字段旁边提供：

- 恢复默认；
- 从 preset 复制；
- 字符数/截断 preview。

不要求用户理解 resource key，普通 GUI 显示人话字段；“高级/翻译”页才显示完整字符串表。

## 9. 架构：两层定制，避免每份邀请都重新签名

这是最重要的构建约束。

### 9.1 Outfit build-time layer

会影响 OS 看到的程序资源：

```text
app icon
bundle/app display name
Windows VERSIONINFO
macOS Info.plist / app icon
可能的 executable/app name
```

这些生成 `ClientFlavor`：

```text
BaseRuntime
   + Outfit build resources
   + platform metadata
        ↓
pack
        ↓
sign / notarize
        ↓
ClientFlavor(platform, outfit_revision, runtime_version)
```

一个 `ClientFlavor` 可以复用很多次。

### 9.2 Invite runtime layer

邀请特有数据：

```text
site.clew
role hint
开始这里
聊天稿
```

不修改已签名的 app/exe：

```text
Signed ClientFlavor
      +
site.clew
      +
start-here assets
      ↓
Alice-Clew-Windows.zip
```

因此：

```text
换 Alice -> Bob
不重新签 ClientFlavor

修改 Huang Lab icon
-> Outfit revision +1
-> 各平台 ClientFlavor 重新构建/签名一次
-> 以后所有邀请复用新 flavor
```

## 10. Platform rules

### 10.1 Windows

Win32 本身支持 icon、string、version information 等 executable resources。Windows icon 需要多尺寸资源才能在不同 DPI/位置清晰显示。

Builder 在 Authenticode 签名前完成：

- app icon；
- VERSIONINFO：ProductName / FileDescription / CompanyName（如果配置）；
- manifest/package visual resources（适用时）；
- runtime UI resource bundle。

普通用户不接触 `.rc`、`rc.exe`、ICO 尺寸或 manifest。

### 10.2 macOS

macOS 的 user-visible app name 和 icon 属于 app bundle：

- `CFBundleDisplayName` / `CFBundleName`；
- `CFBundleIconFile` / app icon assets；
- `InfoPlist.strings` 可本地化 app display name 等资源。

因此 Outfit 修改这些资源后必须：

```text
assemble .app
  -> codesign
  -> notarize
  -> cache ClientFlavor
```

不能签完再改 Logo/icon/Info.plist。

### 10.3 Linux

优先支持 portable artifact 的：

- icon；
- app name；
- `.desktop` metadata（若该交付形式使用）；
- runtime strings。

Linux v1 headless 时仍保留 Outfit 数据模型，以免以后 GUI 再补时格式不兼容。

## 11. Outfit cache

构建键：

```text
FlavorKey = hash(
    runtime_version,
    outfit_revision,
    target_os,
    target_arch,
    signing_profile
)
```

Distribution Studio 显示：

```text
Huang Lab Light
Windows x64     已就绪
macOS arm64     已就绪
macOS x64       需要构建
Linux x64       未构建
```

邀请 Alice 时，如果目标 flavor 已存在：直接组 Site Kit。

如果不存在：生成器在你这边构建一次；朋友端步骤不增加。

## 12. CLI

GUI 是默认入口，但所有能力应可脚本化：

```text
clew outfit list
clew outfit new "Huang Lab" --preset research-lab
clew outfit clone clew-original "Project X"
clew outfit show huang-lab

clew outfit set huang-lab identity.app-display-name "Huang Lab Connect"
clew outfit set huang-lab visuals.primary-color "#2A6FBB"
clew outfit icon huang-lab ./huang-lab-logo.png

clew outfit strings export huang-lab --locale zh-CN > zh-CN.toml
clew outfit strings import huang-lab --locale zh-CN zh-CN.toml

clew outfit preview huang-lab
clew outfit build huang-lab --target windows-x86_64
clew outfit build huang-lab --target macos-aarch64

clew invite alice --outfit huang-lab --profile research-full
```

CLI 的低层 key 适合自动化；GUI 不让普通用户输入这些 key。

## 13. Import / Export

Outfit 可导出为一个不含秘密的配置包：

```text
huang-lab.clew-outfit
```

包含：

- schema/version；
- theme metadata；
- images；
- strings；
- preset ancestry。

**绝不包含** controller private key、enrollment secret、site.clew 或签名证书私钥。

用途：

- 换控制机；
- Git/Drive 保存品牌模板；
- 分享给同事；
- 从一个 Outfit 复制出项目变体。

## 14. 版本与更新

OutfitProfile 是稳定逻辑对象，修改形成 revision：

```text
Huang Lab Light
r1  蓝色 + old logo
r2  新 logo
r3  更新中文文案
```

已有朋友运行的 ClientFlavor **不因为你改 profile 就神奇变化**；新邀请默认使用最新 revision。

以后如果需要远程更新 runtime-level strings/color，可单独设计 signed Outfit Overlay，但 v1.5 不依赖它。

## 15. GUI 与 Invite 的整合

“邀请合作者”对话框新增一行，但不增加复杂度：

```text
外观：Huang Lab Light  [预览] [更换]
```

默认自动选择：

1. 最近使用；否则
2. 用户设为 default 的 Outfit；否则
3. Clew Original。

`更换` 打开穿搭卡片选择器；`编辑穿搭` 才进入 Distribution Studio。

所以日常邀请仍然是：

```text
称呼
权限
系统
外观（已有默认，通常不用点）
        ↓
生成
```

## 16. Friend-side invariant

无论穿搭怎么变，朋友侧产品语义不能变：

```text
双击
 -> 正在连接
 -> 已连接
 -> 可隐藏到托盘
 -> Exit 才真正断开
```

Outfit 可以改：

- “Clew”显示成“Huang Lab Connect”；
- Logo；
- 图标；
- 配色；
- 人话文案。

Outfit 不可以改：

- X 变成退出；
- 红/绿状态含义；
- 隐藏退出入口；
- 让 helper-only 看起来像会开放本机文件；
- 注入额外网络配置步骤。

## 17. 开发分期

### V0/V1 数据准备

从一开始就把 friend-facing strings、window title、logo/icon references 从硬编码中抽成 `UiResources/OutfitRuntimeView`。

### V1.25 — Distribution Studio foundation

在 V1 单机闭环后、V1.5 Site Connector 前或并行实现：

- `OutfitProfile` schema；
- 3–4 个内置 preset；
- Controller GUI 穿搭库；
- live Host/tray preview；
- PNG/SVG import；
- strings resource table；
- `clew outfit` CLI；
- `clew invite --outfit`。

### V6 — Release branding pipeline

与正式 signing 收口一起完成：

- Windows icon/version-resource build；
- macOS bundle name/icon/localization；
- signing/notarization；
- `ClientFlavor` cache；
- cross-platform preview/smoke。

## 18. 验收标准

1. 不改源码，在 GUI 中从 `Research Lab` preset 新建 Outfit，上传一个 PNG，修改程序名和主色，实时看到主窗口和托盘预览。
2. 点击生成后，Windows 分发程序的 app icon / 窗口标题 / Logo / tray tooltip 与预览一致。
3. macOS `.app` 的 Finder display name / app icon 与 Outfit 一致，且完成正常 codesign/notarization 流程。
4. 同一 Outfit 连续邀请 Alice、Bob 时，第二次不因为 Site 数据变化重新签 ClientFlavor。
5. 修改 Outfit 后生成新 revision；旧 ClientFlavor 不被静默覆盖，新邀请使用新 revision。
6. helper-only 自动使用同品牌的 helper 文案，不要求再建一套皮肤。
7. 中文/英文资源按系统 locale 切换；缺失 key 有稳定 fallback。
8. 自定义极浅/极深品牌色时，builder 能给出对比度 warning/自动文字色，不生成看不清按钮。
9. 自定义 Logo 在 256px 主窗口好看但 16px tray 不可辨识时，preview 明确提示，并允许单独提供 tray base icon。
10. Outfit export/import 不携带任何 enrollment/controller secret。

## 19. 一句话体验

控制者：

```text
选一个预设 -> 换 Logo/名字/颜色 -> 看预览 -> 保存成穿搭
```

以后邀请：

```text
选 Alice -> 直接沿用 Huang Lab Light -> 生成
```

朋友仍然只是：

```text
收到 Site Kit -> 双击 -> 已连接 -> 收到托盘
```

**穿搭增加的是识别感和亲切感，不增加朋友的操作数。**
