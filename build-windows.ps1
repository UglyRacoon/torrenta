<#
.SYNOPSIS
    Скрипт сборки Torrenta (Rust + Tauri 2) под Windows x64.

.DESCRIPTION
    Один вход для локальной сборки:
      проверка окружения -> установка Rust/Node.js (при необходимости) ->
      npm ci -> npm run check:contrast -> npm run build -> cargo test ->
      npm run tauri build -> печать путей и размеров артефактов.

    Скрипт рассчитан на Windows PowerShell 5.1 и PowerShell 7.
    Полную справку печатает ключ -Help.

.PARAMETER SkipTests
    Не запускать cargo test.

.PARAMETER NoInstall
    Ничего не устанавливать: только проверить наличие Rust, Node.js и
    node_modules. Если чего-то не хватает, скрипт остановится с подсказкой.

.PARAMETER Dev
    Режим разработки: вместо релизной сборки запустить npm run tauri dev.

.PARAMETER Clean
    Очистить артефакты перед сборкой: cargo clean, удалить node_modules и dist.

.PARAMETER BundleOnly
    Собрать только установщик NSIS: npm run tauri build -- --bundles nsis.

.PARAMETER Help
    Показать справку и выйти, ничего не делая.

.EXAMPLE
    .\build-windows.ps1

.EXAMPLE
    .\build-windows.ps1 -NoInstall -SkipTests

.EXAMPLE
    .\build-windows.ps1 -Dev
#>
[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$NoInstall,
    [switch]$Dev,
    [switch]$Clean,
    [switch]$BundleOnly,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Кириллица в консоли. На работу самого скрипта не влияет, но без этого
# вывод инструментов (cargo, npm) может превратиться в кракозябры.
try {
    $utf8 = New-Object -TypeName System.Text.UTF8Encoding -ArgumentList $false
    [Console]::OutputEncoding = $utf8
    $OutputEncoding = $utf8
} catch {
    Write-Verbose 'Не удалось переключить кодировку консоли на UTF-8.'
}

$script:RepoRoot = $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($script:RepoRoot)) {
    $script:RepoRoot = (Get-Location).Path
}
$script:RepoRoot = [System.IO.Path]::GetFullPath($script:RepoRoot)
$script:NpmPath = $null
$script:RustToolchain = 'stable-x86_64-pc-windows-msvc'
$script:MinRustVersion = [version]'1.77.0'
$script:MinNodeMajor = 20

# ---------------------------------------------------------------------------
# Вывод в консоль
# ---------------------------------------------------------------------------

function Write-Step {
    param([string]$Text)
    Write-Host ''
    Write-Host ('==> ' + $Text) -ForegroundColor White
}

function Write-Info {
    param([string]$Text)
    Write-Host '  [i] ' -NoNewline -ForegroundColor Cyan
    Write-Host $Text
}

function Write-Ok {
    param([string]$Text)
    Write-Host '  [ok] ' -NoNewline -ForegroundColor Green
    Write-Host $Text
}

function Write-Warn {
    param([string]$Text)
    Write-Host '  [!] ' -NoNewline -ForegroundColor Yellow
    Write-Host $Text
}

function Write-Fail {
    param([string]$Text)
    Write-Host '  [x] ' -NoNewline -ForegroundColor Red
    Write-Host $Text
}

function Write-Cmd {
    param([string]$Text)
    Write-Host ('      > ' + $Text) -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------
# Вспомогательные функции
# ---------------------------------------------------------------------------

function Get-RepoPath {
    param([string]$Relative)
    return (Join-Path $script:RepoRoot $Relative)
}

function Test-Command {
    param([string]$Name)
    $cmd = Get-Command $Name -ErrorAction SilentlyContinue
    return ($null -ne $cmd)
}

function Format-Size {
    param([long]$Bytes)
    if ($Bytes -ge 1MB) {
        return ('{0:N1} МБ' -f ($Bytes / 1MB))
    }
    return ('{0:N0} КБ' -f ($Bytes / 1KB))
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [string]$Title
    )

    if ($Title) { Write-Step $Title }
    Write-Cmd ($FilePath + ' ' + ($Arguments -join ' '))
    & $FilePath @Arguments
    $code = $LASTEXITCODE
    if ($null -eq $code) {
        throw ('Не удалось выполнить команду: ' + $FilePath)
    }
    if ($code -ne 0) {
        throw ('Команда завершилась с кодом ' + $code + ': ' + (Split-Path -Leaf $FilePath) + ' ' + ($Arguments -join ' '))
    }
}

