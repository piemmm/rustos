## NAME

elsh — TAIRiX 的命令 shell

## SYNOPSIS

`elsh [-h | -?]`

## DESCRIPTION

运行一个交互式命令 shell — 在继承的标准流上的读取-求值-打印循环。键入的命
令词先在 shell 的内建命令中解析，然后是系统命令商店（`/System/Commands`）、
系统应用商店（`/System/Applications`）、用户自己的命令商店
（`<home>/Commands`）和应用商店（`<home>/Applications`），再是 `PATH` 变量
的目录；这四个商店构成一个固定前缀，用户无法重新排序或覆盖，因此 `PATH`
永远不能遮蔽系统命令。无法解析的词以 `127` 退出；解析成功但不可执行的程序
包以 `126` 退出。

内建命令：

- `cd <path>`、`pwd` — 更改和打印工作目录。
- `echo ...` — 打印其操作数。
- `export NAME=value`、`unset NAME` — 编辑导出的环境。
- `jobs`、`fg`、`bg` — 作业控制。
- `ulimit` — 读取和施加资源限制。
- `elevate` — 经控制台的登录监督者重新认证后运行一条命令。
- `help` — 列出内建命令。
- `exit [code]` — 结束会话。

shell 不接受操作数：脚本执行尚不在其语法之内。

在终端上，shell 提供交互式行编辑器：上/下方向键浏览命令历史，`Ctrl-R` 搜索历史，`Ctrl-C` 放弃当前行，空行上的
`Ctrl-D` 结束会话，Tab 补全命令名、路径以及 `sys:random` 这样的资源引用。命名空间按段逐层补全其已注册的选择符（`state:`
→ `net/` → `wan/` → `link`）。注册表只知道形状的段（接口名、中断线）由 Tab
补全为机器上的真实名称；若本会话无权列举，则不提供任何候选。重定向目标只提供可以打开的命名空间，因此
`info:`/`state:`/`stats:` 只作为参数出现（用 `sysinfo show` 读取），绝不出现在 `>` 之后。

## OPTIONS

- `-h, -?` — 显示本命令自身的简短帮助并退出。

## EXIT STATUS

- `exit` 内建命令的代码，或输入流结束时为 `0`（或简短帮助已显示）。
- `2` — 无法理解此次调用。

## ENVIRONMENT

- `PATH` — 在固定商店前缀之后搜索的目录。
- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`），导出给每
  个被启动的命令。

## SEE ALSO

- `man`
