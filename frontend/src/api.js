// Single API surface. In the Tauri webview it calls the Rust commands through
// IPC; in a plain browser it talks to looper-server over HTTP/SSE. The UI
// code never knows which transport it is using.

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

async function tauriInvoke(cmd, args = {}) {
  const { invoke } = await import('@tauri-apps/api/core');
  const { listen } = await import('@tauri-apps/api/event');
  return { invoke, listen, cmd, args };
}

export async function synthFixtures(dir) {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('synth_fixtures', { dir });
  }
  const r = await fetch('/api/synth', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ dir }),
  });
  return unwrap(await r.json());
}

export async function saveProject(path, project) {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('save_project_file', { path, project });
  }
  const r = await fetch('/api/save', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path, project }),
  });
  return unwrap(await r.json());
}

export async function loadProject(path) {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('load_project_file', { path });
  }
  const r = await fetch(`/api/load?path=${encodeURIComponent(path)}`);
  return unwrap((await r.json()).project);
}

/**
 * Run a build, streaming progress. `onProgress(event)` receives every
 * ProgressEvent the Rust pipeline emits. Resolves with the final manifest.
 */
export async function runBuild(project, baseDir, outDir, onProgress) {
  if (isTauri) {
    const { invoke, listen } = await tauriInvoke();
    const unlisten = await listen('build-progress', (e) => onProgress(e.payload));
    try {
      return await invoke('run_build', { project, baseDir, outDir });
    } finally {
      await unlisten();
    }
  }

  const r = await fetch('/api/build', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ project, base_dir: baseDir, out_dir: outDir }),
  });
  if (!r.ok || !r.body) throw new Error(`build request failed: ${r.status}`);

  const reader = r.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let manifest = null;

  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const parts = buffer.split('\n\n');
    buffer = parts.pop() ?? '';
    for (const part of parts) {
      const dataLine = part.split('\n').find((l) => l.startsWith('data:'));
      const eventLine = part.split('\n').find((l) => l.startsWith('event:'));
      if (!dataLine) continue;
      const raw = dataLine.slice(5).trim();
      const kind = eventLine ? eventLine.slice(6).trim() : 'progress';
      if (kind === 'error') throw new Error(raw);
      const payload = JSON.parse(raw);
      if (kind === 'progress') onProgress(payload);
      else if (kind === 'manifest') manifest = payload.manifest;
    }
  }
  if (!manifest) throw new Error('build ended without a manifest');
  return manifest;
}

export async function verifySoundbank(manifestPath, outDir) {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('verify_soundbank', { manifestPath, outDir });
  }
  const r = await fetch(
    `/api/verify?manifest=${encodeURIComponent(manifestPath)}&out_dir=${encodeURIComponent(outDir)}`
  );
  const j = await r.json();
  return unwrap(j.reports ?? j);
}

function unwrap(value) {
  if (value && typeof value === 'object' && value.ok === false) {
    throw new Error(value.error ?? 'request failed');
  }
  return value;
}
