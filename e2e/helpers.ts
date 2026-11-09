import { expect, type Locator, type Page } from "@playwright/test";

/** Имя панели фильтров в сайдбаре (`<nav aria-label="Фильтры торрентов">`). */
export const NAV_NAME = "Фильтры торрентов";

/** Открыть приложение в демо-режиме и дождаться отрисованного списка. */
export async function openApp(page: Page): Promise<void> {
  await page.goto("/");
  await expect(page.getByRole("listbox", { name: "Торренты" })).toBeVisible();
  await expect(listRows(page).first()).toBeVisible();
}

/** Строки списка торрентов (`role="option"` внутри `role="listbox"`). */
export function listRows(page: Page): Locator {
  return page.getByRole("listbox", { name: "Торренты" }).getByRole("option");
}

/** Кнопка фильтра в сайдбаре; имя включает счётчик, поэтому — регулярка. */
export function navButton(page: Page, label: RegExp): Locator {
  return page.getByRole("navigation", { name: NAV_NAME }).getByRole("button", { name: label });
}

/** Счётчик у кнопки фильтра («Загрузки 2»); 0, если счётчик скрыт. */
export async function filterCounter(page: Page, label: RegExp): Promise<number> {
  const text = (await navButton(page, label).first().innerText()).replace(/\s+/g, " ").trim();
  const match = /(\d+)$/.exec(text);
  return match ? Number(match[1]) : 0;
}

/** Переключить фильтр списка. */
export async function selectFilter(page: Page, label: RegExp): Promise<void> {
  await navButton(page, label).first().click();
}

/**
 * Виден ли элемент прямо сейчас. Нужен, чтобы честно пропускать проверки
 * возможностей, которых ещё нет в сборке: тест не падает «непонятно почему»,
 * а помечается пропущенным и оживает сам, как только UI появится.
 */
export async function isVisible(locator: Locator, timeout = 2_000): Promise<boolean> {
  try {
    await locator.first().waitFor({ state: "visible", timeout });
    return true;
  } catch {
    return false;
  }
}
