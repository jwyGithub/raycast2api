import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { dayjs, formatDate } from './utils.js';
import { message } from './components.js';

class AccountCard extends LitElement {
    static properties = { account: { attribute: false }, busy: { type: Boolean } };
    createRenderRoot() {
        return this;
    }
    date(value) {
        return formatDate(value);
    }
    credits(value) {
        return value == null ? '暂无数据' : Number(value).toLocaleString('zh-CN', { maximumFractionDigits: 2 });
    }
    cooldownLeft(until) {
        if (!until) return '';
        const mins = Math.ceil(dayjs.unix(until).diff(dayjs(), 'minute', true));
        if (mins <= 0) return '';
        if (mins < 60) return mins + ' 分钟';
        const hours = Math.floor(mins / 60);
        const rem = mins % 60;
        return hours + ' 小时' + (rem ? ' ' + rem + ' 分' : '');
    }
    connectedCallback() {
        super.connectedCallback();
        // 冷却倒计时自走:每 30s 重渲染一次。
        this.timer = setInterval(() => this.requestUpdate(), 30000);
    }
    disconnectedCallback() {
        super.disconnectedCallback();
        clearInterval(this.timer);
    }
    render() {
        const a = this.account || {};
        const cooling = a.cooldown_until && dayjs.unix(a.cooldown_until).isAfter(dayjs());
        const state = !a.available ? '不可用' : cooling ? '冷却中' : '可用';
        const status = !a.available ? 'unavailable' : cooling ? 'cooldown' : 'available';
        const error = a.unavailable_reason || a.error || '';
        const action = name =>
            this.dispatchEvent(
                new CustomEvent('account-action', { detail: { action: name, email: a.email }, bubbles: true, composed: true })
            );
        return html`<article
            class="h-full overflow-hidden rounded-xl border border-line bg-white p-0 hover:border-[#c9ced7] flex flex-col"
            aria-label=${(a.email || '未知账号') + '，' + state}
        >
            <div class="flex-1 p-3">
                <div class="flex items-start justify-between gap-3">
                    <div class="flex min-w-0 items-center gap-3">
                        <div class="min-w-0">
                            <h3 class="text-[13px] font-semibold text-ink">
                                <ui-tooltip .content=${a.email || '未知账号'}></ui-tooltip>
                            </h3>
                            ${a.tag ? html`<p class="text-xs text-muted"><ui-tooltip .content=${a.tag}></ui-tooltip></p>` : ''}
                        </div>
                    </div>
                    <div class="flex shrink-0 items-center gap-1.5">
                        ${a.trial_days_left != null
                            ? html`<span
                                  class="whitespace-nowrap text-xs text-muted"
                                  aria-label=${'试用剩余 ' + a.trial_days_left + ' 天'}
                                  >${a.trial_days_left} 天</span
                              >`
                            : ''}
                        <span
                            class="inline-flex shrink-0 items-center gap-[5px] rounded-md px-[7px] py-[3px] text-xs font-medium before:size-[5px] before:rounded-full before:bg-current data-[state=available]:bg-emerald-50 data-[state=available]:text-emerald-700 data-[state=cooldown]:bg-amber-50 data-[state=cooldown]:text-amber-700 data-[state=unavailable]:bg-red-50 data-[state=unavailable]:text-red-700"
                            data-state=${status}
                            >${state}</span
                        >
                    </div>
                </div>
                <div
                    class="my-2 flex items-center justify-between gap-2 [&>div]:flex [&>div]:min-w-0 [&>div]:items-baseline [&>div]:gap-1.5"
                >
                    <div>
                        <p class="text-xs text-muted shrink-0">额度</p>
                        <p class="min-w-0 text-lg leading-[1.3] font-semibold tracking-[-.4px] tabular-nums">
                            <ui-tooltip .content=${this.credits(a.remaining_credits)}></ui-tooltip>
                        </p>
                        <span class="shrink-0 text-xs text-muted">credits</span>
                    </div>
                    <p class="min-w-0 max-w-[40%] text-xs text-[#596373]">
                        <ui-tooltip .content=${a.tier || (a.has_pro_features ? 'Pro' : '—')}></ui-tooltip>
                    </p>
                </div>
                <div class="grid gap-0.5 border-t border-[#eff0f2] pt-1.5 text-xs leading-[18px]">
                    <dl
                        class="grid grid-cols-2 gap-x-2 [&>div]:flex [&>div]:min-w-0 [&>div]:items-baseline [&>div]:gap-1 [&_dd]:min-w-0 [&_dd]:text-[#596373] [&_dt]:shrink-0 [&_dt]:whitespace-nowrap [&_dt]:text-muted"
                    >
                        <div class="min-w-0">
                            <dt>创建时间</dt>
                            <dd><ui-tooltip .content=${this.date(a.created_at)}></ui-tooltip></dd>
                        </div>
                        <div class="min-w-0">
                            <dt>额度同步</dt>
                            <dd><ui-tooltip .content=${this.date(a.info_fetched_at)}></ui-tooltip></dd>
                        </div>
                    </dl>
                </div>
                <div
                    class="mt-2 min-w-0 rounded-lg px-2 py-1.5 text-xs leading-[18px] data-[state=available]:bg-emerald-50 data-[state=available]:text-emerald-700 data-[state=cooldown]:bg-amber-50 data-[state=cooldown]:text-amber-700 data-[state=unavailable]:bg-red-50 data-[state=unavailable]:text-red-700"
                    data-state=${status}
                >
                    <ui-tooltip
                        .content=${error ||
                        (cooling
                            ? '预计恢复：' + this.date(a.cooldown_until) + '（' + this.cooldownLeft(a.cooldown_until) + '后）'
                            : '无')}
                    ></ui-tooltip>
                </div>
            </div>
            <div
                class="px-3 pb-2 [&_button]:min-h-7 [&_button]:px-2 [&_button]:py-[3px] [&_button]:text-xs [&>ui-button:last-child]:ml-auto [&>ui-button:last-child>button]:border-transparent [&>ui-button:last-child>button]:bg-transparent [&>ui-button:last-child>button:hover]:bg-red-50 flex items-center gap-2"
            >
                <ui-button
                    class="inline-flex"
                    label="刷新额度"
                    .disabled=${this.busy}
                    @button-click=${() => action('refresh')}
                ></ui-button
                ><ui-button
                    class="inline-flex"
                    label="恢复"
                    .disabled=${this.busy || (!cooling && a.available)}
                    @button-click=${() => action('restore')}
                ></ui-button>
                <ui-button
                    class="inline-flex"
                    label="删除"
                    variant="danger"
                    .disabled=${this.busy}
                    @button-click=${() => action('delete')}
                ></ui-button>
            </div>
        </article>`;
    }
}
customElements.define('account-card', AccountCard);
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
                    <label for="log-request-id" class="mb-1 block text-xs text-muted">客户端请求 ID</label
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
                                '客户端请求 ID',
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

