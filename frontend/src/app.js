import './styles.css';
import * as api from './api.js';
import { LoopPlayer } from './player.js';

const $ = (id) => document.getElementById(id);
const player = new LoopPlayer();

const EXAMPLE_PROJECT = {
  name: 'game-loop-demo',
  schema_version: 1,
  assets: [
    {
      id: 'engine_idle',
      source: 'samples/engine_dc_offset.wav',
      source_sha256: null,
      region: { start_frame: 4800, end_frame: 80000, search_window: 512 },
      fade_ms: 30,
      fade_curve: 'equal_power',
      min_keep_frames: 256,
      variants: [
        { kind: 'parametric', name: 'intensity_low', gain: 0.55, resample_ratio: 0.92 },
        { kind: 'parametric', name: 'intensity_high', gain: 1.35, resample_ratio: 1.08 },
      ],
      encode_lossy: true,
      encode_packed: true,
      packed_delay_frames: 1024,
      packed_padding_frames: 768,
    },
    {
      id: 'mechanical_hum',
      source: 'samples/mechanical_noninteger_period.wav',
      source_sha256: null,
      region: { start_frame: 2400, end_frame: 60000, search_window: 384 },
      fade_ms: 25,
      fade_curve: 'equal_power',
      min_keep_frames: 128,
      variants: [
        {
          kind: 'independent',
          name: 'hum_alt',
          source: 'samples/ambient_stereo_phase.wav',
          source_sha256: null,
          region: { start_frame: 4800, end_frame: 72000, search_window: 256 },
          fade_ms: 40,
          fade_curve: 'linear',
          min_keep_frames: 128,
        },
      ],
      encode_lossy: true,
      encode_packed: true,
      packed_delay_frames: 1024,
      packed_padding_frames: 768,
    },
    {
      id: 'ambient_bed',
      source: 'samples/ambient_stereo_phase.wav',
      source_sha256: null,
      region: null,
      fade_ms: 50,
      fade_curve: 'equal_power',
      min_keep_frames: 512,
      variants: [],
      encode_lossy: true,
      encode_packed: true,
      packed_delay_frames: 1024,
      packed_padding_frames: 768,
    },
  ],
};

$('projectEditor').value = JSON.stringify(EXAMPLE_PROJECT, null, 2);

function setMsg(id, text, bad = false) {
  const el = $(id);
  el.textContent = text;
  el.style.color = bad ? 'var(--bad)' : 'var(--good)';
}

function readProject() {
  return JSON.parse($('projectEditor').value);
}

function progressLine(ev) {
  const map = {
    started: (e) => `开始：共 ${e.total_assets} 个素材`,
    asset_started: (e) => `▶ 素材 ${e.index}: ${e.asset}`,
    source_loaded: (e) => `  解码载入 ${e.asset}: ${e.frames} 帧`,
    searching: () => '  候选评估中（零交叉仅作候选，同时检测声道与频谱跳变）',
    fading: (e) => `  交叉淡化 ${e.asset}/${e.variant}: 单侧 ${e.fade_frames} 帧`,
    encoding: (e) => `  编码 ${e.asset}/${e.variant} → ${e.codec}`,
    verifying: (e) => `  回解验证接缝 ${e.asset}/${e.variant} [${e.codec}]`,
    asset_done: (e) => `✔ 完成素材 ${e.index}: ${e.asset}`,
    finished: (e) => `🎉 完成：${e.resources} 个资源，清单 ${e.manifest}`,
    rejected: (e) => `✖ 拒绝 ${e.asset}: ${e.reason}`,
  };
  const fn = map[ev.stage];
  return fn ? fn(ev) : JSON.stringify(ev);
}

function appendProgress(text, cls = '') {
  const box = $('progress');
  const div = document.createElement('div');
  div.className = `line ${cls}`;
  div.textContent = text;
  box.appendChild(div);
  box.scrollTop = box.scrollHeight;
}

$('btnSynth').onclick = async () => {
  try {
    const dir = `${$('baseDir').value}/samples`;
    const files = await api.synthFixtures(dir);
    setMsg('pathMsg', `已生成 ${files.length} 个合成素材到 ${dir}`);
  } catch (e) {
    setMsg('pathMsg', e.message, true);
  }
};