function Update-SessionPath {
    # winget/rustup обновляют переменные окружения в реестре, но текущий
    # процесс о них не знает. Пересобираем PATH и добавляем типовые каталоги.
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $joined = @($machinePath, $userPath) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    if ($joined.Count -gt 0) {
        $env:Path = ($joined -join ';')
    }

    $fallbacks = @(
        (Join-Path $env:USERPROFILE '.cargo\bin'),
        (Join-Path $env:ProgramFiles 'nodejs'),
        (Join-Path $env:APPDATA 'npm')
    )
    foreach ($dir in $fallbacks) {
        if ([string]::IsNullOrWhiteSpace($dir)) { continue }
        if (-not (Test-Path -LiteralPath $dir)) { continue }
        if ($env:Path -notlike ('*' + $dir + '*')) {
            $env:Path = $dir + ';' + $env:Path
        }
    }
}

function Get-NpmPath {
    if ($script:NpmPath) { return $script:NpmPath }

    $node = Get-Command 'node.exe' -ErrorAction SilentlyContinue
    if ($node) {
        $candidate = Join-Path (Split-Path -Parent $node.Source) 'npm.cmd'
        if (Test-Path -LiteralPath $candidate) {
            $script:NpmPath = $candidate
            return $candidate
        }
    }

    $npm = Get-Command 'npm.cmd' -ErrorAction SilentlyContinue
    if (-not $npm) { $npm = Get-Command 'npm' -ErrorAction SilentlyContinue }
    if ($npm) {
        $script:NpmPath = $npm.Source
        return $npm.Source
    }
    return $null
}

function Get-CargoPath {
    $cargo = Get-Command 'cargo.exe' -ErrorAction SilentlyContinue
    if (-not $cargo) { $cargo = Get-Command 'cargo' -ErrorAction SilentlyContinue }
    if ($cargo) { return $cargo.Source }
    return $null
}

function Get-NodeVersion {
    $node = Get-Command 'node.exe' -ErrorAction SilentlyContinue
    if (-not $node) { return $null }
    try {
        $raw = & $node.Source '--version'
    } catch {
        return $null
    }
    if (-not $raw) { return $null }
    return ([string]$raw).Trim().TrimStart('v')
}

function Get-RustcVersion {
    $rustc = Get-Command 'rustc.exe' -ErrorAction SilentlyContinue
    if (-not $rustc) { return $null }
    try {
        $raw = [string](& $rustc.Source '--version')
    } catch {
        return $null
    }
    $match = [regex]::Match($raw, '(\d+)\.(\d+)\.(\d+)')
    if (-not $match.Success) { return $null }
    return ([version]($match.Groups[1].Value + '.' + $match.Groups[2].Value + '.' + $match.Groups[3].Value))
}

# ---------------------------------------------------------------------------
# Проверки окружения
# ---------------------------------------------------------------------------

function Assert-RepoLayout {
    $required = @(
        'Cargo.toml',
        'package.json',
        'index.html',
        'src-tauri\tauri.conf.json',
        'src-tauri\Cargo.toml',
        'src-tauri\icons\32x32.png',
        'src-tauri\icons\icon.ico'
    )
    $missing = @()
    foreach ($relative in $required) {
        if (-not (Test-Path -LiteralPath (Get-RepoPath $relative))) { $missing += $relative }
    }
    if ($missing.Count -gt 0) {
        throw ('Каталог ' + $script:RepoRoot + ' не похож на корень репозитория Torrenta. Нет файлов: ' + ($missing -join ', ') + '.')
    }
    Write-Ok 'Структура репозитория в порядке.'
}

function Assert-WindowsX64 {
    if ($env:OS -ne 'Windows_NT') {
        throw 'Скрипт поддерживает только Windows.'
    }
    $arch = $env:PROCESSOR_ARCHITECTURE
    if (($arch -ne 'AMD64') -and ($env:PROCESSOR_ARCHITEW6432 -ne 'AMD64')) {
        throw ('Нужна 64-битная Windows (x64), обнаружена архитектура: ' + $arch + '.')
    }
    Write-Ok ('Windows x64: ' + [Environment]::OSVersion.Version.ToString())
    if ($script:RepoRoot.Length -gt 120) {
        Write-Warn 'Путь проекта длиннее 120 символов: сборка может упереться в ограничение MAX_PATH (260). Обходной путь описан в docs\BUILD.md.'
    }
}

