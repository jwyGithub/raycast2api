import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';

class UiButton extends LitElement {
    static properties = { label: { type: String }, variant: { type: String }, disabled: { type: Boolean } };
    createRenderRoot() {
        return this;
    }
    render() {
        return html`<button
            type="button"
            class="inline-flex min-h-9 items-center justify-center gap-2 rounded-lg border px-3 py-2 text-[13px] leading-[18px] font-medium disabled:cursor-not-allowed disabled:opacity-40 ${this
                .variant === 'danger'
                ? 'border-red-200 text-red-600 hover:bg-red-50'
                : this.variant === 'primary'
                  ? 'border-brand bg-brand text-white hover:border-[#3d4450] hover:bg-[#3d4450]'
                  : 'border-line text-ink hover:bg-slate-50'}"
            ?disabled=${this.disabled}
            @click=${() => this.dispatchEvent(new CustomEvent('button-click', { bubbles: true, composed: true }))}
        >
            ${this.label}
        </button>`;
    }
}
customElements.define('ui-button', UiButton);
