const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

function mount(tag = 'accounts-panel') {
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
        message() {}
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
