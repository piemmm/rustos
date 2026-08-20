## NAME

ls — 列出目录内容

## SYNOPSIS

`ls [-aABbCcdFfGghikIlmNnopQqrRsSTtUuvXx1] [-w cols] [-I PATTERN]`
`[--block-size=SIZE] [--si] [--format=WORD] [--indicator-style=WORD]`
`[--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD]`
`[--quoting-style=STYLE] [--full-time] [--author] [--file-type]`
`[--group-directories-first] [--zero] [--color[=WHEN]] [--] [path...]`

## DESCRIPTION

列出每个路径操作数：目录操作数的条目被读取并列出（除非 `-d` 指目录本
身），其他任何操作数按其自身列出。没有操作数时列出当前目录（`.`）。

条目按名字排序（带 `-S` 时按大小从大到小；带 `-t` 时按时间从新到旧；带 `-r` 时反转），默认每行一个
名字。名字以 `.` 开头的条目被隐藏，除非给出 `-a` 或 `-A`；有条目被隐藏
时，会在标准信息流（fd 3）上发出一条说明，绝不写进列表本身。

长格式（`-l`）呈现类型与权限位、属主与属组、大小，然后是名字。属主与属组
是数字 id：解析账户名需要由能力保护的用户数据库，而列目录不应要求它，因
此输出与 GNU 工具的数字回退一致（`-n` 的输出完全相同）。时间戳列默认显示修改时间；`-c`、`-u` 和 `--time` 选择四个时间戳中显示（并据以排序）的那一个，`--time-style`（或 `--full-time`）决定其格式。尚没有链接计数列，因为文件系统契约尚不携带硬链接；等它携带时该列就会出现。

给出多个操作数时 — 以及在 `-R` 下总是如此 — 每个目录的列表之前有一个
`path:` 头，各块之间以空行分隔。

符号链接以类型字母 `l` 显示，在长格式中显示为 `名称 -> 目标`——目标按存储的原样
给出，不作解析，因为那就是链接所保存的内容。因此悬空链接照常列出；只有解析它的
姿态（`-L`，或对操作数使用 `-H`）才会报告目标不可到达。

## OPTIONS

- `-t` — 按所显示的时间戳排序，最新的在前。
- `-c` — 使用元数据更改时间（ctime）：带 `-l` 时显示它，带 `-t` 时据它排序；不带 `-l` 时据它排序。
- `-u` — 类似 `-c`，但为访问时间（atime）。
- `-i, --inode` — 打印每个条目的节点号。
- `-B, --ignore-backups` — 不列出名称以 `~` 结尾的条目，在所有模式
  下生效（即使使用 `-a` 也会隐藏备份）。
- `-I, --ignore=PATTERN` — 不列出与 shell 通配符 `PATTERN` 匹配的条目
  （可重复）；在所有模式下生效。
- `--hide=PATTERN` — 与 `--ignore` 相同，但当指定 `-a` 或 `-A` 时无效。
- `--time=WORD` — 要显示并据以排序的时间戳：`atime`（`access`、`use`）、`ctime`（`status`）、`mtime`（`modification`）或 `birth`（`creation`）。
- `--time-style=STYLE` — 时间戳格式：`locale`（默认）、`long-iso`、`full-iso`、`iso`。不支持自定义 `+FORMAT`。
- `--full-time` — 等同 `-l --time-style=full-iso`。
- `-a, --all` — 不隐藏名字以 `.` 开头的条目。
- `-A, --almost-all` — 类似 `-a`，但从不列出 `.` 和 `..`。
- `-d, --directory` — 列出目录操作数本身，而非其内容。
- `-F, --classify` — 给目录追加 `/`、给可执行文件追加 `*`。
- `-g` — 不带属主列的长格式；隐含 `-l`。
- `-h, --human-readable` — 与 `-l` 一起，把大小打印为 `1.1K`、`23M`
  （1024 的幂）。
- `-l` — 长格式：权限位、属主、属组、大小，然后是名字。
- `-m` — 以逗号分隔的名字，按宽度换行。
- `-n, --numeric-uid-gid` — 属主与属组为数字的长格式；隐含 `-l`。此处属
  主与属组本来就是数字（见上），因此与 `-l` 相同。
- `-o` — 不带属组列的长格式；隐含 `-l`。
- `-p` — 给目录追加 `/`。
- `-N, --literal` — 原样输出名字，不加引号（`--quoting-style=literal`）。
- `-Q, --quote-name` — C 风格引用：给每个名字加双引号，并转义引号、反斜
  杠和控制字符（`--quoting-style=c`）。
- `-b, --escape` — 与 `-Q` 相同，但不加外围引号，并转义空格
  （`--quoting-style=escape`）。
- `--quoting-style=WORD` — 名字的引用方式：`literal`（`-N`）、`shell`、
  `shell-always`、`shell-escape`、`shell-escape-always`、`c`（`-Q`）
  或 `escape`（`-b`）。默认在终端上为 `shell-escape`，否则为 `literal`；
  不支持 `locale` 和 `clocale` 风格。