const statisticStatuses = [
    ['in_progress', '进行中', 'bg-blue-50 text-blue-700'],
    ['completed', '已完成', 'bg-emerald-50 text-emerald-700'],
    ['failed', '失败', 'bg-red-50 text-red-700'],
    ['cancelled', '已取消', 'bg-slate-100 text-slate-600'],
    ['interrupted', '已中断', 'bg-amber-50 text-amber-700']
];
const statisticFields = [
    ['request_id', '客户端请求 ID'],
    ['account_email', '账号邮箱'],
    ['requested_model', '请求模型'],
    ['upstream_model', '上游模型'],
    ['api_key', 'API Key']
];

class StatisticsPanel extends LitElement {
    static properties = {
        request: { attribute: false },
        items: { state: true },
        page: { state: true },
        total: { state: true },
        pages: { state: true },
        pageSize: { state: true },
        draft: { state: true },
        loading: { state: true },
        error: { state: true },
        detailId: { state: true },
        detail: { state: true },
        detailLoading: { state: true },
        detailError: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, {
            items: [],
            page: 1,
            total: 0,
            pages: 0,
            pageSize: 20,
            draft: {},
            filters: {},
            loading: false,
            error: '',
            detailId: '',
            detail: null,
            detailLoading: false,
            detailError: '',
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
            let data = await this.request('list', {
                ...this.filters,
                page: this.page,
                page_size: this.pageSize
            });
            if (data.total_pages && this.page > data.total_pages) {
                this.page = data.total_pages;
                data = await this.request('list', {
                    ...this.filters,
                    page: this.page,
                    page_size: this.pageSize
                });
            }
            this.items = data.items || [];
            this.total = data.total || 0;
            this.pages = data.total_pages || 0;
            if (!this.total) this.page = 1;
        } catch (error) {
            this.items = [];
            this.total = 0;
            this.pages = 0;
            this.error = error.message || '加载统计失败';
            message(this.error);
        } finally {
            this.loading = false;
        }
    }
    search(event) {
        event.preventDefault();
        if (this.loading) return;
        const filters = Object.fromEntries(
            Object.entries(this.draft)
                .map(([key, value]) => [key, value.trim()])
                .filter(([, value]) => value)
        );
        for (const key of ['requested_at_from', 'requested_at_to']) {
            if (!filters[key]) continue;
            const value = filters[key].length === 16 ? filters[key] + ':00' : filters[key];
            const seconds = Date.parse(value + '+08:00') / 1000;
            if (
                !Number.isFinite(seconds) ||
                seconds < 0 ||
                new Date((seconds + 8 * 3600) * 1000).toISOString().slice(0, 19) !== value
            ) {
                message('请选择有效的请求时间（北京时间）。');
                return;
            }
            filters[key] = seconds;
        }
        if (
            filters.requested_at_from != null &&
            filters.requested_at_to != null &&
            filters.requested_at_from > filters.requested_at_to
        ) {
            message('开始时间不能晚于结束时间。');
            return;
        }
        this.filters = filters;
        this.page = 1;
        this.load();
    }
    reset() {
        if (this.loading) return;
        this.draft = {};
        this.filters = {};
        this.page = 1;
        this.load();
    }
    changePage(page) {
        if (this.loading || page < 1 || page > this.pages) return;
        this.page = page;
        this.load();
    }
    async openDetail(id) {
        const version = ++this.detailVersion;
        this.detailId = id;
        this.detail = null;
        this.detailError = '';
        this.detailLoading = true;
        try {
            const detail = await this.request('detail', { id });
            if (version === this.detailVersion) this.detail = detail;
        } catch (error) {
            if (version === this.detailVersion) this.detailError = error.message || '加载详情失败';
        } finally {
            if (version === this.detailVersion) this.detailLoading = false;
        }
    }
    closeDetail() {
        this.detailId = '';
        this.detailVersion++;
    }
    disconnectedCallback() {
        this.closeDetail();
        super.disconnectedCallback();
    }
    number(value, maximumFractionDigits = 0) {
        return typeof value === 'number' && Number.isFinite(value)
            ? value.toLocaleString('zh-CN', { maximumFractionDigits })
            : '—';
    }
    statusBadge(value) {
        const [, label, color] = statisticStatuses.find(([status]) => status === value) || [
            '',
            value,
            'bg-slate-100 text-slate-600'
        ];
        return html`<span class="inline-flex rounded-md px-2 py-1 font-medium ${color}">${label || '—'}</span>`;
    }
    render() {
        const detail = this.detail;
        const requestedReasoning = [
            detail?.requested_effort,
            detail?.requested_thinking_json == null ? null : JSON.stringify(detail.requested_thinking_json)
        ]
            .filter(Boolean)
            .join(' · ');
        return html`<section
            class="mx-auto flex h-full min-h-0 w-full flex-col px-8 pt-4 max-[700px]:px-5"
            aria-label="上游调用统计"
        >
            <form class="mb-4 shrink-0 rounded-xl border border-line bg-white p-4" @submit=${this.search}>
                <div class="grid grid-cols-4 gap-3 max-[1100px]:grid-cols-2 max-[500px]:grid-cols-1">
                    ${statisticFields.map(
                        ([key, label]) =>
                            html`<div class="flex min-w-0 items-center gap-3">
                                <label for=${'statistic-' + key} class="shrink-0 whitespace-nowrap text-xs text-muted"
                                    >${label}</label
                                >
                                <input
                                    id=${'statistic-' + key}
                                    type=${key === 'api_key' ? 'password' : 'text'}
                                    autocomplete="off"
                                    class="h-9 min-w-0 flex-1 rounded-lg border border-line bg-white px-3 text-[13px]"
                                    placeholder=${key === 'api_key' ? '输入完整 Key 精确匹配' : '输入完整值精确匹配'}
                                    .value=${this.draft[key] || ''}
                                    ?disabled=${this.loading}
                                    @input=${e => (this.draft = { ...this.draft, [key]: e.target.value })}
                                />
                            </div>`
                    )}
                    ${[
                        [
                            'status',
                            '调用状态',
                            [['', '全部状态'], ...statisticStatuses.map(([value, label]) => [value, label])]
                        ],
                        [
                            'call_type',
                            '调用类型',
                            [
                                ['', '全部类型'],
                                ['inference', '正式推理'],
                                ['compact', '压缩摘要']
                            ]
                        ]
                    ].map(
                        ([key, label, options]) =>
                            html`<div class="flex min-w-0 items-center gap-3">
                                <span class="shrink-0 whitespace-nowrap text-xs text-muted">${label}</span>
                                <ui-select
                                    class="block min-w-0 flex-1 [&_button]:min-w-0"
                                    .label=${label}
                                    .options=${options}
                                    .value=${this.draft[key] || ''}
                                    .disabled=${this.loading}
                                    @select-change=${e => (this.draft = { ...this.draft, [key]: e.detail.value })}
                                ></ui-select>
                            </div>`
                    )}
                    ${[
                        ['requested_at_from', '开始时间'],
                        ['requested_at_to', '结束时间']
                    ].map(
                        ([key, label]) =>
                            html` <ui-date-picker
                                class="flex min-w-0 items-center gap-3 [&>label]:mb-0 [&>label]:shrink-0 [&>label]:whitespace-nowrap [&>div]:min-w-0 [&>div]:flex-1 [&_input]:w-full"
                                .label=${label}
                                .value=${this.draft[key] || ''}
                                .disabled=${this.loading}
                                @date-change=${e => (this.draft = { ...this.draft, [key]: e.detail.value })}
                            ></ui-date-picker>`
                    )}
                    <div class="col-span-3 flex flex-wrap items-center justify-end gap-2 max-[1100px]:col-span-1">
                        <button
                            class="h-9 rounded-lg bg-brand px-4 text-[13px] text-white disabled:opacity-40"
                            ?disabled=${this.loading}
                        >
                            查询
                        </button>
                        <ui-button
                            class="inline-flex"
                            label="重置"
                            .disabled=${this.loading}
                            @button-click=${this.reset}
                        ></ui-button>
                        <ui-button
                            class="inline-flex"
                            .label=${this.loading ? '刷新中…' : '刷新列表'}
                            .disabled=${this.loading}
                            @button-click=${this.load}
                        ></ui-button>
                    </div>
                </div>
            </form>
            ${this.error
                ? html`<p role="alert" class="mb-3 shrink-0 text-xs text-red-700">${this.error}，请刷新列表重试。</p>`
                : ''}
            <div class="min-h-0 flex-1 overflow-auto rounded-xl border border-line bg-white" aria-busy=${String(this.loading)}>
                <table class="w-full whitespace-nowrap text-left text-xs">
                    <caption class="sr-only">
                        上游调用尝试，按开始时间倒序显示
                    </caption>
                    <thead class="border-b border-line bg-slate-50 text-muted">
                        <tr>
                            ${[
                                '请求时间',
                                '客户端请求 ID',
                                '账号',
                                '请求模型 / 上游模型',
                                '类型',
                                '状态',
                                'HTTP',
                                '输入 / 输出 Token',
                                'Cost',
                                'Credits',
                                '操作'
                            ].map(label => html`<th scope="col" class="px-3 py-3 font-medium">${label}</th>`)}
                        </tr>
                    </thead>
                    <tbody class="divide-y divide-line">
                        ${this.items.length
                            ? this.items.map(
                                  item =>
                                      html`<tr class="hover:bg-slate-50/70">
                                          <td class="px-3 py-3 tabular-nums">${item.requested_at}</td>
                                          <td class="px-3 py-3">
                                              <button
                                                  class="max-w-48 truncate text-left font-mono hover:underline"
                                                  title=${item.request_id}
                                                  @click=${() => this.openDetail(item.id)}
                                              >
                                                  ${item.request_id}
                                              </button>
                                          </td>
                                          <td class="max-w-48 truncate px-3 py-3" title=${item.account_email}>
                                              ${item.account_email}
                                          </td>
                                          <td class="max-w-64 px-3 py-3 font-mono">
                                              <p class="truncate" title=${item.requested_model}>${item.requested_model}</p>
                                              <p class="mt-1 truncate text-muted" title=${item.upstream_model}>
                                                  ${item.upstream_model}
                                              </p>
                                          </td>
                                          <td class="px-3 py-3">
                                              ${item.call_type === 'inference'
                                                  ? '正式推理'
                                                  : item.call_type === 'compact'
                                                    ? '压缩摘要'
                                                    : item.call_type}
                                          </td>
                                          <td class="px-3 py-3">${this.statusBadge(item.status)}</td>
                                          <td class="px-3 py-3 font-mono">${item.http_status ?? '—'}</td>
                                          <td class="px-3 py-3 tabular-nums">
                                              ${this.number(item.usage_json?.input_tokens)} /
                                              ${this.number(item.usage_json?.output_tokens)}
                                          </td>
                                          <td class="px-3 py-3 tabular-nums">${this.number(item.usage_json?.total_cost, 6)}</td>
                                          <td class="px-3 py-3 tabular-nums">
                                              ${this.number(item.usage_json?.total_credits, 4)}
                                          </td>
                                          <td class="px-3 py-3">
                                              <button
                                                  class="rounded-md border border-line px-2 py-1 hover:bg-slate-100"
                                                  @click=${() => this.openDetail(item.id)}
                                              >
                                                  详情
                                              </button>
                                          </td>
                                      </tr>`
                              )
                            : html`<tr>
                                  <td colspan="11" class="py-16 text-center text-muted" role="status">
                                      ${this.loading
                                          ? '正在加载统计…'
                                          : this.error
                                            ? '统计加载失败，请重试'
                                            : '暂无符合条件的调用记录'}
                                  </td>
                              </tr>`}
                    </tbody>
                </table>
            </div>
            <footer class="flex shrink-0 flex-wrap items-center justify-between gap-3 py-4 text-xs text-muted">
                <p>共 ${this.total} 次上游调用</p>
                <div class="flex flex-wrap items-center gap-2">
                    <ui-select
                        class="block"
                        label="统计每页条数"
                        .options=${[10, 20, 50, 100].map(size => [String(size), '每页 ' + size + ' 条'])}
                        .value=${String(this.pageSize)}
                        .disabled=${this.loading}
                        @select-change=${e => {
                            this.pageSize = Number(e.detail.value);
                            this.page = 1;
                            this.load();
                        }}
                    ></ui-select>
                    <ui-button
                        class="inline-flex"
                        label="上一页"
                        .disabled=${this.loading || this.page <= 1}
                        @button-click=${() => this.changePage(this.page - 1)}
                    ></ui-button>
                    <span>${this.page} / ${this.pages || 1}</span>
                    <ui-button
                        class="inline-flex"
                        label="下一页"
                        .disabled=${this.loading || this.page >= this.pages}
                        @button-click=${() => this.changePage(this.page + 1)}
                    ></ui-button>
                </div>
            </footer>
            <dialog
                class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-5xl overflow-auto rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
                aria-labelledby="statistic-detail-title"
                @close=${this.closeDetail}
            >
                <div class="sticky top-0 flex items-center justify-between gap-3 border-b border-line bg-white px-5 py-3">
                    <h2 id="statistic-detail-title" class="text-sm font-semibold">调用详情</h2>
                    <button
                        class="size-8 rounded-lg text-lg text-muted hover:bg-slate-100"
                        aria-label="关闭调用详情"
                        @click=${this.closeDetail}
                    >
                        ×
                    </button>
                </div>
                ${this.detailLoading
                    ? html`<p class="p-5 text-xs text-muted" role="status">正在加载详情…</p>`
                    : this.detailError
                      ? html`<div role="alert" class="flex items-center justify-between gap-3 p-5 text-xs text-red-700">
                            <span>${this.detailError}</span
                            ><ui-button
                                class="inline-flex"
                                label="重试"
                                @button-click=${() => this.openDetail(this.detailId)}
                            ></ui-button>
                        </div>`
                      : detail
                        ? html`<div class="p-5 text-xs">
                              <div class="space-y-5">
                                  ${[
                                      [
                                          '请求与来源',
                                          [
                                              ['上游调用 ID', detail.id],
                                              ['客户端请求 ID', detail.request_id],
                                              ['来源 IP', detail.request_ip],
                                              ['API Key（脱敏）', detail.api_key_masked]
                                          ]
                                      ],
                                      [
                                          '模型与推理',
                                          [
                                              ['请求模型', detail.requested_model],
                                              ['上游模型', detail.upstream_model],
                                              ['上游提供方', detail.upstream_provider],
                                              ['账号邮箱', detail.account_email],
                                              ['客户端推理配置', requestedReasoning || null],
                                              ['上游推理强度', detail.upstream_effort]
                                          ]
                                      ],
                                      [
                                          '调用结果与时间',
                                          [
                                              [
                                                  '调用类型',
                                                  detail.call_type === 'inference'
                                                      ? '正式推理'
                                                      : detail.call_type === 'compact'
                                                        ? '压缩摘要'
                                                        : detail.call_type
                                              ],
                                              ['调用状态', this.statusBadge(detail.status)],
                                              ['HTTP 状态', detail.http_status],
                                              ['请求时间', detail.requested_at],
                                              ['开始时间', detail.started_at],
                                              ['完成时间', detail.finished_at]
                                          ]
                                      ]
                                  ].map(
                                      ([title, fields]) =>
                                          html`<section class="border-t border-line pt-4">
                                              <h3 class="mb-3 font-medium">${title}</h3>
                                              <dl
                                                  class="grid grid-cols-4 gap-x-6 gap-y-4 max-[1100px]:grid-cols-3 max-[800px]:grid-cols-2 max-[500px]:grid-cols-1"
                                              >
                                                  ${fields.map(
                                                      ([label, value]) =>
                                                          html`<div class="min-w-0">
                                                              <dt class="text-muted">${label}</dt>
                                                              <dd class="mt-1 break-all font-mono leading-5">
                                                                  ${value ?? '—'}
                                                              </dd>
                                                          </div>`
                                                  )}
                                              </dl>
                                          </section>`
                                  )}
                              </div>
                              ${detail.error_message
                                  ? html`<div class="mt-5 rounded-lg bg-red-50 p-3 text-red-700">
                                        <h3 class="font-medium">错误 / 中断原因</h3>
                                        <p class="mt-1 whitespace-pre-wrap break-words leading-5">${detail.error_message}</p>
                                    </div>`
                                  : ''}
                              <h3 class="mb-2 mt-5 font-medium">完整 Usage</h3>
                              <pre
                                  class="overflow-auto whitespace-pre-wrap break-words rounded-lg bg-slate-50 p-3 font-mono leading-5"
                                  tabindex="0"
                                  aria-label="完整 Usage"
                              >
${detail.usage_json == null ? '暂无记录' : JSON.stringify(detail.usage_json, null, 2)}</pre
                              >
                          </div>`
                        : ''}
            </dialog>
        </section>`;
    }
}
customElements.define('statistics-panel', StatisticsPanel);

