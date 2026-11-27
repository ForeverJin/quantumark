# AGENTS.md

## 项目概览

一个自托管的备份管理器：网页配置任务，定时把本地目录 / MySQL 转储备份到 115。
Rust 单二进制 + 内嵌单页界面，部署成一个 Docker 容器。

- 任务类型：目录镜像、目录归档（tar+zstd[+age]）、MySQL 备份（mode=dump 连库导出（默认）/ archive 打包数据目录 / mirror 同步数据目录）
- 目的地：首版只有 CD2（通过它的 gRPC API）
- 存储：SQLite（任务、运行记录、文件索引）+ 一个加密密钥文件

## 硬约束（不要违反）

1. **对 115 的读写一律经 CD2 的 gRPC API**（`crate::pb` / `src/cd2`）。
   不要引入 115 的直连通道：p115client、115 网页/开放平台接口、CD2 的 WebDAV、
   `GetDownloadUrlPath` 返回的 115 直链（`get_direct_url` 必须保持 `false`），
   也不要用 CD2 的挂载盘。
2. **程序里不出现 115 的地址、Cookie、令牌**：目的地配置只有"CD2 地址 + CD2 凭据 + 根路径"。
3. **列目录是最贵的操作**：115 侧列目录计入配额，超额返回 `770004`。
   除"索引为空时的一次性认领"外，不要新增扫描远端目录的逻辑；
   变化判断以 `files` 表为准。
4. **删除只作用于**索引里记录过、且位于本任务远端目录下的路径。
   源目录为空时不得执行同步删除。
5. 令牌/密码：只加密存库（`secret` 列），不进日志、不在接口里回显明文、界面只显示末 4 位。

## 目录与分层

```text
src/main.rs            入口：配置、日志、存储、调度器、HTTP + 运维子命令 --cd2-check
src/config.rs          QM_* 环境变量（集中读取，别处不要直接读 env）
src/model.rs           任务/目的地/运行记录/限速策略的数据结构
src/secrets.rs         ChaCha20-Poly1305 加解密（密钥 /data/secret.key）
src/store.rs           SQLite：迁移、CRUD、文件索引、待清理临时文件、归档
src/destination.rs     Destination trait、DestError、路径工具
src/cd2/client.rs      CD2 gRPC 客户端：鉴权、请求闸门、错误分类、流式写入
src/cd2/destination.rs CD2 目的地实现：上传（临时名→校验→改名）、等待完成、读回
src/jobs/mod.rs        运行上下文（日志/统计/取消）、本地扫描、远端目录缓存
src/jobs/mirror.rs     目录镜像
src/jobs/archive.rs    目录归档
src/jobs/mysql.rs      MySQL 转储
src/jobs/pipeline.rs   子进程流水线（tar/mysqldump → zstd → age）
src/jobs/fake.rs       测试用内存目的地（仅 cfg(test)）
src/runner.rs          全局串行的执行器：运行记录、冷却、取消
src/scheduler.rs       cron 调度、错过补跑
src/web/               axum 路由、会话鉴权、JSON 接口、内嵌界面（ui/）
proto/clouddrive.proto CD2 官方 proto（与 Full-Stack-Media-Library 里的逐字节相同）
scripts/client-push/   客户端推送脚本（Linux/macOS 的 Python、Windows 的 PowerShell，不进镜像）
```

界面是 `src/web/mod.rs` 用 `include_str!` 内嵌的纯静态文件（`src/web/ui/index.html`、
`app.js`、`style.css`）：**没有前端构建步骤**，但改完必须重新 `cargo build` 才会生效。
`ui/` 下的 `*.bak-*` 是编辑器备份，不是源码，已被 `.gitignore` 忽略，不要改。

## 常用命令

本机 `cargo` 不在 PATH，用 `$HOME/.cargo/bin/cargo`（或先 `export PATH="$HOME/.cargo/bin:$PATH"`）。