function Get-MsvcInstallPath {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere) {
        try {
            $lines = & $vswhere '-products' '*' '-requires' 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64' '-property' 'installationPath'
        } catch {
            $lines = @()
        }
        foreach ($line in @($lines)) {
            if (-not [string]::IsNullOrWhiteSpace($line)) {
                return ([string]$line).Trim()
            }
        }
    }
    if (Test-Command 'cl.exe') { return 'cl.exe (PATH)' }
    return $null
}

function Assert-Msvc {
    $path = Get-MsvcInstallPath
    if ($path) {
        Write-Ok ('MSVC: ' + $path)
        return
    }
    $hint = @(
        'Не найдены средства сборки MSVC (Visual Studio Build Tools с C++).'
        'Установите их в терминале администратора:'
        '  winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"'
        'Если Visual Studio уже стоит, проверьте компонент "MSVC v143 - C++ Build Tools" и "Windows 10/11 SDK".'
        'Подробности: docs\BUILD.md'
    ) -join [Environment]::NewLine
    throw $hint
}

function Get-WebView2Version {
    $keys = @(
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
        'HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
        'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    )
    foreach ($key in $keys) {
        if (-not (Test-Path -LiteralPath $key)) { continue }
        $pv = $null
        try {
            $pv = (Get-ItemProperty -LiteralPath $key -Name 'pv' -ErrorAction Stop).pv
        } catch {
            $pv = $null
        }
        if ($pv -and ($pv -ne '0.0.0.0')) { return $pv }
    }
    return $null
}

function Assert-WebView2 {
    $pv = Get-WebView2Version
    if ($pv) {
        Write-Ok ('WebView2 Runtime: ' + $pv)
        return
    }
    Write-Warn 'WebView2 Runtime не найден. Установщик Tauri скачает его сам (embedBootstrapper), но само приложение без рантайма не запустится.'
    Write-Warn 'Скачать: https://developer.microsoft.com/microsoft-edge/webview2/ (Evergreen Bootstrapper).'
}

# ---------------------------------------------------------------------------
# Установка Rust и Node.js
# ---------------------------------------------------------------------------

function Install-Rust {
    if (Test-Command 'winget') {
        Write-Info 'Пробую установить через winget: Rustlang.Rustup'
        & winget 'install' '--id' 'Rustlang.Rustup' '-e' '--source' 'winget' '--accept-source-agreements' '--accept-package-agreements' '--silent'
        $code = $LASTEXITCODE
        # 0 - успех, -1978335189 (0x8A15002B) - пакет уже установлен
        if (($code -eq 0) -or ($code -eq -1978335189)) {
            Write-Ok 'winget: Rust установлен.'
            return
        }
        Write-Warn ('winget завершился с кодом ' + $code + ', перехожу к rustup-init.exe.')
    }

    $url = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
    $target = Join-Path $env:TEMP 'rustup-init.exe'
    Write-Info ('Скачиваю ' + $url)
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    } catch {
        Write-Verbose 'Не удалось явно включить TLS 1.2.'
    }
    Invoke-WebRequest -Uri $url -OutFile $target -UseBasicParsing
    Write-Info 'Запускаю rustup-init (toolchain по умолчанию: ' + $script:RustToolchain + ').'
    & $target '-y' '--default-toolchain' $script:RustToolchain '--profile' 'minimal'
    if ($LASTEXITCODE -ne 0) {
        throw ('rustup-init завершился с кодом ' + $LASTEXITCODE + '.')
    }
    Write-Ok 'Rust установлен через rustup-init.'
}

