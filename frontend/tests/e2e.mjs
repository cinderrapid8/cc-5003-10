// Real-browser end-to-end check. Run against the built frontend served by
// looper-server: `node tests/e2e.mjs`. It drives the actual UI buttons and
// Web Audio loop player rather than mocking anything.
import { chromium } from 'playwright';

const BASE = process.env.LOOPER_URL || 'http://127.0.0.1:8088';
const results = [];
function check(name, cond, detail = '') {
  results.push({ name, ok: !!cond, detail });
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? ' — ' + detail : ''}`);
}

const browser = await chromium.launch();
const page = await browser.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));
page.on('console', (m) => {
  if (m.type() === 'error') errors.push(m.text());
});

await page.goto(BASE, { waitUntil: 'networkidle' });
check('frontend loads with title', (await page.title()).includes('循环声库'));

// 1. Generate fixtures through the UI.
await page.fill('#baseDir', 'demo_e2e');
await page.click('#btnSynth');
await page.waitForFunction(
  () => document.getElementById('pathMsg').textContent.includes('合成素材'),
  { timeout: 10000 }
);
check('synth fixtures via UI', true);

// 2. Load the example project into the editor and adjust source paths to
//    the e2e directory, then save through the UI.
await page.click('#btnInit');
const project = JSON.parse(await page.inputValue('#projectEditor'));
// Sources stay relative to baseDir: the Rust pipeline joins base_dir/source,
// and synth wrote fixtures under demo_e2e/samples/.
for (const a of project.assets) {
  if (!a.source.startsWith('samples/')) throw new Error('unexpected source layout');
}
await page.fill('#projectEditor', JSON.stringify(project, null, 2));
await page.fill('#outDir', 'out_e2e');
await page.click('#btnSave');
await page.waitForFunction(
  () => document.getElementById('pathMsg').textContent.includes('工程已保存'),
  { timeout: 10000 }
);
check('project save via UI', true);

// 3. Run the build and wait for the streamed finished event + manifest.
await page.click('#btnBuild');
await page.waitForFunction(
  () => [...document.querySelectorAll('#progress .line')].some((d) =>
    d.textContent.includes('完成：')
  ),
  { timeout: 60000 }
);
const progressText = await page.locator('#progress').innerText();
check('progress shows search stage', progressText.includes('候选评估'));
check('progress shows crossfade', progressText.includes('交叉淡化'));
check('progress shows decode verification', progressText.includes('回解验证'));
check('progress shows finished count', /完成：\d+ 个资源/.test(progressText));

// 4. Manifest rendered with traceable loop indices and fade intervals.
await page.waitForSelector('#manifestView .asset');
const assetCount = await page.locator('#manifestView .asset').count();
check('manifest renders all assets', assetCount === 3, `${assetCount} assets`);
const manifestText = await page.locator('#manifestView').innerText();
check('shows sample-accurate source indices', /源起点/.test(manifestText) && /采纳终点/.test(manifestText));
check('shows traceable fade intervals', /头部区间/.test(manifestText) && /尾部区间/.test(manifestText));
check('shows measured codec delay', /实测延迟/.test(manifestText));
check('shows DC offset', /DC偏移/.test(manifestText));
check('shows parametric provenance', /参数化 gain=/.test(manifestText));
check('shows independent provenance', /独立原件/.test(manifestText));

// 5. Drive the actual Web Audio multi-round loop player on the packed
//    resource (non-zero delay coordinates). Round counter must advance.
const rounds = 3;
await page.fill('#rounds', String(rounds));
const playBtn = page.locator('.playbtn', {
  hasText: '播放循环',
}).first();
await playBtn.click();
await page.waitForFunction(
  (r) => {
    const msgs = [...document.querySelectorAll('.round-msg')].map((e) => e.textContent);
    return msgs.some((t) => t.includes(`第 ${r}/${r} 轮`));
  },
  rounds,
  { timeout: 15000 }
);
const roundMsg = await page.locator('.round-msg').first().innerText();
check('audible multi-round loop reaches final round', roundMsg.includes(`第 ${rounds}/${rounds} 轮`), roundMsg);
check(
  'player uses decoded coordinate space',
  /坐标 \[\d+, \d+\)/.test(roundMsg) && /解码率/.test(roundMsg)
);

// 6. Re-verification through the UI re-decodes every resource.
await page.click('#btnVerify');
await page.waitForSelector('#verifyView table tr');
const verifyRows = await page.locator('#verifyView table tr').count();
check('reverify lists every resource (header + 18)', verifyRows === 19, `${verifyRows} rows`);

check('no page/console errors', errors.length === 0, errors.join(' | '));

await browser.close();
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length ? 1 : 0);
