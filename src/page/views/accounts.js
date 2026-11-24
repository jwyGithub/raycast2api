import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { dayjs, formatDate } from '../utils.js';
import { message } from '../components.js';

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
        const enabled = a.enabled !== false;
        const state = !enabled ? '已停用' : !a.available ? '不可用' : cooling ? '冷却中' : '可用';
        const status = !enabled ? 'disabled' : !a.available ? 'unavailable' : cooling ? 'cooldown' : 'available';
        const error = a.unavailable_reason || a.error || '';
        const action = (name, value) =>
            this.dispatchEvent(
                new CustomEvent('account-action', {
                    detail: { action: name, email: a.email, value },
                    bubbles: true,
                    composed: true
                })
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
                            class="inline-flex shrink-0 items-center gap-[5px] rounded-md px-[7px] py-[3px] text-xs font-medium before:size-[5px] before:rounded-full before:bg-current data-[state=available]:bg-emerald-50 data-[state=available]:text-emerald-700 data-[state=cooldown]:bg-amber-50 data-[state=cooldown]:text-amber-700 data-[state=unavailable]:bg-red-50 data-[state=unavailable]:text-red-700 data-[state=disabled]:bg-slate-100 data-[state=disabled]:text-slate-600"
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
                        class="grid grid-cols-1 gap-y-0.5 [&>div]:flex [&>div]:min-w-0 [&>div]:items-baseline [&>div]:gap-1 [&_dd]:min-w-0 [&_dd]:font-medium [&_dd]:text-ink [&_dt]:w-[5em] [&_dt]:shrink-0 [&_dt]:whitespace-nowrap [&_dt]:text-muted"
                    >
                        <div class="min-w-0">
                            <dt>创建时间：</dt>
                            <dd><ui-tooltip .content=${this.date(a.created_at)}></ui-tooltip></dd>
                        </div>
                        <div class="min-w-0">
                            <dt>额度同步：</dt>
                            <dd><ui-tooltip .content=${this.date(a.info_fetched_at)}></ui-tooltip></dd>
                        </div>
                        <div class="min-w-0">
                            <dt>优先级：</dt>
                            <dd>
                                <span class="inline-flex rounded-md border border-blue-200 bg-blue-50 px-[7px] py-px tabular-nums text-blue-700"
                                    >${a.priority ?? 0}</span
                                >
                            </dd>
                        </div>
                    </dl>
                </div>
                <div
                    class="mt-2 min-w-0 rounded-lg px-2 py-1.5 text-xs leading-[18px] data-[state=available]:bg-emerald-50 data-[state=available]:text-emerald-700 data-[state=cooldown]:bg-amber-50 data-[state=cooldown]:text-amber-700 data-[state=unavailable]:bg-red-50 data-[state=unavailable]:text-red-700 data-[state=disabled]:bg-slate-100 data-[state=disabled]:text-slate-600"
                    data-state=${status}
                >
                    <ui-tooltip
                        .content=${error ||
                        (!enabled
                            ? '已停用，不参与调度'
                            : cooling
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
                    .disabled=${this.busy || !enabled || cooling}
                    @button-click=${() => action('refresh')}
                ></ui-button
                ><ui-button
                    class="inline-flex"
                    label="恢复"
                    .disabled=${this.busy || !enabled || (!cooling && a.available)}
                    @button-click=${() => action('restore')}
                ></ui-button>
                <ui-button
                    class="inline-flex"
                    label=${enabled ? '停用' : '启用'}
                    .disabled=${this.busy}
                    @button-click=${() => action('toggle', !enabled)}
                ></ui-button>
                <ui-button
                    class="inline-flex"
                    label="设置"
                    .disabled=${this.busy}
                    @button-click=${() => action('settings')}
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

// 严格解析优先级:拒绝 "12abc"、"1e3"、小数与超出 i32 的值,非法返回 null。
function parsePriority(raw) {
    const text = String(raw).trim();
    const next = /^-?\d+$/.test(text) ? Number(text) : NaN;
    return Number.isSafeInteger(next) && next >= -2147483648 && next <= 2147483647 ? next : null;
}

const filters = [
    ['all', '全部状态'],
    ['available', '可用'],
    ['cooldown', '冷却中'],
    ['unavailable', '不可用'],
    ['disabled', '已停用']
];

class AccountsPanel extends LitElement {
    static properties = {
        request: { attribute: false },
        active: { type: Boolean },
        pageSize: { state: true },
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
        dragActive: { state: true },
        settingsEmail: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, {
            active: false,
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
            dragActive: false,
            settingsEmail: ''
        });
    }
    createRenderRoot() {
        return this;
    }
    connectedCallback() {
        super.connectedCallback();
        // 列表 60s 静默轮询，仅账号面板可见且未操作时刷新。
        this.pollTimer = setInterval(() => {
            if (!this.active || this.workingEmail || this.dialog || this.settingsEmail) return;
            this.load(true);
        }, 60000);
    }
    disconnectedCallback() {
        super.disconnectedCallback();
        clearInterval(this.pollTimer);
    }
    async load(silent = false) {
        if (!this.isConnected) return;
        if (!silent) {
            this.loading = true;
            this.error = '';
        }
        try {
            const data = await this.request('list', { page: this.page, page_size: this.pageSize, status: this.status });
            if (!this.isConnected) return;
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
    // 读取一个或多个 .json 文件,合并为账号数组写入文本框,由用户确认后再导入。
    async readJsonFiles(fileList) {
        const files = [...(fileList || [])];
        if (!files.length) return;
        if (files.some(file => !/\.json$/i.test(file.name) && file.type !== 'application/json')) {
            message('只支持 .json 文件。');
            return;
        }
        const records = [];
        for (const file of files) {
            try {
                const parsed = JSON.parse(await file.text());
                records.push(...(Array.isArray(parsed) ? parsed : [parsed]));
            } catch {
                message(file.name + ' 不是有效的 JSON 文件。');
                return;
            }
        }
        this.source = JSON.stringify(records, null, 2);
        message('已读取 ' + files.length + ' 个文件，共 ' + records.length + ' 个账号，确认后导入。', 'success');
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
    async saveSettings() {
        const email = this.settingsEmail;
        const input = this.querySelector('#account-priority');
        const value = parsePriority(input?.value ?? '');
        if (value === null) {
            message('优先级必须是 -2147483648 到 2147483647 之间的整数。');
            return;
        }
        const current = this.accounts.find(x => x.email === email)?.priority ?? 0;
        if (value === current) {
            this.settingsEmail = '';
            return;
        }
        if (await this.accountAction({ detail: { action: 'priority', email, value } })) this.settingsEmail = '';
    }
    chooseStatus(e) {
        this.status = e.detail.value;
        this.page = 1;
        this.load();
    }
    async accountAction(e) {
        const { action, email, value } = e.detail;
        if (action === 'settings') {
            this.settingsEmail = email;
            return;
        }
        const resetPriority = () => {
            // Lit 对未变化的绑定值不会重写 DOM,须主动回退未保存的输入。
            const input = this.querySelector(`input[data-priority-email="${CSS.escape(email)}"]`);
            if (input) input.value = String(this.accounts.find(x => x.email === email)?.priority ?? 0);
        };
        if (this.workingEmail) {
            if (action === 'priority') resetPriority();
            message('有操作进行中，请稍后再试。');
            return;
        }
        if (action === 'delete' && !confirm('确定删除账号 ' + email + ' 吗？')) return;
        this.workingEmail = email;
        try {
            if (action === 'toggle' || action === 'priority') {
                const body = action === 'toggle' ? { email, enabled: value } : { email, priority: value };
                await this.request('update', body);
                message(action === 'toggle' ? (value ? '账号已启用' : '账号已停用') : '优先级已更新', 'success');
                await this.load();
                return true;
            }
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
            if (action === 'priority') resetPriority();
            if (action !== 'delete') await this.load(true);
        } finally {
            this.workingEmail = '';
        }
    }
    render() {
        return html`<section class="min-h-0 flex-1 flex flex-col pt-4" aria-label="账号列表">
            <div class="mx-auto min-h-0 flex-1 flex flex-col w-full">
                <div
                    class="mb-4 shrink-0 px-4 flex items-center justify-between gap-3 max-[700px]:flex-wrap"
                >
                    <div class="flex items-center gap-3">
                        <span class="shrink-0 whitespace-nowrap text-xs text-muted">状态</span>
                        <ui-select
                            class="block"
                            label="筛选账号状态"
                            .options=${filters}
                            .value=${this.status}
                            @select-change=${this.chooseStatus}
                        ></ui-select>
                    </div>
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
                    class="[scrollbar-width:thin] [scrollbar-color:#d9dce5_transparent] [&::-webkit-scrollbar]:w-[7px] [&::-webkit-scrollbar-thumb]:rounded-full [&::-webkit-scrollbar-thumb]:bg-[#d9dce5] min-h-0 flex-1 overflow-y-auto px-4"
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
                                                .busy=${Boolean(this.workingEmail)}
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
                <footer class="shrink-0 px-4">
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
        ${this.renderImport()} ${this.renderSettings()}`;
    }
    renderImport() {
        return html`<dialog
            id="account-import-dialog"
            class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-xl overflow-y-auto rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
            aria-labelledby="import-title"
            @close=${this.closeImport}
        >
            <div class="flex shrink-0 items-center justify-between gap-3 border-b border-line px-5 py-3">
                <h2 id="import-title" class="truncate text-sm font-semibold">导入账号</h2>
                <button
                    class="size-8 shrink-0 rounded-lg text-lg text-muted hover:bg-slate-100"
                    aria-label="关闭导入窗口"
                    @click=${this.closeImport}
                >
                    ×
                </button>
            </div>
            <div class="p-5">
                <p class="text-xs leading-5 text-muted">
                    粘贴账号 JSON，或选择 / 拖入一个或多个 .json 文件；支持单个对象或数组，按 user.email 更新账号。
                </p>
                <div class="mt-4 flex items-center justify-between gap-3">
                    <label for="account-json" class="block text-xs font-medium">账号 JSON</label>
                    <label
                        class="inline-flex cursor-pointer items-center rounded-lg border border-line px-3 py-1.5 text-xs hover:bg-slate-50"
                    >
                        选择 JSON 文件
                        <input
                            type="file"
                            accept=".json,application/json"
                            multiple
                            class="sr-only"
                            @change=${e => {
                                this.readJsonFiles(e.target.files);
                                e.target.value = '';
                            }}
                        />
                    </label>
                </div>
                <textarea
                    id="account-json"
                    class="mt-2 h-64 w-full resize-y rounded-lg border border-dashed bg-slate-50 p-3 font-mono text-xs leading-5 outline-none focus:border-brand focus:bg-white max-[700px]:text-base ${this
                        .dragActive
                        ? 'border-brand bg-white'
                        : 'border-line'}"
                    .value=${this.source}
                    @input=${e => (this.source = e.target.value)}
                    @dragover=${e => {
                        e.preventDefault();
                        this.dragActive = true;
                    }}
                    @dragleave=${() => (this.dragActive = false)}
                    @drop=${e => {
                        e.preventDefault();
                        this.dragActive = false;
                        this.readJsonFiles(e.dataTransfer?.files);
                    }}
                    placeholder='[{"tag":"主账号","user":{"email":"name@example.com"},"access_token":"rca_...","refresh_token":"..."}]'
                ></textarea>
            </div>
            <div class="flex shrink-0 flex-wrap justify-end gap-2 border-t border-line px-5 py-3">
                <ui-button class="inline-flex" label="取消" @button-click=${this.closeImport}></ui-button>
                <ui-button
                    class="inline-flex"
                    variant="primary"
                    label=${this.importing ? '导入中…' : '确定导入'}
                    .disabled=${this.importing}
                    @button-click=${this.importAccounts}
                ></ui-button>
            </div>
        </dialog>`;
    }
    closeImport() {
        this.dialog = false;
    }
    renderSettings() {
        const a = this.accounts.find(x => x.email === this.settingsEmail);
        const busy = Boolean(this.workingEmail);
        return html`<dialog
            id="account-settings-dialog"
            class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] max-w-sm overflow-hidden rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
            aria-labelledby="account-settings-title"
            @close=${this.closeSettings}
        >
            ${a
                ? html`<div class="flex shrink-0 items-start justify-between gap-3 border-b border-line px-5 py-3">
                          <div class="min-w-0">
                              <h2 id="account-settings-title" class="truncate text-sm font-semibold">优先级设置</h2>
                              <p class="truncate font-mono text-[11px] text-muted">${a.email}</p>
                          </div>
                          <button
                              class="size-8 shrink-0 rounded-lg text-lg text-muted hover:bg-slate-100"
                              aria-label="关闭优先级设置"
                              @click=${this.closeSettings}
                          >
                              ×
                          </button>
                      </div>
                      <div class="p-5">
                          <div class="flex items-center gap-1">
                              <label for="account-priority" class="shrink-0 whitespace-nowrap text-xs text-muted">优先级：</label>
                              <input
                                  id="account-priority"
                                  type="number"
                                  step="1"
                                  min="-2147483648"
                                  max="2147483647"
                                  data-priority-email=${a.email}
                                  class="h-9 min-w-0 flex-1 rounded-lg border border-line bg-white px-3 text-[13px] tabular-nums text-ink disabled:opacity-50"
                                  .value=${String(a.priority ?? 0)}
                                  ?disabled=${busy}
                                  @keydown=${e => e.key === 'Enter' && this.saveSettings()}
                              />
                          </div>
                      </div>
                      <div class="flex shrink-0 flex-wrap justify-end gap-2 border-t border-line px-5 py-3">
                          <ui-button class="inline-flex" label="取消" @button-click=${this.closeSettings}></ui-button>
                          <ui-button
                              class="inline-flex"
                              variant="primary"
                              label=${this.workingEmail === a.email ? '保存中…' : '保存'}
                              .disabled=${busy}
                              @button-click=${this.saveSettings}
                          ></ui-button>
                      </div>`
                : ''}
        </dialog>`;
    }
    closeSettings() {
        this.settingsEmail = '';
    }
    updated() {
        // 账号被删除或被筛选掉后,不留下没有内容的弹窗。
        if (this.settingsEmail && !this.accounts.some(x => x.email === this.settingsEmail)) {
            this.settingsEmail = '';
        }
        for (const [id, wanted] of [
            ['account-import-dialog', this.dialog],
            ['account-settings-dialog', this.settingsEmail]
        ]) {
            const dialog = this.querySelector('#' + id);
            if (!dialog) continue;
            if (wanted && !dialog.open) dialog.showModal();
            else if (!wanted && dialog.open) dialog.close();
        }
    }
}
customElements.define('accounts-panel', AccountsPanel);