$('btnInit').onclick = () => {
  $('projectEditor').value = JSON.stringify(EXAMPLE_PROJECT, null, 2);
  setMsg('pathMsg', '示例工程已载入编辑器（点击“保存工程”落盘）');
};

$('btnSave').onclick = async () => {
  try {
    const project = readProject();
    const path = `${$('baseDir').value}/${$('projectPath').value}`;
    await api.saveProject(path, project);
    setMsg('pathMsg', `工程已保存：${path}`);
  } catch (e) {
    setMsg('pathMsg', e.message, true);
  }
};

$('btnLoad').onclick = async () => {
  try {
    const path = `${$('baseDir').value}/${$('projectPath').value}`;
    const project = await api.loadProject(path);
    $('projectEditor').value = JSON.stringify(project, null, 2);
    setMsg('pathMsg', `已读取工程：${path}`);
  } catch (e) {
    setMsg('pathMsg', e.message, true);
  }
};

$('btnBuild').onclick = async () => {
  $('progress').innerHTML = '';
  $('manifestView').innerHTML = '';
  try {
    const project = readProject();
    const baseDir = $('baseDir').value;
    const outDir = `${baseDir}/${$('outDir').value}`;
    const manifest = await api.runBuild(project, baseDir, outDir, (ev) => {
      const cls = ev.stage === 'rejected' ? 'reject' : ev.stage === 'finished' ? 'finish' : '';
      appendProgress(progressLine(ev), cls);
    });
    renderManifest(manifest);
    appendProgress('清单已渲染，可在下方多轮试听。', 'finish');
  } catch (e) {
    appendProgress(`构建失败：${e.message}`, 'reject');
  }
};

$('btnVerify').onclick = async () => {
  try {
    const baseDir = $('baseDir').value;
    const outDir = `${baseDir}/${$('outDir').value}`;
    const reports = await api.verifySoundbank(`${outDir}/soundbank.json`, outDir);
    $('verifyView').innerHTML = renderVerify(reports);
  } catch (e) {
    $('verifyView').innerHTML = `<p class="seam-bad">验证失败：${e.message}</p>`;
  }
};

$('btnStopAudio').onclick = () => player.stopAll();

function seamClass(jump) {
  if (jump < 0.05) return 'seam-good';
  if (jump < 0.25) return 'seam-warn';
  return 'seam-bad';
}

