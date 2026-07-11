## NAME

unmount — 分离已挂载的卷

## SYNOPSIS

`unmount [option...] name`

## DESCRIPTION

将挂载在 `name` 下的卷撤出服务：刷写文件系统和设备，撤除
`/Storage` 下的挂载，并收回该卷持久的 `id::` 根。`name` 是卷的
目录名（`usb1`）或其挂载点路径（`/Storage/usb1`），与系统信息
API 的挂载列表进行匹配。

设备在尚有未提交写入时被拔出的卷，会以 `unavailable-dirty`（或
`unavailable-lost`）的状态留在挂载列表中，普通的 `unmount` 会
拒绝：其保留的数据将为经过验证的重新插入而保存。`--force` 是
有意的出口 —— 丢弃保留数据、撤除该卷，并将损失记入审计日志。
对健康的卷，`--force` 仍会干净地刷写并分离；只要可以干净提交，
就不会丢弃任何数据。

分离需要挂载权限（`CAP_FS_MOUNT`）；内核会检查它并审计每个决定。
永久的启动卷和系统的视图绑定不可分离。

## OPTIONS

- `-f, --force` — 强制卸载：即使数据无法提交也撤除该卷，并丢弃
  保留的数据。
- `-?, --help` — 显示本命令的简短帮助。

## EXAMPLES

- `unmount usb1` — 干净地分离挂载为 `usb1` 的卷。
- `unmount /Storage/usb1` — 同样的操作，以挂载点命名。
- `unmount --force usb1` — 撤除不可用的卷并丢弃其保留数据。

## EXIT STATUS

- `0` — 卷已分离（或已输出简短帮助）。
- `1` — 找不到该卷、该卷不可分离，或内核拒绝了分离。
- `2` — 无法理解命令行。

## ENVIRONMENT

- `LANG` — 简短帮助的首选区域设置（BCP-47 标签，如 `zh-CN`）。

## SEE ALSO

- `mount`
- `df`
- `man`
