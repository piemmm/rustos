## NAME

configure — 读取并设置启动时的系统配置

## SYNOPSIS

`configure [<key> [<value>]]`

## DESCRIPTION

列出、显示并设置位于
`/System/Settings/Configuration/system.conf` 的配置存储中的各项设置。
不带操作数时，列出每项设置及其当前值；只给出键时，显示该设置的值；
给出键和值时，更改该设置。

该存储位于加密的根卷上，其使用方在根文件系统解锁后才读取它；因此
更改会在其使用方下次启动时生效（`os.loginType`：下次启动的登录；
`cache.*` 开关：下次启动的解锁）。

键的集合是封闭的：未知的键，或超出某键取值集合的值，会在指明有效
选项后被拒绝，且不做任何更改。更改设置会以规范形式重写存储，并需要
对 `/System/Settings` 的写权限——普通账户可以读取设置但不能更改。

- `os.loginType` — `text` 或 `graphical`：登录服务为已认证用户启动的
  会话类型。`graphical`（默认）在认证后直接启动桌面会话，在无法运行
  桌面的机器上退回文本登录；`text` 启动账户的 shell——仍可用
  `desktop` 命令按需启动桌面。
- `cache.all` — `on` 或 `off`：缓存的总开关。`on`（默认）让下面每
  个缓存类别遵循各自的设置；`off` 是一个上限，无论各类别的
  设置如何，都禁用所有内存缓存。
- `cache.filesystem`、`cache.block`、`cache.transform`、
  `cache.semantic` — `auto` 或 `off`：针对四个可回收内存缓存
  （文件系统、整盘块、解压后的簇以及应用启动缓存）的分
  类开关。`auto`（默认）让内存压力管理器控制该类别；`off`
  完全禁用它。没有分类的 `on`：无法强制某个类别忽略内存
  压力。只要 `cache.all` 为 `off`，该类别实际上就是 `off`。

每个缓存都是可回收的加速器，绝不是真实来源，因此关闭其中
任何一个或全部，只会使受影响的工作变慢——绝不会改变结果。

- `net.ipv4.enabled`、`net.ipv6.enabled` — `true` 或 `false`：
  全栈地址族开关。两者默认均为 `true`。被禁用的地址族不绑定地址、
  不回应任何报文，并以带类型的错误拒绝该地址族的套接字——绝不
  静默丢弃。
- `net.ipv6.privacy` — `true` 或 `false`：栈是否在稳定地址之外
  生成临时（隐私）IPv6 地址。`false`（默认）仅使用稳定的 SLAAC
  地址。
- `net.tcp.syncookies` — `auto` 或 `always`：SYN 洪水防御策略。
  `auto`（默认）保持有界的半开队列，并在溢出时回退到无状态
  cookie；`always` 对每个连接请求都以无状态方式回应。没有 `off`
  ——不设防的连接队列不是一种设置。
- `net.tcp.keepalive` — `true` 或 `false`：TCP 连接是否在空闲链路上
  发送保活探测。`false`（默认）从不探测，也不断开空闲连接；`true`
  在通常的间隔后探测空闲对端，若其停止响应则断开连接。
- `net.tcp.ecn` — `true` 或 `false`：TCP 连接是否协商显式拥塞通知
  （ECN）。`false`（默认）使连接保持 Not-ECT；`true` 在握手中提供
  ECN，此后将拥塞标记视为减速信号，而不是强制丢包。
- `time.servers` — `none` 或以逗号分隔的网络时间服务器列表，每项为主机名
  或地址。`none`（默认）表示从不通过网络设置时钟：TAIRiX 没有自己的时间
  服务器池，因此指定服务器是管理者的选择。
- `time.refresh` — `6h`、`12h`、`1d`、`2d` 或 `7d`：时间已知后，两次时钟
  查询之间经过的运行时间。默认为 `1d`。未设置、不合理或过于陈旧的时钟，
  无论此设置为何，都会在网络可用后立即校正。
- `input.mouse.debounce` — 整毫秒，默认 `25`，`0` 表示关闭，最大 `100`：
  鼠标按键松开之后，多长时间内同一按键的下一次按下会被当作开关抖动忽略，
  而不是当作一次新的点击。磨损的微动开关可能在松开后几毫秒内报告第二次
  按下，而它本意只是一次点击。若鼠标的连点模式会刻意发送成对的点击，请
  设为 `0`。

`net.*` 设置由网络栈读取；更改在网络栈下次应用其配置时生效。

## OPTIONS

- `-h, -?` — 显示本命令自己的简短帮助。

## EXAMPLES

- `configure` — 列出全部设置。
- `configure os.loginType` — 显示默认会话类型。
- `configure os.loginType graphical` — 启动到图形登录。
- `configure cache.all off` — 禁用全系统的所有内存缓存。
- `configure cache.filesystem off` — 仅禁用文件系统缓存。

## EXIT STATUS

- `0` — 列表、取值、简短帮助或更改已完成。
- `1` — 无法读取或写入存储（例如调用者无权更改系统设置），或输出
  无法送达。
- `2` — 无法理解命令行、键未知，或值超出该键的取值集合。

## ENVIRONMENT

- `LANG` — 简短帮助的首选语言（BCP-47 标签，如 `fr-FR`）。

## SEE ALSO

- `man`
