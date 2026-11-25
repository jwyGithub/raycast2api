import { LitElement, html, css } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';
import { repeat } from 'https://cdn.jsdelivr.net/npm/lit-html@3.3.1/directives/repeat.js';
import { motionEnabled } from '../utils.js';

class UiMessage extends LitElement {
    static properties = { items: { state: true } };
    static styles = css`
        :host {
            display: flex;
            flex-direction: column;
            align-items: flex-end;
            position: fixed;
            inset: 20px 20px auto auto;
            margin: 0;
            padding: 0;
            border: 0;
            width: max-content;
            max-width: min(440px, calc(100vw - 32px));
            overflow: visible;
            background: transparent;
            color: #242933;
            font:
                13px/20px system-ui,
                sans-serif;
        }
        .message {
            box-sizing: border-box;
            display: flex;
            width: max-content;
            max-width: 100%;
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
        .text {
            flex: 1;
            min-width: 0;
            white-space: pre-wrap;
            overflow-wrap: anywhere;
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
                    ><span class="text">${item.text}</span
                    ><button type="button" aria-label="关闭提示" @click=${() => this.close(item.id)}>×</button>
                </div>`
        )}`;
    }
}
customElements.define('ui-message', UiMessage);
const messageHost = document.createElement('ui-message');
document.body.append(messageHost);
export const message = (text, type = 'error') => messageHost.show(text, type);
