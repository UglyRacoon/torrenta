/**
 * Окно диагностики (1.7): сеть и DHT, проброс порта, расписание скорости,
 * действующие лимиты, торренты, аптайм и состояние обновлений.
 *
 * Живёт отдельно от modal host (и от app.ts): кнопку в сайдбаре и горячую
 * клавишу Ctrl+D подключает `bindDiagnosticsHotkey`, сама кнопка дёргает
 * `openDiagnostics`. Пока окно открыто, данные перечитываются раз в 2 секунды,
 * плюс кнопка «Обновить» для ручного опроса.
 */
import { requireEngineApi } from "../api/engine";
import type { DiagnosticsDto, PortableInfoDto, UpdateStateDto } from "../api/types";
import { h, isEditableTarget, messageOf, setText } from "../lib/dom";
import { fmtCount, fmtRate } from "../lib/format";
import { icon } from "../lib/icons";

/** Период автообновления, пока окно открыто. */
const REFRESH_MS = 2000;

/** Что считается фокусируемым элементом внутри окна. */
const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

type Tone = "ok" | "warn" | "muted";

interface DiagRow {
  el: HTMLElement;
  set(value: string, tone?: Tone): void;
}

/** Строка «подпись — значение»; значение меняется при каждом обновлении. */
function diagRow(label: string, initial = "…"): DiagRow {
  const value = h("span", { class: "diag__value", text: initial });
  const el = h(
    "div",
    { class: "diag__row" },
    h("span", { class: "diag__label", text: label }),
    value,
  );
  return {
    el,
    set(text: string, tone?: Tone) {
      setText(value, text);
      value.classList.toggle("diag__value--ok", tone === "ok");
      value.classList.toggle("diag__value--warn", tone === "warn");
      value.classList.toggle("diag__value--muted", tone === "muted");
    },
  };
}

/** Блок строк с заголовком. */
function diagGroup(title: string, rows: DiagRow[]): HTMLElement {
  return h(
    "section",
    { class: "diag__group" },
    h("h3", { class: "diag__title", text: title }),
    h("div", { class: "diag__list" }, ...rows.map((row) => row.el)),
  );
}

function diagButton(text: string, className: string): HTMLButtonElement {
  return h("button", { class: `btn ${className}`, text, attrs: { type: "button" } });
}

/** 1234 КиБ/с → «1,2 МБ/с»; 0 → «без ограничения». */
function limitText(kib: number): string {
  return kib > 0 ? fmtRate(kib * 1024) : "без ограничения";
}

/** Минуты от полуночи → «ЧЧ:ММ». */
function clockText(minutes: number): string {
  const total = ((Math.round(Number(minutes) || 0) % 1440) + 1440) % 1440;
  return `${String(Math.floor(total / 60)).padStart(2, "0")}:${String(total % 60).padStart(2, "0")}`;
}

