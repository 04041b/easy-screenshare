export const VIEWER_HTML = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>screenshare viewer</title>
<style>
  html,body { margin:0; height:100%; background:#000; color:#eee; font-family:system-ui,sans-serif; }
  #stage { width:100vw; height:100vh; display:flex; align-items:center; justify-content:center; }
  video,canvas { max-width:100vw; max-height:100vh; }
  #status { position:fixed; top:8px; left:8px; padding:6px 10px; background:rgba(0,0,0,.6);
            border-radius:4px; font-size:12px; pointer-events:none; }
  #status.hidden { display:none; }
  button { padding:8px 16px; font-size:14px; cursor:pointer; }
  /* PIN entry overlay */
  #pin-gate { position:fixed; inset:0; display:flex; align-items:center; justify-content:center;
              background:#111; z-index:10; }
  #pin-card { background:#1d1d1d; padding:28px 32px; border-radius:10px; text-align:center;
              box-shadow:0 8px 24px rgba(0,0,0,.5); min-width:280px; }
  #pin-card h1 { margin:0 0 6px; font-size:18px; font-weight:600; }
  #pin-card p  { margin:0 0 18px; font-size:13px; color:#999; }
  #pin-input { width:100%; padding:10px 12px; font-size:22px; letter-spacing:0.4em; text-align:center;
               background:#000; color:#fff; border:1px solid #333; border-radius:6px;
               font-family:ui-monospace,Menlo,monospace; }
  #pin-input:focus { outline:none; border-color:#5b9bff; }
  #pin-submit { margin-top:14px; padding:10px 24px; font-size:14px; background:#5b9bff; color:#fff;
                border:none; border-radius:6px; cursor:pointer; }
  #pin-submit[disabled] { opacity:.5; cursor:not-allowed; }
  #pin-error { color:#ff7a7a; font-size:12px; margin-top:10px; min-height:1em; }
  /* Debug overlay (press D to toggle) */
  #debug { position:fixed; top:8px; right:8px; padding:8px 10px;
           background:rgba(0,0,0,.72); color:#bcd; border-radius:4px;
           font-family:ui-monospace,Menlo,monospace; font-size:11px;
           line-height:1.4; min-width:220px; pointer-events:none; z-index:5; }
  #debug.hidden { display:none; }
  #debug dt { color:#888; display:inline; }
  #debug dd { display:inline; margin:0; color:#fff; }
  #debug .row { display:flex; justify-content:space-between; gap:8px; }
</style>
</head>
<body>
<div id="stage">
  <video id="v" autoplay playsinline></video>
  <canvas id="c" hidden></canvas>
</div>
<div id="status" class="hidden">connecting…</div>
<div id="debug" class="hidden">
  <div class="row"><dt>pin</dt><dd id="dbg-pin">—</dd></div>
  <div class="row"><dt>pc</dt><dd id="dbg-pc">—</dd></div>
  <div class="row"><dt>ice</dt><dd id="dbg-ice">—</dd></div>
  <div class="row"><dt>gather</dt><dd id="dbg-gather">—</dd></div>
  <div class="row"><dt>ontrack</dt><dd id="dbg-track">0</dd></div>
  <div class="row"><dt>video bytes</dt><dd id="dbg-vb">0</dd></div>
  <div class="row"><dt>fallback poll</dt><dd id="dbg-fp">—</dd></div>
  <div class="row"><dt>ws</dt><dd id="dbg-ws">—</dd></div>
  <div class="row"><dt>ws frames v/a</dt><dd id="dbg-wsf">0/0</dd></div>
  <div class="row"><dt>vp8 decoder</dt><dd id="dbg-vdec">—</dd></div>
  <div class="row"><dt>opus decoder</dt><dd id="dbg-adec">—</dd></div>
</div>

<div id="pin-gate">
  <form id="pin-card">
    <h1>Enter PIN</h1>
    <p>Ask the person sharing for the 6-digit PIN.</p>
    <input id="pin-input" inputmode="numeric" pattern="\\d{6}" maxlength="6" autocomplete="one-time-code" autofocus />
    <button id="pin-submit" type="submit">Connect</button>
    <div id="pin-error"></div>
  </form>
