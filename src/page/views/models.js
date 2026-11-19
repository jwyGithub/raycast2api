import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { message } from '../components.js';
import { formatDate } from '../utils.js';

// 能力标签:[展示名, 判定函数(abilities)];推理强度有单独一列,不在此重复。
const abilityTags = [
    ['工具', a => a.tools?.supported],
    ['视觉', a => a.vision],
    ['思考', a => a.thinking?.supported],
    ['联网', a => a.web_search],
    ['生图', a => a.image_generation]
];
// 映射预览与模型配置两个弹窗共用:行形如 { alias, model_id, model_name, created_at }。
const mappingColumns = [
    { label: '映射名称', class: 'max-w-56 truncate font-mono text-ink', title: r => r.alias, render: r => r.alias },
    {
        label: '模型名称',
        class: 'max-w-40 truncate',
        title: r => r.model_name || r.model_id,
        render: r => r.model_name || r.model_id
    },
    { label: '模型ID', class: 'max-w-48 truncate font-mono', title: r => r.model_id, render: r => r.model_id },
    { label: '创建时间', class: 'tabular-nums text-muted', render: r => formatDate(r.created_at) }
];
const usd = value => (value == null || value === '' ? '—' : '$' + Number(value));
// 订阅等级(in_better_ai_subscription):true = 仅 Pro Plus 可用,false = Pro 与 Pro Plus 均可使用。
const tierOptions = [
    ['', '全部等级'],
    ['pro', 'Pro 可用'],
    ['plus', 'Pro Plus 可用']
];
const tierIcon = plus =>
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

