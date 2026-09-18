import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

/**
 * Payload of the `sidecar-status` event emitted by the Rust supervisor
 * (`spawn_sidecar_monitor` in src-tauri/src/lib.rs).
 *
 * `down` and `restarted` are the two the demo is about; `failed` means the
 * supervisor exhausted its backoff ladder and stopped trying.
 */
type SidecarStatusEvent = {
  status: "down" | "restarted" | "failed";
  attempt: number;
};

/** Shape returned by the `get_status` handler in sidecar/handlers.py. */
type SidecarStatus = {
  pid: number;
  schema_version: number;
  note_count: number;
};

type LogEntry = { at: string; text: string; kind: "info" | "warn" | "error" };

/** How often the UI asks the sidecar who it is. */
const POLL_INTERVAL_MS = 1500;

function now(): string {
  return new Date().toLocaleTimeString();
}

function App() {
  const [status, setStatus] = useState<SidecarStatus | null>(null);
  const [reachable, setReachable] = useState<boolean | null>(null);
  const [supervisorState, setSupervisorState] = useState<SidecarStatusEvent | null>(null);
  const [message, setMessage] = useState("hello from the renderer");
  const [echoed, setEchoed] = useState<string | null>(null);
  const [echoError, setEchoError] = useState<string | null>(null);
  const [log, setLog] = useState<LogEntry[]>([]);

  // The PID the sidecar last reported. Kept in a ref as well as in state so the
  // poll callback can compare against it without re-subscribing every tick —
  // a changed PID is the proof that the process really was replaced.
  const lastPid = useRef<number | null>(null);

  const append = useCallback((text: string, kind: LogEntry["kind"] = "info") => {
    setLog((entries) => [{ at: now(), text, kind }, ...entries].slice(0, 60));
  }, []);

  // Subscribe to the supervisor's status events.
  useEffect(() => {
    const unlisten = listen<SidecarStatusEvent>("sidecar-status", (event) => {
      setSupervisorState(event.payload);
      const { status: s, attempt } = event.payload;
      if (s === "down") {
        append(`Supervisor: sidecar died (attempt ${attempt} pending)`, "warn");
      } else if (s === "restarted") {
        append(`Supervisor: sidecar restarted after attempt ${attempt}`, "info");
      } else {
        append(`Supervisor: gave up after ${attempt} attempts`, "error");
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, [append]);

  // Poll `get_status` so the panel reflects reality rather than the last event.
  useEffect(() => {
    let cancelled = false;

    async function poll() {
      try {
        const next = await invoke<SidecarStatus>("sidecar_request", {
          method: "get_status",
          params: {},
        });
        if (cancelled) return;
        if (lastPid.current !== null && lastPid.current !== next.pid) {
          append(`New sidecar process: pid ${lastPid.current} -> ${next.pid}`, "info");
        }
        lastPid.current = next.pid;
        setStatus(next);
        setReachable(true);
      } catch {
        if (cancelled) return;
        setReachable(false);
      }
    }

    poll();
    const timer = setInterval(poll, POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [append]);

  async function sendEcho(event: React.FormEvent) {
    event.preventDefault();
    setEchoError(null);
    try {
      const result = await invoke<{ message: string }>("sidecar_request", {
        method: "echo",
        params: { message },
      });
      setEchoed(result.message);
      append(`Echo round-trip ok: "${result.message}"`, "info");
    } catch (err) {
      setEchoed(null);
      setEchoError(String(err));
      append(`Echo failed: ${String(err)}`, "error");
    }
  }

  const badge =
    reachable === null
      ? { label: "connecting", cls: "unknown" }
      : reachable
        ? { label: "connected", cls: "ok" }
        : { label: "unreachable", cls: "bad" };

  return (
    <main className="container">
      <header>
        <h1>Supervised Python sidecar</h1>
        <p className="lede">
          Kill the Python process in Task Manager (or with{" "}
          <code>taskkill /PID &lt;pid&gt; /F</code>) and watch the supervisor bring it back.
        </p>
      </header>

      <section className="panel">
        <div className="row">
          <span className={`badge ${badge.cls}`}>{badge.label}</span>
          {supervisorState && (
            <span className={`badge ${supervisorState.status === "failed" ? "bad" : "warn"}`}>
              last supervisor event: {supervisorState.status} (attempt {supervisorState.attempt})
            </span>
          )}
        </div>
        <dl className="facts">
          <div>
            <dt>Sidecar PID</dt>
            <dd>{status ? status.pid : "—"}</dd>
          </div>
          <div>
            <dt>Schema version</dt>
            <dd>
              {status ? status.schema_version : "—"}
              {status && status.schema_version > 0 && (
                <span className="hint"> migration applied</span>
              )}
            </dd>
          </div>
          <div>
            <dt>Rows in notes</dt>
            <dd>{status ? status.note_count : "—"}</dd>
          </div>
        </dl>
      </section>

      <section className="panel">
        <h2>Echo round-trip</h2>
        <form className="row" onSubmit={sendEcho}>
          <input
            value={message}
            onChange={(e) => setMessage(e.currentTarget.value)}
            placeholder="Anything at all"
          />
          <button type="submit">Send</button>
        </form>
        {echoed !== null && <p className="result">Sidecar replied: {echoed}</p>}
        {echoError !== null && <p className="result error">{echoError}</p>}
      </section>

      <section className="panel">
        <h2>Supervisor log</h2>
        <ul className="log">
          {log.length === 0 && <li className="info">Waiting for events…</li>}
          {log.map((entry, i) => (
            <li key={`${entry.at}-${i}`} className={entry.kind}>
              <span className="at">{entry.at}</span> {entry.text}
            </li>
          ))}
        </ul>
      </section>
    </main>
  );
}

export default App;