const filters = [
    ['all', '全部状态'],
    ['available', '可用'],
    ['cooldown', '冷却中'],
    ['unavailable', '不可用']
];

class AccountConsole extends LitElement {
    static properties = {
        pageSize: { state: true },
        panel: { state: true },
        accounts: { state: true },
        page: { state: true },
        total: { state: true },
        pages: { state: true },
        status: { state: true },
        loading: { state: true },
        workingEmail: { state: true },
        error: { state: true },
        dialog: { state: true },
        source: { state: true },
        importing: { state: true },
        authenticated: { state: true },
        dashboardPassword: { state: true },
        loginError: { state: true },
        loggingIn: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, {
            panel: 'accounts',
            pageSize: 24,
            accounts: [],
            page: 1,
            total: 0,
            pages: 0,
            status: 'all',
            loading: false,
            workingEmail: '',
            error: '',
            dialog: false,
            source: '',
            importing: false,
            authenticated: false,
            dashboardPassword: '',
            apiKey: '',
            loginError: '',
            loggingIn: false
        });
        this.handlePageShow = event => {
            if (event.persisted) this.lockConsole();
        };
        this.handlePageHide = () => this.lockConsole();
    }
    createRenderRoot() {
        return this;
    }
    connectedCallback() {
        super.connectedCallback();
        window.addEventListener('pageshow', this.handlePageShow);
        window.addEventListener('pagehide', this.handlePageHide);
        // 列表 60s 静默轮询:跟进冷却倒计时与账号状态变化,
        // 操作中/弹窗打开时跳过,避免覆盖用户正在看的内容。
        this.pollTimer = setInterval(() => {
            if (!this.authenticated || this.panel !== 'accounts' || this.workingEmail || this.dialog) return;
            this.load(true);
        }, 60000);
    }
    disconnectedCallback() {
        super.disconnectedCallback();
        clearInterval(this.pollTimer);
        window.removeEventListener('pageshow', this.handlePageShow);
        window.removeEventListener('pagehide', this.handlePageHide);
        this.lockConsole();
    }
    updated() {
        const dialog = this.querySelector('#dashboard-login');
        if (dialog && !dialog.open) {
            dialog.showModal();
            dialog.querySelector('input').focus();
        }
    }
    lockConsole(error = '') {
        this.authenticated = false;
        this.dashboardPassword = '';
        this.apiKey = '';
        this.loginAttempt = null;
        this.loggingIn = false;
        this.querySelector('logs-panel')?.closeDetail();
        this.querySelector('statistics-panel')?.closeDetail();
        this.querySelectorAll('ui-date-picker').forEach(picker => picker.picker?.close());
        this.querySelectorAll('ui-tooltip').forEach(tooltip => tooltip.hide());
        this.loginError = error;
        this.accounts = [];
        this.total = 0;
        this.pages = 0;
        this.panel = 'accounts';
        this.dialog = false;
        this.source = '';
    }
    async login(event) {
        event.preventDefault();
        if (this.loggingIn) return;
        const attempt = (this.loginAttempt = {});
        this.loggingIn = true;
        this.loginError = '';
        try {
            const response = await fetch('/v1/login', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ dashboard_password: this.dashboardPassword }),
                cache: 'no-store'
            });
            const result = await response.json().catch(() => ({}));
            if (this.loginAttempt !== attempt) return;
            if (!response.ok || result.code >= 400) throw new Error(result.message || '登录失败，请稍后重试。');
            if (!result.data || (result.data.api_key !== null && typeof result.data.api_key !== 'string'))
                throw new Error('登录响应无效，请重试。');
            this.apiKey = result.data.api_key || '';
            this.dashboardPassword = '';
            this.authenticated = true;
            await this.updateComplete;
            await this.load();
        } catch (error) {
            if (this.loginAttempt === attempt) this.loginError = error.message || '登录失败，请检查网络后重试。';
        } finally {
            if (this.loginAttempt === attempt) {
                this.loggingIn = false;
                if (!this.authenticated) {
                    await this.updateComplete;
                    const input = this.querySelector('#dashboard-password');
                    input?.focus();
                    input?.select();
                }
            }
        }
    }
    async request(path, body, resource = 'accounts') {
        if (!this.authenticated) throw new Error('请先输入控制台密码登录。');
        const response = await fetch('/v1/' + resource + '/' + path, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', ...(this.apiKey ? { Authorization: 'Bearer ' + this.apiKey } : {}) },
            body: JSON.stringify(body)
        });
        const result = await response.json().catch(() => ({}));
        if (response.status === 401) {
            this.lockConsole('登录已失效，请重新输入控制台密码。');
            throw new Error('登录已失效，请重新输入控制台密码。');
        }
        if (!response.ok || result.code >= 400) throw new Error(result.message || '请求失败 (' + response.status + ')');
        return result.data;
    }
    async load(silent = false) {
        if (!this.authenticated) return;
        if (!silent) {
            this.loading = true;
            this.error = '';
        }
        try {
            const data = await this.request('list', { page: this.page, page_size: this.pageSize, status: this.status });
            if (!this.authenticated) return;
            this.accounts = data.items || [];
            this.total = data.total || 0;
            this.pages = data.total_pages || 0;
            if (this.pages && this.page > this.pages) {
                this.page = this.pages;
                await this.load();
            }
        } catch (e) {
            // 静默轮询失败不打扰当前视图,只记 console。
            if (silent) console.warn('后台刷新失败:', e);
            else {
                this.error = e.message || '加载账号失败';
                message(this.error);
            }
        } finally {
            this.loading = false;
        }
    }
    async importAccounts() {
        let records;
        try {
            records = JSON.parse(this.source.trim());
            if (!Array.isArray(records)) records = [records];
            if (
                !records.length ||
                records.some(
                    x =>
                        !x ||
                        typeof x !== 'object' ||
                        Array.isArray(x) ||
                        typeof x.user?.email !== 'string' ||
                        typeof x.access_token !== 'string' ||
                        !x.access_token
                )
            )
                throw new Error('每个账号都需要包含 user.email 和 access_token。');
        } catch (e) {
            message(e instanceof SyntaxError ? 'JSON 格式无效，请检查后重试。' : e.message);
            return;
        }
        this.importing = true;
        try {
            const data = await this.request(
                'create',
                records.length === 1 && !this.source.trim().startsWith('[') ? records[0] : records
            );
            const results = data.results || [data.result];
            message(
                '导入完成：新增 ' +
                    results.filter(x => x === 'inserted').length +
                    '，更新 ' +
                    results.filter(x => x === 'updated').length +
                    '，未变化 ' +
                    results.filter(x => x === 'unchanged').length,
                'success'
            );
            this.dialog = false;
            this.source = '';
            this.status = 'all';
            this.page = 1;
            await this.load();
        } catch (e) {
            message(e.message || '导入失败');
        } finally {
            this.importing = false;
        }
    }
    chooseStatus(e) {
        this.status = e.detail.value;
        this.page = 1;
        this.load();
    }
    async accountAction(e) {
        const { action, email } = e.detail;
        if (this.workingEmail) return;
        if (action === 'delete' && !confirm('确定删除账号 ' + email + ' 吗？')) return;
        this.workingEmail = email;
        try {
            const account = await this.request(action, { email });
            const failed = action === 'restore' && !account.available;
            message(
                action === 'delete'
                    ? '账号已删除'
                    : action === 'restore'
                      ? failed
                          ? account.unavailable_reason || '账号校验失败，仍不可用'
                          : '账号已恢复并完成校验'
                      : '账号额度已刷新',
                failed ? 'error' : 'success'
            );
            await this.load();
        } catch (error) {
            message(error.message || '操作失败');
            if (action === 'refresh' || action === 'restore') await this.load(true);
        } finally {
            this.workingEmail = '';
        }
    }
    async switchPanel(panel) {
        this.querySelectorAll('ui-date-picker').forEach(picker => picker.picker?.close());
        this.panel = panel;
        await this.updateComplete;
        if (panel === 'logs') this.querySelector('logs-panel').load();
        if (panel === 'statistics') this.querySelector('statistics-panel').load();
    }
    render() {
        if (!this.authenticated)
            return html`<dialog
                id="dashboard-login"
                aria-labelledby="login-title"
                aria-describedby="login-description"
                aria-modal="true"
                class="fixed inset-0 m-auto w-[calc(100%-32px)] max-w-sm rounded-xl border border-line bg-white p-6 text-ink shadow-[0_24px_80px_#10182826] backdrop:bg-slate-950/40 backdrop:backdrop-blur-[2px]"
                @cancel=${event => event.preventDefault()}
            >
                <form @submit=${this.login}>
                    <h2 id="login-title" class="text-base font-semibold">登录管理控制台</h2>
                    <p id="login-description" class="mt-2 text-xs leading-5 text-muted">请输入控制台密码以继续。</p>
                    <label for="dashboard-password" class="mt-5 block text-xs font-medium">控制台密码</label>
                    <input
                        id="dashboard-password"
                        name="dashboard_password"
                        type="password"
                        autocomplete="current-password"
                        autofocus
                        required
                        aria-invalid=${this.loginError ? 'true' : 'false'}
                        aria-describedby="login-error"
                        class="mt-2 w-full rounded-lg border border-line bg-slate-50 px-3 py-2 text-sm outline-none focus:border-brand focus:bg-white max-[700px]:text-base"
                        .value=${this.dashboardPassword}
                        ?disabled=${this.loggingIn}
                        @input=${event => {
                            this.dashboardPassword = event.target.value;
                            this.loginError = '';
                        }}
                    />
                    <p id="login-error" role="alert" class="mt-2 min-h-5 text-xs leading-5 text-red-600">${this.loginError}</p>
                    <button
                        type="submit"
                        class="mt-3 inline-flex min-h-9 w-full items-center justify-center rounded-lg bg-brand px-3.5 py-2 text-[13px] font-medium text-white disabled:opacity-50"
                        ?disabled=${this.loggingIn}
                    >
                        ${this.loggingIn ? '验证中…' : '进入控制台'}
                    </button>
                </form>
            </dialog>`;
        return html` <main class="flex h-full flex-col overflow-hidden">
            <header class="border-b border-line bg-white shrink-0">
                <div
                    class="mx-auto grid w-full grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] min-h-14 items-center gap-3 px-8 py-2 max-[700px]:grid-cols-1 max-[700px]:px-5"
                >
                    <div class="flex items-center gap-3 text-[15px] font-[650] tracking-[-.3px]">
                        <span
                            class="grid size-8 place-items-center rounded-lg bg-blue-600 text-[19px] text-white"
                            aria-hidden="true"
                            >R</span
                        ><span>Raycast<span class="text-muted">2API</span></span>
                    </div>
                    <nav class="flex justify-self-center gap-1 rounded-lg bg-slate-100 p-1" aria-label="面板切换">
                        ${[
                            ['accounts', '账号面板'],
                            ['logs', '日志面板'],
                            ['statistics', '统计面板']
                        ].map(
                            ([value, label]) =>
                                html`<button
                                    class="rounded-md px-3 py-1.5 text-xs ${this.panel === value
                                        ? 'bg-white text-ink shadow-sm'
                                        : 'text-muted hover:bg-white/60'}"
                                    aria-pressed=${this.panel === value ? 'true' : 'false'}
                                    @click=${() => this.switchPanel(value)}
                                >
                                    ${label}
                                </button>`
                        )}
                    </nav>
                </div>
            </header>
            <div
                class="min-h-0 flex-1 flex-col ${this.panel === 'accounts' ? 'flex' : 'hidden'}"
                ?inert=${this.panel !== 'accounts'}
            >
                <section class="min-h-0 flex-1 flex flex-col pt-4" aria-label="账号列表">
                    <div class="mx-auto min-h-0 flex-1 flex flex-col w-full">
                        <div
                            class="mb-4 shrink-0 px-8 max-[700px]:px-5 flex items-center justify-between gap-3 max-[700px]:flex-wrap"
                        >
                            <ui-select
                                class="flex items-center gap-3"
                                label="筛选账号状态"
                                .options=${filters}
                                .value=${this.status}
                                @select-change=${this.chooseStatus}
                            ></ui-select>
                            <div class="flex items-center gap-2">
                                <ui-button
                                    class="inline-flex"
                                    label=${this.loading ? '刷新中…' : '刷新列表'}
                                    .disabled=${this.loading}
                                    @button-click=${() => this.load()}
                                ></ui-button>
                                <ui-button
                                    class="inline-flex"
                                    label="＋ 导入账号"
                                    variant="primary"
                                    @button-click=${() => {
                                        this.dialog = true;
                                    }}
                                ></ui-button>
                            </div>
                        </div>
                        <div
                            class="[scrollbar-width:thin] [scrollbar-color:#d9dce5_transparent] [&::-webkit-scrollbar]:w-[7px] [&::-webkit-scrollbar-thumb]:rounded-full [&::-webkit-scrollbar-thumb]:bg-[#d9dce5] min-h-0 flex-1 overflow-y-auto px-8 max-[700px]:px-5"
                            @scroll=${() => this.querySelectorAll('ui-tooltip').forEach(tooltip => tooltip.hide())}
                        >
                            ${this.loading && !this.accounts.length
                                ? html`<div
                                      class="grid grid-cols-4 items-stretch gap-3 max-[1200px]:grid-cols-3 max-[900px]:grid-cols-2 max-[700px]:grid-cols-1"
                                  >
                                      ${[1, 2, 3].map(
                                          () =>
                                              html`<div
                                                  class="h-52 animate-pulse rounded-xl border border-line bg-white"
                                              ></div>`
                                      )}
                                  </div>`
                                : this.accounts.length
                                  ? html`
                                        <div
                                            class="grid grid-cols-4 items-stretch gap-3 max-[1200px]:grid-cols-3 max-[900px]:grid-cols-2 max-[700px]:grid-cols-1"
                                            @account-action=${this.accountAction}
                                        >
                                            ${this.accounts.map(
                                                account =>
                                                    html`<account-card
                                                        class="block min-w-0"
                                                        .account=${account}
                                                        .busy=${this.workingEmail === account.email}
                                                    ></account-card>`
                                            )}
                                        </div>
                                    `
                                  : html`<div
                                        class="min-h-[200px] border-0 bg-transparent flex flex-col items-center justify-center px-6 text-center"
                                    >
                                        <div
                                            class="size-14 rounded-xl border border-line bg-white text-[#7b8491] grid place-items-center text-xl"
                                        >
                                            ＋
                                        </div>
                                        <h3 class="mt-4 text-sm font-semibold text-ink">
                                            ${this.status === 'all' ? '添加你的第一个账号' : '没有符合条件的账号'}
                                        </h3>
                                        <p class="mt-2 text-[13px] text-muted">导入账号 JSON，开始管理账号状态与额度。</p>
                                        ${this.status === 'all'
                                            ? html`<button
                                                  class="mt-4 rounded-lg bg-brand px-4 py-2 text-xs font-medium text-white"
                                                  @click=${() => (this.dialog = true)}
                                              >
                                                  导入账号
                                              </button>`
                                            : ''}
                                    </div>`}
                        </div>
                        <footer class="shrink-0 px-8 max-[700px]:px-5">
                            <div
                                class="flex items-center justify-between gap-4 py-4 max-[700px]:flex-wrap max-[700px]:[&>div]:ml-auto"
                            >
                                <p class="truncate text-xs text-muted">
                                    共 ${this.total} 个账号<span class="mx-2 text-slate-300">·</span>${this.total
                                        ? '第 ' + this.page + ' / ' + this.pages + ' 页'
                                        : '暂无账号'}
                                </p>
                                <div class="flex shrink-0 items-center gap-2">
                                    <ui-select
                                        class="block"
                                        label="账号每页条数"
                                        .options=${[12, 24, 48, 96].map(size => [String(size), '每页 ' + size + ' 条'])}
                                        .value=${String(this.pageSize)}
                                        .disabled=${this.loading}
                                        @select-change=${e => {
                                            this.pageSize = Number(e.detail.value);
                                            this.page = 1;
                                            this.load();
                                        }}
                                    ></ui-select>
                                    <ui-button
                                        class="inline-flex"
                                        label="上一页"
                                        .disabled=${this.page <= 1 || this.loading}
                                        @button-click=${() => {
                                            this.page--;
                                            this.load();
                                        }}
                                    ></ui-button
                                    ><span class="min-w-16 text-center text-xs font-medium"
                                        >${this.page} / ${this.pages || 1}</span
                                    ><ui-button
                                        class="inline-flex"
                                        label="下一页"
                                        .disabled=${this.page >= this.pages || this.loading}
                                        @button-click=${() => {
                                            this.page++;
                                            this.load();
                                        }}
                                    ></ui-button>
                                </div>
                            </div>
                        </footer>
                    </div>
                </section>
            </div>
            <logs-panel
                class="min-h-0 flex-1 overflow-hidden ${this.panel === 'logs' ? 'block' : 'hidden'}"
                ?inert=${this.panel !== 'logs'}
                .request=${(path, body) => this.request(path, body, 'logs')}
            ></logs-panel>
            <statistics-panel
                class="min-h-0 flex-1 overflow-hidden ${this.panel === 'statistics' ? 'block' : 'hidden'}"
                ?inert=${this.panel !== 'statistics'}
                .request=${(path, body) => this.request(path, body, 'statistics')}
            ></statistics-panel>
            ${this.dialog
                ? html`<div
                      class="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/40 p-4 backdrop-blur-[2px]"
                      @click=${e => {
                          if (e.target === e.currentTarget) this.dialog = false;
                      }}
                  >
                      <section
                          role="dialog"
                          aria-modal="true"
                          aria-labelledby="import-title"
                          class="max-h-[calc(100dvh-32px)] overflow-y-auto rounded-xl border border-line p-0 shadow-[0_24px_80px_#10182826] [&_input]:rounded-lg [&_textarea]:rounded-lg max-[700px]:[&_input]:text-base max-[700px]:[&_textarea]:text-base w-full max-w-xl bg-white"
                      >
                          <div class="flex items-center justify-between gap-4 border-b border-line px-6 py-5 max-[700px]:p-4">
                              <h2 id="import-title" class="text-base font-semibold">导入账号</h2>
                              <button
                                  class="grid size-8 place-items-center rounded-lg text-muted hover:bg-gray-100"
                                  aria-label="关闭导入窗口"
                                  @click=${() => (this.dialog = false)}
                              >
                                  ×
                              </button>
                          </div>
                          <div class="p-6 max-[700px]:p-4">
                              <p class="mt-1 text-xs leading-5 text-muted">
                                  粘贴单个账号 JSON 或账号 JSON 数组，按 user.email 更新账号。
                              </p>
                              <label for="account-json" class="mt-5 block text-xs font-medium">账号 JSON</label
                              ><textarea
                                  id="account-json"
                                  class="mt-2 h-64 w-full resize-y rounded-lg border border-line bg-slate-50 p-4 font-mono text-xs leading-5 outline-none focus:border-brand focus:bg-white"
                                  .value=${this.source}
                                  @input=${e => (this.source = e.target.value)}
                                  placeholder='[{"tag":"主账号","user":{"email":"name@example.com"},"access_token":"rca_...","refresh_token":"..."}]'
                              ></textarea>
                          </div>
                          <div class="flex justify-end gap-2 border-t border-line bg-[#fafbfc] px-6 py-4 max-[700px]:p-4">
                              <button
                                  class="inline-flex min-h-9 items-center justify-center rounded-lg border border-line px-3.5 py-2 text-[13px]"
                                  @click=${() => (this.dialog = false)}
                              >
                                  取消</button
                              ><button
                                  class="inline-flex min-h-9 items-center justify-center rounded-lg border border-line px-3.5 py-2 text-[13px] bg-brand text-white disabled:opacity-50"
                                  ?disabled=${this.importing}
                                  @click=${this.importAccounts}
                              >
                                  ${this.importing ? '导入中…' : '确定导入'}
                              </button>
                          </div>
                      </section>
                  </div>`
                : ''}
        </main>`;
    }
}
customElements.define('account-console', AccountConsole);
