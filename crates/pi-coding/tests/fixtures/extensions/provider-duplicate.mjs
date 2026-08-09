// Invalid provider registration: registering the same id twice in one load
// phase must fail actionably (re-registration replaces only across
// generations, like tools).
export default function (pi) {
  pi.registerProvider({
    id: "duplicate",
    api: "duplicate-api",
    stream: async function* () {},
  });
  pi.registerProvider({
    id: "duplicate",
    api: "duplicate-api",
    stream: async function* () {},
  });
}
