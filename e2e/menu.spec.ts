import { expect, test } from "@playwright/test";

import { listRows, openApp } from "./helpers";

/**
 * Подменю «Ограничить скорость» (1.8.1).
 *
 * Регрессия: пункт подменю закрывал собственное меню по наведению, потому что
 * обработчик «наведение на обычный пункт закрывает подменю» срабатывал и внутри
 * подменю. Здесь проверяется весь путь: открыть меню → навести на пункт →
 * перевести курсор в подменю → выбрать скорость.
 */
test.describe("Контекстное меню: подменю скорости", () => {
  test("подменю не закрывается при наведении и выбирает лимит", async ({ page }) => {
    await openApp(page);

    const row = listRows(page).first();
    const box = await row.boundingBox();
    expect(box, "строка списка должна иметь размеры").not.toBeNull();
    await page.mouse.click(box!.x + 60, box!.y + 10, { button: "right" });

    const item = page.getByRole("menuitem", { name: /Ограничить скорость/ });
    await expect(item).toBeVisible();

    // Наводим на пункт — подменю открывается.
    await item.hover();
    const submenu = page.getByRole("menu").filter({ has: page.getByRole("menuitemradio") });
    await expect(submenu).toBeVisible();

    // Переводим курсор внутрь подменю и проверяем, что оно на месте.
    const option = page.getByRole("menuitemradio", { name: "256 КиБ/с", exact: true });
    await option.hover();
    await expect(submenu).toBeVisible();
    await expect(option).toBeVisible();

    // Выбор закрывает меню целиком — значит пункт действительно нажимается.
    await option.click();
    await expect(page.getByRole("menu")).toHaveCount(0);
  });
});
