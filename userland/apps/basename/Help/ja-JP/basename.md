## NAME

basename — 名前からディレクトリと接尾辞を取り除く

## SYNOPSIS

`basename name [suffix]`

`basename [-az] [-s suffix] name...`

## DESCRIPTION

各パス表記の最後の構成要素を印字します。まず末尾のスラッシュを取り除き、
次に最後に残ったスラッシュまで（それを含めて）を取り除きます。この処置は
純粋に字句上のもの — パスは解決されず、ディスクにも触れません。`suffix`
（第二オペランド、または `-s`）があれば、末尾の `suffix` も取り除かれます。
ただしそれが残った名前の全体である場合を除きます。

ルートが剥がされることはありません。`basename /` は `/` であり、TAIRiX の
ストレージフォレストにおける対応物として `basename Home:/` は `Home:/` で
す。エイリアスのルート（`Home:/`、`System:/`、…）は、POSIX システムで `/`
が果たす役割をそのまま担います。

`-a` も `-s` もなければ、受け付けるオペランドは最大二つ — 名前と任意の接尾
辞 — です。`-a`（またはそれを含意する `-s`）があれば、すべてのオペランドが
名前です。

## OPTIONS

- `-a, --multiple` — すべてのオペランドを名前として扱う。
- `-s, --suffix <suffix>` — 各名前から末尾の `suffix` を取り除く。`-a` を
  含意する。`--suffix=<suffix>` や束ねた形（`-s.rs`）でも書ける。
- `-z, --zero` — 各結果を改行の代わりに NUL で終える。
- `-h, -?` — このコマンド自身の短いヘルプを表示する。

## EXAMPLES

- `basename /System/Apps/top.app` — `top.app` を印字する。
- `basename src/lib.rs .rs` — `lib` を印字する。
- `basename -s .rs -a a.rs b.rs` — `a` と `b` を印字する。
- `basename Home:/` — `Home:/` を印字する（ルートは剥がされない）。

## EXIT STATUS

- `0` — 結果（または短いヘルプ）を書き出した。
- `1` — 出力を届けられなかった。
- `2` — コマンド行を解釈できなかった。

## ENVIRONMENT

- `LANG` — 短いヘルプの優先ロケール（`ja-JP` のような BCP-47 タグ）。

## SEE ALSO

- `dirname`
- `man`
