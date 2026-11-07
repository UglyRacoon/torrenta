#!/usr/bin/env node
/**
 * Проверка контраста палитры Torrenta по WCAG 2.1 AA.
 *
 * Скрипт читает `src/styles/tokens.css`, вытаскивает цветовые токены
 * (`--color-*`) из блоков `@theme dark` и `@theme light` и считает
 * коэффициент контраста для пар «текст/фон», «иконка/фон», «граница/фон».
 *
 * Правила:
 *   • обычный текст            — не ниже 4.5:1;
 *   • крупный текст, иконки,
 *     графические элементы,
 *     границы интерактивных
 *     контролов и фокус-ринг   — не ниже 3:1.
 *
 * Запуск: `npm run check:contrast`. При нарушении — код выхода 1.
 */

import { readFileSync } from "node:fs";
import { dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const TOKENS_PATH = resolve(ROOT, "src", "styles", "tokens.css");

const PAIRS = [
  // ——— основной текст: 4.5:1 ———
  { fg: "text", bg: "bg", min: 4.5, role: "основной текст / фон" },
  { fg: "text", bg: "bg-elevated", min: 4.5, role: "основной текст / боковая панель" },
  { fg: "text", bg: "surface", min: 4.5, role: "основной текст / карточка" },
  { fg: "text", bg: "surface-2", min: 4.5, role: "основной текст / hover-строка" },
  { fg: "text", bg: "surface-3", min: 4.5, role: "основной текст / выбранная строка" },
  { fg: "text-muted", bg: "bg", min: 4.5, role: "вторичный текст / фон" },
  { fg: "text-muted", bg: "bg-elevated", min: 4.5, role: "вторичный текст / панель" },
  { fg: "text-muted", bg: "surface", min: 4.5, role: "вторичный текст / карточка" },
  { fg: "text-muted", bg: "surface-2", min: 4.5, role: "вторичный текст / hover" },
  { fg: "text-faint", bg: "bg", min: 4.5, role: "мета-текст / фон" },
  { fg: "text-faint", bg: "bg-elevated", min: 4.5, role: "мета-текст / панель" },
  { fg: "text-faint", bg: "surface", min: 4.5, role: "мета-текст / карточка" },

  // ——— текст на цветных заливках: 4.5:1 ———
  { fg: "on-accent", bg: "accent", min: 4.5, role: "текст на акцентной кнопке" },
  { fg: "accent-soft-ink", bg: "accent-soft", min: 4.5, role: "бейдж «Загрузка»" },
  { fg: "success-ink", bg: "success-soft", min: 4.5, role: "бейдж «Раздача»" },
  { fg: "warning-ink", bg: "warning-soft", min: 4.5, role: "бейдж «Метаданные»" },
  { fg: "danger-ink", bg: "danger-soft", min: 4.5, role: "бейдж «Ошибка»" },
  { fg: "info-ink", bg: "info-soft", min: 4.5, role: "бейдж «В очереди»" },

  // ——— акцентный текст/ссылки: 4.5:1 ———
  { fg: "accent-strong", bg: "bg", min: 4.5, role: "акцентный текст / фон" },
  { fg: "accent-strong", bg: "bg-elevated", min: 4.5, role: "акцентный текст / панель" },
  { fg: "accent-strong", bg: "surface", min: 4.5, role: "акцентный текст / карточка" },

  // ——— графика, иконки, границы контролов, фокус: 3:1 ———
  { fg: "accent", bg: "bg", min: 3, role: "акцентная графика / фон" },
  { fg: "accent", bg: "surface", min: 3, role: "акцентная графика / карточка" },
  { fg: "accent", bg: "track", min: 3, role: "заливка прогресса / трек" },
  { fg: "bar-from", bg: "track", min: 3, role: "тёмный край прогресса / трек" },
  { fg: "success", bg: "bg", min: 3, role: "иконка раздачи / фон" },
  { fg: "success", bg: "surface", min: 3, role: "иконка раздачи / карточка" },
  { fg: "warning", bg: "surface", min: 3, role: "иконка ожидания / карточка" },
  { fg: "danger", bg: "surface", min: 3, role: "иконка ошибки / карточка" },
  { fg: "info", bg: "surface", min: 3, role: "иконка очереди / карточка" },
  { fg: "focus", bg: "bg", min: 3, role: "фокус-ринг / фон" },
  { fg: "focus", bg: "surface", min: 3, role: "фокус-ринг / карточка" },
  { fg: "border-strong", bg: "surface", min: 3, role: "граница контрола / карточка" },
  { fg: "border-strong", bg: "bg", min: 3, role: "граница контрола / фон" },
];

const HEX = /^#([0-9a-f]{6})$/i;

function parseThemes(cssSource) {
  const marks = [...cssSource.matchAll(/\/\*\s*@theme\s+([a-z0-9-]+)\s*\*\//g)];
  if (marks.length === 0) {
    fail(`в ${relative(ROOT, TOKENS_PATH)} не найдено ни одного блока /* @theme … */`);
  }

  const themes = new Map();
  for (let i = 0; i < marks.length; i += 1) {
    const name = marks[i][1];
    const start = marks[i].index + marks[i][0].length;
    const end = i + 1 < marks.length ? marks[i + 1].index : cssSource.length;
    const block = cssSource.slice(start, end);

    const colors = new Map();
    for (const match of block.matchAll(/--color-([a-z0-9-]+)\s*:\s*([^;]+);/g)) {
      const token = match[1];
      const value = match[2].trim().toLowerCase();
      if (!HEX.test(value)) {
        fail(
          `токен --color-${token} в теме «${name}» задан как «${value}», ` +
            `а для расчёта нужен hex-цвет вида #RRGGBB`,
        );
      }
      colors.set(token, value);
    }
    themes.set(name, colors);
  }
  return themes;
}

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}

/** Относительная яркость по WCAG 2.1. */
function luminance(hex) {
  const value = parseInt(hex.slice(1), 16);
  const channels = [(value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff].map((channel) => {
    const srgb = channel / 255;
    return srgb <= 0.04045 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
}

/** Коэффициент контраста двух цветов: 1..21. */
function contrast(fg, bg) {
  const [light, dark] = [luminance(fg), luminance(bg)].sort((a, b) => b - a);
  return (light + 0.05) / (dark + 0.05);
}

function pad(text, width) {
  return text.length >= width ? text : text + " ".repeat(width - text.length);
}

function padStart(text, width) {
  return text.length >= width ? text : " ".repeat(width - text.length) + text;
}

function main() {
  const source = readFileSync(TOKENS_PATH, "utf8");
  const themes = parseThemes(source);

  const rule = "─".repeat(102);
  console.log("");
  console.log("  TORRENTA · ПРОВЕРКА КОНТРАСТА (WCAG 2.1 AA)");
  console.log(`  файл: ${relative(ROOT, TOKENS_PATH)}`);
  console.log("");

  let total = 0;
  let failed = 0;
  const problems = [];

  for (const [themeName, colors] of themes) {
    const title = themeName === "dark" ? "ТЁМНАЯ ТЕМА" : themeName === "light" ? "СВЕТЛАЯ ТЕМА" : themeName.toUpperCase();
    console.log(`  ${title} — ${colors.size} цветовых токенов`);
    console.log(`  ${rule}`);
    console.log(
      `  ${pad("Пара", 40)}${pad("Передний план", 18)}${pad("Фон", 18)}${padStart("Контраст", 11)}${padStart("Норма", 8)}  Итог`,
    );
    console.log(`  ${rule}`);

    for (const pair of PAIRS) {
      const fg = colors.get(pair.fg);
      const bg = colors.get(pair.bg);
      if (!fg || !bg) {
        const missing = !fg ? `--color-${pair.fg}` : `--color-${pair.bg}`;
        fail(`в теме «${themeName}» отсутствует ${missing} (нужен для пары «${pair.role}»)`);
      }

      const ratio = contrast(fg, bg);
      total += 1;
      const ok = ratio + 1e-9 >= pair.min;
      if (!ok) {
        failed += 1;
        problems.push({ themeName, role: pair.role, fg, bg, ratio, min: pair.min });
      }
      console.log(
        `  ${pad(pair.role, 40)}${pad(fg.toUpperCase(), 18)}${pad(bg.toUpperCase(), 18)}` +
          `${padStart(`${ratio.toFixed(2)}:1`, 11)}${padStart(pair.min.toFixed(1), 8)}  ${ok ? "OK" : "FAIL"}`,
      );
    }
    console.log("");
  }

  if (failed > 0) {
    console.error(`  ✗ Провалено пар: ${failed} из ${total}`);
    for (const problem of problems) {
      console.error(
        `    — [${problem.themeName}] ${problem.role}: ${problem.ratio.toFixed(2)}:1 ` +
          `(нужно ≥ ${problem.min.toFixed(1)}), ${problem.fg} на ${problem.bg}`,
      );
    }
    console.error("");
    process.exit(1);
  }

  console.log(`  ✓ Все пары проходят WCAG AA: ${total} из ${total}`);
  console.log("");
}

main();
