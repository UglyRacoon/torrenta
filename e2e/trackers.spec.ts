import { expect, test } from "@playwright/test";

import { isVisible, listRows, openApp } from "./helpers";

/** Открывает панель деталей первого торрента и вкладку «Трекеры». */
async function openTrackersTab(page: import("@playwright/test").Page) {
  // Одиночный клик только выбирает торрент; панель деталей открывает двойной
  // клик по строке (или «Открыть» в контекстном меню).
  await listRows(page).first().dblclick();
  const details = page.getByRole("complementary", { name: "Детали торрента" });
  await expect(details).toBeVisible();

  const trackersTab = details.getByRole("tab", { name: /Трекеры/ });
  if (!(await isVisible(trackersTab))) {
    test.skip(true, "вкладка «Трекеры» ещё не реализована (1.7)");
  }
  await trackersTab.click();
  await expect(trackersTab).toHaveAttribute("aria-selected", "true");

  return { details, panel: details.getByRole("tabpanel", { name: "Трекеры" }) };
}

test.describe("Вкладка «Трекеры»", () => {
  test("показывает список трекеров и действия", async ({ page }) => {
    await openApp(page);
    const { details, panel } = await openTrackersTab(page);

    await expect(panel).toBeVisible();
    // Демо-данные: трекеры у торрента есть, но пустой список — тоже нормальный
    // случай (работают DHT и PEX), поэтому допускаем оба варианта.
    await expect(panel).toContainText(/(udp:\/\/|https?:\/\/|Трекеров нет)/);
    await expect(panel.getByRole("button", { name: "Изменить", exact: true })).toBeVisible();
    await expect(panel.getByRole("button", { name: "Обновить трекеры" })).toBeVisible();

    // Esc закрывает панель деталей целиком.
    await page.keyboard.press("Escape");
    await expect(details).toBeHidden();
  });
});

test.describe("Редактор трекеров", () => {
  test("правит список трекеров и сохраняет его", async ({ page }) => {
    await openApp(page);
    const { details, panel } = await openTrackersTab(page);

    const edit = panel.getByRole("button", { name: "Изменить", exact: true });
    if (!(await isVisible(edit))) {
      test.skip(true, "кнопки редактора трекеров ещё нет (1.7)");
    }
    await edit.click();

    const dialog = page.getByRole("dialog", { name: "Редактор трекеров" });
    if (!(await isVisible(dialog, 2_000))) {
      test.skip(true, "модалка редактора трекеров ещё не реализована (1.7, frontend)");
    }
    await expect(dialog).toBeVisible();

    // Список подгружается из движка асинхронно; пока модалка занята, кнопка
    // «Добавить» выключена — дожидаемся её и только потом считаем строки.
    const addButton = dialog.getByRole("button", { name: "Добавить", exact: true });
    await expect(addButton).toBeEnabled();

    // Каждый трекер — отдельная строка ввода; адрес обязан начинаться со схемы.
    const rows = dialog.getByRole("textbox");
    const initialCount = await rows.count();
    for (let index = 0; index < initialCount; index += 1) {
      await expect(rows.nth(index)).toHaveValue(/^(udp|https?):\/\//);
    }

    // «Добавить» создаёт новую строку; сохраняем в неё уникальный адрес.
    await addButton.click();
    await expect(rows).toHaveCount(initialCount + 1);

    const draft = "udp://tracker.e2e.torrenta.test:1337/announce";
    await rows.last().fill(draft);
    await dialog.getByRole("button", { name: "Сохранить", exact: true }).click();
    await expect(dialog).toBeHidden();

    // Демо-движок сохраняет список у себя и сразу перерисовывает вкладку.
    await expect(panel).toContainText(draft);

    // Esc закрывает панель деталей.
    await page.keyboard.press("Escape");
    await expect(details).toBeHidden();
  });
});
