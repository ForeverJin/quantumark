@echo off
rem quantumark 客户端推送：双击运行（Windows）。
rem
rem 用法：把本文件和 qm_push.ps1 一起复制到要备份的目录，双击本文件即可。
rem 共享地址和客户端名可以改下面的默认值，也可以先设置环境变量 QM_DEST / QM_CLIENT；
rem QM_FULL=1 强制全量重推，QM_ADOPT=1 认领目标里已有的文件。
rem 用 UNC 路径前，先让当前账户能访问共享（例如先 net use 或用凭据管理器）。

setlocal
cd /d "%~dp0"

if not defined QM_DEST set "QM_DEST=\\192.168.8.139\BackupHub"
if not defined QM_CLIENT set "QM_CLIENT=PC"
if not defined QM_FULL set "QM_FULL=0"
if not defined QM_ADOPT set "QM_ADOPT=0"
set "FULL_ARG="
if "%QM_FULL%"=="1" set "FULL_ARG=-Full"
set "ADOPT_ARG="
if "%QM_ADOPT%"=="1" set "ADOPT_ARG=-Adopt"

if not exist "%~dp0qm_push.ps1" (
    echo 找不到 qm_push.ps1：请把它和本文件放在同一目录。
    pause
    exit /b 2
)

if not exist "%QM_DEST%\" (
    echo 投递目录不存在：%QM_DEST%
    echo 请先 net use 或映射网络驱动器，或者修改本文件里的 QM_DEST。
    pause
    exit /b 2
)

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0qm_push.ps1" -Source "%~dp0." -Dest "%QM_DEST%" -Client "%QM_CLIENT%" %FULL_ARG% %ADOPT_ARG%
set "RC=%ERRORLEVEL%"
echo.
pause
exit /b %RC%
