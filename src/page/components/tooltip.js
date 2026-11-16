import { LitElement, html, css } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { computePosition, autoUpdate, offset, flip, shift } from 'https://cdn.jsdelivr.net/npm/@floating-ui/dom@1.8.0/+esm';
import { motionEnabled } from '../utils.js';

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
