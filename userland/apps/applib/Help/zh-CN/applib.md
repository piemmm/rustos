## NAME

applib — 管理桌面的程序库

## SYNOPSIS

`applib [list [--category <folder>]]`

`applib add <bundle> [--category <folder>] [--name <name>] [--icon <asset>] [--user]`

`applib remove <id|bundle> [--user]`

`applib hide <id> [--user]`

`applib show <id> [--user]`

`applib rescan [--user]`

## DESCRIPTION

管理程序库——即桌面启动器显示的按文件夹组织的启动程序目录。
程序库是卷上的数据，绝非固化列表：一个是位于
`/System/Settings/ProgramLibrary/library.conf` 的系统级存储，供每个账户读取；
另一个是位于用户自身 `Settings/` 下相同路径的可选用户级覆盖。
启动器显示的内容是两者合并解析的结果：用户自己的条目和调整优先于系统级的设置。

不带子命令（或使用 `list`）时，解析后的程序库将按文件夹逐行打印，
每行一个条目：标识符、显示名称和包路径——这与启动器显示的内容完全一致。
文件夹属于封闭集合：`Accessories`、`Graphics`、`Internet`、`Multimedia`、
`Office`、`Programming`、`Games`、`SystemTools`、`Utilities` 和 `Other`；
不支持自定义名称的文件夹。

`applib add` 注册一个应用程序包。其身份、显示名称、文件夹和图标取自包自身签署的
`AppInfo` 清单；使用 `--category`、`--name` 和 `--icon` 会覆盖清单中的设置。
如果清单未声明程序库文件夹，则包必须指定 `--category`——本工具从不进行推测。
`applib remove` 删除一个记录，通过其标识符或注册时使用的包路径来命名。

`applib hide` 从解析后的程序库中屏蔽某个条目，但不删除其记录——其标识符仍被占用，
因此随后的 `rescan` 无法恢复它——使用 `applib show` 可重新显示它。
隐藏仅关乎展示，而非权限：无论目录如何设置，启动包始终受加载器的签名和
能力（capability）检查约束。

`applib rescan` 遍历应用程序存储（`/System/Commands`、`/System/Applications`
和 `/Apps`，或者在指定 `--user` 时遍历调用者自身的 `<home>/Commands` 和
`<home>/Applications`），读取每个包的清单，
并注册每一个请求列出且尚未编目的应用程序。现有记录（包括管理员的重命名和屏蔽）
绝不会被干扰，且清单不可读或畸形的包将被跳过并计数，绝不会因此中止任务。
这就是新系统的程序库如何根据实际安装的包自动填充，而无需任何手动维护的列表。

默认情况下，该工具编辑系统级存储，只有受 `/System/Settings` 写入策略准入的主体
才能更改；普通账户可以读取它，但可以通过 `--user` 使用自己的覆盖层进行个性化。
被拒绝的写入会说明原因且不更改任何内容。

成功时，该工具在标准输出上保持静默；更改的结果将以结构化建议记录的形式在
标准信息流 (fd 3) 上发出，脚本可以通过 `3>records.jsonl` 捕获该记录，
其他程序可以忽略它。

## OPTIONS

- `--category <folder>` — 配合 `list` 使用时，仅显示该文件夹；配合 `add` 使用时，
  将条目归类到其下（覆盖清单的声明）。
- `--name <name>` — 配合 `add` 使用时，指定要显示的显示名称，而非清单中的名称。
- `--icon <asset>` — 配合 `add` 使用时，指定图标资产（包内 `Resources/` 下的文件名），
  而非清单中的图标。
- `--user` — 将更改应用于调用者自身的覆盖层（或者在 `rescan` 时遍历调用者自身的
  `<home>/Commands` 和 `<home>/Applications`），而非系统级存储。
- `-h, -?` — 显示本命令自己的简短帮助。

## EXAMPLES

- `applib` — 按文件夹显示解析后的程序库。
- `applib list --category Games` — 显示单个文件夹。
- `applib add /Apps/chess.app` — 按清单要求注册一个包。
- `applib add /Apps/tool.app --category Utilities --name "Disk Tool"` —
  在一个明确的文件夹下注册一个未声明列出的包。
- `applib remove os.tairix.chess` — 按标识符删除一个条目。
- `applib hide os.tairix.chess --user` — 仅从你自己的程序库中隐藏它。
- `applib rescan` — 注册每个已安装、已列出但尚未进入系统目录的包。

## EXIT STATUS

- `0` — 列表、更改、重新扫描或简短帮助已完成。
- `1` — 存储、包或输出失败（例如，调用者无权更改系统级目录）；原因在诊断流中说明。
- `2` — 命令行无法理解，文件夹或条目未知，或包无法按要求注册。

## ENVIRONMENT

- `LANG` — 简短帮助的首选语言（BCP-47 标签，如 `fr-FR`）。
- `HOME` — 调用者的主目录：命名用户级覆盖层和 `--user` 重新扫描根目录 `<home>/Commands` 和 `<home>/Applications`。

## SEE ALSO

- `man`
- `configure`
