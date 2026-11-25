import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { message } from '../components.js';
import { formatDate } from '../utils.js';

class ProxiesPanel extends LitElement {
    static properties = {
        request: { attribute: false }, proxies: { state: true }, loading: { state: true },
        busy: { state: true }, dialog: { state: true }, editingId: { state: true }, form: { state: true },
        formTest: { state: true }, exitDialog: { state: true }, error: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, { proxies: [], loading: false, busy: '', dialog: false, editingId: '', form: {}, formTest: null, exitDialog: null, error: '' });
    }
    createRenderRoot() { return this; }
    clear() {
        this.epoch = {};
        this.proxies = [];
        this.form = {};
        this.formTest = null;
        this.closeExit();
        this.dialog = false;
        this.busy = '';
    }
    disconnectedCallback() { this.clear(); super.disconnectedCallback(); }
    async load() {
        const epoch = this.epoch = {};
        this.loading = true;
        this.error = '';
        try {
            const proxies = await this.request('list', {});
            if (this.isConnected && this.epoch === epoch) this.proxies = proxies || [];
        } catch (error) {
            if (this.isConnected && this.epoch === epoch) {
                this.error = error.message || '加载代理失败';
                message(this.error);
            }
        } finally { if (this.epoch === epoch) this.loading = false; }
    }
    edit(proxy) {
        this.editingId = proxy?.id || '';
        this.form = proxy ? { type: proxy.type, host: proxy.host, port: proxy.port, user_name: proxy.user_name, password: proxy.password }
            : { type: 'http', host: '', port: '', user_name: '', password: '' };
        this.formTest = null;
        this.dialog = true;
    }
    setField(name, value) {
        this.form = { ...this.form, [name]: value };
        this.formTest = null;
    }
    closeDialog() {
        if (this.busy) return;
        this.dialog = false;
        this.form = {};
        this.formTest = null;
    }
    validatedForm() {
        const host = (this.form.host || '').trim();
        const port = Number(this.form.port);
        if (!host || !Number.isInteger(port) || port < 1 || port > 65535) {
            message('请填写 HOST 和有效 PORT（1–65535）'); return null;
        }
        return { ...this.form, host, port };
    }
    testMessage(result) {
        const exit = result.exit_ip ? '，出口：' + result.exit_ip + (result.exit_location ? '（' + result.exit_location + '）' : '')
            : result.exit_error ? '，' + result.exit_error : '';
        return '代理延迟：' + result.latency_ms + ' ms' + exit;
    }
    async testDraft() {
        if (this.busy) return;
        const input = this.validatedForm();
        if (!input) return;
        this.busy = 'test-config';
        this.formTest = null;
        const epoch = this.epoch;
        const form = this.form;
        try {
            const result = await this.request('test-config', input);
            if (!this.isConnected || this.epoch !== epoch || !this.dialog || this.form !== form) return;
            this.formTest = result;
            message(this.testMessage(result), result.exit_error ? 'error' : 'success');
        } catch (error) {
            if (this.isConnected && this.epoch === epoch && this.dialog) message(error.message || '测试失败');
        } finally { if (this.epoch === epoch && this.busy === 'test-config') this.busy = ''; }
    }
    async save() {
        if (this.busy) return;
        const input = this.validatedForm();
        if (!input) return;
        this.busy = 'save';
        const epoch = this.epoch;
        try {
            await this.request(this.editingId ? 'update' : 'create', {
                ...input,
                ...(this.editingId ? { id: this.editingId } : {})
            });
            if (!this.isConnected || this.epoch !== epoch) return;
            this.dialog = false;
            this.form = {};
            this.formTest = null;
            message('代理已保存', 'success');
            await this.load();
        } catch (error) { if (this.isConnected && this.epoch === epoch) message(error.message || '保存失败'); }
        finally { this.busy = ''; }
    }
    async action(proxy, action) {
        if (this.busy) return;
        if (action === 'delete' && !confirm('确定删除代理 ' + proxy.host + ':' + proxy.port + ' 吗？')) return;
        this.busy = proxy.id;
        const epoch = this.epoch;
        try {
            const result = await this.request(action, { id: proxy.id });
            if (!this.isConnected || this.epoch !== epoch) return;
            if (action === 'test') {
                this.proxies = this.proxies.map(item => item.id === proxy.id ? {
                    ...item, latency_ms: result.latency_ms, tested_at: result.tested_at,
                    exit_ip: result.exit_ip ?? null, exit_location: result.exit_location ?? null,
                    exit_details: result.exit_details ?? null
                } : item);
                message(this.testMessage(result), result.exit_error ? 'error' : 'success');
            } else { message('代理已删除', 'success'); await this.load(); }
        } catch (error) {
            if (this.isConnected && this.epoch === epoch) {
                message(error.message || '操作失败');
                if (action === 'test') {
                    try {
                        const proxies = await this.request('list', {});
                        if (this.isConnected && this.epoch === epoch) this.proxies = proxies || [];
                    } catch { /* 刷新失败保留当前数据，测试错误已提示。 */ }
                }
            }
        }
        finally { this.busy = ''; }
    }
    input(name, label, type = 'text') {
        return html`<label class="block text-xs font-medium">${label}
            <input class="mt-2 w-full rounded-lg border border-line px-3 py-2 text-sm outline-none focus:border-brand"
                type=${type} .value=${String(this.form[name] ?? '')} ?disabled=${!!this.busy}
                min=${type === 'number' ? '1' : ''} max=${type === 'number' ? '65535' : ''}
                autocomplete="off" @input=${e => this.setField(name, e.target.value)}>
        </label>`;
    }
    closeExit() {
        this.exitAttempt = {};
        this.exitDialog = null;
    }
    async openExit(proxy) {
        if (!proxy.exit_ip || this.busy) return;
        const attempt = this.exitAttempt = {};
        const epoch = this.epoch;
        this.exitDialog = { ip: proxy.exit_ip, details: proxy.exit_details || null, loading: !proxy.exit_details, error: '' };
        if (proxy.exit_details) return;
        if (!proxy.id) {
            this.exitDialog = { ...this.exitDialog, loading: false, error: '暂无详情，请重新测试' };
            return;
        }
        try {
            const details = await this.request('exit-details', { id: proxy.id });
            if (!this.isConnected || this.epoch !== epoch || this.exitAttempt !== attempt) return;
            this.exitDialog = { ...this.exitDialog, details, loading: false };
            this.proxies = this.proxies.map(item => item.id === proxy.id && item.exit_ip === proxy.exit_ip ? { ...item, exit_details: details } : item);
        } catch (error) {
            if (this.isConnected && this.epoch === epoch && this.exitAttempt === attempt)
                this.exitDialog = { ...this.exitDialog, loading: false, error: error.message || '加载出口详情失败' };
        }
    }
    renderExitIp(proxy) {
        return proxy.exit_ip ? html`<button type="button"
            class="font-mono text-blue-600 underline-offset-4 hover:underline focus-visible:outline focus-visible:outline-2 focus-visible:outline-brand disabled:cursor-not-allowed disabled:opacity-40"
            ?disabled=${!!this.busy} @click=${() => this.openExit(proxy)}>${proxy.exit_ip}</button>` : html`<span class="text-muted">—</span>`;
    }
    renderExit(proxy) {
        return proxy.exit_ip ? html`<div class="flex flex-col gap-1">
            ${this.renderExitIp(proxy)}
            <span class="text-[11px] text-muted">${proxy.exit_location || '未知地区'}</span>
        </div>` : html`<span class="text-muted">—</span>`;
    }
    renderExitDetails() {
        const dialog = this.exitDialog;
        if (!dialog) return '';
        if (dialog.loading) return html`<p class="text-xs text-muted" role="status">正在加载出口详情…</p>`;
        if (dialog.error) return html`<p class="text-xs text-red-600" role="alert">${dialog.error}</p>`;
        if (!dialog.details) return '';
        const labels = {
            query: 'IP地址', status: '状态', message: '说明', continent: '洲', continentCode: '洲代码',
            country: '国家', countryCode: '国家代码', region: '地区代码', regionName: '地区', city: '城市',
            district: '区县', zip: '邮编', lat: '纬度', lon: '经度', timezone: '时区', offset: '时区偏移',
            currency: '货币', isp: '运营商', org: '组织', as: '自治系统', asname: '自治系统名称',
            reverse: '反向DNS', mobile: '移动网络', proxy: '代理/VPN', hosting: '数据中心'
        };
        return html`<dl class="grid grid-cols-1 gap-x-6 text-xs sm:grid-cols-3">
            ${Object.entries(dialog.details).map(([key, value]) => html`<div class="min-w-0 border-b border-line py-3">
                <dt class="text-muted">${labels[key] || key}${labels[key] ? html`<span class="mt-1 block font-mono text-[10px]">${key}</span>` : ''}</dt>
                <dd class="mt-2 break-words font-mono">${value == null ? '—' : typeof value === 'object' ? JSON.stringify(value) : String(value)}</dd>
            </div>`)}
        </dl>
        <details class="mt-4 rounded-lg border border-line text-xs">
            <summary class="cursor-pointer px-3 py-2 text-muted">原始 JSON</summary>
            <pre class="overflow-auto border-t border-line bg-slate-50 p-3 text-[11px]">${JSON.stringify(dialog.details, null, 2)}</pre>
        </details>`;
    }
    columns() {
        return [
            { label: '序号', class: 'tabular-nums text-muted', render: (_, index) => index + 1 },
            {
                label: '类型',
                render: proxy => html`<span class="inline-flex rounded-md bg-slate-100 px-2 py-1 text-[11px] font-medium text-slate-700">${proxy.type.toUpperCase()}</span>`
            },
            { label: 'HOST', render: proxy => html`<code class="font-mono text-ink">${proxy.host}</code>` },
            { label: 'PORT', key: 'port', class: 'tabular-nums text-muted' },
            { label: 'USER_NAME', render: proxy => proxy.user_name || html`<span class="text-muted">—</span>` },
            { label: 'PASSWORD', render: proxy => proxy.password ? html`<span class="tracking-wider text-muted">••••••••</span>` : html`<span class="text-muted">—</span>` },
            {
                label: '延迟(ms)', class: 'tabular-nums',
                render: proxy => proxy.latency_ms == null ? '' : html`<span class="inline-flex rounded-md bg-emerald-50 px-2 py-1 font-medium text-emerald-700">${proxy.latency_ms} ms</span>`
            },
            { label: '出口IP', render: proxy => this.renderExitIp(proxy) },
            { label: '出口地区', key: 'exit_location', class: 'text-muted' },
            { label: '创建时间', class: 'tabular-nums text-muted', render: proxy => formatDate(proxy.created_at) },
            {
                label: '账号',
                render: proxy => html`<span
                    class="inline-flex rounded-md px-2 py-1 font-medium tabular-nums ${proxy.account_count ? 'bg-blue-50 text-blue-700' : 'bg-slate-100 text-muted'}"
                    title="使用此代理的账号数量"
                >${proxy.account_count ?? 0} 个账号</span>`
            },
            {
                label: '操作',
                render: proxy => html`<div class="flex gap-2">
                    <button type="button" class="rounded-md border border-line px-2 py-1 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40"
                        ?disabled=${!!this.busy} @click=${() => this.edit(proxy)}>修改</button>
                    <button type="button" class="rounded-md border border-line px-2 py-1 text-red-600 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40"
                        ?disabled=${!!this.busy} @click=${() => this.action(proxy, 'delete')}>删除</button>
                    <button type="button" class="rounded-md border border-line px-2 py-1 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40"
                        ?disabled=${!!this.busy} @click=${() => this.action(proxy, 'test')}>测试</button>
                </div>`
            }
        ];
    }
    render() {
        return html`<section class="h-full min-h-0 flex flex-col p-4" aria-label="代理管理">
            <div class="mb-4 flex shrink-0 flex-wrap items-center justify-between gap-3">
                <span class="text-xs text-muted">共 <span class="font-medium tabular-nums text-ink">${this.proxies.length}</span> 个代理</span>
                <div class="flex gap-2">
                    <ui-button label=${this.loading ? '刷新中…' : '刷新列表'} .disabled=${this.loading || !!this.busy} @button-click=${() => this.load()}></ui-button>
                    <ui-button label="新增代理" variant="primary" .disabled=${!!this.busy} @button-click=${() => this.edit()}></ui-button>
                </div>
            </div>
            <ui-table
                class="min-h-0 flex-1"
                caption="代理管理列表"
                .columns=${this.columns()}
                .rows=${this.proxies}
                .loading=${this.loading}
                .empty=${this.loading ? '加载中…' : this.error || '暂无代理'}
            ></ui-table>
        </section>
        <ui-dialog .open=${this.dialog} heading=${this.editingId ? '修改代理' : '新增代理'}
            .dismissible=${!this.busy} .closeOnBackdrop=${!this.busy} @dialog-close=${() => this.closeDialog()}
            .content=${this.dialog ? html`<div class="grid gap-4">
                <label class="text-xs font-medium">类型
                    <ui-select class="mt-2 block" label="代理类型" .options=${[['http','http'],['https','https'],['socks5','socks5']]}
                        .value=${this.form.type} .disabled=${!!this.busy} @select-change=${e => this.setField('type', e.detail.value)}></ui-select>
                </label>
                <div class="grid grid-cols-2 gap-4">${this.input('host', 'HOST')}${this.input('port', 'PORT', 'number')}</div>
                ${this.input('user_name', 'USER_NAME（可选）')}${this.input('password', 'PASSWORD（可选）', 'password')}
                ${this.formTest ? html`<div class="rounded-lg bg-slate-50 p-3 text-xs">
                    <div class="flex items-center justify-between gap-3">
                        <span class="text-muted">延迟</span><span class="font-medium tabular-nums text-emerald-700">${this.formTest.latency_ms} ms</span>
                    </div>
                    <div class="mt-3 flex items-start justify-between gap-3">
                        <span class="text-muted">出口</span>${this.renderExit(this.formTest)}
                    </div>
                    ${this.formTest.exit_error ? html`<p class="mt-2 text-red-600">${this.formTest.exit_error}</p>` : ''}
                </div>` : ''}
            </div>` : ''}
            .footer=${html`<ui-button label="取消" .disabled=${!!this.busy} @button-click=${() => this.closeDialog()}></ui-button>
                ${!this.editingId ? html`<ui-button label="测试" .disabled=${!!this.busy} @button-click=${() => this.testDraft()}></ui-button>` : ''}
                <ui-button label=${this.busy === 'save' ? '保存中…' : '保存'} variant="primary" .disabled=${!!this.busy} @button-click=${() => this.save()}></ui-button>`}
        ></ui-dialog>
        <ui-dialog .open=${!!this.exitDialog} heading="出口IP详情" .subtitle=${this.exitDialog?.ip || ''} size="4xl"
            .closeOnBackdrop=${true} @dialog-close=${() => this.closeExit()}
            .content=${this.renderExitDetails()}
            .footer=${html`<ui-button label="关闭" @button-click=${() => this.closeExit()}></ui-button>`}
        ></ui-dialog>`;
    }
}
customElements.define('proxies-panel', ProxiesPanel);
