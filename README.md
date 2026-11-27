# 备份管理器

自托管的备份管理器：网页配置任务，定时把本地目录 / MySQL 转储备份到 115。

> **硬约束：所有对 115 的读写都经过 CloudDrive2（CD2）。**
> 程序只认识 CD2 的地址和令牌，调用 CD2 的 gRPC 接口，由 CD2 去访问 115。
> 程序里没有 115 的地址、Cookie 或令牌，也不使用挂载盘、WebDAV 或 115 开放平台接口。
>
> 之所以这么设计：往挂载盘拷贝拿不到"传完了"的信号，也就没法校验、没法在云端轮转旧备份；
> CD2 的接口能拿到上传任务状态和文件大小。

## 功能

- **三类任务**
  - 目录镜像：增量上传，可选"源端删除时云端也删"
  - 目录归档：每次运行打一个 `tar.zst`（可选 `age` 加密），保留最近 N 份
  - MySQL / MariaDB：连库导出（默认，`mariadb-dump`，不锁表），或把 datadir 当文件备份（打包归档 / 按文件镜像）
- **调度**：网页里选"每天 / 每周 / 每月 / 每小时 / 自定义 cron"，时间用时间选择器；支持停机后 24 小时内补跑一次
- **运行记录**：每轮的统计与完整日志，可在网页里看；失败会带上原因
- **上传校验**：只核对大小（不计算 SHA1）；先传临时名、校验通过再改成正式名，旧版本在此之前一直保留
- **风控保护**：全局串行、文件/列目录/删除最小间隔、连续失败次数上限、触发风控后自动冷却
- **目的地可扩展**：代码里是 `Destination` trait，首版实现 CD2

## 快速开始

前置：一台常开机器（Linux + Docker Compose v2），以及一个**能访问到的 CD2**（同机或同网段都行）。

### 方式一：用现成镜像（推荐）

镜像发布在 GitHub Container Registry，支持 `linux/amd64` / `linux/arm64`，可匿名拉取：

```bash
docker pull ghcr.io/foreverjin/quantumark:latest
```

新建一个部署目录（不需要克隆源码），写入 `docker-compose.yml`：

```yaml
services:
  quantumark:
    image: ghcr.io/foreverjin/quantumark:latest
    container_name: quantumark
    restart: unless-stopped
    user: "1000:1000"                        # 与 ./data 的属主一致
    environment:
      QM_DATA_DIR: /data
      QM_BIND: 0.0.0.0:8790
      QM_ADMIN_PASSWORD: ""                   # 留空则首次启动打印设置码
      QM_LOG: quantumark=info
      TZ: Asia/Shanghai
    volumes:
      - ./data:/data                          # 状态、密钥、日志
      - /:/host:ro                            # 网页里可浏览/备份的宿主机目录
      - ./inbox:/upload:rw                    # 可选：开启“上传后删除本地”时才需要，不用可删
    ports:
      - "8790:8790"
    read_only: true
    tmpfs:
      - /tmp:size=64m,mode=1777
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true
    healthcheck:
      test: ["CMD", "curl", "-fsS", "-o", "/dev/null", "http://127.0.0.1:8790/health"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s
```

首次启动（`/data` 必须可写，且属主与 `user` 一致）：

```bash
mkdir -p data && sudo chown -R 1000:1000 data
docker compose up -d
docker compose logs -f      # 找“初始设置码”
```

打开 `http://<机器 IP>:8790`，然后按下面的「基本使用」配置。

更新到最新镜像：

```bash
docker compose pull
docker compose up -d
```

### 方式二：从源码构建

```bash
cd quantumark
cp .env.example .env     # 按需改：端口、源目录、UID
docker compose up -d --build
docker compose logs -f
```

仓库里的 `docker-compose.yml` 已包含下面的完整配置，`.env` 里的 `QM_*` 会代入 compose 文件。

## compose 部署配置说明

