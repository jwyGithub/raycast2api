import { LitElement, html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { computePosition, autoUpdate, offset, flip, shift } from 'https://cdn.jsdelivr.net/npm/@floating-ui/dom@1.8.0/+esm';
import flatpickr from 'https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/+esm';
import { Mandarin } from 'https://cdn.jsdelivr.net/npm/flatpickr@4.6.13/dist/l10n/zh.js/+esm';
import { dayjs } from '../utils.js';

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
                // 模态 dialog 位于 top layer，挂在 body 的日历会被盖住且不可点击
                appendTo: this.closest('dialog') || undefined,
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