function Ensure-Rust {
    param([switch]$NoInstall)

    $rustup = Get-Command 'rustup.exe' -ErrorAction SilentlyContinue
    if (-not $rustup) {
        $hint = @(
            'Rust не найден: в PATH нет rustup.exe.'
            'Варианты установки:'
            '  winget install --id Rustlang.Rustup -e'
            '  либо скачать https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe и запустить с флагом -y'
            'После установки откройте новый терминал или запустите скрипт ещё раз.'
        ) -join [Environment]::NewLine
        if ($NoInstall) { throw $hint }
        Write-Warn 'Rust не найден, устанавливаю.'
        Install-Rust
        Update-SessionPath
        $rustup = Get-Command 'rustup.exe' -ErrorAction SilentlyContinue
        if (-not $rustup) {
            throw ('Rust установлен, но rustup.exe не виден в PATH.' + [Environment]::NewLine + 'Перезапустите терминал и повторите запуск.')
        }
    }

    $list = @()
    try { $list = @(& $rustup.Source 'toolchain' 'list') } catch { $list = @() }
    $hasToolchain = @($list | Where-Object { ([string]$_).Trim().StartsWith($script:RustToolchain) }).Count -gt 0
    if (-not $hasToolchain) {
        if ($NoInstall) {
            throw ('Rust-тулчейн ' + $script:RustToolchain + ' не установлен. Выполните: rustup toolchain install ' + $script:RustToolchain + ' --profile minimal')
        }
        Write-Info ('Устанавливаю Rust-тулчейн ' + $script:RustToolchain + ' (профиль minimal).')
        & $rustup.Source 'toolchain' 'install' $script:RustToolchain '--profile' 'minimal'
        if ($LASTEXITCODE -ne 0) {
            throw ('Не удалось установить тулчейн ' + $script:RustToolchain + ' (код ' + $LASTEXITCODE + ').')
        }
    }

    $active = ''
    try { $active = [string](& $rustup.Source 'show' 'active-toolchain') } catch { $active = '' }
    if ($active -notlike '*x86_64-pc-windows-msvc*') {
        if ($NoInstall) {
            Write-Warn ('Активный Rust-тулчейн не MSVC (' + $active.Trim() + '). Переключите вручную: rustup default ' + $script:RustToolchain)
        } else {
            Write-Info ('Переключаю активный тулчейн на ' + $script:RustToolchain + '.')
            & $rustup.Source 'default' $script:RustToolchain
            if ($LASTEXITCODE -ne 0) {
                throw 'Не удалось назначить тулчейн по умолчанию.'
            }
        }
    }

    $version = Get-RustcVersion
    if (-not $version) {
        throw 'rustc не отвечает на --version. Проверьте установку Rust.'
    }
    if ($version -lt $script:MinRustVersion) {
        if ($NoInstall) {
            throw ('rustc ' + $version + ' старее требуемого ' + $script:MinRustVersion + '. Выполните: rustup update stable')
        }
        Write-Warn ('rustc ' + $version + ' старее требуемого ' + $script:MinRustVersion + ', обновляю stable.')
        & $rustup.Source 'update' 'stable'
        if ($LASTEXITCODE -ne 0) {
            throw 'Не удалось обновить Rust через rustup update stable.'
        }
        $version = Get-RustcVersion
        if ((-not $version) -or ($version -lt $script:MinRustVersion)) {
            throw ('После обновления rustc всё ещё ' + $version + ', нужен ' + $script:MinRustVersion + '+.')
        }
    }

    if (-not (Get-CargoPath)) {
        throw 'cargo не найден в PATH. Перезапустите терминал после установки Rust.'
    }
    Write-Ok ('Rust: rustc ' + $version + ' | тулчейн ' + $script:RustToolchain)
}

function Install-Node {
    if (-not (Test-Command 'winget')) {
        throw ('Node.js не установлен, а winget недоступен. Скачайте LTS вручную: https://nodejs.org/en/download')
    }
    Write-Info 'Устанавливаю Node.js LTS через winget: OpenJS.NodeJS.LTS'
    & winget 'install' '--id' 'OpenJS.NodeJS.LTS' '-e' '--source' 'winget' '--accept-source-agreements' '--accept-package-agreements' '--silent'
    $code = $LASTEXITCODE
    if (($code -ne 0) -and ($code -ne -1978335189)) {
        throw ('winget завершился с кодом ' + $code + '. Установите Node.js LTS вручную: https://nodejs.org/en/download')
    }
    Update-SessionPath
}

