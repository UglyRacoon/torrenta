#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Генератор иконок приложения Torrenta.

Все иконки рисуются программно (Pillow + numpy), никаких внешних картинок.
Мотив: тёмная скруглённая плитка с бирюзовым градиентом и белой стрелкой
«скачать». Каждый размер рендерится отдельно с 4-кратным суперсэмплингом и
уменьшается фильтром Ланцоша, поэтому иконка остаётся чистой в 32x32.

Скрипт детерминирован: повторный запуск даёт побайтово те же файлы
(Pillow пишет PNG без временных меток).

Примеры:
    python scripts/make-icons.py                  # сгенерировать в src-tauri/icons
    python scripts/make-icons.py --check          # только проверить результат
    python scripts/make-icons.py --out ./icons    # другой каталог
"""

from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path

try:
    import numpy as np
    from PIL import Image, ImageDraw
except ImportError as exc:  # pragma: no cover - подсказка при отсутствии зависимостей
    sys.exit(
        "Не хватает зависимостей ({0}). Установите: python -m pip install pillow numpy".format(exc)
    )

# В интерактивной консоли Windows Python печатает через WriteConsoleW и кириллица
# видна всегда, а при перенаправлении вывода (CI, файл) он берёт кодовую страницу
# ANSI — тогда в UTF-8-логах получаются кракозябры. Поэтому для не-tty потоков
# принудительно включаем UTF-8.
for _stream in (sys.stdout, sys.stderr):
    if not _stream.isatty():
        try:
            _stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):  # старые интерпретаторы/обёртки
            pass

# ----------------------------------------------------------------- палитра ----
# Тёмная бирюза в левом верхнем углу -> почти чёрный синий в правом нижнем.
TILE_START = (0x12, 0x40, 0x45)
TILE_END = (0x06, 0x0E, 0x15)
GLOW_RGB = (0x2D, 0xD4, 0xBF)  # акцент продукта: бирюза #2DD4BF
ARROW_RGB = (0xFF, 0xFF, 0xFF)

# Скругление плитки: 22.5% стороны (совпадает с радиусами интерфейса).
RADIUS_RATIO = 0.225

# Геометрия стрелки в нормализованных координатах (0..1 внутри плитки).
STEM_HALF = 0.068  # половина ширины древка
HEAD_HALF = 0.225  # половина ширины наконечника
Y_TOP = 0.195
Y_BASE = 0.515
Y_APEX = 0.715
BAR_X0 = 0.235
BAR_X1 = 0.765
BAR_Y0 = 0.772
BAR_Y1 = 0.858

GLOW_STRENGTH = 0.42
GLOW_RADIUS = 0.72

SUPERSAMPLE = 4
MAX_RENDER = 2048
MIN_RENDER = 64

PNG_SIZES = [
    ("32x32.png", 32),
    ("128x128.png", 128),
    ("128x128@2x.png", 256),
    ("icon.png", 512),
    ("StoreLogo.png", 50),
    ("Square30x30Logo.png", 30),
    ("Square44x44Logo.png", 44),
    ("Square71x71Logo.png", 71),
    ("Square89x89Logo.png", 89),
    ("Square107x107Logo.png", 107),
    ("Square142x142Logo.png", 142),
    ("Square150x150Logo.png", 150),
    ("Square284x284Logo.png", 284),
    ("Square310x310Logo.png", 310),
]

ICO_NAME = "icon.ico"
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)


# ------------------------------------------------------------------ отрисовка --
def _render_size(size: int, supersample: int) -> int:
    """Размер холста до уменьшения: кратно больше целевого, но в разумных пределах."""
    return max(MIN_RENDER, min(int(round(size * supersample)), MAX_RENDER))


def _tile_pixels(n: int) -> np.ndarray:
    """Фон плитки: диагональный градиент + мягкое бирюзовое свечение за стрелкой."""
    ys = np.linspace(0.0, 1.0, n, dtype=np.float64)[:, None]
    xs = np.linspace(0.0, 1.0, n, dtype=np.float64)[None, :]

    diag = np.clip(xs * 0.35 + ys * 0.65, 0.0, 1.0) ** 0.85
    rgb = np.empty((n, n, 3), dtype=np.float64)
    for channel in range(3):
        rgb[:, :, channel] = TILE_START[channel] + (TILE_END[channel] - TILE_START[channel]) * diag

    gx = (xs - 0.5) / GLOW_RADIUS
    gy = (ys - 0.46) / GLOW_RADIUS
    dist = np.sqrt(gx * gx + gy * gy)
    glow = np.clip(1.0 - dist, 0.0, 1.0) ** 2.1 * GLOW_STRENGTH
    glow = glow[:, :, None]

    glow_rgb = np.array(GLOW_RGB, dtype=np.float64)
    return rgb * (1.0 - glow) + glow_rgb * glow


def _rounded_mask(n: int) -> Image.Image:
    mask = Image.new("L", (n, n), 0)
    draw = ImageDraw.Draw(mask)
    radius = int(round(n * RADIUS_RATIO))
    draw.rounded_rectangle((0, 0, n - 1, n - 1), radius=radius, fill=255)
    return mask


def _glyph_mask(n: int) -> Image.Image:
    """Белая стрелка вниз + полка под ней (классический знак «загрузка»)."""
    mask = Image.new("L", (n, n), 0)
    draw = ImageDraw.Draw(mask)

    def px(value: float) -> float:
        return value * n

    cx = 0.5
    arrow = [
        (px(cx - STEM_HALF), px(Y_TOP)),
        (px(cx + STEM_HALF), px(Y_TOP)),
        (px(cx + STEM_HALF), px(Y_BASE)),
        (px(cx + HEAD_HALF), px(Y_BASE)),
        (px(cx), px(Y_APEX)),
        (px(cx - HEAD_HALF), px(Y_BASE)),
        (px(cx - STEM_HALF), px(Y_BASE)),
    ]
    draw.polygon(arrow, fill=255)

    bar_radius = px((BAR_Y1 - BAR_Y0) / 2.0)
    draw.rounded_rectangle(
        (px(BAR_X0), px(BAR_Y0), px(BAR_X1), px(BAR_Y1)), radius=bar_radius, fill=255
    )
    return mask


def render(size: int, supersample: int = SUPERSAMPLE) -> Image.Image:
    """Отрисовать квадратную иконку заданной стороны (RGBA)."""
    n = _render_size(size, supersample)

    rgb = np.clip(_tile_pixels(n), 0.0, 255.0).astype(np.uint8)
    canvas = Image.fromarray(rgb).convert("RGBA")
    canvas = Image.composite(Image.new("RGBA", (n, n), ARROW_RGB + (255,)), canvas, _glyph_mask(n))
    canvas.putalpha(_rounded_mask(n))

    if canvas.size != (size, size):
        canvas = canvas.resize((size, size), Image.LANCZOS)
    return canvas


# ------------------------------------------------------------------ запись ----
def _write_pngs(out_dir: Path, supersample: int) -> list[Path]:
    written = []
    for name, size in PNG_SIZES:
        path = out_dir / name
        render(size, supersample).save(path, format="PNG", optimize=True)
        written.append(path)
    return written


def _write_ico(out_dir: Path, supersample: int) -> Path:
    """Многоразмерный .ico: каждый кадр рисуется отдельно, без «мыла» из 256."""
    frames = {size: render(size, supersample) for size in ICO_SIZES}
    largest = max(ICO_SIZES)
    path = out_dir / ICO_NAME
    frames[largest].save(
        path,
        format="ICO",
        sizes=[(size, size) for size in sorted(ICO_SIZES)],
        append_images=[frames[size] for size in sorted(ICO_SIZES) if size != largest],
    )
    return path


def generate(out_dir: Path, supersample: int = SUPERSAMPLE) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    files = _write_pngs(out_dir, supersample)
    files.append(_write_ico(out_dir, supersample))
    return files


# ----------------------------------------------------------------- проверка ---
def _ico_frame_sizes(path: Path) -> list[tuple[int, int]]:
    """Размеры кадров из каталога ICO (читаем заголовок вручную, без Pillow)."""
    data = path.read_bytes()
    if len(data) < 6 or data[:4] != b"\x00\x00\x01\x00":
        raise ValueError("не ICO-файл: {0}".format(path))
    count = struct.unpack_from("<H", data, 4)[0]
    frames = []
    for index in range(count):
        offset = 6 + index * 16
        width = data[offset] or 256
        height = data[offset + 1] or 256
        frames.append((width, height))
    return frames


def check(out_dir: Path) -> int:
    """Проверить, что все файлы на месте, валидны и нужных размеров."""
    problems = []
    total = 0

    for name, size in PNG_SIZES:
        path = out_dir / name
        if not path.is_file():
            problems.append("{0}: файла нет".format(name))
            continue
        data = path.read_bytes()
        total += len(data)
        if not data.startswith(b"\x89PNG\r\n\x1a\n"):
            problems.append("{0}: нет PNG-сигнатуры".format(name))
            continue
        with Image.open(path) as img:
            if img.size != (size, size):
                problems.append("{0}: размер {1}, ожидался {2}".format(name, img.size, (size, size)))
            if img.mode != "RGBA":
                problems.append("{0}: режим {1}, ожидался RGBA".format(name, img.mode))
            if img.getextrema()[3][1] != 255:
                problems.append("{0}: нет непрозрачных пикселей".format(name))
        print("  ok  {0:<24} {1:>4}x{1:<4} {2:>7} Б".format(name, size, len(data)))

    ico = out_dir / ICO_NAME
    if not ico.is_file():
        problems.append("{0}: файла нет".format(ICO_NAME))
    else:
        data = ico.read_bytes()
        total += len(data)
        frames = _ico_frame_sizes(ico)
        got = sorted(width for width, _ in frames)
        if got != sorted(ICO_SIZES):
            problems.append("{0}: кадры {1}, ожидались {2}".format(ICO_NAME, got, sorted(ICO_SIZES)))
        with Image.open(ico) as img:
            if img.size != (max(ICO_SIZES),) * 2:
                problems.append("{0}: основной кадр {1}".format(ICO_NAME, img.size))
        print(
            "  ok  {0:<24} кадры: {1}  {2:>7} Б".format(
                ICO_NAME, ", ".join(str(width) for width in got), len(data)
            )
        )

    if problems:
        print("\nНайдены проблемы:")
        for problem in problems:
            print("  - {0}".format(problem))
        return 1

    print("\nВсе проверки пройдены: {0} файлов, {1:.1f} КБ.".format(
        len(PNG_SIZES) + 1, total / 1024.0))
    return 0


# --------------------------------------------------------------------- CLI ----
def main(argv: list[str] | None = None) -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(
        description="Генерация иконок Torrenta (тёмная плитка, бирюзовый градиент, белая стрелка).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="По умолчанию иконки пишутся в <repo>/src-tauri/icons.",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=repo_root / "src-tauri" / "icons",
        help="каталог назначения (по умолчанию src-tauri/icons)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="ничего не рисовать, только проверить существующие файлы",
    )
    parser.add_argument(
        "--supersample",
        type=int,
        default=SUPERSAMPLE,
        help="коэффициент суперсэмплинга (по умолчанию {0})".format(SUPERSAMPLE),
    )
    args = parser.parse_args(argv)

    if args.supersample < 1:
        parser.error("--supersample должен быть >= 1")

    out_dir = args.out.resolve()

    if args.check:
        print("Проверка иконок в {0}".format(out_dir))
        return check(out_dir)

    print("Генерация иконок Torrenta в {0}".format(out_dir))
    files = generate(out_dir, args.supersample)
    print("Записано файлов: {0}".format(len(files)))
    print()
    return check(out_dir)


if __name__ == "__main__":
    sys.exit(main())
