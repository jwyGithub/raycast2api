import { LitElement, html, css } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { repeat } from 'https://cdn.jsdelivr.net/npm/lit-html@3.3.1/directives/repeat.js';
import { computePosition, autoUpdate, offset, flip, shift } from 'https://cdn.jsdelivr.net/npm/@floating-ui/dom@1.8.0/+esm';
import flatpickr from 'https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/+esm';
import { Mandarin } from 'https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/l10n/zh.js/+esm';
import { dayjs, motionEnabled } from './utils.js';

class UiTooltip extends LitElement {
    static properties = { content: { type: String } };
    static styles = css`
        :host {
            display: block;
            min-width: 0;
        }
        .trigger {
            display: block;
            overflow: hidden;
            text-overflow: ellipsis;
            white-space: nowrap;
            outline-offset: 3px;
        }
        .trigger:focus-visible {
            outline: 2px solid #3b82f6;
        }
        .tip {
            position: fixed;
            inset: auto;
            margin: 0;
            box-sizing: border-box;
            width: max-content;
            max-width: min(420px, calc(100vw - 24px));
            max-height: min(360px, calc(100dvh - 24px));
            overflow: visible;
            border: 0;
            padding: 0;
            background: transparent;
        }
        .content {
            box-sizing: border-box;
            max-height: inherit;
            overflow: auto;
            border-radius: 8px;
            padding: 9px 12px;
            background: #242933;
            color: white;
            font:
                12px/1.6 system-ui,
                sans-serif;
            white-space: pre-wrap;
            overflow-wrap: anywhere;
            box-shadow: 0 6px 24px #10182826;
        }
    `;
    async show() {
        clearTimeout(this.hideTimer);
        if (!this.content) return;
        await this.updateComplete;
        if (!this.isConnected) return;
        const trigger = this.shadowRoot.querySelector('.trigger');
        if (trigger.scrollWidth <= trigger.clientWidth) {
            this.hide();
            return;
        }
        const tip = this.shadowRoot.querySelector('.tip');
        const content = this.shadowRoot.querySelector('.content');
        const opening = !tip.matches(':popover-open');
        window.gsap?.killTweensOf(content);
        this.cleanupPosition?.();
        this.cleanupPosition = null;
        if (opening) {
            tip.style.visibility = 'hidden';
            tip.showPopover();
        }
        const update = async () => {
            if (trigger.scrollWidth <= trigger.clientWidth) {
                this.hide();
                return;
            }
            try {
                const { x, y } = await computePosition(trigger, tip, {
                    strategy: 'fixed',
                    placement: 'bottom-start',
                    middleware: [offset(8), flip({ padding: 12 }), shift({ padding: 12, crossAxis: true })]
                });
                if (this.updatePosition !== update || !this.isConnected) return;
                Object.assign(tip.style, { left: x + 'px', top: y + 'px', visibility: 'visible' });
            } catch (error) {
                if (this.updatePosition === update) this.hide();
                console.warn('Tooltip 定位失败:', error);
            }
        };
        this.updatePosition = update;
        await update();
        if (this.updatePosition !== update) return;
        this.cleanupPosition = autoUpdate(trigger, tip, update, { animationFrame: true });
        if (motionEnabled()) {
            if (opening) window.gsap.set(content, { opacity: 0, y: 4 });
            window.gsap.to(content, { opacity: 1, y: 0, duration: 0.16, ease: 'power2.out', overwrite: true });
        } else if (window.gsap) window.gsap.set(content, { opacity: 1, y: 0 });
    }
    updated() {
        this.updatePosition?.();
    }
    hide() {
        clearTimeout(this.hideTimer);
        this.updatePosition = null;
        this.cleanupPosition?.();
        this.cleanupPosition = null;
        const tip = this.shadowRoot?.querySelector('.tip');
        if (!tip?.matches(':popover-open')) return;
        const content = this.shadowRoot.querySelector('.content');
        window.gsap?.killTweensOf(content);
        if (this.isConnected && motionEnabled())
            window.gsap.to(content, {
                opacity: 0,
                y: 4,
                duration: 0.12,
                ease: 'power1.out',
                onComplete: () => tip.hidePopover()
            });
        else tip.hidePopover();
    }
    scheduleHide() {
        this.hideTimer = setTimeout(() => this.hide(), 120);
    }
    disconnectedCallback() {
        this.hide();
        super.disconnectedCallback();
    }
    render() {
        return html`<span
                class="trigger"
                tabindex="0"
                aria-describedby="tooltip"
                @mouseenter=${this.show}
                @mouseleave=${this.scheduleHide}
                @focus=${this.show}
                @blur=${this.scheduleHide}
                @keydown=${e => {
                    if (e.key === 'Escape') this.hide();
                }}
                ><slot>${this.content}</slot></span
            >
            <div
                id="tooltip"
                class="tip"
                role="tooltip"
                popover="manual"
                @mouseenter=${() => clearTimeout(this.hideTimer)}
                @mouseleave=${this.scheduleHide}
                @keydown=${e => {
                    if (e.key === 'Escape') this.hide();
                }}
            >
                <div class="content">${this.content}</div>
            </div>`;
    }
}
customElements.define('ui-tooltip', UiTooltip);
class UiMessage extends LitElement {
    static properties = { items: { state: true } };
    static styles = css`
        :host {
            position: fixed;
            inset: 20px 20px auto auto;
            margin: 0;
            padding: 0;
            border: 0;
            width: min(440px, calc(100vw - 32px));
            overflow: visible;
            background: transparent;
            color: #242933;
            font:
                13px/20px system-ui,
                sans-serif;
        }
        .message {
            display: flex;
            align-items: center;
            gap: 10px;
            margin-bottom: 10px;
            padding: 10px 12px;
            border: 1px solid #e4e7ec;
            border-radius: 10px;
            background: white;
            box-shadow: 0 6px 24px #1018281a;
        }
        .icon {
            flex: none;
            display: grid;
            place-items: center;
            width: 18px;
            height: 18px;
            border-radius: 50%;
            font-size: 12px;
            font-weight: 600;
            background: #fef2f2;
            color: #b91c1c;
        }
        .success .icon {
            background: #ecfdf5;
            color: #047857;
        }
        ui-tooltip {
            flex: 1;
            min-width: 0;
        }
        button {
            flex: none;
            width: 28px;
            height: 28px;
            border: 0;
            border-radius: 5px;
            background: transparent;
            color: #667080;
            font: inherit;
            font-size: 18px;
            cursor: pointer;
        }
        button:hover {
            background: #f1f5f9;
        }
        button:focus-visible {
            outline: 2px solid #3b82f6;
        }
    `;
    constructor() {
        super();
        this.items = [];
        this.timers = new Map();
        this.nextId = 0;
    }
    connectedCallback() {
        super.connectedCallback();
        this.setAttribute('popover', 'manual');
    }
    show(text, type = 'error') {
        const item = { id: ++this.nextId, text: String(text), type };
        this.items = [...this.items, item];
        this.resume(item);
    }
    pause(item) {
        clearTimeout(this.timers.get(item.id));
    }
    resume(item) {
        this.pause(item);
        if (!item.closing)
            this.timers.set(
                item.id,
                setTimeout(() => this.close(item.id), item.type === 'error' ? 8000 : 4000)
            );
    }
    close(id) {
        const item = this.items.find(item => item.id === id);
        if (!item || item.closing) return;
        item.closing = true;
        this.pause(item);
        this.timers.delete(id);
        const node = this.shadowRoot.querySelector('[data-id="' + id + '"]');
        const remove = () => {
            const positions = new Map(
                [...this.shadowRoot.querySelectorAll('.message')].map(element => [element, element.getBoundingClientRect().top])
            );
            this.items = this.items.filter(item => item.id !== id);
            this.updateComplete.then(() => {
                if (!this.isConnected || !motionEnabled()) return;
                for (const element of this.shadowRoot.querySelectorAll('.message')) {
                    const y = positions.get(element) - element.getBoundingClientRect().top;
                    if (y && !this.items.find(item => item.id === Number(element.dataset.id))?.closing)
                        window.gsap.fromTo(element, { y }, { y: 0, duration: 0.2, ease: 'power2.out', overwrite: 'auto' });
                }
            });
        };
        if (node && motionEnabled())
            window.gsap.to(node, {
                opacity: 0,
                x: 20,
                duration: 0.18,
                ease: 'power1.out',
                overwrite: true,
                onComplete: remove
            });
        else remove();
    }
    updated() {
        if (this.items.length) {
            if (this.matches(':popover-open')) this.hidePopover();
            this.showPopover();
        } else if (!this.items.length && this.matches(':popover-open')) this.hidePopover();
        for (const node of this.shadowRoot.querySelectorAll('.message')) {
            if (node.dataset.entered) continue;
            node.dataset.entered = 'true';
            if (motionEnabled())
                window.gsap.fromTo(
                    node,
                    { opacity: 0, x: 24 },
                    { opacity: 1, x: 0, duration: 0.24, ease: 'power2.out', overwrite: true }
                );
        }
    }
    disconnectedCallback() {
        for (const timer of this.timers.values()) clearTimeout(timer);
        window.gsap?.killTweensOf(this.shadowRoot.querySelectorAll('.message'));
        super.disconnectedCallback();
    }
    render() {
        return html`${repeat(
            this.items,
            item => item.id,
            item =>
                html`<div
                    data-id=${item.id}
                    class=${'message ' + item.type}
                    role=${item.type === 'error' ? 'alert' : 'status'}
                    @mouseenter=${() => this.pause(item)}
                    @mouseleave=${() => this.resume(item)}
                    @focusin=${() => this.pause(item)}
                    @focusout=${() => this.resume(item)}
                >
                    <span class="icon" aria-hidden="true">${item.type === 'success' ? '✓' : '!'}</span
                    ><ui-tooltip .content=${item.text}></ui-tooltip
                    ><button type="button" aria-label="关闭提示" @click=${() => this.close(item.id)}>×</button>
                </div>`
        )}`;
    }
}
customElements.define('ui-message', UiMessage);
const messageHost = document.createElement('ui-message');
document.body.append(messageHost);
export const message = (text, type = 'error') => messageHost.show(text, type);

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
class UiDatePicker extends LitElement {
    static properties = { value: { type: String }, disabled: { type: Boolean }, label: { type: String } };
    static nextId = 0;
    constructor() {
        super();
        this.value = '';
        this.label = '日期时间';
        this.inputId = 'date-picker-' + ++UiDatePicker.nextId;
    }
    createRenderRoot() {
        return this;
    }
    connectedCallback() {
        super.connectedCallback();
        this.updateComplete.then(() => {
            if (!this.isConnected || this.picker) return;
            this.picker = flatpickr(this.querySelector('input'), {
                locale: Mandarin,
                enableTime: true,
                enableSeconds: true,
                time_24hr: true,
                minuteIncrement: 1,
                disableMobile: true,
                animate: false,
                monthSelectorType: 'static',
                dateFormat: 'Y-m-d H:i:S',
                ariaDateFormat: 'Y年m月d日',
                position: () => this.updatePosition?.(),
                onReady: (_, __, picker) => {
                    picker.calendarContainer.classList.add('raycast-calendar');
                    picker.hourElement.setAttribute('aria-label', '时');
                    picker.minuteElement.setAttribute('aria-label', '分');
                    picker.secondElement.setAttribute('aria-label', '秒');
                    const actions = document.createElement('div');
                    actions.className = 'picker-actions';
                    for (const [text, action] of [
                        [
                            '清除',
                            () => {
                                picker.clear();
                                picker.close();
                            }
                        ],
                        ['确定', () => picker.close()]
                    ]) {
                        const button = document.createElement('button');
                        button.type = 'button';
                        button.textContent = text;
                        button.addEventListener('click', action);
                        actions.append(button);
                    }
                    picker.calendarContainer.append(actions);
                },
                onChange: dates => {
                    this.value = dates.length ? dayjs(dates[0]).format('YYYY-MM-DDTHH:mm:ss') : '';
                    this.dispatchEvent(
                        new CustomEvent('date-change', { detail: { value: this.value }, bubbles: true, composed: true })
                    );
                },
                onOpen: (_, __, picker) => this.startPositioning(picker),
                onClose: () => this.stopPositioning()
            });
            this.syncValue();
        });
    }
    updated() {
        this.syncValue();
    }
    syncValue() {
        if (!this.picker) return;
        if (this.disabled) this.picker.close();
        const current = this.picker.selectedDates[0];
        if ((current ? dayjs(current).format('YYYY-MM-DDTHH:mm:ss') : '') === this.value) return;
        if (!this.value) this.picker.clear(false);
        else {
            const date = dayjs(this.value, ['YYYY-MM-DDTHH:mm:ss', 'YYYY-MM-DDTHH:mm'], true);
            if (date.isValid()) this.picker.setDate(date.toDate(), false);
        }
    }
    startPositioning(picker) {
        this.stopPositioning();
        const calendar = picker.calendarContainer;
        calendar.style.visibility = 'hidden';
        const update = async () => {
            try {
                const { x, y } = await computePosition(this.querySelector('input'), calendar, {
                    strategy: 'fixed',
                    placement: 'bottom-start',
                    middleware: [offset(8), flip({ padding: 12 }), shift({ padding: 12, crossAxis: true })]
                });
                if (this.updatePosition !== update || !picker.isOpen || !this.isConnected) return;
                Object.assign(calendar.style, { left: x + 'px', top: y + 'px', visibility: 'visible' });
            } catch (error) {
                if (this.updatePosition === update) picker.close();
                console.warn('日期控件定位失败:', error);
            }
        };
        this.updatePosition = update;
        this.cleanupPosition = autoUpdate(this.querySelector('input'), calendar, update);
    }
    stopPositioning() {
        this.updatePosition = null;
        this.cleanupPosition?.();
        this.cleanupPosition = null;
    }
    disconnectedCallback() {
        this.stopPositioning();
        this.picker?.destroy();
        this.picker = null;
        super.disconnectedCallback();
    }
    render() {
        return html`<label for=${this.inputId} class="mb-1 block text-xs text-muted">${this.label}</label>
            <div class="relative">
                <input
                    id=${this.inputId}
                    type="text"
                    autocomplete="off"
                    class="h-9 w-56 max-w-full cursor-pointer rounded-lg border border-line bg-white pl-3 pr-9 text-[13px] text-ink hover:border-[#c9ced7] disabled:opacity-40"
                    placeholder="请选择日期和时间"
                    ?disabled=${this.disabled}
                />
                <svg
                    class="pointer-events-none absolute right-3 top-2.5 size-4 text-muted"
                    viewBox="0 0 16 16"
                    fill="none"
                    aria-hidden="true"
                >
                    <rect x="2" y="3.5" width="12" height="10.5" rx="2" stroke="currentColor"></rect>
                    <path d="M5 2v3M11 2v3M2 7h12" stroke="currentColor" stroke-linecap="round"></path>
                </svg>
            </div>`;
    }
}
customElements.define('ui-date-picker', UiDatePicker);
