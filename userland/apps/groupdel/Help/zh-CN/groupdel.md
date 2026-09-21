## NAME

groupdel — 删除一个组

## SYNOPSIS

`groupdel [--] NAME`

## DESCRIPTION

从组台账中移除一个组。删除组是管理操作：台账会拒绝不具备用户管理权能的调用者。

能否移除由台账裁定。它拒绝删除仍被某个账户引用的组，因此没有账户会指向不存在的组。

`--` 结束选项解析：其后的每个参数都是操作数。

## OPTIONS

- `-h, -?, --help` — 显示本命令自身的简短帮助。

## EXAMPLES

- `groupdel staff` — 删除组 `staff`。

## EXIT STATUS

- `0` — 组已删除。
- `1` — 台账拒绝或未能完成删除（例如缺少权能、组未知，或该组仍被引用）；原因打印到标准错误。
- `2` — 未能理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助首选的区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `groupadd`
- `usermod`
- `users`