| 配置 | 作用 | 注意 |
| --- | --- | --- |
| `user` | 容器运行用户 | 必须与 `./data` 的属主一致，否则 SQLite 打不开 |
| `./data:/data` | SQLite、加密密钥、运行日志 | 迁移时整个目录一起搬；`secret.key` 丢了凭据要重填 |
| `/:/host:ro` | 只读浏览宿主机目录 | 想缩小范围改成 `/vol1:/host:ro`，容器里就只看得到 `/vol1` |
| `./inbox:/upload:rw` | 可写的“收件箱” | 只有开启“上传后删除本地”才需要，目录属主也要是运行用户；不用可删掉这行 |
| `ports` | 网页端口 | 默认 `0.0.0.0:8790` 允许局域网访问；只本机访问改 `127.0.0.1:8790:8790` |
| `read_only` + `tmpfs` | 根文件系统只读 | 程序只写 `/data`、`/upload`、`/tmp` |
| `TZ` | 时区 | 影响 cron 触发时间和归档文件名 |
| `cap_drop: ALL` | 去掉 Linux capabilities | 程序不需要任何特权 |

几个易错点：

- 容器**不以 root 运行**。`./data` 的 UID/GID 不是 1000 时，把 `user` 和 `chown` 一起改。
- 宿主机路径是按**容器内路径**选的：备份 `/vol1/1000/media`，在网页浏览里选 `host → vol1 → 1000 → media`，不要填宿主机的绝对路径。
- 不要把网页端口或 CD2 端口暴露到公网；令牌等同于密码，跨网段请用 VPN。

## 基本使用

### 1. 首次登录

- `QM_ADMIN_PASSWORD` 留空：启动日志会打印一次性的**初始设置码**，打开网页输入它并设置密码。
- 填了 `QM_ADMIN_PASSWORD`：直接用这个密码登录，要求至少 8 位。

设置完成后设置码失效。忘记密码见「重置管理员密码」。

### 2. 添加目的地

「目的地」→ 新增，填：

| 字段 | 示例 | 说明 |
| --- | --- | --- |
| 地址 | `http://192.168.8.114:19798` | CD2 的 gRPC 地址，带 `http://`，默认端口 19798 |
| 根路径 | `/115open` | 115 在 CD2 里的路径；任务的远端目录必须位于它下面 |
| API Token | `...` | 在 CD2 网页创建，权限见「CD2 令牌权限」 |

保存后依次点 **测试连接**、**自检**。自检会上传一个 64 KiB 随机文件、读回比对内容再删掉，用来确认整条上传链路。

不想手动建 Token，也可以填 CD2 的账号密码，点“用账号密码生成最小权限 Token”；账号密码只用于这一次调用，不保存。

### 3. 新建任务

「备份任务」→ 新建，选类型：

| 类型 | 适合 | 远端产物 |
| --- | --- | --- |
| 目录镜像 | 照片、文档等需要“云端和本地一致”的目录 | 与原目录同结构的文件 |
| 目录归档 | 需要留多个历史版本的目录 | 每次一个 `tar.zst[.age]`，保留最近 N 份 |
| MySQL | 数据库备份；再选 mode：`dump` 连库导出（默认，运行中可用）/ `archive` 打包数据目录 / `mirror` 同步数据目录 | 每库一个 `*.sql.zst[.age]`，或数据目录的归档/文件镜像 |

填写要点：

- **源目录**：点「浏览」从 `/host` 里选（只读）。
- **远端目录**：必须在目的地根路径之下，例如 `/115open/backup/media`。
- **调度**：每天 / 每周 / 每月 / 每小时，或自定义 cron（5 段，时区跟随 `TZ`）。
- **排除规则**：镜像任务支持 glob，默认排除常见的临时文件。
- **加密**：归档/导出可选 `age` 公钥加密；私钥只显示一次，容器不保存，丢了无法解密。

### 4. 运行与查看

- **立即运行**：不等调度手动跑一次；运行中可点**停止**。
- **预览**（仅镜像任务）：只计算会新增/修改/删除哪些文件，不上传、不删除。
- **运行记录**：每轮的统计、错误原因和完整日志；失败时先看这里的最后几行。
- 任务全局串行：前一个任务没结束，后一个会排队。

### 5. 维护

```bash
# 更新镜像
docker compose pull && docker compose up -d

# 看日志
docker compose logs -f quantumark
```

