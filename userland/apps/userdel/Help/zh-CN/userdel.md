## NAME

userdel — 删除一个用户账户

## SYNOPSIS

`userdel [--] NAME`

## DESCRIPTION

从用户数据库中移除一个账户。删除账户是管理操作：数据库会拒绝不具备用户管理权能的调用者。

能否移除由数据库裁定。它拒绝删除最后一个可管理用户的活动账户，因此系统永远不会失去被管理的途径。

`--` 结束选项解析：其后的每个参数都是操作数。

## OPTIONS

- `-h, -?, --help` — 显示本命令自身的简短帮助。

## EXAMPLES

- `userdel ada` — 删除账户 `ada`。

## EXIT STATUS

- `0` — 账户已删除。
- `1` — 数据库拒绝或未能完成删除（例如缺少权能、账户未知，或这是最后一个管理员）；原因打印到标准错误。
- `2` — 未能理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助首选的区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `useradd`
- `usermod`
- `passwd`
- `users`
