## NAME

basename — 从名字中去掉目录和后缀

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

打印每个路径写法的最后一个部件：先去掉结尾的斜杠，再去掉直到（并包括）最
后一个剩余斜杠的所有内容。这一手术纯属词法操作 — 不解析任何路径，也不触碰
磁盘。给出 `suffix`（第二个操作数，或 `-s`）时，还会去掉结尾的 `suffix`，
除非它就是剩下的整个名字。

根永远不会被剥开：`basename /` 是 `/`，而 — TAIRiX 存储森林中的对应物 —
`basename Home:/` 是 `Home:/`。别名根（`Home:/`、`System:/`……）所扮演的
角色与 POSIX 系统上的 `/` 完全相同。

没有 `-a` 或 `-s` 时，最多接受两个操作数：名字和可选的后缀。有 `-a`（或隐
含它的 `-s`）时，每个操作数都是名字。

## OPTIONS

- `-a, --multiple` — 把每个操作数都当作名字。
- `-s, --suffix <suffix>` — 从每个名字去掉结尾的 `suffix`；隐含 `-a`。也
  可写作 `--suffix=<suffix>` 或捆绑形式（`-s.rs`）。
- `-z, --zero` — 每个结果以 NUL 而非换行符结束。
- `-h, -?` — 显示本命令自身的简短帮助。

## EXAMPLES

- `basename /System/Commands/top.app` — 打印 `top.app`。
- `basename src/lib.rs .rs` — 打印 `lib`。
- `basename -s .rs -a a.rs b.rs` — 打印 `a` 和 `b`。
- `basename Home:/` — 打印 `Home:/`（根永远不会被剥开）。

## EXIT STATUS

- `0` — 结果（或简短帮助）已写出。
- `1` — 输出无法送达。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，例如 `zh-CN`）。

## SEE ALSO

- `dirname`
- `man`
