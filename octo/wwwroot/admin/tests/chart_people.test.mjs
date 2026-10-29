// Runs the dashboard's per-person chart block (the part of admin.js between `let chartPeople`
// and `function relativeTime`) against a scripted fake of the admin API.
//
//   node --test octo/wwwroot/admin/tests/

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../admin.js', import.meta.url), 'utf8');
const start = source.indexOf('let chartPeople = null;');
const end = source.indexOf('function relativeTime');
assert.ok(start > 0 && end > start, 'the charts block is in admin.js');
const block = source.slice(start, end);

function element(extra = {}) {
  return {
    value: '',
    hidden: false,
    innerHTML: '',
    className: '',
    textContent: '',
    replaceChildren() {},
    addEventListener() {},
    ...extra,
  };
}

// `replies` are handed out, in order, to each PUT; any other call is answered not ok.
function load(replies) {
  const holder = element();
  const country = element({ value: 'br' });
  const pins = [element(), element(), element()];
  const byId = { 'charts-people': element(), 'charts-people-note': holder, 'charts-person-country': country };
  const puts = [];
  const context = {
    document: {
      getElementById: id => byId[id] ?? null,
      querySelector: () => element(),
      querySelectorAll: selector => (selector.endsWith('.charts-pin') ? pins : []),
      createElement: () => element(),
    },
    api: async (url, options = {}) => {
      if (options.method !== 'PUT') return { ok: false, status: 404, json: async () => ({}) };
      puts.push(JSON.parse(options.body));
      const reply = replies.shift();
      await Promise.resolve();
      return { ok: reply.status === 200, status: reply.status, json: async () => reply.body };
    },
    chartsInfo: { countries: [], country: { used: 'us' } },
    regionName: code => code,
    icon: () => '',
    esc: text => text,
    srAlert: null,
    renderChartStatus() {},
    setTimeout: () => 0,
    clearTimeout() {},
  };
  vm.createContext(context);
  vm.runInContext(block, context);
  vm.runInContext(
    `chartPeople = ${JSON.stringify({
      people: [{ user: 'anna', country: 'auto', pinned: [], revision: 3 }],
    })}; chartPeopleUser = 'anna';`,
    context,
  );
  return {
    puts,
    holder,
    country,
    save: () => vm.runInContext('saveChartPeople()', context),
    settled: () => vm.runInContext('chartPeopleSaving', context),
    people: () => vm.runInContext('chartPeople', context).people,
  };
}

const answer = (revision, countryCode = 'br') => ({
  status: 200,
  body: { people: [{ user: 'anna', country: countryCode, pinned: [], revision, notOnAppleMusic: [] }] },
});

test("a second quick save carries the first answer's revision", async () => {
  const page = load([answer(4, 'br'), answer(5, 'jp')]);

  page.country.value = 'br';
  page.save();
  page.country.value = 'jp';
  page.save();
  await page.settled();

  assert.deepEqual(
    page.puts.map(put => [put.country, put.revision]),
    [
      ['br', 3],
      ['jp', 4],
    ],
  );
  assert.equal(page.people()[0].revision, 5);
});

test('a conflict drops the saves queued behind it and shows the latest', async () => {
  const latest = {
    status: 409,
    body: {
      error: 'Changed elsewhere just now; showing the latest.',
      people: [{ user: 'anna', country: 'kr', pinned: [], revision: 9, notOnAppleMusic: [] }],
    },
  };
  const page = load([latest, answer(10, 'de')]);

  page.country.value = 'br';
  page.save();
  page.country.value = 'jp';
  page.save();
  page.save();
  await page.settled();

  assert.equal(page.puts.length, 1, 'the two queued behind the refused one are never sent');
  assert.equal(page.people()[0].country, 'kr');
  assert.equal(page.people()[0].revision, 9);
  assert.ok(page.holder.innerHTML.includes('Changed elsewhere just now; showing the latest.'));
  assert.ok(page.holder.className.includes('error'));

  page.country.value = 'de';
  page.save();
  await page.settled();

  assert.deepEqual(
    page.puts.map(put => [put.country, put.revision]),
    [
      ['br', 3],
      ['de', 9],
    ],
    'a save made after the refusal builds on the latest row',
  );
});
