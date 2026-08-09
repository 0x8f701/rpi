// Load-phase gate: registerProvider called from a runtime event hook (after
// the load phase) must be rejected; the provider never becomes resolvable.
export default function (pi) {
  pi.on("session_start", () => {
    pi.registerProvider({
      id: "late",
      api: "late-api",
      stream: async function* () {
        yield { type: "text", text: "late" };
      },
    });
  });
}
