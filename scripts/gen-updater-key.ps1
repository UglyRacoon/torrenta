<#
.SYNOPSIS
    Генерирует пару ключей для подписи обновлений Torrenta (Tauri 2).

.DESCRIPTION
    Апдейтер Tauri проверяет подпись пакета обновления: приватным ключом
    подписывается установщик, публичный ключ зашит в приложение и проверяет
    подпись на стороне клиента. Без пары ключей автообновление включить нельзя.

    Скрипт создаёт ключи через официальный генератор из Tauri CLI 2
    (`tauri signer generate`) и печатает только ПУБЛИЧНЫЙ ключ — он идёт в
    src-tauri\tauri.conf.json. Приватный ключ скрипт не читает и не печатает:
    он остаётся в файле и передаётся только в секреты CI.

    Куда класть ключи. По умолчанию — .tauri-keys\torrenta.key в корне
    репозитория; каталог .tauri-keys уже в .gitignore и в репозиторий
    попасть не должен ни при каких условиях.

    Пароль. Приватный ключ шифруется паролем, иначе Tauri выдаёт
    предупреждение: ключ без пароля подходит для локальной проверки, но для
    релиза так делать не стоит. Пароль вводится скрыто (по умолчанию —
    дважды), его же нужно положить в секрет TAURI_SIGNING_PRIVATE_KEY_PASSWORD.

    Скрипт рассчитан на Windows PowerShell 5.1 и PowerShell 7.
    Полную справку печатает ключ -Help.

.PARAMETER Password
    Пароль приватного ключа. Если не задан, скрипт спросит его скрыто.
    Параметр нужен для неинтерактивного запуска (например, на стенде).

.PARAMETER KeyPath
    Путь к приватному ключу. Относительный путь считается от корня
    репозитория. По умолчанию .tauri-keys\torrenta.key; рядом появится
    файл .pub с публичным ключом.

.PARAMETER Force
    Перезаписать существующую пару ключей. Без него скрипт останавливается:
    старый приватный ключ нельзя терять — с ним перестанут проверяться уже
    выпущенные подписи.

.PARAMETER Help
    Показать справку и выйти, ничего не делая.

.EXAMPLE
    .\scripts\gen-updater-key.ps1

.EXAMPLE
    .\scripts\gen-updater-key.ps1 -Force -KeyPath .tauri-keys\release.key

.EXAMPLE
    .\scripts\gen-updater-key.ps1 -KeyPath .tauri-keys\ci.key -Password 'длинный-пароль'
#>
[CmdletBinding()]
param(
    [string]$Password,
    [string]$KeyPath,
    [switch]$Force,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Кириллица в консоли: иначе сообщения Tauri CLI превращаются в кракозябры.
try {
    $utf8 = New-Object -TypeName System.Text.UTF8Encoding -ArgumentList $false
    [Console]::OutputEncoding = $utf8
    $OutputEncoding = $utf8
} catch {
    Write-Verbose 'Не удалось переключить кодировку консоли на UTF-8.'
}

$script:RepoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($script:RepoRoot)) {
    $script:RepoRoot = (Get-Location).Path
}
$script:RepoRoot = [System.IO.Path]::GetFullPath($script:RepoRoot)
$script:DefaultKeyPath = '.tauri-keys\torrenta.key'

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

function Show-Help {
    Write-Host ''
    Write-Host '  gen-updater-key.ps1 — пара ключей для подписи обновлений Torrenta' -ForegroundColor White
    Write-Host ''
    Write-Host '    .\scripts\gen-updater-key.ps1                   ключи в .tauri-keys\torrenta.key'
    Write-Host '    .\scripts\gen-updater-key.ps1 -Force            перезаписать существующую пару'
    Write-Host '    .\scripts\gen-updater-key.ps1 -KeyPath <путь>   другой путь к приватному ключу'
    Write-Host '    .\scripts\gen-updater-key.ps1 -Password <пароль> без интерактивного запроса'
    Write-Host ''
    Write-Host '    -Help         эта справка' -ForegroundColor DarkGray
    Write-Host '    Публичный ключ печатается, приватный — никогда.' -ForegroundColor DarkGray
    Write-Host '    Подробности: docs\UPDATE.md' -ForegroundColor DarkGray
    Write-Host ''
}

# ---------------------------------------------------------------------------
# Вспомогательные функции
# ---------------------------------------------------------------------------

function Get-RepoPath {
    param([string]$Relative)
    if ([System.IO.Path]::IsPathRooted($Relative)) { return $Relative }
    return (Join-Path $script:RepoRoot $Relative)
}

function Resolve-KeyFilePath {
    if ([string]::IsNullOrWhiteSpace($KeyPath)) {
        return (Get-RepoPath $script:DefaultKeyPath)
    }
    return [System.IO.Path]::GetFullPath((Get-RepoPath $KeyPath))
}

