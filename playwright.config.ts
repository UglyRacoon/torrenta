import { defineConfig, devices } from "@playwright/test";

/**
 * E2E-тесты Torrenta (1.7.0).
 *
 * Тесты идут против `vite preview` — то есть против обычной веб-сборки: Tauri
 * отсутствует, `src/api/engine.ts` видит, что `__TAURI_INTERNALS__` нет, и
 * поднимает демо-движок (`src/api/mock.ts`). Ничего в системе при этом не
 * меняется: ни настроек, ни сессии, ни загрузок.
 *
 * Отчёт: `playwright-report/`, черновики прогона — `test-results/`. Оба каталога
 * в `.gitignore`.
 *
 * Требования перед запуском:
 *   1. `npm run build` — `vite preview` отдаёт статику из `dist/`;
 *   2. установленный Google Chrome — браузеры Playwright не скачиваются,
 *      проект явно использует системный канал `chrome`
 *      (`npx playwright install` здесь не нужен и не поможет);
 *   3. свободный порт 4173 либо уже запущенный `npm run preview`
 *      (`reuseExistingServer`).
 *
 * Запуск: `npx playwright test` (или `npm run test:e2e`).
 */

/**
 * `@types/node` в проекте не установлен (зависимости не меняем), поэтому для
 * чтения переменных окружения объявляем минимум — глобальный `process` в Node
 * есть всегда, конфиг исполняется уже на стороне Playwright.
 */
declare const process: { env: Record<string, string | undefined> };

const PORT = 4173;
const BASE_URL = `http://localhost:${PORT}`;

export default defineConfig({
  testDir: "./e2e",
  outputDir: "test-results",
  // Демо-движок живёт на таймерах и общем `localStorage`: параллельные окна
  // мешали бы друг другу счётчиками и сортировкой, поэтому строго по одному.
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  timeout: 30_000,
  expect: {
    timeout: 7_000,
  },
  reporter: [
    ["list"],
    ["html", { outputFolder: "playwright-report", open: "never" }],
  ],
  use: {
    baseURL: BASE_URL,
    locale: "ru-RU",
    // Широкое окно: в узком сайдбаре подписи прячутся и кнопки становятся
    // иконками — доступные имена тогда приходят из `aria-label`, а не из
    // текста. Для стабильных селекторов держим «широкую» раскладку.
    viewport: { width: 1440, height: 900 },
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },
  projects: [
    {
      name: "chrome",
      use: {
        ...devices["Desktop Chrome"],
        // Системный Chrome вместо скачанного Chromium: браузеры Playwright
        // в этом окружении не устанавливались.
        channel: "chrome",
        viewport: { width: 1440, height: 900 },
      },
    },
  ],
  webServer: {
    command: "npm run preview",
    url: BASE_URL,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
});
