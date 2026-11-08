import type { AppState } from "../state/store";
import type { AutoShutdownState } from "../api/types";
import { fmtActive, fmtRate } from "../lib/format";
import { h, setAttr, setText } from "../lib/dom";
import { icon } from "../lib/icons";

export interface StatusbarHandlers {
  /** Пользователь переключил «Выключить ПК после загрузок». */
  onToggleAutoShutdown(enabled: boolean): void;
  /** Пользователь нажал «Отменить» в отсчёте. */
  onCancelAutoShutdown(): void;
}

export interface Statusbar {
  update(state: AppState): void;
  setAutoShutdown(state: AutoShutdownState): void;
}

/** 42 → «0:42», 125 → «2:05». */
function fmtCountdown(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const minutes = Math.floor(total / 60);
  const rest = total % 60;
  return `${minutes}:${rest < 10 ? "0" : ""}${rest}`;
}

export function createStatusbar(mount: HTMLElement, handlers: StatusbarHandlers): Statusbar {
  const dot = h("span", { class: "statusbar__dot", attrs: { "aria-hidden": "true" } });
  const modeText = h("span", { text: "Подключение…" });
  const mode = h("span", { class: "statusbar__mode" }, dot, modeText);

  const portValue = h("span", { class: "num", text: "авто" });
  const port = h(
    "span",
    { class: "statusbar__item", attrs: { title: "Порт прослушивания" } },
    h("span", { class: "statusbar__key", text: "порт" }),
    portValue,
  );

  const engine = h("span", { class: "statusbar__engine", text: "—" });

  const downValue = h("b", { class: "num", text: "0 Б/с" });
  const downItem = h(
    "span",
    { class: "statusbar__item", attrs: { title: "Скорость приёма" } },
    icon("arrowDown", 12),
    downValue,
  );
  const upValue = h("b", { class: "num", text: "0 Б/с" });
  const upItem = h(
    "span",
    { class: "statusbar__item", attrs: { title: "Скорость отдачи" } },
    icon("arrowUp", 12),
    upValue,
  );
  const activeValue = h("b", { class: "num", text: "0" });
  const activeItem = h(
    "span",
    { class: "statusbar__item", attrs: { title: "Активных торрентов" } },
    h("span", { class: "statusbar__key", text: "активных" }),
    activeValue,
  );

  const version = h("span", { class: "statusbar__version", text: "Torrenta" });

  // --- Автовыключение: переключатель в статус-баре ---

  const switchKnob = h("span", { class: "switch__knob" });
  const switchTrack = h("span", { class: "switch__track", attrs: { "aria-hidden": "true" } }, switchKnob);
  const switchLabel = h("span", { class: "autoshut__label", text: "Выключить ПК после загрузок" });
  const autoSwitch = h(
    "button",
    {
      class: "switch autoshut",
      attrs: {
        type: "button",
        role: "switch",
        "aria-checked": "false",
        title: "Выключить компьютер после завершения всех загрузок",
      },
    },
    icon("power", 14),
    switchLabel,
    switchTrack,
  );

  // --- Отсчёт до выключения: строка над статус-баром ---

  const countdownText = h("span", { class: "autoshut__countdown-text", text: "Выключение через 0:00" });
  const countdownCancel = h("button", {
    class: "autoshut__cancel",
    text: "Отменить",
    attrs: { type: "button" },
  });
  const countdown = h(
    "div",
    {
      class: "autoshut__strip",
      attrs: { role: "status", "aria-live": "polite" },
      hidden: true,
    },
    h("span", { class: "autoshut__strip-icon" }, icon("power", 15)),
    countdownText,
    countdownCancel,
  );

  const left = h("div", { class: "statusbar__group" }, mode, port, engine);
  const right = h(
    "div",
    { class: "statusbar__group statusbar__group--right" },
    autoSwitch,
    downItem,
    upItem,
    activeItem,
    h("span", { class: "statusbar__sep", attrs: { "aria-hidden": "true" } }),
    version,
  );

  mount.append(countdown, left, right);
  mount.dataset.mode = "offline";

  let enabled = false;

  autoSwitch.addEventListener("click", () => {
    handlers.onToggleAutoShutdown(!enabled);
  });
  countdownCancel.addEventListener("click", () => handlers.onCancelAutoShutdown());

  function paintSwitch(next: boolean): void {
    enabled = next;
    setAttr(autoSwitch, "aria-checked", next ? "true" : "false");
    autoSwitch.classList.toggle("is-on", next);
    setAttr(
      autoSwitch,
      "title",
      next
        ? "Компьютер выключится после завершения загрузок"
        : "Выключить компьютер после завершения всех загрузок",
    );
  }

  function paintCountdown(pending: number | null): void {
    const visible = pending !== null && pending > 0;
    countdown.hidden = !visible;
    if (!visible) {
      return;
    }
    setText(countdownText, `Выключение через ${fmtCountdown(pending ?? 0)}`);
  }

  return {
    update(state) {
      const payload = state.payload;
      const totals = payload?.totals;

      if (!payload) {
        mount.dataset.mode = "offline";
        setText(modeText, state.ready ? "Нет связи" : "Подключение…");
        setAttr(mode, "title", "Нет связи с движком — показаны последние данные");
        setText(portValue, "—");
        setText(engine, "движок недоступен");
        setText(version, "Torrenta");
        return;
      }

      mount.dataset.mode = state.mode;
      setText(modeText, state.mode === "mock" ? "Демо-данные" : "Движок подключён");
      setAttr(
        mode,
        "title",
        state.mode === "mock"
          ? "Демо-режим: данные имитируются в браузере"
          : "Подключено к локальному движку",
      );

      const port = payload.app.listen_port;
      setText(portValue, port === 0 ? "авто" : String(port));
      setText(engine, `Torrenta ${payload.app.version}`);
      setAttr(engine, "title", `Движок ${payload.app.engine} · ${payload.app.platform}`);

      setText(downValue, fmtRate(totals?.down_rate ?? 0));
      setText(upValue, fmtRate(totals?.up_rate ?? 0));
      const active = payload.torrents.filter(
        (torrent) => torrent.state === "downloading" || torrent.state === "seeding" || torrent.state === "metadata",
      ).length;
      setText(activeValue, fmtActive(active));
      setText(version, `${payload.app.name} ${payload.app.version}`);
    },
    setAutoShutdown(state) {
      paintSwitch(state.enabled);
      paintCountdown(state.pending_seconds ?? null);
    },
  };
}
