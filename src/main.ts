import "./styles/base.css";
import "./styles/layout.css";
import "./styles/list.css";
import "./styles/details.css";
import "./styles/overlays.css";

import { mountApp } from "./app.ts";

function renderFatal(reason: unknown): void {
  const message = reason instanceof Error ? reason.message : String(reason);
  const list = document.getElementById("list");
  const fallback = document.createElement("div");
  fallback.className = "empty";

  const title = document.createElement("h2");
  title.className = "empty__title";
  title.textContent = "Интерфейс не запустился";

  const text = document.createElement("p");
  text.className = "empty__text";
  text.textContent = message;

  const hint = document.createElement("p");
  hint.className = "empty__hint";
  hint.textContent = "Перезапустите Torrenta. Если ошибка повторяется — сообщите разработчику.";

  fallback.append(title, text, hint);
  list?.replaceChildren(fallback);
}

try {
  mountApp();
} catch (error) {
  console.error("[torrenta] не удалось смонтировать интерфейс", error);
  renderFatal(error);
}
