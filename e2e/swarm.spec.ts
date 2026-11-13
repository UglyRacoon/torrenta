import { expect, test } from "@playwright/test";

import { listRows, openApp } from "./helpers";

/**
 * Размер роя и остаток (1.8.0).
 *
 * Демо-движок отдаёт `swarm_seeds` / `swarm_leechers` и `remaining_bytes` так
 * же, как настоящий: трекер сообщает размер роя, движок — остаток по выбранным
 * файлам. Проверяем именно то, что видит пользователь, а не внутренние поля.
 */
test.describe("Рой и остаток", () => {
  test("строка списка показывает пиры и размер роя в скобках", async ({ page }) => {
    await openApp(page);

    const meta = listRows(page).first().locator(".torrent__meta-main");
    // Вид: «5,7 ГБ · пиры 27 (1 698)» — сначала наши сокеты, в скобках рой.
    await expect(meta).toHaveText(/· пиры \d+ \(\d[\d\s\u00a0]*\)/);

    const title = await meta.getAttribute("title");
    // Числа форматируются с разделителем разрядов (1 698), поэтому в шаблоне
    // допускаем пробелы и неразрывные пробелы, а не только \d.
    expect(title, "подсказка расшифровывает скобки").toMatch(
      /^подключено \d+ из [\d\s\u00a0\u202f]+ в рое$/,
    );
  });

  test("карточка деталей показывает сидов и личеров в рое", async ({ page }) => {
    await openApp(page);
    await listRows(page).first().dblclick();

    const details = page.getByRole("complementary", { name: "Детали торрента" });
    await expect(details).toBeVisible();

    // Шесть карточек статистики: четыре прежние и две про рой.
    const cards = details.locator(".stat");
    await expect(cards).toHaveCount(6);
    await expect(details.getByText("Сиды в рое", { exact: true })).toBeVisible();
    await expect(details.getByText("Личеры в рое", { exact: true })).toBeVisible();
    // Числа приходят из трекера, поэтому подпись говорит именно об этом.
    await expect(details.getByText("по данным трекера").first()).toBeVisible();

    // Сноска вкладки «Пиры» тоже знает размер роя.
    const peersTab = details.getByRole("tab", { name: /Пиры/ });
    await peersTab.click();
    await expect(details.getByText(/в рое \d/).first()).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(details).toBeHidden();
  });
});
