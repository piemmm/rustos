## NAME

false — 什么也不做，并失败返回

## SYNOPSIS

`false [ignored arguments]`

## DESCRIPTION

以状态 `1` 退出，忽略所有参数。脚本在任何需要一个总是失败的命令的地方使用
它 — 作为恒假条件，或一次有意的失败。

只有作为**第一个**参数的 `-h`、`-?` 或 `--help` 才被接受（这是 GNU `false`
接受 `--help` 的位置）；出现在之后任何位置时，这些记号会像其他参数一样被忽
略。与仍以 `1` 退出的 GNU `false --help` 不同，此处成功送达的简短帮助以
`0` 退出 — 这是 RustOS 的简短帮助约定。

## OPTIONS

- `-h, -?` — （仅作为第一个参数时）显示本命令自身的简短帮助。

## EXAMPLES

- `false` — 失败。
- `until false; do …; done` — 执行循环体一次（条件恒为假）。

## EXIT STATUS

- `1` — 总是如此（这就是本工具的全部目的）。
- `0` — 请求的简短帮助已送达。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `true`
- `man`
