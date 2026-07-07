## NAME

true — 什么也不做，并成功返回

## SYNOPSIS

`true [ignored arguments]`

## DESCRIPTION

以状态 `0` 退出，忽略所有参数。脚本在任何需要一个总是成功的命令的地方使用
它 — 作为占位命令、恒真条件，或循环体。

只有作为**第一个**参数的 `-h`、`-?` 或 `--help` 才被接受（这是 GNU `true`
接受 `--help` 的位置）；出现在之后任何位置时，这些记号会像其他参数一样被忽
略。

## OPTIONS

- `-h, -?` — （仅作为第一个参数时）显示本命令自身的简短帮助。

## EXAMPLES

- `true` — 成功。
- `while true; do …; done` — 循环直至被中断。

## EXIT STATUS

- `0` — 总是如此（这就是本工具的全部目的）。
- `1` — 请求的简短帮助无法写出。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `false`
- `man`
