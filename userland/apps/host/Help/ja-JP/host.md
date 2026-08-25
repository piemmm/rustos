## NAME

host — DNS で名前を解決する

## SYNOPSIS

`host [-t type] name|address`

## DESCRIPTION

システムのスタブリゾルバを使ってドメイン名をそのアドレスに解決し、各応答を 1 行ずつ
出力します。`-t` を付けない場合は `A`（IPv4）と `AAAA`（IPv6）の両方のレコードを問い
合わせます。`-t type` は問い合わせを 1 つに限定します。

問い合わせ先の再帰 DNS サーバーは、システム情報 API を通じてホスト構成から読み取られ
ます——`state:net/resolver/servers` の読み取りが報告するものと同じ有効な集合です——そ
して各応答はアドレスを表示する前に検証されます。`/etc/resolv.conf` もローカルの hosts
ファイルもありません。

オペランドが IPv4 または IPv6 のアドレスリテラルの場合は**逆引き**になります。その
アドレスに対応する `in-addr.arpa` / `ip6.arpa` の名前へ書き換えられ、既定のレコード
種別は `PTR` になり、見つかったレコードは
`<reverse-name> domain name pointer <name>.` として出力されます。

対応するのは `A`、`AAAA`、`PTR` のレコードだけです。その他の種類（`MX`、`TXT` な
ど）は、黙って `A` として扱われるのではなく拒否されます。存在しない名前は
`Host <name> not found: 3(NXDOMAIN)` を出力します。どのサーバーにも到達できない場合、
`host` は標準エラーにタイムアウトを報告します。

## OPTIONS

- `-t, --type` — 問い合わせる DNS レコードの種類：`A`、`AAAA`、`PTR`（大文字小文字
  を区別しない）。指定しない場合、名前は `A` と `AAAA` を、アドレスは `PTR` を問い
  合わせます。
- `-?, --help` — このコマンド自身の簡易ヘルプを表示する。

## EXAMPLES

- `host example.com` — 名前の IPv4 と IPv6 のアドレス。
- `host -t AAAA example.com` — IPv6 アドレスのみ。
- `host 10.0.2.2` — そのアドレスが逆引きされる名前。

## EXIT STATUS

- `0` — 少なくとも 1 つのアドレスが見つかった（または簡易ヘルプを出力した）。
- `1` — 名前がどのアドレスにも解決しなかった（否定応答、タイムアウト、またはリゾル
  バの失敗）。
- `2` — コマンドラインを理解できなかった、または出力を書き込めなかった。

## ENVIRONMENT

- `LANG` — 簡易ヘルプに優先されるロケール（`fr-FR` のような BCP-47 タグ）。

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