# Tauri CLI из devDependencies: сначала локальный .cmd, потом npx.
function Get-TauriInvocation {
    $local = Join-Path $script:RepoRoot 'node_modules\.bin\tauri.cmd'
    if (Test-Path -LiteralPath $local) {
        return @{ File = $local; Prefix = @() }
    }

    $localNpx = Join-Path $script:RepoRoot 'node_modules\.bin\npx.cmd'
    if (Test-Path -LiteralPath $localNpx) {
        return @{ File = $localNpx; Prefix = @('tauri') }
    }

    $npx = Get-Command 'npx.cmd' -ErrorAction SilentlyContinue
    if (-not $npx) { $npx = Get-Command 'npx' -ErrorAction SilentlyContinue }
    if ($npx) {
        return @{ File = $npx.Source; Prefix = @('tauri') }
    }

    return $null
}

function Invoke-Tauri {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [string]$Echo
    )

    $invocation = Get-TauriInvocation
    if (-not $invocation) {
        throw 'Не найден Tauri CLI. Выполните npm ci в корне репозитория и повторите.'
    }

    $argv = @($invocation.Prefix) + $Arguments
    if ($Echo) { Write-Cmd $Echo }

    # Предупреждения CLI пишутся в stderr; не даём им превратиться в
    # исключение PowerShell — код возврата проверяем сами.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $invocation.File @argv
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }

    if ($null -eq $code) {
        throw ('Не удалось запустить ' + (Split-Path -Leaf $invocation.File))
    }
    if ($code -ne 0) {
        throw ('Tauri CLI завершился с кодом ' + $code + '. Сообщение выше.')
    }
}