/** Аптайм: 45 → «45 с», 5400 → «1 ч 30 мин», 200000 → «2 д 7 ч». */
function uptimeText(seconds: number): string {
  const total = Math.max(0, Math.floor(Number(seconds) || 0));
  if (total < 60) {
    return `${total} с`;
  }
  const minutes = Math.floor(total / 60);
  if (total < 3600) {
    return `${minutes} мин`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours} ч ${minutes % 60} мин`;
  }
  return `${Math.floor(hours / 24)} д ${hours % 24} ч`;
}

/** Пароль в адресе прокси не показываем: socks5://user:***@host:port. */
function maskProxy(value: string): string {
  const scheme = /^[a-z0-9+.-]+:\/\//i.exec(value)?.[0] ?? "";
  const rest = value.slice(scheme.length);
  const at = rest.lastIndexOf("@");
  if (at < 0) {
    return value;
  }
  const userinfo = rest.slice(0, at);
  const colon = userinfo.indexOf(":");
  const masked = colon < 0 ? userinfo : `${userinfo.slice(0, colon)}:***`;
  return `${scheme}${masked}${rest.slice(at)}`;
}

/** Держим Tab внутри открытого окна: остальная часть приложения не должна ловить фокус. */
function trapFocus(event: KeyboardEvent, dialog: HTMLElement): void {
  const nodes = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (node) => !node.hidden && node.offsetParent !== null,
  );
  const first = nodes[0];
  const last = nodes[nodes.length - 1];
  if (!first || !last) {
    event.preventDefault();
    dialog.focus();
    return;
  }
  const active = document.activeElement;
  const inside = active instanceof Node && dialog.contains(active);
  if (event.shiftKey) {
    if (!inside || active === first) {
      event.preventDefault();
      last.focus();
    }
    return;
  }
  if (!inside || active === last) {
    event.preventDefault();
    first.focus();
  }
}

/** Открытое окно: одно на приложение, повторный вызов просто обновляет данные. */
let scrim: HTMLElement | null = null;
let timer: number | null = null;
let refreshNow: (() => void) | null = null;
let closeHandler: (() => void) | null = null;
let hotkeyBound = false;

export function openDiagnostics(): void {
  if (scrim !== null) {
    refreshNow?.();
    return;
  }
  const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;

  const title = h("h2", { class: "modal__title", text: "Диагностика", attrs: { id: "diag-title" } });
  const closeButton = h("button", {
    class: "icon-btn icon-btn--sm modal__close",
    attrs: { type: "button", "aria-label": "Закрыть диагностику" },
  });
  closeButton.append(icon("close", 16));

  const stamp = h("p", { class: "diag__stamp", text: "Читаем состояние движка…" });
  const errorEl = h("p", { class: "form-error", attrs: { role: "alert", hidden: "" } });

  // Сеть и порты.
  const port = diagRow("Порт прослушивания");
  const dht = diagRow("DHT");
  const dhtPort = diagRow("Порт DHT");
  const dhtNodes = diagRow("Узлы DHT");
  const pex = diagRow("PEX");
  const webSeeds = diagRow("Web seeds");
  const proxy = diagRow("Прокси");
  const mapping = diagRow("Проброс порта");

  // Расписание и ограничения.
  const schedule = diagRow("Расписание");
  const scheduleNow = diagRow("Сейчас");
  const limits = diagRow("Действующие лимиты");
  const sequential = diagRow("Последовательная загрузка");

  // Торренты и скорости.
  const torrents = diagRow("Торренты");
  const speed = diagRow("Скорости");
  const uptime = diagRow("Аптайм");

  // Обновления и данные приложения.
  const updates = diagRow("Обновления");
  const version = diagRow("Версия");
  const dataDir = diagRow("Каталог данных");
  const portable = diagRow("Портативный режим");

  const body = h(
    "div",
    { class: "modal__body diag" },
    stamp,
    diagGroup("Сеть", [port, dht, dhtPort, dhtNodes, pex, webSeeds, proxy, mapping]),
    diagGroup("Расписание и лимиты", [schedule, scheduleNow, limits, sequential]),
    diagGroup("Торренты и пиры", [torrents, speed, uptime]),
    diagGroup("Обновления", [updates, version]),
    diagGroup("Данные", [dataDir, portable]),
    errorEl,
  );

  const refreshButton = diagButton("Обновить", "btn--ghost");
  const closeFooter = diagButton("Закрыть", "btn--primary");
  const footer = h("footer", { class: "modal__foot" }, refreshButton, closeFooter);

  const dialog = h(
    "div",
    {
      class: "modal modal--diag",
      attrs: {
        role: "dialog",
        "aria-modal": "true",
        "aria-labelledby": "diag-title",
        tabindex: "-1",
      },
    },
    h("header", { class: "modal__head" }, title, closeButton),
    body,
    footer,
  );
  const scrimEl = h("div", { class: "modal-scrim" }, dialog);

  let busy = false;

  const apply = (data: DiagnosticsDto, update: UpdateStateDto, info: PortableInfoDto): void => {
    port.set(
      data.listen_port > 0 ? String(data.listen_port) : "не слушаем",
      data.listen_port > 0 ? undefined : "muted",
    );
    dht.set(data.dht ? "включён" : "выключен", data.dht ? "ok" : "muted");
    dhtPort.set(data.dht ? String(data.dht_port) : "—", data.dht ? undefined : "muted");
    dhtNodes.set(fmtCount(data.dht_nodes));
    pex.set(data.pex ? "включён" : "выключен", data.pex ? "ok" : "muted");
    webSeeds.set(data.web_seeds ? "включены" : "выключены", data.web_seeds ? "ok" : "muted");
    proxy.set(data.proxy === null ? "выключен" : maskProxy(data.proxy), data.proxy === null ? "muted" : "ok");
    if (!data.port_mapping) {
      mapping.set("выключен", "muted");
    } else if (data.mapped_external === null) {
      mapping.set("не удалось", "warn");
    } else {
      mapping.set(data.mapped_external, "ok");
    }

    if (data.schedule_enabled) {
      schedule.set(`окно ${clockText(data.schedule_from_min)}–${clockText(data.schedule_to_min)}`, "ok");
      scheduleNow.set(
        data.schedule_active ? "в окне: лимиты расписания" : "вне окна: основные лимиты",
        data.schedule_active ? "ok" : "muted",
      );
    } else {
      schedule.set("выключено", "muted");
      scheduleNow.set("—", "muted");
    }
    limits.set(`приём ${limitText(data.download_limit_kib)} · отдача ${limitText(data.upload_limit_kib)}`);
    sequential.set(data.sequential ? "включена" : "выключена", data.sequential ? "ok" : "muted");

    torrents.set(
      `${fmtCount(data.torrents)} · ${fmtCount(data.active)} активных · ${fmtCount(data.peers)} пиров`,
    );
    speed.set(`приём ${fmtRate(data.download_bps)} · отдача ${fmtRate(data.upload_bps)}`);
    uptime.set(uptimeText(data.uptime_secs));

    if (!update.configured) {
      updates.set(update.message ?? "не настроены: нет ключа подписи", "muted");
    } else if (update.available !== null) {
      updates.set(`доступна ${update.available}`, "warn");
    } else {
      updates.set(update.message ?? "обновлений нет", "ok");
    }
    version.set(update.current_version);

    dataDir.set(info.data_dir);
    portable.set(info.portable ? "да" : "нет", info.portable ? "ok" : "muted");
  };

  const pull = async (): Promise<void> => {
    if (busy || scrim === null || document.hidden) {
      return;
    }
    busy = true;
    try {
      const api = await requireEngineApi();
      const [data, update, info] = await Promise.all([
        api.engineDiagnostics(),
        api.checkUpdate(),
        api.portableInfo(),
      ]);
      if (scrim === null) {
        return;
      }
      apply(data, update, info);
      errorEl.hidden = true;
      setText(errorEl, "");
      setText(
        stamp,
        `Обновлено в ${new Date().toLocaleTimeString("ru-RU")} · обновляется каждые 2 с`,
      );
    } catch (reason) {
      if (scrim === null) {
        return;
      }
      errorEl.hidden = false;
      setText(errorEl, messageOf(reason));
      setText(stamp, "Не удалось прочитать состояние движка");
    } finally {
      busy = false;
    }
  };

  const stopTimer = (): void => {
    if (timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
  };

  const closeModal = (): void => {
    if (scrim === null) {
      return;
    }
    stopTimer();
    refreshNow = null;
    closeHandler = null;
    document.removeEventListener("keydown", onDialogKeydown, true);
    scrim.remove();
    scrim = null;
    if (!document.querySelector(".modal-scrim")) {
      document.documentElement.classList.remove("is-locked");
    }
    if (previous && previous.isConnected) {
      previous.focus();
    }
  };

  function onDialogKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeModal();
      return;
    }
    if (event.key === "Tab") {
      // Перехватываем до обработчиков приложения: фокус не должен уходить из окна.
      event.stopPropagation();
      trapFocus(event, dialog);
      return;
    }
    // Окно модальное: горячие клавиши списка не должны срабатывать за ним.
    event.stopPropagation();
  }

  closeButton.addEventListener("click", closeModal);
  closeFooter.addEventListener("click", closeModal);
  refreshButton.addEventListener("click", () => {
    void pull();
  });
  scrimEl.addEventListener("mousedown", (event) => {
    if (event.target === scrimEl) {
      closeModal();
    }
  });

  scrim = scrimEl;
  refreshNow = () => {
    void pull();
  };
  closeHandler = closeModal;
  document.documentElement.classList.add("is-locked");
  document.body.append(scrimEl);
  document.addEventListener("keydown", onDialogKeydown, true);
  dialog.focus();
  timer = window.setInterval(() => {
    void pull();
  }, REFRESH_MS);
  void pull();
}

/**
 * Ctrl+D (и ⌘D) открывает диагностику. Обработчик висит на фазе перехвата,
 * чтобы не срабатывать в полях ввода и не мешать другим клавишам приложения.
 */
function onHotkey(event: KeyboardEvent): void {
  if (event.defaultPrevented || event.altKey || event.shiftKey) {
    return;
  }
  if (!(event.ctrlKey || event.metaKey)) {
    return;
  }
  // Клавиша D в русской раскладке даёт «в», физически это та же кнопка.
  const key = event.key.toLowerCase();
  if (key !== "d" && key !== "в") {
    return;
  }
  if (isEditableTarget(event.target)) {
    return;
  }
  if (scrim === null && document.querySelector(".modal-scrim") !== null) {
    // Открыт другой диалог — не перебиваем его.
    return;
  }
  event.preventDefault();
  event.stopPropagation();
  openDiagnostics();
}

/** Подписка на Ctrl+D; вызывается один раз при сборке интерфейса. */
export function bindDiagnosticsHotkey(): void {
  if (hotkeyBound) {
    return;
  }
  hotkeyBound = true;
  document.addEventListener("keydown", onHotkey, true);
}

/** Принудительно закрыть окно (например, при размонтировании интерфейса). */
export function closeDiagnostics(): void {
  closeHandler?.();
}
