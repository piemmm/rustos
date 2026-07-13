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
更改会在其使用方下次启动时生效（`os.loginType`：下次启动的登录）。

键的集合是封闭的：未知的键，或超出某键取值集合的值，会在指明有效
选项后被拒绝，且不做任何更改。更改设置会以规范形式重写存储，并需要
对 `/System/Settings` 的写权限——普通账户可以读取设置但不能更改。

- `os.loginType` — `text` 或 `graphical`：登录服务为已认证用户启动的
  会话类型。`text`（默认）启动账户的 shell——仍可用 `desktop` 命令
  按需启动桌面；`graphical` 在认证后直接启动桌面会话（已安装桌面
  时），无桌面时退回文本。

## OPTIONS

- `-h, -?` — 显示本命令自己的简短帮助。

## EXAMPLES

- `configure` — 列出全部设置。
- `configure os.loginType` — 显示默认会话类型。
- `configure os.loginType graphical` — 启动到图形登录。

## EXIT STATUS

- `0` — 列表、取值、简短帮助或更改已完成。
- `1` — 无法读取或写入存储（例如调用者无权更改系统设置），或输出
  无法送达。
- `2` — 无法理解命令行、键未知，或值超出该键的取值集合。

## ENVIRONMENT

- `LANG` — 简短帮助的首选语言（BCP-47 标签，如 `fr-FR`）。

## SEE ALSO

- `man`