- `/data` 也要备份：它记录了“云端文件对应本地哪次修改”，丢了会触发重新认领（多一次列目录配额）。
- 索引异常或想强制重新认领时，在「备份任务」里点该任务的「重置索引」，下次运行会重新读一次远端目录。

## 重置管理员密码

忘记密码时在容器里执行（比 8 位短的密码只适合可信内网）：

```bash
docker compose exec quantumark /usr/local/bin/quantumark --set-password 新密码
```

## CD2 令牌权限

在 CD2 网页创建 API Token，建议把**根目录限制**到 `/115open`（或更窄的备份目录），权限勾选：

| 权限 | 用途 |
| --- | --- |
| 列目录 | 首次认领、大小兜底查询 |
| 建文件夹 | 建备份目录 |
| 建文件 + 写入 | 上传 |
| 改名 | 临时文件转正 |
| 删除 | 同步删除、保留份数轮转、自检清理 |
| 查看传输任务 | 等 CD2 把文件真正传到 115 |

空间信息可选择开启（仪表盘会显示 115 剩余空间）。

嫌麻烦的话，也可以在目的地表单里填 CD2 的**账号密码**，点"用账号密码生成最小权限 Token"：
程序会调用 CD2 的 `CreateToken` 生成令牌，账号密码只用这一次、不保存。

## 任务说明

### 目录镜像

- 以本地索引为准判断增删改，**不重复扫描云端目录**（列目录在 115 侧要计配额）。
- 只有**索引为空**的首次运行会读一次云端备份目录，把大小一致的已有文件认领进来（不校验内容）；之后不再读。
  如果索引丢了，在「备份任务」里点该任务的「重置索引」，下次运行会重新认领。
- **同步删除**默认关闭。开启后只删"索引里记录过、且本次源端不存在"的文件，删除会进 115 回收站。
- **上传后删除本地**（移动式备份）默认关闭。开启后：上传成功的文件会被删掉（或移到本地回收目录），
  本地不留存；本次跳过但索引里已记录的旧文件也会一并清理。和"同步删除"互斥。
  删除本地只以索引记录为准，**不做内容校验**；开启前请确认索引可信。
  删除本地要求源目录是**可写挂载**（`docker-compose.yml` 里的 `QM_UPLOAD_DIR` → `/upload`），
  只读挂载下删除会失败并在日志里告警，不会假装成功。
