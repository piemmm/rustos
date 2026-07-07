## NAME

seq — 打印一列数

## SYNOPSIS

`seq [-f format] [-s string] [-w] [first [increment]] last`

## DESCRIPTION

以 `increment` 为步长打印从 `first` 到 `last` 的数，默认每行一个。省略的
`first` 或 `increment` 默认为 1 — 包括 `last` 小于 `first` 的情形，因此
`seq 5 1` 什么也不打印。当再加一个 `increment` 会越过 `last` 时，序列结
束。

三个操作数都按浮点值读取；`first` 低于 `last` 时 `increment` 通常为正，高
于时为负，且不得为零。`last` 可以是 `inf` 以永远计数。默认输出精度跟随操
作数的写法（`seq 1 0.25 2` 打印两位小数），纯整数序列无论数字多大都精确生
成。

选项扫描在第一个操作数处停止，开头的负数是操作数而非选项：`seq -5 5` 从
-5 开始计数。

## OPTIONS

- `-f, --format <format>` — 通过 printf 风格的浮点 `<format>` 打印每个数
  （一个 `%` 指令，类型为 `e`、`f`、`g` 或 `a`，大小写皆可，带常见的标
  志、宽度和精度）。不能与 `-w` 组合。
- `-s, --separator <string>` — 用 `<string>` 而非换行符分隔各数。输出仍以
  换行符结束。
- `-w, --equal-width` — 用前导零把每个数填充到相同宽度。不能与 `-f` 组
  合。
- `-h, -?` — 显示本命令自身的简短帮助。
- `--` — 结束选项解析；之后的每个参数都是操作数。

## EXAMPLES

- `seq 5` — 打印 1 到 5。
- `seq 2 5` — 打印 2 到 5。
- `seq 1 2 10` — 打印 1 到 9 的奇数。
- `seq 5 -1 1` — 从 5 倒数到 1。
- `seq -w 8 10` — 打印 `08`、`09`、`10`。
- `seq -s , 3` — 打印 `1,2,3`。
- `seq -f %.2f 3` — 打印 `1.00`、`2.00`、`3.00`。

## EXIT STATUS

- `0` — 序列（或请求的简短帮助）已写出。
- `1` — 输出不再接受字节。
- `2` — 无法理解命令行（未识别的选项、无效的数、零步长或坏格式）。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `yes`
- `man`
