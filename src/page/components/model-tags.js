import { html } from 'https://cdn.jsdelivr.net/gh/lit/dist@3/core/lit-core.min.js';

const abilityTags = [
    ['工具', abilities => abilities.tools?.supported],
    ['视觉', abilities => abilities.vision],
    ['思考', abilities => abilities.thinking?.supported],
    ['联网', abilities => abilities.web_search],
    ['生图', abilities => abilities.image_generation]
];

export const modelTag = (classes, text, title) =>
    html`<span class="rounded px-1.5 py-0.5 ${classes}" title=${title ?? ''}>${text}</span>`;

export const modelAbilityColumn = {
    label: '能力',
    render: ({ abilities }) => {
        const tags = abilityTags.filter(([, supported]) => supported(abilities || {}));
        return html`<div class="flex gap-1">${tags.length ? tags.map(([label]) => modelTag('bg-slate-100', label)) : '—'}</div>`;
    }
};

export const modelCapabilityColumns = [
    modelAbilityColumn,
    {
        label: '推理强度',
        render: ({ abilities }) => {
            const { supported, options, default: defaultEffort } = abilities?.reasoning_effort || {};
            const efforts = supported && Array.isArray(options) ? options : [];
            return html`<div class="flex gap-1">
                ${efforts.length
                    ? efforts.map(option =>
                          option === defaultEffort
                              ? modelTag('bg-emerald-50 text-emerald-700', option, '默认')
                              : modelTag('bg-slate-100', option)
                      )
                    : '—'}
            </div>`;
        }
    }
];
