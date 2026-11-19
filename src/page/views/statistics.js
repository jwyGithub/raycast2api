import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { message } from '../components.js';

const statisticStatuses = [
    ['in_progress', '进行中', 'bg-blue-50 text-blue-700'],
    ['completed', '已完成', 'bg-emerald-50 text-emerald-700'],
    ['failed', '失败', 'bg-red-50 text-red-700'],
    ['cancelled', '已取消', 'bg-slate-100 text-slate-600'],
    ['interrupted', '已中断', 'bg-amber-50 text-amber-700']
];
const statisticFields = [
    ['request_id', '请求 ID'],
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
    columns() {
        const text = (key, width) => ({ class: `${width} truncate`, title: item => item[key], render: item => item[key] });
        return [
            { label: '请求时间', class: 'tabular-nums', key: 'requested_at' },
            {
                label: '请求 ID',
                render: item => html`<button
                    class="max-w-48 truncate text-left font-mono hover:underline"
                    title=${item.request_id}
                    @click=${() => this.openDetail(item.id)}
                >
                    ${item.request_id}
                </button>`
            },
            { label: '账号', ...text('account_email', 'max-w-48') },
            {
                label: '请求模型 / 上游模型',
                class: 'max-w-64 font-mono',
                render: item => html`<p class="truncate" title=${item.requested_model}>${item.requested_model}</p>
                    <p class="mt-1 truncate text-muted" title=${item.upstream_model}>${item.upstream_model}</p>`
            },
            {
                label: '类型',
                render: item =>
                    item.call_type === 'inference' ? '正式推理' : item.call_type === 'compact' ? '压缩摘要' : item.call_type
            },
            { label: '状态', render: item => this.statusBadge(item.status) },
            { label: 'HTTP', class: 'font-mono', render: item => item.http_status ?? '—' },
            {
                label: '输入 / 输出 Token',
                class: 'tabular-nums',
                render: item => `${this.number(item.usage_json?.input_tokens)} / ${this.number(item.usage_json?.output_tokens)}`
            },
            { label: 'Cost', class: 'tabular-nums', render: item => this.number(item.usage_json?.total_cost, 6) },
            { label: 'Credits', class: 'tabular-nums', render: item => this.number(item.usage_json?.total_credits, 4) },
            {
                label: '操作',
                render: item => html`<button
                    class="rounded-md border border-line px-2 py-1 hover:bg-slate-100"
                    @click=${() => this.openDetail(item.id)}
                >
                    详情
                </button>`
            }
        ];
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
            <ui-table
                class="min-h-0 flex-1"
                caption="上游调用尝试，按开始时间倒序显示"
                .columns=${this.columns()}
                .rows=${this.items}
                .loading=${this.loading}
                .empty=${this.loading ? '正在加载统计…' : this.error ? '统计加载失败，请重试' : '暂无符合条件的调用记录'}
                pagination
                .page=${this.page}
                .pages=${this.pages}
                .total=${this.total}
                .pageSize=${this.pageSize}
                .summary=${`共 ${this.total} 次上游调用`}
                @page-change=${e => this.changePage(e.detail.page)}
                @page-size-change=${e => {
                    this.pageSize = e.detail.pageSize;
                    this.page = 1;
                    this.load();
                }}
            ></ui-table>
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
                                              ['请求 ID', detail.request_id],
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
