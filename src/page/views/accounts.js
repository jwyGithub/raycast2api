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

const filters = [
    ['all', '全部状态'],
    ['available', '可用'],
    ['cooldown', '冷却中'],
    ['unavailable', '不可用']
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
        importing: { state: true }
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
            importing: false
        });
    }
    createRenderRoot() {
        return this;
    }
    connectedCallback() {
        super.connectedCallback();
        // 列表 60s 静默轮询，仅账号面板可见且未操作时刷新。
        this.pollTimer = setInterval(() => {
            if (!this.active || this.workingEmail || this.dialog) return;
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
    render() {
        return html`<section class="min-h-0 flex-1 flex flex-col pt-4" aria-label="账号列表">
            <div class="mx-auto min-h-0 flex-1 flex flex-col w-full">
                <div
                    class="mb-4 shrink-0 px-8 max-[700px]:px-5 flex items-center justify-between gap-3 max-[700px]:flex-wrap"
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
            : ''}`;
    }
}
customElements.define('accounts-panel', AccountsPanel);
