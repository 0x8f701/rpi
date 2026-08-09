// Provider unregister/re-register fixture: registers a provider, unregisters
// it, then re-registers the same id under a new api (last registration wins),
// and registers a second provider that is unregistered without a re-register.
export default function (pi) {
  pi.registerProvider({
    id: "replaced",
    api: "replaced-api-v1",
    stream: async function* () {
      yield { type: "text", text: "v1" };
    },
  });
  pi.unregisterProvider("replaced");
  pi.registerProvider({
    id: "replaced",
    api: "replaced-api-v2",
    stream: async function* () {
      yield { type: "text", text: "v2" };
    },
  });
  pi.registerProvider({
    id: "gone",
    api: "gone-api",
    stream: async function* () {
      yield { type: "text", text: "gone" };
    },
  });
  pi.unregisterProvider("gone");
}
