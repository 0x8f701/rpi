// QuickJS provider registration fixture: proves the registerProvider API is
// alive in the in-process runtime. Loading with the `provider` capability
// granted succeeds and exposes the provider; loading without it fails the
// capability gate.
export default function (pi) {
  pi.registerProvider({
    id: "provider-registration",
    label: "Provider Registration",
    api: "provider-registration-api",
    capabilities: ["streaming"],
    stream: async function* () {
      yield { type: "text", text: "provider-registration-ok" };
      yield { type: "done", stopReason: "stop" };
    },
  });
}
