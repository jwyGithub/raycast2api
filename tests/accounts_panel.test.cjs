const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

function mount(tag = 'accounts-panel', messages = []) {
    const elements = new Map();
    const source = readFileSync(join(__dirname, '../src/page/views/accounts.js'), 'utf8');
    runInNewContext(source.replace(/^import .*;\r?\n/gm, ''), {
        LitElement: class { isConnected = true; },
        html: (parts, ...values) => ({ parts, values }),
        customElements: { define: (name, element) => elements.set(name, element) },
        dayjs: Object.assign(() => ({}), {
            unix: value => ({ isAfter: () => value > Date.now() / 1000, diff: () => 60 })
        }),
        formatDate: () => '',
        CSS: { escape: value => value },
        CustomEvent: class {
            constructor(type, init) {
                this.type = type;
                this.detail = init?.detail;
            }
        },
        message: (text, type) => messages.push([text, type])
    });
    return new (elements.get(tag))();
}

test('refresh is disabled during cooldown or while disabled and resumes after expiry or enabling', () => {
    const card = mount('account-card');
    card.busy = false;
    for (const [account, disabled] of [
        [{ enabled: false }, true],
        [{ enabled: true, cooldown_until: Math.floor(Date.now() / 1000) + 600 }, true],
        [{ enabled: true, cooldown_until: Math.floor(Date.now() / 1000) - 1 }, false],
        [{ enabled: true }, false]
    ]) {
        card.account = { email: 'paused@x', available: true, ...account };
        const template = card.render();
        const index = template.parts.findIndex(part => part.includes('label="刷新额度"') && part.endsWith('.disabled='));
        assert.notEqual(index, -1);
        assert.equal(Boolean(template.values[index]), disabled);
    }
});

function busyStates(template) {
    if (Array.isArray(template)) return Array.from(template).flatMap(busyStates);
    if (!template?.parts) return [];
    return Array.from(template.values).flatMap((value, index) =>
        template.parts[index].endsWith('.busy=') ? [value] : busyStates(value)
    );
}

test('pending account operations disable every card and rejected priority edits restore the saved value', async () => {
    const panel = mount();
    panel.accounts = [{ email: 'first@x', priority: 0 }, { email: 'second@x', priority: 0 }];
    const input = { value: '9' };
    panel.querySelector = () => input;
    const refresh = Promise.withResolvers();
    const requests = [];
    panel.request = async (path, body) => {
        requests.push({ path, body });
        if (path === 'refresh') return refresh.promise;
        if (path === 'update') panel.accounts[1].priority = body.priority;
        return { items: panel.accounts, total: 2, total_pages: 1 };
    };

    const pending = panel.accountAction({ detail: { action: 'refresh', email: 'first@x' } });
    assert.deepEqual(busyStates(panel.render()), [true, true]);
    await panel.accountAction({ detail: { action: 'priority', email: 'second@x', value: 9 } });
    assert.deepEqual(requests.map(request => request.path), ['refresh']);
    assert.equal(input.value, '0');
    assert.equal(panel.accounts[1].priority, 0);

    refresh.resolve({ available: true });
    await pending;
    assert.deepEqual(busyStates(panel.render()), [false, false]);
    input.value = '9';
    await panel.accountAction({ detail: { action: 'priority', email: 'second@x', value: 9 } });
    const update = requests.find(request => request.path === 'update');
    assert.equal(update.body.email, 'second@x');
    assert.equal(update.body.priority, 9);
    assert.equal(panel.accounts[1].priority, 9);
    assert.equal(panel.workingEmail, '');
});

test('a failed priority save restores the stored value and releases all cards', async () => {
    const panel = mount();
    panel.accounts = [{ email: 'first@x', priority: 3 }];
    const input = { value: '9' };
    panel.querySelector = () => input;
    panel.request = async path => {
        if (path === 'update') throw new Error('save failed');
        return { items: panel.accounts, total: 1, total_pages: 1 };
    };

    await panel.accountAction({ detail: { action: 'priority', email: 'first@x', value: 9 } });
    assert.equal(input.value, '3');
    assert.equal(panel.accounts[0].priority, 3);
    assert.equal(panel.workingEmail, '');
    assert.deepEqual(busyStates(panel.render()), [false]);
});

