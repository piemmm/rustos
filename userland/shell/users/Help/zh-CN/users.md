## NAME

users — 管理用户账户和组

## SYNOPSIS

`users [-h | -?]`

## DESCRIPTION

在受门禁的 `users_admin` 接口上运行交互式账户管理会话。每个操作都在内核
一侧、以你经内核认证的身份决定：账户上限中没有 `CAP_USER_ADMIN` 时，每个
操作在分派时即被拒绝。口令在终端回显关闭的情况下读取，并在客户端一侧散列
为带盐记录；明文从不穿过接口，也从不回显或记录。

工具不接受操作数：账户用会话内键入的命令管理。

- `list` — 列出用户账户。
- `groups` — 列出组。
- `create <name> <uid> <gid>` — 创建账户。
- `passwd <name>` — 设置账户口令。
- `lock <name>`、`unlock <name>` — 禁用或重新启用账户。
- `grant <name> <CAP_...>`、`revoke <name> <CAP_...>` — 编辑账户的能力
  授予。
- `deluser <name>` — 删除账户。
- `addgroup`、`delgroup` — 创建或删除组。
- `help` — 列出会话命令。
- `exit`、`quit` — 结束会话。

## OPTIONS

- `-h, -?` — 显示本命令自身的简短帮助并退出。

## EXIT STATUS

- `0` — 会话干净地结束，或简短帮助已显示。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `man`
