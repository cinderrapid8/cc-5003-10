// Auditable multi-round loop player.
//
// It does NOT trust the WAV length. For each resource it uses the manifest's
// decoded-frame coordinates (start_frame / end_frame) and the resource's
// sample rate, scheduling precise Web Audio buffer-source loops so the seam
// is exercised every round and can be listened to directly.

export class LoopPlayer {
  constructor() {
    this.ctx = null;
    this.active = new Map(); // key -> { source, gain, rounds }
  }

  ensureContext() {
    if (!this.ctx) {
      this.ctx = new (window.AudioContext || window.webkitAudioContext)();
    }
    if (this.ctx.state === 'suspended') this.ctx.resume();
    return this.ctx;
  }

  fileUrl(file, baseDir) {
    if (window.__TAURI_INTERNALS__) {
      // Tauri serves bundled/asset URLs through convertFileSrc.
      return `asset://localhost/${encodeURI(file)}`;
    }
    // looper-server exposes the working directory under /files.
    return `/files/${baseDir.split('/').map(encodeURIComponent).join('/')}/${encodeURIComponent(file)}`;
  }

  async decode(file, baseDir) {
    const ctx = this.ensureContext();
    const res = await fetch(this.fileUrl(file, baseDir));
    if (!res.ok) throw new Error(`cannot fetch ${file}: ${res.status}`);
    const arr = await res.arrayBuffer();
    return await ctx.decodeAudioData(arr);
  }

  /**
   * Play `rounds` loops of one variant resource.
   * @param {object} resource  manifest CodecResource
   * @param {object} coords    manifest ResourceCoordinates for that codec
   * @param {string} baseDir   directory the output files live under
   * @param {number} rounds    how many times to wrap the loop
   * @param {function} onRound called after each scheduled round
   */
  async play(resource, coords, baseDir, rounds = 4, onRound = () => {}) {
    const ctx = this.ensureContext();
    const key = resource.file;
    this.stop(key);

    const audioBuffer = await this.decode(resource.file, baseDir);

    // Coordinates are frame indices into the *decoded* stream. Convert them
    // to seconds using the decoder's actual sample rate (not the WAV header
    // value the author happened to write).
    const rate = audioBuffer.sampleRate;
    const startSec = coords.start_frame / rate;
    const endSec = coords.end_frame / rate;
    const loopLen = endSec - startSec;

    const source = ctx.createBufferSource();
    source.buffer = audioBuffer;
    source.loop = true;
    source.loopStart = startSec;
    source.loopEnd = endSec;

    const gain = ctx.createGain();
    gain.gain.value = 0.9;
    source.connect(gain).connect(ctx.destination);

    const startedAt = ctx.currentTime + 0.05;
    source.start(startedAt, startSec);
    // Stop after the requested number of rounds, plus a small tail margin.
    source.stop(startedAt + loopLen * rounds + 0.1);

    let done = 0;
    const timer = setInterval(() => {
      const elapsed = ctx.currentTime - startedAt;
      const reached = Math.min(rounds, Math.floor(elapsed / loopLen) + 1);
      if (reached !== done) {
        done = reached;
        onRound(done, rounds, { rate });
      }
      if (done >= rounds || ctx.currentTime > startedAt + loopLen * rounds + 0.2) {
        clearInterval(timer);
      }
    }, 30);

    source.onended = () => {
      clearInterval(timer);
      this.active.delete(key);
    };

    this.active.set(key, { source, gain, timer });
    return { startSec, endSec, loopLen, rate, decodedDuration: audioBuffer.duration };
  }

  stop(key) {
    const a = this.active.get(key);
    if (!a) return;
    clearInterval(a.timer);
    try {
      a.source.onended = null;
      a.source.stop();
    } catch {
      /* already stopped */
    }
    this.active.delete(key);
  }

  stopAll() {
    for (const key of [...this.active.keys()]) this.stop(key);
  }
}
