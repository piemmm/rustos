## NAME

useradd — 创建用户账户

## SYNOPSIS

`useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME`

## DESCRIPTION

向用户数据库添加一个账户。登录名必须匹配 `[a-z_][a-z0-9_-]*`；主组
（`-g`）是必需的，每个组或用户引用都是十进制 id。创建账户是管理操作：数
据库拒绝没有用户管理能力的调用者。

创建的账户**没有可用的口令**：在管理员设置口令之前，没有任何口令能与之匹
配（也无从猜测），与 GNU 工具创建禁用账户的做法完全一致。之后用 `users`
工具的 `passwd` 命令设置口令。

省略 `-u` 时，用户 id 被自动分配，为现有最高 id 加一。省略 `-d` 时，主目
录采用标准的 `/Users/NAME` 布局。账户以系统默认 shell 和普通的会话能力上
限开始；管理员之后用 `users` 工具的 `grant` 命令加宽它。

`--` 结束选项解析：之后的每个参数都是操作数。

## OPTIONS

- `-u, --uid UID` — 数字用户 id；省略时自动分配（现有最高 id 加一）。
- `-g, --gid GID` — 数字主组 id。必需：没有可猜测的默认组策略。
- `-G, --groups LIST` — 以逗号分隔的数字补充组 id。
- `-c, --comment TEXT` — 账户注释 / 完整显示名。
- `-d, --home PATH` — 主目录；省略时为 `/Users/NAME`。
- `-h, -?, --help` — 显示本命令自身的简短帮助。

## EXAMPLES

- `useradd -g 100 alice` — 在主组 `100` 中以自动分配的 id 创建
  `alice`。
- `useradd -u 1000 -g 100 -G 10,20 -c 'Alice A' alice` — 写明每个字段。

## EXIT STATUS

- `0` — 账户已创建。
- `1` — 数据库拒绝或未能创建（例如缺少能力、id 重复或组未知）；原因打印
  在标准错误上。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `groupadd`
- `users`
