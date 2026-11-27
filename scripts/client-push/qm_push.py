#!/usr/bin/env python3
"""把本机目录增量推送到 quantumark 的 SMB 投递目录（Linux / macOS）。

背景：
  服务端会把投递目录里上传成功的文件删掉，所以目标目录不能当作“远端状态”来
  做 rsync 增量。这个脚本在本机保存一份“已发送清单”（路径 → 大小 + mtime），
  每次只发送新增/变化的文件，并保证写入是原子的：
      先写 <文件名>.part -> rename 成正式名
  `.part` 会被服务端的默认排除规则跳过。

目录结构（默认保留源目录名）：
  --src /data/photos --client laptop  ->  <dest>/laptop/photos/<相对路径>
  --flat                              ->  <dest>/laptop/<相对路径>
  --subdir 2026/photos                ->  <dest>/laptop/2026/photos/<相对路径>

用法：
  python3 qm_push.py --src /data/photos --dest /mnt/quantumark-inbox --client laptop-jin
  python3 qm_push.py --src ~/Documents --dest /Volumes/inbox --client macbook --dry-run
  python3 qm_push.py --src /data/photos --dest /mnt/inbox --client nas --full
  python3 qm_push.py --src /data/photos --dest /mnt/inbox --client nas --flat
  python3 qm_push.py --src /data/photos --dest /mnt/inbox --client nas --adopt

第一次想用 Finder 粘贴省时间：先把整棵树粘到 <dest>/<client>/<目录名>/，再跑
一次脚本（首次运行会自动认领，也可以显式加 --adopt）。大小 + mtime（1 秒容差）
一致的文件直接记入清单，不会重抄；之后正常增量。

注意：
  - 默认 8 个并发复制文件（`--jobs` 可调，上限 64），且不逐文件 fsync；SMB 上大量
    小文件时串行 + fsync 会非常慢，确实需要严格落盘时再加 `--fsync`。
  - 脚本自身、`.DS_Store`、`._*`、`*.tmp/*.part` 等默认直接跳过，不占 SMB 往返；
    服务端本来也会排除这些。
  - 目标目录是队列，不是存储：服务端上传成功后会删掉文件，不要手工清理/搬动。
  - 只按大小 + mtime 判断变化，必须保留源文件的 mtime（本脚本会保留）。
  - 改了 --dest / --src / --client / --subdir 都会被当成新目标，重新全量发送一次
    （服务端会按大小+mtime 去重，不会重复上传）；同一客户端可以推多个目录，
    清单按“目标共享 + 布局”分别记录。共享里的文件被手工删掉时，用 --full 重推。

退出码：0 全部成功；1 有文件失败；2 参数/挂载等致命错误。
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import re
import stat
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Iterator, NoReturn

MANIFEST_VERSION = 2
DEFAULT_JOBS = 8
COPY_BUFSIZE = 4 * 1024 * 1024
SAVE_EVERY = 20
# 认领粘贴过来的文件时，允许 mtime 有 1 秒误差（SMB/文件系统时间精度）
ADOPT_MTIME_TOLERANCE_NS = 1_000_000_000
CLIENT_RE = re.compile(r"^[A-Za-z0-9._-]+$")

# 这些文件不用推：服务端本来也会排除，客户端直接跳过可以省掉 SMB 往返
TOOL_FILES = {"qm_push.py", "qm_push.ps1", "qm_push.command", "qm_push.cmd"}
SKIP_FILES = {".DS_Store", "Thumbs.db"}
SKIP_DIRS = {"@eaDir", "#recycle", ".stfolder"}


def is_ignored(name: str) -> bool:
    if name in TOOL_FILES or name in SKIP_FILES:
        return True
    if name.startswith("._") or name.startswith(".Trash"):
        return True
    return name.endswith((".tmp", ".part", ".bmpart"))


def log(msg: str) -> None:
    print(f"{time.strftime('%Y-%m-%d %H:%M:%S')} {msg}", flush=True)


def fail(msg: str, code: int = 2) -> NoReturn:
    print(f"{time.strftime('%Y-%m-%d %H:%M:%S')} 错误: {msg}", file=sys.stderr, flush=True)
    raise SystemExit(code)


def default_state_dir() -> str:
    base = os.environ.get("XDG_STATE_HOME") or os.path.join(
        os.path.expanduser("~"), ".local", "state"
    )
    return os.path.join(base, "quantumark-push")


def normalize_subdir(value: str) -> str:
    """校验并规范化共享里的目标子目录（相对 <dest>/<client>）。"""
    text = value.strip().replace("\\", "/").strip("/")
    if not text:
        return ""
    parts = text.split("/")
    for part in parts:
        if part in ("", ".", ".."):
            fail(f"--subdir 不合法（不能有 / 开头、空段或 ..）: {value}")
        if any(ord(ch) < 0x20 for ch in part):
            fail(f"--subdir 含有控制字符: {value}")
    return "/".join(parts)


def load_manifest(path: str) -> dict:
    """返回 {"<client>/<subdir>": {rel: {size, mtime_ns}}}；坏文件按首次运行处理。"""
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        if not isinstance(data, dict) or data.get("version") != MANIFEST_VERSION:
            return {}
        layouts = data.get("layouts")
        return layouts if isinstance(layouts, dict) else {}
    except FileNotFoundError:
        return {}
    except (OSError, ValueError) as exc:
        log(f"警告: 读取清单失败，按首次运行全量处理（{exc}）")
        return {}


def save_manifest(path: str, layouts: dict) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = f"{path}.tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump({"version": MANIFEST_VERSION, "layouts": layouts}, f, ensure_ascii=False)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def walk_files(root: str) -> Iterator[tuple[str, str]]:
    """产出 (绝对路径, 相对路径)；不跟随符号链接，跳过目录链接。"""
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            it = os.scandir(current)
        except OSError as exc:
            log(f"警告: 无法读取目录 {current}: {exc}")
            continue
        with it:
            for entry in it:
                try:
                    if entry.is_symlink():
                        continue
                    if entry.is_dir(follow_symlinks=False):
                        if entry.name not in SKIP_DIRS:
                            stack.append(entry.path)
                    elif entry.is_file(follow_symlinks=False):
                        if is_ignored(entry.name):
                            continue
                        rel = os.path.relpath(entry.path, root).replace(os.sep, "/")
                        yield entry.path, rel
                except OSError as exc:
                    log(f"警告: 跳过 {entry.path}: {exc}")


def mkdirs(path: str) -> None:
    """服务端清空本地时会删掉空目录，所以每次写之前都重建一次。"""
    for attempt in range(2):
        try:
            os.makedirs(path, exist_ok=True)
            return
        except OSError:
            if attempt == 1:
                raise


# 同一目录里有很多文件时，只 mkdir/stat 一次，省掉每个文件的 SMB 往返
_dir_cache: set[str] = set()
_dir_lock = threading.Lock()


def ensure_dir(path: str, force: bool = False) -> None:
    if not force:
        with _dir_lock:
            if path in _dir_cache:
                return
    mkdirs(path)
    with _dir_lock:
        _dir_cache.add(path)


def try_adopt(target: str, st: os.stat_result) -> str | None:
    """目标里已有同名文件时，判断能不能直接认领。

    返回 "exact"（大小 + mtime 一致）、"size"（大小一致但 mtime 不同，通常是
    Finder 粘贴把 mtime 改成了复制时间）或 None。

    用于“第一次直接 Finder 粘贴到共享，再用脚本做增量”：不再把整棵树重抄一遍。
    "size" 认领后会把目标文件的 mtime 校正回源文件，避免服务端每次都当新文件。
    """
    try:
        dst = os.stat(target)
    except OSError:
        return None
    if not stat.S_ISREG(dst.st_mode) or dst.st_size != st.st_size:
        return None
    if abs(dst.st_mtime_ns - st.st_mtime_ns) <= ADOPT_MTIME_TOLERANCE_NS:
        return "exact"
    return "size"


def _copy_data(src: str, part: str, do_fsync: bool) -> None:
    with open(src, "rb") as fin, open(part, "wb") as fout:
        while True:
            chunk = fin.read(COPY_BUFSIZE)
            if not chunk:
                break
            fout.write(chunk)
        fout.flush()
        if do_fsync:
            os.fsync(fout.fileno())


def copy_atomic(src: str, dst: str, st: os.stat_result, do_fsync: bool) -> None:
    """写入 dst.part，设置 mtime，再 rename 成 dst。

    默认不逐文件 fsync：SMB 上每个 fsync 都是一次同步往返，文件一多会非常慢。
    4 MiB 一块，减少写入次数；并发由调用方控制。
    """
    part = f"{dst}.part"
    parent = os.path.dirname(dst) or "."
    ensure_dir(parent)
    try:
        try:
            _copy_data(src, part, do_fsync)
        except FileNotFoundError:
            # 目标目录可能刚好被服务端清掉了；源文件还在就重建一次再试
            if not os.path.exists(src):
                raise
            ensure_dir(parent, force=True)
            _copy_data(src, part, do_fsync)
        os.utime(part, ns=(st.st_atime_ns, st.st_mtime_ns))

        # rename 前再确认源文件没在被写；否则这一份可能是半新旧混合。
        now = os.stat(src)
        if now.st_size != st.st_size or now.st_mtime_ns != st.st_mtime_ns:
            try:
                os.unlink(part)
            except OSError:
                pass
            raise ChangedDuringCopy()

        os.replace(part, dst)
    except BaseException:
        try:
            if os.path.exists(part):
                os.unlink(part)
        except OSError:
            pass
        raise


class ChangedDuringCopy(Exception):
    pass


def main() -> int:
    ap = argparse.ArgumentParser(
        description="把目录增量推送到 quantumark 的 SMB 投递目录"
    )
    ap.add_argument("--src", required=True, help="要备份的本地目录")
    ap.add_argument("--dest", required=True, help="SMB 投递目录的挂载点（本机路径）")
    ap.add_argument("--client", required=True, help="客户端名，会作为投递目录下的第一层子目录")
    ap.add_argument(
        "--subdir",
        default=None,
        help="共享里的目标子目录（默认用源目录名；支持 a/b 这样的多级）",
    )
    ap.add_argument(
        "--flat",
        action="store_true",
        help="不保留源目录名，内容直接放在 <dest>/<client>/ 下",
    )
    ap.add_argument("--state-dir", default=default_state_dir(), help="已发送清单的存放目录")
    ap.add_argument("--full", action="store_true", help="忽略清单，全部重发")
    ap.add_argument(
        "--adopt",
        action="store_true",
        help="先检查目标里已有的文件（例如 Finder 粘贴的），大小和 mtime 一致就认领，不重复复制",
    )
    ap.add_argument("--dry-run", action="store_true", help="只列出要发送的文件，不写入")
    ap.add_argument(
        "--jobs",
        type=int,
        default=DEFAULT_JOBS,
        help=f"并发复制文件数（默认 {DEFAULT_JOBS}，上限 64；小文件多时调大）",
    )
    ap.add_argument(
        "--fsync",
        action="store_true",
        help="每写完一个文件强制落盘（更稳妥，但在 SMB 上会慢很多）",
    )
    args = ap.parse_args()

    if args.jobs < 1 or args.jobs > 64:
        fail("--jobs 需要在 1..64 之间")
    if args.full and args.adopt:
        fail("--full 和 --adopt 不能同时使用")

    src = os.path.abspath(os.path.expanduser(args.src))
    dest = os.path.abspath(os.path.expanduser(args.dest))
    client = args.client.strip()

    if not os.path.isdir(src):
        fail(f"源目录不存在或不是目录: {src}")
    if not os.path.isdir(dest):
        fail(f"投递目录不存在（SMB 没挂上？）: {dest}")
    if not CLIENT_RE.match(client) or client in (".", ".."):
        fail("--client 只能包含字母、数字、点、下划线、短横线，且不能是 . 或 ..")
    if args.subdir is not None and args.flat:
        fail("--subdir 和 --flat 不能同时使用")

    if args.flat:
        subdir = ""
    elif args.subdir is not None:
        subdir = normalize_subdir(args.subdir)
        if not subdir:
            fail("--subdir 不能为空（想放到 <client>/ 下就直接用 --flat）")
    else:
        subdir = normalize_subdir(os.path.basename(src))
        if not subdir:
            fail("源目录是根目录，请用 --subdir 指定共享里的目录名")

    # 防止把投递目录自己卷进源目录，越传越多
    try:
        if os.path.commonpath([src, dest]) == src:
            fail("投递目录不能位于源目录里面")
    except ValueError:
        pass  # Windows 不同盘符

    base = os.path.join(dest, client, *subdir.split("/")) if subdir else os.path.join(dest, client)
    try:
        mkdirs(base)
    except OSError as exc:
        fail(f"投递目录不可写（检查 SMB 挂载和属主）: {base}: {exc}")

    state_abs = os.path.abspath(os.path.expanduser(args.state_dir))
    if state_abs == src:
        fail("--state-dir 不能就是源目录本身")
    # 清单目录在源目录内部时，跳过它，避免把清单自己也推上去
    skip_prefix = state_abs + os.sep if (state_abs + os.sep).startswith(src + os.sep) else None

    # 清单按「目标共享 + 布局」分开存：换挂载点/换共享会被当成新目标重新全量推一次，
    # 不会出现“清单说已发送、但新的目标目录里其实没有文件”的情况。
    layout = f"{dest}::{client}/{subdir}" if subdir else f"{dest}::{client}"
    log(f"目标目录：{base}（并发 {args.jobs}）")

    manifest_path = os.path.join(state_abs, f"{client}.json")
    layouts = load_manifest(manifest_path)
    first_for_layout = layout not in layouts
    # 首次推送（清单里还没有这个目标）：先看目标里是不是已经有人粘贴好了；
    # --adopt 可以随时强制做一次对账。
    adopt_mode = args.adopt or (first_for_layout and not args.full)
    if adopt_mode:
        log("先检查目标里已有的文件（大小 + mtime 一致就认领，不重复复制）")

    old = {} if args.full else layouts.get(layout)
    if not isinstance(old, dict):
        old = {}
    files = dict(old)
    layouts[layout] = files
    seen = set()

    total = sent = skipped = adopted = adopted_fixed = failed = 0
    sent_bytes = 0
    tasks: list[tuple[str, str, os.stat_result, str]] = []

    for abs_path, rel in walk_files(src):
        if skip_prefix and abs_path.startswith(skip_prefix):
            continue
        total += 1
        seen.add(rel)
        try:
            st = os.stat(abs_path)
        except OSError as exc:
            failed += 1
            log(f"警告: 读取 {rel} 失败: {exc}")
            continue

        target = os.path.join(base, *rel.split("/"))

        adopt = try_adopt(target, st) if adopt_mode else None
        if adopt:
            if adopt == "size":
                if args.dry_run:
                    log(f"[dry-run] 认领 {rel}（按大小，mtime 会校正）")
                else:
                    try:
                        os.utime(target, ns=(st.st_atime_ns, st.st_mtime_ns))
                    except OSError as exc:
                        log(f"警告: {rel} 认领后校正 mtime 失败: {exc}")
                    else:
                        adopted_fixed += 1
            elif args.dry_run:
                log(f"[dry-run] 认领 {rel}（目标已有）")
            files[rel] = {"size": st.st_size, "mtime_ns": st.st_mtime_ns}
            adopted += 1
            continue

        if not adopt_mode:
            prev = files.get(rel)
            unchanged = (
                not args.full
                and isinstance(prev, dict)
                and prev.get("size") == st.st_size
                and prev.get("mtime_ns") == st.st_mtime_ns
            )
            if unchanged:
                skipped += 1
                continue

        if args.dry_run:
            log(f"[dry-run] 发送 {rel}（{st.st_size} 字节）")
            sent += 1
            sent_bytes += st.st_size
            continue
        tasks.append((abs_path, rel, st, target))

    # 并发复制：SMB 上大量小文件时，串行 + 每文件 fsync 会慢得离谱
    if tasks:
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = {}
            for abs_path, rel, st, target in tasks:
                fut = pool.submit(copy_atomic, abs_path, target, st, args.fsync)
                futures[fut] = (rel, st)

            for fut in as_completed(futures):
                rel, st = futures[fut]
                try:
                    fut.result()
                except ChangedDuringCopy:
                    failed += 1
                    log(f"跳过 {rel}: 复制过程中文件又变了，留到下一轮")
                    continue
                except OSError as exc:
                    failed += 1
                    if exc.errno in (errno.EIO, errno.ENOSPC, errno.ENOTCONN):
                        log(f"错误: 写入 {rel} 失败，可能 SMB 断开或磁盘满: {exc}")
                        for pending in futures:
                            pending.cancel()
                        break
                    log(f"错误: 写入 {rel} 失败: {exc}")
                    continue

                files[rel] = {"size": st.st_size, "mtime_ns": st.st_mtime_ns}
                sent += 1
                sent_bytes += st.st_size
                log(f"已发送 {rel}（{st.st_size} 字节）")
                if sent % SAVE_EVERY == 0:
                    save_manifest(manifest_path, layouts)

    # 只清理当前布局里源端已删除的条目；同一客户端其他目录的清单保持不动
    for rel in list(files):
        if rel not in seen:
            del files[rel]

    if not args.dry_run:
        try:
            save_manifest(manifest_path, layouts)
        except OSError as exc:
            failed += 1
            log(f"错误: 保存清单失败（{manifest_path}）: {exc}")

    log(
        f"完成：扫描 {total}，发送 {sent}（{sent_bytes} 字节），"
        f"认领 {adopted}（校正 mtime {adopted_fixed}），未变化 {skipped}，失败 {failed}"
    )
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("已中断", file=sys.stderr)
        raise SystemExit(130)