class ModelsPanel extends LitElement {
    static properties = {
        request: { attribute: false },
        items: { state: true },
        providers: { state: true },
        provider: { state: true },
        tier: { state: true },
        draftProvider: { state: true },
        draftTier: { state: true },
        detail: { state: true },
        config: { state: true },
        aliasInput: { state: true },
        aliasBusy: { state: true },
        preview: { state: true },
        previewItems: { state: true },
        previewLoading: { state: true },
        page: { state: true },
        total: { state: true },
        pages: { state: true },
        pageSize: { state: true },
        loading: { state: true },
        error: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, {
            items: [],
            providers: [],
            provider: '',
            tier: '',
            draftProvider: '',
            draftTier: '',
            detail: null,
            config: null,
            aliasInput: '',
            aliasBusy: false,
            preview: false,
            previewItems: [],
            previewLoading: false,
            page: 1,
            total: 0,
            pages: 0,
            pageSize: 20,
            loading: false,
            error: ''
        });
    }
    createRenderRoot() {
        return this;
    }
    updated() {
        for (const [id, wanted] of [
            ['model-detail-dialog', this.detail],
            ['model-config-dialog', this.config],
            ['model-preview-dialog', this.preview]
        ]) {
            const dialog = this.querySelector('#' + id);
            if (wanted && !dialog.open) dialog.showModal();
            else if (!wanted && dialog.open) dialog.close();
        }
    }
    openDetail(item) {
        this.detail = item;
    }
    closeDetail() {
        this.detail = null;
        this.closeConfig();
        this.closePreview();
    }
    openConfig(item) {
        this.config = item;
        this.aliasInput = '';
    }
    closeConfig() {
        this.config = null;
    }
    // 映射预览:每次打开都重新拉全量别名。
    async openPreview() {
        this.preview = true;
        this.previewItems = [];
        this.previewLoading = true;
        try {
            this.previewItems = (await this.request('alias/list', {})).items || [];
        } catch (error) {
            message(error.message || '加载映射失败');
        } finally {
            this.previewLoading = false;
        }
    }
    closePreview() {
        this.preview = false;
    }
    // 别名增删:成功后重新拉列表,弹窗里的别名列表随 items 刷新。
    async changeAlias(path, body, failure) {
        if (this.aliasBusy) return false;
        this.aliasBusy = true;
        try {
            await this.request('alias/' + path, body);
            await this.load();
            return true;
        } catch (error) {
            message(error.message || failure);
            return false;
        } finally {
            this.aliasBusy = false;
        }
    }
    async addAlias(event) {
        event.preventDefault();
        const alias = this.aliasInput.trim();
        if (!alias) return message('请输入客户端使用的模型名。');
        if (await this.changeAlias('create', { alias, model_id: this.config.id }, '添加映射失败')) this.aliasInput = '';
    }
    removeAlias(alias) {
        return this.changeAlias('delete', { alias }, '删除映射失败');
    }
    async load() {
        if (this.loading) return;
        this.loading = true;
        this.error = '';
        try {
            const query = () =>
                this.request('list', {
                    page: this.page,
                    page_size: this.pageSize,
                    ...(this.provider ? { provider_brand: this.provider } : {}),
                    ...(this.tier ? { in_better_ai_subscription: this.tier === 'plus' } : {})
                });
            let data = await query();
            if (data.total_pages && this.page > data.total_pages) {
                this.page = data.total_pages;
                data = await query();
            }
            this.items = data.items || [];
            this.total = data.total || 0;
            this.pages = data.total_pages || 0;
            this.providers = data.providers || [];
        } catch (error) {
            this.error = error.message || '加载模型失败';
            message(this.error);
        } finally {
            this.loading = false;
        }
    }
    search() {
        if (this.loading) return;
        this.provider = this.draftProvider;
        this.tier = this.draftTier;
        this.page = 1;
        this.load();
    }
    reset() {
        if (this.loading) return;
        this.provider = this.draftProvider = '';
        this.tier = this.draftTier = '';
        this.page = 1;
        this.load();
    }
    changePage(page) {
        if (this.loading || page < 1 || page > this.pages) return;
        this.page = page;
        this.load();
    }
    render() {
        return html`<div class="mx-auto flex h-full min-h-0 w-full flex-col px-8 pt-4 max-[700px]:px-5">
            <div class="mb-4 flex shrink-0 flex-wrap items-center gap-3">
                <div class="flex items-center gap-3">
                    <span class="shrink-0 whitespace-nowrap text-xs text-muted">供应商</span>
                    <ui-select
                        class="block w-50"
                        label="供应商"
                        .options=${[['', '全部供应商'], ...this.providers.map(p => [p.value, p.label])]}
                        .value=${this.draftProvider}
                        .disabled=${this.loading}
                        @select-change=${e => (this.draftProvider = e.detail.value)}
                    ></ui-select>
                </div>
                <div class="flex items-center gap-3">
                    <span class="shrink-0 whitespace-nowrap text-xs text-muted">订阅等级</span>
                    <ui-select
                        class="block w-48"
                        label="订阅等级"
                        .options=${tierOptions}
                        .value=${this.draftTier}
                        .disabled=${this.loading}
                        @select-change=${e => (this.draftTier = e.detail.value)}
                    ></ui-select>
                </div>
                <div class="ml-auto flex items-center gap-2">
                    <ui-button
                        class="inline-flex"
                        variant="primary"
                        label=${this.loading ? '查询中…' : '查询'}
                        .disabled=${this.loading}
                        @button-click=${this.search}
                    ></ui-button>
                    <ui-button
                        class="inline-flex"
                        label="重置"
                        .disabled=${this.loading}
                        @button-click=${this.reset}
                    ></ui-button>
                    <ui-button class="inline-flex" label="映射预览" @button-click=${this.openPreview}></ui-button>
                </div>
            </div>
            <ui-table
                class="min-h-0 flex-1"
                caption="Raycast 模型列表，按模型 ID 升序显示"
                .columns=${this.columns()}
                .rows=${this.items}
                .loading=${this.loading}
                .empty=${this.loading ? '正在加载模型…' : this.error ? '模型加载失败，请重试' : '暂无符合条件的模型'}
                pagination
                .page=${this.page}
                .pages=${this.pages}
                .total=${this.total}
                .pageSize=${this.pageSize}
                .summary=${`共 ${this.total} 个模型`}
                @page-change=${e => this.changePage(e.detail.page)}
                @page-size-change=${e => {
                    this.pageSize = e.detail.pageSize;
                    this.page = 1;
                    this.load();
                }}
            ></ui-table>
            <dialog
                id="model-detail-dialog"
                class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-sm overflow-hidden rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
                aria-labelledby="model-detail-title"
                @close=${this.closeDetail}
            >
                <div class="flex shrink-0 items-start justify-between gap-3 border-b border-line px-5 py-3">
                    <div class="min-w-0">
                        <h2 id="model-detail-title" class="truncate text-sm font-semibold">
                            ${this.detail?.name || this.detail?.id || '价格详情'}
                        </h2>
                        <p class="truncate font-mono text-[11px] text-muted">${this.detail?.id}</p>
                    </div>
                    <button
                        class="size-8 shrink-0 rounded-lg text-lg text-muted hover:bg-slate-100"
                        aria-label="关闭价格详情"
                        @click=${this.closeDetail}
                    >
                        ×
                    </button>
                </div>
                ${this.renderDetail()}
            </dialog>
            ${this.renderConfig()} ${this.renderPreview()}
        </div>`;
    }
    renderPreview() {
        return html`<dialog
            id="model-preview-dialog"
            class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-4xl overflow-hidden rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
            aria-labelledby="model-preview-title"
            @close=${this.closePreview}
        >
            <div class="flex shrink-0 items-center justify-between gap-3 border-b border-line px-5 py-3">
                <h2 id="model-preview-title" class="text-sm font-semibold">映射预览 · 共 ${this.previewItems.length} 条</h2>
                <button
                    class="size-8 shrink-0 rounded-lg text-lg text-muted hover:bg-slate-100"
                    aria-label="关闭映射预览"
                    @click=${this.closePreview}
                >
                    ×
                </button>
            </div>
            <div class="p-5">
                <ui-table
                    class="max-h-[60dvh]"
                    caption="全部模型映射"
                    .columns=${mappingColumns}
                    .rows=${this.previewItems}
                    .empty=${this.previewLoading ? '正在加载映射…' : '暂无映射'}
                ></ui-table>
            </div>
        </dialog>`;
    }
    columns() {
        const tag = (cls, text, title) => html`<span class="rounded px-1.5 py-0.5 ${cls}" title=${title ?? ''}>${text}</span>`;
        return [
            {
                label: '模型',
                class: 'max-w-72',
                title: item => (item.description || '').trim(),
                render: item => html`<div class="flex items-center gap-2">
                    ${tierIcon(item.in_better_ai_subscription === true)}
                    <div class="min-w-0">
                        <div class="truncate font-medium text-ink">${item.name || item.id}</div>
                        <div class="truncate font-mono text-[11px] text-muted">${item.id}</div>
                    </div>
                </div>`
            },
            {
                label: '供应商',
                render: item =>
                    html`${item.provider_name || '—'}<span class="ml-1 font-mono text-[11px] text-muted">${item.provider_brand || ''}</span>`
            },
            {
                label: '能力',
                render: item => {
                    const tags = abilityTags.filter(([, test]) => test(item.abilities || {}));
                    return html`<div class="flex gap-1">${tags.length ? tags.map(([label]) => tag('bg-slate-100', label)) : '—'}</div>`;
                }
            },
            {
                label: '推理强度',
                render: item => {
                    const { supported, options = [], default: defaultEffort } = item.abilities?.reasoning_effort || {};
                    const efforts = supported ? options : [];
                    return html`<div class="flex gap-1">
                        ${efforts.length
                            ? efforts.map(option =>
                                  option === defaultEffort
                                      ? tag('bg-emerald-50 text-emerald-700', option, '默认')
                                      : tag('bg-slate-100', option)
                              )
                            : '—'}
                    </div>`;
                }
            },
            {
                label: '状态',
                render: item => {
                    const replacement = item.deprecation_replacement_model_id;
                    return html`<div class="flex gap-1">
                        ${item.status ? tag('bg-amber-50 text-amber-700', item.status) : ''}
                        ${replacement ? tag('bg-red-50 text-red-700', '即将下线', '替代模型：' + replacement) : ''}
                        ${item.status || replacement ? '' : html`<span class="text-muted">—</span>`}
                    </div>`;
                }
            },
            {
                label: '别名',
                render: item => {
                    const count = item.aliases?.length ?? 0;
                    return tag(
                        'tabular-nums ' + (count ? 'bg-sky-50 text-sky-700' : 'bg-slate-100 text-muted'),
                        count,
                        count ? `已映射 ${count} 个别名` : '没有别名映射'
                    );
                }
            },
            {
                label: '操作',
                render: item => html`<button
                        class="rounded-md border border-line px-2 py-1 hover:bg-slate-100"
                        @click=${() => this.openDetail(item)}
                    >
                        价格
                    </button>
                    <button
                        class="ml-1 rounded-md border border-line px-2 py-1 hover:bg-slate-100"
                        @click=${() => this.openConfig(item)}
                    >
                        配置
                    </button>`
            }
        ];
    }
    renderConfig() {
        const model = this.config;
        // 取最新一次列表里的别名;列表翻页/筛选后找不到时退回打开时的快照。
        const aliases = (this.items.find(item => item.id === model?.id) || model)?.aliases || [];
        return html`<dialog
            id="model-config-dialog"
            class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-4xl overflow-hidden rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
            aria-labelledby="model-config-title"
            @close=${this.closeConfig}
        >
            <div class="flex shrink-0 items-start justify-between gap-3 border-b border-line px-5 py-3">
                <div class="min-w-0">
                    <h2 id="model-config-title" class="truncate text-sm font-semibold">
                        模型映射 · ${model?.name || model?.id}
                    </h2>
                    <p class="truncate font-mono text-[11px] text-muted">${model?.id}</p>
                </div>
                <button
                    class="size-8 shrink-0 rounded-lg text-lg text-muted hover:bg-slate-100"
                    aria-label="关闭模型配置"
                    @click=${this.closeConfig}
                >
                    ×
                </button>
            </div>
            <form class="flex shrink-0 items-center gap-2 border-b border-line px-5 py-3" @submit=${this.addAlias}>
                <label for="model-alias-input" class="sr-only">映射名称</label>
                <input
                    id="model-alias-input"
                    class="h-9 min-w-0 flex-1 rounded-lg border border-line bg-white px-3 font-mono text-xs"
                    placeholder="输入映射名称，例如 claude-sonnet-5-5"
                    autocomplete="off"
                    .value=${this.aliasInput}
                    ?disabled=${this.aliasBusy}
                    @input=${e => (this.aliasInput = e.target.value)}
                />
                <button
                    class="h-9 shrink-0 rounded-lg bg-brand px-4 text-xs text-white disabled:opacity-40"
                    ?disabled=${this.aliasBusy}
                >
                    添加
                </button>
            </form>
            <div class="p-5">
                <ui-table
                    class="max-h-[50dvh]"
                    caption="模型映射列表"
                    .columns=${[
                        ...mappingColumns,
                        {
                            label: html`<span class="sr-only">操作</span>`,
                            class: 'text-right',
                            render: ({ alias }) => html`<button
                                class="rounded-md border border-red-200 px-2 py-1 text-red-600 hover:bg-red-50 disabled:opacity-40"
                                ?disabled=${this.aliasBusy}
                                @click=${() => this.removeAlias(alias)}
                            >
                                删除
                            </button>`
                        }
                    ]}
                    .rows=${aliases.map(({ alias, created_at }) => ({
                        alias,
                        created_at,
                        model_id: model?.id,
                        model_name: model?.name
                    }))}
                    empty="暂无映射"
                ></ui-table>
            </div>
        </dialog>`;
    }
    renderDetail() {
        const item = this.detail;
        if (!item) return '';
        const pricing = item.pricing || {};
        const rows = [
            ['上下文', item.context != null ? item.context + 'K' : '—'],
            ['智能', item.intelligence ?? '—'],
            ['速度', item.speed ?? '—'],
            ['成本', item.cost ?? '—'],
            ['输入($/M)', usd(pricing.input_per_million_usd)],
            ['输出($/M)', usd(pricing.output_per_million_usd)],
            ['缓存输入($/M)', usd(pricing.cached_input_per_million_usd)],
            ['单次对话($)', usd(pricing.typical_message_usd)]
        ];
        return html`<dl class="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-2 px-5 py-4 text-xs">
            ${rows.map(
                ([label, value]) =>
                    html`<dt class="text-muted">${label}</dt>
                        <dd class="font-mono tabular-nums text-ink">${value}</dd>`
            )}
        </dl>`;
    }
}
customElements.define('models-panel', ModelsPanel);
