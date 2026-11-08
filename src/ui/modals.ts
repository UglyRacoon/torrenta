import type { AddOptions, SettingsDto, TorrentPreview, TorrentSource } from "../api/types";
import { SPEED_STEPS } from "../api/types";
import { fmtBytes, basename } from "../lib/format";
import { h, messageOf, setAttr, setText } from "../lib/dom";
import { icon } from "../lib/icons";
import { createSelect, type SelectOption } from "./select";

export interface AddRequest {
  /** Готовый источник (файл/магнит); `null` — диалог начинается с выбора источника. */
  source: TorrentSource | null;
  /** Каталог загрузки по умолчанию (из настроек). */
  savePath: string;
}

export interface AddHandlers {
  /** Попросить движок разобрать источник: имя, размер, хеш, список файлов. */
  onPreview(source: TorrentSource): Promise<TorrentPreview>;
  /** Добавить торрент. `files === null` — скачать целиком. */
  onAdd(source: TorrentSource, options: AddOptions): Promise<void>;
  /** Диалог выбора .torrent на диске. */
  onPickSource(): Promise<TorrentSource | null>;
  /** Диалог выбора каталога загрузки. */
  onBrowseDir(): Promise<string | null>;
}

export interface SettingsHandlers {
  onSave(settings: SettingsDto): Promise<void>;
  onBrowseDir(): Promise<string | null>;
}

export interface ModalHost {
  isOpen(): boolean;
  closeAll(): void;
  openAdd(request: AddRequest, handlers: AddHandlers): void;
  openSettings(settings: SettingsDto, handlers: SettingsHandlers): void;
  confirmRemove(name: string, onConfirm: (deleteFiles: boolean) => Promise<void>): void;
}

interface ModalHandle {
  scrim: HTMLElement;
  dialog: HTMLElement;
  body: HTMLElement;
  footer: HTMLElement;
  close(): void;
  setBusy(busy: boolean): void;
  /** Очистка внешних виджетов (например, всплывающих списков) при закрытии. */
  onClose(cleanup: () => void): void;
}

interface StackEntry {
  handle: ModalHandle;
  restore: HTMLElement | null;
  busy: boolean;
}

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusables(dialog: HTMLElement): HTMLElement[] {
  return Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (node) => !node.hidden && node.offsetParent !== null,
  );
}

/** 1 файл / 2 файла / 5 файлов. */
function filesWord(count: number): string {
  const tail = count % 100;
  if (tail >= 11 && tail <= 14) {
    return "файлов";
  }
  switch (count % 10) {
    case 1:
      return "файл";
    case 2:
    case 3:
    case 4:
      return "файла";
    default:
      return "файлов";
  }
}

/** Подпись источника, когда превью получить не удалось. */
function sourceLabel(source: TorrentSource): string {
  if (source.kind === "path") {
    return basename(source.path);
  }
  const match = /[?&]dn=([^&]+)/i.exec(source.link);
  if (match?.[1]) {
    try {
      return decodeURIComponent(match[1].replace(/\+/g, " "));
    } catch {
      return match[1];
    }
  }
  return "Magnet-ссылка";
}

/** Пресеты лимита скорости + сохранённое значение, если оно не из пресетов. */
function limitOptions(current: number): SelectOption[] {
  const presets: SelectOption[] = SPEED_STEPS.map((kib) => ({
    value: String(kib),
    label: kib === 0 ? "Без ограничения" : `${kib} КиБ/с`,
  }));
  if (!SPEED_STEPS.includes(current)) {
    presets.push({ value: String(current), label: `${current} КиБ/с` });
  }
  return presets;
}

