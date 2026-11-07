import type { SettingsDto } from "../api/types";
import { h, messageOf, setAttr, setText } from "../lib/dom";
import { icon } from "../lib/icons";

export interface AddHandlers {
  prefill?: string;
  onAddMagnet(link: string): Promise<void>;
  onFiles(files: File[]): Promise<void>;
  onPick(): Promise<void>;
}

export interface SettingsHandlers {
  onSave(settings: SettingsDto): Promise<void>;
  onBrowseDir(): Promise<string | null>;
}

export interface ModalHost {
  isOpen(): boolean;
  closeAll(): void;
  openAdd(handlers: AddHandlers): void;
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

  function openRoot(options: {
    title: string;
    className: string;
    footer?: boolean;
  }): ModalHandle {
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
    const modal: ModalHandle = {
      scrim,
      dialog,
      body,
      footer,
      close() {
        const index = stack.indexOf(entry);
        if (index < 0) {
          return;
        }
        stack.splice(index, 1);
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

  function numberInput(value: number, attrs: Record<string, string>): HTMLInputElement {
    return h("input", {
      class: "input num",
      attrs: { type: "number", inputmode: "numeric", value: String(value), ...attrs },
    });
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

  function openAdd(handlers: AddHandlers): void {
    closeAll();
    const modal = openRoot({ title: "Добавить торрент", className: "modal--add" });

    const dropzone = h("div", {
      class: "dropzone",
      attrs: { role: "button", tabindex: "0", "aria-label": "Выбрать .torrent файл" },
    });
    dropzone.append(
      h("span", { class: "dropzone__icon" }, icon("upload", 22)),
      h("p", { class: "dropzone__title", text: "Перетащите .torrent сюда" }),
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
    const errorEl = formError();
    modal.body.append(
      h(
        "div",
        { class: "add-form" },
        dropzone,
        h("div", { class: "modal__divider" }, h("span", { text: "или" })),
        field("Magnet-ссылка", textarea),
        errorEl,
      ),
    );

    const hint = h("p", {
      class: "modal__hint",
      text: "Ctrl+Enter — добавить · Esc — закрыть",
    });
    modal.body.append(hint);

    const cancel = button("Отмена", "btn--ghost");
    const submit = button("Добавить", "btn--primary");
    modal.footer.append(cancel, submit);

    let dragDepth = 0;
    const clearDrag = (): void => {
      dragDepth = 0;
      dropzone.classList.remove("is-over");
    };

    const pick = (): void => {
      run(modal, submit, errorEl, handlers.onPick);
    };

    function submitMagnet(): void {
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
      run(modal, submit, errorEl, () => handlers.onAddMagnet(value));
    }

    if (handlers.prefill) {
      textarea.value = handlers.prefill;
    }

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
    dropzone.addEventListener("dragenter", (event) => {
      event.preventDefault();
      dragDepth += 1;
      dropzone.classList.add("is-over");
    });
    dropzone.addEventListener("dragover", (event) => {
      event.preventDefault();
      if (event.dataTransfer) {
        event.dataTransfer.dropEffect = "copy";
      }
    });
    dropzone.addEventListener("dragleave", () => {
      dragDepth -= 1;
      if (dragDepth <= 0) {
        clearDrag();
      }
    });
    dropzone.addEventListener("drop", (event) => {
      event.preventDefault();
      clearDrag();
      const files = Array.from(event.dataTransfer?.files ?? []).filter((file) =>
        /\.torrent$/i.test(file.name),
      );
      if (files.length > 0) {
        run(modal, submit, errorEl, () => handlers.onFiles(files));
        return;
      }
      const text = event.dataTransfer?.getData("text/plain") ?? "";
      if (/^magnet:\?/i.test(text.trim())) {
        textarea.value = text.trim();
        submitMagnet();
        return;
      }
      showError(errorEl, "Нужен файл .torrent или magnet-ссылка");
    });

    textarea.addEventListener("input", () => showError(errorEl, null));
    textarea.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
        event.preventDefault();
        submitMagnet();
      }
    });
    cancel.addEventListener("click", () => {
      if (!(modal.dialog.classList.contains("is-busy"))) {
        modal.close();
      }
    });
    submit.addEventListener("click", submitMagnet);

    focusFirst(modal, "textarea");
  }

  function openSettings(settings: SettingsDto, handlers: SettingsHandlers): void {
    closeAll();
    const modal = openRoot({ title: "Настройки", className: "modal--settings" });

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

    const downLimit = numberInput(settings.download_limit_kib, { min: "0", step: "16" });
    const upLimit = numberInput(settings.upload_limit_kib, { min: "0", step: "16" });
    const port = numberInput(settings.listen_port, { min: "0", max: "65535", step: "1" });
    const maxPeers = numberInput(settings.max_peers, { min: "1", max: "200", step: "1" });

    const seedCheck = h("input", {
      class: "checkbox__input",
      attrs: { type: "checkbox" },
    });
    seedCheck.checked = settings.seed_after_complete;
    const seedRow = h(
      "label",
      { class: "checkbox" },
      seedCheck,
      h("span", { class: "checkbox__box" }, icon("check", 12)),
      h(
        "span",
        { class: "checkbox__text" },
        h("span", { class: "checkbox__title", text: "Раздавать после завершения" }),
        h("span", { class: "checkbox__hint", text: "Торрент продолжит отдавать данные после загрузки." }),
      ),
    );

    let theme: "dark" | "light" = settings.theme === "light" ? "light" : "dark";
    const themeButtons = new Map<"dark" | "light", HTMLButtonElement>();
    const themeGroup = h("div", { class: "segmented", attrs: { role: "group", "aria-label": "Тема оформления" } });
    for (const name of ["dark", "light"] as const) {
      const label = name === "dark" ? "Тёмная" : "Светлая";
      const themeButton = h("button", {
        class: "segmented__btn",
        text: label,
        attrs: { type: "button", "aria-pressed": theme === name ? "true" : "false" },
      });
      themeButton.addEventListener("click", () => {
        theme = name;
        for (const [key, node] of themeButtons) {
          setAttr(node, "aria-pressed", key === theme ? "true" : "false");
        }
      });
      themeButtons.set(name, themeButton);
      themeGroup.append(themeButton);
    }
    const themeField = h(
      "div",
      { class: "field" },
      h("span", { class: "field__label", text: "Тема оформления" }),
      themeGroup,
    );

    const errorEl = formError();
    modal.body.append(
      dirField,
      h(
        "div",
        { class: "field-row" },
        field("Ограничение приёма, КиБ/с", downLimit),
        field("Ограничение отдачи, КиБ/с", upLimit),
      ),
      h(
        "div",
        { class: "field-row" },
        field("Порт прослушивания", port),
        field("Пиров на торрент", maxPeers),
      ),
      seedRow,
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
      const down = readNumber(downLimit);
      const up = readNumber(upLimit);
      const portValue = readNumber(port);
      const peers = readNumber(maxPeers);
      if (!Number.isFinite(down) || down < 0 || down > 4_000_000) {
        return "Лимит приёма — число от 0 до 4 000 000 КиБ/с";
      }
      if (!Number.isFinite(up) || up < 0 || up > 4_000_000) {
        return "Лимит отдачи — число от 0 до 4 000 000 КиБ/с";
      }
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
        .then((selected) => {
          if (selected) {
            dirInput.value = selected;
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
        download_dir: dirInput.value.trim(),
        max_peers: Math.round(readNumber(maxPeers)),
        download_limit_kib: Math.round(readNumber(downLimit)),
        upload_limit_kib: Math.round(readNumber(upLimit)),
        listen_port: Math.round(readNumber(port)),
        seed_after_complete: seedCheck.checked,
        theme,
      };
      run(modal, save, errorEl, () => handlers.onSave(next));
    });

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
    const check = h("input", { class: "checkbox__input", attrs: { type: "checkbox" } });
    const checkbox = h(
      "label",
      { class: "checkbox" },
      check,
      h("span", { class: "checkbox__box" }, icon("check", 12)),
      h(
        "span",
        { class: "checkbox__text" },
        h("span", { class: "checkbox__title", text: "Удалить скачанные файлы" }),
        h("span", { class: "checkbox__hint", text: "Данные будут удалены с диска без возможности восстановления." }),
      ),
    );
    const errorEl = formError();
    modal.body.append(text, checkbox, errorEl);

    const cancel = button("Отмена", "btn--ghost");
    const remove = button("Удалить", "btn--danger");
    modal.footer.append(cancel, remove);

    cancel.addEventListener("click", () => {
      if (!(modal.dialog.classList.contains("is-busy"))) {
        modal.close();
      }
    });
    remove.addEventListener("click", () => {
      run(modal, remove, errorEl, () => onConfirm(check.checked));
    });

    focusFirst(modal, ".modal__foot button");
  }

  function closeAll(): void {
    while (stack.length > 0) {
      stack[stack.length - 1].handle.close();
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