```bash
cargo build --release --locked
cargo test --locked                 # 单元测试，不联网（52 个）
cargo test --locked mirror          # 只跑镜像相关
rustfmt --edition 2024 --check src/main.rs src/*.rs src/**/*.rs

# 本机直接跑（不要把 QM_DATA_DIR 留在默认的 /data）：
QM_DATA_DIR=./data QM_BIND=127.0.0.1:8790 cargo run

docker compose build && docker compose up -d
docker compose logs -f
# 只读探测 CD2 是否可达、协议是否对得上（不需要令牌）
docker compose exec quantumark /usr/local/bin/quantumark --cd2-check http://192.168.8.114:19798
# 忘记管理员密码时重置（容器内执行）
docker compose exec quantumark /usr/local/bin/quantumark --set-password 新密码
```

镜像按 `BUILDPLATFORM` 编译、按 `TARGETARCH` 交叉编译，所以交叉构建不需要 QEMU 模拟编译：

```bash
docker buildx build --platform linux/arm64 --output ./out --target binary .   # 只导出二进制
docker buildx build --platform linux/arm64 -t quantumark:arm64 .              # 完整镜像；运行镜像里的 apt 仍需 binfmt
```

## 代码约定

- 注释、错误信息、界面文案统一中文；错误用 `Result<T, String>`，面向用户可读。
- 新增配置项：`config.rs` 读取 → `.env.example` → README 环境变量表。
  环境变量只在 `config.rs` 里读（`QM_DATA_DIR` / `QM_BIND` / `QM_ADMIN_PASSWORD` / `QM_LOG`）。
- 子进程工具（`mariadb-dump`/`mariadb`、`zstd`、`age`）由镜像的 runtime 阶段安装
  （见 `Dockerfile`）；新增外部命令要同步装进镜像。
- 容器路径约定：宿主机根只读挂在 `/host`（任务路径从这里选），可写的"收件箱"在
  `/upload`；状态在 `/data`（SQLite + `secret.key` + 日志）。
- 新的目的地类型：实现 `Destination` trait，在 `runner.rs` 里按 `kind` 构造；
  不要把 CD2 专有的逻辑塞进任务运行器。
- 上传流程不要改成"直接写正式名"：临时名 + 校验 + 改名保证了旧版本在
  新版本校验通过前一直存在，也绕开了 CD2 对同名文件的处理策略。
- 日志里不要出现令牌或密码；任务的凭据只存在 `destinations.secret`（加密）。
- 时间统一用 `chrono::Local`（容器里跟随 `TZ`），数据库里存 unix 秒。

## 测试与验证

- MySQL 有两条路：`dump`（连库导出，默认）和文件级（archive/mirror）。改任一分支时保持另一条可用。
- 单元测试用 `src/jobs/fake.rs` 的内存目的地覆盖任务逻辑：
  首次全传、二次不重传且不读远端、首次认领、改动重传、同步删除、空源目录保护、
  连续失败中止、风控中止、临时文件清理。
- gRPC 客户端细节（流式写入、等待 CD2 传到 115、大小校验、读回）只能在实机验证：
  网页「目的地 → 测试连接 / 自检」。
- 手动验证远程机器上的服务时，用**该机器自己的地址**（如 `http://192.168.8.114:8790`），
  不要用 `127.0.0.1`。
- 改限速/风控/配额相关逻辑前，先读 README 的「风控与限速」和「已知限制」；
  改恢复相关行为前读「恢复（手动）」。README 是面向用户的契约，行为和它不一致时要同步改。

## 修改边界

- `proto/clouddrive.proto` 来自 CD2 官方；升级时整份替换，并用 `--cd2-check` 和自检确认兼容。
- `secrets.rs` 的密文格式（`v1:<nonce>:<ciphertext>`）和 `/data/secret.key` 一旦变更，
  已保存的凭据会解不开；要改就做版本兼容，不要静默换格式。
- 索引表 `files` 的语义是"云端有什么"的唯一事实来源，改结构时要考虑重置后的重新认领成本。
