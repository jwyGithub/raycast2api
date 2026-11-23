const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

const source = readFileSync(join(__dirname, '../src/page/views/logs.js'), 'utf8');

function mount(globals) {
    let Element;
    runInNewContext(source.replace(/^import .*;\r?\n/gm, ''), {
        LitElement: class { isConnected = true; },
        html: (parts, ...values) => ({ parts, values }),
        customElements: { define: (_, element) => { Element = element; } },
        message() {},
        ...globals
    });
    return new Element();
}

test('each active tab copies headers and body from separate regions', async () => {
    const copied = [];
    const panel = mount({ navigator: { clipboard: { writeText: async text => copied.push(text) } } });
    panel.detail = {
        inbound_headers: 'x-client: cli', inbound_json: '{"model":"m"}',
        upstream_headers: 'accept: application/json', upstream_json: '{"model":"upstream"}',
        response_headers: 'content-type: text/event-stream', response_text: 'first\nsecond'
    };
    for (const tab of ['inbound_json', 'upstream_json', 'response_text']) {
        panel.detailTab = tab;
        const regions = panel.renderDetailContent().values;
        assert.equal(regions.length, 2);
        for (const region of regions) {
            assert.match(region.parts.join(''), /<svg/);
            assert.match(region.parts.join(''), /<pre class="[^"]*overflow-auto/);
            assert.ok(region.values.includes(false));
            await region.values.find(value => typeof value === 'function')();
        }
    }
    assert.deepEqual(copied, [
        'x-client: cli', '{\n  "model": "m"\n}',
        'accept: application/json', '{\n  "model": "upstream"\n}',
        'content-type: text/event-stream', 'first\nsecond'
    ]);
    panel.detail = { response_text: 'legacy' };
    const [headers, body] = panel.renderDetailContent().values;
    assert.ok(headers.values.includes('暂无记录'));
    assert.ok(headers.values.includes(true));
    await body.values.find(value => typeof value === 'function')();
    assert.equal(copied.at(-1), 'legacy');
});

test('clipboard fallback selects the text inside the modal dialog', async () => {
    let appended;
    const notifications = [];
    const dialog = { open: true, append: input => { appended = input; } };
    const input = { style: {}, select() {}, setSelectionRange() {}, remove() { appended = null; } };
    const panel = mount({
        navigator: {},
        document: {
            createElement: () => input,
            execCommand: command => {
                assert.equal(command, 'copy');
                assert.equal(appended.value, 'headers and body');
                return true;
            }
        },
        message: (...args) => notifications.push(args)
    });
    panel.querySelector = () => dialog;
    await panel.copyDetail('headers and body');
    assert.equal(appended, null);
    assert.deepEqual(notifications, [['已复制到剪贴板', 'success']]);
});
