import { expect, test } from "@playwright/test";

import { isVisible, listRows, openApp } from "./helpers";

/**
 * Мультисортировка (1.7.0).
 *
 * Панель «Сортировка списка» — это `role="dialog"` без модальности: до трёх
 * ключей, порядок строк в панели задаёт приоритет. Спецификация сразу
 * сохраняется в `localStorage["torrenta.sort"]` массивом `[{ key, dir }, …]`.
 */
test.describe("Сортировка списка", () => {
  test("два ключа сортировки применяются и сохраняются", async ({ page }) => {
    await openApp(page);

    // Имя кнопки — «Сортировка списка: <цепочка>», поэтому ищем по префиксу.
    const sortButton = page.getByRole("button", { name: /^Сортировка списка/ }).first();
    if (!(await isVisible(sortButton))) {
      test.skip(true, "панели мультисортировки ещё нет в сборке (1.7)");
    }
    await sortButton.click();

    const panel = page.getByRole("dialog", { name: "Сортировка списка" });
    if (!(await isVisible(panel))) {
      test.skip(true, "панель «Сортировка списка» ещё не реализована (1.7)");
    }
    await expect(panel).toBeVisible();

    /** Панель закрывается повторным нажатием на кнопку-триггер. */
    const closePanel = async (): Promise<void> => {
      await sortButton.click();
      await expect(panel).toBeHidden();
    };

    // По умолчанию активен единственный ключ «Сначала новые»; добавляем
    // второй ключ «Размер». Список «Добавить ключ» не содержит занятых полей,
    // поэтому пункт отличается от текущего значения.
    await expect(panel.getByRole("button", { name: "Убрать ключ: Сначала новые" })).toBeVisible();
    await panel.getByRole("button", { name: /Добавить ключ сортировки/ }).click();
    await page.getByRole("option", { name: "Размер", exact: true }).click();
    await expect(panel.getByRole("button", { name: "Убрать ключ: Размер" })).toBeVisible();

    // Второй ключ уточняет порядок: поднимаем его в главный и проверяем,
    // что в хранилище поменялся именно порядок цепочки.
    await panel.getByRole("button", { name: "Выше приоритетом: Размер" }).click();

    // Ключи применяются сразу; закрываем панель и проверяем сохранение.
    await closePanel();
    await expect(listRows(page)).toHaveCount(7);

    const stored = await page.evaluate(() => window.localStorage.getItem("torrenta.sort"));
    expect(stored, "порядок сортировки должен сохраняться в localStorage").not.toBeNull();

    const keys = JSON.parse(stored ?? "[]") as Array<{ key?: string; dir?: string }>;
    expect(keys.map((item) => item.key)).toEqual(["size", "added"]);
    expect(keys.every((item) => item.dir === "asc" || item.dir === "desc")).toBe(true);

    // Цепочка видна и в подписи кнопки: «Сортировка списка: Размер …, Сначала новые».
    await expect(sortButton).toHaveAttribute("aria-label", /^Сортировка списка: Размер/);

    // Направление у главного ключа переключается на месте и тоже сохраняется.
    await sortButton.click();
    const dirSize = panel.getByRole("button", { name: /^Направление сортировки: / }).first();
    const dirBefore = (await dirSize.getAttribute("aria-label")) ?? "";
    await dirSize.click();
    await closePanel();
    const afterDir = await page.evaluate(() => window.localStorage.getItem("torrenta.sort"));
    const firstAfterDir = (JSON.parse(afterDir ?? "[]") as Array<{ dir?: string }>)[0];
    expect(firstAfterDir?.dir, `направление изменилось против «${dirBefore}»`).toBe(
      dirBefore.includes("убыв") ? "asc" : "desc",
    );

    // Сброс возвращает единственный ключ по умолчанию — состояние не залипает.
    await sortButton.click();
    await panel.getByRole("button", { name: "Сбросить", exact: true }).click();
    await closePanel();
    const afterReset = await page.evaluate(() => window.localStorage.getItem("torrenta.sort"));
    expect(JSON.parse(afterReset ?? "[]")).toEqual([{ key: "added", dir: "desc" }]);
  });
});