function Ensure-Node {
    param([switch]$NoInstall)

    $version = Get-NodeVersion
    $major = 0
    if ($version) {
        $majorText = ([string]$version).Split('.')[0]
        [void][int]::TryParse($majorText, [ref]$major)
    }

    if ($version -and ($major -ge $script:MinNodeMajor)) {
        Write-Ok ('Node.js: v' + $version)
    } else {
        if ($version) {
            Write-Warn ('Node.js v' + $version + ' старее ' + $script:MinNodeMajor + '.')
        } else {
            Write-Warn 'Node.js не найден.'
        }
        if ($NoInstall) {
            throw ('Нужен Node.js ' + $script:MinNodeMajor + '+. Установите: winget install --id OpenJS.NodeJS.LTS -e')
        }
        Install-Node
        $version = Get-NodeVersion
        if (-not $version) {
            throw 'Node.js установлен, но не виден в PATH. Перезапустите терминал и повторите запуск.'
        }
        $major = 0
        [void][int]::TryParse(([string]$version).Split('.')[0], [ref]$major)
        if ($major -lt $script:MinNodeMajor) {
            throw ('После установки Node.js всё ещё v' + $version + ', нужен ' + $script:MinNodeMajor + '+.')
        }
        Write-Ok ('Node.js: v' + $version)
    }

    $npm = Get-NpmPath
    if (-not $npm) {
        throw 'npm не найден рядом с Node.js. Переустановите Node.js LTS.'
    }
    $npmVersion = ''
    try { $npmVersion = [string](& $npm '--version') } catch { $npmVersion = '' }
    Write-Ok ('npm: ' + $npmVersion.Trim())
}

# ---------------------------------------------------------------------------
# Шаги сборки
# ---------------------------------------------------------------------------

function Invoke-Clean {
    Write-Step 'Очистка артефактов (-Clean)'

    foreach ($relative in @('node_modules', 'dist')) {
        $path = Get-RepoPath $relative
        if (Test-Path -LiteralPath $path) {
            Write-Info ('Удаляю ' + $path)
            Remove-Item -LiteralPath $path -Recurse -Force
        } else {
            Write-Info ($relative + ': нечего удалять')
        }
    }

    $cargo = Get-CargoPath
    if (Test-Path -LiteralPath (Get-RepoPath 'target')) {
        Invoke-Native $cargo @('clean') 'cargo clean'
    } else {
        Write-Info 'target: нечего удалять'
    }
    Write-Ok 'Очистка завершена.'
}

function Install-FrontendDependencies {
    $npm = Get-NpmPath
    if ($NoInstall) {
        if (Test-Path -LiteralPath (Get-RepoPath 'node_modules')) {
            Write-Info 'node_modules на месте, установка пропущена (-NoInstall).'
        } else {
            Write-Warn 'node_modules отсутствует, а установка запрещена (-NoInstall): сборка, скорее всего, упадёт.'
        }
        return
    }

    $lock = Get-RepoPath 'package-lock.json'
    if (Test-Path -LiteralPath $lock) {
        Invoke-Native $npm @('ci') 'Установка зависимостей: npm ci'
    } else {
        Write-Warn 'package-lock.json не найден, ставлю зависимости через npm install (медленнее и без фиксации версий).'
        Invoke-Native $npm @('install') 'Установка зависимостей: npm install'
    }
}

function Invoke-CheckContrast {
    $npm = Get-NpmPath
    Invoke-Native $npm @('run', 'check:contrast') 'Проверка контрастности интерфейса'
}

function Invoke-FrontendBuild {
    $npm = Get-NpmPath
    Invoke-Native $npm @('run', 'build') 'Сборка фронтенда (tsc + vite)'
}

function Invoke-RustTests {
    if ($SkipTests) {
        Write-Info 'Тесты пропущены (-SkipTests).'
        return
    }
    if ($Dev) {
        Write-Info 'В режиме разработки (-Dev) тесты не запускаются.'
        return
    }
    $cargo = Get-CargoPath
    Invoke-Native $cargo @('test', '--workspace') 'Тесты Rust: cargo test --workspace'
}

function Invoke-TauriBuild {
    $npm = Get-NpmPath
    if ($Dev) {
        Write-Step 'Режим разработки: npm run tauri dev'
        Write-Info 'Откроется окно приложения, в нём будет работать горячая перезагрузка интерфейса.'
        Write-Info 'Закройте окно приложения, чтобы выйти из режима разработки.'
        Write-Cmd 'npm run tauri dev'
        & $npm 'run' 'tauri' 'dev'
        if ($LASTEXITCODE -ne 0) {
            throw ('npm run tauri dev завершился с кодом ' + $LASTEXITCODE + '.')
        }
        return
    }

    if ($BundleOnly) {
        Invoke-Native $npm @('run', 'tauri', 'build', '--', '--bundles', 'nsis') 'Релизная сборка: только установщик NSIS'
    } else {
        Invoke-Native $npm @('run', 'tauri', 'build') 'Релизная сборка: exe + установщики NSIS и MSI'
    }
}

