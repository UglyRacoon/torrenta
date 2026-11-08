/**
 * Иконки — инлайновые SVG в стиле lucide: 24×24, обводка 1.5 px,
 * цвет наследуется от текста. Никаких внешних шрифтов и CDN (CSP).
 */

const PATHS = {
  layers:
    '<path d="m12 4.2 8.2 4.1L12 12.4 3.8 8.3z"/><path d="m4.2 12.2 7.8 3.9 7.8-3.9"/><path d="m4.2 16 7.8 3.9 7.8-3.9"/>',
  download:
    '<path d="M12 4.2v10.9"/><path d="m7.6 10.7 4.4 4.4 4.4-4.4"/><path d="M5 19.8h14"/>',
  upload:
    '<path d="M12 15.8V4.9"/><path d="m7.6 9.3 4.4-4.4 4.4 4.4"/><path d="M5 19.8h14"/>',
  pause: '<path d="M9.4 5.5v13" stroke-width="2"/><path d="M14.6 5.5v13" stroke-width="2"/>',
  play: '<path d="M8.6 5.7v12.6L18.4 12z" fill="currentColor" stroke="none"/>',
  check: '<path d="m5.4 12.6 4.3 4.3 8.9-9.2"/>',
  checkCircle: '<circle cx="12" cy="12" r="8.2"/><path d="m8.4 12.3 2.5 2.5 4.8-5"/>',
  alert:
    '<path d="M12 4.7 3.9 19.1h16.2z"/><path d="M12 9.9v4.3"/><circle cx="12" cy="17" r=".95" fill="currentColor" stroke="none"/>',
  alertCircle:
    '<circle cx="12" cy="12" r="8.2"/><path d="M12 7.7v5.1"/><circle cx="12" cy="16.3" r=".95" fill="currentColor" stroke="none"/>',
  clock: '<circle cx="12" cy="12" r="8.2"/><path d="M12 7.7V12l3 1.9"/>',
  magnet:
    '<path d="M6.4 4.6v7.1a5.6 5.6 0 0 0 11.2 0V4.6h-3.7v7.1a1.9 1.9 0 0 1-3.8 0V4.6z"/><path d="M6.4 8.7h3.7M13.9 8.7h3.7"/>',
  file: '<path d="M7 3.9h6.1L17.9 8.7v11.4H7z"/><path d="M13.1 3.9v4.8h4.8"/>',
  folder:
    '<path d="M3.9 7.5a1.6 1.6 0 0 1 1.6-1.6h3.8l2 2h7.2a1.6 1.6 0 0 1 1.6 1.6v8a1.6 1.6 0 0 1-1.6 1.6H5.5a1.6 1.6 0 0 1-1.6-1.6z"/>',
  folderOpen:
    '<path d="M3.9 8.2V6.6A1.6 1.6 0 0 1 5.5 5h3.8l2 2h6.6a1.6 1.6 0 0 1 1.6 1.6v1.1"/><path d="M4.5 9.7h15.9l-1.9 8.1a1.6 1.6 0 0 1-1.6 1.2H6a1.6 1.6 0 0 1-1.6-1.2z"/>',
  search: '<circle cx="11" cy="11" r="6.2"/><path d="m15.6 15.6 4 4"/>',
  chevronDown: '<path d="m6.5 9.5 5.5 5.5 5.5-5.5"/>',
  chevronRight: '<path d="m9.5 6.5 5.5 5.5-5.5 5.5"/>',
  close: '<path d="M6.5 6.5 17.5 17.5M17.5 6.5 6.5 17.5"/>',
  settings:
    '<path d="M4 7.4h9.6M18.4 7.4H20M4 16.6h2.2M10.8 16.6H20"/><circle cx="16" cy="7.4" r="2.2"/><circle cx="8.4" cy="16.6" r="2.2"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 3.4v2M12 18.6v2M3.4 12h2M18.6 12h2M5.9 5.9l1.4 1.4M16.7 16.7l1.4 1.4M18.1 5.9l-1.4 1.4M7.3 16.7l-1.4 1.4"/>',
  moon: '<path d="M19.1 14.1A7.6 7.6 0 0 1 9.9 4.9a7.6 7.6 0 1 0 9.2 9.2z"/>',
  plus: '<path d="M12 5.2v13.6M5.2 12h13.6"/>',
  trash:
    '<path d="M4.6 6.9h14.8"/><path d="M9.2 6.9V4.8h5.6v2.1"/><path d="m6.6 6.9.8 12.3h9.2l.8-12.3"/><path d="M10.1 10.4v5.6M13.9 10.4v5.6"/>',
  external:
    '<path d="M13.4 4.9H19v5.6"/><path d="M19 4.9 11.4 12.5"/><path d="M16.9 13.4v4.7a1.5 1.5 0 0 1-1.5 1.5H5.6a1.5 1.5 0 0 1-1.5-1.5V8.3a1.5 1.5 0 0 1 1.5-1.5h4.7"/>',
  more: '<circle cx="12" cy="5.6" r="1.35" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.35" fill="currentColor" stroke="none"/><circle cx="12" cy="18.4" r="1.35" fill="currentColor" stroke="none"/>',
  hash: '<path d="m9.8 4 3.4 16M14.2 4l-3.4 16M4.6 9.2h14.8M3.8 14.8h14.8"/>',
  users:
    '<circle cx="9" cy="8.6" r="3.2"/><path d="M3.8 18.9c.5-3.1 2.6-4.9 5.2-4.9s4.7 1.8 5.2 4.9"/><path d="M15.1 5.9a3 3 0 0 1 0 5.5"/><path d="M17.2 14.4c1.9.6 3.1 2.2 3.4 4.5"/>',
  link: '<path d="M10.2 13.8a3.3 3.3 0 0 0 4.7 0l2.6-2.6a3.3 3.3 0 0 0-4.7-4.7l-1.4 1.4"/><path d="M13.8 10.2a3.3 3.3 0 0 0-4.7 0l-2.6 2.6a3.3 3.3 0 0 0 4.7 4.7l1.4-1.4"/>',
  info: '<circle cx="12" cy="12" r="8.2"/><path d="M12 11v5.2"/><circle cx="12" cy="7.9" r="1" fill="currentColor" stroke="none"/>',
  refresh:
    '<path d="M19.6 12a7.6 7.6 0 1 1-2.2-5.4"/><path d="M19.6 4.7v4.7h-4.7"/>',
  grid: '<rect x="4.2" y="4.2" width="6.2" height="6.2" rx="1.3"/><rect x="13.6" y="4.2" width="6.2" height="6.2" rx="1.3"/><rect x="4.2" y="13.6" width="6.2" height="6.2" rx="1.3"/><rect x="13.6" y="13.6" width="6.2" height="6.2" rx="1.3"/>',
  clipboard:
    '<path d="M9.2 5.2h5.6v2.2H9.2z"/><path d="M7.2 6.6H5.9v12.3h12.2V6.6h-1.3"/><path d="M9.6 11.4h4.8M9.6 14.4h3.4"/>',
  seed: '<path d="M12 20V9.8"/><path d="M12 9.8c0-2.9 2.1-4.8 5.8-4.8 0 3.7-2.1 5.6-5.8 5.6z"/><path d="M12 13.8c0-2.3-1.7-3.8-4.8-3.8 0 2.9 1.7 4.4 4.8 4.4z"/>',
  ratio:
    '<path d="M6.2 8h10.4"/><path d="m13.7 5 3 3-3 3"/><path d="M17.8 16H7.4"/><path d="m10.3 13-3 3 3 3"/>',
  arrowDown: '<path d="M12 4.6v14.2"/><path d="m6.8 13.6 5.2 5.2 5.2-5.2"/>',
  arrowUp: '<path d="M12 19.4V5.2"/><path d="m6.8 10.4 5.2-5.2 5.2 5.2"/>',
  arrowRight: '<path d="M4.6 12h14.2"/><path d="m13.6 6.8 5.2 5.2-5.2 5.2"/>',
  chevronLeft: '<path d="m14.5 6.5-5.5 5.5 5.5 5.5"/>',
  power: '<path d="M12 4.4v7.2"/><path d="M7.4 7a7.2 7.2 0 1 0 9.2 0"/>',
  grip: '<circle cx="9.4" cy="7.4" r="1.15" fill="currentColor" stroke="none"/><circle cx="14.6" cy="7.4" r="1.15" fill="currentColor" stroke="none"/><circle cx="9.4" cy="12" r="1.15" fill="currentColor" stroke="none"/><circle cx="14.6" cy="12" r="1.15" fill="currentColor" stroke="none"/><circle cx="9.4" cy="16.6" r="1.15" fill="currentColor" stroke="none"/><circle cx="14.6" cy="16.6" r="1.15" fill="currentColor" stroke="none"/>',
} as const;

export type IconName = keyof typeof PATHS;

function svgRoot(size: number): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.5");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  return svg;
}

export function icon(name: IconName, size = 18): SVGSVGElement {
  const svg = svgRoot(size);
  svg.innerHTML = PATHS[name];
  return svg;
}

/** Логотип: стрелка загрузки в чаше-кольце. Рисуется отдельно — у него своя палитра. */
export function brandMark(size = 25): SVGSVGElement {
  const svg = svgRoot(size);
  svg.innerHTML =
    '<circle cx="12" cy="12" r="8.6" stroke="currentColor" stroke-opacity=".28" stroke-width="1.4"/>' +
    '<path d="M12 5.5v9.2" stroke="currentColor" stroke-width="1.9"/>' +
    '<path d="m8.5 11.4 3.5 3.5 3.5-3.5" stroke="currentColor" stroke-width="1.9"/>' +
    '<path d="M6.9 17.5c1.4 1 3.2 1.5 5.1 1.5s3.7-.5 5.1-1.5" stroke="currentColor" stroke-width="1.4" stroke-opacity=".65"/>';
  return svg;
}