test('settings dialog rejects invalid priority, skips unchanged values and stays open when saving fails', async () => {
    const messages = [];
    const panel = mount('accounts-panel', messages);
    panel.accounts = [{ email: 'first@x', priority: 3 }];
    const input = { value: 'abc' };
    panel.querySelector = selector => {
        assert.match(selector, /^(#account-priority|input\[data-priority-email="first@x"\])$/);
        return input;
    };
    const requests = [];
    panel.request = async (path, body) => {
        requests.push({ path, body });
        return { items: panel.accounts, total: 1, total_pages: 1 };
    };

    await panel.accountAction({ detail: { action: 'settings', email: 'first@x' } });
    assert.equal(panel.settingsEmail, 'first@x');
    await panel.saveSettings();
    assert.equal(requests.length, 0);
    assert.equal(panel.settingsEmail, 'first@x');
    assert.match(messages.at(-1)[0], /优先级必须是/);

    input.value = '3';
    await panel.saveSettings();
    assert.equal(requests.length, 0);
    assert.equal(panel.settingsEmail, '');

    panel.settingsEmail = 'first@x';
    input.value = '8';
    await panel.saveSettings();
    assert.equal(requests[0].path, 'update');
    assert.deepEqual({ ...requests[0].body }, { email: 'first@x', priority: 8 });
    assert.equal(panel.settingsEmail, '');

    panel.settingsEmail = 'first@x';
    input.value = '9';
    panel.request = async () => {
        throw new Error('save failed');
    };
    await panel.saveSettings();
    assert.equal(panel.settingsEmail, 'first@x');
    assert.equal(panel.workingEmail, '');
    assert.equal(messages.at(-1)[0], 'save failed');
});

function text(template) {
    if (Array.isArray(template)) return Array.from(template).map(text).join('');
    if (!template?.parts) return String(template ?? '');
    return Array.from(template.parts)
        .map((part, index) => part + (index < template.values.length ? text(template.values[index]) : ''))
        .join('');
}

test('the card settings button dispatches a settings action for its account', () => {
    const card = mount('account-card');
    const events = [];
    card.dispatchEvent = event => events.push(event);
    card.busy = false;
    card.account = { email: 'first@x', available: true };
    const template = card.render();
    const index = template.parts.findIndex(
        (part, i) => part.endsWith('@button-click=') && template.parts[i - 1]?.includes('label="设置"')
    );
    assert.notEqual(index, -1);
    template.values[index]();
    assert.equal(events[0].type, 'account-action');
    assert.equal(events[0].detail.action, 'settings');
    assert.equal(events[0].detail.email, 'first@x');
});

test('settings dialog renders the account and its saved priority, and only the priority field', () => {
    const panel = mount();
    panel.accounts = [{ email: 'first@x', priority: 7 }];
    panel.settingsEmail = 'first@x';
    const rendered = text(panel.renderSettings());
    assert.match(rendered, /first@x/);
    assert.match(rendered, /优先级：/);
    assert.match(rendered, /id="account-priority"/);
    assert.match(rendered, /7/);
    assert.doesNotMatch(rendered, /创建时间|额度同步/);
    panel.settingsEmail = '';
    assert.doesNotMatch(text(panel.renderSettings()), /first@x/);
});

test('updated() syncs native dialogs with state and closes settings when the account is gone', () => {
    const panel = mount();
    const dialogs = {
        '#account-import-dialog': { open: false, showModal() { this.open = true; }, close() { this.open = false; } },
        '#account-settings-dialog': { open: false, showModal() { this.open = true; }, close() { this.open = false; } }
    };
    panel.querySelector = selector => dialogs[selector];
    panel.accounts = [{ email: 'first@x' }];

    panel.dialog = true;
    panel.settingsEmail = 'first@x';
    panel.updated();
    assert.equal(dialogs['#account-import-dialog'].open, true);
    assert.equal(dialogs['#account-settings-dialog'].open, true);

    panel.dialog = false;
    panel.accounts = [];
    panel.updated();
    assert.equal(panel.settingsEmail, '');
    assert.equal(dialogs['#account-import-dialog'].open, false);
    panel.updated();
    assert.equal(dialogs['#account-settings-dialog'].open, false);
});