function renderManifest(m) {
  const baseDir = $('baseDir').value;
  const outRel = $('outDir').value;
  const fileBase = `${baseDir}/${outRel}`;
  const html = m.assets
    .map((asset) => {
      const variants = asset.variants
        .map((v) => {
          const lr = v.loop_record;
          const fade = lr.fade;
          const resRows = v.resources
            .map((r) => {
              const coords = lr.resource_coordinates.find((c) => c.codec === r.codec);
              return `
                <tr>
                  <td>${r.codec}</td>
                  <td>${r.decoded_frames}</td>
                  <td>${r.measured_delay_frames}</td>
                  <td>${r.measured_padding_frames}</td>
                  <td>[${coords.start_frame}, ${coords.end_frame})</td>
                  <td><code>${r.file}</code></td>
                  <td><button class="playbtn" data-file="${r.file}" data-codec="${r.codec}" data-variant="${asset.id}/${v.name}">播放循环</button>
                      <span class="round-msg" id="round-${r.codec}-${v.name}"></span></td>
                </tr>`;
            })
            .join('');
          const provenance =
            v.provenance.provenance === 'parametric'
              ? `参数化 gain=${v.provenance.gain} ratio=${v.provenance.resample_ratio}`
              : v.provenance.provenance === 'independent'
                ? `独立原件 ${v.provenance.source}`
                : '母带基准';
          return `
            <div class="variant">
              <strong>${v.name}</strong>
              <span class="pill ${v.provenance.provenance === 'parametric' ? 'warn' : 'good'}">${provenance}</span>
              <table>
                <tr><th>源起点</th><th>请求终点</th><th>采纳终点</th><th>候选数</th><th>接缝跳变</th><th>频谱不连续</th><th>零交叉</th></tr>
                <tr>
                  <td>${lr.source_start_frame}</td>
                  <td>${lr.requested_end_frame}</td>
                  <td>${lr.accepted_end_frame}</td>
                  <td>${lr.candidate_count}</td>
                  <td class="${seamClass(lr.master_seam.amplitude_jump)}">${lr.master_seam.amplitude_jump.toFixed(5)}</td>
                  <td>${lr.master_seam.spectral_discontinuity.toFixed(5)}</td>
                  <td>${lr.master_seam.zero_crossing ? '是' : '否'}</td>
                </tr>
              </table>
              <table>
                <tr><th>淡化帧/侧</th><th>头部区间</th><th>尾部区间</th><th>未混合帧</th><th>循环长度</th><th>曲线</th><th>DC偏移(L/R)</th></tr>
                <tr>
                  <td>${fade.fade_frames}</td>
                  <td>[${fade.head_lo}, ${fade.head_hi})</td>
                  <td>[${fade.tail_lo}, ${fade.tail_hi})</td>
                  <td>${fade.unblended_frames}</td>
                  <td>${fade.loop_length_frames}</td>
                  <td>${fade.curve}</td>
                  <td>${v.dc_offset_per_channel.map((d) => d.toFixed(4)).join(' / ')}</td>
                </tr>
              </table>
              <table>
                <tr><th>编码</th><th>解码帧数</th><th>实测延迟</th><th>实测填充</th><th>循环坐标(解码空间)</th><th>文件</th><th>试听</th></tr>
                ${resRows}
              </table>
            </div>`;
        })
        .join('');
      return `
        <div class="asset">
          <h3>${asset.id} <span class="pill">源 ${asset.source_sample_rate}Hz / ${asset.source_channels}ch / ${asset.source_decoded_frames}帧</span>
              <span class="pill">sha ${asset.source_sha256.slice(0, 12)}…</span></h3>
          ${variants}
        </div>`;
    })
    .join('');
  $('manifestView').innerHTML = html;

  document.querySelectorAll('.playbtn').forEach((btn) => {
    btn.onclick = async () => {
      const variant = btn.dataset.variant;
      const codec = btn.dataset.codec;
      const file = btn.dataset.file;
      const rounds = Math.max(1, parseInt($('rounds').value, 10) || 4);
      // Locate resource + coordinates from the rendered manifest.
      const asset = m.assets.find((a) => variant.startsWith(a.id));
      const vName = variant.split('/')[1];
      const variantRec = asset.variants.find((v) => v.name === vName);
      const resource = variantRec.resources.find((r) => r.file === file);
      const coords = variantRec.loop_record.resource_coordinates.find((c) => c.codec === codec);
      const msg = $(`round-${codec}-${vName}`);
      try {
        const info = await player.play(resource, coords, fileBase, rounds, (done, total, live) => {
          msg.textContent =
            `第 ${done}/${total} 轮 · 解码率 ${live.rate}Hz · 坐标 [${coords.start_frame}, ${coords.end_frame})`;
        });
        msg.textContent =
          `播放完成 ${rounds} 轮 · 解码率 ${info.rate}Hz · 循环 ${info.loopLen.toFixed(4)}s ` +
          `· 坐标 [${coords.start_frame}, ${coords.end_frame})`;
      } catch (e) {
        msg.textContent = `播放失败：${e.message}`;
        msg.style.color = 'var(--bad)';
      }
    };
  });
}

function renderVerify(reports) {
  const rows = reports
    .map(
      (r) => `
      <tr>
        <td>${r.asset}/${r.variant}</td>
        <td>${r.codec}</td>
        <td>${r.decoded_frames}</td>
        <td class="${seamClass(r.seam.amplitude_jump)}">${r.seam.amplitude_jump.toFixed(5)}</td>
        <td>${r.seam.spectral_discontinuity.toFixed(5)}</td>
        <td>${r.seam.zero_crossing ? '是' : '否'}</td>
      </tr>`
    )
    .join('');
  return `
    <h2 style="margin-top:1rem">解码回验（基于实际解码 PCM，非原 WAV 长度）</h2>
    <table>
      <tr><th>变体</th><th>编码</th><th>解码帧数</th><th>接缝跳变</th><th>频谱不连续</th><th>零交叉</th></tr>
      ${rows}
    </table>`;
}
