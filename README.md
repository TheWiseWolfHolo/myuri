<p align="center">
  <img src="docs/icon.png" width="96" alt="Myuri">
</p>

<h1 align="center">Myuri 缪里</h1>

<p align="center">
  选中内容，按住鼠标右键，挑一个助手。<br>
  一个原生的 Windows 桌面 AI 助手，用 Rust 和 GPUI 写成。
</p>

<p align="center">
  <a href="https://github.com/TheWiseWolfHolo/myuri/releases/latest"><img src="https://img.shields.io/github/v/release/TheWiseWolfHolo/myuri?label=下载" alt="下载"></a>
  <a href="https://github.com/TheWiseWolfHolo/myuri/actions/workflows/ci.yml"><img src="https://github.com/TheWiseWolfHolo/myuri/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/Windows-10%20%2F%2011-0078D4" alt="Windows 10 / 11">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="Apache-2.0"></a>
</p>

<p align="center">
  <img src="docs/panel.png" width="370" alt="助手面板">
</p>

## 它做什么

在任何程序里选中一段文字、一张图片或几个文件，按住右键不放，鼠标旁边弹出助手面板。
松手选中一个助手，它带着你选中的内容打开一个小窗口，流式给出回答，可以继续追问。

不用切换窗口，不用复制粘贴，也不依赖别的宿主程序。常驻内存约 60 MB。

## 功能

**唤出**

- 按住右键约 0.2 秒唤出面板，时长可调；短按和右键拖动照常工作
- 全局快捷键（默认 `Alt+Space`）
- 自动抓取选中的文字、图片和资源管理器里选中的文件，用完把剪贴板原样还原
- 全屏程序和指定程序里自动让路

**助手**

- 每个助手有自己的提示词、模型、思考强度、图标和出现条件
- 按内容类型筛选：有的助手只在选中图片时出现，有的只在选中文件时出现
- 用正则把匹配的助手排到最前；面板里支持拼音首字母搜索和数字键直选
- 自带翻译、润色、解释、总结、OCR、文档问答等七个助手，都可以改

**对话窗口**

- 流式 Markdown，支持追问、停止、复制、编辑任意消息、从任意一轮重试、删除单条消息以及一键清空上下文开新对话
- 模型分享思考过程时，折叠显示在回答上方，写明想了多久
- 模型选择器分为「对话」「生图」「语音」，按 `Ctrl+M` 快速开关。生成过程中也能直接切换模型和思考强度（不中断当前回复），在选择器里可直接收藏模型或设为全局默认、本助手默认、默认生图与默认语音
- 选生图模型后，输入框下方显示对应的尺寸、分辨率、质量和格式。选中的图片或 `Ctrl+V` 粘贴的图片可以做参考图，也可以在生成结果上点「改图」
- 麦克风听写进输入框，对话回复可以朗读或保存音频。选语音合成模型后，输入文字即可生成带播放按钮的音频卡片，音频保存在用户「音乐」文件夹的 `Myuri` 子目录
- 对话自动保存，在「历史」里搜索、重读、接着聊

**工具**

- MCP：本地程序（标准输入输出）和远程 HTTP 服务都能接，四种对话协议下都能调用
- 工具运行前先问你，可以只允许一次，也可以在这次对话里一直允许
- Skills：把带 `SKILL.md` 的文件夹放进技能文件夹，助手在需要时自己读取
- 每个助手单独勾选它能用的服务和技能

**渠道与模型**

- 四种对话协议：OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini
- Gemini 对话与生图支持 Vertex AI 服务账号登录
- 五种生图接口：OpenAI、Gemini、xAI、EvoLink、WaveSpeed
- 语音识别和合成支持 ElevenLabs、Soniox、OpenAI。ElevenLabs 可拉取音色，修改音色、语速和稳定度后直接试听草稿
- 对话、生图、语音各自保存渠道和密钥，三页可以使用相同的渠道 ID
- 一个渠道可以存多把密钥，各自备注和停用；出问题换下一把、轮流用、同一对话固定一把，三种策略
- 一键拉取渠道的最新模型列表。可以搜索、勾选或复制模型 ID，也可以追加新增模型或让列表与渠道一致
- 在设置「通用」页或命令行更新 models.dev 目录。在对话和生图渠道的「从 models.dev 同步」面板按厂商浏览模型，查看上下文、视觉、思考、工具调用和价格，再勾选加入或复制 ID
- 渠道和模型自动配上厂商图标，也可以自己指定图标名、图片或网址

**设置**