- `-q, --hide-control-chars` — 将非图形字符显示为 `?`（终端上的默认值）；
  仅影响不转义的风格。
- `--show-control-chars` — 原样输出非图形字符（输出不是终端时的默认值）。
- `-r, --reverse` — 反转排序顺序。
- `-R, --recursive` — 递归列出子目录。
- `-L, --dereference` — 无论符号链接出现在哪里，都显示它所指文件的信息，而不是
  链接本身的信息。目标无法到达的链接会报告在标准错误上，列出继续进行，退出状态
  非零。
- `-H, --dereference-command-line` — 只解引用命令行上给出的符号链接；列表内部的
  链接仍显示为链接。`-L` 与 `-H` 中较晚的一个生效。
- `--dereference-command-line-symlink-to-dir` — 在没有格式选项另作规定时的默认
  行为：命令行上*指向目录*的链接会被解引用，因此 `ls linkdir` 列出该目录，而其他
  链接都显示自身。`-l`、`-d` 和 `-F` 则默认显示每个链接自身。
- `-s, --size` — 以 1024 字节块打印每个条目的分配大小（受 `-h` 缩放），
  每个目录列表带一行 `total`。
- `-C` — 分列显示，自上而下填充（终端上的默认方式）。
- `-S` — 按大小排序，最大的在前。
- `-U` — 不排序；按目录顺序列出条目。
- `-X` — 按文件名扩展名（最后一个 `.` 之后的文本）排序，
  相同时按名称。
- `-v` — 自然的“版本”排序，使 `f2` 排在 `f10` 之前；相同时
  按名称。
- `-f` — 不排序并显示所有条目：启用 `-a` 和 `-U`，禁用 `-l`
  和 `-s`。在其出现处生效，因此后面的 `-l`/`-s`/排序标志会
  覆盖它。
- `--sort=WORD` — 按名称选择排序键：`none`（`-U`）、`size`
  （`-S`）、`time`（`-t`）、`version`（`-v`）、`extension`
  （`-X`）或 `name`。
- `--group-directories-first` — 在其他条目之前列出目录；即使
  使用 `-r`，目录也在前。
- `-w, --width <cols>` — 设置输出宽度（列数）；`0` 表示不限。
- `-x` — 分列显示，从左到右填充。
- `-1` — 每行一个名字（默认）。
- `-?` — 显示本命令自身的简短帮助（`--help` 是长形式）。

- `--file-type` — 给目录追加 `/`，但不给可执行文件追加 `*`
  （`--indicator-style=file-type`）。
- `--indicator-style=WORD` — 按名称选择类型标记：`none`、`slash`
  （`-p`）、`file-type`（`--file-type`）、`classify`（`-F`）。
- `-G, --no-group` — 在长格式中省略组列。与 `-o` 不同，它本身
  不选择长格式。
- `--author` — 与 `-l` 一起，在所有者之后、组之前打印作者列
  （所属用户）。
- `--si` — 与 `-h` 类似，但用 1000 的幂（`1.1k`、`23M`）。
- `-k, --kibibytes` — 对 `-s` 单元格和 `total` 行使用 1024 字节块
  （已是默认，因此输出不变；大小选项优先）。
- `--block-size=SIZE` — 按 SIZE 缩放文件大小和 `-s` 块：整数
  （字节），或单位 `K`/`M`/`G`/`T`/`P`/`E`（1024）、`KiB` 形式
  （1024）、`KB` 形式（1000），可选地带整数系数。
- `--format=WORD` — 按名称选择排列：`long`（`-l`）/`verbose`、
  `single-column`（`-1`）、`vertical`（`-C`）、`across`/`horizontal`
  （`-x`）、`commas`（`-m`）。
- `-T, --tabsize <cols>` — 设置列网格的制表位（默认 8）；`0`
  仅用空格填充。
- `--zero` — 每行以 NUL 而非换行结尾；还会选择单列、原样引用
  和显示控制字符。

- `--color[=WHEN]` — 按类型为名称着色（目录、可执行文件、普通
  文件）。`WHEN` 为 `auto`（默认：仅当输出为已确认的终端时
  着色）、`always`（即使不是也着色，例如串行控制台）或
  `never`；不带 `WHEN` 的 `--color` 等同于 `always`。管道或
  重定向的输出从不着色。

## EXAMPLES

- `ls` — 列出当前目录。
- `ls -al /System` — `/System` 的长格式列表，包括隐藏条目。
- `ls -lhS` — 长格式、人类可读的大小、最大的在前。
- `ls -R Documents` — 递归遍历 `Documents`，每个目录一个头。
- `ls -F` — 用 `/` 标记目录、用 `*` 标记可执行文件。
- `ls -d Documents` — 列出 `Documents` 条目本身，而非其内容。

## EXIT STATUS

- `0` — 每个操作数都已列出。
- `1` — 某个操作数无法检视、某个目录无法读取，或输出无法送达。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

- `TERM` — 终端类型，决定 `--color` 输出的颜色深度。未设置或
  无颜色的 `TERM` 在 `auto` 下输出纯文本。

## SEE ALSO

- `cat`
- `man`
