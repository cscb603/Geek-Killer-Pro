param (
    [string]$Action = "verify", # verify, build, release
    [string]$Version = "1.0.0"
)

$ErrorActionPreference = "Stop"
$ProjectRoot = Get-Location

Write-Host "===============================================" -ForegroundColor Cyan
Write-Host "   星TAP实验室 Rust 大师级 SOP (Windows)   " -ForegroundColor Cyan
Write-Host "===============================================" -ForegroundColor Cyan

function Run-Step([string]$Name, [scriptblock]$Command) {
    Write-Host "`n>>> [STEP] $Name" -ForegroundColor Magenta
    & $Command
    if ($LASTEXITCODE -ne 0) {
        Write-Host "❌ $Name 失败！" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# 1. 基础验证门禁
Run-Step "代码格式化 (fmt)" { cargo fmt --all --check }
Run-Step "静态检查 (clippy)" { cargo clippy --workspace -- -D warnings }
Run-Step "自动化测试 (test)" { cargo test --workspace }

if ($Action -eq "verify") {
    Write-Host "`n✨ 验证通过！代码质量符合大师级标准。" -ForegroundColor Green
    exit 0
}

# 2. 发布版编译
Run-Step "发布版构建 (release)" { cargo build --release --workspace }

if ($Action -eq "build") {
    Write-Host "`n✨ 构建成功！EXE 位于 target\release\" -ForegroundColor Green
    exit 0
}

# 3. 自动打包发布 (需安装 gh CLI)
if ($Action -eq "release") {
    Run-Step "自动打包与 GitHub 发布" {
        $BinaryName = (Get-Item "Cargo.toml" | Select-String "name = `"(.*)`"").Matches.Groups[1].Value
        $ZipName = "${BinaryName}_v${Version}_Win_Portable.zip"
        $DistDir = "dist"
        
        if (Test-Path $DistDir) { Remove-Item $DistDir -Recurse -Force }
        New-Item -ItemType Directory -Path $DistDir | Out-Null
        
        Copy-Item "target/release/${BinaryName}.exe" -Destination "$DistDir/${BinaryName}.exe"
        Copy-Item "README.md" -Destination $DistDir
        
        Compress-Archive -Path "$DistDir/*" -DestinationPath "$ZipName" -Force
        
        Write-Host "📦 已生成压缩包: $ZipName" -ForegroundColor Green
        
        # GitHub Release 逻辑 (可选)
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            Write-Host "🚀 正在发布到 GitHub..." -ForegroundColor Cyan
            gh release create "v$Version" $ZipName --title "v$Version Release" --notes "Released via Master SOP"
        } else {
            Write-Host "提示: 未检测到 gh CLI，请手动上传 $ZipName" -ForegroundColor Yellow
        }
    }
}
