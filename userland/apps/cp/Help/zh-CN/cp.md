## NAME

cp — 复制文件和目录

## SYNOPSIS

`cp [-finrRvT] [-t dir] [--] source... dest`

## DESCRIPTION

把每个来源操作数复制到目的地。只有一个来源、且目的地不指目录时，来源被复
制到那个确切路径。当目的地指向已存在的目录 — 以及来源多于一个的任何时候 —
每个来源都以其自身的基本名被复制*进*该目录。

目录来源只有带 `-r` 才被复制，它重现整棵子树；没有 `-r` 时目录操作数被拒
绝。已存在的目标文件默认被覆盖，带 `-n` 时被跳过，带 `-i` 时在标准错误流
上询问（被拒绝的询问无错误地跳过该次复制；无法读取的回答绝不被当作同
意）。

第一个失败会在任何后续操作数之前停止运行。`--` 结束选项解析：之后的每个
参数都是路径。

## OPTIONS

- `-r, -R, --recursive` — 复制目录及其内容。
- `-f, --force` — 目标文件无法创建时，删除它并把复制重试一次。
- `-i, --interactive` — 覆盖已存在的文件前询问；只有以 `y`/`Y` 开头的回答
  才算同意。
- `-n, --no-clobber` — 从不覆盖已存在的文件。`-i` 与 `-n` 以后者为准。
- `-v, --verbose` — 以 `'source' -> 'dest'` 报告每次复制。
- `-t dir, --target-directory=dir` — 把每个来源复制进 `dir`，它必须是已存
  在的目录。值可紧跟（`-tdir`、`--target-directory=dir`）或作为下一个参
  数。
- `-T, --no-target-directory` — 把目的地当作普通文件；恰好允许一个来源。
  不能与 `-t` 组合。
- `-h, -?, --help` — 显示本命令自身的简短帮助。

## EXAMPLES

- `cp notes.txt backup.txt` — 把一个文件复制为新名字。
- `cp -r Projects Archive` — 在 `Archive` 里重现 `Projects` 树（若
  `Archive` 不存在则复制为 `Archive`）。
- `cp -v -t Backup a.txt b.txt` — 把两个文件都复制进 `Backup`，并报告每次
  复制。

## EXIT STATUS

- `0` — 每次复制都成功（`-n` 的跳过和被拒绝的 `-i` 询问不算失败）。
- `1` — 文件系统、询问或输出失败；原因打印在标准错误上。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `ls`
- `mv`
- `rm`
