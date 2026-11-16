import { expect, test } from "@playwright/test";

import { listRows, openApp } from "./helpers";

/**
 * QA-проверки интерфейса по отчёту пользователя (1.9).
 *
 * Покрывают ровно то, что нельзя проверить модульными тестами: появление
 * кнопок только на строке под курсором, порядок блоков настроек, отсутствие
 * галочки «Запоминать последний путь» в диалоге добавления и неактивность
 * «Запускать свёрнутым в трей» без автозапуска.
 */
test.describe("QA 1.9: правки интерфейса", () => {
  test("кнопки строки появляются только при наведении", async ({ page }) => {
    await openApp(page);
    const row = listRows(page).first();
    // Кнопки лежат в контейнере, который и меняет прозрачность: у самой
    // кнопки `opacity` всегда 1, наследуется только визуальный эффект.
    const actions = row.locator(".torrent__actions");
    // Фокус мог остаться внутри строки после загрузки — `:focus-within` тоже
    // показывает кнопки (это нужно клавиатуре), поэтому для проверки наведения
    // фокус снимаем.
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await row.hover();
    await expect(actions).toHaveCSS("opacity", "1");
    // Уводим курсор в сайдбар: строка не под курсором и без фокуса — кнопок нет.
    await page.getByRole("button", { name: "Настройки" }).first().hover();
    await page.waitForTimeout(300);
    const active = await page.evaluate(() => {
      const node = document.activeElement as HTMLElement | null;
      return node ? `${node.tagName}.${node.className}` : "none";
    });
    const rowState = await row.evaluate((node) => ({
      focusWithin: node.matches(":focus-within"),
      hover: node.matches(":hover"),
    }));
    console.log("QA-HOVER active=", active, JSON.stringify(rowState));
    await expect(actions).toHaveCSS("opacity", "0");
    await page.screenshot({ path: "qa/screens/row-actions-hover.png" });
  });

  test("порядок блоков настроек соответствует отчёту", async ({ page }) => {
    await openApp(page);
    await page.getByRole("button", { name: "Настройки" }).first().click();
    const dialog = page.getByRole("dialog", { name: "Настройки" });
    await expect(dialog).toBeVisible();

    const order = await dialog.evaluate((root) => {
      const labels = Array.from(root.querySelectorAll(".field__label")).map(
        (node) => node.textContent?.trim() ?? "",
      );
      return labels;
    });
    const index = (name: string) => order.findIndex((label) => label === name);
    const expected = [
      "Запуск и трей",
      "Раздача",
      "Поведение",
      "Каталог загрузки",
      "Сеть",
    ];
    let previous = -1;
    for (const name of expected) {
      const at = index(name);
      expect(at, `блок «${name}» должен присутствовать`).toBeGreaterThanOrEqual(0);
      expect(at, `блок «${name}» стоит не по порядку: ${order.join(" | ")}`).toBeGreaterThan(
        previous,
      );
      previous = at;
    }
    // «Запоминать последний путь загрузки» живёт в настройках.
    await expect(dialog.getByText("Запоминать последний путь загрузки")).toBeVisible();
    await page.screenshot({ path: "qa/screens/settings-order.png" });
    await page.keyboard.press("Escape");
  });

  test("в диалоге добавления нет галочки «Запоминать последний путь»", async ({ page }) => {
    await openApp(page);
    await page.getByRole("button", { name: "Добавить" }).first().click();
    const chooserPromise = page.waitForEvent("filechooser");
    await page.getByRole("button", { name: "Выбрать файл…" }).click();
    const chooser = await chooserPromise;
    await chooser.setFiles({
      name: "QA раздача.torrent",
      mimeType: "application/x-bittorrent",
      buffer: Buffer.from("d4:infod4:name7:testeee", "utf8"),
    });
    const dialog = page.getByRole("dialog", { name: "Добавить торрент" });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByText(/Запоминать последний путь/i)).toHaveCount(0);
    await page.screenshot({ path: "qa/screens/add-dialog.png" });
    await page.keyboard.press("Escape");
  });

  test("«Запускать свёрнутым в трей» неактивна без автозапуска", async ({ page }) => {
    await openApp(page);
    await page.getByRole("button", { name: "Настройки" }).first().click();
    const dialog = page.getByRole("dialog", { name: "Настройки" });
    const autostart = dialog.getByRole("checkbox", {
      name: /Запускать при входе в Windows/i,
    });
    const minimized = dialog.getByRole("checkbox", {
      name: /Запускать свёрнутым в трей/i,
    });
    // По умолчанию автозапуск выключен — зависимая галочка неактивна.
    if (!(await autostart.isChecked())) {
      await expect(minimized).toBeDisabled();
      // Включаем автозапуск — галочка становится доступной.
      // Сам input визуально скрыт (стилизованная галочка с `pointer-events:
      // none`), поэтому нажимаем на него программно — ровно такой же `change`
      // получает и пользователь при клике по подписи.
      await autostart.evaluate((node) => (node as HTMLInputElement).click());
      await expect(autostart).toBeChecked();
      await expect(minimized).toBeEnabled();
      await autostart.evaluate((node) => (node as HTMLInputElement).click());
      await expect(autostart).not.toBeChecked();
      await expect(minimized).toBeDisabled();
    }
    await page.keyboard.press("Escape");
  });
});
