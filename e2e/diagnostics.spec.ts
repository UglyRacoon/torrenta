import { expect, test } from "@playwright/test";

import { isVisible, openApp } from "./helpers";

/**
 * Экран «Диагностика» (1.7.0): сводка движка и состояние обновлений.
 *
 * Проверки открываются, как только кнопка/экран появятся в сборке.
 */
test.describe("Диагностика", () => {
  test("открывается кнопкой в сайдбаре", async ({ page }) => {
    await openApp(page);

    const button = page.getByRole("button", { name: /Диагностик/ });
    if (!(await isVisible(button))) {
      test.skip(true, "экран «Диагностика» ещё не реализован (1.7)");
    }
    await button.first().click();

    const dialog = page.getByRole("dialog", { name: /Диагностик/ });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByText(/DHT/i).first()).toBeVisible();
    await expect(dialog.getByText(/Обновлени/i).first()).toBeVisible();
    // Ключ подписи не настроен — экран обязан сказать это словами.
    await expect(dialog.getByText(/не настроен/i).first()).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });

  test("открывается по Ctrl+D", async ({ page }) => {
    await openApp(page);

    const button = page.getByRole("button", { name: /Диагностик/ });
    if (!(await isVisible(button))) {
      test.skip(true, "горячая клавиша Ctrl+D ещё не реализована (1.7)");
    }

    await page.keyboard.press("Control+d");

    const dialog = page.getByRole("dialog", { name: /Диагностик/ });
    await expect(dialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });
});
