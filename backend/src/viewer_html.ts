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
</style>
</head>
<body>
<div id="stage">
  <video id="v" autoplay playsinline></video>
  <canvas id="c" hidden></canvas>
</div>
<div id="status">connecting…</div>
<script>
(async () => {
  const ID = location.pathname.split('/').pop();
  const statusEl = document.getElementById('status');
  const video = document.getElementById('v');
  const canvas = document.getElementById('c');
  const setStatus = (s, hideAfter) => {
    statusEl.textContent = s;
    statusEl.classList.remove('hidden');
    if (hideAfter) setTimeout(() => statusEl.classList.add('hidden'), hideAfter);
  };

  // ---- WebRTC primary path ----
  async function tryWebRTC() {
    setStatus('fetching offer…');
    let offer;
    for (let i = 0; i < 60; i++) {
      const r = await fetch('/api/sessions/' + ID + '/offer');
      if (r.ok) { offer = await r.json(); break; }
      if (r.status === 410 || r.status === 404 && i > 5) {
        // 404 acceptable in first few seconds while sender is still gathering
        if (r.status === 410) throw new Error('session expired');
      }
      await new Promise(r => setTimeout(r, 1000));
    }
    if (!offer) throw new Error('no offer received');

    const pc = new RTCPeerConnection({
      iceServers: [
        { urls: 'stun:stun.cloudflare.com:3478' },
        { urls: 'stun:stun.l.google.com:19302' },
      ],
    });
    pc.ontrack = (e) => {
      if (video.srcObject !== e.streams[0]) {
        video.srcObject = e.streams[0];
        setStatus('connected', 1500);
      }
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
      // hard cap so we don't hang forever
      setTimeout(resolve, 5000);
    });
    await fetch('/api/sessions/' + ID + '/answer', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ sdp: pc.localDescription.sdp }),
    });

    return new Promise((resolve, reject) => {
      let settled = false;
      pc.onconnectionstatechange = () => {
        if (settled) return;
        if (pc.connectionState === 'connected') { settled = true; resolve(pc); }
        if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
          settled = true; reject(new Error('webrtc failed: ' + pc.connectionState));
        }
      };
      setTimeout(() => {
        if (!settled) { settled = true; reject(new Error('webrtc connect timeout')); }
      }, 15000);
    });
  }

  // ---- WS fallback path ----
  async function waitForFallback() {
    for (let i = 0; i < 30; i++) {
      const r = await fetch('/api/sessions/' + ID + '/fallback');
      if (r.ok) {
        const j = await r.json();
        if (j.fallback) return true;
      }
      await new Promise(r => setTimeout(r, 2000));
    }
    return false;
  }

  function startWsFallback() {
    setStatus('falling back to relay…');
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(proto + '//' + location.host + '/ws/relay/' + ID + '?role=viewer');
    ws.binaryType = 'arraybuffer';

    video.hidden = true;
    canvas.hidden = false;
    const ctx = canvas.getContext('2d');

    let vdec = null, adec = null;
    let actx = null;
    let nextAudioTime = 0;

    ws.addEventListener('message', async (ev) => {
      if (typeof ev.data === 'string') {
        const cfg = JSON.parse(ev.data);
        if (cfg.v !== 1) return;
        // configure video decoder
        vdec = new VideoDecoder({
          output: (frame) => {
            if (canvas.width !== frame.displayWidth) {
              canvas.width = frame.displayWidth;
              canvas.height = frame.displayHeight;
            }
            ctx.drawImage(frame, 0, 0);
            frame.close();
          },
          error: (e) => console.error('video decoder', e),
        });
        vdec.configure({ codec: cfg.codec || 'vp8' });

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
            error: (e) => console.error('audio decoder', e),
          });
          adec.configure({ codec: cfg.audio.codec || 'opus', sampleRate: cfg.audio.rate, numberOfChannels: cfg.audio.channels });
        }
        setStatus('relay active', 2000);
        return;
      }

      // binary frame: [u8 stream][u8 flags][u64 ts_us LE][bytes...]
      const buf = ev.data;
      if (buf.byteLength < 10) return;
      const dv = new DataView(buf);
      const stream = dv.getUint8(0);
      const flags = dv.getUint8(1);
      const ts = Number(dv.getBigUint64(2, true));
      const payload = new Uint8Array(buf, 10);
      if (stream === 0 && vdec && vdec.state === 'configured') {
        vdec.decode(new EncodedVideoChunk({
          type: (flags & 1) ? 'key' : 'delta',
          timestamp: ts,
          data: payload,
        }));
      } else if (stream === 1 && adec && adec.state === 'configured') {
        adec.decode(new EncodedAudioChunk({
          type: 'key',
          timestamp: ts,
          data: payload,
        }));
      }
    });

    ws.addEventListener('close', () => setStatus('relay closed'));
    ws.addEventListener('error', () => setStatus('relay error'));
  }

  try {
    await tryWebRTC();
  } catch (e) {
    console.warn('webrtc failed:', e.message);
    setStatus('direct connection failed, waiting for relay…');
    const ok = await waitForFallback();
    if (!ok) { setStatus('could not connect'); return; }
    startWsFallback();
  }
})();
</script>
</body>
</html>
`;
