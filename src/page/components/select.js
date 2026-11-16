import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { dayjs } from '../utils.js';

class UiSelect extends LitElement {
    static properties = {
        options: { attribute: false },
        value: { type: String },
        label: { type: String },
        disabled: { type: Boolean },
        expanded: { state: true },
        activeIndex: { state: true }
    };
    static nextId = 0;
    constructor() {
        super();
        this.options = [];
        this.label = '请选择';
        this.expanded = false;
        this.activeIndex = -1;
        this.listId = 'select-options-' + ++UiSelect.nextId;
        this.closeOnViewportChange = event => {
            if (!this.querySelector('[role=listbox]')?.contains(event.target)) this.close();
        };
    }
    createRenderRoot() {
        return this;
    }
    connectedCallback() {
        super.connectedCallback();
        window.addEventListener('resize', this.closeOnViewportChange);
        window.addEventListener('scroll', this.closeOnViewportChange, true);
    }
    disconnectedCallback() {
        window.removeEventListener('resize', this.closeOnViewportChange);
        window.removeEventListener('scroll', this.closeOnViewportChange, true);
        this.close();
        super.disconnectedCallback();
    }
    updated() {
        if (this.disabled) this.close();
    }
    close() {
        const list = this.querySelector('[role=listbox]');
        if (list?.matches(':popover-open')) list.hidePopover();
        this.expanded = false;
    }
    async open(index) {
        if (this.disabled || !this.options.length) return;
        this.activeIndex =
            index ??
            Math.max(
                0,
                this.options.findIndex(([value]) => value === this.value)
            );
        await this.updateComplete;
        if (!this.isConnected || this.disabled) return;
        const list = this.querySelector('[role=listbox]');
        const anchor = this.querySelector('[role=combobox]').getBoundingClientRect();
        const below = innerHeight - anchor.bottom - 12;
        const above = anchor.top - 12;
        const upwards = below < Math.min(240, this.options.length * 36 + 10) && above > below;
        list.style.width = Math.min(Math.max(anchor.width, 140), innerWidth - 24) + 'px';
        list.style.maxHeight = Math.max(0, Math.min(240, upwards ? above : below)) + 'px';
        list.showPopover();
        const bounds = list.getBoundingClientRect();
        list.style.left = Math.max(12, Math.min(anchor.left, innerWidth - bounds.width - 12)) + 'px';
        list.style.top = (upwards ? anchor.top - bounds.height - 6 : anchor.bottom + 6) + 'px';
        this.expanded = true;
        this.scrollActive();
    }
    async scrollActive() {
        await this.updateComplete;
        this.querySelector('[data-active=true]')?.scrollIntoView({ block: 'nearest' });
    }
    choose(index) {
        if (this.disabled || !this.options[index]) return;
        const [value] = this.options[index];
        const changed = value !== this.value;
        this.value = value;
        this.close();
        this.querySelector('[role=combobox]').focus();
        if (changed) this.dispatchEvent(new CustomEvent('select-change', { detail: { value }, bubbles: true, composed: true }));
    }
    onKeydown(event) {
        if (this.disabled || !this.options.length) return;
        const { key } = event;
        if (key === 'Tab' || key === 'Escape') {
            this.close();
            if (key === 'Escape') event.preventDefault();
            return;
        }
        if (['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter', ' '].includes(key)) {
            event.preventDefault();
            if (!this.expanded) {
                this.open(key === 'Home' ? 0 : key === 'End' ? this.options.length - 1 : undefined);
            } else if (key === 'Enter' || key === ' ') this.choose(this.activeIndex);
            else {
                this.activeIndex =
                    key === 'Home'
                        ? 0
                        : key === 'End'
                          ? this.options.length - 1
                          : (this.activeIndex + (key === 'ArrowDown' ? 1 : -1) + this.options.length) % this.options.length;
                this.scrollActive();
            }
        } else if (key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
            const now = dayjs().valueOf();
            this.searchText = (now - (this.searchTime || 0) < 700 ? this.searchText : '') + key.toLowerCase();
            this.searchTime = now;
            const index = this.options.findIndex(([, label]) => label.toLowerCase().startsWith(this.searchText));
            if (index >= 0) {
                event.preventDefault();
                this.open(index);
            }
        }
    }
    render() {
        const selected = this.options.find(([value]) => value === this.value);
        return html`<button
                type="button"
                role="combobox"
                aria-haspopup="listbox"
                aria-label=${this.label}
                aria-expanded=${String(this.expanded)}
                aria-controls=${this.listId}
                aria-activedescendant=${this.expanded ? this.listId + '-' + this.activeIndex : ''}
                class="flex h-9 w-full min-w-32 items-center justify-between gap-3 rounded-lg border border-line bg-white px-3 text-[13px] text-ink hover:border-[#c9ced7] hover:bg-slate-50 disabled:opacity-40"
                ?disabled=${this.disabled}
                @click=${() => (this.expanded ? this.close() : this.open())}
                @keydown=${this.onKeydown}
                @blur=${this.close}
            >
                <span class="truncate">${selected?.[1] || this.label}</span>
                <svg
                    class="size-3.5 shrink-0 text-muted transition-transform ${this.expanded ? 'rotate-180' : ''}"
                    viewBox="0 0 16 16"
                    fill="none"
                    aria-hidden="true"
                >
                    <path
                        d="m4 6 4 4 4-4"
                        stroke="currentColor"
                        stroke-width="1.5"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    ></path>
                </svg>
            </button>
            <div
                id=${this.listId}
                role="listbox"
                aria-label=${this.label}
                popover="auto"
                class="fixed overflow-auto rounded-xl border border-line bg-white p-1 text-[13px] text-ink shadow-[0_8px_24px_#1018281a] [scrollbar-width:thin]"
                style="inset:auto;margin:0;box-sizing:border-box"
                @toggle=${event => {
                    this.expanded = event.newState === 'open';
                }}
                @mousedown=${event => event.preventDefault()}
            >
                ${this.options.map(
                    ([value, label], index) =>
                        html`<div
                            id=${this.listId + '-' + index}
                            role="option"
                            aria-selected=${String(value === this.value)}
                            data-active=${String(index === this.activeIndex)}
                            class="flex min-h-9 cursor-pointer items-center justify-between gap-3 rounded-lg px-2.5 py-2 hover:bg-slate-100 data-[active=true]:bg-slate-100 aria-selected:font-medium"
                            @click=${() => this.choose(index)}
                        >
                            <span>${label}</span>
                            <svg
                                class="size-3.5 shrink-0 ${value === this.value ? '' : 'invisible'}"
                                viewBox="0 0 16 16"
                                fill="none"
                                aria-hidden="true"
                            >
                                <path
                                    d="m3.5 8 3 3 6-6"
                                    stroke="currentColor"
                                    stroke-width="1.5"
                                    stroke-linecap="round"
                                    stroke-linejoin="round"
                                ></path>
                            </svg>
                        </div>`
                )}
            </div>`;
    }
}
customElements.define('ui-select', UiSelect);
