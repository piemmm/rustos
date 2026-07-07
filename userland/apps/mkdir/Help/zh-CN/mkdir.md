## NAME

mkdir — 创建目录

## SYNOPSIS

`mkdir [-pv] [--] directory...`

## DESCRIPTION

按顺序创建每个目录操作数。没有 `-p` 时，每个操作数的父目录必须已经存在，
而操作数本身必须不存在；第一个失败会在任何后续操作数之前停止运行。

有 `-p` 时，先创建每个缺失的祖先，自最外层开始；已经作为目录存在的操作数
（或祖先）不是错误。以文件形式存在的祖先仍然失败：任何东西都不会被悄悄替
换。

GNU `mkdir` 的 `-m`/`--mode` 尚未被接受：在模式设置机制落地之前，目录以文
件系统的默认模式创建；该开关将与其一同到来，而不是被忽略。`--` 结束选项解
析：之后的每个参数都是路径。

## OPTIONS

- `-p, --parents` — 创建缺失的父目录；已是目录的操作数不是错误。
- `-v, --verbose` — 以 `mkdir: created directory 'dir'` 报告每个创建的目
  录。
- `-h, -?` — 显示本命令自身的简短帮助（也可用 `--help`）。

## EXAMPLES

- `mkdir Notes` — 在当前目录中创建一个目录。
- `mkdir -p Projects/os/build` — 创建整条链，跳过已经存在的部分。
- `mkdir -pv Home:/tools/bin` — 在别名根之下创建，并报告每个新目录。

## EXIT STATUS

- `0` — 每个目录都已创建（或在 `-p` 下本已存在）。
- `1` — 文件系统或输出失败；原因打印在标准错误上。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

rmdir, rm, ls
