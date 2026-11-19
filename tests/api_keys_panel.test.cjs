const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

const key = { id: 'test-id', api_key: 'sk-fixture-for-panel-tests' };
const deferred = () => Promise.withResolvers();

function mount(file, globals = {}) {
    let Element;
    const source = readFileSync(join(__dirname, '../src/page', file), 'utf8');
    runInNewContext(source.replace(/^import .*;\r?\n/gm, ''), {
        LitElement: class {
            isConnected = true;
            querySelector() { return null; }
            querySelectorAll() { return []; }
        },
        html: (parts, ...values) => ({ parts, values }),
        nothing: null,
        customElements: { define: (_, element) => { Element = element; } },
        ...globals
    }, { filename: file });
    return new Element();
}

test('a stale list or model response cannot restore the closed panel data', async () => {
    const panel = mount('views/api-keys.js');
    const list = deferred();
    const models = deferred();
    panel.request = path => path === 'list' ? list.promise : models.promise;
    const loading = panel.load();
    const preview = panel.openModels(key, true);
    panel.closeDialog();
    models.resolve({ items: [{ id: 'model', selected: true }], unrestricted: false });
    await preview;
    assert.equal(panel.dialog, null);
    panel.clear();
    list.resolve({ items: [key], total: 1, total_pages: 1 });
    await loading;
    assert.equal(panel.items.length, 0);
});

test('an old API response cannot log out a newly authenticated session', async () => {
    const fetch = deferred();
    const console = mount('views.js', { fetch: () => fetch.promise });
    console.authenticated = true;
    const pending = console.request('list', {}, 'api-keys');
    console.lockConsole();
    console.authenticated = true;
    fetch.resolve({ status: 401, json: async () => ({}) });
    await assert.rejects(pending, /会话已结束/);
    assert.equal(console.authenticated, true);
});

test('copy falls back to the full key and suppresses results after clearing', async () => {
    const notifications = [];
    const clipboard = deferred();
    const input = { style: {}, select() {}, setSelectionRange() {}, remove() {} };
    let copied = true;
    let fallbackCalls = 0;
    const panel = mount('views/api-keys.js', {
        navigator: { clipboard: { writeText: async () => { throw Error('unavailable'); } } },
        document: {
            createElement: () => input,
            execCommand: command => {
                assert.equal(command, 'copy');
                assert.equal(input.value, key.api_key);
                fallbackCalls += 1;
                return copied;
            }
        },
        message: (...args) => notifications.push(args)
    });
    panel.append = () => {};
    await panel.copy(key);
    assert.equal(notifications.length, 1);
    assert.equal(panel.dialog, null);
    copied = false;
    await panel.copy(key);
    assert.equal(panel.dialog.type, 'copy');
    assert.equal(panel.dialog.row.api_key, key.api_key);
    assert.equal(notifications.length, 1);
    panel.closeDialog();
    const waiting = mount('views/api-keys.js', {
        navigator: { clipboard: { writeText: () => clipboard.promise } },
        message: (...args) => notifications.push(args)
    });
    const pending = waiting.copy(key);
    waiting.clear();
    clipboard.reject(Error('unavailable'));
    await pending;
    assert.equal(waiting.dialog, null);
    assert.equal(notifications.length, 1);
    assert.equal(fallbackCalls, 2);
});

test('editing keeps the UTC+8 wall time and submits exact UTC seconds', () => {
    const panel = mount('views/api-keys.js');
    const start = Date.parse('2026-03-07T18:30:07Z') / 1000;
    panel.openForm({ ...key, starts_at: start, ends_at: start + 30, quota_kind: 'cost', quota_limit: '0.0091105' });
    assert.equal(panel.dialog.draft.starts_at, '2026-03-08T02:30:07');
    assert.equal(panel.dialog.draft.validity, 'period');
    assert.equal(panel.dialog.draft.quota_kind, 'cost');
    let submitted;
    panel.save = (path, data) => { submitted = { path, data }; };
    panel.submitForm({ preventDefault() {} });
    assert.equal(submitted.path, 'update');
    assert.equal(submitted.data.starts_at, start);
    assert.equal(submitted.data.ends_at, start + 30);
    assert.equal(submitted.data.quota_limit, '0.0091105');
});

test('switching a timed key to permanent clears both start and end times', () => {
    const panel = mount('views/api-keys.js');
    const start = Date.parse('2026-03-07T18:30:07Z') / 1000;
    panel.openForm({ ...key, starts_at: start, ends_at: start + 30, quota_kind: 'unlimited' });
    panel.setDraft('validity', 'permanent');
    let submitted;
    panel.save = (path, data) => { submitted = { path, data }; };
    panel.submitForm({ preventDefault() {} });
    assert.equal(submitted.path, 'update');
    assert.equal(submitted.data.starts_at, null);
    assert.equal(submitted.data.ends_at, null);
});

test('used quota renders the real credits and cost on one line with two decimals', () => {
    const panel = mount('views/api-keys.js');
    const used = { used_credits: '19.340000000', used_cost: '0.190000000' };
    for (const quota_kind of ['unlimited', 'credits', 'cost']) {
        assert.equal(panel.renderUsed({ ...used, quota_kind }), '19.34 / $0.19');
    }
    assert.equal(panel.renderUsed({ used_credits: '0', used_cost: '0', quota_kind: 'cost' }), '0.00 / $0.00');
});
