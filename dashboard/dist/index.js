// WhatsApp Voice tab for the Hermes dashboard. Plain IIFE, no build step: React, Button and Input
// come from the dashboard SDK; layout comes from dist/style.css (hwv-* classes).
(function () {
  "use strict";

  const SDK = window.__HERMES_PLUGIN_SDK__;
  const { React } = SDK;
  const { useState, useEffect, useCallback } = SDK.hooks;
  const { Button, Input } = SDK.components;
  const h = React.createElement;
  const API = "/api/plugins/hermes-whatsapp-voice";

  const get = (path) => SDK.fetchJSON(API + path);
  const post = (path, body) =>
    SDK.fetchJSON(API + path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body || {}) });
  const message = (e) => String((e && e.message) || e).replace(/^Error:\s*/, "");

  // Settings are grouped here; any key not listed lands in Advanced.
  const GROUPS = [
    { title: "You", keys: ["owner_number"] },
    { title: "Voice", keys: ["assistant_name", "languages", "voice_instructions"] },
    { title: "Calls", keys: ["ring_timeout_seconds", "max_call_minutes", "silence_hangup_seconds", "answer_inbound", "send_summary"] },
  ];
  const WIDE = ["voice_instructions", "live_config"];

  // The Voice card leads with two linked dropdowns: the realtime model (which also picks
  // voice_provider) and that provider's voices. Model IDs stay editable in Advanced.
  const PROVIDERS = [
    {
      id: "openai",
      name: "OpenAI",
      note: "ChatGPT subscription",
      modelKey: "openai_model",
      voiceKey: "openai_voice",
      voices: ["alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "marin", "cedar"],
    },
    {
      id: "grok",
      name: "Grok",
      note: "SuperGrok login",
      modelKey: "grok_model",
      voiceKey: "grok_voice",
      voices: ["ara", "rex", "sal", "eve", "leo", "carina", "zagan", "helix", "orion", "luna", "iris", "altair", "zenith", "perseus",
        "helios", "lux", "kepler", "rigel", "cosmo", "celeste", "ursa", "sirius", "lumen", "castor", "naksh", "atlas", "aurora", "liora"],
    },
    {
      id: "gemini",
      name: "Gemini",
      note: "API key",
      modelKey: "voice_model",
      voiceKey: "voice",
      voices: ["Zephyr", "Puck", "Charon", "Kore", "Fenrir", "Leda", "Orus", "Aoede", "Callirrhoe", "Autonoe", "Enceladus", "Iapetus",
        "Umbriel", "Algieba", "Despina", "Erinome", "Algenib", "Rasalgethi", "Laomedeia", "Achernar", "Alnilam", "Schedar", "Gacrux",
        "Pulcherrima", "Achird", "Zubenelgenubi", "Vindemiatrix", "Sadachbia", "Sadaltager", "Sulafat"],
    },
  ];
  const PICKER_KEYS = ["voice_provider"].concat(PROVIDERS.map((p) => p.voiceKey));

  function Select({ value, options, onChange }) {
    return h(
      "select",
      { className: "hwv-select", value, onChange: (e) => onChange(e.target.value) },
      ...options.map((o) => h("option", { key: o.value, value: o.value }, o.label)),
    );
  }

  function VoicePicker({ form, schema, set }) {
    const provider = PROVIDERS.find((p) => p.id === form.voice_provider) || PROVIDERS[0];
    const model = (p) => form[p.modelKey] || (schema[p.modelKey] && schema[p.modelKey].default) || "";
    const voice = form[provider.voiceKey] || "";
    // A configured voice outside the known list stays selectable rather than silently changing.
    const voices = provider.voices.includes(voice) || !voice ? provider.voices : [voice].concat(provider.voices);
    return [
      h(
        "label",
        { key: "model", className: "hwv-field" },
        h("span", { className: "hwv-label" }, "Voice model"),
        h(Select, {
          value: provider.id,
          options: PROVIDERS.map((p) => ({ value: p.id, label: `${p.name} · ${model(p)} (${p.note})` })),
          onChange: (id) => set({ voice_provider: id }),
        }),
        h("span", { className: "hwv-help" }, "The realtime model that speaks on calls. OpenAI and Grok fall back to Gemini if they cannot connect."),
      ),
      h(
        "label",
        { key: "voice", className: "hwv-field" },
        h("span", { className: "hwv-label" }, "Voice"),
        h(Select, {
          value: voice || provider.voices[0],
          options: voices.map((v) => ({ value: v, label: v })),
          onChange: (v) => set({ [provider.voiceKey]: v }),
        }),
        h("span", { className: "hwv-help" }, `${provider.name} voices.`),
      ),
    ];
  }

  function Card(props, ...children) {
    return h(
      "section",
      { className: "hwv-card" },
      props.title &&
        h("div", { className: "hwv-card-head" }, h("h2", { className: "hwv-card-title" }, props.title), props.actions || null),
      ...children,
    );
  }


  function readiness(status) {
    const s = status.sidecar;
    return [
      { key: "gemini", ok: status.gemini_key, label: "Gemini key", hint: "Add a Gemini API key to Hermes (Keys page, GOOGLE_API_KEY). The plugin reuses it." },
      { key: "owner", ok: !!status.owner_number, label: "Your number", hint: "Set your WhatsApp number in Settings below." },
      { key: "hermes", ok: status.hermes_api_key, label: "Hermes API", hint: "Set API_SERVER_ENABLED=true and an API_SERVER_KEY (Keys page), then restart the gateway." },
      { key: "linked", ok: !!(s && s.logged_in) || status.pairing === "linked", label: "WhatsApp linked", hint: "Link the bot's WhatsApp number once." },
      { key: "sidecar", ok: !!s, label: "Sidecar", hint: "Not running. Restart it, or install the background service: hermes whatsapp-voice service." },
      { key: "test", ok: status.tested, label: "Test call", hint: "Have the bot call your phone to check everything works end to end." },
    ];
  }

  function Pairing() {
    const [state, setState] = useState("idle");
    const [svg, setSvg] = useState(null);
    const [error, setError] = useState(null);

    useEffect(() => {
      if (state !== "waiting") return undefined;
      const timer = setInterval(() => {
        get("/pair/qr")
          .then((r) => {
            setSvg(r.svg);
            if (r.pairing === "linked" || r.pairing === "failed") setState(r.pairing);
          })
          .catch((e) => setError(message(e)));
      }, 1500);
      return () => clearInterval(timer);
    }, [state]);

    const start = () => {
      setError(null);
      setSvg(null);
      post("/pair/start").then(() => setState("waiting")).catch((e) => setError(message(e)));
    };

    if (state === "linked") return h("div", { className: "hwv-step-hint" }, "Linked. Restart the sidecar to start using it.");
    return h(
      "div",
      { style: { marginTop: ".75rem" } },
      state === "waiting"
        ? h(
            "div",
            { className: "hwv-qr" },
            svg ? h("img", { alt: "WhatsApp pairing QR code", src: "data:image/svg+xml;utf8," + encodeURIComponent(svg) }) : h("span", { className: "hwv-muted hwv-small" }, "Getting a code…"),
            h(
              "div",
              null,
              h("div", { className: "hwv-small" }, "On the bot's phone: WhatsApp › Linked devices › Link a device, then scan."),
              h("div", { className: "hwv-small hwv-muted", style: { margin: ".35rem 0 .75rem" } }, "The code refreshes on its own."),
              h(Button, { size: "sm", variant: "outline", onClick: () => post("/pair/stop").then(() => setState("idle")) }, "Cancel"),
            ),
          )
        : h(Button, { size: "sm", onClick: start }, state === "failed" ? "Try again" : "Pair with WhatsApp"),
      error && h("div", { className: "hwv-error", style: { marginTop: ".5rem" } }, error),
    );
  }

  function TestCall({ ready }) {
    const [note, setNote] = useState(null);
    const call = () => {
      setNote(null);
      post("/call", { reason: "A quick test call to check the voice line works." })
        .then(() => setNote("Calling your phone… pick up, then say bye."))
        .catch((e) => setNote(message(e)));
    };
    return h(
      "div",
      { style: { marginTop: ".75rem" } },
      h(Button, { size: "sm", disabled: !ready, onClick: call }, "Call me"),
      !ready && h("div", { className: "hwv-step-hint" }, "Available once the steps above are done."),
      note && h("div", { className: "hwv-step-hint" }, note),
    );
  }

  function Setup({ checks, onRestart, restarting }) {
    const missing = checks.filter((c) => !c.ok);
    if (missing.length === 0) return null;
    return Card(
      { title: "Finish setup" },
      h(
        "div",
        { className: "hwv-card-body" },
        h(
          "ol",
          { className: "hwv-steps" },
          ...missing.map((c) =>
            h(
              "li",
              { key: c.key, className: "hwv-step" },
              h("span", { className: "hwv-dot is-off" }),
              h("div", null, h("div", { className: "hwv-step-title" }, c.label), h("div", { className: "hwv-step-hint" }, c.hint), c.key === "linked" && h(Pairing), c.key === "test" && h(TestCall, { ready: checks.every((x) => x.ok || x.key === "test") })),
              c.key === "sidecar"
                ? h(Button, { size: "sm", variant: "outline", disabled: restarting, onClick: onRestart }, restarting ? "Restarting…" : "Restart")
                : h("span"),
            ),
          ),
        ),
      ),
    );
  }

  const PILL = { completed: "is-good", in_progress: "is-live", ringing: "is-live", dialing: "is-live", no_answer: "is-warn", rejected: "is-warn", failed: "is-bad" };
  const duration = (c) => {
    if (!c.answered_at || !c.ended_at) return "";
    const s = Math.max(0, c.ended_at - c.answered_at);
    return Math.floor(s / 60) + ":" + String(s % 60).padStart(2, "0");
  };

  function Calls({ canCall }) {
    const [calls, setCalls] = useState([]);
    const [reason, setReason] = useState("");
    const [note, setNote] = useState(null);
    const [open, setOpen] = useState(null);
    const refresh = useCallback(() => get("/calls").then((r) => setCalls(r.calls)).catch(() => {}), []);

    useEffect(() => {
      refresh();
      const timer = setInterval(refresh, 4000);
      return () => clearInterval(timer);
    }, [refresh]);

    const call = () => {
      setNote(null);
      post("/call", { reason })
        .then(() => {
          setReason("");
          setNote("Calling your phone…");
          refresh();
        })
        .catch((e) => setNote(message(e)));
    };

    const bar = h(
      "div",
      { className: "hwv-callbar", style: { marginBottom: "1rem" } },
      h(Input, { placeholder: "Test call about…", value: reason, onChange: (e) => setReason(e.target.value), disabled: !canCall }),
      h(Button, { size: "sm", disabled: !canCall, onClick: call }, "Call me"),
    );

    return h(
      "details",
      { className: "hwv-card hwv-history" },
      h("summary", { className: "hwv-card-head" }, h("h2", { className: "hwv-card-title" }, "Call history"), h("span", { className: "hwv-muted hwv-small" }, calls.length === 1 ? "1 call" : calls.length + " calls")),
      h(
        "div",
        { className: "hwv-card-body" },
        bar,
        note && h("div", { className: "hwv-small hwv-muted", style: { marginBottom: ".75rem" } }, note),
        calls.length === 0
          ? h("div", { className: "hwv-muted hwv-small" }, "No calls since the sidecar started. Call your bot's number, or use Call me.")
          : h(
              "div",
              { className: "hwv-calls" },
              ...calls.map((c) =>
                h(
                  "div",
                  { key: c.id, className: "hwv-call" },
                  h(
                    "button",
                    { className: "hwv-call-row", onClick: () => setOpen(open === c.id ? null : c.id), "aria-expanded": open === c.id },
                    h("span", { className: "hwv-pill " + (PILL[c.status] || "") }, c.status.replace("_", " ")),
                    h("span", { className: "hwv-call-reason" }, (c.direction === "inbound" ? "You called" : "Called you") + (c.reason && c.direction === "outbound" ? " · " + c.reason : "")),
                    h("span", { className: "hwv-call-meta" }, duration(c)),
                    h("span", { className: "hwv-call-meta" }, SDK.utils.timeAgo(c.started_at)),
                  ),
                  open === c.id &&
                    h(
                      "div",
                      { className: "hwv-transcript" },
                      c.summary && h("div", { className: "hwv-summary" }, c.summary),
                      c.transcript.length === 0 && h("div", { className: "hwv-muted" }, c.end_reason || "No transcript."),
                      ...c.transcript.map((t, i) => h("div", { key: i }, h("b", null, t.role === "owner" ? "You: " : "Assistant: "), t.text.trim())),
                    ),
                ),
              ),
            ),
      ),
    );
  }

  function Field({ name, spec, value, onChange }) {
    const type = spec.type || "str";
    const label = spec.label || name.replace(/_/g, " ");
    const help = spec.description ? h("span", { className: "hwv-help" }, spec.description) : null;
    if (type === "bool") {
      return h(
        "label",
        { className: "hwv-field hwv-toggle" },
        h("span", { className: "hwv-field" }, h("span", { className: "hwv-label" }, label), help),
        h("input", { type: "checkbox", checked: !!value, onChange: (e) => onChange(e.target.checked) }),
      );
    }
    let input;
    if (type === "dict") {
      input = h("textarea", { className: "hwv-textarea is-code", spellCheck: false, value, onChange: (e) => onChange(e.target.value) });
    } else if (name === "voice_instructions") {
      input = h("textarea", { className: "hwv-textarea", value: value || "", onChange: (e) => onChange(e.target.value) });
    } else {
      input = h(Input, {
        type: type === "int" ? "number" : "text",
        value: value === undefined || value === null ? "" : String(value),
        onChange: (e) => onChange(type === "int" ? Number(e.target.value) : e.target.value),
      });
    }
    // Label, input, then help: inputs stay aligned across a row however long the help text is.
    return h("label", { className: "hwv-field" + (WIDE.includes(name) ? " is-wide" : "") }, h("span", { className: "hwv-label" }, label), input, help);
  }

  function Settings({ onApplied }) {
    const [schema, setSchema] = useState(null);
    const [saved, setSaved] = useState({});
    const [form, setForm] = useState({});
    const [note, setNote] = useState(null);
    const [busy, setBusy] = useState(false);

    // Dict settings are edited as JSON text; everything else as its own type.
    const toForm = (sch, values) =>
      Object.fromEntries(Object.keys(sch).map((k) => [k, (sch[k].type || "str") === "dict" ? JSON.stringify(values[k] || {}, null, 2) : values[k]]));

    useEffect(() => {
      get("/settings")
        .then((r) => {
          setSchema(r.schema);
          setSaved(toForm(r.schema, r.values));
          setForm(toForm(r.schema, r.values));
        })
        .catch((e) => setNote(message(e)));
    }, []);

    if (!schema) return Card({ title: "Settings" }, h("div", { className: "hwv-card-body hwv-muted hwv-small" }, note || "Loading…"));

    const changed = Object.keys(form).filter((k) => JSON.stringify(form[k]) !== JSON.stringify(saved[k]));

    const save = () => {
      const body = {};
      for (const key of changed) {
        if ((schema[key].type || "str") === "dict") {
          try {
            body[key] = JSON.parse(form[key] || "{}");
          } catch (e) {
            setNote(schema[key].label + ": not valid JSON");
            return;
          }
        } else {
          body[key] = form[key];
        }
      }
      setBusy(true);
      setNote(null);
      post("/settings", body)
        .then(() => post("/restart"))
        .then(() => {
          setSaved(form);
          setNote("Saved and applied.");
          onApplied();
        })
        .catch((e) => setNote(message(e)))
        .finally(() => setBusy(false));
    };

    const field = (key) => h(Field, { key, name: key, spec: schema[key], value: form[key], onChange: (v) => setForm(Object.assign({}, form, { [key]: v })) });
    const picker = schema.voice_provider ? VoicePicker({ form, schema, set: (patch) => setForm(Object.assign({}, form, patch)) }) : [];
    const grouped = GROUPS.flatMap((g) => g.keys).concat(schema.voice_provider ? PICKER_KEYS : []);
    const advanced = Object.keys(schema).filter((k) => !grouped.includes(k));

    return Card(
      { title: "Settings" },
      ...GROUPS.map((g) =>
        h("div", { key: g.title, className: "hwv-group" }, h("h3", { className: "hwv-group-title" }, g.title), h("div", { className: "hwv-fields" }, ...(g.title === "Voice" ? picker : []), ...g.keys.filter((k) => schema[k]).map(field))),
      ),
      h(
        "details",
        { className: "hwv-group hwv-advanced" },
        h("summary", null, h("h3", { className: "hwv-group-title", style: { marginBottom: 0 } }, "Advanced ›")),
        h("div", { className: "hwv-fields", style: { marginTop: "1rem" } }, ...advanced.map(field)),
      ),
      h(
        "div",
        { className: "hwv-savebar" },
        note && h("span", { className: "hwv-small hwv-muted" }, note),
        h(Button, { size: "sm", disabled: busy || changed.length === 0, onClick: save }, busy ? "Applying…" : "Save changes"),
      ),
    );
  }

  function Page() {
    const [status, setStatus] = useState(null);
    const [error, setError] = useState(null);
    const [restarting, setRestarting] = useState(false);
    const refresh = useCallback(
      () =>
        get("/status")
          .then((s) => {
            setStatus(s);
            setError(null);
          })
          .catch((e) => setError(message(e))),
      [],
    );

    useEffect(() => {
      refresh();
      const timer = setInterval(refresh, 5000);
      return () => clearInterval(timer);
    }, [refresh]);

    const restart = () => {
      setRestarting(true);
      post("/restart")
        .then(() => new Promise((r) => setTimeout(r, 4000)))
        .then(refresh)
        .catch((e) => setError(message(e)))
        .finally(() => setRestarting(false));
    };

    if (!status) return h("div", { className: "hwv-page" }, h("div", { className: error ? "hwv-error" : "hwv-muted" }, error || "Loading…"));
    const checks = readiness(status);
    const linked = !!(status.sidecar && status.sidecar.logged_in);
    return h(
      "div",
      { className: "hwv-page" },
      h(Setup, { checks, onRestart: restart, restarting }),
      h(Settings, { onApplied: () => setTimeout(refresh, 4000) }),
      h(Calls, { canCall: linked }),
    );
  }

  window.__HERMES_PLUGINS__.register("hermes-whatsapp-voice", Page);
})();
