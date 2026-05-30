interface DOEnv {}

const IDLE_TIMEOUT_MS = 60_000;
const CONFIG_KEY = "lastConfig";

export class RelayRoom {
  state: DurableObjectState;

  constructor(state: DurableObjectState, _env: DOEnv) {
    this.state = state;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const role = url.searchParams.get("role");
    if (role !== "sender" && role !== "viewer") {
      return new Response("invalid role", { status: 400 });
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];

    const before = {
      senders: this.state.getWebSockets("sender").length,
      viewers: this.state.getWebSockets("viewer").length,
    };

    if (role === "sender") {
      // close any previous sender to avoid duplicate broadcasters
      for (const ws of this.state.getWebSockets("sender")) {
        try { ws.close(1000, "replaced by new sender"); } catch {}
      }
      // Stale config belongs to the previous sender; the new one will resend.
      await this.state.storage.delete(CONFIG_KEY);
      this.state.acceptWebSocket(server, ["sender"]);
    } else {
      this.state.acceptWebSocket(server, ["viewer"]);
      // The sender sends its RelayConfig once at WS open. Viewers poll the
      // `fallback` flag every 2s before joining, so they almost always
      // connect *after* that message has already been broadcast. Without
      // replaying it the viewer's VideoDecoder is never configured and
      // every binary frame is silently dropped (the "v/a 0/0" symptom).
      const lastConfig = await this.state.storage.get<string>(CONFIG_KEY);
      if (lastConfig) {
        try { server.send(lastConfig); } catch {}
      }
    }

    await this.bumpIdleAlarm();
    console.log(`[relay] accept role=${role} before=${JSON.stringify(before)} after_senders=${this.state.getWebSockets("sender").length} after_viewers=${this.state.getWebSockets("viewer").length}`);

    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const tags = this.state.getTags(ws);
    if (!tags.includes("sender")) {
      console.log(`[relay] msg from non-sender ignored tags=${JSON.stringify(tags)}`);
      return;
    }
    if (typeof message === "string") {
      console.log(`[relay] sender text msg (${message.length} bytes) — buffering as config`);
      await this.state.storage.put(CONFIG_KEY, message);
    } else {
      // Don't log every binary frame, only the first one as a heartbeat-ish.
    }
    const viewers = this.state.getWebSockets("viewer");
    for (const v of viewers) {
      try { v.send(message); } catch {}
    }
    await this.bumpIdleAlarm();
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string, _wasClean: boolean): Promise<void> {
    const tags = this.state.getTags(ws);
    console.log(`[relay] close tags=${JSON.stringify(tags)} code=${code} reason=${reason} clean=${_wasClean}`);
    try { ws.close(code, reason); } catch {}
    if (tags.includes("sender")) {
      // If another sender is still attached (e.g., this one was just replaced
      // by a fresh connect), keep the viewers — the new sender will keep
      // broadcasting. Only tear down when no sender remains.
      // NB: CF's getWebSockets() still includes the closing WS during this
      // handler, so filter it out before counting.
      const remaining = this.state.getWebSockets("sender").filter((w) => w !== ws);
      console.log(`[relay] sender closed, remaining senders=${remaining.length}`);
      if (remaining.length > 0) return;
      await this.state.storage.delete(CONFIG_KEY);
      const viewers = this.state.getWebSockets("viewer");
      console.log(`[relay] cascading close to ${viewers.length} viewers`);
      for (const v of viewers) {
        try { v.close(1001, "sender disconnected"); } catch {}
      }
    }
  }

  async webSocketError(ws: WebSocket, err: unknown): Promise<void> {
    const tags = this.state.getTags(ws);
    console.log(`[relay] ws error tags=${JSON.stringify(tags)} err=${String(err)}`);
    try { ws.close(1011, "ws error"); } catch {}
  }

  async alarm(): Promise<void> {
    const senders = this.state.getWebSockets("sender").length;
    const viewers = this.state.getWebSockets("viewer").length;
    console.log(`[relay] alarm fired senders=${senders} viewers=${viewers}`);
    if (senders > 0) {
      await this.state.storage.setAlarm(Date.now() + IDLE_TIMEOUT_MS);
      return;
    }
    for (const ws of this.state.getWebSockets()) {
      try { ws.close(4001, "no sender connected"); } catch {}
    }
  }

  private async bumpIdleAlarm(): Promise<void> {
    await this.state.storage.setAlarm(Date.now() + IDLE_TIMEOUT_MS);
  }
}
