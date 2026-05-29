interface DOEnv {}

const IDLE_TIMEOUT_MS = 60_000;

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

    if (role === "sender") {
      // close any previous sender to avoid duplicate broadcasters
      for (const ws of this.state.getWebSockets("sender")) {
        try { ws.close(1000, "replaced by new sender"); } catch {}
      }
      this.state.acceptWebSocket(server, ["sender"]);
    } else {
      this.state.acceptWebSocket(server, ["viewer"]);
    }

    await this.bumpIdleAlarm();

    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const tags = this.state.getTags(ws);
    if (!tags.includes("sender")) return; // viewers don't push
    const viewers = this.state.getWebSockets("viewer");
    for (const v of viewers) {
      try { v.send(message); } catch {}
    }
    await this.bumpIdleAlarm();
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string, _wasClean: boolean): Promise<void> {
    try { ws.close(code, reason); } catch {}
    const tags = this.state.getTags(ws);
    if (tags.includes("sender")) {
      // sender left — close all viewers too
      for (const v of this.state.getWebSockets("viewer")) {
        try { v.close(1001, "sender disconnected"); } catch {}
      }
    }
  }

  async webSocketError(ws: WebSocket, _err: unknown): Promise<void> {
    try { ws.close(1011, "ws error"); } catch {}
  }

  async alarm(): Promise<void> {
    // idle timeout — close everything
    for (const ws of this.state.getWebSockets()) {
      try { ws.close(1000, "idle timeout"); } catch {}
    }
  }

  private async bumpIdleAlarm(): Promise<void> {
    await this.state.storage.setAlarm(Date.now() + IDLE_TIMEOUT_MS);
  }
}
