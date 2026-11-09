import { expect, test } from "@playwright/test";

import { isVisible, openApp } from "./helpers";

/**
 * Диалог «Настройки».
 *
 * Базовые группы («Сеть», «Раздача») есть с 1.6; поля последовательной
 * загрузки и расписания скорости появляются в 1.7 — вторая проверка сама
 * включится, когда они появятся в сборке. Третья проверяет блок «Ассоциация
 * файлов» (1.7): статус приходит из движка асинхронно, починка и снятый
 * переключатель влияют на кнопку, а `associate_files` переживает сохранение.
 */
test.describe("Настройки", () => {
  test("открывается и содержит группы «Сеть» и «Раздача»", async ({ page }) => {
    await openApp(page);

    await page.getByRole("button", { name: "Настройки" }).first().click();
    const dialog = page.getByRole("dialog", { name: "Настройки" });
    await expect(dialog).toBeVisible();

    await expect(dialog.getByText("Сеть", { exact: true }).first()).toBeVisible();
    await expect(dialog.getByText("Раздача", { exact: true }).first()).toBeVisible();

    // Закрытие — Esc (модальные окна слушают клавиатуру).
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });

  test("1.7: последовательная загрузка и расписание скорости", async ({ page }) => {
    await openApp(page);

    await page.getByRole("button", { name: "Настройки" }).first().click();
    const dialog = page.getByRole("dialog", { name: "Настройки" });
    await expect(dialog).toBeVisible();

    const sequential = dialog.getByText(/Последовательная загрузка/i).first();
    if (!(await isVisible(sequential))) {
      test.skip(true, "поля 1.7 (последовательная загрузка) ещё не в сборке");
    }

    await expect(sequential).toBeVisible();
    await expect(dialog.getByText(/Расписание скорости/i).first()).toBeVisible();
    // Расписание задаётся парой «с/по» — два поля времени.
    await expect(dialog.locator('input[type="time"]')).toHaveCount(2);

    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });

  test("1.7: ассоциация .torrent — статус, починка и сохранение", async ({ page }) => {
    await openApp(page);

    const openSettings = async () => {
      await page.getByRole("button", { name: "Настройки" }).first().click();
      const dialog = page.getByRole("dialog", { name: "Настройки" });
      await expect(dialog).toBeVisible();
      return dialog;
    };

    let dialog = await openSettings();
    const checkbox = () =>
      dialog.getByRole("checkbox", { name: /Связывать файлы \.torrent с Torrenta/ });
    const repair = () => dialog.getByRole("button", { name: "Починить ассоциацию" });
    const status = dialog.getByRole("status");

    // Состояние привязки читается из движка асинхронно — ждём текста, а не таймера.
    await expect(status).toHaveText("Torrenta открывает файлы .torrent");
    await expect(checkbox()).toBeChecked();
    await expect(repair()).toBeEnabled();

    // Починка: на время вызова кнопка блокируется, затем статус и подпись обновляются.
    await repair().click();
    await expect(status).toHaveText("Ассоциация восстановлена");
    await expect(repair()).toHaveText("Починить ассоциацию");
    await expect(repair()).toBeEnabled();

    // Снятый переключатель запрещает починку и объясняет причину.
    // Сам input визуально скрыт (`.checkbox__input`: 1×1, pointer-events: none),
    // поэтому переключаем кликом по тексту внутри его <label>.
    await dialog.getByText("Связывать файлы .torrent с Torrenta", { exact: true }).click();
    await expect(checkbox()).not.toBeChecked();
    await expect(repair()).toBeDisabled();
    await expect(dialog.getByText(/без него привязка не восстанавливается/)).toBeVisible();

    // Сохранение закрывает окно и подтверждает тостом.
    await dialog.getByRole("button", { name: "Сохранить" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByText("Настройки сохранены")).toBeVisible();

    // Круг через движок и стор: переоткрытое окно видит сохранённое значение.
    dialog = await openSettings();
    await expect(checkbox()).not.toBeChecked();
    await expect(repair()).toBeDisabled();

    // Второе закрытие — без изменений.
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });
});
