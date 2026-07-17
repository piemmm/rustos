## NAME

whoami — 打印当前用户的账户名

## SYNOPSIS

`whoami`

## DESCRIPTION

打印与本进程身份相关联的用户名，后跟一个换行符，除此之外不输出任何
内容。

TAIRiX 没有 `/etc/passwd`：用户标识符来自内核为调用进程保存的记录，
对应的账户名来自系统信息 API 的公开账户目录。若目录中没有该标识符
对应的名称，命令报告 `cannot find name for user ID <uid>` 并失败。

该命令不接受操作数；任何参数都是 `extra operand` 错误。

## OPTIONS

- `-h, -?` — 显示本命令自身的简短帮助。
- `--` — 结束选项解析；之后的任何参数仍是多余的操作数（`whoami` 不接
  受任何操作数）。

## EXAMPLES

- `whoami` — 打印运行该命令的账户的名称。

## EXIT STATUS

- `0` — 已写出名称（或请求的简短帮助）。
- `1` — 读取身份、查询目录或输出失败，或目录中没有该用户标识符对应
  的名称。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `users`
- `ps`
