import { expect, test } from "@playwright/test";

import { filterCounter, listRows, navButton, openApp, selectFilter } from "./helpers";

/**
 * Список торрентов в демо-режиме (без Tauri).
 *
 * Демо-движок отдаёт 7 торрентов — это же число проверяет контракт 1.7.0.
 */
test.describe("Список торрентов", () => {
  test("загружает 7 демо-торрентов и заголовок фильтра", async ({ page }) => {
    await openApp(page);

    await expect(listRows(page)).toHaveCount(7);
    await expect(page.getByRole("heading", { level: 1, name: "Все торренты" })).toBeVisible();
    // Ищем по строкам списка, а не по всей странице: то же имя встречается
    // ещё в панели деталей и в списке файлов.
    await expect(
      listRows(page).filter({ hasText: "ubuntu-24.04.1-desktop-amd64.iso" }),
    ).toHaveCount(1);
    await expect(
      listRows(page).filter({ hasText: "Дюна. Часть вторая (2024) 2160p HDR10 HEVC.mkv" }),
    ).toHaveCount(1);
  });

  test("поиск по названию сужает список и показывает пустое состояние", async ({ page }) => {
    await openApp(page);

    const search = page.getByRole("searchbox", { name: "Поиск торрентов" });
    await search.fill("дюна");

    await expect(listRows(page)).toHaveCount(1);
    await expect(listRows(page).first()).toContainText("Дюна");

    await search.fill("ничего-такого-нет");
    await expect(listRows(page)).toHaveCount(0);
    await expect(page.getByRole("heading", { name: "Ничего не найдено" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Сбросить фильтры" })).toBeVisible();

    await search.fill("");
    await expect(listRows(page)).toHaveCount(7);
  });

  test("фильтр по состоянию совпадает со счётчиком в сайдбаре", async ({ page }) => {
    await openApp(page);

    const downloading = await filterCounter(page, /^Загрузки/);
    expect(downloading, "в демо-данных есть активные загрузки").toBeGreaterThan(0);

    await selectFilter(page, /^Загрузки/);
    await expect(page.getByRole("heading", { level: 1, name: "Загрузки" })).toBeVisible();
    await expect(navButton(page, /^Загрузки/).first()).toHaveAttribute("aria-current", "page");
    await expect(listRows(page)).toHaveCount(downloading);

    await selectFilter(page, /^Все/);
    await expect(page.getByRole("heading", { level: 1, name: "Все торренты" })).toBeVisible();
    await expect(listRows(page)).toHaveCount(7);
  });

  test("фильтр «Пауза» показывает только приостановленные", async ({ page }) => {
    await openApp(page);

    const paused = await filterCounter(page, /^Пауза/);
    await selectFilter(page, /^Пауза/);

    await expect(page.getByRole("heading", { level: 1, name: "На паузе" })).toBeVisible();
    await expect(listRows(page)).toHaveCount(paused);
  });
});
