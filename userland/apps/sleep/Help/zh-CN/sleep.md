## NAME

sleep — 暂停一段时间间隔的总和

## SYNOPSIS

`sleep NUMBER[SUFFIX]...`

## DESCRIPTION

暂停所给间隔的总和，然后退出。

每个 `NUMBER` 都是一个浮点值；单字母 `SUFFIX` 对其进行缩放：`s` 表示秒
（默认），`m` 表示分钟，`h` 表示小时，`d` 表示天。多个操作数相加，因此
`sleep 1m 30s` 暂停九十秒。`inf`（或 `infinity`）会一直暂停，直到进程被
杀死。

与 shell 自身的计时不同，`sleep` 在处理器之外睡眠：任务被驻留，直到间隔
过去为止，绝不会让某个核心空转。

负值、`nan`、未知后缀，或数字之后的多余字符都是 `invalid time interval`。
完全不给操作数则是 `missing operand`。

此命令不打印系统版本；TAIRiX 没有这样的字符串，因此——与 GNU `sleep`
不同——它没有 `--version` 选项。

## OPTIONS

- `-h, -?` — 显示本命令自身的简短帮助。
- `--` — 结束选项解析；其后的任何参数都作为操作数。

## EXAMPLES

- `sleep 5` — 暂停五秒。
- `sleep 1.5h` — 暂停九十分钟。
- `sleep 1m 30s` — 暂停九十秒（操作数相加）。
- `sleep inf` — 一直暂停，直到进程被杀死。

## EXIT STATUS

- `0` — 间隔已过去，或已写出所请求的简短帮助。
- `1` — 写出简短帮助失败。
- `2` — 命令行无法理解（未知选项、缺少操作数或无效的时间间隔）。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（形如 `fr-FR` 的 BCP-47 标签）。

## SEE ALSO

- `top`
