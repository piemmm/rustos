## NAME

usermod — 修改一个用户账户

## SYNOPSIS

`usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST] [-L | -U]
[--grants LIST] [--] NAME`

## DESCRIPTION

更改一个账户的身份字段、锁定状态或权能上限。修改账户是管理操作：数据库会拒绝不具备用户管理权能的调用者。

身份编辑会替换该账户全部非安全字段，因此工具先读取账户当前记录，并将无人指定的每个字段原样回送。数据库未列出的账户在发送任何内容之前即被拒绝。

每个开关都是各自独立的数据库操作，逐个应用且要么全做要么不做。一条要求多项的命令行会发出多次操作，顺序固定——字段、然后权能、然后锁定状态——并在首次拒绝时停止，指明步骤并提醒先前的更改可能已经生效。

`-G` 替换整个附加组集合，而不是追加：数据库接受的是整集，基于过期读数的追加比显式替换更糟。`--grants` 是 TAIRiX 自身的概念，没有
coreutils 对应项，故只写长格式；调用账户本身不持有的权能，数据库一律拒绝。

`--` 结束选项解析：其后的每个参数都是操作数。

## OPTIONS

- `-c, --comment COMMENT` — 账户注释／全名。
- `-d, --home HOME` — 主目录。
- `-s, --shell SHELL` — 登录外壳。
- `-g, --gid GID` — 主组的数字标识。
- `-G, --groups LIST` — 以逗号分隔的附加组数字标识，替换当前集合。空列表会清空它。
- `-L, --lock` — 禁止该账户登录。
- `-U, --unlock` — 重新允许登录。
- `--grants LIST` — 以逗号分隔的权能名，构成整个上限。空列表会清空它。
- `-h, -?, --help` — 显示本命令自身的简短帮助。

## EXAMPLES

- `usermod -c 'Ada Lovelace' ada` — 设置账户全名。
- `usermod -L ada` — 锁定账户。
- `usermod --grants LIST` — 替换权能上限。

## EXIT STATUS

- `0` — 所有请求的更改均已完成。
- `1` — 数据库拒绝或未能完成某项更改；步骤与原因打印到标准错误。
- `2` — 未能理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助首选的区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `useradd`
- `userdel`
- `passwd`
- `groupadd`
- `users`
