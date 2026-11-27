#!/bin/bash
# quantumark 客户端推送：双击运行（macOS）。
#
# 用法：把本文件和 qm_push.py 一起复制到要备份的目录，双击本文件即可。
# 首次如果提示没有执行权限，在终端执行一次：
#   chmod +x qm_push.command
#
# 共享挂载点、客户端名和并发数可以改下面的默认值，也可以设置环境变量
# QM_DEST / QM_CLIENT / QM_JOBS；QM_FULL=1 强制全量重推，QM_ADOPT=1 认领目标里已有的文件。
# 默认挂载点不存在时，会先在已挂载的 SMB 卷里找这个共享，找不到就自动发起连接。

cd "$(dirname "$0")" || exit 1

SHARE_URL="${QM_SHARE_URL:-smb://192.168.8.139/BackupHub}"
SHARE_NAME="${QM_SHARE_NAME:-BackupHub}"
DEST="${QM_DEST:-/Volumes/BackupHub}"
CLIENT="${QM_CLIENT:-Mac}"
JOBS="${QM_JOBS:-8}"

pause_exit() {
    echo
    read -n 1 -s -r -p "按任意键关闭窗口..."
    echo
}

# 在 mount 输出里找这个共享的挂载点（兼容挂成 /Volumes/BackupHub-1 之类）
find_share_mount() {
    mount | while IFS= read -r line; do
        case "$line" in
            *"(smbfs"*)
                src=${line%% on *}
                case "$src" in
                    *"/${SHARE_NAME}") ;;
                    *) continue ;;
                esac
                mp=${line#* on }
                mp=${mp%% (smbfs*}
                printf '%s\n' "$mp"
                ;;
        esac
    done | head -n 1
}

if [ ! -f "qm_push.py" ]; then
    echo "找不到 qm_push.py：请把它和本文件放在同一目录。"
    pause_exit
    exit 2
fi

# 默认挂载点不在，先找已经挂载的共享
if [ ! -d "$DEST" ]; then
    detected=$(find_share_mount)
    if [ -n "$detected" ]; then
        DEST="$detected"
        echo "使用已挂载的共享：$DEST"
    fi
fi

# 还没挂载：打开 Finder 连接，最多等 60 秒
if [ ! -d "$DEST" ]; then
    echo "共享没有挂载，正在连接 $SHARE_URL ..."
    open "$SHARE_URL" >/dev/null 2>&1
    for _ in $(seq 1 60); do
        sleep 1
        detected=$(find_share_mount)
        if [ -n "$detected" ]; then
            DEST="$detected"
            echo "已挂载：$DEST"
            break
        fi
        [ -d "$DEST" ] && break
    done
fi

if [ ! -d "$DEST" ]; then
    echo "投递目录不存在：$DEST"
    echo "当前已挂载的卷："
    ls -1 /Volumes 2>/dev/null | sed 's/^/  /'
    echo "请先在 Finder 里连接 $SHARE_URL，或用 QM_DEST 指定实际挂载点。"
    pause_exit
    exit 2
fi

args=(--src . --dest "$DEST" --client "$CLIENT" --jobs "$JOBS")
if [ "${QM_FULL:-0}" = "1" ]; then
    args+=(--full)
elif [ "${QM_ADOPT:-0}" = "1" ]; then
    args+=(--adopt)
fi
python3 qm_push.py "${args[@]}"
status=$?
pause_exit
exit $status
