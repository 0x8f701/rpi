// Invalid provider registration: unregistering an unknown provider id must
// fail the load actionably.
export default function (pi) {
  pi.unregisterProvider("never-registered");
}
