import type { AppState } from "../state/store";
import { fmtActive, fmtCount, fmtRate } from "../lib/format";
import { h, setAttr, setText } from "../lib/dom";
import { icon } from "../lib/icons";

export interface Statusbar {
  update(state: AppState): void;
}

/**
 * Нижняя полоса: режим, порт, движок, суммарные скорости, активные, версия.
 * В демо-режиме честно пишет «Демо-данные», а не притворяется живым движком.
 */
export function createStatusbar(mount: HTMLElement): Statusbar {
  const dot = h("i", { class: "statusbar__dot" });
  const modeText = h("span", { class: "statusbar__mode-text", text: "Подключение…" });
  const mode = h("span", { class: "statusbar__mode" }, dot, modeText);

  const portValue = h("b", { class: "num", text: "—" });
  const port = h(
    "span",
    { class: "statusbar__item", attrs: { title: "Порт прослушивания" } },
    h("span", { class: "statusbar__key", text: "Порт" }),
    portValue,
  );
  const engine = h("span", { class: "statusbar__item statusbar__engine", text: "—" });

  const downValue = h("b", { class: "num", text: "0 Б/с" });
  const upValue = h("b", { class: "num", text: "0 Б/с" });
  const downItem = h(
    "span",
    { class: "statusbar__item statusbar__item--rate", attrs: { title: "Суммарный приём" } },
    icon("arrowDown", 12),
    downValue,
  );
  const upItem = h(
    "span",
    { class: "statusbar__item statusbar__item--rate", attrs: { title: "Суммарная отдача" } },
    icon("arrowUp", 12),
    upValue,
  );

  const activeValue = h("b", { class: "num", text: "0" });
  const activeItem = h(
    "span",
    { class: "statusbar__item", attrs: { title: "Активные торренты" } },
    activeValue,
  );

  const version = h("span", { class: "statusbar__item statusbar__version", text: "Torrenta" });

  const left = h("div", { class: "statusbar__left" }, mode, h("span", { class: "statusbar__sep" }), port, engine);
  const right = h(
    "div",
    { class: "statusbar__right" },
    downItem,
    upItem,
    activeItem,
    h("span", { class: "statusbar__sep" }),
    version,
  );

  mount.append(left, right);
  mount.dataset.mode = "offline";

  return {
    update(state) {
      const payload = state.payload;
      if (!payload) {
        mount.dataset.mode = "offline";
        setText(modeText, state.fatalError ? "Нет связи" : "Подключение…");
        setAttr(mode, "title", state.fatalError ?? "Ожидание ответа движка");
        setText(portValue, "—");
        setText(engine, "—");
        setText(downValue, "0 Б/с");
        setText(upValue, "0 Б/с");
        setText(activeValue, fmtCount(0));
        setText(version, "Torrenta");
        return;
      }

      mount.dataset.mode = state.mode;
      setText(modeText, state.mode === "mock" ? "Демо-данные" : "Движок подключён");
      setAttr(
        mode,
        "title",
        state.mode === "mock"
          ? "Открыто в браузере: работают встроенные демо-данные, файлы не скачиваются"
          : "Соединение с локальным движком Torrenta",
      );

      const listenPort = payload.settings.listen_port;
      setText(portValue, listenPort === 0 ? "авто" : String(listenPort));
      setText(engine, payload.app.engine);
      setAttr(engine, "title", `Движок ${payload.app.engine} · ${payload.app.platform}`);

      setText(downValue, fmtRate(payload.totals.down_rate));
      setText(upValue, fmtRate(payload.totals.up_rate));
      setText(activeValue, fmtActive(payload.totals.active));
      setText(version, `${payload.app.name} ${payload.app.version}`);
    },
  };
}
