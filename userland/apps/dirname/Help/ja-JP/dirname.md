## NAME

dirname — 名前から最後の構成要素を取り除く

## SYNOPSIS

`dirname [-z] name...`

## DESCRIPTION

各パス表記から最後の構成要素を取り除いたものを印字します。まず末尾のスラ
ッシュを取り除き、次に最後の構成要素とその前のスラッシュを取り除きます。
この処置は純粋に字句上のもの — パスは解決されず、ディスクにも触れません。
スラッシュが残らない表記の親は `.`、空になってしまう親はルートです。

ルートが剥がされることはありません。`dirname /tools` は `/` であり、
TAIRiX のストレージフォレストにおける対応物として `dirname Home:/tools`
は `Home:/` です。エイリアスのルート（`Home:/`、`System:/`、…）は、POSIX
システムで `/` が果たす役割をそのまま担います。

## OPTIONS

- `-z, --zero` — 各結果を改行の代わりに NUL で終える。
- `-h, -?` — このコマンド自身の短いヘルプを表示する。

## EXAMPLES

- `dirname /System/Apps/top.app` — `/System/Apps` を印字する。
- `dirname src/lib.rs` — `src` を印字する。
- `dirname file` — `.` を印字する（ディレクトリ部分がない）。
- `dirname Home:/tools` — `Home:/` を印字する（ルートは剥がされない）。

## EXIT STATUS

- `0` — 結果（または短いヘルプ）を書き出した。
- `1` — 出力を届けられなかった。
- `2` — コマンド行を解釈できなかった。

## ENVIRONMENT

- `LANG` — 短いヘルプの優先ロケール（`ja-JP` のような BCP-47 タグ）。

## SEE ALSO

- `basename`
- `man`
