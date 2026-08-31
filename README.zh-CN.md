# codex-rewind

[English](https://github.com/extracurricular-ai/codex-rewind/blob/main/README.md) · **简体中文**

[![npm](https://img.shields.io/npm/v/codex-rewind?label=npm&color=cb3837)](https://www.npmjs.com/package/codex-rewind)
[![downloads](https://img.shields.io/npm/dm/codex-rewind?label=downloads&color=2f855a)](https://www.npmjs.com/package/codex-rewind)
[![licence](https://img.shields.io/badge/licence-Apache--2.0-blue)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-informational)](#安装)

**[OpenAI Codex CLI](https://github.com/openai/codex) 的非官方发行版。**

> Codex CLI 是什么、怎么登录、怎么使用,请看
> **[官方 README](https://github.com/openai/codex#readme)**。那里写的一切在这里同样适用,
> 本页只讲这个发行版**多出来的东西**。
>
> 与 OpenAI 无隶属关系,未获其背书或支持。许可证与上游一致,均为 Apache-2.0。
> 有问题请在本仓库反馈,不要提给 OpenAI —— 请附上 npm 的版本号,只有它能定位到
> 具体是哪个发行版。见[提交 bug 报告](#提交-bug-报告)。

---

## 它加了什么:rewind

上游 Codex 能把你带回**对话**里更早的位置,却带不回你的**文件**——于是模型开始基于一段
比磁盘上的代码更旧的对话来推理。

这个发行版补上了缺的那一半。`/rewind` 把工作区恢复成你选中那条提示词时的样子,
`/redo` 在你反悔时把它放回去。

![agent 删掉 notes.txt;/rewind 选中删除之前那一步;ls 显示文件回来了](https://raw.githubusercontent.com/extracurricular-ai/codex-rewind/main/.github/rewind.gif)

▶ [完整讲解(22 分钟)](https://youtu.be/OpJI8NQ-mvY) —— 上面这段演示的完整版,
以及它背后的设计:为什么 git 是错的地基、三个桶是什么、以及它到哪儿为止。

```
/rewind     选一条提示词,对话和文件一起回到那时
/redo       撤销上一次 rewind
/status     查看快照是否开启,以及占了多少磁盘
```

**不需要 git,也绝不碰你的 git 状态** —— 不提交、不 stash、不写 index,`.git` 里面
一个字节都不动。在根本不是仓库的目录里同样能用。

## 安装

```shell
npm install -g codex-rewind
```

命令是 **`codexr`**,不是 `codex`,所以它和官方版**并存**,不会互相覆盖。

```shell
codexr          # 这个发行版
codex           # 官方版(如果你装了)
```

提供 macOS、Linux、Windows 的预编译二进制,x64 和 arm64 都有,和上游的目标平台一致。
需要 Node 16 或更高。

卸载:

```shell
npm uninstall -g codex-rewind
rm -rf ~/.codex/file_snapshots      # 可选:连快照一起删掉
```

**不要删 `~/.codex` 本身** —— 官方版用的是同一个目录,你的登录和历史都在里面。

版本号格式是 `<上游版本>-rewind.<n>` —— `0.147.0-rewind.1` 表示它构建自上游
`rust-v0.147.0`,每个版本的基线都写在版本号里。它们在 semver 里属于 prerelease,
所以**版本范围**会跳过它们:写 `^0.147.0` 的依赖永远不会误装到。

基线那一半就是 `codexr --version` 报的值,`-rewind.<n>` 那一半只存在于 npm 包里。
见[提交 bug 报告](#提交-bug-报告)。

## 启用

rewind 默认关闭。想先试一次、不改任何文件:

```shell
codexr --enable file_snapshots
```

想长期开着,在 `/experimental` 里打开,或者:

```toml
# ~/.codex/config.toml
[features]
file_snapshots = true
```

它是**按会话绑定**的:打开只对**新会话**生效,关闭**不会中断**已经在追踪的会话。所以一个
会话要么全程有快照、要么全程没有,不存在"追踪了一半"这种需要动脑子的状态。

## ⚠️ 与官方版共用 `~/.codex`

这个发行版**刻意**使用和官方 Codex **同一个** `~/.codex` 目录,这样你的登录状态、配置和
历史对话都能直接沿用,不用重新登录一次。

代价你需要知道:

- **用官方 `codex` 打开一个被 rewind 追踪过的会话,会让它的追踪断掉。** 官方版完全不知道
  快照的存在。它会照常把对话继续下去,而它跑的每一轮都是**背后没有 checkpoint** 的——
  之后你在 `codexr` 里 `/rewind`,最远只能回到**本发行版最后看见的那一轮**。
  **没有报错,也没有警告,这个缺口是静默的。**
- 在官方版里开始的对话**完全没有快照**。在那里 `/rewind` 会退化成只回退对话,和上游行为一致。
- 官方版会打印一行 `unknown feature key in config: file_snapshots`,并忽略
  `[file_snapshots]` 配置段。无害,但你会看到。

**如果两个都用,一个对话就在开始它的那个版本里做完。** 如果你更希望两者彻底隔离,
把这个指到别处:

```shell
CODEX_HOME=~/.codex-rewind codexr
```

在那个目录里你需要重新登录一次,之后两个版本就什么都不共享了。

## 「为什么不每轮 commit 一次?」

有人这么做,并且说很折磨([#19205](https://github.com/openai/codex/issues/19205))。
两个理由说明它不是替代品:

- **每一轮都得有人记得这件事。** 人来做,那就是 #19205 里说的「折磨」;
  模型来做,它总会有忘掉的时候。检查点应该自己发生,不管有没有人想起来。
- **git 只看得见它已经知道的文件。** [#9203](https://github.com/openai/codex/issues/9203)
  背后的事故都是未跟踪文件 —— 表格、笔记、生成的数据。
  `git status` 显示一切正常,而你没有任何东西可以恢复。

设计推理、三分区边界背后的实测数据,以及正确性规则都在
**[RFC](https://github.com/extracurricular-ai/codex-rewind/blob/main/docs/rfc-file-snapshot-rewind.md)** 里。

## 追踪哪些文件

三个来源取并集,而且**每一个的上界都不是目录树的大小**——所以成本不会随着"这个仓库被
构建过多久"而增长:

| | |
| --- | --- |
| **git 已追踪的文件** | 直接读索引。**不设上限**:项目提交了什么,那就是项目的内容,多大都算。 |
| **agent 编辑过的文件** | 由 edit 工具捕获,不管它在哪——**包括工作目录之外**。不设上限。 |
| **最近修改的文件** | 兜底,用于捕捉 shell 对其余文件的改动。上限 100 个文件、单个 16 MB,并跳过 `node_modules`、`target`、`Pods` 之类的目录。 |

**隐藏文件默认不动** —— `.env`、`.vscode/`、虚拟环境和各种缓存属于工具状态而非你的工作
成果,跟着一轮对话被回滚是意外,甚至是破坏性的。`.git` 永远不读。但被 agent **显式编辑过**
的隐藏文件仍然会被追踪,因为那确实是你的工作成果。

想排除更多,加一个 `.codexsnapignore`(gitignore 语法)。它**刻意**和 `.gitignore` 分开:
被它忽略的路径不会被快照、不会被恢复,**也不会被恢复操作删除**。

## 从 Claude Code 过来的话

Claude Code 有检查点功能已经有一段时间了。如果那是你的参照系,差别在这几处:

| | Claude Code `file-history` | codex-rewind |
| --- | --- | --- |
| 追踪范围 | 它自己的编辑工具碰过的文件 | 同样这些,**外加** git 索引,**外加**最近改动的 100 个 |
| shell 命令做的改动 | 不追踪 | 由「最近改动」这个桶兜住 |
| 能回溯多远 | 一个会话里最近的 100 个检查点 | 没有上限 —— 不会为了腾地方丢弃轮次 |
| 存储形式 | 逐文件副本 | 内容寻址、去重 |
| 是否需要 git | 否 | 否 |

**无界的那部分两边是一样的**:谁都没有限制 agent 能编辑多少文件。
差别在于**还有什么在范围内**,以及**会不会为了腾地方丢东西**。

<sub>Claude Code 的数字引自其官方文档(Claude Code Docs → Checkpointing),
数据截至 2026 年 8 月。「bash 命令的改动不被追踪」「外部改动不被追踪」
出自同一页的 Limitations 小节。</sub>

## 它不做什么

- **不会恢复任何快照都没见过的文件。** 删除需要**确凿证据**证明文件当时不存在——即某次捕获
  找过它、而且没找到。绝不从"路径恰好不在记录里"去推断,因为猜错就会毁掉本来就不该由 agent
  删除的工作。
- **不会恢复"它还不知道这个文件存在"之前的内容。** 工作目录之外的文件,是在 agent 第一次
  碰到它时才进入追踪的,所以在那之前的提示词手上没有副本可还。回退到那里时会**明确告诉你**,
  并建议你选一条更近的提示词。
- **不会合并并发会话。** 同一目录下的两个会话可能互相覆盖文件。`/redo` 会在动手前**警告并
  列出文件名**,但它不做合并。请用 worktree 或另一份 checkout。
- **不支持远程环境。** 仅限本地。

## 与其信,不如查

这东西直接动你的文件,所以怀疑是正确的态度。有两份文档专门留着让你自己核,而不是听 README 说:

- **[RFC](https://github.com/extracurricular-ai/codex-rewind/blob/main/docs/rfc-file-snapshot-rewind.md)**
  —— 系统**是**什么:正确性规则、三分区边界及其背后的实测数据、以及为什么删除必须有正面证据。
- **[决策日志](https://github.com/extracurricular-ai/codex-rewind/blob/main/docs/file-snapshots-decision-log.zh.md)**
  —— 系统**不是**什么,以及为什么。每一个试过又被推翻的方案,都带日期、推翻理由,以及对那些
  "看起来很合理但就是不行"的做法标注的 **不要改回**。里面也记着这份代码里发现过的 bug 和它们
  背后的平台陷阱,包括作者自己踩的。

如果你想知道的是"有没有人认真想过这东西会怎么弄丢我的工作",第二份比第一份有用。

## 磁盘占用

快照存在 `~/.codex/file_snapshots/`,内容寻址——相同的文件内容只存一份,无论多少轮次或
多少会话共享它。`/status` 会显示占用大小。**删除对话时会连同它的快照一起删除**,文件内容
也一并清除。

## 衍生项目

从这个仓库衍生出去的两个项目,如果你要的不是「装在一个 Codex 发行版里的那一份」:

- **[filesnap](https://github.com/extracurricular-ai/filesnap)** —— 快照引擎本身,外面
  不套任何 agent。一个 Rust crate + CLI(`cargo install filesnap-cli`):内容寻址的存储、
  按 session 和 turn 做 `capture` / `restore`、方便脚本处理的 JSON Lines 输出、
  `.filesnapignore`,以及同一条规则——没有任何快照见过的文件,绝不删。想把 rewind 做进
  自己的东西里,用它。
- **[dsh-filesnap](https://github.com/extracurricular-ai/dsh-filesnap)** —— 同样的
  `/rewind` 和 `/redo`,做成 DeepSeek Harness 的插件:
  `dsh plugin --profile web add dsh-filesnap`。每轮工作区快照、浏览器 UI 里的 rewind
  入口,同样不碰 git。

## 提交 bug 报告

**`codexr --version` 报的是上游基线,不是发行版号。** 发行版是 `0.147.0-rewind.1`
时它只会说 `0.147.0`:编译进二进制的版本号来自上游工作区,而 `-rewind.<n>` 后缀是
打 npm 包时才加上的。同一基线上的两个发行版报出来是同一个号。

所以发行版号请从 npm 取:

```shell
npm ls -g codex-rewind
```

提 bug 时两个都附上:npm 版本定位到具体发行版,`codexr --version` 确认它构建自哪个上游。

## 参与贡献

提交请带签名:`git commit -s`。

> 本仓库的改动若将来要提给 openai/codex,**原作者必须本人签署 OpenAI 的 CLA**,
> 维护者无法代签。

## 许可证

Apache-2.0,继承自上游,保留 `NOTICE` 文件并按第 4 条声明了改动内容。
不主张任何 OpenAI 商标或背书。
