#!/usr/bin/env python3
"""把本机目录增量推送到 quantumark 的 SMB 投递目录（Linux / macOS）。

背景：
  服务端会把投递目录里上传成功的文件删掉，所以目标目录不能当作“远端状态”来
  做 rsync 增量。这个脚本在本机保存一份“已发送清单”（路径 → 大小 + mtime），
  每次只发送新增/变化的文件，并保证写入是原子的：
      先写 <文件名>.part -> fsync -> rename 成正式名
  这样服务端任何时刻扫描到的都是完整文件；`.part` 也被服务端的默认排除规则跳过。

目录结构（默认保留源目录名）：
  --src /data/photos --client laptop  ->  <dest>/laptop/photos/<相对路径>
  --flat                              ->  <dest>/laptop/<相对路径>
  --subdir 2026/photos                ->  <dest>/laptop/2026/photos/<相对路径>

用法：
  python3 qm_push.py --src /data/photos --dest /mnt/quantumark-inbox --client laptop-jin
  python3 qm_push.py --src ~/Documents --dest /Volumes/inbox --client macbook --dry-run
  python3 qm_push.py --src /data/photos --dest /mnt/inbox --client nas --full
  python3 qm_push.py --src /data/photos --dest /mnt/inbox --client nas --flat

注意：
  - 脚本自己、日志、SMB 凭据都不要放在 --src 里，否则会被一起备份到 115。
  - 目标目录是队列，不是存储：服务端上传成功后会删掉文件，不要手工清理/搬动。
  - 只按大小 + mtime 判断变化，必须保留源文件的 mtime（本脚本会保留）。
  - 以 `.tmp` / `.part` 结尾的源文件会被服务端排除规则跳过，尽量别用这种名字。
  - 改了 --src / --client / --subdir（即共享里的目标子目录）会被当成新的布局，
    重新全量发送一次；同一客户端可以推多个目录，清单按布局分别记录。

退出码：0 全部成功；1 有文件失败；2 参数/挂载等致命错误。
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import re
import sys
import time
from typing import Iterator, NoReturn

MANIFEST_VERSION = 2
CHUNK = 1024 * 1024
SAVE_EVERY = 20
CLIENT_RE = re.compile(r"^[A-Za-z0-9._-]+$")


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
                        stack.append(entry.path)
                    elif entry.is_file(follow_symlinks=False):
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


def copy_atomic(src: str, dst: str, st: os.stat_result) -> None:
    """写入 dst.part，设置 mtime，再 rename 成 dst。"""
    part = f"{dst}.part"
    mkdirs(os.path.dirname(dst) or ".")
    try:
        with open(src, "rb") as fin, open(part, "wb") as fout:
            while True:
                chunk = fin.read(CHUNK)
                if not chunk:
                    break
                fout.write(chunk)
            fout.flush()
            try:
                os.fsync(fout.fileno())
            except OSError as exc:
                # 少数网络盘不支持 fsync，rename 前数据通常也已提交，继续即可。
                log(f"  提示: fsync 失败，继续（{exc}）")
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
    ap.add_argument("--dry-run", action="store_true", help="只列出要发送的文件，不写入")
    args = ap.parse_args()

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

    # 清单按「布局」分开存：同一客户端推多个目录、或改了 subdir，互不影响
    layout = f"{client}/{subdir}" if subdir else client
    log(f"目标布局：<dest>/{layout}/（相对路径）")

    manifest_path = os.path.join(state_abs, f"{client}.json")
    layouts = load_manifest(manifest_path)
    old = {} if args.full else layouts.get(layout)
    if not isinstance(old, dict):
        old = {}
    files = dict(old)
    layouts[layout] = files
    seen = set()

    total = sent = skipped = failed = 0
    sent_bytes = 0

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

        target = os.path.join(base, *rel.split("/"))
        if args.dry_run:
            log(f"[dry-run] 发送 {rel}（{st.st_size} 字节）")
            sent += 1
            sent_bytes += st.st_size
            continue

        try:
            copy_atomic(abs_path, target, st)
        except ChangedDuringCopy:
            failed += 1
            log(f"跳过 {rel}: 复制过程中文件又变了，留到下一轮")
            continue
        except OSError as exc:
            failed += 1
            if exc.errno in (errno.EIO, errno.ENOSPC, errno.ENOTCONN):
                log(f"错误: 写入 {rel} 失败，可能 SMB 断开或磁盘满: {exc}")
            else:
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
        f"未变化 {skipped}，失败 {failed}"
    )
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("已中断", file=sys.stderr)
        raise SystemExit(130)
