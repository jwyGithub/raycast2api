import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import './views/accounts.js';
import './views/proxies.js';
import './views/api-keys.js';
import './views/logs.js';
import './views/models.js';
import './views/statistics.js';
import { message } from './components/message.js';
import './components/dialog.js';

class AccountConsole extends LitElement {
    static properties = {
        panel: { state: true },
        authenticated: { state: true },
        restoringSession: { state: true },
        dashboardPassword: { state: true },
        loginError: { state: true },
        loggingIn: { state: true }
    };
    constructor() {
        super();
        Object.assign(this, {
            panel: 'accounts',
            authenticated: false,
            restoringSession: true,
            dashboardPassword: '',
            loginError: '',
            loggingIn: false
        });
        this.handlePageShow = event => {
            if (event.persisted) this.restoreSession();
        };
        this.handlePageHide = () => this.lockConsole('', true);
    }
    createRenderRoot() {
        return this;
    }
    connectedCallback() {
        super.connectedCallback();
        window.addEventListener('pageshow', this.handlePageShow);
        window.addEventListener('pagehide', this.handlePageHide);
        this.restoreSession();
    }
    disconnectedCallback() {
        super.disconnectedCallback();
        window.removeEventListener('pageshow', this.handlePageShow);
        window.removeEventListener('pagehide', this.handlePageHide);
        this.lockConsole('', true);
    }
    lockConsole(error = '', restoringSession = false) {
        this.authenticated = false;
        this.restoringSession = restoringSession;
        this.dashboardPassword = '';
        this.loginAttempt = null;
        this.sessionEpoch = {};
        this.querySelector('api-keys-panel')?.clear();
        this.querySelector('proxies-panel')?.clear();
        this.querySelector('accounts-panel')?.closeProxy();
        this.loggingIn = false;
        this.querySelector('logs-panel')?.closeDetail();
        this.querySelector('models-panel')?.closeDetail();
        this.querySelector('statistics-panel')?.closeDetail();
        this.querySelectorAll('ui-date-picker').forEach(picker => picker.picker?.close());
        this.querySelectorAll('ui-tooltip').forEach(tooltip => tooltip.hide());
        this.loginError = error;
        this.panel = 'accounts';
    }
    async restoreSession() {
        const attempt = (this.loginAttempt = {});
        this.restoringSession = true;
        this.loggingIn = true;
        try {
            const response = await fetch('/v1/session', {
                method: 'POST',
                headers: { 'X-Dashboard-Request': '1' },
                credentials: 'same-origin',
                cache: 'no-store'
            });
            if (this.loginAttempt !== attempt || response.status === 401) return;
            const result = await response.json();
            if (this.loginAttempt !== attempt) return;
            if (!response.ok) throw new Error(result.message || '会话验证失败，请稍后重试。');
            this.authenticated = true;
            await this.updateComplete;
            await this.querySelector('accounts-panel').load();
        } catch (error) {
            if (this.loginAttempt === attempt) this.loginError = error.message || '会话验证失败。';
        } finally {
            if (this.loginAttempt === attempt) {
                this.restoringSession = false;
                this.loggingIn = false;
            }
        }
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
                credentials: 'same-origin',
                body: JSON.stringify({ dashboard_password: this.dashboardPassword }),
                cache: 'no-store'
            });
            const result = await response.json().catch(() => ({}));
            if (this.loginAttempt !== attempt) return;
            if (!response.ok || result.code >= 400) throw new Error(result.message || '登录失败，请稍后重试。');
            if (!result.data || typeof result.data.expires_in !== 'number')
                throw new Error('登录响应无效，请重试。');
            this.dashboardPassword = '';
            this.authenticated = true;
            await this.updateComplete;
            await this.querySelector('accounts-panel').load();
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
    async logout() {
        try {
            const response = await fetch('/v1/logout', {
                method: 'POST',
                headers: { 'X-Dashboard-Request': '1' },
                credentials: 'same-origin',
                cache: 'no-store'
            });
            if (!response.ok && response.status !== 401) throw new Error('退出失败，请稍后重试。');
            this.lockConsole();
        } catch (error) {
            message(error.message || '退出失败，请检查网络。');
        }
    }
    async request(path, body, resource = 'accounts') {
        if (!this.authenticated) throw new Error('请先输入控制台密码登录。');
        const session = this.sessionEpoch;
        const response = await fetch('/v1/' + resource + '/' + path, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', 'X-Dashboard-Request': '1' },
            credentials: 'same-origin',
            cache: 'no-store',
            body: JSON.stringify(body)
        });
        const result = await response.json().catch(() => ({}));
        if (!this.authenticated || this.sessionEpoch !== session) throw new Error('会话已结束。');
        if (response.status === 401) {
            this.lockConsole('登录已失效，请重新输入控制台密码。');
            throw new Error('登录已失效，请重新输入控制台密码。');
        }
        if (!response.ok || result.code >= 400) throw new Error(result.message || '请求失败 (' + response.status + ')');
        return result.data;
    }
    async switchPanel(panel) {
        this.querySelectorAll('ui-date-picker').forEach(picker => picker.picker?.close());
        this.panel = panel;
        await this.updateComplete;
        if (!this.authenticated || this.panel !== panel) return;
        if (panel === 'proxies') this.querySelector('proxies-panel')?.load();
        if (panel === 'api-keys') this.querySelector('api-keys-panel')?.load();
        if (panel === 'logs') this.querySelector('logs-panel').load();
        if (panel === 'models') this.querySelector('models-panel').load();
        if (panel === 'statistics') this.querySelector('statistics-panel').load();
    }
    render() {
        if (!this.authenticated && this.restoringSession)
            return html`<div role="status" class="grid h-full place-items-center text-sm text-muted">正在恢复会话…</div>`;
        if (!this.authenticated)
            return html`<ui-dialog
                id="dashboard-login"
                .open=${true}
                heading="登录管理控制台"
                describedBy="login-description"
                size="sm"
                .dismissible=${false}
                .content=${html`<form @submit=${event => this.login(event)}>
                    <p id="login-description" class="text-xs leading-5 text-muted">请输入控制台密码以继续。</p>
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
                </form>`}
            ></ui-dialog>`;
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
                    <nav class="flex max-w-full flex-wrap justify-self-center gap-1 rounded-lg bg-slate-100 p-1" aria-label="面板切换">
                        ${[
                            ['accounts', '账号面板'],
                            ['models', '模型面板'],
                            ['proxies', '代理管理'],
                            ['api-keys', 'API Key'],
                            ['statistics', '统计面板'],
                            ['logs', '日志面板']
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
                    <button class="justify-self-end text-xs text-muted hover:text-ink" @click=${this.logout}>退出登录</button>
                </div>
            </header>
            <accounts-panel
                class="min-h-0 flex-1 flex-col ${this.panel === 'accounts' ? 'flex' : 'hidden'}"
                ?inert=${this.panel !== 'accounts'}
                .active=${this.panel === 'accounts'}
                .request=${(path, body, resource) => this.request(path, body, resource)}
            ></accounts-panel>
            <logs-panel
                class="min-h-0 flex-1 overflow-hidden ${this.panel === 'logs' ? 'block' : 'hidden'}"
                ?inert=${this.panel !== 'logs'}
                .request=${(path, body) => this.request(path, body, 'logs')}
            ></logs-panel>
            <models-panel
                class="min-h-0 flex-1 overflow-hidden ${this.panel === 'models' ? 'block' : 'hidden'}"
                ?inert=${this.panel !== 'models'}
                .request=${(path, body) => this.request(path, body, 'models')}
            ></models-panel>
            ${this.panel === 'proxies' ? html`<proxies-panel
                class="block min-h-0 flex-1 overflow-hidden"
                .request=${(path, body) => this.request(path, body, 'proxies')}
            ></proxies-panel>` : ''}
            ${this.panel === 'api-keys' ? html`<api-keys-panel
                class="block min-h-0 flex-1 overflow-hidden"
                .request=${(path, body) => this.request(path, body, 'api-keys')}
            ></api-keys-panel>` : ''}
            <statistics-panel
                class="min-h-0 flex-1 overflow-hidden ${this.panel === 'statistics' ? 'block' : 'hidden'}"
                ?inert=${this.panel !== 'statistics'}
                .request=${(path, body) => this.request(path, body, 'statistics')}
            ></statistics-panel>
        </main>`;
    }
}
customElements.define('account-console', AccountConsole);
