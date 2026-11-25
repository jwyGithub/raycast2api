import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { dayjs, formatDate } from '../utils.js';
import { message } from '../components.js';

const detailTabs = {
    inbound_json: { label: '入站请求', headers: 'inbound_headers', headerLabel: '请求头', bodyLabel: '请求正文' },
    upstream_json: { label: '上游请求', headers: 'upstream_headers', headerLabel: '请求头', bodyLabel: '请求正文' },
    response_text: { label: '上游响应', headers: 'response_headers', headerLabel: '响应头', bodyLabel: '响应正文' }
};

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
    updated(changed) {
        if (changed.has('detailTab')) {
            const dialog = this.querySelector('ui-dialog');
            dialog.updateComplete.then(() => {
                for (const region of dialog.querySelectorAll('pre')) region.scrollTop = 0;
            });
        }
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
        this.detail = null;
        this.detailVersion++;
    }
    async copyDetail(text) {
        const version = this.detailVersion;
        let copied = false;
        try {
            if (navigator.clipboard?.writeText) {
                await navigator.clipboard.writeText(text);
                copied = true;
            }
        } catch {}
        if (version !== this.detailVersion || !this.isConnected) return;
        if (!copied) {
            const dialog = this.querySelector('dialog');
            if (!dialog?.open) return;
            const focus = document.activeElement;
            const input = document.createElement('textarea');
            input.value = text;
            input.readOnly = true;
            input.style.cssText = 'position:fixed;left:-9999px;top:0';
            dialog.append(input);
            try {
                input.select();
                input.setSelectionRange(0, text.length);
                copied = document.execCommand('copy');
            } catch {} finally {
                input.remove();
                focus?.focus();
            }
        }
        if (copied) message('已复制到剪贴板', 'success');
        else message('复制失败，请选中内容后手动复制。');
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
    renderDetailSummary() {
        const fields = [['请求 ID', this.detailId]];
        if (this.detail) {
            const { created_at, method, path, model, upstream_model, effort } = this.detail;
            fields.push(
                ['创建时间', formatDate(created_at)],
                ['请求路径', `${method || '—'} ${path || '—'}`],
                ['模型', model],
                ['上游模型', upstream_model],
                ['推理强度', effort]
            );
        }
        return html`<dl class="grid shrink-0 gap-x-4 overflow-x-auto px-5 py-3 text-left text-xs leading-5 ${this.detail
            ? 'grid-cols-[minmax(9rem,1.35fr)_minmax(9rem,1.1fr)_minmax(8rem,1fr)_minmax(8rem,1fr)_minmax(8rem,1fr)_minmax(6rem,0.65fr)]'
            : ''}">
            ${fields.map(([label, value]) => html`<div class="flex min-w-0 items-baseline gap-1.5">
                <dt class="shrink-0 whitespace-nowrap text-muted">${label}</dt>
                <dd class="min-w-0 flex-1 font-mono"><ui-tooltip .content=${value || '—'}></ui-tooltip></dd>
            </div>`)}
        </dl>`;
    }
    renderDetailContent() {
        const tab = detailTabs[this.detailTab];
        const headers = this.detail?.[tab.headers];
        const body = this.detail?.[this.detailTab];
        const formattedBody = this.formatBody(body);
        return html`<div class="log-detail-content grid h-[min(40rem,72dvh)] min-h-52 grid-rows-[minmax(0,1fr)_minmax(0,2fr)] gap-4">
            ${this.renderDetailRegion('headers', tab.headerLabel, headers || '暂无记录', headers)}
            ${this.renderDetailRegion('body', tab.bodyLabel, formattedBody, body ? formattedBody : '')}
        </div>`;
    }
    renderDetailRegion(kind, label, content, copyText) {
        return html`<section data-kind=${kind} class="log-region relative flex min-h-0 min-w-0 flex-col rounded-lg border">
            <h3 class="log-region-label absolute left-2 top-0 -translate-y-1/2 rounded-md px-2 text-xs leading-5 font-semibold">${label}</h3>
            <button type="button" class="log-region-copy absolute right-2 top-1.5 grid size-7 place-items-center rounded-md disabled:opacity-35"
                aria-label=${'复制' + label} title=${'复制' + label} ?disabled=${!copyText} @click=${() => this.copyDetail(copyText)}>
                <svg viewBox="0 0 16 16" class="size-4" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <rect x="5.5" y="5.5" width="8" height="8" rx="1.5"></rect>
                    <path d="M3 10.5H2.5V4A1.5 1.5 0 0 1 4 2.5h6.5V3"></path>
                </svg>
            </button>
            <pre class="min-h-0 min-w-0 flex-1 overflow-auto overscroll-contain rounded-lg whitespace-pre-wrap pb-2.5 pl-3 pr-11 pt-3 font-mono text-xs leading-5 [overflow-wrap:anywhere] focus-visible:outline-offset-[-3px]"
                tabindex="0" aria-label=${label}>${content}</pre>
        </section>`;
    }
    bytes(value) {
        if (value == null) return '—';
        if (value < 1024) return value + ' B';
        if (value < 1048576) return (value / 1024).toFixed(1) + ' KB';
        return (value / 1048576).toFixed(1) + ' MB';
    }
    columns() {
        const mono = (key, width) => ({
            class: `${width} truncate font-mono`,
            title: item => item[key] || '',
            render: item => item[key] || '—'
        });
        const size = key => ({ class: 'tabular-nums text-muted', render: item => this.bytes(item[key]) });
        return [
            {
                label: '请求 ID',
                render: item => html`<button
                    class="max-w-64 truncate text-left font-mono hover:underline"
                    title=${item.request_id}
                    @click=${() => this.openDetail(item.request_id)}
                >
                    ${item.request_id}
                </button>`
            },
            { label: '创建时间', class: 'tabular-nums', render: item => formatDate(item.created_at) },
            {
                label: '方法',
                render: item => html`<span class="rounded bg-slate-100 px-1.5 py-0.5 font-mono">${item.method || '—'}</span>`
            },
            { label: '路径', ...mono('path', 'max-w-64') },
            { label: '模型', ...mono('model', 'max-w-48') },
            { label: '上游模型', ...mono('upstream_model', 'max-w-48') },
            { label: '推理强度', ...mono('effort', 'max-w-40') },
            { label: '入站大小', ...size('inbound_bytes') },
            { label: '上游大小', ...size('upstream_bytes') },
            { label: '响应大小', ...size('response_bytes') },
            {
                label: '操作',
                render: item => html`<button
                    class="rounded-md border border-line px-2 py-1 hover:bg-slate-100"
                    @click=${() => this.openDetail(item.request_id)}
                >
                    详情
                </button>`
            }
        ];
    }
    render() {
        return html`<div class="mx-auto flex h-full min-h-0 w-full flex-col px-4 pt-4 max-[700px]:px-5">
            <form class="mb-4 flex shrink-0 flex-wrap items-center gap-3" @submit=${this.search}>
                <div class="flex min-w-48 flex-1 items-center gap-3">
                    <label for="log-request-id" class="shrink-0 whitespace-nowrap text-xs text-muted">请求 ID</label
                    ><input
                        id="log-request-id"
                        class="h-9 min-w-0 flex-1 rounded-lg border border-line bg-white px-3 text-[13px]"
                        placeholder="输入完整客户端请求 ID"
                        .value=${this.requestId}
                        @input=${e => (this.requestId = e.target.value)}
                    />
                </div>
                <ui-date-picker
                    class="flex items-center gap-3 [&>label]:mb-0 [&>label]:shrink-0 [&>label]:whitespace-nowrap"
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
            <ui-table
                class="min-h-0 flex-1"
                caption="请求日志，按创建顺序倒序显示"
                .columns=${this.columns()}
                .rows=${this.items}
                .loading=${this.loading}
                .empty=${this.loading ? '正在加载日志…' : this.error ? '日志加载失败，请重试' : '暂无符合条件的日志'}
                pagination
                .page=${this.page}
                .pages=${this.pages}
                .total=${this.total}
                .pageSize=${this.pageSize}
                .summary=${`共 ${this.total} 条日志 · 日志保留 7 天`}
                @page-change=${e => this.changePage(e.detail.page)}
                @page-size-change=${e => {
                    this.pageSize = e.detail.pageSize;
                    this.page = 1;
                    this.load();
                }}
            ></ui-table>
            <ui-dialog
                .open=${!!this.detailId}
                heading="日志详情"
                size="7xl"
                @dialog-close=${this.closeDetail}
                .toolbar=${html`${this.renderDetailSummary()}
                    <div class="flex shrink-0 gap-1 border-b border-line px-5 pb-2" aria-label="日志内容">
                        ${Object.entries(detailTabs).map(
                            ([value, { label }]) =>
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
                    </div>`}
                .content=${this.detailLoading
                    ? html`<p class="text-xs text-muted" role="status">正在加载详情…</p>`
                    : this.detailError
                      ? html`<div role="alert" class="flex items-center justify-between gap-3 text-xs text-red-700">
                            <span>详情加载失败</span>
                            <ui-button class="inline-flex" label="重试"
                                @button-click=${() => this.openDetail(this.detailId)}></ui-button>
                        </div>`
                      : this.renderDetailContent()}
            ></ui-dialog>
        </div>`;
    }
}
customElements.define('logs-panel', LogsPanel);
