// Invalid provider registration: a provider id that violates the identifier
// grammar must fail the load actionably.
export default function (pi) {
  pi.registerProvider({
    id: "bad id!",
    api: "never-registered",
    stream: async function* () {},
  });
}
