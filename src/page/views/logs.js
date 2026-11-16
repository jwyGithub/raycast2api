import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { dayjs, formatDate } from '../utils.js';
import { message } from '../components.js';

class LogsPanel extends LitElement {
    static properties = {
        request: { attribute: false },
        items: { state: true },
        page: { state: true },
        total: { state: true },
        pages: { state: true },
        pageSize: { state: true },
        requestId: { state: true },
        createdAt: { state: true },
        loading: { state: true },
        error: { state: true },
        detailId: { state: true },
        detail: { state: true },
        detailLoading: { state: true },
        detailError: { state: true },
        detailTab: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, {
            items: [],
            page: 1,
            total: 0,
            pages: 0,
            pageSize: 20,
            requestId: '',
            createdAt: '',
            loading: false,
            error: '',
            filters: {},
            detailId: '',
            detail: null,
            detailLoading: false,
            detailError: '',
            detailTab: 'inbound_json',
            detailVersion: 0
        });
    }
    createRenderRoot() {
        return this;
    }
    updated() {
        const dialog = this.querySelector('dialog');
        if (this.detailId && !dialog.open) dialog.showModal();
        else if (!this.detailId && dialog.open) dialog.close();
    }
    async load() {
        if (this.loading) return;
        this.loading = true;
        this.error = '';
        try {
            let data = await this.request('list', { page: this.page, page_size: this.pageSize, ...this.filters });
            if (data.total_pages && this.page > data.total_pages) {
                this.page = data.total_pages;
                data = await this.request('list', { page: this.page, page_size: this.pageSize, ...this.filters });
            }
            this.items = data.items || [];
            this.total = data.total || 0;
            this.pages = data.total_pages || 0;
        } catch (error) {
            this.error = error.message || '加载日志失败';
            message(this.error);
        } finally {
            this.loading = false;
        }
    }
    search(event) {
        event.preventDefault();
        if (this.loading) return;
        const date = this.createdAt ? dayjs(this.createdAt, ['YYYY-MM-DDTHH:mm:ss', 'YYYY-MM-DDTHH:mm'], true) : null;
        if (date && !date.isValid()) {
            message('请输入有效的创建时间，精确到秒。');
            return;
        }
        const createdAt = date ? date.unix() : null;
        this.filters = {
            ...(this.requestId.trim() ? { request_id: this.requestId.trim() } : {}),
            ...(createdAt != null ? { created_at: createdAt } : {})
        };
        this.page = 1;
        this.load();
    }
    reset() {
        if (this.loading) return;
        this.requestId = '';
        this.createdAt = '';
        this.filters = {};
        this.page = 1;
        this.load();
    }
    changePage(page) {
        if (this.loading || page < 1 || page > this.pages) return;
        this.page = page;
        this.load();
    }
    async openDetail(requestId) {
        const version = ++this.detailVersion;
        this.detailId = requestId;
        this.detail = null;
        this.detailError = '';
        this.detailLoading = true;
        this.detailTab = 'inbound_json';
        try {
            const detail = await this.request('detail', { request_id: requestId });
            if (version === this.detailVersion) this.detail = detail;
        } catch (error) {
            if (version === this.detailVersion) {
                this.detailError = error.message || '加载详情失败';
                message(this.detailError);
            }
        } finally {
            if (version === this.detailVersion) this.detailLoading = false;
        }
    }
    closeDetail() {
        this.detailId = '';
        this.detailVersion++;
    }
    formatBody(value) {
        if (value == null) return '暂无记录';
        if (value === '') return '（空内容）';
        try {
            return JSON.stringify(JSON.parse(value), null, 2);
        } catch {
            return value;
        }
    }
    bytes(value) {
        if (value == null) return '—';
        if (value < 1024) return value + ' B';
        if (value < 1048576) return (value / 1024).toFixed(1) + ' KB';
        return (value / 1048576).toFixed(1) + ' MB';
    }
    render() {
        return html`<div class="mx-auto flex h-full min-h-0 w-full flex-col px-8 pt-4 max-[700px]:px-5">
            <form class="mb-4 flex shrink-0 flex-wrap items-end gap-3" @submit=${this.search}>
                <div class="min-w-48 flex-1">
                    <label for="log-request-id" class="mb-1 block text-xs text-muted">请求 ID</label
                    ><input
                        id="log-request-id"
                        class="h-9 w-full rounded-lg border border-line bg-white px-3 text-[13px]"
                        placeholder="输入完整客户端请求 ID"
                        .value=${this.requestId}
                        @input=${e => (this.requestId = e.target.value)}
                    />
                </div>
                <ui-date-picker
                    class="block"
                    label="创建时间（精确到秒）"
                    .value=${this.createdAt}
                    .disabled=${this.loading}
                    @date-change=${e => (this.createdAt = e.detail.value)}
                ></ui-date-picker>
                <button
                    class="h-9 rounded-lg bg-brand px-4 text-[13px] text-white disabled:opacity-40"
                    ?disabled=${this.loading}
                >
                    查询
                </button>
                <ui-button class="inline-flex" label="重置" .disabled=${this.loading} @button-click=${this.reset}></ui-button>
                <ui-button
                    class="inline-flex"
                    label=${this.loading ? '刷新中…' : '刷新列表'}
                    .disabled=${this.loading}
                    @button-click=${this.load}
                ></ui-button>
            </form>
            <div
                class="min-h-0 flex-1 overflow-auto rounded-xl border border-line bg-white"
                aria-busy=${this.loading ? 'true' : 'false'}
            >
                <table class="w-full whitespace-nowrap text-left text-xs">
                    <caption class="sr-only">
                        请求日志，按创建顺序倒序显示
                    </caption>
                    <thead class="border-b border-line bg-slate-50 text-muted">
                        <tr>
                            ${[
                                '请求 ID',
                                '创建时间',
                                '方法',
                                '路径',
                                '模型',
                                '上游模型',
                                '推理强度',
                                '入站大小',
                                '上游大小',
                                '响应大小',
                                '操作'
                            ].map(label => html`<th scope="col" class="px-3 py-3 font-medium">${label}</th>`)}
                        </tr>
                    </thead>
                    <tbody class="divide-y divide-line">
                        ${this.items.length
                            ? this.items.map(
                                  item =>
                                      html`<tr class="hover:bg-slate-50/70">
                                          <td class="px-3 py-2">
                                              <button
                                                  class="max-w-64 truncate text-left font-mono hover:underline"
                                                  title=${item.request_id}
                                                  @click=${() => this.openDetail(item.request_id)}
                                              >
                                                  ${item.request_id}
                                              </button>
                                          </td>
                                          <td class="px-3 py-2 tabular-nums">${formatDate(item.created_at)}</td>
                                          <td class="px-3 py-2">
                                              <span class="rounded bg-slate-100 px-1.5 py-0.5 font-mono"
                                                  >${item.method || '—'}</span
                                              >
                                          </td>
                                          <td class="max-w-64 truncate px-3 py-2 font-mono" title=${item.path || ''}>
                                              ${item.path || '—'}
                                          </td>
                                          <td class="max-w-48 truncate px-3 py-2 font-mono" title=${item.model || ''}>
                                              ${item.model || '—'}
                                          </td>
                                          <td class="max-w-48 truncate px-3 py-2 font-mono" title=${item.upstream_model || ''}>
                                              ${item.upstream_model || '—'}
                                          </td>
                                          <td class="max-w-40 truncate px-3 py-2 font-mono" title=${item.effort || ''}>
                                              ${item.effort || '—'}
                                          </td>
                                          <td class="px-3 py-2 tabular-nums text-muted">${this.bytes(item.inbound_bytes)}</td>
                                          <td class="px-3 py-2 tabular-nums text-muted">${this.bytes(item.upstream_bytes)}</td>
                                          <td class="px-3 py-2 tabular-nums text-muted">${this.bytes(item.response_bytes)}</td>
                                          <td class="px-3 py-2">
                                              <button
                                                  class="rounded-md border border-line px-2 py-1 hover:bg-slate-100"
                                                  @click=${() => this.openDetail(item.request_id)}
                                              >
                                                  详情
                                              </button>
                                          </td>
                                      </tr>`
                              )
                            : html`<tr>
                                  <td colspan="11" class="py-16 text-center text-muted" role="status">
                                      ${this.loading
                                          ? '正在加载日志…'
                                          : this.error
                                            ? '日志加载失败，请重试'
                                            : '暂无符合条件的日志'}
                                  </td>
                              </tr>`}
                    </tbody>
                </table>
            </div>
            <footer class="flex shrink-0 flex-wrap items-center justify-between gap-3 py-4 text-xs text-muted">
                <p>共 ${this.total} 条日志 · 日志保留 7 天</p>
                <div class="flex items-center gap-2">
                    <ui-select
                        class="block"
                        label="每页条数"
                        .options=${[10, 20, 50, 100].map(size => [String(size), '每页 ' + size + ' 条'])}
                        .value=${String(this.pageSize)}
                        .disabled=${this.loading}
                        @select-change=${e => {
                            this.pageSize = Number(e.detail.value);
                            this.page = 1;
                            this.load();
                        }}
                    ></ui-select
                    ><ui-button
                        class="inline-flex"
                        label="上一页"
                        .disabled=${this.loading || this.page <= 1}
                        @button-click=${() => this.changePage(this.page - 1)}
                    ></ui-button
                    ><span>${this.page} / ${this.pages || 1}</span
                    ><ui-button
                        class="inline-flex"
                        label="下一页"
                        .disabled=${this.loading || this.page >= this.pages}
                        @button-click=${() => this.changePage(this.page + 1)}
                    ></ui-button>
                </div>
            </footer>
            <dialog
                class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-5xl overflow-hidden rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
                aria-labelledby="log-detail-title"
                @close=${this.closeDetail}
            >
                <div class="flex max-h-[calc(100dvh-34px)] flex-col">
                    <div class="flex shrink-0 items-center justify-between gap-3 border-b border-line px-5 py-3">
                        <h2 id="log-detail-title" class="text-sm font-semibold">日志详情</h2>
                        <button
                            class="size-8 rounded-lg text-lg text-muted hover:bg-slate-100"
                            aria-label="关闭日志详情"
                            @click=${this.closeDetail}
                        >
                            ×
                        </button>
                    </div>
                    <div class="shrink-0 space-y-1 px-5 py-3 text-xs text-muted">
                        <p class="break-all font-mono">${this.detailId}</p>
                        ${this.detail
                            ? html`<p class="break-all">
                                      ${formatDate(this.detail.created_at)} · ${this.detail.method || '—'} ·
                                      ${this.detail.path || '—'}
                                  </p>
                                  <p class="break-all">
                                      模型：<span class="font-mono text-ink">${this.detail.model || '—'}</span>
                                  </p>
                                  <p class="break-all">
                                      上游模型：<span class="font-mono text-ink">${this.detail.upstream_model || '—'}</span>
                                  </p>
                                  <p class="break-all">
                                      推理强度：<span class="font-mono text-ink">${this.detail.effort || '—'}</span>
                                  </p>`
                            : ''}
                    </div>
                    <div class="flex shrink-0 gap-1 border-b border-line px-5 pb-2" aria-label="日志内容">
                        ${[
                            ['inbound_json', '入站请求'],
                            ['upstream_json', '上游请求'],
                            ['response_text', '上游响应']
                        ].map(
                            ([value, label]) =>
                                html`<button
                                    class="rounded-lg px-3 py-1.5 text-xs ${this.detailTab === value
                                        ? 'bg-brand text-white'
                                        : 'text-muted hover:bg-slate-100'}"
                                    aria-pressed=${this.detailTab === value ? 'true' : 'false'}
                                    @click=${() => (this.detailTab = value)}
                                >
                                    ${label}
                                </button>`
                        )}
                    </div>
                    ${this.detailLoading
                        ? html`<p class="p-5 text-xs text-muted" role="status">正在加载详情…</p>`
                        : this.detailError
                          ? html`<div role="alert" class="flex items-center justify-between gap-3 p-5 text-xs text-red-700">
                                <span>详情加载失败</span
                                ><ui-button
                                    class="inline-flex"
                                    label="重试"
                                    @button-click=${() => this.openDetail(this.detailId)}
                                ></ui-button>
                            </div>`
                          : html`<pre
                                class="min-h-32 min-w-0 overflow-auto whitespace-pre-wrap break-words bg-slate-50 p-5 font-mono text-xs leading-5"
                                tabindex="0"
                                aria-label="日志正文"
                            >
${this.formatBody(this.detail?.[this.detailTab])}</pre
                            >`}
                </div>
            </dialog>
        </div>`;
    }
}
customElements.define('logs-panel', LogsPanel);