function Show-Artifacts {
    Write-Step 'Артефакты'

    $mainExe = Get-RepoPath 'target\release\torrenta.exe'
    if (Test-Path -LiteralPath $mainExe) {
        $item = Get-Item -LiteralPath $mainExe
        Write-Host ('      ' + $item.FullName + '   ' + (Format-Size $item.Length)) -ForegroundColor White
    } else {
        Write-Warn 'Не найден target\release\torrenta.exe.'
    }

    $files = @()
    foreach ($relative in @('target\release\bundle', 'src-tauri\target\release\bundle')) {
        $root = Get-RepoPath $relative
        if (-not (Test-Path -LiteralPath $root)) { continue }
        $found = @(Get-ChildItem -LiteralPath $root -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { ($_.Extension -eq '.exe') -or ($_.Extension -eq '.msi') })
        $files += $found
    }

    if ($files.Count -eq 0) {
        Write-Warn 'Установщики не найдены в target\release\bundle: посмотрите вывод сборки выше.'
        return
    }

    foreach ($file in ($files | Sort-Object -Property FullName)) {
        Write-Host ('      ' + $file.FullName + '   ' + (Format-Size $file.Length)) -ForegroundColor White
    }
    Write-Info ('Установщиков найдено: ' + $files.Count)
}

function Get-ModeName {
    if ($Dev) { return 'разработка (-Dev)' }
    if ($BundleOnly) { return 'только NSIS (-BundleOnly)' }
    return 'релизная сборка'
}

function Get-FlagsText {
    $flags = @()
    if ($SkipTests) { $flags += '-SkipTests' }
    if ($NoInstall) { $flags += '-NoInstall' }
    if ($Dev) { $flags += '-Dev' }
    if ($Clean) { $flags += '-Clean' }
    if ($BundleOnly) { $flags += '-BundleOnly' }
    if ($flags.Count -eq 0) { return 'нет' }
    return ($flags -join ' ')
}

function Show-Banner {
    Write-Host ''
    Write-Host '  Torrenta: сборка под Windows x64' -ForegroundColor Cyan
    Write-Host ('  каталог: ' + $script:RepoRoot) -ForegroundColor DarkGray
    Write-Host ('  режим: ' + (Get-ModeName) + ' | параметры: ' + (Get-FlagsText)) -ForegroundColor DarkGray
    Write-Host '  справка: .\build-windows.ps1 -Help' -ForegroundColor DarkGray
}

function Invoke-Build {
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    Show-Banner

    if ($Clean -and $NoInstall) {
        throw 'Параметры -Clean и -NoInstall несовместимы: очистка удаляет node_modules, а -NoInstall запрещает их устанавливать.'
    }

    Push-Location $script:RepoRoot
    try {
        Write-Step 'Проверка окружения'
        Assert-RepoLayout
        Assert-WindowsX64

        $msvc = Get-MsvcInstallPath
        if ($msvc) {
            Write-Ok ('MSVC: ' + $msvc)
        } else {
            Write-Warn 'MSVC не найден, но установка инструментов не запрещена: пробую продолжить.'
        }

        Assert-WebView2

        Write-Step 'Инструменты сборки'
        Ensure-Rust -NoInstall:$NoInstall
        Ensure-Node -NoInstall:$NoInstall

        if (-not $msvc) {
            Assert-Msvc
        }

        if ($Clean) {
            Invoke-Clean
        }

        Write-Step 'Зависимости фронтенда'
        Install-FrontendDependencies

        Invoke-CheckContrast

        if (($Dev -eq $false) -and ($BundleOnly -eq $false)) {
            Invoke-FrontendBuild
        } else {
            Write-Info 'Сборка фронтенда выполнится внутри tauri build/tauri dev.'
        }

        Invoke-RustTests
        Invoke-TauriBuild
    } finally {
        Pop-Location
    }

    $stopwatch.Stop()

    if ($Dev) {
        Write-Step 'Итог'
        Write-Ok ('Режим разработки завершён за ' + ('{0:mm\:ss}' -f $stopwatch.Elapsed) + '.')
        return
    }

    Show-Artifacts

    Write-Step 'Итог'
    Write-Ok ('Готово за ' + ('{0:mm\:ss}' -f $stopwatch.Elapsed) + ', код возврата 0.')
    Write-Info 'Запуск: .\target\release\torrenta.exe'
    Write-Info 'Документация по сборке: docs\BUILD.md'
}