- 源目录为空时**不会**执行同步删除（防止挂载失败把云端清空）。
- 多台设备汇总备份（客户端经 SMB 投递到 Armbian，再由一个镜像任务上传）见
  「[多设备推送（SMB 中转）](#多设备推送smb-中转)」。

### 目录归档

- `tar -cf - -C <上级目录> <目录名> | zstd | [age -r <公钥>]` 流式上传，不落临时文件。
- 打包进程任何一段报错，本次归档会被删除、不记录，避免留下不完整的包。
- 保留份数按同一远端目录下的归档计算。

### MySQL / MariaDB

三种方式，新建任务时选：

**① 连数据库导出（默认，推荐）**

- 填数据库地址、端口、用户名、密码（密码加密存库，接口不回显），库名留空表示所有非系统库
- 用 `mariadb-dump`（随 mariadb-client 安装，兼容 MySQL 8.x）导出，参数含
  `--single-transaction --quick --routines --triggers --events --hex-blob --no-tablespaces`
- **不锁表、不用停库**；每个库一个文件：`<远端目录>/<库名>/<库名>-<时间>.sql.zst[.age]`
- 账号只要有对应库的读权限即可（`SELECT`，导出存储过程/事件需要相应权限；全库备份用 root）
- 单个库失败不影响其它库；连续失败到阈值整轮停

**② 打包数据目录 / ③ 按文件同步数据目录**

- 备份对象是数据目录文件（`ibdata1`、`*.ibd`、redo/undo、系统库），不连数据库
- 数据目录要在网页里浏览选择（宿主根目录只读挂在 `/host`，datadir 一般是 `/host/var/lib/mysql`）
- **一致性**：数据库运行中拷贝可能不一致。程序会检测 `*.sock`/`*.pid` 并告警，但不会替你停库——
  建议停库或用文件系统快照。一般优先用①
- 默认排除临时文件：`ibtmp1`、`#innodb_temp`、`*.pid`、`*.sock`、`*.err`；redo/binlog 不排除

## 多设备推送（SMB 中转）

多台电脑各自要备份一个目录时，不必给每台建任务：让它们把文件推进 Armbian 的 SMB 共享，
quantumark 用一个镜像任务上传到 115，上传成功后把共享里的文件删掉。共享目录就是投递队列，
服务端只认一个可写的投递目录（下面以 `/upload` 为例；已有部署也可能挂成 `/backuphub`
之类的路径，以 `docker inspect` 里的挂载为准）。

```text
Linux / macOS / Windows ──SMB 推送──> Armbian:/srv/smb/quantumark-inbox
                                             │
                                     镜像任务（源 /upload）
                                             │
                                           115
```

### 服务端

1. 建一个**专用**投递目录（不要用现有共享的根目录——上传成功的文件会被删掉）：

   ```bash
   sudo mkdir -p /srv/smb/quantumark-inbox
   sudo chown -R 1000:1000 /srv/smb/quantumark-inbox   # 与 QM_UID/QM_GID 一致
   ```

   一定要先建目录再启动容器：Docker 发现 bind 源不存在时会用 root 建一个空目录，
   容器里的 uid 1000 就删不掉文件。

2. Samba 导出它，并让写入的文件属主/权限和容器用户一致，否则容器能读不能删：

   ```ini
   [quantumark-inbox]
      path = /srv/smb/quantumark-inbox
      read only = no
      force user = qm          # uid 1000，与 QM_UID 一致
      create mask = 0644
      directory mask = 0775
   ```

3. `.env` 里把可写收件箱指过去，然后重建容器（`docker-compose.yml` 本身不用改）：

   ```bash
   # .env
   QM_UPLOAD_DIR=/srv/smb/quantumark-inbox
   ```

   ```bash
   docker compose up -d          # 没生效就加 --force-recreate
   docker compose config | grep -A1 upload   # 复核挂载
   ```

4. 新建**目录镜像**任务：

   - 源目录填**可写挂载点**（不是 `/host/...`，那边只读，删不掉文件）。本仓库 compose 默认是
     `/upload`；已有部署可能额外挂成 `/backuphub` 之类，用 `docker inspect` 看实际挂载名
   - 远端目录例如 `/backup/inbox`
   - 打开「上传后删除本地」，关闭「同步删除」，回收目录留空
   - 保留默认排除规则（含 `*.part`/`*.tmp`）；如果客户端脚本或日志放在源目录里，把名字补进排除
   - 调度建议 1–2 小时一次或凌晨多次：投递目录为空时任务几秒就结束，多跑没有额外开销

### 客户端

仓库里的 `scripts/client-push/` 提供两个脚本：只发送新增/变化的文件，先写 `xxx.part` 再
rename 成正式名，并保留源文件 mtime。**脚本、日志、SMB 凭据都不要放在被备份的目录里**
（会被一起传上 115）。

默认保留源目录名，一台设备一层客户端目录：

```text
--src /data/photos --client laptop  ->  <dest>/laptop/photos/<相对路径>
--flat                              ->  <dest>/laptop/<相对路径>
--subdir 2026/photos                ->  <dest>/laptop/2026/photos/<相对路径>
```

同一客户端可以推多个目录，清单按“布局”（客户端 + 子目录）分别记录；改了 `--src` 的目录名
或 `--subdir` 会被当成新布局，重新全量发送一次。

Linux / macOS（需要 Python 3，无第三方依赖）：

```bash
python3 qm_push.py --src /data/photos --dest /mnt/quantumark-inbox --client laptop-jin
python3 qm_push.py --src /data/photos --dest /mnt/quantumark-inbox --client laptop-jin --dry-run
python3 qm_push.py --src /data/photos --dest /mnt/quantumark-inbox --client laptop-jin --flat
```

已发送清单默认在 `~/.local/state/quantumark-push/<client>.json`，可用 `--state-dir` 改；
`--full` 强制重发当前布局。挂载共享可以用 `mount -t cifs`（Linux）或 Finder / `mount_smbfs`
（macOS），凭据放在系统钥匙串或 credentials 文件里。

Windows：

```powershell
.\qm_push.ps1 -Source D:\photos -Dest \\armbian\quantumark-inbox -Client mypc
.\qm_push.ps1 -Source D:\photos -Dest \\armbian\quantumark-inbox -Client mypc -Subdir 2026\photos
```

清单默认在 `%LOCALAPPDATA%\quantumark-push`；用 SYSTEM 跑计划任务时加
`-StateDir C:\ProgramData\quantumark-push`。用 UNC 路径前先让运行账户能访问共享（`net use`
或凭据管理器）。脚本头部注释里有 `schtasks` 示例。

脚本和双击启动器也放了一份到共享的设备目录（`<dest>/Mac/`、`<dest>/PC/`）。用的时候把
对应文件一起复制到需要备份的根目录：

- **macOS**：复制 `qm_push.py` + `qm_push.command`，双击 `qm_push.command`。首次从共享复制
  过来可能没有执行权限，在终端执行一次 `chmod +x qm_push.command` 即可。默认挂载点不存在时，
  启动器会先在已挂载的 SMB 卷里找 `BackupHub`，找不到就自动打开 `smb://` 连接窗口。
- **Windows**：复制 `qm_push.ps1` + `qm_push.cmd`，双击 `qm_push.cmd`。
- 启动器里已经写好了共享路径和客户端名（`Mac` / `PC`）；换机器或挂载点不同时，改文件开头
  的 `DEST` / `CLIENT`（macOS）或 `QM_DEST` / `QM_CLIENT`（Windows）。

也可以继续用命令行（`--src .` 会保留当前目录名）：

```bash
# macOS
cp /Volumes/BackupHub/Mac/qm_push.py /Users/mrqiang/stm32/
cd /Users/mrqiang/stm32
python3 qm_push.py --src . --dest /Volumes/BackupHub --client Mac
```

脚本放在源目录里没关系：客户端会直接跳过 `qm_push.py` / `qm_push.ps1` / `qm_push.command` /
`qm_push.cmd`（以及 `.DS_Store`、`._*`、`*.tmp`/`*.part` 这类文件），不会复制到共享；
服务端的排除规则同样会忽略它们。**不要改脚本名**，或者把新名字同步加进排除规则。

复制性能：默认 **8 个并发**、**不逐文件 fsync**（SMB 上每个 fsync 都是一次同步往返，文件
一多会非常慢）。文件很多时可以试 `--jobs 16`（双击启动器可设 `QM_JOBS=16`，上限 64）；
只有确实需要“写完立刻落盘”时才加 `--fsync`。Finder 直接粘贴看起来快，是因为它不做增量
清单和原子改名，而且每次都得整份重传；脚本首次全量会多出少量元数据开销，之后只发变化的文件。

第一次想省时间，可以先 Finder 把整棵树粘贴到目标目录，再跑一次脚本（或加 `--adopt`）：
脚本会把目标里大小 + mtime（1 秒容差）一致的文件认领进清单，不会重抄一遍；之后正常增量。
认领不做内容校验，同大小且 mtime 也相同的变化识别不了。

清单按“目标共享 + 布局”记录：换挂载点 / 换共享会自动重新全量推一次（服务端按大小 + mtime
去重，不会重复上传）。如果共享里的文件被手工删掉，客户端不会自动发现，用 `--full` 重推
（双击启动器前 `export QM_FULL=1`）。

定时任务示例（客户端先推，服务端任务晚一点跑；两者重叠也不会传半截文件）：

```cron
10 2 * * * /usr/bin/python3 /opt/quantumark/qm_push.py --src /data/photos --dest /mnt/inbox --client laptop-jin
```

### 注意

- **投递目录是队列，不是存储**：上传成功就会被删，不要手工清理或搬动；客户端源目录才是权威。
- **删除不联动**：客户端删掉文件，不会删 115 上的旧副本（同步删除默认关）。
- **115 上每个路径只保留最新一份**，不保留历史版本；要历史用目录归档任务。
- 变化判断是大小 + mtime，客户端必须保留 mtime（脚本已经处理）；用别的工具推送时注意。
- 服务端删除本地前会再 stat 一次，任务运行期间客户端换上的新版本会留到下一轮；但同大小、
  同 mtime 的内容替换识别不了（见「已知限制」）。
- 以 `.tmp` / `.part` 结尾的真实文件会被默认排除规则跳过，尽量别用这种名字；macOS 的
  `._*` 元数据文件和 `.DS_Store` 也在默认排除里。
- 客户端脚本 `qm_push.py` / `qm_push.ps1` 在默认排除里：放在源目录不会上传、也不会被删；
  改了脚本名就要同步把新名字加进排除规则。
- 客户端子目录在上传清空后会被清理掉（空目录不保留），脚本每次写入前都会重建，属正常现象。
- 投递目录要留够磁盘：每轮任务之间新增的数据都先落在 Armbian 上。

## 恢复（手动）

归档/转储都是标准格式，用同目录下的命令即可：

```bash
# 连库导出的转储
age -d -i key.txt inventory_db-20261125-030000.sql.zst.age | zstd -d | mariadb -h 192.168.8.139 -u admin -p inventory_db
# 未加密时
zstd -d inventory_db-20261125-030000.sql.zst | mariadb -h 192.168.8.139 -u admin -p inventory_db

# MySQL 数据目录（务必先停库，还原到一个空目录）
age -d -i key.txt mysql-data-20261125-030000.tar.zst.age | zstd -d | tar -xf - -C /var/lib/mysql
# 未加密时
zstd -d mysql-data-20261125-030000.tar.zst | tar -xf - -C /var/lib/mysql

# 普通目录归档
age -d -i key.txt media-20261125-030000.tar.zst.age | zstd -d | tar -xf - -C /path/to/restore
```

镜像任务的文件在 115 上就是普通文件，直接在 115 网页里下载即可。

`age` 密钥对在"设置 → 任务表单 → 生成密钥对"里生成，**私钥只显示一次**，容器不保存；
丢了私钥就无法解密对应归档。

## 风控与限速（重要）

115 对列目录/上传有访问配额，超额会返回 `770004`（文档里记录过多次触发，恢复时间从几分钟到次日不等）。
默认策略是按现有项目经验定的，都在「设置」页可调：

| 项 | 默认 | 说明 |
| --- | --- | --- |
| 任意 CD2 调用间隔 | 200 ms | 全局串行，避免并发冲击 |
| 文件之间间隔 | 300 ms | 小文件突发上传最容易打满配额 |
| 列目录间隔 | 1000 ms | 仅首次认领时用 |
| 删除间隔 | 1000 ms | 同步删除与轮转 |
| 上传状态轮询 | 10 s | 读的是 CD2 本地任务列表，不走 115 |
| 连续失败中止 | 3 次 | 避免循环重试放大拦截 |
| 风控冷却 | 60 分钟 | 命中风控后定时任务暂停，可手动触发 |

**不要把网页或 CD2 的端口暴露到公网。** 令牌等同于密码，请放在可信内网，跨网段用 VPN。

## 环境变量

程序会读取的环境变量（`TZ` 由 chrono 使用）：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `QM_DATA_DIR` | `/data` | 数据目录（SQLite、密钥、运行日志） |
| `QM_BIND` | `0.0.0.0:8790` | 网页监听地址 |
| `QM_ADMIN_PASSWORD` | 空 | 首次启动的管理员密码；留空则打印一次性设置码 |
| `QM_LOG` | `quantumark=info` | 日志级别（`RUST_LOG` 风格） |
| `TZ` | 系统时区 | 定时任务与文件名使用这个时区 |

`docker-compose.yml` / `.env` 里还会用到这些（由 compose 代入，程序本身不读）：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `QM_HOST_ROOT` | `/` | 宿主机上挂到 `/host`（只读）的路径，决定网页里能选到哪些目录 |
| `QM_UPLOAD_DIR` | `./data/empty-upload` | 宿主机上挂到 `/upload`（可写）的路径，“上传后删除本地”要用它 |
| `QM_UID` / `QM_GID` | `1000` | 容器用户；`./data` 必须归它所有 |
| `QM_PORT` / `QM_PUBLISH` | `8790` / `0.0.0.0` | 网页端口与绑定地址 |
| `QM_TAG` | `0.1.1` | 本地构建的镜像标签 |
| `BUILDER_BASE` / `RUNTIME_BASE` | `rust:1-bookworm` / `debian:bookworm-slim` | 构建用的基础镜像（被墙时可换镜像站） |
| `CARGO_BUILD_JOBS` | `4` | 构建并行度 |
| `CARGO_REGISTRY_MIRROR` | 空 | crates.io 镜像，例如 `sparse+https://rsproxy.cn/index/` |

## 数据与状态

- `/data/quantumark.db`：任务、目的地、运行记录、文件索引（SQLite，WAL）
- `/data/secret.key`：敏感字段加密密钥（0600）。**这个文件丢了，已保存的令牌/密码就解不开，需要重新填**
- `/data/logs/`：每轮运行的日志，保留 30 天

备份管理器自己的 `/data` 也值得备份——它记录了"云端哪个文件对应本地哪次修改"。
你可以再建一个目录归档任务把 `/data` 打进去（注意其中含加密密钥）。

## 镜像与构建

### 现成镜像

- 地址：`ghcr.io/foreverjin/quantumark`
- 架构：`linux/amd64`、`linux/arm64`（多架构 manifest，`docker pull` 会自动选择）
- 标签：

| 标签 | 来源 |
| --- | --- |
| `latest` | 最近一次构建（标签或手动触发） |
| `sha-<短提交>` | 对应提交，便于回滚（不发布版本号标签） |

回滚时把 compose 里的镜像固定到某个提交，再重新拉起：

```yaml
image: ghcr.io/foreverjin/quantumark:sha-abc1234
```

```bash
docker compose pull && docker compose up -d
```

### 本地构建

```bash
# 当前架构
docker compose build

# 交叉编译 arm64
docker buildx build --platform linux/arm64 -t quantumark:0.1.1-arm64 --load .
```

arm64 的编译阶段在构建机上交叉完成，但 runtime 阶段安装 `mariadb-client` / `zstd` / `age` 需要能执行 arm64 二进制。
构建机没注册 binfmt 时会报 `exec format error`：在目标机器上原生 `docker compose build`，
或执行 `docker run --privileged --rm tonistiigi/binfmt --install arm64` 注册一次。

### CI

`.github/workflows/ci.yml`：

- push 到 `main` / PR：只跑 `cargo test --locked`，不构建 Release、不推镜像。
- 推送 `v*` 标签：测试通过后构建 Release 二进制，并推送 amd64 + arm64 镜像（`sha-*` / `latest`）。
- 手动触发：走标签的构建流程，镜像打 `sha-*` / `latest`。

## 已知限制

- 单实例部署，任务全局串行（这是刻意的：115 的配额不适合并发）
- **不做内容校验**：上传后只核对大小，不计算 SHA1；首次认领也按大小认领，同名同大小但内容不同的文件不会被发现
- **上传后删除本地**（SMB 中转）只能按大小/mtime 判断，删除前会复核一次；客户端必须保留 mtime
- SMB 中转时目标目录会被清空，普通 rsync 增量不适用，客户端需自备“已发送清单”（见「多设备推送」）
- 镜像任务在 115 上每个路径只保留最新版本；需要历史版本用目录归档任务
- 单个大文件失败后从头重传（暂不支持断点续传）
- MySQL 数据目录只做文件级拷贝，不保证运行中拷贝的一致性（见上文）
- 恢复是手动的（后续版本再加界面）
- 目录归档需要在本机临时读一遍源目录，源目录很大时耗时较长
- 首次认领最多读 5000 个远端目录，超过就停止认领（其余按需要上传处理）

## 开发

```bash
$HOME/.cargo/bin/cargo test --locked      # 单元测试，不联网
$HOME/.cargo/bin/cargo build --release --locked
rustfmt --edition 2024 --check src/*.rs src/**/*.rs
```

代码约定和"不要违反的约束"见 [AGENTS.md](AGENTS.md)。
