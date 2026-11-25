const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

function mount(file) {
    let Panel;
    const messages = [];
    runInNewContext(readFileSync(join(__dirname, '../src/page/views', file), 'utf8').replace(/^import .*;\r?\n/gm, ''), {
        LitElement: class { isConnected = true; querySelectorAll() { return []; } },
        html: (parts, ...values) => ({ parts, values }),
        customElements: { define: (_, element) => { Panel = element; } },
        message: (...args) => messages.push(args), confirm: () => true
    });
    return { panel: new Panel(), messages };
}

test('proxy test updates latency including zero and reports it', async () => {
    const { panel, messages } = mount('proxies.js');
    panel.proxies = [{ id: 'p', latency_ms: null }];
    panel.request = async (path, body) => {
        assert.equal(path, 'test'); assert.equal(body.id, 'p'); return { latency_ms: 0 };
    };
    await panel.action(panel.proxies[0], 'test');
    assert.equal(panel.proxies[0].latency_ms, 0);
    assert.deepEqual(messages[0], ['代理延迟：0 ms', 'success']);
});

test('closing a panel ignores pending list and test results', async () => {
    const { panel, messages } = mount('proxies.js');
    let resolve;
    panel.request = () => new Promise(r => { resolve = r; });
    const loading = panel.load();
    panel.clear(); resolve([{ id: 'stale' }]); await loading;
    assert.equal(panel.proxies.length, 0);
    const testing = panel.action({ id: 'p' }, 'test');
    panel.clear(); resolve({ latency_ms: 123 }); await testing;
    assert.equal(messages.length, 0);
});

test('failed test preserves last measured latency', async () => {
    const { panel, messages } = mount('proxies.js');
    panel.proxies = [{ id: 'p', latency_ms: 10 }];
    panel.request = async () => { throw new Error('timeout'); };
    await panel.action(panel.proxies[0], 'test');
    assert.equal(panel.proxies[0].latency_ms, 10); assert.equal(messages[0][0], 'timeout');
});

test('proxy dialog loads options from management and saves selected account proxy', async () => {
    const { panel } = mount('accounts.js');
    panel.accounts = [{ email: 'a@test', proxy: 'direct' }];
    const calls = [];
    panel.request = async (path, body, resource) => {
        calls.push([path, body, resource]);
        if (resource === 'proxies') return [{ id: 'p', type: 'socks5', host: 'localhost', port: 1080 }];
        return {};
    };
    panel.load = async () => {};
    await panel.openProxy('a@test');
    assert.equal(panel.proxyMode, 'direct');
    assert.equal(panel.proxyOptions[0][0], 'p');
    assert.equal(calls[0][2], 'proxies');
    panel.proxyMode = 'proxy'; panel.proxyId = 'p';
    await panel.saveProxy();
    assert.equal(calls[1][0], 'settings'); assert.equal(calls[1][1].proxy, 'p');
    assert.equal(panel.proxyDialog, false);
});

test('closed account dialog discards options and missing selection cannot be saved', async () => {
    const { panel, messages } = mount('accounts.js');
    panel.accounts = [{ email: 'a@test', proxy: 'direct' }];
    let resolve;
    panel.request = () => new Promise(r => { resolve = r; });
    const loading = panel.openProxy('a@test');
    panel.closeProxy(); resolve([{ id: 'stale' }]); await loading;
    assert.equal(panel.proxyOptions.length, 0);
    panel.proxyLoading = false; panel.proxyMode = 'proxy'; panel.proxyId = '';
    await panel.saveProxy(); assert.equal(messages[0][0], '请选择代理');
});
