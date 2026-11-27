<#
.SYNOPSIS
  把本机目录增量推送到 quantumark 的 SMB 投递目录（Windows）。

.DESCRIPTION
  服务端上传成功后会删掉投递目录里的文件，所以目标目录不能当作“远端状态”做
  robocopy /MIR 之类的镜像。本脚本在本机保存一份“已发送清单”（路径 → 大小 +
  mtime），每次只发送新增/变化的文件，并保证写入是原子的：
      先写 <文件名>.part -> rename 成正式名
  这样服务端任何时刻扫描到的都是完整文件；`.part` 也会被服务端的默认排除规则跳过。
  默认不逐文件强制落盘（Flush），在 SMB 上每个 flush 都是一次同步往返；需要严格
  落盘时加 -Fsync。脚本自身、.DS_Store、._*、*.tmp/*.part 等会直接跳过。

  目标目录是队列，不是存储：服务端上传成功后会删掉文件，不要手工清理。

  只按大小 + mtime 判断变化；脚本会保留源文件的 LastWriteTimeUtc。
  以 .tmp / .part 结尾的源文件会被服务端排除规则跳过，尽量别用这种名字。

  目录结构（默认保留源目录名）：
    -Source D:\photos -Client mypc        ->  <Dest>\mypc\photos\<相对路径>
    -Flat                                 ->  <Dest>\mypc\<相对路径>
    -Subdir 2026\photos                   ->  <Dest>\mypc\2026\photos\<相对路径>
  改了 -Source / -Dest / -Client / -Subdir 会被当成新目标、重新全量发送一次；
  同一客户端可以推多个目录，清单按“目标 + 布局”分别记录。

  第一次想用资源管理器粘贴省时间：先把整棵树粘到 <Dest>\<Client>\<目录名>\，再跑
  一次脚本（首次运行会自动认领，也可以显式加 -Adopt）。大小 + mtime（1 秒容差）
  一致的文件直接记入清单，不会重抄；之后正常增量。

.EXAMPLE
  .\qm_push.ps1 -Source D:\photos -Dest \\armbian\quantumark-inbox -Client mypc

.EXAMPLE
  .\qm_push.ps1 -Source D:\photos -Dest Z:\ -Client mypc -DryRun

.EXAMPLE
  .\qm_push.ps1 -Source D:\photos -Dest Z:\ -Client mypc -Subdir 2026\photos

.EXAMPLE
  .\qm_push.ps1 -Source D:\photos -Dest Z:\ -Client mypc -Adopt

.NOTES
  定时推送（任务计划程序，每天 02:10；SYSTEM 运行时用 ProgramData 存清单）：
    schtasks /Create /TN "quantumark-push" /SC DAILY /ST 02:10 /RU SYSTEM /RL HIGHEST `
      /TR "powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\tools\qm_push.ps1 -Source D:\photos -Dest \\armbian\quantumark-inbox -Client mypc -StateDir C:\ProgramData\quantumark-push"

  用 UNC 路径时，先让 SYSTEM 能访问共享（例如先 net use 或用计算机账户授权）。
  退出码：0 全部成功；1 有文件失败；2 参数/挂载等致命错误。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Dest,
    [string]$Client = $env:COMPUTERNAME,
    [string]$Subdir,
    [switch]$Flat,
    [switch]$Adopt,
    [string]$StateDir = (Join-Path $env:LOCALAPPDATA "quantumark-push"),
    [switch]$Full,
    [switch]$DryRun,
    [switch]$Fsync
)

$ErrorActionPreference = "Stop"
$ManifestVersion = 2
$ChunkSize = 4MB
$SaveEvery = 20

function Write-Log([string]$Message) {
    Write-Host ("{0} {1}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $Message)
}

function Stop-Fatal([string]$Message) {
    [Console]::Error.WriteLine(("{0} 错误: {1}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $Message))
    exit 2
}

function Get-FilesRecursive([string]$Root) {
    $stack = [System.Collections.Generic.Stack[string]]::new()
    $stack.Push($Root)
    while ($stack.Count -gt 0) {
        $dir = $stack.Pop()
        try {
            $entries = [System.IO.DirectoryInfo]::new($dir).GetFileSystemInfos()
        }
        catch {
            Write-Log ("警告: 无法读取目录 {0}: {1}" -f $dir, $_.Exception.Message)
            continue
        }
        foreach ($entry in $entries) {
            try {
                # 不跟随符号链接 / junction，避免把别的目录卷进来
                if (($entry.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                    if ($entry -is [System.IO.FileInfo]) {
                        Write-Log ("警告: 跳过重解析点文件 {0}" -f $entry.FullName)
                    }
                    continue
                }
                if ($entry -is [System.IO.DirectoryInfo]) {
                    if ($entry.Name -notin @('@eaDir', '#recycle', '.stfolder')) {
                        $stack.Push($entry.FullName)
                    }
                }
                elseif ($entry -is [System.IO.FileInfo]) {
                    $entry
                }
            }
            catch {
                Write-Log ("警告: 跳过 {0}: {1}" -f $entry.FullName, $_.Exception.Message)
            }
        }
    }
}

function Save-Manifest {
    $obj = [ordered]@{ version = $ManifestVersion; layouts = $layouts }
    $json = $obj | ConvertTo-Json -Depth 6 -Compress
    $tmp = "$ManifestPath.tmp"
    [System.IO.File]::WriteAllText($tmp, $json, [System.Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $tmp -Destination $ManifestPath -Force
}

function Normalize-Subdir([string]$Value) {
    $text = $Value.Trim().Replace('\', '/').Trim('/')
    if ([string]::IsNullOrEmpty($text)) { return "" }
    foreach ($part in $text.Split('/')) {
        if ($part -eq '' -or $part -eq '.' -or $part -eq '..') {
            Stop-Fatal "-Subdir 不合法（不能有 / 开头、空段或 ..）: $Value"
        }
        foreach ($ch in $part.ToCharArray()) {
            if ([int][char]$ch -lt 32) { Stop-Fatal "-Subdir 含有控制字符: $Value" }
        }
    }
    return $text
}

# ---------- 参数与路径校验 ----------

$Source = [System.IO.Path]::GetFullPath($Source)
$Dest = [System.IO.Path]::GetFullPath($Dest)
if ([string]::IsNullOrWhiteSpace($Client)) {
    Stop-Fatal "-Client 不能为空（默认取 %COMPUTERNAME%）"
}
$Client = $Client.Trim()

if (-not [System.IO.Directory]::Exists($Source)) {
    Stop-Fatal "源目录不存在或不是目录: $Source"
}
if (-not [System.IO.Directory]::Exists($Dest)) {
    Stop-Fatal "投递目录不存在（SMB 没挂上 / 凭据没缓存？）: $Dest"
}
if ($Client -notmatch '^[A-Za-z0-9._-]+$' -or $Client -eq '.' -or $Client -eq '..') {
    Stop-Fatal "-Client 只能包含字母、数字、点、下划线、短横线，且不能是 . 或 .."
}

$srcFull = $Source.TrimEnd('\')

if ($Flat -and -not [string]::IsNullOrEmpty($Subdir)) {
    Stop-Fatal "-Subdir 和 -Flat 不能同时使用"
}
if ($Full -and $Adopt) {
    Stop-Fatal "-Full 和 -Adopt 不能同时使用"
}
if ($Flat) {
    $Subdir = ""
}
elseif (-not [string]::IsNullOrEmpty($Subdir)) {
    $Subdir = Normalize-Subdir $Subdir
    if ([string]::IsNullOrEmpty($Subdir)) {
        Stop-Fatal "-Subdir 不能为空（想放到 <Client>/ 下就直接用 -Flat）"
    }
}
else {
    $Subdir = Normalize-Subdir ([System.IO.Path]::GetFileName($srcFull))
    if ([string]::IsNullOrEmpty($Subdir) -or $Subdir -match '^[A-Za-z]:$') {
        Stop-Fatal "源目录是根目录，请用 -Subdir 指定共享里的目录名"
    }
}

$destFull = $Dest.TrimEnd('\')
if ($destFull.Equals($srcFull, [System.StringComparison]::OrdinalIgnoreCase) -or
    $destFull.StartsWith($srcFull + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
    Stop-Fatal "投递目录不能位于源目录里面"
}

$Layout = if ($Subdir) { "{0}::{1}/{2}" -f $destFull, $Client, $Subdir } else { "{0}::{1}" -f $destFull, $Client }
$Base = Join-Path $Dest $Client
if ($Subdir) {
    $Base = Join-Path $Base ($Subdir.Replace('/', '\'))
}
try {
    [System.IO.Directory]::CreateDirectory($Base) | Out-Null
}
catch {
    Stop-Fatal "投递目录不可写（检查共享权限）: $Base : $($_.Exception.Message)"
}
Write-Log ("目标目录：{0}" -f $Base)

if (-not [System.IO.Directory]::Exists($StateDir)) {
    try { [System.IO.Directory]::CreateDirectory($StateDir) | Out-Null }
    catch { Stop-Fatal "清单目录不可写（可用 -StateDir 指定）: $StateDir : $($_.Exception.Message)" }
}
$ManifestPath = Join-Path $StateDir "$Client.json"

# ---------- 读取已发送清单 ----------
# 清单按「布局」分开存：同一客户端推多个目录、或改了 -Subdir，互不影响。

$layouts = @{}
if ([System.IO.File]::Exists($ManifestPath)) {
    try {
        $json = [System.IO.File]::ReadAllText($ManifestPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
        if ($json.version -eq $ManifestVersion -and $null -ne $json.layouts) {
            foreach ($layoutProp in $json.layouts.PSObject.Properties) {
                $entry = @{}
                foreach ($prop in $layoutProp.Value.PSObject.Properties) {
                    $entry[$prop.Name] = @{
                        size  = [int64]$prop.Value.size
                        mtime = [int64]$prop.Value.mtime
                    }
                }
                $layouts[$layoutProp.Name] = $entry
            }
        }
    }
    catch {
        Write-Log ("警告: 清单损坏，按首次运行全量处理: {0}" -f $_.Exception.Message)
        $layouts = @{}
    }
}
$FirstForLayout = -not $layouts.ContainsKey($Layout)
$AdoptMode = $Adopt -or ($FirstForLayout -and -not $Full)
if ($AdoptMode) {
    Write-Log "先检查目标里已有的文件（大小 + mtime 一致就认领，不重复复制）"
}
$files = @{}
if (-not $Full -and $layouts.ContainsKey($Layout)) {
    $files = $layouts[$Layout]
}
$layouts[$Layout] = $files

# ---------- 扫描并发送 ----------

$seen = [System.Collections.Generic.HashSet[string]]::new()
$total = 0; $sent = 0; $skipped = 0; $adopted = 0; $adoptedFixed = 0; $failed = 0
$sentBytes = [int64]0
$srcPrefix = $srcFull + '\'

foreach ($file in Get-FilesRecursive $srcFull) {
    $name = $file.Name
    if ($name -in @('qm_push.py', 'qm_push.ps1', 'qm_push.command', 'qm_push.cmd', '.DS_Store', 'Thumbs.db') -or
        $name.StartsWith('._') -or $name.StartsWith('.Trash') -or
        $name.EndsWith('.tmp') -or $name.EndsWith('.part') -or $name.EndsWith('.bmpart')) {
        continue
    }

    $total++
    $rel = $file.FullName.Substring($srcPrefix.Length).Replace('\', '/')
    $null = $seen.Add($rel)

    $size = 0
    $mtime = [int64]0
    try {
        $size = [int64]$file.Length
        $mtime = [int64]$file.LastWriteTimeUtc.Ticks
    }
    catch {
        $failed++
        Write-Log ("警告: 读取 {0} 失败（可能刚被删除）: {1}" -f $file.FullName, $_.Exception.Message)
        continue
    }

    $target = Join-Path $Base $rel.Replace('/', '\')
    if ($AdoptMode) {
        $exact = $false
        $bySize = $false
        try {
            $dst = [System.IO.FileInfo]::new($target)
            if ($dst.Exists -and $dst.Length -eq $size) {
                if ([Math]::Abs($dst.LastWriteTimeUtc.Ticks - $mtime) -le [System.TimeSpan]::TicksPerSecond) {
                    $exact = $true
                }
                else {
                    $bySize = $true
                }
            }
        }
        catch { }
        if ($exact -or $bySize) {
            if ($bySize) {
                if ($DryRun) {
                    Write-Log ("[dry-run] 认领 {0}（按大小，mtime 会校正）" -f $rel)
                }
                else {
                    try {
                        [System.IO.File]::SetLastWriteTimeUtc($target, $file.LastWriteTimeUtc)
                        $adoptedFixed++
                    }
                    catch {
                        Write-Log ("警告: {0} 认领后校正 mtime 失败: {1}" -f $rel, $_.Exception.Message)
                    }
                }
            }
            elseif ($DryRun) {
                Write-Log ("[dry-run] 认领 {0}" -f $rel)
            }
            $files[$rel] = @{ size = $size; mtime = $mtime }
            $adopted++
            continue
        }
    }
    else {
        $prev = $files[$rel]
        if (-not $Full -and $null -ne $prev -and $prev.size -eq $size -and $prev.mtime -eq $mtime) {
            $skipped++
            continue
        }
    }

    if ($DryRun) {
        Write-Log ("[dry-run] 发送 {0}（{1} 字节）" -f $rel, $size)
        $sent++
        $sentBytes += $size
        continue
    }

    $part = "$target.part"
    try {
        [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($target)) | Out-Null

        $in = [System.IO.File]::OpenRead($file.FullName)
        try {
            $out = [System.IO.File]::Open($part, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try {
                $buffer = [byte[]]::new($ChunkSize)
                while (($n = $in.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    $out.Write($buffer, 0, $n)
                }
                if ($Fsync) { $out.Flush($true) }
            }
            finally { $out.Dispose() }
        }
        finally { $in.Dispose() }

        [System.IO.File]::SetLastWriteTimeUtc($part, $file.LastWriteTimeUtc)

        # rename 前再确认源文件没在被写；否则这一份可能是半新旧混合
        $now = [System.IO.FileInfo]::new($file.FullName)
        if ($now.Length -ne $size -or $now.LastWriteTimeUtc.Ticks -ne $mtime) {
            Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
            throw "文件在复制过程中发生变化"
        }

        Move-Item -LiteralPath $part -Destination $target -Force
        $files[$rel] = @{ size = $size; mtime = $mtime }
        $sent++
        $sentBytes += $size
        Write-Log ("已发送 {0}（{1} 字节）" -f $rel, $size)
        if (($sent % $SaveEvery) -eq 0) { Save-Manifest }
    }
    catch {
        $failed++
        Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
        Write-Log ("错误: 写入 {0} 失败: {1}" -f $rel, $_.Exception.Message)
        continue
    }
}

# 只清理当前布局里源端已删除的条目；同一客户端其他目录的清单保持不动
foreach ($key in @($files.Keys)) {
    if (-not $seen.Contains($key)) {
        $files.Remove($key)
    }
}

if (-not $DryRun) {
    try { Save-Manifest }
    catch {
        $failed++
        Write-Log ("错误: 保存清单失败（{0}）: {1}" -f $ManifestPath, $_.Exception.Message)
    }
}

Write-Log ("完成：扫描 {0}，发送 {1}（{2} 字节），认领 {3}（校正 mtime {4}），未变化 {5}，失败 {6}" -f $total, $sent, $sentBytes, $adopted, $adoptedFixed, $skipped, $failed)
if ($failed -gt 0) { exit 1 }
exit 0
