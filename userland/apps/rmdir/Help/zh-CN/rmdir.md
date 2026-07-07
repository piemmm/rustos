## NAME

rmdir — 删除空目录

## SYNOPSIS

`rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...`

## DESCRIPTION

按顺序删除每个目录操作数。只有**空目录**会被删除：文件系统本身以原子方式
拒绝文件（或任何非目录）以及有内容的目录，因此绝不会有别的东西在其位置上
被解除链接。文件请用 `rm`，有内容的目录树请用 `rm -r`。

有 `-p` 时，每个操作数的祖先也会被删除，自最内层开始：`rmdir -p a/b/c`
先删 `a/b/c`，再删 `a/b`，再删 `a`。路径的裸根（`/` 或诸如 `Home:/` 的别
名根）永远不会被要求删除。

有 `--ignore-fail-on-non-empty` 时，「目录非空」的拒绝不算错误 — 该操作数
（或 `-p` 的向上行走）就停在那里。任何其他拒绝都不被容忍。第一个真正的失
败会在任何后续操作数之前停止运行。`--` 结束选项解析：之后的每个参数都是路
径。

## OPTIONS

- `-p, --parents` — 也删除每个操作数的祖先，自最内层开始。
- `-v, --verbose` — 以 `rmdir: removing directory, 'dir'` 报告每次删除尝
  试。
- `--ignore-fail-on-non-empty` — 非空目录不算错误；有 `-p` 时向上行走停在
  那里。
- `-h, -?` — 显示本命令自身的简短帮助（也可用 `--help`）。

## EXAMPLES

- `rmdir Scratch` — 删除一个空目录。
- `rmdir -p Projects/os/build` — 自最内层起删除整条链。
- `rmdir -p --ignore-fail-on-non-empty a/b` — 删除 `a/b`，若这使 `a` 变空
  则一并删除。

## EXIT STATUS

- `0` — 每次删除都成功（被 `--ignore-fail-on-non-empty` 容忍的拒绝不算失
  败）。
- `1` — 文件系统或输出失败；原因打印在标准错误上。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

mkdir, rm, ls
