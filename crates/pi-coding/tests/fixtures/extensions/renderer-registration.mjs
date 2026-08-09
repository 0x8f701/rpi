// QuickJS twin of renderer-registration.ts. The message_renderers capability
// is rejected by manifest validation before this entry ever runs; the entry
// exists only so the failing spec has a real path.
export default function (pi) {
  pi.registerCommand("renderer-registration", {
    handler: () => "unreachable",
  });
}