function Show-Help {
    Write-Host ''
    Write-Host '  Torrenta: сборка под Windows x64' -ForegroundColor Cyan
    Write-Host '  ================================' -ForegroundColor DarkGray
    Write-Host ''
    Write-Host '  Использование:' -ForegroundColor White
    Write-Host '    .\build-windows.ps1 [параметры]'
    Write-Host '    .\build-windows.bat [параметры]'
    Write-Host ''
    Write-Host '  Параметры:' -ForegroundColor White
    Write-Host '    -SkipTests    не запускать cargo test'
    Write-Host '    -NoInstall    ничего не устанавливать: только проверить Rust, Node.js и node_modules'
    Write-Host '    -Dev          режим разработки: npm run tauri dev вместо релизной сборки'
    Write-Host '    -Clean        очистка перед сборкой: cargo clean, удаление node_modules и dist'
    Write-Host '    -BundleOnly   только установщик NSIS: npm run tauri build -- --bundles nsis'
    Write-Host '    -Help         эта справка'
    Write-Host ''
    Write-Host '  Что делает обычный запуск:' -ForegroundColor White
    Write-Host '    1. Проверяет Windows x64, MSVC (VS Build Tools), WebView2 Runtime.'
    Write-Host '    2. Проверяет Rust с MSVC-тулчейном и Node.js 20+, при необходимости ставит их.'
    Write-Host '    3. npm ci (или npm install, если нет package-lock.json).'
    Write-Host '    4. npm run check:contrast: проверка контрастности палитры интерфейса.'
    Write-Host '    5. npm run build: сборка фронтенда (tsc --noEmit + vite build).'
    Write-Host '    6. cargo test --workspace: тесты ядра и обвязки.'
    Write-Host '    7. npm run tauri build: релизный exe и установщики NSIS + MSI.'
    Write-Host '    8. Печатает пути и размеры артефактов.'
    Write-Host ''
    Write-Host '  Артефакты:' -ForegroundColor White
    Write-Host '    target\release\torrenta.exe'
    Write-Host '    target\release\bundle\nsis\Torrenta_1.0.0_x64-setup.exe'
    Write-Host '    target\release\bundle\msi\Torrenta_1.0.0_x64_*.msi'
    Write-Host ''
    Write-Host '  Примеры:' -ForegroundColor White
    Write-Host '    .\build-windows.ps1                        обычная релизная сборка'
    Write-Host '    .\build-windows.ps1 -SkipTests -NoInstall  быстрая пересборка без установки и тестов'
    Write-Host '    .\build-windows.ps1 -Dev                   разработка с горячей перезагрузкой UI'
    Write-Host '    .\build-windows.ps1 -Clean                 сборка с нуля'
    Write-Host '    .\build-windows.ps1 -BundleOnly            только установщик NSIS'
    Write-Host ''
    Write-Host '  Коды возврата: 0 - успех, 1 - ошибка (ищите строку с [x]).' -ForegroundColor DarkGray
    Write-Host '  Подробная инструкция: docs\BUILD.md' -ForegroundColor DarkGray
    Write-Host ''
}

# ---------------------------------------------------------------------------
# Точка входа
# ---------------------------------------------------------------------------

if ($Help) {
    Show-Help
    exit 0
}

try {
    Invoke-Build
    exit 0
} catch {
    Write-Host ''
    Write-Fail $_.Exception.Message
    $position = $null
    if ($_.InvocationInfo) { $position = $_.InvocationInfo.PositionMessage }
    if ($position) {
        $firstLine = ([string]$position).Split([Environment]::NewLine)[0]
        if (-not [string]::IsNullOrWhiteSpace($firstLine)) {
            Write-Host ('      ' + $firstLine) -ForegroundColor DarkGray
        }
    }
    Write-Host ''
    Write-Host '  Что делать: см. docs\BUILD.md или запустите .\build-windows.ps1 -Help' -ForegroundColor DarkGray
    exit 1
}
