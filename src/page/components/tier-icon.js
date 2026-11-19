import { html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';

// 订阅等级标识(in_better_ai_subscription):true = 仅 Pro Plus 可用(星标),false = Pro 与 Pro Plus 均可使用(对勾)。
// 模型面板与 API Key 面板的模型列表共用。
export const tierIcon = plus =>
    plus
        ? html`<span class="shrink-0 text-amber-500" role="img" aria-label="仅 Pro Plus 可用" title="仅 Pro Plus 可用"
              ><svg class="size-4" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
                  <path d="M8 1.5 9.7 6.15 14.66 6.34 10.76 9.4 12.11 14.16 8 11.4 3.89 14.16 5.24 9.4 1.34 6.34 6.3 6.15Z"></path></svg
          ></span>`
        : html`<span
              class="shrink-0 text-blue-600"
              role="img"
              aria-label="Pro 与 Pro Plus 均可使用"
              title="Pro 与 Pro Plus 均可使用"
              ><svg class="size-4" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                  <circle cx="8" cy="8" r="6.25" stroke="currentColor" stroke-width="1.5"></circle>
                  <path
                      d="m5.3 8.2 1.9 1.9 3.5-3.7"
                      stroke="currentColor"
                      stroke-width="1.5"
                      stroke-linecap="round"
                      stroke-linejoin="round"
                  ></path></svg
          ></span>`;
