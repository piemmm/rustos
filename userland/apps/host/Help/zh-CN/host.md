## NAME

host — 通过 DNS 解析名称

## SYNOPSIS

`host [-t type] name`

## DESCRIPTION

使用系统的存根解析器把域名解析为其地址，并逐行打印每条应答。不带 `-t` 时，会同时
查询 `A`（IPv4）和 `AAAA`（IPv6）记录；`-t type` 把查询限制为其中一种。

要查询的递归 DNS 服务器通过系统信息 API 从主机配置中读取——与
`state:net/resolver/servers` 读取所报告的活动集合相同——并且在显示地址之前会校验每
条应答。没有 `/etc/resolv.conf`，也没有本地主机文件。

只支持 `A` 和 `AAAA` 地址记录；其他类型（`MX`、`TXT` 等）会被拒绝，而不是被悄悄当作
`A`。不存在的名称会打印 `Host <name> not found: 3(NXDOMAIN)`；当无法到达任何服务器
时，`host` 会在标准错误上报告超时。

## OPTIONS

- `-t, --type` — 要查询的 DNS 记录类型：`A` 或 `AAAA`（不区分大小写）。不带该
  选项时会查询两者。
- `-?, --help` — 显示本命令自己的简短帮助。

## EXAMPLES

- `host example.com` — 该名称的 IPv4 与 IPv6 地址。
- `host -t AAAA example.com` — 仅 IPv6 地址。

## EXIT STATUS

- `0` — 至少找到一个地址（或已写出简短帮助）。
- `1` — 名称未解析出任何地址（否定应答、超时或解析器故障）。
- `2` — 无法理解命令行，或无法写出输出。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，如 `fr-FR`）。

## SEE ALSO

- `ping`
- `ss`
- `sysinfo`
- `man`
