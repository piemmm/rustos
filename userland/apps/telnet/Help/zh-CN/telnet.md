## NAME

telnet — RFC 854 网络虚拟终端客户端

## SYNOPSIS

`telnet [option...] [host [port]]`

## DESCRIPTION

打开到某台主机的 TCP 连接，并把终端转接给它：主机的输出出现在标准输出上，
按键送往主机，而转义字符（默认为 `^]`）进入 `telnet>` 命令解释器。不指定
主机时，`telnet` 直接从该提示符开始，由 `open` 建立连接。

它既是访问另一台机器上面向行的服务的途径，也是手工探询任何 TCP 服务的途
径——`telnet host 80` 打开一条可以直接键入请求的连接。

主机可以是名称，也可以是字面的 IPv4/IPv6 地址。名称由系统的存根解析器解析，
该解析器通过系统信息 API 读取已配置的递归 DNS 服务器。端口是一个数字：本系
统没有服务数据库，因此服务*名称*属于用法错误，而不是悄悄退回到端口 23。

选项协商遵循 RFC 855，并采用 RFC 1143 的无环纪律，因此重复发送的对端绝不会
使客户端也跟着重复。本客户端实现的选项为 BINARY、ECHO、SUPPRESS GO AHEAD、
STATUS、TIMING MARK、TERMINAL TYPE、NAWS、TERMINAL SPEED、TOGGLE FLOW
CONTROL、LINEMODE 和 NEW-ENVIRON；其余一律拒绝，这正是“未实现的选项”应有的
含义。RFC 1184 的 LINEMODE 完整实现——`MODE` 掩码、本地字符表（SLC）以及
`FORWARDMASK`——因此客户端按服务器的要求编辑行，并使用服务器协商出的字符。

终端窗口大小在连接时通过 NAWS 上报，之后每次变化再次上报。TAIRiX 没有窗口
变化信号，因此每次键入时都会重新读取大小；调整窗口会在你下一次按键时到达
主机。

`NEW-ENVIRON` **只**披露你用 `environ` 命令定义并导出的变量；客户端绝不发送
自己的环境。`-a` 与 `-l` 导出一个登录名，这也是一次调用本身唯一会披露的
内容。

历史工具中的两条命令被有意省略。没有 `!` shell 转义：解析敌意网络输入的程序
不会被赋予启动 shell 的权限。没有 `slc check`，因为 RFC 1184 未给它任何与
`slc export` 不同的线上形式。套接字接口不暴露 TCP 紧急数据，因此 Synch 仅以
Data Mark 的形式传送。当标准输入到达文件末尾时——例如 `telnet host 80 < request`
这样的重定向调用——只关闭发送方向，会话继续读取，直到远端主机也关闭为止，因此
响应不会像历史工具那样被丢弃。

## OPTIONS

- `-4, --ipv4` — 仅通过 IPv4 连接。
- `-6, --ipv6` — 仅通过 IPv6 连接。
- `-8, --binary` — 请求双向的 8 位数据通路。
- `-L, --eight-bit-output` — 仅请求输出方向的 8 位数据通路。
- `-E, --no-escape` — 不设转义字符；每次按键都送往主机。
- `-e, --escape <char>` — 设置转义字符（`^]`、`^A`、单个字符，或留空表示无）。
- `-a, --login` — 通过 `NEW-ENVIRON` 导出会话的登录名。
- `-l, --user <name>` — 将 `name` 导出为登录名（隐含 `-a`）。
- `-b, --bind <address>` — 连接前绑定该本地地址。
- `-d, --debug` — 在标准错误上跟踪选项协商。
- `-?, --help` — 显示本命令自带的简短帮助。

## EXAMPLES

- `telnet example.test` — 在指定的 telnet 端口上打开会话。
- `telnet 10.0.2.2 25` — 手工与邮件服务对话。
- `telnet -6 fe80::2` — 仅通过 IPv6 连接。
- `telnet -l ada host` — 以 `ada` 作为登录名。
- `telnet -8 host` — 请求双向 8 位通路。
- 先 `telnet`，再 `open host` — 从命令提示符建立连接。

## EXIT STATUS

- `0` — 会话已进行（无论远端主机如何结束它），或已写出简短帮助。
- `1` — 无法建立会话：主机无法解析、套接字被拒绝，或终端无法切换到原始模式。
- `2` — 未能理解命令行。

## ENVIRONMENT

- `TERM` — 通过 TERMINAL TYPE 选项上报给主机。
- `USER` — `-a` 导出的登录名。
- `LANG` — 简短帮助首选的区域设置（如 `zh-CN` 这样的 BCP-47 标记）。

## SEE ALSO

- `host`
- `ping`
- `ss`
- `man`
