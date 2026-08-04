## NAME

dirname — 去掉名字的最后一个部件

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

打印去掉最后一个部件后的每个路径写法：先去掉结尾的斜杠，再去掉最后一个部
件及其前面的斜杠。这一手术纯属词法操作 — 不解析任何路径，也不触碰磁盘。没
有剩余斜杠的写法，其父目录是 `.`；被掏空的父目录就是根。

根永远不会被剥开：`dirname /tools` 是 `/`，而 — TAIRiX 存储森林中的对应物
— `dirname Home:/tools` 是 `Home:/`。别名根（`Home:/`、`System:/`……）所扮
演的角色与 POSIX 系统上的 `/` 完全相同。

## OPTIONS

- `-z, --zero` — 每个结果以 NUL 而非换行符结束。
- `-h, -?` — 显示本命令自身的简短帮助。

## EXAMPLES

- `dirname /System/Commands/top.app` — 打印 `/System/Commands`。
- `dirname src/lib.rs` — 打印 `src`。
- `dirname file` — 打印 `.`（没有目录部分）。
- `dirname Home:/tools` — 打印 `Home:/`（根永远不会被剥开）。

## EXIT STATUS

- `0` — 结果（或简短帮助）已写出。
- `1` — 输出无法送达。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `basename`
- `man`
