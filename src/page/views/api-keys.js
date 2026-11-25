import { LitElement, html, nothing } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { message } from '../components.js';
import { modelAbilityColumn } from '../components/model-tags.js';
import { tierIcon } from '../components/tier-icon.js';

const statuses = {
    active: ['有效', 'bg-emerald-50 text-emerald-800'],
    disabled: ['已禁用', 'bg-slate-100 text-slate-700'],
    not_yet_valid: ['未生效', 'bg-blue-50 text-blue-800'],
    expired: ['已过期', 'bg-amber-50 text-amber-900'],
    quota_exceeded: ['已超额', 'bg-red-50 text-red-800']
};
const field = 'h-9 w-full rounded-lg border border-line bg-white px-3 text-[13px] text-ink disabled:opacity-50';
// 与 ui-button primary 同款；表单提交需要原生 submit 按钮（保留浏览器校验与回车提交）。
const submit = 'inline-flex min-h-9 items-center justify-center rounded-lg border border-brand bg-brand px-3 py-2 text-[13px] leading-[18px] font-medium text-white hover:border-[#3d4450] hover:bg-[#3d4450] disabled:cursor-not-allowed disabled:opacity-40';
const link = 'rounded px-1 py-1 text-blue-700 hover:bg-blue-50';
const offset = 8 * 60 * 60;
const toWallClock = seconds => seconds == null ? '' : new Date((seconds + offset) * 1000).toISOString().slice(0, 19);
const toUTC = value => value ? Date.parse(value + 'Z') / 1000 - offset : null;
const displayTime = seconds => toWallClock(seconds).replace('T', ' ');
const credits = value => Number(value).toFixed(4) + ' Credits';
const cost = value => '$' + Number(value).toFixed(6);
const blank = { api_key: '', status: '', validity: '' };
const validityOptions = [['permanent', '永久'], ['period', '期限']];
const statusFilter = [['', '全部状态'], ...Object.entries(statuses).map(([key, [label]]) => [key, label])];
const validityFilter = [['', '全部类型'], ...validityOptions];

