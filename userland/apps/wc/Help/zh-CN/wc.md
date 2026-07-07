## NAME

wc — 打印每个文件的换行、单词和字节计数

## SYNOPSIS

`wc [option...] [file...]`

`wc [option...] --files0-from <file>`

## DESCRIPTION

为每个 `file` 统计其行数（换行符数）、单词数和字节数，并打印在一行中，后
跟文件名。没有 `file` 或 `file` 为 `-` 时，读取标准输入（无操作数形式不打
印名字）。输入多于一个时，按 `--total` 的选择打印最后的 `total` 行。

选择器 `-l`、`-w`、`-m`、`-c` 和 `-L` 决定打印哪些计数；一个都没有时，打
印行、单词和字节计数。计数总是按固定顺序出现：行、单词、字符、字节、最大
行宽。单词是非空白字符的最长连续串。`-m` 统计 UTF-8 字符（不是有效 UTF-8
的字节计为字节但不计为字符）；`-L` 以终端列度量每行的显示宽度，制表符前进
到下一个 8 的倍数。

`--files0-from <file>` 从 `file` 读取以 NUL 分隔的操作数列表（`-` 表示标
准输入）；它不能与 `file` 操作数组合使用。

无法读取的输入在标准错误上报告，运行随即继续处理下一个输入。

## OPTIONS

- `-c, --bytes` — 打印字节计数。
- `-m, --chars` — 打印字符计数。
- `-l, --lines` — 打印换行计数。
- `-w, --words` — 打印单词计数。
- `-L, --max-line-length` — 打印一行的最大显示宽度。
- `--files0-from <file>` — 从 `file` 读取以 NUL 分隔的操作数列表（`-` 从
  标准输入读取）。
- `--total <when>` — 何时打印 `total` 行：`auto`（默认：仅在输入多于一个
  时）、`always`、`only`（只打印总计，不带标签）或 `never`。
- `-h, -?` — 显示本命令自身的简短帮助。

## EXAMPLES

- `wc notes.txt` — 打印 `notes.txt` 的行、单词和字节计数。
- `wc -l a b` — 打印 `a` 和 `b` 的行计数，然后是总计。
- `wc -L table.txt` — 以终端列打印 `table.txt` 最宽的一行。
- `wc -c --total=only a b` — 只打印相加后的字节计数。

## EXIT STATUS

- `0` — 每个输入都已统计（或简短帮助已写出）。
- `1` — 某个输入无法读取，或输出无法送达。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `cat`
- `head`
- `man`
