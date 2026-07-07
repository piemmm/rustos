## NAME

groupadd — 创建组

## SYNOPSIS

`groupadd [-g GID] [--] NAME`

## DESCRIPTION

向组登记处添加一个组。组名必须匹配 `[a-z_][a-z0-9_-]*`，id 是十进制值。
创建组是管理操作：登记处拒绝没有用户管理能力的调用者。

省略 `-g` 时，组 id 被自动分配，为现有最高 id 加一。请求的 id 已被占用时
会被拒绝；登记处是冲突的权威。

`--` 结束选项解析：之后的每个参数都是操作数。

## OPTIONS

- `-g, --gid GID` — 数字组 id；省略时自动分配（现有最高 id 加一）。
- `-h, -?, --help` — 显示本命令自身的简短帮助。

## EXAMPLES

- `groupadd staff` — 以自动分配的 id 创建 `staff`。
- `groupadd -g 100 staff` — 以 id `100` 创建 `staff`。

## EXIT STATUS

- `0` — 组已创建。
- `1` — 登记处拒绝或未能创建（例如缺少能力或 id 重复）；原因打印在标准错
  误上。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `useradd`
- `users`
