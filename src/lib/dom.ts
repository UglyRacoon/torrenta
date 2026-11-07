/** Небольшие помощники для ручного создания DOM без innerHTML на данных. */

export type Child = Node | string | number | null | undefined | false;

export interface ElementProps {
  class?: string;
  text?: string;
  hidden?: boolean;
  attrs?: Record<string, string | number | boolean | null | undefined>;
  dataset?: Record<string, string>;
  style?: Partial<CSSStyleDeclaration>;
}

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props?: ElementProps | null,
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (props) {
    if (props.class) {
      node.className = props.class;
    }
    if (props.text !== undefined) {
      node.textContent = props.text;
    }
    if (props.hidden) {
      node.hidden = true;
    }
    if (props.attrs) {
      for (const [name, value] of Object.entries(props.attrs)) {
        if (value === null || value === undefined || value === false) {
          continue;
        }
        node.setAttribute(name, value === true ? "" : String(value));
      }
    }
    if (props.dataset) {
      for (const [name, value] of Object.entries(props.dataset)) {
        node.dataset[name] = value;
      }
    }
    if (props.style) {
      Object.assign(node.style, props.style);
    }
  }
  append(node, children);
  return node;
}

export function append(parent: Node, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) {
      continue;
    }
    parent.appendChild(
      typeof child === "string" || typeof child === "number"
        ? document.createTextNode(String(child))
        : child,
    );
  }
}

/** Пишет текст только при реальном изменении — дешёвый дифф для тиков. */
export function setText(node: Element, value: string): void {
  if (node.textContent !== value) {
    node.textContent = value;
  }
}

export function setAttr(node: Element, name: string, value: string | null): void {
  if (value === null) {
    if (node.hasAttribute(name)) {
      node.removeAttribute(name);
    }
    return;
  }
  if (node.getAttribute(name) !== value) {
    node.setAttribute(name, value);
  }
}

export function toggleClass(node: Element, name: string, enabled: boolean): void {
  if (node.classList.contains(name) !== enabled) {
    node.classList.toggle(name, enabled);
  }
}

export function setScaleX(node: HTMLElement, value: number, last: { value: number }): void {
  const next = Math.round(Math.min(1, Math.max(0, value)) * 10000) / 10000;
  if (last.value !== next) {
    last.value = next;
    node.style.transform = `scaleX(${next})`;
  }
}

export function clear(node: Node): void {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

export function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) {
    return false;
  }
  if (target.isContentEditable) {
    return true;
  }
  return target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "SELECT";
}

export function prefersReducedMotion(): boolean {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export function messageOf(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  if (typeof error === "string") {
    return error;
  }
  return "Неизвестная ошибка";
}