- 完整的设置窗口：新建、改名、删除、拖动排序、搜索、分组折叠
- 配置是几个带注释的 TOML 文件，手改也行，保存立即生效，写错了会提示哪里有问题
- 密钥只存在 Windows 凭据管理器里，不写进任何配置文件
- 备份与同步：整套配置导出成一个文件、从文件导入，或者放到 WebDAV 上；密钥要带的话用你设的密码加密

<p align="center">
  <img src="docs/settings.png" width="760" alt="设置窗口">
</p>

## 安装

1. 到 [Releases](https://github.com/TheWiseWolfHolo/myuri/releases/latest) 下载 `Myuri-windows-x64.zip`，解压到任意位置。
2. 运行 `myuri.exe`，托盘里会出现图标。再次打开程序会唤出设置窗口。
3. 左键点托盘图标打开设置，在「对话」里点 `+` 新建一个渠道：填接口地址、选请求格式、粘贴密钥、拉取模型列表。
4. 在「通用」里选一个默认模型。
5. 随便选中一段文字，按住右键。

需要 64 位的 Windows 10 或 11。想开机启动，在设置「通用」页打开「开机自启」，或在托盘右键菜单里勾选。
「通用」页也显示当前版本，并可检查 GitHub 上的正式版本。

## 配置文件

都在 `%APPDATA%\Myuri\`：

| 文件 | 内容 |
| --- | --- |
| `config.toml` | 手势、快捷键、主题、默认模型、语音 |
| `assistants.toml` | 助手列表，顺序就是面板里的顺序 |
| `prompts\*.md` | 每个助手的系统提示词 |
| `providers.toml` | 对话渠道、模型列表、分组与收藏 |
| `image_providers.toml` | 生图渠道、模型列表、收藏与默认生图参数 |
| `speech_providers.toml` | 语音渠道、识别和合成模型、音色及语音参数 |
| `mcp.toml` | MCP 服务 |
| `skills\*\SKILL.md` | 技能 |
| `history\` | 保存的对话 |

升级时，程序从旧版 `providers.toml` 生成缺失的生图和语音配置，并把原渠道凭据复制到对应的凭据条目。目标已经有密钥时使用目标密钥。三个凭据条目的前缀分别是 `Myuri/provider/`、`Myuri/image/` 和 `Myuri/speech/`。

日志在 `%LOCALAPPDATA%\Myuri\logs\myuri.log`，记录内容的类型和长度。

## 命令行

```text
myuri                             启动
myuri --version                   显示当前版本，也可用 -V
myuri settings                    打开设置窗口
myuri autostart [on|off]          查看或开关开机自启
myuri check                       检查配置文件
myuri ask <助手> [文字]           不开窗口直接问，可以接管道
myuri image <渠道> <模型> <描述>  生成图片，可用 --resolution 1K|2K|4K 设分辨率
                                  --ref <图片路径> 添加参考图，可重复传入
myuri models fetch <渠道>         拉取渠道的模型列表并比对
myuri models refresh              更新模型能力目录
myuri key [--image|--speech] <渠道> [--delete]
                                  默认操作对话密钥。从标准输入读入，或用 --delete 删除
myuri mcp <服务> [工具] [JSON]    列出一个 MCP 服务的工具，或直接调用其中一个
myuri quit                        退出
```

## 从源码构建

需要较新的 Rust stable（MSVC 工具链）。

```sh
cargo test --workspace
cargo build --release
```

产物是 `target\release\myuri.exe`，单个文件，没有运行时依赖。

也可在 GitHub Actions 手动运行 **Release** 工作流，从该次运行的 Artifacts 下载 `Myuri-windows-x64.zip`。推送匹配 Cargo 版本的 `v*` 标签会走正式发布流程。

代码分两个包：`crates/myuri-core` 是纯逻辑（手势状态机、配置、各家协议的请求和解析），不碰 Win32 也不碰界面，全部有单元测试；`crates/myuri` 是程序本体（鼠标钩子、内容抓取、托盘、窗口、网络）。

## 隐私

Myuri 没有服务器，也不收集任何数据。你选中的内容只会发给你自己配置的渠道。密钥由 Windows 凭据管理器保管。

## 现状

项目还很年轻，每天都在用也每天都在改。对话和生图用得最多；语音、工具调用在 Gemini 和 Anthropic 原生协议下测得相对少。遇到问题欢迎提 [Issue](https://github.com/TheWiseWolfHolo/myuri/issues)。

## 致谢

- [GPUI](https://www.gpui.rs/) 和 [gpui-kit](https://github.com/longbridge/gpui-kit)：界面框架和组件
- [models.dev](https://models.dev)：模型能力数据
- [Lucide](https://lucide.dev)：线条图标
- [Lobe Icons](https://github.com/lobehub/lobe-icons)：模型和厂商图标

## 许可证

[Apache-2.0](LICENSE)
