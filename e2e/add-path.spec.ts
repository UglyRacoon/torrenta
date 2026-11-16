import { expect, test } from "@playwright/test";

import { openApp } from "./helpers";

/**
 * Диалог добавления: папка раздачи в пути и история каталогов (1.9).
 *
 * Демо-движок для `.torrent`-файла отдаёт два файла в превью, поэтому папка с
 * именем раздачи должна добавиться к каталогу загрузки. История каталогов
 * хранится в `localStorage` и открывается кликом по полю.
 */
test.describe("Добавление торрента", () => {
  test("к каталогу добавляется папка раздачи, а в поле есть история", async ({ page }) => {
    await openApp(page);

    await page.getByRole("button", { name: "Добавить" }).first().click();
    const chooserPromise = page.waitForEvent("filechooser");
    await page.getByRole("button", { name: "Выбрать файл…" }).click();
    const chooser = await chooserPromise;
    await chooser.setFiles({
      name: "Моя раздача.torrent",
      mimeType: "application/x-bittorrent",
      buffer: Buffer.from("d4:infod4:name7:testeee", "utf8"),
    });

    const dialog = page.getByRole("dialog", { name: "Добавить торрент" });
    await expect(dialog).toBeVisible();

    const folder = dialog.getByRole("textbox", { name: "Папка загрузки" });
    // Путь оканчивается папкой раздачи: «…\Моя раздача».
    await expect(folder).toHaveValue(/Моя раздача$/);

    // Истории пока нет — список недавних каталогов пуст и не показывается.
    await folder.click();
    await expect(
      dialog.getByRole("listbox", { name: "Недавние папки загрузки" }),
    ).toBeHidden();

    // Добавляем торрент: базовый каталог должен запомниться в истории.
    await dialog.getByRole("button", { name: "Скачать", exact: true }).click();
    await expect(dialog).toBeHidden();

    await page.getByRole("button", { name: "Добавить" }).first().click();
    const secondChooser = page.waitForEvent("filechooser");
    await page.getByRole("button", { name: "Выбрать файл…" }).click();
    await (await secondChooser).setFiles({
      name: "Вторая раздача.torrent",
      mimeType: "application/x-bittorrent",
      buffer: Buffer.from("d4:infod4:name7:testeee", "utf8"),
    });
    const secondDialog = page.getByRole("dialog", { name: "Добавить торрент" });
    const secondFolder = secondDialog.getByRole("textbox", { name: "Папка загрузки" });
    await secondFolder.click();
    const options = secondDialog
      .getByRole("listbox", { name: "Недавние папки загрузки" })
      .getByRole("option");
    await expect(options.first()).toBeVisible();
    // В истории — каталог без папки раздачи.
    await expect(options.first()).not.toContainText("Моя раздача");
  });
});
