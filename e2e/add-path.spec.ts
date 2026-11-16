import { expect, test } from "@playwright/test";

import { openApp } from "./helpers";

/**
 * Диалог добавления в стиле qBittorrent (1.9).
 *
 * Поле «Папка загрузки» — базовый каталог, а куда именно внутри него попадут
 * файлы, решает раскладка: «как в раздаче» (у многофайлового торрента есть
 * папка с его именем), «создавать папку» или «без папки». История каталогов
 * приходит из настроек и открывается по клику на поле.
 */
test.describe("Добавление торрента", () => {
  test("каталог, история и раскладка файлов", async ({ page }) => {
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

    // Поле показывает базовый каталог: папку раздачи добавляет движок по
    // выбранной раскладке, а не поле.
    const folder = dialog.getByRole("textbox", { name: "Папка загрузки" });
    await expect(folder).toHaveValue(/Downloads$/);

    // Подсказка объясняет, появится ли папка раздачи.
    await expect(dialog.getByText(/папку «Моя раздача»/i)).toBeVisible();

    // История каталогов из настроек (демо-движок кладёт туда три записи).
    await folder.click();
    const options = dialog
      .getByRole("listbox", { name: "Недавние папки загрузки" })
      .getByRole("option");
    await expect(options.first()).toBeVisible();
    await expect(options).toHaveCount(3);

    // «Без папки» меняет подсказку и не создаёт папку раздачи.
    await dialog.getByRole("button", { name: /Раскладка файлов/ }).click();
    await page.getByRole("option", { name: "Без папки" }).click();
    await expect(dialog.getByText(/без корневой папки/i)).toBeVisible();

    // Раскладка «Создавать папку» работает и для одного файла (папка по имени).
    await dialog.getByRole("button", { name: /Раскладка файлов/ }).click();
    await page.getByRole("option", { name: "Создавать папку" }).click();
    await expect(dialog.getByText(/папку «Моя раздача»/i)).toBeVisible();

    // Добавляем торрент: каталог должен попасть в историю настроек.
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
    await expect(secondFolder).toBeVisible();
    await secondFolder.click();
    const secondOptions = secondDialog
      .getByRole("listbox", { name: "Недавние папки загрузки" })
      .getByRole("option");
    await expect(secondOptions.first()).toBeVisible();
  });

  test("раскладка новых торрентов задаётся в настройках", async ({ page }) => {
    await openApp(page);
    await page.getByRole("button", { name: "Настройки" }).first().click();
    const settings = page.getByRole("dialog", { name: "Настройки" });
    await expect(settings.getByText("Сколько каталогов помнить")).toBeVisible();
    await expect(settings.getByText("Раскладка файлов новых торрентов")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(settings).toBeHidden();
  });
});
