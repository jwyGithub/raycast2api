import { LitElement, html, nothing } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';

/**
 * <ui-table>：表头固定（原生 position: sticky）、可选分页的数据表。
 *
 * 属性
 *   columns   [{ label, key?, render?(row, i), class?, title?(row) }]  label 可为字符串或模板
 *   rows      行数据数组
 *   caption   屏幕阅读器用标题
 *   empty     无数据时的提示文本（加载中/失败/空由调用方决定）
 *   loading   加载中：置 aria-busy，并禁用分页控件
 *   pagination  显示分页（默认不显示）；配合 page / pages / total / pageSize / summary / pageSizes
 * 事件
 *   page-change       detail: { page }
 *   page-size-change  detail: { pageSize }
 *
 * 尺寸：宿主是 flex 列容器（style.css 里设置 display:flex），由使用处决定高度，例如页面铺满用 `flex-1 min-h-0`，
 * 弹窗里用 `max-h-[60dvh]`；表格体在内部滚动，表头不跟着滚。
 */
class UiTable extends LitElement {
    static properties = {
        columns: { attribute: false },
        rows: { attribute: false },
        caption: { type: String },
        empty: { type: String },
        loading: { type: Boolean },
        pagination: { type: Boolean },
        page: { type: Number },
        pages: { type: Number },
        total: { type: Number },
        pageSize: { type: Number },
        pageSizes: { attribute: false },
        summary: { attribute: false }
    };
    constructor() {
        super();
        Object.assign(this, {
            columns: [],
            rows: [],
            caption: '',
            empty: '暂无数据',
            loading: false,
            pagination: false,
            page: 1,
            pages: 0,
            total: 0,
            pageSize: 20,
            pageSizes: [10, 20, 50, 100],
            summary: null
        });
    }
    createRenderRoot() {
        return this;
    }
    emit(name, detail) {
        this.dispatchEvent(new CustomEvent(name, { detail, bubbles: true, composed: true }));
    }
    goto(page) {
        if (!this.loading && page >= 1 && page <= this.pages) this.emit('page-change', { page });
    }
    renderFooter() {
        return html`<footer class="flex shrink-0 flex-wrap items-center justify-between gap-3 py-4 text-xs text-muted">
            <p>${this.summary ?? `共 ${this.total} 条`}</p>
            <div class="flex flex-wrap items-center gap-2">
                <ui-select
                    class="block"
                    label="每页条数"
                    .options=${this.pageSizes.map(size => [String(size), '每页 ' + size + ' 条'])}
                    .value=${String(this.pageSize)}
                    .disabled=${this.loading}
                    @select-change=${e => this.emit('page-size-change', { pageSize: Number(e.detail.value) })}
                ></ui-select>
                <ui-button
                    class="inline-flex"
                    label="上一页"
                    .disabled=${this.loading || this.page <= 1}
                    @button-click=${() => this.goto(this.page - 1)}
                ></ui-button>
                <span>${this.page} / ${this.pages || 1}</span>
                <ui-button
                    class="inline-flex"
                    label="下一页"
                    .disabled=${this.loading || this.page >= this.pages}
                    @button-click=${() => this.goto(this.page + 1)}
                ></ui-button>
            </div>
        </footer>`;
    }
    render() {
        // ponytail: sticky 放在 th 上而不是 thead（Safari 对 thead sticky 支持不稳）；
        // border-collapse 下边框不会跟着 sticky，所以用 inset 阴影画表头底线。
        return html`<div
                class="min-h-0 flex-1 overflow-auto rounded-xl border border-line bg-white"
                aria-busy=${this.loading ? 'true' : 'false'}
            >
                <table class="w-full whitespace-nowrap text-left text-xs">
                    ${this.caption ? html`<caption class="sr-only">${this.caption}</caption>` : nothing}
                    <thead class="text-muted">
                        <tr>
                            ${this.columns.map(
                                col =>
                                    html`<th
                                        scope="col"
                                        class="sticky top-0 z-10 bg-slate-50 px-3 py-3 font-medium shadow-[inset_0_-1px_0_var(--color-line)]"
                                    >
                                        ${col.label}
                                    </th>`
                            )}
                        </tr>
                    </thead>
                    <tbody class="divide-y divide-line">
                        ${this.rows.length
                            ? this.rows.map(
                                  (row, i) =>
                                      html`<tr class="hover:bg-slate-50/70">
                                          ${this.columns.map(
                                              col =>
                                                  html`<td
                                                      class="px-3 py-2 ${col.class || ''}"
                                                      title=${col.title?.(row) ?? nothing}
                                                  >
                                                      ${col.render ? col.render(row, i) : (row[col.key] ?? '—')}
                                                  </td>`
                                          )}
                                      </tr>`
                              )
                            : html`<tr>
                                  <td colspan=${this.columns.length} class="py-12 text-center text-muted" role="status">
                                      ${this.empty}
                                  </td>
                              </tr>`}
                    </tbody>
                </table>
            </div>
            ${this.pagination ? this.renderFooter() : nothing}`;
    }
}
customElements.define('ui-table', UiTable);
