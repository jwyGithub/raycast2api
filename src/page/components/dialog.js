import { LitElement, html, nothing } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';

const widths = { sm: 'max-w-sm', lg: 'max-w-lg', xl: 'max-w-xl', '4xl': 'max-w-4xl', '5xl': 'max-w-5xl', '7xl': 'max-w-7xl' };

class UiDialog extends LitElement {
    // content、toolbar、footer 接收 Lit 模板，页面事件用箭头函数保留页面实例。
    static properties = {
        open: { type: Boolean },
        heading: { type: String },
        subtitle: { type: String },
        describedBy: { type: String },
        size: { type: String },
        dismissible: { type: Boolean },
        closeOnBackdrop: { type: Boolean },
        content: { attribute: false },
        toolbar: { attribute: false },
        footer: { attribute: false }
    };
    static nextId = 0;
    constructor() {
        super();
        Object.assign(this, {
            open: false, heading: '', subtitle: '', describedBy: '', size: 'lg', dismissible: true,
            closeOnBackdrop: false, content: nothing, toolbar: nothing, footer: nothing
        });
        this.headingId = 'ui-dialog-title-' + ++UiDialog.nextId;
    }
    // 使用 Light DOM，让全局 Tailwind 样式和表单关联在弹窗内继续生效。
    createRenderRoot() { return this; }
    updated() {
        const dialog = this.querySelector('dialog');
        if (this.open && !dialog.open) dialog.showModal();
        else if (!this.open && dialog.open) dialog.close();
    }
    disconnectedCallback() {
        this.querySelector('dialog')?.close();
        super.disconnectedCallback();
    }
    close() {
        if (!this.dismissible || !this.open) return;
        this.open = false;
        this.dispatchEvent(new CustomEvent('dialog-close', { bubbles: true, composed: true }));
    }
    onBackdropClick(event) {
        if (!this.closeOnBackdrop || event.target !== event.currentTarget) return;
        const { left, right, top, bottom } = event.currentTarget.getBoundingClientRect();
        if (event.clientX < left || event.clientX > right || event.clientY < top || event.clientY > bottom) this.close();
    }
    render() {
        const hasFooter = this.footer !== nothing && this.footer != null && this.footer !== '';
        return html`<dialog
            class="m-auto max-h-[calc(100dvh-32px)] w-[calc(100%-32px)] ${widths[this.size] || widths.lg} overflow-hidden rounded-xl border border-line bg-white p-0 text-ink shadow-xl backdrop:bg-slate-950/40"
            aria-labelledby=${this.headingId}
            aria-describedby=${this.describedBy || (this.subtitle ? this.headingId + '-subtitle' : nothing)}
            @cancel=${event => { event.preventDefault(); this.close(); }}
            @close=${() => { if (this.isConnected) this.close(); }}
            @click=${event => this.onBackdropClick(event)}
        >
            ${this.open ? html`<div class="flex max-h-[calc(100dvh-34px)] flex-col">
                <header class="flex shrink-0 items-center justify-between gap-3 border-b border-line px-5 py-3">
                    <div class="min-w-0">
                        <h2 id=${this.headingId} class="truncate text-sm font-semibold">${this.heading}</h2>
                        ${this.subtitle ? html`<p id=${this.headingId + '-subtitle'} class="truncate font-mono text-[11px] text-muted">${this.subtitle}</p>` : nothing}
                    </div>
                    ${this.dismissible ? html`<button type="button" class="size-8 shrink-0 rounded-lg text-lg text-muted hover:bg-slate-100"
                        aria-label="关闭弹窗" @click=${() => this.close()}>×</button>` : nothing}
                </header>
                ${this.toolbar}
                <div class="min-h-0 flex-1 overflow-auto p-5">${this.content}</div>
                ${hasFooter ? html`<footer class="flex shrink-0 flex-wrap justify-end gap-2 border-t border-line px-5 py-3">${this.footer}</footer>` : nothing}
            </div>` : nothing}
        </dialog>`;
    }
}
customElements.define('ui-dialog', UiDialog);