export function createModalHost(root: HTMLElement): ModalHost {
  const stack: StackEntry[] = [];

  document.addEventListener("keydown", (event) => {
    const top = stack[stack.length - 1];
    if (!top) {
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      if (!top.busy) {
        top.handle.close();
      }
      return;
    }
    if (event.key !== "Tab") {
      return;
    }
    const nodes = focusables(top.handle.dialog);
    if (nodes.length === 0) {
      event.preventDefault();
      return;
    }
    const first = nodes[0];
    const last = nodes[nodes.length - 1];
    const active = document.activeElement;
    if (!top.handle.dialog.contains(active)) {
      event.preventDefault();
      first.focus();
      return;
    }
    if (event.shiftKey && active === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  });

  function openRoot(options: { title: string; className: string }): ModalHandle {
    const headingId = `modal-title-${Math.random().toString(36).slice(2, 8)}`;
    const title = h("h2", { class: "modal__title", text: options.title, attrs: { id: headingId } });
    const closeButton = h("button", {
      class: "icon-btn icon-btn--sm modal__close",
      attrs: { type: "button", "aria-label": "Закрыть диалог" },
    });
    closeButton.append(icon("close", 16));
    const body = h("div", { class: "modal__body" });
    const footer = h("footer", { class: "modal__foot" });
    const dialog = h(
      "div",
      {
        class: `modal ${options.className}`,
        attrs: {
          role: "dialog",
          "aria-modal": "true",
          "aria-labelledby": headingId,
          tabindex: "-1",
        },
      },
      h("header", { class: "modal__head" }, title, closeButton),
      body,
      footer,
    );
    const scrim = h("div", { class: "modal-scrim" }, dialog);

    const entry: StackEntry = { handle: null as unknown as ModalHandle, restore: null, busy: false };
    const cleanups: (() => void)[] = [];
    let closed = false;
    const modal: ModalHandle = {
      scrim,
      dialog,
      body,
      footer,
      close() {
        const index = stack.indexOf(entry);
        if (index < 0 || closed) {
          return;
        }
        closed = true;
        stack.splice(index, 1);
        for (const cleanup of cleanups) {
          cleanup();
        }
        scrim.remove();
        if (stack.length === 0) {
          document.documentElement.classList.remove("is-locked");
        }
        const restore = entry.restore;
        if (restore && restore.isConnected) {
          restore.focus();
        }
      },
      setBusy(busy: boolean) {
        entry.busy = busy;
        dialog.classList.toggle("is-busy", busy);
        setAttr(closeButton, "aria-disabled", busy ? "true" : "false");
        closeButton.disabled = busy;
      },
      onClose(cleanup: () => void) {
        cleanups.push(cleanup);
      },
    };
    entry.handle = modal;
    entry.restore = document.activeElement instanceof HTMLElement ? document.activeElement : null;

    closeButton.addEventListener("click", () => {
      if (!entry.busy) {
        modal.close();
      }
    });
    scrim.addEventListener("mousedown", (event) => {
      if (event.target === scrim && !entry.busy) {
        modal.close();
      }
    });

    root.append(scrim);
    stack.push(entry);
    document.documentElement.classList.add("is-locked");
    dialog.focus();
    return modal;
  }

  function focusFirst(modal: ModalHandle, selector?: string): void {
    const target = selector ? modal.dialog.querySelector<HTMLElement>(selector) : null;
    const node = target ?? focusables(modal.dialog)[0] ?? modal.dialog;
    node.focus();
    if (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement) {
      node.select();
    }
  }

  function button(text: string, className: string, type: "button" | "submit" = "button"): HTMLButtonElement {
    return h("button", { class: `btn ${className}`, text, attrs: { type } });
  }

  function field(label: string, control: HTMLElement): HTMLElement {
    return h("label", { class: "field" }, h("span", { class: "field__label", text: label }), control);
  }

  function fieldBlock(label: string, control: HTMLElement): HTMLElement {
    return h("div", { class: "field" }, h("span", { class: "field__label", text: label }), control);
  }

  function numberInput(value: number, attrs: Record<string, string>): HTMLInputElement {
    return h("input", {
      class: "input num",
      attrs: { type: "number", inputmode: "numeric", value: String(value), ...attrs },
    });
  }

  function checkboxRow(titleText: string, hintText: string, checked: boolean): {
    row: HTMLElement;
    input: HTMLInputElement;
  } {
    const input = h("input", { class: "checkbox__input", attrs: { type: "checkbox" } });
    input.checked = checked;
    const row = h(
      "label",
      { class: "checkbox" },
      input,
      h("span", { class: "checkbox__box" }, icon("check", 12)),
      h(
        "span",
        { class: "checkbox__text" },
        h("span", { class: "checkbox__title", text: titleText }),
        h("span", { class: "checkbox__hint", text: hintText }),
      ),
    );
    return { row, input };
  }

  function formError(): HTMLElement {
    return h("p", { class: "form-error", attrs: { role: "alert", hidden: "" } });
  }

  function showError(node: HTMLElement, message: string | null): void {
    node.hidden = message === null;
    setText(node, message ?? "");
  }

  function run(
    modal: ModalHandle,
    buttonEl: HTMLButtonElement,
    errorEl: HTMLElement,
    action: () => Promise<void>,
  ): void {
    showError(errorEl, null);
    modal.setBusy(true);
    buttonEl.disabled = true;
    void action()
      .then(() => modal.close())
      .catch((reason: unknown) => {
        showError(errorEl, messageOf(reason));
        modal.setBusy(false);
      })
      .finally(() => {
        buttonEl.disabled = false;
      });
  }

  function openAdd(request: AddRequest, handlers: AddHandlers): void {
    closeAll();
    const modal = openRoot({ title: "Добавить торрент", className: "modal--add" });

    let source = request.source;
    let preview: TorrentPreview | null = null;
    let folder = request.savePath;
    let token = 0;
    let submitBtn: HTMLButtonElement | null = null;
    const selected = new Set<number>();

    const errorEl = formError();

    // --- Каталог загрузки: общий узел для превью и запасного режима ---
    const dirInput = h("input", {
      class: "input",
      attrs: { type: "text", value: folder, spellcheck: "false", autocomplete: "off" },
    });
    dirInput.addEventListener("input", () => {
      folder = dirInput.value;
      showError(errorEl, null);
    });
    const browseBtn = button("Обзор…", "btn--ghost field__button");
    browseBtn.addEventListener("click", () => {
      dirInput.disabled = true;
      void handlers
        .onBrowseDir()
        .then((picked) => {
          if (picked) {
            folder = picked;
            dirInput.value = picked;
          }
        })
        .catch((reason: unknown) => showError(errorEl, messageOf(reason)))
        .finally(() => {
          dirInput.disabled = false;
        });
    });
    const dirRow = h("div", { class: "field__row" }, dirInput, browseBtn);
    const folderField = (): HTMLElement =>
      h(
        "div",
        { class: "field" },
        h("span", { class: "field__label", text: "Папка загрузки" }),
        dirRow,
      );

    function setFooter(...nodes: HTMLElement[]): void {
      modal.footer.replaceChildren(...nodes);
    }

    function cancelButton(): HTMLButtonElement {
      const cancel = button("Отмена", "btn--ghost");
      cancel.addEventListener("click", () => {
        if (!modal.dialog.classList.contains("is-busy")) {
          modal.close();
        }
      });
      return cancel;
    }

    function fit(): void {
      showError(errorEl, null);
      modal.body.scrollTop = 0;
    }

    // --- Состояние 1: выбор источника (диалог открыт кнопкой «Добавить») ---
    function renderChooser(): void {
      preview = null;
      submitBtn = null;
      fit();
      const dropzone = h("div", {
        class: "dropzone",
        attrs: { role: "button", tabindex: "0", "aria-label": "Выбрать .torrent файл" },
      });
      dropzone.append(
        h("span", { class: "dropzone__icon" }, icon("upload", 22)),
        h("p", { class: "dropzone__title", text: "Перетащите .torrent в окно" }),
        h("p", { class: "dropzone__hint", text: "или выберите файл на диске" }),
      );
      const pickButton = button("Выбрать файл…", "btn--ghost dropzone__button");
      dropzone.append(pickButton);

      const textarea = h("textarea", {
        class: "input input--area",
        attrs: {
          rows: "2",
          placeholder: "magnet:?xt=urn:btih:…",
          "aria-label": "Magnet-ссылка",
          spellcheck: "false",
          autocomplete: "off",
        },
      });

      const pick = (): void => {
        void handlers
          .onPickSource()
          .then((picked) => {
            if (picked) {
              resolve(picked);
            }
          })
          .catch((reason: unknown) => showError(errorEl, messageOf(reason)));
      };

      const submitMagnet = (): void => {
        const value = textarea.value.trim();
        if (value === "") {
          showError(errorEl, "Вставьте magnet-ссылку или выберите файл");
          textarea.focus();
          return;
        }
        if (!/^magnet:\?/i.test(value)) {
          showError(errorEl, "Ссылка должна начинаться с magnet:?");
          textarea.focus();
          return;
        }
        resolve({ kind: "magnet", link: value });
      };

      dropzone.addEventListener("click", (event) => {
        if (event.target instanceof HTMLElement && event.target.closest("button")) {
          return;
        }
        pick();
      });
      pickButton.addEventListener("click", (event) => {
        event.stopPropagation();
        pick();
      });
      dropzone.addEventListener("keydown", (event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          pick();
        }
      });
      textarea.addEventListener("input", () => showError(errorEl, null));
      textarea.addEventListener("keydown", (event) => {
        if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
          event.preventDefault();
          submitMagnet();
        }
      });

      const magnetButton = button("Получить превью", "btn--primary");
      magnetButton.addEventListener("click", submitMagnet);

      modal.body.replaceChildren(
        h(
          "div",
          { class: "add-form" },
          dropzone,
          h("div", { class: "modal__divider" }, h("span", { text: "или" })),
          field("Magnet-ссылка", textarea),
        ),
        magnetButton,
        errorEl,
        h("p", { class: "modal__hint", text: "Esc — закрыть диалог" }),
      );
      setFooter(cancelButton());
      focusFirst(modal, "textarea");
    }

    // --- Состояние 2: получение метаданных ---
    function renderLoading(): void {
      fit();
      const hint =
        source?.kind === "magnet"
          ? "Ожидание метаданных от пиров может занять до 25 секунд."
          : "Читаем .torrent-файл.";
      modal.body.replaceChildren(
        h(
          "div",
          { class: "preview-loading", attrs: { role: "status", "aria-live": "polite" } },
          h("span", { class: "spinner", attrs: { "aria-hidden": "true" } }),
          h("p", { class: "preview-loading__title", text: "Получаем список файлов…" }),
          h("p", { class: "preview-loading__hint", text: hint }),
          source ? h("p", { class: "preview-loading__source", text: sourceLabel(source) }) : null,
        ),
        errorEl,
      );
      setFooter(cancelButton());
      submitBtn = null;
    }

    // --- Состояние 3: превью с выбором файлов ---
    function renderPreview(): void {
      if (!preview) {
        return;
      }
      fit();
      selected.clear();
      for (const file of preview.files) {
        selected.add(file.index);
      }

      const summary = h("p", { class: "preview__summary num" });
      const download = button("Скачать", "btn--primary");
      submitBtn = download;
      download.addEventListener("click", () => {
        downloadSelected(false);
      });

      const list = h("div", { class: "preview__files", attrs: { role: "group", "aria-label": "Файлы торрента" } });
      const checks = new Map<number, HTMLInputElement>();

      function refresh(): void {
        const total = preview?.total_bytes ?? 0;
        let count = 0;
        let bytes = 0;
        for (const file of preview?.files ?? []) {
          if (selected.has(file.index)) {
            count += 1;
            bytes += file.length;
          }
        }
        setText(
          summary,
          `Выбрано ${count} ${filesWord(count)} · ${fmtBytes(bytes)} из ${fmtBytes(total)}`,
        );
        download.disabled = count === 0 || total === 0;
        showError(errorEl, null);
      }

      for (const file of preview.files) {
        const input = h("input", {
          class: "checkbox__input",
          attrs: { type: "checkbox", "aria-label": file.path },
        });
        input.checked = selected.has(file.index);
        checks.set(file.index, input);
        input.addEventListener("change", () => {
          if (input.checked) {
            selected.add(file.index);
          } else {
            selected.delete(file.index);
          }
          refresh();
        });
        const row = h(
          "label",
          { class: "file-check", attrs: { title: file.path } },
          input,
          h("span", { class: "checkbox__box" }, icon("check", 12)),
          h("span", { class: "file-check__name", text: file.path }),
          h("span", { class: "file-check__size num", text: fmtBytes(file.length) }),
        );
        list.append(row);
      }

      const selectAll = button("Выбрать все", "btn--ghost btn--xs");
      const selectNone = button("Снять все", "btn--ghost btn--xs");
      selectAll.addEventListener("click", () => {
        for (const file of preview?.files ?? []) {
          selected.add(file.index);
          const input = checks.get(file.index);
          if (input) {
            input.checked = true;
          }
        }
        refresh();
      });
      selectNone.addEventListener("click", () => {
        selected.clear();
        for (const input of checks.values()) {
          input.checked = false;
        }
        refresh();
      });

      const head = h(
        "div",
        { class: "preview__head" },
        h("h3", { class: "preview__name", text: preview.name, attrs: { title: preview.name } }),
        h(
          "p",
          { class: "preview__facts num" },
          h("span", { text: fmtBytes(preview.total_bytes) }),
          h("span", { class: "preview__dot", attrs: { "aria-hidden": "true" }, text: "·" }),
          h("span", {
            text: `${preview.files.length} ${filesWord(preview.files.length)}`,
          }),
          h("span", { class: "preview__dot", attrs: { "aria-hidden": "true" }, text: "·" }),
          h(
            "span",
            {
              class: "preview__hash",
              text: `#${preview.info_hash.slice(0, 12)}`,
              attrs: { title: `info hash: ${preview.info_hash}` },
            },
          ),
        ),
      );

      const filesHead = h(
        "div",
        { class: "preview__files-head" },
        h("span", { class: "preview__files-title", text: "Файлы" }),
        h("span", { class: "preview__bulk" }, selectAll, selectNone),
      );

      modal.body.replaceChildren(
        h(
          "div",
          { class: "preview" },
          head,
          folderField(),
          filesHead,
          list,
          summary,
        ),
        errorEl,
      );

      setFooter(cancelButton(), download);
      refresh();
      download.focus();
    }

    // --- Состояние 4: метаданные не получены, но скачать целиком можно ---
    function renderFallback(reason: string): void {
      fit();
      submitBtn = null;
      const download = button("Скачать всё равно", "btn--primary");
      const retry = button("Повторить", "btn--ghost");
      download.addEventListener("click", () => {
        downloadSelected(true);
      });
      retry.addEventListener("click", () => {
        if (source) {
          resolve(source);
        }
      });
      modal.body.replaceChildren(
        h(
          "div",
          { class: "preview preview--fallback" },
          h("span", { class: "preview__warn" }, icon("alert", 16), h("span", { text: reason })),
          source
            ? h("h3", {
                class: "preview__name",
                text: sourceLabel(source),
                attrs: { title: sourceLabel(source) },
              })
            : null,
          h("p", {
            class: "preview__note",
            text: "Список файлов появится позже — торрент начнёт загрузку целиком.",
          }),
          folderField(),
        ),
        errorEl,
      );
      setFooter(cancelButton(), retry, download);
      submitBtn = download;
      download.focus();
    }

    function resolve(next: TorrentSource): void {
      source = next;
      token += 1;
      const my = token;
      preview = null;
      showError(errorEl, null);
      renderLoading();
      void handlers
        .onPreview(next)
        .then((result) => {
          if (my !== token) {
            return;
          }
          preview = result;
          renderPreview();
        })
        .catch((reason: unknown) => {
          if (my !== token) {
            return;
          }
          preview = null;
          renderFallback(messageOf(reason));
        });
    }

    function downloadSelected(all: boolean): void {
      if (!source) {
        return;
      }
      let files: number[] | null = null;
      if (!all) {
        if (selected.size === 0) {
          showError(errorEl, "Выберите хотя бы один файл");
          return;
        }
        files = Array.from(selected).sort((left, right) => left - right);
      }
      const trimmed = folder.trim();
      const options: AddOptions = { savePath: trimmed === "" ? null : trimmed, files };
      const submit = submitBtn;
      if (!submit) {
        return;
      }
      run(modal, submit, errorEl, () => handlers.onAdd(source as TorrentSource, options));
    }

    // Enter — скачать (контракт 4.1), когда фокус не в поле ввода.
    modal.dialog.addEventListener("keydown", (event) => {
      if (event.key !== "Enter") {
        return;
      }
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        submitBtn?.click();
        return;
      }
      const target = event.target;
      if (
        target instanceof HTMLElement &&
        target.closest("textarea, input, select, a, .select")
      ) {
        return;
      }
      if (submitBtn && !submitBtn.disabled) {
        event.preventDefault();
        submitBtn.click();
      }
    });

    if (source) {
      resolve(source);
    } else {
      renderChooser();
    }
  }

  function openSettings(settings: SettingsDto, handlers: SettingsHandlers): void {
    closeAll();
    const modal = openRoot({ title: "Настройки", className: "modal--settings" });
    const selects: { destroy(): void }[] = [];

    const dirInput = h("input", {
      class: "input",
      attrs: { type: "text", value: settings.download_dir, spellcheck: "false", autocomplete: "off" },
    });
    const browse = button("Обзор…", "btn--ghost field__button");
    const dirRow = h("div", { class: "field__row" }, dirInput, browse);
    const dirField = h(
      "div",
      { class: "field" },
      h("span", { class: "field__label", text: "Каталог загрузки" }),
      dirRow,
    );

    let downLimit = settings.download_limit_kib;
    let upLimit = settings.upload_limit_kib;
    const downSelect = createSelect({
      label: "Ограничение приёма",
      options: limitOptions(downLimit),
      value: String(downLimit),
      onChange: (value) => {
        downLimit = Number(value);
        showError(errorEl, null);
      },
    });
    const upSelect = createSelect({
      label: "Ограничение отдачи",
      options: limitOptions(upLimit),
      value: String(upLimit),
      onChange: (value) => {
        upLimit = Number(value);
        showError(errorEl, null);
      },
    });
    selects.push(downSelect, upSelect);

    const port = numberInput(settings.listen_port, { min: "0", max: "65535", step: "1" });
    const maxPeers = numberInput(settings.max_peers, { min: "1", max: "200", step: "1" });

    const seed = checkboxRow(
      "Раздавать после завершения",
      "Торрент продолжит отдавать данные после загрузки.",
      settings.seed_after_complete,
    );

    let theme: "dark" | "light" = settings.theme === "light" ? "light" : "dark";
    const themeSelect = createSelect({
      label: "Тема оформления",
      options: [
        { value: "dark", label: "Тёмная" },
        { value: "light", label: "Светлая" },
      ],
      value: theme,
      onChange: (value) => {
        theme = value === "light" ? "light" : "dark";
        showError(errorEl, null);
      },
    });
    selects.push(themeSelect);
    const themeField = fieldBlock("Тема оформления", themeSelect.element);

    const errorEl = formError();
    modal.body.append(
      dirField,
      h(
        "div",
        { class: "field-row" },
        fieldBlock("Ограничение приёма", downSelect.element),
        fieldBlock("Ограничение отдачи", upSelect.element),
      ),
      h(
        "div",
        { class: "field-row" },
        field("Порт прослушивания", port),
        field("Пиров на торрент", maxPeers),
      ),
      seed.row,
      themeField,
      errorEl,
      h("p", { class: "modal__hint", text: "Порт 0 — выбрать свободный автоматически. Лимит 0 — без ограничения." }),
    );

    const cancel = button("Отмена", "btn--ghost");
    const save = button("Сохранить", "btn--primary");
    modal.footer.append(cancel, save);

    const readNumber = (input: HTMLInputElement): number => {
      const raw = input.value.trim();
      return raw === "" ? Number.NaN : Number(raw);
    };

    function validate(): string | null {
      if (dirInput.value.trim() === "") {
        return "Укажите каталог загрузки";
      }
      const portValue = readNumber(port);
      const peers = readNumber(maxPeers);
      if (!Number.isFinite(portValue) || !Number.isInteger(portValue) || portValue < 0 || portValue > 65535) {
        return "Порт — целое число от 0 до 65535";
      }
      if (!Number.isFinite(peers) || !Number.isInteger(peers) || peers < 1 || peers > 500) {
        return "Пиров на торрент — целое число от 1 до 500";
      }
      return null;
    }

    browse.addEventListener("click", () => {
      dirInput.disabled = true;
      void handlers
        .onBrowseDir()
        .then((picked) => {
          if (picked) {
            dirInput.value = picked;
          }
        })
        .catch((reason: unknown) => showError(errorEl, messageOf(reason)))
        .finally(() => {
          dirInput.disabled = false;
        });
    });

    cancel.addEventListener("click", () => {
      if (!(modal.dialog.classList.contains("is-busy"))) {
        modal.close();
      }
    });
    save.addEventListener("click", () => {
      const problem = validate();
      if (problem) {
        showError(errorEl, problem);
        return;
      }
      const next: SettingsDto = {
        ...settings,
        download_dir: dirInput.value.trim(),
        max_peers: Math.round(readNumber(maxPeers)),
        download_limit_kib: downLimit,
        upload_limit_kib: upLimit,
        listen_port: Math.round(readNumber(port)),
        seed_after_complete: seed.input.checked,
        theme,
        auto_shutdown: settings.auto_shutdown,
      };
      run(modal, save, errorEl, () => handlers.onSave(next));
    });

    const destroySelects = (): void => {
      for (const select of selects) {
        select.destroy();
      }
    };
    modal.onClose(destroySelects);

    focusFirst(modal, "input[type=text]");
  }

  function confirmRemove(name: string, onConfirm: (deleteFiles: boolean) => Promise<void>): void {
    closeAll();
    const modal = openRoot({ title: "Удалить торрент?", className: "modal--confirm" });
    const text = h("p", { class: "modal__text" });
    text.append(
      document.createTextNode("Торрент «"),
      h("span", { class: "modal__text-name", text: name }),
      document.createTextNode("» будет удалён из списка."),
    );
    const files = checkboxRow(
      "Удалить скачанные файлы",
      "Данные будут удалены с диска без возможности восстановления.",
      false,
    );
    const errorEl = formError();
    modal.body.append(text, files.row, errorEl);

    const cancel = button("Отмена", "btn--ghost");
    const remove = button("Удалить", "btn--danger");
    modal.footer.append(cancel, remove);

    cancel.addEventListener("click", () => {
      if (!(modal.dialog.classList.contains("is-busy"))) {
        modal.close();
      }
    });
    remove.addEventListener("click", () => {
      run(modal, remove, errorEl, () => onConfirm(files.input.checked));
    });

    focusFirst(modal, ".modal__foot button");
  }

  function closeAll(): void {
    while (stack.length > 0) {
      const top = stack[stack.length - 1];
      if (top) {
        top.handle.close();
      }
    }
  }

  return {
    isOpen() {
      return stack.length > 0;
    },
    closeAll,
    openAdd,
    openSettings,
    confirmRemove,
  };
}