</div>

<script>
(async () => {
  const ID = location.pathname.split('/').pop();
  const statusEl = document.getElementById('status');
  const video = document.getElementById('v');
  const canvas = document.getElementById('c');
  const gate = document.getElementById('pin-gate');
  const pinForm = document.getElementById('pin-card');
  const pinInput = document.getElementById('pin-input');
  const pinErr = document.getElementById('pin-error');
  const pinBtn = document.getElementById('pin-submit');

  const setStatus = (s, hideAfter) => {
    statusEl.textContent = s;
    statusEl.classList.remove('hidden');
    if (hideAfter) setTimeout(() => statusEl.classList.add('hidden'), hideAfter);
  };

  // ---- Debug overlay (auto-on if ?debug=1 in URL, toggle with D) ----
  const dbg = document.getElementById('debug');
  const dbgEls = {
    pin: document.getElementById('dbg-pin'),
    pc: document.getElementById('dbg-pc'),
    ice: document.getElementById('dbg-ice'),
    gather: document.getElementById('dbg-gather'),
    track: document.getElementById('dbg-track'),
    vb: document.getElementById('dbg-vb'),
    fp: document.getElementById('dbg-fp'),
    ws: document.getElementById('dbg-ws'),
    wsf: document.getElementById('dbg-wsf'),
    vdec: document.getElementById('dbg-vdec'),
    adec: document.getElementById('dbg-adec'),
  };
  const dbgState = { trackCnt: 0, vBytes: 0, wsFramesV: 0, wsFramesA: 0, fpAttempts: 0 };
  function setDbg(k, v) { if (dbgEls[k]) dbgEls[k].textContent = String(v); }
  if (new URLSearchParams(location.search).get('debug') === '1') dbg.classList.remove('hidden');
  window.addEventListener('keydown', (e) => {
    if (e.key === 'd' || e.key === 'D') dbg.classList.toggle('hidden');
  });

  let PIN = '';

  function pinHeaders() { return { 'X-Viewer-Pin': PIN }; }

  // ---- Validate PIN by attempting to fetch the offer ----
  async function validatePin(pin) {
    PIN = pin;
    const r = await fetch('/api/sessions/' + ID + '/offer', { headers: pinHeaders() });
    if (r.status === 401) {
      const body = await r.json().catch(() => ({}));
      throw new Error(body.error || 'invalid PIN');
    }
    if (r.status === 423) throw new Error('session locked');
    if (r.status === 410) throw new Error('session expired');
    if (r.ok) return await r.json();          // offer ready
    if (r.status === 404) return null;        // PIN ok, offer not yet there
    throw new Error('unexpected ' + r.status);
  }

  pinForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    pinErr.textContent = '';
    const pin = pinInput.value.trim();
    if (!/^\\d{6}$/.test(pin)) { pinErr.textContent = 'enter 6 digits'; return; }
    pinBtn.disabled = true;
    try {
      const initialOffer = await validatePin(pin);
      setDbg('pin', 'ok');
      gate.style.display = 'none';
      start(initialOffer);
    } catch (err) {
      pinErr.textContent = err.message;
      setDbg('pin', 'fail: ' + err.message);
      pinBtn.disabled = false;
      pinInput.select();
    }
  });

  // ---- WebRTC primary path ----
  async function start(initialOffer) {
    setStatus('fetching offer…');
    let offer = initialOffer;
    for (let i = 0; i < 60 && !offer; i++) {
      const r = await fetch('/api/sessions/' + ID + '/offer', { headers: pinHeaders() });
      if (r.ok) { offer = await r.json(); break; }
      if (r.status === 410) throw new Error('session expired');
      await new Promise(r => setTimeout(r, 1000));
    }
    if (!offer) throw new Error('no offer received');

    const pc = new RTCPeerConnection({
      iceServers: [
        { urls: 'stun:stun.cloudflare.com:3478' },
        { urls: 'stun:stun.l.google.com:19302' },
      ],
    });
    setDbg('pc', pc.connectionState);
    setDbg('ice', pc.iceConnectionState);
    setDbg('gather', pc.iceGatheringState);
    pc.addEventListener('connectionstatechange', () => setDbg('pc', pc.connectionState));
    pc.addEventListener('iceconnectionstatechange', () => setDbg('ice', pc.iceConnectionState));
    pc.addEventListener('icegatheringstatechange', () => setDbg('gather', pc.iceGatheringState));
    pc.ontrack = (e) => {
      dbgState.trackCnt++;
      setDbg('track', dbgState.trackCnt + ' (' + e.track.kind + ')');
      if (video.srcObject !== e.streams[0]) {
        video.srcObject = e.streams[0];
        setStatus('connected', 1500);
      }
      // Poll inbound bytes every 1s so we can see whether media is flowing
      // even after the connection reports "connected"
      const startedAt = performance.now();
      const tick = async () => {
        if (pc.connectionState === 'closed' || pc.connectionState === 'failed') return;
        try {
          const stats = await pc.getStats();
          stats.forEach((r) => {
            if (r.type === 'inbound-rtp' && r.kind === 'video') {
              dbgState.vBytes = r.bytesReceived || 0;
              setDbg('vb', dbgState.vBytes);
            }
          });
        } catch {}
        setTimeout(tick, 1000);
        // Stop after 60s of trying — keeps the loop bounded
        if (performance.now() - startedAt > 60000) return;
      };
      tick();
    };

    setStatus('negotiating…');
    await pc.setRemoteDescription({ type: 'offer', sdp: offer.sdp });
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    await new Promise((resolve) => {
      if (pc.iceGatheringState === 'complete') return resolve();
      pc.addEventListener('icegatheringstatechange', () => {
        if (pc.iceGatheringState === 'complete') resolve();
      });
      setTimeout(resolve, 5000);
    });
    await fetch('/api/sessions/' + ID + '/answer', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...pinHeaders() },
      body: JSON.stringify({ sdp: pc.localDescription.sdp }),
    });

    const connected = new Promise((resolve, reject) => {
      let settled = false;
      pc.onconnectionstatechange = () => {
        if (settled) return;
        if (pc.connectionState === 'connected') { settled = true; resolve(pc); }
        if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
          settled = true; reject(new Error('webrtc failed: ' + pc.connectionState));
        }
      };
      // Match the sender's 4s budget. A working P2P handshake completes in
      // well under 2s; waiting longer just keeps the viewer on a black screen
      // while the sender is already trying to escalate to the relay.
      setTimeout(() => { if (!settled) { settled = true; reject(new Error('webrtc connect timeout')); } }, 4000);
    });

    try {
      await connected;
    } catch (e) {
      console.warn('webrtc failed:', e.message);
      setStatus('direct connection failed, waiting for relay…');
      const ok = await waitForFallback();
      if (!ok) { setStatus('could not connect'); return; }
      startWsFallback();
    }
  }

  async function waitForFallback() {
    for (let i = 0; i < 30; i++) {
      dbgState.fpAttempts++;
      setDbg('fp', 'poll ' + dbgState.fpAttempts);
      const r = await fetch('/api/sessions/' + ID + '/fallback');
      if (r.ok) {
        const j = await r.json();
        if (j.fallback) { setDbg('fp', 'flag set'); return true; }
      }
      await new Promise(r => setTimeout(r, 2000));
    }
    setDbg('fp', 'timeout');
    return false;
  }

  function startWsFallback() {
    setStatus('falling back to relay…');
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    // PIN goes in the URL query because WS upgrades can't carry custom headers
    const ws = new WebSocket(proto + '//' + location.host + '/ws/relay/' + ID + '?role=viewer&pin=' + encodeURIComponent(PIN));
    ws.binaryType = 'arraybuffer';
    setDbg('ws', 'connecting');
    ws.addEventListener('open', () => setDbg('ws', 'open'));

    video.hidden = true;
    canvas.hidden = false;
    const ctx = canvas.getContext('2d');

    let vdec = null, adec = null;
    let actx = null;
    let nextAudioTime = 0;
    // VP8 P-frames can't be decoded without a preceding keyframe — feeding
    // one to a freshly-configured VideoDecoder throws "a key frame is
    // required after configure() or flush()" and corrupts the decoder for
    // the rest of the session. Gate decode until we see the first keyframe,
    // and ask the relay (which asks the sender) to produce one.
    let seenKeyframe = false;

    ws.addEventListener('message', async (ev) => {
      if (typeof ev.data === 'string') {
        const cfg = JSON.parse(ev.data);
        if (cfg.v !== 1) return;
        vdec = new VideoDecoder({
          output: (frame) => {
            if (canvas.width !== frame.displayWidth) {
              canvas.width = frame.displayWidth;
              canvas.height = frame.displayHeight;
            }
            ctx.drawImage(frame, 0, 0);
            frame.close();
          },
          error: (e) => { console.error('video decoder', e); setDbg('vdec', 'err: ' + e.message); },
        });
        try { vdec.configure({ codec: cfg.codec || 'vp8' }); setDbg('vdec', vdec.state); }
        catch (e) { setDbg('vdec', 'cfg fail: ' + e.message); }

        if (cfg.audio) {
          actx = new AudioContext({ sampleRate: cfg.audio.rate });
          adec = new AudioDecoder({
            output: (data) => {
              const buf = actx.createBuffer(data.numberOfChannels, data.numberOfFrames, data.sampleRate);
              for (let ch = 0; ch < data.numberOfChannels; ch++) {
                const arr = new Float32Array(data.numberOfFrames);
                data.copyTo(arr, { planeIndex: ch, format: 'f32-planar' });
                buf.copyToChannel(arr, ch);
              }
              const src = actx.createBufferSource();
              src.buffer = buf;
              src.connect(actx.destination);
              const now = actx.currentTime;
              if (nextAudioTime < now) nextAudioTime = now;
              src.start(nextAudioTime);
              nextAudioTime += buf.duration;
              data.close();
            },
            error: (e) => { console.error('audio decoder', e); setDbg('adec', 'err: ' + e.message); },
          });
          try {
            adec.configure({ codec: cfg.audio.codec || 'opus', sampleRate: cfg.audio.rate, numberOfChannels: cfg.audio.channels });
            setDbg('adec', adec.state);
          } catch (e) { setDbg('adec', 'cfg fail: ' + e.message); }
        }
        setStatus('relay active', 2000);
        return;
      }

      const buf = ev.data;
      if (buf.byteLength < 10) return;
      const dv = new DataView(buf);
      const stream = dv.getUint8(0);
      const flags = dv.getUint8(1);
      const ts = Number(dv.getBigUint64(2, true));
      const payload = new Uint8Array(buf, 10);
      if (stream === 0 && vdec && vdec.state === 'configured') {
        const isKey = (flags & 1) === 1;
        if (!seenKeyframe) {
          if (!isKey) {
            // Drop silently until the first keyframe lands. The DO will have
            // asked the sender for one on viewer-join; meanwhile we don't
            // want to push a delta into the decoder.
            return;
          }
          seenKeyframe = true;
          setDbg('vdec', 'configured (key seen)');
        }
        dbgState.wsFramesV++;
        setDbg('wsf', dbgState.wsFramesV + '/' + dbgState.wsFramesA);
        try {
          vdec.decode(new EncodedVideoChunk({
            type: isKey ? 'key' : 'delta',
            timestamp: ts,
            data: payload,
          }));
        } catch (e) { setDbg('vdec', 'decode err: ' + e.message); }
      } else if (stream === 1 && adec && adec.state === 'configured') {
        dbgState.wsFramesA++;
        setDbg('wsf', dbgState.wsFramesV + '/' + dbgState.wsFramesA);
        try {
          adec.decode(new EncodedAudioChunk({
            type: 'key',
            timestamp: ts,
            data: payload,
          }));
        } catch (e) { setDbg('adec', 'decode err: ' + e.message); }
      }
    });

    ws.addEventListener('close', (e) => { setStatus('relay closed'); setDbg('ws', 'closed ' + e.code); });
    ws.addEventListener('error', () => { setStatus('relay error'); setDbg('ws', 'error'); });
  }
})();
</script>
</body>
</html>
`;
