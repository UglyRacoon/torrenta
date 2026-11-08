import type { ToastKind } from "../api/types";
import { h } from "../lib/dom";
import { icon, type IconName } from "../lib/icons";

const ICONS: Record<ToastKind, IconName> = {
  success: "checkCircle",
  error: "alertCircle",
  warning: "alertCircle",
  info: "info",
};

const MAX_VISIBLE = 3;
const LIFETIME_MS = 4600;

export interface Toaster {
  push(kind: ToastKind, title: string, message?: string): void;
}

/** Стек уведомлений внизу справа. Живёт вне основного дерева, чтобы не влиять на раскладку. */
export function createToaster(mount: HTMLElement): Toaster {
  function dismiss(node: HTMLElement): void {
    if (!node.isConnected || node.dataset.leaving === "1") {
      return;
    }
    node.dataset.leaving = "1";
    node.classList.add("toast--out");
    const remove = () => node.remove();
    node.addEventListener("animationend", remove, { once: true });
    window.setTimeout(remove, 220);
  }

  return {
    push(kind, title, message) {
      while (mount.children.length >= MAX_VISIBLE) {
        const first = mount.firstElementChild;
        if (!(first instanceof HTMLElement)) {
          break;
        }
        dismiss(first);
        if (first.parentElement === mount) {
          first.remove();
        }
      }

      const close = h("button", {
        class: "toast__close",
        attrs: { type: "button", "aria-label": "Закрыть уведомление" },
      });
      close.append(icon("close", 13));

      const node = h(
        "div",
        { class: `toast toast--${kind}` },
        h("span", { class: "toast__icon" }, icon(ICONS[kind], 17)),
        h(
          "div",
          { class: "toast__body" },
          h("p", { class: "toast__title", text: title }),
          message ? h("p", { class: "toast__message", text: message }) : null,
        ),
        close,
      );

      close.addEventListener("click", () => dismiss(node));
      mount.append(node);
      window.setTimeout(() => dismiss(node), LIFETIME_MS);
    },
  };
}