class ApiKeysPanel extends LitElement {
    static properties = {
        request: { attribute: false }, items: { state: true }, page: { state: true },
        pageSize: { state: true }, pages: { state: true }, total: { state: true },
        loading: { state: true }, error: { state: true }, dialog: { state: true }, query: { state: true }, togglingId: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, { items: [], page: 1, pageSize: 10, pages: 0, total: 0, loading: false, error: '', dialog: null, filters: {}, query: { ...blank }, togglingId: '' });
    }
    createRenderRoot() { return this; }
    disconnectedCallback() {
        this.clear();
        super.disconnectedCallback();
    }
    clear() {
        this.pendingList = null;
        this.items = [];
        this.filters = {};
        this.query = { ...blank };
        this.togglingId = '';
        const secret = this.querySelector('dialog input[readonly], dialog input[type=password]');
        if (secret) secret.value = '';
        this.querySelector('dialog')?.close();
        this.closeDialog();
    }
    closeDialog() {
        this.pendingCopy = null;
        this.dialog = null;
    }
    setDialog(patch) { this.dialog = { ...this.dialog, ...patch }; }
    async load() {
        if (!this.isConnected) return;
        const pending = (this.pendingList = {});
        this.loading = true;
        this.error = '';
        try {
            const result = await this.request('list', { ...this.filters, page: this.page, page_size: this.pageSize });
            if (!this.isConnected || this.pendingList !== pending) return;
            if (this.page > Math.max(1, result.total_pages)) {
                this.page = Math.max(1, result.total_pages);
                return this.load();
            }
            this.items = result.items;
            this.total = result.total;
            this.pages = result.total_pages;
        } catch (error) {
            if (this.pendingList === pending) {
                this.items = [];
                this.error = error.message;
            }
        } finally {
            if (this.pendingList === pending) this.loading = false;
        }
    }
    setQuery(key, value) { this.query = { ...this.query, [key]: value }; }
    search() {
        this.filters = Object.fromEntries(Object.entries(this.query).filter(([, value]) => value !== ''));
        this.page = 1;
        this.load();
    }
    reset() {
        this.query = { ...blank };
        this.filters = {};
        this.page = 1;
        this.load();
    }
    openForm(row) {
        this.dialog = { type: 'form', row, draft: {
            // 默认并发 10 与后端 ApiKeyStore::create 的默认值保持一致。
            mode: 'random', api_key: '', name: row?.name ?? '', concurrency_limit: row?.concurrency_limit ?? 10,
            validity: row?.ends_at == null ? 'permanent' : 'period', starts_at: toWallClock(row?.starts_at),
            ends_at: toWallClock(row?.ends_at), quota_kind: row?.quota_kind || 'unlimited', quota_limit: row?.quota_limit || ''
        } };
    }
    setDraft(key, value) { this.setDialog({ draft: { ...this.dialog.draft, [key]: value } }); }
    submitForm(event) {
        event.preventDefault();
        const { row, draft } = this.dialog;
        // 永久 = 没有开始和结束时间：草稿里的时间只在期限模式下生效，切回永久时一并清除。
        const period = draft.validity === 'period';
        const starts = period ? toUTC(draft.starts_at) : null;
        const ends = period ? toUTC(draft.ends_at) : null;
        if (Number.isNaN(starts) || Number.isNaN(ends)) {
            this.setDialog({ error: '请输入有效的日期和时间。' });
            return;
        }
        if (period && !(starts !== null && ends !== null && starts < ends)) {
            this.setDialog({ error: '期限模式须选择开始和结束时间，且开始时间早于结束时间。' });
            return;
        }
        const payload = {
            name: draft.name || null,
            concurrency_limit: Number(draft.concurrency_limit), starts_at: starts, ends_at: ends,
            quota_kind: draft.quota_kind, quota_limit: draft.quota_kind === 'unlimited' ? null : draft.quota_limit
        };
        if (row) payload.id = row.id;
        else {
            payload.mode = draft.mode;
            if (draft.mode === 'custom') payload.api_key = draft.api_key;
        }
        this.save(row ? 'update' : 'create', payload, row ? 'API Key 已更新' : 'API Key 已创建');
    }
    async openModels(row, edit) {
        const pending = (this.dialog = { type: 'models', row, edit, loading: true });
        try {
            const result = await this.request('model-options', { id: row.id });
            if (this.dialog !== pending) return;
            this.setDialog({ ...result, loading: false, selected: new Set(result.items.filter(item => item.selected).map(item => item.id)) });
        } catch (error) {
            if (this.dialog === pending) this.setDialog({ error: error.message, loading: false });
        }
    }
    selectModel(id, checked) {
        const selected = new Set(this.dialog.selected);
        if (checked) selected.add(id);
        else selected.delete(id);
        this.setDialog({ selected });
    }
    selectAll(checked) {
        const { items, selected } = this.dialog;
        this.setDialog({ selected: new Set(items.filter(item => item.offline ? selected.has(item.id) : checked).map(item => item.id)) });
    }
    saveModels(modelIds) {
        this.save('update-models', { id: this.dialog.row.id, model_ids: modelIds }, '模型权限已更新');
    }
    async save(path, body, success) {
        if (this.dialog.busy) return;
        this.setDialog({ busy: true, error: '' });
        const pending = this.dialog;
        try {
            await this.request(path, body);
            if (this.dialog !== pending) return;
            this.closeDialog();
            message(success, 'success');
            if (path === 'create') {
                this.query = { ...blank };
                this.filters = {};
                this.page = 1;
            }
            await this.load();
        } catch (error) {
            if (this.dialog === pending) this.setDialog({ busy: false, error: error.message });
        }
    }
    async copy(row) {
        const pending = (this.pendingCopy = {});
        let copied = false;
        try {
            if (navigator.clipboard?.writeText) {
                await navigator.clipboard.writeText(row.api_key);
                copied = true;
            }
        } catch {}
        if (this.pendingCopy !== pending || this.dialog) return;
        if (!copied) {
            const input = document.createElement('textarea');
            input.value = row.api_key;
            input.readOnly = true;
            input.style.cssText = 'position:fixed;left:-9999px;top:0';
            this.append(input);
            input.select();
            input.setSelectionRange(0, input.value.length);
            try { copied = document.execCommand('copy'); } catch {}
            finally { input.remove(); }
        }
        if (copied) message('API Key 已复制', 'success');
        else this.dialog = { type: 'copy', row };
    }
    columns() {
        const action = (label, click, cls = link, disabled = false) =>
            html`<button type="button" class=${cls + ' disabled:opacity-40'} ?disabled=${disabled} @click=${click}>${label}</button>`;
        // 永久：没有结束时间；开始时间可选，未设置时与结束日期一样显示“永久”。
        const day = seconds => seconds == null ? '永久' : displayTime(seconds);
        return [
            { label: '名称', title: row => row.name, render: row => html`<span class="block max-w-40 truncate">${row.name ?? '—'}</span>` },
            { label: 'API Key', render: row => html`<span class="inline-flex items-center gap-2">
                <code class="rounded bg-slate-100 px-1.5 py-1 font-mono text-ink">${row.api_key_masked}</code>
                <button type="button" class=${link} aria-label="复制 API Key" title="复制 API Key" @click=${() => this.copy(row)}>
                    <svg class="size-4" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
                        <rect x="5.5" y="5.5" width="8" height="8" rx="1.5"></rect><path d="M3 10.5H2.5V4A1.5 1.5 0 0 1 4 2.5h6.5V3"></path>
                    </svg>
                </button></span>` },
            { label: '模型列表', render: row => action(row.model_count == null ? '全部模型' : row.model_count + ' 个模型', () => this.openModels(row, false)) },
            { label: '并发数', class: 'tabular-nums', key: 'concurrency_limit' },
            { label: '已用额度', class: 'tabular-nums', render: row => this.renderUsed(row) },
            { label: '总额度', class: 'tabular-nums', render: row => row.quota_kind === 'credits' ? credits(row.quota_limit)
                : row.quota_kind === 'cost' ? cost(row.quota_limit) : '不限额' },
            { label: '状态', render: row => html`<span class="rounded-full px-2 py-1 font-medium ${statuses[row.status][1]}">${statuses[row.status][0]}</span>` },
            { label: '开始日期', class: 'tabular-nums', render: row => day(row.starts_at) },
            { label: '结束日期', class: 'tabular-nums', render: row => day(row.ends_at) },
            { label: '创建时间', class: 'tabular-nums', render: row => displayTime(row.created_at) },
            { label: '操作', render: row => html`<div class="flex gap-1">
                ${action(row.enabled ? '禁用' : '启用', () => this.toggle(row), link, !!this.togglingId)}
                ${action('编辑', () => this.openForm(row))}
                ${action('配置', () => this.openModels(row, true))}
                ${action('重置', () => this.dialog = { type: 'reset', row })}
                ${action('删除', () => this.dialog = { type: 'delete', row }, 'rounded px-1 py-1 text-red-700 hover:bg-red-50')}
            </div>` }
        ];
    }
    // 一行显示 “Credits / $Cost”，两项都是真实累计值（后端对每个 Key 同时累计两种用量）。
    renderUsed(row) {
        return Number(row.used_credits).toFixed(2) + ' / $' + Number(row.used_cost).toFixed(2);
    }
    async toggle(row) {
        if (this.togglingId) return;
        this.togglingId = row.id;
        try {
            await this.request('update', { id: row.id, enabled: !row.enabled });
            if (!this.isConnected) return;
            message(row.enabled ? 'API Key 已禁用' : 'API Key 已启用', 'success');
            await this.load();
        } catch (error) {
            if (this.isConnected) message(error.message || '操作失败');
        } finally {
            this.togglingId = '';
        }
    }
    renderForm() {
        const { row, draft, busy } = this.dialog;
        return html`<form id="api-key-form" @submit=${event => this.submitForm(event)} class="grid gap-4">
            <fieldset ?disabled=${!!busy} class="grid gap-4">
                ${row ? nothing : html`
                    <div class="grid gap-1 text-xs text-muted"><span>生成方式</span><ui-select class="block" label="生成方式" .disabled=${!!busy}
                        .options=${[['random', '随机生成'], ['custom', '自定义']]} .value=${draft.mode}
                        @select-change=${e => this.setDraft('mode', e.detail.value)}></ui-select></div>
                    ${draft.mode === 'custom' ? html`<label class="grid gap-1 text-xs text-muted">自定义 API Key
                        <input class=${field} type="password" autocomplete="off" minlength="16" maxlength="128" required
                            .value=${draft.api_key} @input=${e => this.setDraft('api_key', e.target.value)}>
                        <span>16 至 128 个字符，不含空格；保存后不可修改。</span></label>` : nothing}`}
                <label class="grid gap-1 text-xs text-muted">名称（可选）
                    <input class=${field} type="text" maxlength="64" .value=${draft.name} @input=${e => this.setDraft('name', e.target.value)}></label>
                <div class="grid gap-3 sm:grid-cols-2">
                    <label class="grid gap-1 text-xs text-muted">并发上限<input class=${field} type="number" min="1" max="4294967295" step="1" required
                        .value=${String(draft.concurrency_limit)} @input=${e => this.setDraft('concurrency_limit', e.target.value)}></label>
                    <div class="grid gap-1 text-xs text-muted"><span>有效期类型</span><ui-select class="block" label="有效期类型" .disabled=${!!busy}
                        .options=${validityOptions} .value=${draft.validity}
                        @select-change=${e => this.setDraft('validity', e.detail.value)}></ui-select></div>
                    ${draft.validity === 'period' ? html`
                        <ui-date-picker class="block [&_input]:w-full" .disabled=${!!busy} .value=${draft.starts_at} label="开始时间"
                            @date-change=${e => this.setDraft('starts_at', e.detail.value)}></ui-date-picker>
                        <ui-date-picker class="block [&_input]:w-full" .disabled=${!!busy} .value=${draft.ends_at} label="结束时间"
                            @date-change=${e => this.setDraft('ends_at', e.detail.value)}></ui-date-picker>` : nothing}
                    <div class="grid gap-1 text-xs text-muted"><span>额度类型</span><ui-select class="block" label="额度类型" .disabled=${!!busy}
                        .options=${[['unlimited', '不限'], ['credits', 'Credits'], ['cost', 'Cost']]} .value=${draft.quota_kind}
                        @select-change=${e => this.setDraft('quota_kind', e.detail.value)}></ui-select></div>
                    ${draft.quota_kind !== 'unlimited' ? html`<label class="grid gap-1 text-xs text-muted">总额度（${draft.quota_kind === 'credits' ? 'Credits' : 'Cost'}）
                        <input class=${field} type="text" inputmode="decimal" pattern="[0-9]+([.][0-9]{1,9})?" required
                            .value=${draft.quota_limit} @input=${e => this.setDraft('quota_limit', e.target.value)}></label>` : nothing}
                </div>
            </fieldset>
        </form>`;
    }
    renderModels() {
        const { items, selected, edit, loading, unrestricted, busy } = this.dialog;
        if (loading) return html`<p role="status" class="text-xs text-muted">正在加载模型…</p>`;
        if (!items) return nothing;
        const rows = edit ? items : items.filter(item => item.selected);
        const available = items.filter(item => !item.offline);
        const picked = available.filter(item => selected.has(item.id)).length;
        const columns = [
            ...(edit ? [{
                label: html`<input type="checkbox" aria-label="全选当前可用模型" title="全选当前可用模型"
                    .checked=${available.length > 0 && picked === available.length} .indeterminate=${picked > 0 && picked < available.length}
                    ?disabled=${!!busy || !available.length} @change=${e => this.selectAll(e.target.checked)}>`,
                render: item => html`<input type="checkbox" aria-label=${'选择模型 ' + item.id} .checked=${selected.has(item.id)}
                    ?disabled=${!!busy} @change=${e => this.selectModel(item.id, e.target.checked)}>`
            }] : []),
            { label: '模型 ID', class: 'font-mono', render: item => html`<div class="flex items-center gap-1">
                ${item.in_better_ai_subscription == null ? nothing : tierIcon(item.in_better_ai_subscription)}
                <ui-tooltip class="flex-1" .content=${item.id}></ui-tooltip></div>` },
            { label: '名称', render: item => html`<ui-tooltip .content=${item.name || '—'}></ui-tooltip>` },
            { ...modelAbilityColumn, class: 'api-key-model-abilities' },
            { label: '全局别名', render: item => html`<ui-tooltip .content=${item.aliases.map(alias => alias.alias).join('、') || '—'}></ui-tooltip>` }
        ];
        return html`<p class="mb-3 text-xs leading-5 text-muted">${edit
            ? '全选只包含当前目录中的模型。保存白名单后，新上线的模型需要手动添加；恢复不限制后会自动可用。'
            : (unrestricted ? '不限制，当前可用 ' : '白名单，共配置 ') + rows.length + ' 个模型。'}</p>
            ${edit ? html`<p class="mb-2 text-xs text-muted">${unrestricted ? '当前不限制 · ' : ''}已勾选 ${selected.size} 个模型</p>` : nothing}
            <ui-table class="api-key-model-table max-h-[50dvh]" caption="API Key 可用模型" .columns=${columns} .rows=${rows} empty="当前没有模型"></ui-table>`;
    }
    renderDialog() {
        const dialog = this.dialog;
        const busy = !!dialog?.busy;
        const cancel = html`<ui-button class="inline-flex" label="取消" @button-click=${() => this.closeDialog()}></ui-button>`;
        let title, content, footer = nothing;
        if (dialog?.type === 'form') {
            title = dialog.row ? '编辑 API Key' : '新增 API Key';
            content = this.renderForm();
            footer = html`${cancel}<button type="submit" form="api-key-form" class=${submit} ?disabled=${busy}>${busy ? '保存中…' : dialog.row ? '保存修改' : '创建 API Key'}</button>`;
        } else if (dialog?.type === 'models') {
            title = dialog.edit ? '配置可用模型' : '可用模型预览';
            content = this.renderModels();
            if (dialog.edit && dialog.items) footer = html`
                <ui-button class="inline-flex" label="恢复不限制" .disabled=${busy || dialog.unrestricted} @button-click=${() => this.saveModels(null)}></ui-button>
                <ui-button class="inline-flex" variant="primary" label="保存白名单" .disabled=${busy || !dialog.selected.size}
                    @button-click=${() => this.saveModels([...dialog.selected])}></ui-button>`;
        } else if (dialog?.type === 'copy') {
            title = '手动复制 API Key';
            content = html`<label class="grid gap-2 text-xs text-muted">自动复制不可用，请选中下方 Key 手动复制。
                <input class="${field} font-mono" readonly autocomplete="off" .value=${dialog.row.api_key} @focus=${e => e.target.select()}></label>`;
        } else if (dialog) {
            const revoke = dialog.type === 'delete';
            title = revoke ? '删除 API Key' : '重置用量';
            content = html`<p class="text-[13px] leading-6">${revoke
                ? '删除后，该 Key 无法发起新请求。在途请求可继续，历史记录会保留。'
                : '将 Credits 和 Cost 的已用额度清零。历史记录会保留；之后完成的在途请求仍会计入。'}</p>`;
            footer = html`${cancel}<ui-button class="inline-flex" variant=${revoke ? 'danger' : 'primary'} label=${revoke ? '确认删除' : '确认重置'}
                .disabled=${busy} @button-click=${() => this.save(revoke ? 'delete' : 'reset-usage', { id: dialog.row.id }, revoke ? 'API Key 已删除' : '用量已重置')}></ui-button>`;
        }
        return html`<ui-dialog
            .open=${!!dialog}
            .heading=${title || ''}
            .subtitle=${dialog?.row?.api_key_masked || ''}
            .size=${dialog?.type === 'models' ? '5xl' : 'lg'}
            @dialog-close=${this.closeDialog}
            .content=${dialog ? html`
                ${dialog.error ? html`<p role="alert" class="mb-3 text-xs text-red-700">${dialog.error}</p>` : nothing}
                ${content}
            ` : nothing}
            .footer=${footer}
        ></ui-dialog>`;
    }
    render() {
        return html`<div class="mx-auto flex h-full min-h-0 w-full flex-col px-4 pt-4 max-[700px]:px-5">
            <div class="mb-4 flex shrink-0 flex-wrap items-center gap-3">
                <div class="flex min-w-48 flex-1 items-center gap-3">
                    <label for="api-key-filter" class="shrink-0 whitespace-nowrap text-xs text-muted">API Key</label>
                    <input id="api-key-filter" class=${field} type="password" autocomplete="off" placeholder="输入完整 Key 精确匹配"
                        .value=${this.query.api_key} @input=${e => this.setQuery('api_key', e.target.value)}
                        @keydown=${e => e.key === 'Enter' && !this.loading && this.search()}>
                </div>
                <div class="flex items-center gap-3">
                    <span class="shrink-0 whitespace-nowrap text-xs text-muted">状态</span>
                    <ui-select class="block w-40" label="状态" .options=${statusFilter} .value=${this.query.status}
                        .disabled=${this.loading} @select-change=${e => this.setQuery('status', e.detail.value)}></ui-select>
                </div>
                <div class="flex items-center gap-3">
                    <span class="shrink-0 whitespace-nowrap text-xs text-muted">有效期类型</span>
                    <ui-select class="block w-40" label="有效期类型" .options=${validityFilter} .value=${this.query.validity}
                        .disabled=${this.loading} @select-change=${e => this.setQuery('validity', e.detail.value)}></ui-select>
                </div>
                <div class="flex items-center gap-2">
                    <ui-button class="inline-flex" variant="primary" label=${this.loading ? '查询中…' : '查询'}
                        .disabled=${this.loading} @button-click=${this.search}></ui-button>
                    <ui-button class="inline-flex" label="重置" .disabled=${this.loading} @button-click=${this.reset}></ui-button>
                    <ui-button class="inline-flex" label="新增" @button-click=${() => this.openForm()}></ui-button>
                </div>
            </div>
            ${this.error ? html`<p role="alert" class="mb-3 shrink-0 text-sm text-red-700">${this.error}</p>` : nothing}
            <ui-table class="min-h-0 flex-1" caption="托管 API Key" pagination .rows=${this.items} .columns=${this.columns()}
                .loading=${this.loading} .empty=${this.loading ? '正在加载 API Key…' : '没有符合条件的 API Key'}
                .page=${this.page} .pages=${this.pages} .pageSize=${this.pageSize} .total=${this.total}
                @page-change=${e => { this.page = e.detail.page; this.load(); }}
                @page-size-change=${e => { this.pageSize = e.detail.pageSize; this.page = 1; this.load(); }}></ui-table>
            ${this.renderDialog()}
        </div>`;
    }
}
customElements.define('api-keys-panel', ApiKeysPanel);
