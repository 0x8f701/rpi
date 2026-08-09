// Invalid provider registration: a missing stream function must fail the load
// actionably.
export default function (pi) {
  pi.registerProvider({ id: "no-stream", api: "no-stream-api" });
}