# Скрытый ввод пароля; .NET Framework 4.x умеет SecureStringToBSTR.
function Read-PlainPassword {
    param([string]$Prompt)
    $secure = Read-Host -Prompt $Prompt -AsSecureString
    if ($null -eq $secure) { return '' }
    $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        return [System.Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
    } finally {
        [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    }
}

function Get-KeyPassword {
    if (-not [string]::IsNullOrEmpty($Password)) {
        return $Password
    }

    $first = Read-PlainPassword '  Пароль приватного ключа (пусто — без пароля): '
    if ([string]::IsNullOrEmpty($first)) {
        Write-Warn 'Ключ будет без пароля: так делать не рекомендуется, но для локальной проверки сойдёт.'
        return ''
    }

    $second = Read-PlainPassword '  Повторите пароль: '
    if ($first -cne $second) {
        throw 'Пароли не совпали. Запустите скрипт заново.'
    }
    return $first
}

function Format-RepoRelative {
    param([string]$FullPath)
    $root = $script:RepoRoot.TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar
    if ($FullPath.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $FullPath.Substring($root.Length)
    }
    return $FullPath
}

function Test-KeysIgnored {
    $gitignore = Get-RepoPath '.gitignore'
    if (-not (Test-Path -LiteralPath $gitignore)) { return $false }
    $text = Get-Content -LiteralPath $gitignore -Raw -Encoding UTF8
    return ($text -match '(?m)^\s*/?\.tauri-keys/?\s*$')
}

# ---------------------------------------------------------------------------
# Генерация
# ---------------------------------------------------------------------------

function Invoke-Generate {
    Write-Step 'Проверка окружения'

    if (-not (Test-Path -LiteralPath (Get-RepoPath 'node_modules\.bin\tauri.cmd'))) {
        Write-Warn 'Не вижу node_modules\.bin\tauri.cmd — вероятно, npm ci ещё не выполнялся.'
    }

    $keyFull = Resolve-KeyFilePath
    $pubFull = $keyFull + '.pub'
    $keyDir = Split-Path -Parent $keyFull

    Write-Info ('Приватный ключ: ' + (Format-RepoRelative $keyFull))
    Write-Info ('Публичный ключ: ' + (Format-RepoRelative $pubFull))

    if ((Test-Path -LiteralPath $keyFull) -and (-not $Force)) {
        throw ('Ключ уже существует: ' + (Format-RepoRelative $keyFull) + '. Перезаписать — с ключом -Force. Внимание: старым ключом подписаны прошлые обновления.')
    }

    if (-not (Test-Path -LiteralPath $keyDir)) {
        New-Item -ItemType Directory -Force -Path $keyDir | Out-Null
        Write-Ok ('Создан каталог ' + (Format-RepoRelative $keyDir))
    }

    if ($Force) {
        foreach ($stale in @($keyFull, $pubFull)) {
            if (Test-Path -LiteralPath $stale) {
                Remove-Item -LiteralPath $stale -Force
                Write-Info ('Удалён старый файл ' + (Format-RepoRelative $stale))
            }
        }
    }

    Write-Step 'Генерация ключей'
    $secret = Get-KeyPassword

    $argv = @('signer', 'generate', '-w', $keyFull, '--ci')
    if (-not [string]::IsNullOrEmpty($secret)) {
        $argv += @('-p', $secret)
    }
    if ($Force) { $argv += '-f' }

    # Пароль в эхо команды не попадает — печатаем только пометку.
    $echo = 'tauri signer generate -w ' + (Format-RepoRelative $keyFull) + ' --ci'
    if (-not [string]::IsNullOrEmpty($secret)) { $echo += ' -p ***' }
    Invoke-Tauri -Arguments $argv -Echo $echo

    if (-not (Test-Path -LiteralPath $keyFull)) {
        throw ('Tauri CLI не создал приватный ключ: ' + (Format-RepoRelative $keyFull))
    }
    if (-not (Test-Path -LiteralPath $pubFull)) {
        throw ('Tauri CLI не создал публичный ключ: ' + (Format-RepoRelative $pubFull))
    }

    $pub = (Get-Content -LiteralPath $pubFull -Raw -Encoding UTF8).Trim()
    if ([string]::IsNullOrWhiteSpace($pub)) {
        throw ('Публичный ключ пуст: ' + (Format-RepoRelative $pubFull))
    }

    Write-Ok 'Пара ключей создана.'

    Write-Step 'Публичный ключ — в src-tauri\tauri.conf.json'
    Write-Info 'Одна строка base64, копируется как есть:'
    Write-Host ''
    Write-Host $pub -ForegroundColor Green
    Write-Host ''
    Write-Info 'Фрагмент конфигурации (адрес сервера обновлений — свой):'
    Write-Host ''
    Write-Host '      "plugins": {' -ForegroundColor DarkGray
    Write-Host '        "updater": {' -ForegroundColor DarkGray
    Write-Host '          "active": true,' -ForegroundColor DarkGray
    Write-Host '          "endpoints": ["https://<сервер>/torrenta/{{target}}/{{arch}}/{{current_version}}"],' -ForegroundColor DarkGray
    Write-Host ('          "pubkey": "' + $pub + '"') -ForegroundColor DarkGray
    Write-Host '        }' -ForegroundColor DarkGray
    Write-Host '      }' -ForegroundColor DarkGray
    Write-Host ''

    Write-Step 'Приватный ключ — только в секреты CI'
    Write-Info ('Содержимое файла ' + (Format-RepoRelative $keyFull) + ' (целиком, вместе со строкой-комментарием)')
    Write-Info '  -> секрет репозитория TAURI_SIGNING_PRIVATE_KEY.'
    if ([string]::IsNullOrEmpty($secret)) {
        Write-Warn 'Ключ без пароля: секрет TAURI_SIGNING_PRIVATE_KEY_PASSWORD не нужен.'
    } else {
        Write-Info '  -> секрет TAURI_SIGNING_PRIVATE_KEY_PASSWORD: тот же пароль.'
    }
    Write-Info 'GitHub: Settings -> Secrets and variables -> Actions -> New repository secret.'
    Write-Info 'Локальная сборка: можно указать путь, а не содержимое — TAURI_SIGNING_PRIVATE_KEY_PATH.'

    Write-Step 'Проверки перед коммитом'
    if (Test-KeysIgnored) {
        Write-Ok '.gitignore уже игнорирует .tauri-keys — ключи в репозиторий не попадут.'
    } else {
        Write-Warn 'В .gitignore нет строки /.tauri-keys. Добавьте её вручную: скрипт .gitignore не меняет.'
    }
    Write-Info 'Приватный ключ скрипт не печатает и не читает — он остаётся только в файле.'
    Write-Info 'Никогда не коммитьте .tauri-keys и не пересылайте ключ в мессенджерах:'
    Write-Info 'потеря ключа или пароля ломает выпуск обновлений для уже установленных копий.'

    Write-Host ''
    Write-Host '  Дальше: docs\UPDATE.md — включение автообновления и проверка подписи.' -ForegroundColor DarkGray
    Write-Host ''
}

# ---------------------------------------------------------------------------
# Точка входа
# ---------------------------------------------------------------------------

Set-Location -LiteralPath $script:RepoRoot

if ($Help) {
    Show-Help
    exit 0
}

try {
    Invoke-Generate
    exit 0
} catch {
    Write-Host ''
    Write-Fail $_.Exception.Message
    Write-Host ''
    Write-Host '  Что делать: см. docs\UPDATE.md или запустите .\scripts\gen-updater-key.ps1 -Help' -ForegroundColor DarkGray
    exit 1
}
