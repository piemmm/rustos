## NAME

sysinfo — システム情報を問い合わせる

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

システム情報 API に型付きの問い合わせを一つ発行し、返答を描画します。
TAIRiX には `/proc` も `/sys` もありません。このコマンドは、あらゆるプロ
グラムが使うのと同じ、バージョン付きで権能検査される API の端末側の顔で
あり、権能検査を迂回する経路はありません。

問い合わせ：

- `processes`、`ps` — プロセスを一覧する。プロセスごとに一行。
- `memory`、`mem` — カーネルのメモリ統計（`CAP_SYSINFO_KERNEL` が必
  要）。
- `hardware`、`hw` — 検出されたハードウェアツリー（`CAP_SYSINFO_HW` が
  必要）。
- `identity`、`id` — 機体の身元と OS のバージョン。
- `uptime` — 起動からの時間と起動時の壁時計時刻。
- `limits`、`rlimits` — あなたの有効な資源制限と生の使用量。
- `seats` — シート一覧：各ディスプレイの所有者と前面コンソール
  （`CAP_SYSINFO_HW` が必要）。
- `pressure` — ライブのメモリ圧力ゲージ：バンド、ウォーターマーク、
  遷移カウンタ（`CAP_SYSINFO_KERNEL` が必要）。
- `reclaim` — 回収可能キャッシュ台帳（クラスごとに 1 行、
  `CAP_SYSINFO_KERNEL` が必要）。
- `ramzip` — 圧縮メモリ層のカウンタ（`CAP_SYSINFO_KERNEL` が必要）。
- `cpu` — CPU ごとの実行キュー深さ、コンテキストスイッチ、プリエンプション
  （`CAP_SYSINFO_KERNEL` が必要）。
- `irq`、`irqs` — カーネルの IRQ テーブル: バインドされた割り込み線ごとに
  1 行 — その ID、所有するドライバータスク、起動以降の割り込み回数、および
  線が隔離されているかどうか（`CAP_SYSINFO_HW` が必要）。
- `help` — このコマンド自身の短いヘルプ。

問い合わせなしなら短いヘルプが表示されます。

## OPTIONS

- `--all, -a` — `processes` とともに：自分のものだけでなくシステム上の
  すべてのプロセスを一覧する。サービスはこの眺めを
  `CAP_SYSINFO_GLOBAL` を持つ呼び出し元にだけ許す。
- `-h, -?` — このコマンド自身の短いヘルプを表示する。

## EXAMPLES

- `sysinfo identity` — 機体の身元と OS のバージョンを印字する。
- `sysinfo ps --all` — システム上のすべてのプロセスを一覧する。

## EXIT STATUS

- `0` — 問い合わせに答え、描画した。
- `1` — サービスが拒否または失敗した、または結果を届けられなかった。
- `2` — コマンド行を解釈できなかった。

## ENVIRONMENT

- `LANG` — 短いヘルプの優先ロケール（`ja-JP` のような BCP-47 タグ）。

## SEE ALSO

- `man`
- `ps`
- `top`
