import { VIEWER_HTML } from "./viewer_html.js";
export { RelayRoom } from "./relay_room.js";

interface Env {
  SIGNAL_DB: D1Database;
  RELAY_ROOM: DurableObjectNamespace;
}

const SESSION_TTL_MS = 30 * 60 * 1000;
const BASE32_ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

const corsHeaders = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, PUT, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, X-Sender-Token",
  "Access-Control-Max-Age": "86400",
};

function json(body: unknown, status = 200, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json", ...corsHeaders, ...extra },
  });
}

function err(status: number, message: string): Response {
  return json({ error: message }, status);
}

function randomBase32(len: number): string {
  const buf = new Uint8Array(len);
  crypto.getRandomValues(buf);
  let out = "";
  for (const b of buf) out += BASE32_ALPHABET[b % BASE32_ALPHABET.length];
  return out;
}

function randomHex(bytes: number): string {
  const buf = new Uint8Array(bytes);
  crypto.getRandomValues(buf);
  return Array.from(buf, (b) => b.toString(16).padStart(2, "0")).join("");
}

function isValidId(id: string): boolean {
  return /^[A-Z0-9]{8}$/.test(id);
}

async function gcExpired(db: D1Database): Promise<void> {
  await db.prepare("DELETE FROM sessions WHERE expires_at < ?").bind(Date.now()).run();
}

async function getSession(db: D1Database, id: string) {
  return db
    .prepare("SELECT id, sender_token, sender_offer, viewer_answer, fallback, expires_at FROM sessions WHERE id = ?")
    .bind(id)
    .first<{
      id: string;
      sender_token: string;
      sender_offer: string | null;
      viewer_answer: string | null;
      fallback: number;
      expires_at: number;
    }>();
}

function requireSenderToken(req: Request, session: { sender_token: string }): Response | null {
  const token = req.headers.get("X-Sender-Token");
  if (!token || token !== session.sender_token) return err(401, "invalid sender token");
  return null;
}

async function readJson(req: Request): Promise<unknown> {
  try {
    return await req.json();
  } catch {
    return null;
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const method = request.method;

    if (method === "OPTIONS") {
      return new Response(null, { status: 204, headers: corsHeaders });
    }

    // POST /api/sessions
    if (path === "/api/sessions" && method === "POST") {
      await gcExpired(env.SIGNAL_DB);
      const id = randomBase32(8);
      const senderToken = randomHex(16);
      const now = Date.now();
      await env.SIGNAL_DB
        .prepare("INSERT INTO sessions (id, sender_token, created_at, expires_at) VALUES (?, ?, ?, ?)")
        .bind(id, senderToken, now, now + SESSION_TTL_MS)
        .run();
      const viewerUrl = `${url.origin}/viewer/${id}`;
      return json({ id, sender_token: senderToken, viewer_url: viewerUrl });
    }

    // /api/sessions/:id/<sub>
    const apiMatch = path.match(/^\/api\/sessions\/([A-Z0-9]{8})\/(offer|answer|fallback)$/);
    if (apiMatch) {
      const [, id, sub] = apiMatch;
      if (!isValidId(id)) return err(400, "invalid session id");
      const session = await getSession(env.SIGNAL_DB, id);
      if (!session) return err(404, "session not found");
      if (session.expires_at < Date.now()) return err(410, "session expired");

      if (sub === "offer") {
        if (method === "PUT") {
          const auth = requireSenderToken(request, session);
          if (auth) return auth;
          const body = await readJson(request);
          if (!body || typeof body !== "object" || typeof (body as { sdp?: unknown }).sdp !== "string") {
            return err(400, "expected { sdp: string }");
          }
          await env.SIGNAL_DB
            .prepare("UPDATE sessions SET sender_offer = ? WHERE id = ?")
            .bind(JSON.stringify(body), id)
            .run();
          return json({ ok: true });
        }
        if (method === "GET") {
          if (!session.sender_offer) return err(404, "offer not yet available");
          return new Response(session.sender_offer, {
            headers: { "Content-Type": "application/json", ...corsHeaders },
          });
        }
      }

      if (sub === "answer") {
        if (method === "PUT") {
          const body = await readJson(request);
          if (!body || typeof body !== "object" || typeof (body as { sdp?: unknown }).sdp !== "string") {
            return err(400, "expected { sdp: string }");
          }
          await env.SIGNAL_DB
            .prepare("UPDATE sessions SET viewer_answer = ? WHERE id = ?")
            .bind(JSON.stringify(body), id)
            .run();
          return json({ ok: true });
        }
        if (method === "GET") {
          const auth = requireSenderToken(request, session);
          if (auth) return auth;
          if (!session.viewer_answer) return err(404, "answer not yet available");
          return new Response(session.viewer_answer, {
            headers: { "Content-Type": "application/json", ...corsHeaders },
          });
        }
      }

      if (sub === "fallback") {
        if (method === "PUT") {
          const auth = requireSenderToken(request, session);
          if (auth) return auth;
          await env.SIGNAL_DB
            .prepare("UPDATE sessions SET fallback = 1 WHERE id = ?")
            .bind(id)
            .run();
          return json({ ok: true });
        }
        if (method === "GET") {
          return json({ fallback: session.fallback === 1 });
        }
      }

      return err(405, "method not allowed");
    }

    // GET /viewer/:id
    const viewerMatch = path.match(/^\/viewer\/([A-Z0-9]{8})$/);
    if (viewerMatch && method === "GET") {
      return new Response(VIEWER_HTML, {
        headers: { "Content-Type": "text/html; charset=utf-8" },
      });
    }

    // GET /ws/relay/:id?role=sender|viewer&token=...
    const wsMatch = path.match(/^\/ws\/relay\/([A-Z0-9]{8})$/);
    if (wsMatch) {
      const id = wsMatch[1];
      if (request.headers.get("Upgrade") !== "websocket") {
        return err(426, "expected websocket upgrade");
      }
      const role = url.searchParams.get("role");
      if (role !== "sender" && role !== "viewer") return err(400, "role must be sender|viewer");

      const session = await getSession(env.SIGNAL_DB, id);
      if (!session) return err(404, "session not found");
      if (session.expires_at < Date.now()) return err(410, "session expired");
      if (session.fallback !== 1) return err(409, "session not in fallback mode");

      if (role === "sender") {
        const token = url.searchParams.get("token");
        if (token !== session.sender_token) return err(401, "invalid sender token");
      }

      const doId = env.RELAY_ROOM.idFromName(id);
      const stub = env.RELAY_ROOM.get(doId);
      return stub.fetch(request);
    }

    // GET / — tiny landing page
    if (path === "/" && method === "GET") {
      return new Response(
        "<!doctype html><meta charset=utf-8><title>screenshare</title><body style=font-family:sans-serif;padding:2rem><h1>screenshare backend</h1><p>This is the signaling backend. Download the client to share your screen.</p>",
        { headers: { "Content-Type": "text/html; charset=utf-8" } },
      );
    }

    return err(404, "not found");
  },
};
